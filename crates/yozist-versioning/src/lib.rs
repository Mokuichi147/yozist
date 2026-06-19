//! yozist-versioning — コミット履歴 + プラガブル CRDT/LWW マージエンジン。
//!
//! # 設計原則
//! - **拡張可能**: `CrdtFormat` トレイトを実装すれば対応フォーマットを増やせる。
//!   サードパーティクレートからの登録も想定。
//! - **書き込みの単一経路**: SMB/API/AI のどこから書く場合も `commit()` を経由。
//! - **並行性**: テキストは CRDT（自動マージ）、バイナリは LWW（最終書き込み勝ち）。
//!
//! # TODO
//! - [ ] `PlainTextCrdt`（yrs ベース）の本実装
//! - [ ] Markdown / JSON / CSV CRDT
//! - [ ] commit DAG（merge コミット）対応
//! - [ ] スナップショット圧縮間隔（N コミット毎にフル保存）
//! - [ ] `broadcast` チャネルによる変更通知

use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use yozist_core::{
    ActorId, BlobId, Commit, CommitId, FileId, FileMeta, FormatHint,
};
use yozist_db::SharedMetaStore;
use yozist_storage::{ByteStream, SharedBlobStore, StorageError};

pub mod registry;
pub mod text;
pub use registry::{CrdtRegistry, LwwFormat, PlainTextCrdt};
pub use text::{decode_text, detect_charset, encode_text, http_charset};

/// CRDT 状態。フォーマット実装側が任意の内部表現を保持する。
pub struct CrdtState {
    pub inner: Box<dyn std::any::Any + Send + Sync>,
}

impl std::fmt::Debug for CrdtState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CrdtState{{..}}")
    }
}

/// 編集操作（OpLog の 1 要素）。
#[derive(Debug, Clone)]
pub struct CrdtOp {
    pub actor: ActorId,
    pub bytes: bytes::Bytes,
}

/// 1 つのファイル種別を扱う CRDT/マージ実装。
#[async_trait]
pub trait CrdtFormat: Send + Sync {
    /// MIME 風の識別子（例: `text/plain`, `application/json`, `_/lww`）
    fn format_id(&self) -> &'static str;
    /// このフォーマットで処理すべきかどうか。
    fn detect(&self, hint: &FormatHint) -> bool;
    /// 正規化（load→serialize）が恒等で、入力バイトをそのまま blob へ
    /// ストリーミング保存してよいフォーマットなら `true`。
    /// `true` の場合、エンジンは本文をメモリに載せずに `BlobStore::put_stream`
    /// へ直接流す。差分計算が必要な CRDT フォーマットは `false`（既定）。
    fn supports_streaming(&self) -> bool {
        false
    }
    /// バイト列を CRDT 状態に取り込む。
    async fn load(&self, bytes: &[u8]) -> Result<CrdtState, VersioningError>;
    /// 編集操作（OpLog）を適用。
    async fn apply_ops(
        &self,
        state: &mut CrdtState,
        ops: &[CrdtOp],
    ) -> Result<(), VersioningError>;
    /// CRDT 状態をシリアライズ。
    async fn serialize(&self, state: &CrdtState) -> Result<Vec<u8>, VersioningError>;
    /// 2 つの状態を競合無くマージ。
    async fn merge(
        &self,
        a: &CrdtState,
        b: &CrdtState,
    ) -> Result<CrdtState, VersioningError>;
}

pub type SharedCrdtFormat = Arc<dyn CrdtFormat>;

/// バージョニングエンジン。`BlobStore` + `MetaStore` + `CrdtRegistry` を束ね、
/// 「書き込みの単一経路」を提供する。
pub struct VersioningEngine {
    pub registry: Arc<CrdtRegistry>,
    pub blob: SharedBlobStore,
    pub meta: SharedMetaStore,
}

/// クライアントへ提示する見かけのサイズ。blob は UTF-8 だが、SMB / HTTP では
/// 元 charset へ再エンコードして返すため、`FileMeta.size` も提示サイズ（再エンコード
/// 後の長さ）に揃える。これで「一覧のサイズ」と「open/read のサイズ」が一致し、
/// macOS 等が folder 上のサイズと実体の食い違いを reconcile できずループするのを防ぐ。
/// charset が無い（バイナリ）場合は blob 長そのまま。
fn presented_size(normalized: &[u8], charset: Option<&str>) -> u64 {
    match charset {
        Some(cs) => text::encode_text(&String::from_utf8_lossy(normalized), cs).len() as u64,
        None => normalized.len() as u64,
    }
}

impl VersioningEngine {
    pub fn new(
        registry: Arc<CrdtRegistry>,
        blob: SharedBlobStore,
        meta: SharedMetaStore,
    ) -> Self {
        Self {
            registry,
            blob,
            meta,
        }
    }

    /// 新規ファイルを作成し、初回コミットを記録する。
    pub async fn create_file(
        &self,
        display_name: impl Into<String>,
        content: &[u8],
        actor: ActorId,
        committed_by: Option<String>,
        committed_by_user_id: Option<i64>,
        hint_override: Option<FormatHint>,
    ) -> Result<(FileMeta, Commit), VersioningError> {
        let display_name = display_name.into();
        let now = time::OffsetDateTime::now_utc();

        let mut hint = hint_override.unwrap_or_else(|| FormatHint {
            extension: ext_of(&display_name),
            mime: None,
            first_bytes: Some(content.iter().take(64).copied().collect()),
            display_name: Some(display_name.clone()),
        });
        // フォーマット判定(resolve)の前に MIME を確定する。PlainTextCrdt::detect は
        // text/* を最優先で見るため、ここで埋めれば保存形式(CRDT/LWW)の選択にも効く。
        if hint.mime.is_none() {
            hint.mime = guess_mime(&display_name, content);
        }
        let fmt = self.registry.resolve(&hint);
        let mime = hint.mime.clone();
        // テキストは元エンコーディングを判定して保持する。blob には UTF-8 で
        // 正規化保存し、ダウンロード時にこの charset へ再エンコードして返す。
        let charset = if fmt.format_id() == "text/plain" {
            Some(text::detect_charset(content))
        } else {
            None
        };

        // 内容を一度フォーマット経由で正規化 (load -> apply -> serialize)
        let normalized = self.normalize(&fmt, content, actor).await?;
        let blob_id = self.blob.put(&normalized).await?;

        // FTS index: display_name + content (テキストフォーマット時のみ内容も)
        let content_str = if fmt.format_id() == "text/plain" {
            std::str::from_utf8(&normalized).unwrap_or("").to_string()
        } else {
            String::new()
        };
        let size = presented_size(&normalized, charset.as_deref());
        self.persist_create(
            display_name,
            mime,
            charset,
            blob_id,
            size,
            fmt.format_id(),
            actor,
            committed_by,
            committed_by_user_id,
            &content_str,
            now,
            &hint,
        )
        .await
    }

    /// 既存ファイルへの新規コミット。
    pub async fn commit(
        &self,
        file_id: FileId,
        new_content: &[u8],
        actor: ActorId,
        committed_by: Option<String>,
        committed_by_user_id: Option<i64>,
        message: Option<String>,
    ) -> Result<Commit, VersioningError> {
        let mut file = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or_else(|| VersioningError::NotFound(file_id))?;

        let mut hint = FormatHint {
            extension: ext_of(&file.display_name),
            mime: file.mime.clone(),
            first_bytes: Some(new_content.iter().take(64).copied().collect()),
            display_name: Some(file.display_name.clone()),
        };
        // mime 未設定の既存ファイルは、ここで拡張子/内容から補完する。
        if hint.mime.is_none() {
            hint.mime = guess_mime(&file.display_name, new_content);
        }
        let fmt = self.registry.resolve(&hint);

        // 既存状態を読み込み、新規 op を適用してから保存。
        let prev_bytes = if let Some(prev_commit_id) = file.current_commit {
            let commits = self.meta.list_commits(&file_id).await?;
            let prev = commits
                .into_iter()
                .find(|c| c.id == prev_commit_id)
                .ok_or_else(|| {
                    VersioningError::Conflict("current_commit references missing row".into())
                })?;
            self.blob.get(&prev.blob).await?.to_vec()
        } else {
            Vec::new()
        };

        let mut state = fmt.load(&prev_bytes).await?;
        let op = CrdtOp {
            actor,
            bytes: bytes::Bytes::copy_from_slice(new_content),
        };
        fmt.apply_ops(&mut state, &[op]).await?;
        let serialized = fmt.serialize(&state).await?;

        let blob_id = self.blob.put(&serialized).await?;
        let now = time::OffsetDateTime::now_utc();
        let content_str = if fmt.format_id() == "text/plain" {
            std::str::from_utf8(&serialized).unwrap_or("").to_string()
        } else {
            String::new()
        };
        // charset 未設定（この機能以前に作られた等）のテキストは新内容から補完する。
        // 既に確定済みの charset は「元の形式」を保つため上書きしない。
        let charset = if fmt.format_id() == "text/plain" {
            Some(text::detect_charset(new_content))
        } else {
            None
        };
        let size = presented_size(&serialized, file.charset.as_deref().or(charset.as_deref()));
        self.persist_commit(
            &mut file,
            blob_id,
            size,
            fmt.format_id(),
            actor,
            committed_by,
            committed_by_user_id,
            message,
            charset,
            &content_str,
            now,
            &hint,
        )
        .await
    }

    /// 既に「最終結果の全文」が手元にある場合に、CRDT 差分計算を経ずに直接 blob として
    /// コミットする（巨大ファイルの部分編集用）。
    ///
    /// plain text の `serialize` 出力は本文そのものなので、結果の blob は通常の
    /// `commit()` と同一になる。`commit()` が行う「前バージョンの全文読込 → yrs への
    /// ロード → 文字差分 → serialize」を丸ごと省くため、GB 級でも軽い。FTS 用の本文は
    /// 巨大ファイルでコミットが重くならないよう先頭のみに制限する。
    pub async fn commit_raw(
        &self,
        file_id: FileId,
        content: &[u8],
        actor: ActorId,
        committed_by: Option<String>,
        committed_by_user_id: Option<i64>,
        message: Option<String>,
    ) -> Result<Commit, VersioningError> {
        let mut file = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or_else(|| VersioningError::NotFound(file_id))?;

        let mut hint = FormatHint {
            extension: ext_of(&file.display_name),
            mime: file.mime.clone(),
            first_bytes: Some(content.iter().take(64).copied().collect()),
            display_name: Some(file.display_name.clone()),
        };
        if hint.mime.is_none() {
            hint.mime = guess_mime(&file.display_name, content);
        }
        let fmt = self.registry.resolve(&hint);

        let blob_id = self.blob.put(content).await?;
        let now = time::OffsetDateTime::now_utc();
        // charset は確定済みなら検出しない（巨大バイト列の走査を避ける）。
        let charset = if fmt.format_id() == "text/plain" && file.charset.is_none() {
            Some(text::detect_charset(content))
        } else {
            None
        };
        let size = presented_size(content, file.charset.as_deref().or(charset.as_deref()));
        // FTS 本文は巨大ファイルで重くならないよう先頭 256KiB に制限する。
        const FTS_MAX: usize = 256 * 1024;
        let fts_content = if fmt.format_id() == "text/plain" {
            let mut end = content.len().min(FTS_MAX);
            // UTF-8 継続バイトの途中で切らない。
            while end > 0 && end < content.len() && (content[end] & 0xC0) == 0x80 {
                end -= 1;
            }
            String::from_utf8_lossy(&content[..end]).into_owned()
        } else {
            String::new()
        };
        self.persist_commit(
            &mut file,
            blob_id,
            size,
            fmt.format_id(),
            actor,
            committed_by,
            committed_by_user_id,
            message,
            charset,
            &fts_content,
            now,
            &hint,
        )
        .await
    }

    /// 新規ファイルをストリームから作成する。
    ///
    /// フォーマット判定（拡張子/MIME）は本文を読む前に確定できるため、
    /// `supports_streaming()` なフォーマット（バイナリ=LWW）は本文をメモリに
    /// 載せず `BlobStore::put_stream` へ直接流す。テキスト等の CRDT 対象は
    /// 差分計算のため一度バッファし、既存 `create_file` に委譲する。
    pub async fn create_file_streaming(
        &self,
        display_name: impl Into<String>,
        stream: ByteStream,
        actor: ActorId,
        committed_by: Option<String>,
        committed_by_user_id: Option<i64>,
        hint_override: Option<FormatHint>,
    ) -> Result<(FileMeta, Commit), VersioningError> {
        let display_name = display_name.into();
        let mut hint = hint_override.unwrap_or_else(|| FormatHint {
            extension: ext_of(&display_name),
            mime: None,
            first_bytes: None,
            display_name: Some(display_name.clone()),
        });
        // フォーマット判定(resolve)の前に MIME を確定する。detect が text/* を
        // 最優先で見るため、保存形式(CRDT/LWW)の選択に MIME を反映できる。
        // 本文はバッファせず、拡張子で決まらない場合のみ先頭バイトを覗き、
        // 覗いた分は失わないようストリームへ連結し直す。
        let stream = if hint.mime.is_none() {
            let (mime, rewound) = resolve_stream_mime(&display_name, None, stream).await?;
            hint.mime = mime;
            rewound
        } else {
            stream
        };
        let fmt = self.registry.resolve(&hint);

        if fmt.supports_streaming() {
            let now = time::OffsetDateTime::now_utc();
            let (blob_id, size) = self.blob.put_stream(stream).await?;
            self.persist_create(
                display_name,
                hint.mime.clone(),
                None, // ストリーミング経路は LWW（バイナリ）のみ。charset は持たない。
                blob_id,
                size,
                fmt.format_id(),
                actor,
                committed_by,
                committed_by_user_id,
                "",
                now,
                &hint,
            )
            .await
        } else {
            // CRDT 経路へフォールバック。MIME 確定済みの hint を渡し二重推測を避ける。
            let buf = collect_stream(stream).await?;
            self.create_file(
                display_name,
                &buf,
                actor,
                committed_by,
                committed_by_user_id,
                Some(hint),
            )
            .await
        }
    }

    /// 単一タグを `upsert_tag` → `attach_tag` の冪等な 2 段で付与する内部ヘルパ。
    ///
    /// `persist_create` のシステムタグ付与と同じ作法で、失敗してもアップロード自体
    /// は壊さないよう警告に留める。付与は `file_tags` テーブルへ行うため、フィルタ／
    /// by-tags の絞り込み（`file_tags` 直参照）からは即座に辿れる。FTS のタグ列は
    /// 作成時点の内容で確定済みで本文を再取得せず安全に作り直せないため更新しない。
    async fn attach_meta_tag(&self, file_id: FileId, tag: yozist_core::Tag, what: &str) {
        match self.meta.upsert_tag(&tag).await {
            Ok(tag_id) => {
                if let Err(e) = self.meta.attach_tag(&file_id, &tag_id).await {
                    tracing::warn!("{what}の付与に失敗: {e}");
                }
            }
            Err(e) => tracing::warn!("{what}の登録に失敗: {e}"),
        }
    }

    /// 作成済みファイルにアップロード経路タグ `src:<source>` を付与する。
    /// `source` は `rest` / `smb` などアップロード経路の識別子（WebUI も REST 経由
    /// なので `rest`）。どのクライアントソフトかは `attach_client_tag` で表す。
    pub async fn attach_source_tag(&self, file_id: FileId, source: &str) {
        self.attach_meta_tag(file_id, yozist_tagging::source_tag(source), "ソースタグ")
            .await;
    }

    /// 作成済みファイルにクライアントソフト識別タグ `client:<name>` を付与する。
    /// `client` が空などで正規化結果が無い場合は何もしない。`src:`（経路）とは
    /// 独立した名前空間なので、経路タグと併存して両軸で絞り込める。
    pub async fn attach_client_tag(&self, file_id: FileId, client: &str) {
        if let Some(tag) = yozist_tagging::client_tag(client) {
            self.attach_meta_tag(file_id, tag, "クライアントタグ").await;
        }
    }

    /// 既存ファイルへの新規コミットをストリームから行う。
    /// バイナリ（LWW=全置換）は直前 blob を読まずにそのまま保存する。
    pub async fn commit_streaming(
        &self,
        file_id: FileId,
        stream: ByteStream,
        actor: ActorId,
        committed_by: Option<String>,
        committed_by_user_id: Option<i64>,
        message: Option<String>,
    ) -> Result<Commit, VersioningError> {
        let mut file = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or(VersioningError::NotFound(file_id))?;

        let mut hint = FormatHint {
            extension: ext_of(&file.display_name),
            mime: file.mime.clone(),
            first_bytes: None,
            display_name: Some(file.display_name.clone()),
        };
        // mime 未設定なら、本文をバッファせず先頭バイトから補完する。
        let stream = if hint.mime.is_none() {
            let (mime, rewound) = resolve_stream_mime(&file.display_name, None, stream).await?;
            hint.mime = mime;
            rewound
        } else {
            stream
        };
        let fmt = self.registry.resolve(&hint);

        if fmt.supports_streaming() {
            let now = time::OffsetDateTime::now_utc();
            let (blob_id, size) = self.blob.put_stream(stream).await?;
            self.persist_commit(
                &mut file,
                blob_id,
                size,
                fmt.format_id(),
                actor,
                committed_by,
                committed_by_user_id,
                message,
                None, // ストリーミング経路は LWW（バイナリ）のみ。charset は持たない。
                "",
                now,
                &hint,
            )
            .await
        } else {
            let buf = collect_stream(stream).await?;
            self.commit(file_id, &buf, actor, committed_by, committed_by_user_id, message)
                .await
        }
    }

    /// アップロードによる内容の全置換。既存バージョンとはマージせず、新しい内容を
    /// そのまま新コミットとして記録する。形式・mime・charset・表示名は新しい名前
    /// ＋内容から判定し直すため、別形式へ差し替えても前バージョンの解釈
    /// （例: PNG をテキストとして load）に引きずられず破損しない。
    ///
    /// `commit()`（CRDT マージ）と異なり前コミットの内容を読まないので、テキスト
    /// 形式でも空状態から正規化する（= 新内容そのもの）。メタ更新は `persist_commit`
    /// 内の単一書き込みに集約し、blob 保存に成功した場合のみ反映する。
    pub async fn replace_streaming(
        &self,
        file_id: FileId,
        new_name: String,
        stream: ByteStream,
        actor: ActorId,
        committed_by: Option<String>,
        committed_by_user_id: Option<i64>,
        message: Option<String>,
    ) -> Result<Commit, VersioningError> {
        let mut file = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or(VersioningError::NotFound(file_id))?;

        // 表示名を更新し、mime/charset は確定済みでも破棄して再判定させる
        // （persist_commit は None のときだけ hint/charset で補完する）。
        file.display_name = new_name;
        file.mime = None;
        file.charset = None;

        let mut hint = FormatHint {
            extension: ext_of(&file.display_name),
            mime: None,
            first_bytes: None,
            display_name: Some(file.display_name.clone()),
        };
        // mime を確定（PlainTextCrdt::detect 等の保存形式選択に効かせる）。
        // 本文をバッファせず先頭バイトを覗き、ストリームは巻き戻す。
        let (mime, stream) = resolve_stream_mime(&file.display_name, None, stream).await?;
        hint.mime = mime;
        let fmt = self.registry.resolve(&hint);
        let now = time::OffsetDateTime::now_utc();

        if fmt.supports_streaming() {
            // バイナリ(LWW): 本文をメモリに載せず blob へ直接流す。
            let (blob_id, size) = self.blob.put_stream(stream).await?;
            self.persist_commit(
                &mut file, blob_id, size, fmt.format_id(), actor, committed_by,
                committed_by_user_id, message, None, "", now, &hint,
            )
            .await
        } else {
            // テキスト等: バッファし、空状態から正規化（前バージョンとマージしない）。
            let buf = collect_stream(stream).await?;
            let normalized = self.normalize(&fmt, &buf, actor).await?;
            let blob_id = self.blob.put(&normalized).await?;
            let content_str = if fmt.format_id() == "text/plain" {
                std::str::from_utf8(&normalized).unwrap_or("").to_string()
            } else {
                String::new()
            };
            let charset = if fmt.format_id() == "text/plain" {
                Some(text::detect_charset(&buf))
            } else {
                None
            };
            let size = presented_size(&normalized, charset.as_deref());
            self.persist_commit(
                &mut file,
                blob_id,
                size,
                fmt.format_id(),
                actor,
                committed_by,
                committed_by_user_id,
                message,
                charset,
                &content_str,
                now,
                &hint,
            )
            .await
        }
    }

    /// 新規ファイルの DB 反映（file + commit + current_commit 更新 + FTS）。
    /// buffered/streaming 両経路の共通部。
    #[allow(clippy::too_many_arguments)]
    async fn persist_create(
        &self,
        display_name: String,
        mime: Option<String>,
        charset: Option<String>,
        blob_id: BlobId,
        size: u64,
        format_id: &str,
        actor: ActorId,
        committed_by: Option<String>,
        committed_by_user_id: Option<i64>,
        fts_content: &str,
        now: time::OffsetDateTime,
        hint: &FormatHint,
    ) -> Result<(FileMeta, Commit), VersioningError> {
        let file = FileMeta {
            id: FileId::new(),
            display_name,
            size,
            mime,
            charset,
            current_commit: None,
            created_at: now,
            updated_at: now,
            deleted: false,
            deleted_at: None,
            // 作成者/更新者ラベルは利用者を知る上位層（API 等）が書き込む。
            created_by: None,
            updated_by: None,
            created_by_user_id: None,
            updated_by_user_id: None,
        };
        self.meta.insert_file(&file).await?;

        let commit = Commit {
            id: CommitId::new(),
            file_id: file.id,
            parent: None,
            actor,
            blob: blob_id,
            format_id: format_id.to_string(),
            timestamp: now,
            message: Some("create".into()),
            committed_by,
            committed_by_user_id,
        };
        self.meta.insert_commit(&commit).await?;

        let mut updated = file.clone();
        updated.current_commit = Some(commit.id);
        updated.updated_at = now;
        self.meta.update_file(&updated).await?;

        // システムタグ（ext:/type:）を拡張子・MIME から自動付与する。
        // 付与失敗はファイル作成を妨げないよう警告に留める。
        for tag in yozist_tagging::system_tags_for(hint) {
            match self.meta.upsert_tag(&tag).await {
                Ok(tag_id) => {
                    if let Err(e) = self.meta.attach_tag(&updated.id, &tag_id).await {
                        tracing::warn!("システムタグの付与に失敗: {e}");
                    }
                }
                Err(e) => tracing::warn!("システムタグの登録に失敗: {e}"),
            }
        }

        // FTS には付与済みタグ名も含める。
        let tag_names = self
            .meta
            .list_tags_of(&updated.id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
            .join(" ");
        let _ = self
            .meta
            .upsert_fts(&updated.id, &updated.display_name, &tag_names, fts_content)
            .await;

        Ok((updated, commit))
    }

    /// 既存ファイルへのコミットの DB 反映（commit + current_commit 更新 + FTS）。
    /// buffered/streaming 両経路の共通部。`file` は呼び出し後に最新状態へ更新される。
    #[allow(clippy::too_many_arguments)]
    async fn persist_commit(
        &self,
        file: &mut FileMeta,
        blob_id: BlobId,
        size: u64,
        format_id: &str,
        actor: ActorId,
        committed_by: Option<String>,
        committed_by_user_id: Option<i64>,
        message: Option<String>,
        charset: Option<String>,
        fts_content: &str,
        now: time::OffsetDateTime,
        hint: &FormatHint,
    ) -> Result<Commit, VersioningError> {
        let commit = Commit {
            id: CommitId::new(),
            file_id: file.id,
            parent: file.current_commit,
            actor,
            blob: blob_id,
            format_id: format_id.to_string(),
            timestamp: now,
            message,
            committed_by,
            committed_by_user_id,
        };
        self.meta.insert_commit(&commit).await?;

        file.current_commit = Some(commit.id);
        file.size = size;
        file.updated_at = now;
        // 更新者ラベルはユーザーを知る層（REST API）がコミット後に書き込む。
        // ここでリセットしないと SMB 等の経路で更新した際に前回の名前が残り、
        // 誤った更新者が表示され続ける。
        file.updated_by = None;
        // mime 未設定だった既存ファイルを確定済み hint で補完する。
        if file.mime.is_none() {
            file.mime = hint.mime.clone();
        }
        // charset 未設定のテキストファイルのみ補完する。確定済みは「元の形式」を
        // 保つため上書きしない（後続コミットが別エンコーディングでも維持）。
        if file.charset.is_none() {
            file.charset = charset;
        }
        self.meta.update_file(file).await?;

        // システムタグ（ext:/type:）を補完。upsert は同名を既存IDへ集約し、
        // attach は ON CONFLICT DO NOTHING なので冪等。付与失敗は警告に留める。
        for tag in yozist_tagging::system_tags_for(hint) {
            match self.meta.upsert_tag(&tag).await {
                Ok(tag_id) => {
                    if let Err(e) = self.meta.attach_tag(&file.id, &tag_id).await {
                        tracing::warn!("システムタグの付与に失敗: {e}");
                    }
                }
                Err(e) => tracing::warn!("システムタグの登録に失敗: {e}"),
            }
        }

        // FTS 更新 (display_name とタグ一覧と内容)
        let tag_names = self
            .meta
            .list_tags_of(&file.id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
            .join(" ");
        let _ = self
            .meta
            .upsert_fts(&file.id, &file.display_name, &tag_names, fts_content)
            .await;

        Ok(commit)
    }

    /// 指定したコミットの内容を取得する（履歴閲覧用）。
    pub async fn read_at_commit(
        &self,
        file_id: FileId,
        commit_id: CommitId,
    ) -> Result<Vec<u8>, VersioningError> {
        let _ = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or(VersioningError::NotFound(file_id))?;
        let blob_id = self.find_blob(&file_id, commit_id).await?;
        Ok(self.blob.get(&blob_id).await?.to_vec())
    }

    /// `commit_id` 時点の内容を新規コミットとして再投入する (= rollback)。
    /// `commit()` を内部で呼ぶので CRDT/LWW フォーマットの正規化が行われ、
    /// 新しい履歴 1 件として残る（履歴を破壊的に切り詰めない）。
    pub async fn rollback_to(
        &self,
        file_id: FileId,
        commit_id: CommitId,
        actor: ActorId,
        committed_by: Option<String>,
        committed_by_user_id: Option<i64>,
        message: Option<String>,
    ) -> Result<Commit, VersioningError> {
        let bytes = self.read_at_commit(file_id, commit_id).await?;
        let msg = message.unwrap_or_else(|| format!("rollback to {commit_id}"));
        self.commit(file_id, &bytes, actor, committed_by, committed_by_user_id, Some(msg))
            .await
    }

    /// ファイル名（display_name）だけを変更する。新しいコミットは作らない。
    ///
    /// 拡張子の変更に追従して mime と system タグ（ext:/type:）を貼り替え、
    /// FTS を新しい名前で更新する。テキストファイルは検索インデックスから本文が
    /// 消えないよう、現行内容を読み直して FTS の content も維持する。
    pub async fn rename_file(
        &self,
        file_id: FileId,
        new_name: impl Into<String>,
        updated_by: Option<String>,
        updated_by_user_id: Option<i64>,
    ) -> Result<FileMeta, VersioningError> {
        let new_name = new_name.into();
        let mut file = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or(VersioningError::NotFound(file_id))?;

        // テキストファイルのみ現行内容を読む（FTS 本文の維持と mime 判定に使う）。
        // バイナリは FTS 本文を持たず mime は拡張子で判定するため、巨大 blob を
        // 読み込まない。
        let content: Vec<u8> = if file.charset.is_some() {
            self.read_current(file_id).await.unwrap_or_default()
        } else {
            Vec::new()
        };

        file.display_name = new_name;
        file.updated_at = time::OffsetDateTime::now_utc();
        file.updated_by = updated_by;
        file.updated_by_user_id = updated_by_user_id;
        // 拡張子から mime を再判定。判定できなければ従来値を保持する。
        if let Some(m) = guess_mime(&file.display_name, &content) {
            if m != "application/octet-stream" {
                file.mime = Some(m);
            }
        }
        self.meta.update_file(&file).await?;

        // system タグ（ext:/type:）を貼り替える。旧 system タグを外し、
        // 新しい名前・mime から再付与する（手動/AI タグはそのまま残す）。
        let hint = FormatHint {
            extension: ext_of(&file.display_name),
            mime: file.mime.clone(),
            first_bytes: None,
            display_name: Some(file.display_name.clone()),
        };
        if let Ok(existing) = self.meta.list_tags_of(&file_id).await {
            for t in existing
                .iter()
                .filter(|t| t.kind == yozist_core::TagKind::System)
            {
                let _ = self.meta.detach_tag(&file_id, &t.id).await;
            }
        }
        for tag in yozist_tagging::system_tags_for(&hint) {
            if let Ok(tag_id) = self.meta.upsert_tag(&tag).await {
                let _ = self.meta.attach_tag(&file_id, &tag_id).await;
            }
        }

        // FTS 更新（display_name + タグ + 本文）。本文はテキストのみ。
        let tag_names = self
            .meta
            .list_tags_of(&file_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
            .join(" ");
        let fts_content = if file.charset.is_some() {
            String::from_utf8_lossy(&content).into_owned()
        } else {
            String::new()
        };
        let _ = self
            .meta
            .upsert_fts(&file_id, &file.display_name, &tag_names, &fts_content)
            .await;

        Ok(file)
    }

    /// 現在の内容を取得する。
    pub async fn read_current(&self, file_id: FileId) -> Result<Vec<u8>, VersioningError> {
        let file = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or(VersioningError::NotFound(file_id))?;
        let commit_id = file
            .current_commit
            .ok_or_else(|| VersioningError::Conflict("file has no commits".into()))?;
        let blob_id = self.find_blob(&file_id, commit_id).await?;
        Ok(self.blob.get(&blob_id).await?.to_vec())
    }

    async fn find_blob(
        &self,
        file_id: &FileId,
        commit_id: CommitId,
    ) -> Result<BlobId, VersioningError> {
        let commits = self.meta.list_commits(file_id).await?;
        commits
            .into_iter()
            .find(|c| c.id == commit_id)
            .map(|c| c.blob)
            .ok_or_else(|| VersioningError::Conflict("commit not found in log".into()))
    }

    async fn normalize(
        &self,
        fmt: &SharedCrdtFormat,
        content: &[u8],
        actor: ActorId,
    ) -> Result<Vec<u8>, VersioningError> {
        let mut state = fmt.load(content).await?;
        // 同じバイト列で 1 op 適用しても結果は変わらない実装が多いため、空 op 適用は省略。
        // ただし将来のフォーマット実装に備え、load 後にそのまま serialize する経路を確保。
        let _ = actor;
        let _ = &mut state;
        fmt.serialize(&state).await
    }
}

fn ext_of(name: &str) -> Option<String> {
    std::path::Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
}

/// ファイル名と先頭バイトから MIME タイプを推測する。
///
/// 拡張子（`mime_guess`）からの具体的な判定を優先し、拡張子が未知、
/// または汎用の `application/octet-stream` にしかならない場合は先頭バイトの
/// マジックナンバー（`infer`）で補う。どちらでも不明なら `None`。
fn guess_mime(name: &str, head: &[u8]) -> Option<String> {
    let by_ext = mime_guess::from_path(name)
        .first()
        .map(|m| m.essence_str().to_string());
    if let Some(m) = &by_ext {
        if m != "application/octet-stream" {
            return by_ext;
        }
    }
    // 拡張子が未知 / 汎用バイナリ → 中身のマジックナンバーで判定。
    // それも不明なら拡張子由来（octet-stream）にフォールバック。
    infer::get(head)
        .map(|k| k.mime_type().to_string())
        .or(by_ext)
}

/// ストリーム本文をバッファせずに MIME を判定する。
///
/// ヒント → 拡張子の順に確定できればストリームに触れず返す。どちらでも
/// 確定できない場合のみ先頭バイトを覗き、覗いたチャンクは失わないよう
/// 連結し直したストリームと共に返す。
async fn resolve_stream_mime(
    name: &str,
    hint_mime: Option<String>,
    stream: ByteStream,
) -> Result<(Option<String>, ByteStream), StorageError> {
    if let Some(m) = hint_mime {
        return Ok((Some(m), stream));
    }
    let by_ext = mime_guess::from_path(name)
        .first()
        .map(|m| m.essence_str().to_string());
    if let Some(m) = &by_ext {
        if m != "application/octet-stream" {
            return Ok((by_ext, stream));
        }
    }
    // 拡張子が未知 / 汎用バイナリ → 先頭バイトを覗いて判定。
    let (head, stream) = peek_head(stream, 512).await?;
    let mime = infer::get(&head)
        .map(|k| k.mime_type().to_string())
        .or(by_ext);
    Ok((mime, stream))
}

/// ストリーム先頭を最大 `limit` バイト読み取り、読み取ったチャンクを
/// 先頭に連結し直したストリームと、判定用のバイト列を返す。
/// 本文をメモリへ全展開しないための覗き見ヘルパー。
async fn peek_head(
    mut stream: ByteStream,
    limit: usize,
) -> Result<(Vec<u8>, ByteStream), StorageError> {
    let mut head = Vec::new();
    let mut buffered: Vec<bytes::Bytes> = Vec::new();
    while head.len() < limit {
        match stream.next().await {
            Some(Ok(chunk)) => {
                head.extend_from_slice(&chunk);
                buffered.push(chunk);
            }
            Some(Err(e)) => return Err(e),
            None => break,
        }
    }
    let prefix = futures::stream::iter(buffered.into_iter().map(Ok));
    let combined = prefix.chain(stream).boxed();
    Ok((head, combined))
}

/// ストリームをメモリへ集約する（CRDT/テキスト経路用フォールバック）。
async fn collect_stream(mut stream: ByteStream) -> Result<Vec<u8>, VersioningError> {
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(buf)
}

#[derive(Debug, thiserror::Error)]
pub enum VersioningError {
    #[error("file not found: {0}")]
    NotFound(FileId),
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("format mismatch: {0}")]
    FormatMismatch(String),
    #[error("merge conflict: {0}")]
    Conflict(String),
    #[error("storage error: {0}")]
    Storage(#[from] yozist_storage::StorageError),
    #[error("db error: {0}")]
    Db(#[from] yozist_db::DbError),
}

impl From<VersioningError> for yozist_core::Error {
    fn from(e: VersioningError) -> Self {
        yozist_core::Error::Versioning(e.to_string())
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use std::sync::Arc;
    use yozist_db::SqliteMetaStore;
    use yozist_storage::FsBlobStore;

    async fn engine() -> (VersioningEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob = Arc::new(FsBlobStore::new(dir.path().join("blobs")).await.unwrap());
        let meta = Arc::new(SqliteMetaStore::open_in_memory().await.unwrap());
        let reg = Arc::new(CrdtRegistry::with_defaults());
        (VersioningEngine::new(reg, blob, meta), dir)
    }

    #[tokio::test]
    async fn create_and_read_roundtrip() {
        let (eng, _td) = engine().await;
        let (file, commit) = eng
            .create_file("note.md", b"hello", ActorId::new(), None, None, None)
            .await
            .unwrap();
        assert!(file.current_commit.is_some());
        assert_eq!(commit.format_id, "text/plain");
        let bytes = eng.read_current(file.id).await.unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn commit_chains_history() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        let (file, c1) = eng
            .create_file("doc.txt", b"v1", actor, None, None, None)
            .await
            .unwrap();
        let c2 = eng
            .commit(file.id, b"v2", actor, None, None, Some("update".into()))
            .await
            .unwrap();
        let c3 = eng
            .commit(file.id, b"v3", actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(c2.parent, Some(c1.id));
        assert_eq!(c3.parent, Some(c2.id));
        assert_eq!(eng.read_current(file.id).await.unwrap(), b"v3");

        let log = eng.meta.list_commits(&file.id).await.unwrap();
        assert_eq!(log.len(), 3);
    }

    #[tokio::test]
    async fn records_committed_by_label() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // 作成・更新ともに実行ユーザーのラベルと不変キー(user.id)が各コミットへ記録される。
        let (file, c1) = eng
            .create_file("note.txt", b"v1", actor, Some("alice".into()), Some(1), None)
            .await
            .unwrap();
        assert_eq!(c1.committed_by.as_deref(), Some("alice"));
        assert_eq!(c1.committed_by_user_id, Some(1));
        let c2 = eng
            .commit(file.id, b"v2", actor, Some("bob".into()), Some(2), None)
            .await
            .unwrap();
        assert_eq!(c2.committed_by.as_deref(), Some("bob"));
        assert_eq!(c2.committed_by_user_id, Some(2));
        // ラベル・user.id 未指定（SMB 等）は NULL のまま。
        let c3 = eng
            .commit(file.id, b"v3", actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(c3.committed_by, None);
        assert_eq!(c3.committed_by_user_id, None);

        // DB へ往復しても保持される。
        let log = eng.meta.list_commits(&file.id).await.unwrap();
        assert_eq!(log[0].committed_by.as_deref(), Some("alice"));
        assert_eq!(log[0].committed_by_user_id, Some(1));
        assert_eq!(log[1].committed_by.as_deref(), Some("bob"));
        assert_eq!(log[1].committed_by_user_id, Some(2));
        assert_eq!(log[2].committed_by, None);
        assert_eq!(log[2].committed_by_user_id, None);
    }

    #[tokio::test]
    async fn lww_fallback_for_binary() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        let bytes = vec![0xFFu8, 0xD8, 0xFF]; // JPEG マジック
        let (file, commit) = eng
            .create_file("photo.jpg", &bytes, actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(commit.format_id, "_/lww");
        let got = eng.read_current(file.id).await.unwrap();
        assert_eq!(got, bytes);
    }

    fn byte_stream(chunks: Vec<&'static [u8]>) -> yozist_storage::ByteStream {
        let items: Vec<Result<bytes::Bytes, yozist_storage::StorageError>> = chunks
            .into_iter()
            .map(|c| Ok(bytes::Bytes::from_static(c)))
            .collect();
        futures::stream::iter(items).boxed()
    }

    #[tokio::test]
    async fn streaming_binary_uses_lww_and_roundtrips() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // バイナリ（.bin）は LWW 経路でストリーム保存される。
        let stream = byte_stream(vec![b"\x00\x01\x02", b"\x03\x04"]);
        let (file, commit) = eng
            .create_file_streaming("movie.bin", stream, actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(commit.format_id, "_/lww");
        assert_eq!(file.size, 5);
        assert_eq!(eng.read_current(file.id).await.unwrap(), vec![0, 1, 2, 3, 4]);

        // 続けて commit_streaming で全置換できる。
        let c2 = eng
            .commit_streaming(file.id, byte_stream(vec![b"\xAA\xBB"]), actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(c2.parent, Some(commit.id));
        assert_eq!(eng.read_current(file.id).await.unwrap(), vec![0xAA, 0xBB]);
    }

    #[tokio::test]
    async fn replace_streaming_switches_format_without_corrupting() {
        // 回帰: 画像(LWW)にテキストをアップロードして全置換しても、前バージョンの
        // バイナリをテキスト形式で load してマージする破損が起きないこと。
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // 画像(LWW)として作成。
        let (file, c1) = eng
            .create_file_streaming(
                "picture.png",
                byte_stream(vec![b"\x89PNG\r\n\x1a\n\x00\x00data"]),
                actor,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(c1.format_id, "_/lww");
        assert_eq!(file.mime.as_deref(), Some("image/png"));

        // テキストファイルで全置換。
        let text = b"hello\nworld";
        let c2 = eng
            .replace_streaming(
                file.id,
                "notes.txt".into(),
                byte_stream(vec![text]),
                actor,
                None,
                None,
                Some("upload notes.txt".into()),
            )
            .await
            .unwrap();
        // 履歴は連結され、形式・mime・charset・表示名が新内容から再判定される。
        assert_eq!(c2.parent, Some(c1.id));
        assert_eq!(c2.format_id, "text/plain");
        // 破損していない: 取り出した内容が渡したテキストと完全一致。
        assert_eq!(eng.read_current(file.id).await.unwrap(), text.to_vec());
        let got = eng.meta.get_file(&file.id).await.unwrap().unwrap();
        assert_eq!(got.display_name, "notes.txt");
        assert_eq!(got.mime.as_deref(), Some("text/plain"));
        assert_eq!(got.charset.as_deref(), Some("UTF-8"));
    }

    #[tokio::test]
    async fn create_file_sets_mime_from_extension() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // JPEG マジックを持つ .jpg。拡張子から image/jpeg を確定する。
        let bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let (file, _c) = eng
            .create_file("photo.jpg", &bytes, actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(file.mime.as_deref(), Some("image/jpeg"));
    }

    #[tokio::test]
    async fn streaming_sets_mime_from_magic_number_when_extension_unknown() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // 拡張子なしのファイル名。先頭の PNG マジックから image/png を判定する。
        let stream = byte_stream(vec![b"\x89PNG\r\n\x1a\n", b"\x00\x00rest"]);
        let (file, commit) = eng
            .create_file_streaming("blob", stream, actor, None, None, None)
            .await
            .unwrap();
        // LWW 経路を通り、本文は欠落せず保存される。
        assert_eq!(commit.format_id, "_/lww");
        assert_eq!(
            eng.read_current(file.id).await.unwrap(),
            b"\x89PNG\r\n\x1a\n\x00\x00rest"
        );
        assert_eq!(file.mime.as_deref(), Some("image/png"));
    }

    #[tokio::test]
    async fn commit_backfills_mime_and_system_tags() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // この変更前に作られた想定の mime=None ファイルを直接用意する。
        let now = time::OffsetDateTime::now_utc();
        let file = FileMeta {
            id: FileId::new(),
            display_name: "photo.jpg".into(),
            size: 0,
            mime: None,
            charset: None,
            current_commit: None,
            created_at: now,
            updated_at: now,
            deleted: false,
            deleted_at: None,
            created_by: None,
            updated_by: None,
            created_by_user_id: None,
            updated_by_user_id: None,
        };
        eng.meta.insert_file(&file).await.unwrap();
        // commit すると mime が補完され ext:/type: タグが付く。
        eng.commit(file.id, &[0xFF, 0xD8, 0xFF, 0xE0], actor, None, None, None)
            .await
            .unwrap();
        let got = eng.meta.get_file(&file.id).await.unwrap().unwrap();
        assert_eq!(got.mime.as_deref(), Some("image/jpeg"));
        let names: Vec<String> = eng
            .meta
            .list_tags_of(&file.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(names.contains(&"ext:jpg".to_string()), "tags={names:?}");
        assert!(names.contains(&"type:image".to_string()), "tags={names:?}");
    }

    #[tokio::test]
    async fn create_file_attaches_system_tags() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        let (file, _c) = eng
            .create_file("photo.jpg", &[0xFF, 0xD8, 0xFF, 0xE0], actor, None, None, None)
            .await
            .unwrap();
        let names: Vec<String> = eng
            .meta
            .list_tags_of(&file.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        // 拡張子と MIME カテゴリのシステムタグが自動付与される。
        assert!(names.contains(&"ext:jpg".to_string()), "tags={names:?}");
        assert!(names.contains(&"type:image".to_string()), "tags={names:?}");
    }

    #[tokio::test]
    async fn attach_source_tag_adds_src_tag() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        let (file, _c) = eng
            .create_file("note.txt", b"hi", actor, None, None, None)
            .await
            .unwrap();
        eng.attach_source_tag(file.id, "rest").await;
        let names: Vec<String> = eng
            .meta
            .list_tags_of(&file.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(names.contains(&"src:rest".to_string()), "tags={names:?}");
    }

    #[tokio::test]
    async fn attach_client_tag_adds_client_tag() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        let (file, _c) = eng
            .create_file("note.txt", b"hi", actor, None, None, None)
            .await
            .unwrap();
        eng.attach_client_tag(file.id, " MyApp ").await;
        eng.attach_client_tag(file.id, "   ").await; // 空は無視される
        let names: Vec<String> = eng
            .meta
            .list_tags_of(&file.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(names.contains(&"client:myapp".to_string()), "tags={names:?}");
    }

    #[tokio::test]
    async fn mime_hint_drives_crdt_format_selection() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // 拡張子が無くても MIME が text/* なら CRDT(text/plain) 経路で保存される。
        // = 保存形式の選択に MIME が効いていることの確認。
        let hint = FormatHint {
            extension: None,
            mime: Some("text/x-custom".into()),
            first_bytes: None,
            display_name: None,
        };
        let (file, commit) = eng
            .create_file("data", b"hello", actor, None, None, Some(hint))
            .await
            .unwrap();
        assert_eq!(commit.format_id, "text/plain");
        assert_eq!(file.mime.as_deref(), Some("text/x-custom"));
    }

    #[tokio::test]
    async fn guessed_text_mime_drives_crdt_for_unlisted_extension() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // .vtt は detect の拡張子リストに無いが mime_guess は text/vtt を返すため、
        // resolve 前に確定した MIME により CRDT 経路が選ばれる。
        let stream = byte_stream(vec![b"WEBVTT\n\n", b"00:00.000 --> 00:01.000\nhi"]);
        let (file, commit) = eng
            .create_file_streaming("subtitle.vtt", stream, actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(commit.format_id, "text/plain");
        assert_eq!(file.mime.as_deref(), Some("text/vtt"));
    }

    #[tokio::test]
    async fn shift_jis_text_is_decoded_stored_utf8_and_charset_recorded() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        let text = "こんにちは、世界";
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode(text);
        assert!(sjis.iter().any(|b| *b >= 0x80), "Shift-JIS のはず");

        // 以前は from_utf8 で失敗していたケース。今は取り込めて charset が記録される。
        let (file, commit) = eng
            .create_file("memo.txt", &sjis, actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(commit.format_id, "text/plain");
        assert_eq!(file.charset.as_deref(), Some("Shift_JIS"));

        // blob は UTF-8 平文で保存される（他ツール互換）。
        let stored = eng.read_current(file.id).await.unwrap();
        assert_eq!(std::str::from_utf8(&stored).unwrap(), text);

        // ダウンロード相当の再エンコードで元の Shift-JIS バイト列に戻る。
        let restored = encode_text(&String::from_utf8_lossy(&stored), "Shift_JIS");
        assert_eq!(restored, sjis.to_vec());
    }

    #[tokio::test]
    async fn utf16le_text_roundtrips_via_charset() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        let text = "日本語 mixed テキスト";
        let mut bytes = vec![0xFF, 0xFE]; // UTF-16LE BOM
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let (file, _commit) = eng
            .create_file("note.txt", &bytes, actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(file.charset.as_deref(), Some("UTF-16LE"));
        let stored = eng.read_current(file.id).await.unwrap();
        assert_eq!(std::str::from_utf8(&stored).unwrap(), text);
        let restored = encode_text(&String::from_utf8_lossy(&stored), "UTF-16LE");
        assert_eq!(restored, bytes);
    }

    #[tokio::test]
    async fn binary_has_no_charset() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        let (file, _c) = eng
            .create_file("photo.jpg", &[0xFF, 0xD8, 0xFF, 0xE0], actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(file.charset, None);
    }

    #[tokio::test]
    async fn streaming_text_falls_back_to_buffered_crdt() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // テキスト（.md）は CRDT 経路へフォールバックし text/plain になる。
        let stream = byte_stream(vec![b"hello ", b"world"]);
        let (file, commit) = eng
            .create_file_streaming("note.md", stream, actor, None, None, None)
            .await
            .unwrap();
        assert_eq!(commit.format_id, "text/plain");
        assert_eq!(eng.read_current(file.id).await.unwrap(), b"hello world");
    }
}
