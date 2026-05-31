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
use yozist_storage::{ByteStream, SharedBlobStore};

pub mod registry;
pub use registry::{CrdtRegistry, LwwFormat, PlainTextCrdt};

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
        hint_override: Option<FormatHint>,
    ) -> Result<(FileMeta, Commit), VersioningError> {
        let display_name = display_name.into();
        let now = time::OffsetDateTime::now_utc();

        let hint = hint_override.unwrap_or_else(|| FormatHint {
            extension: ext_of(&display_name),
            mime: None,
            first_bytes: Some(content.iter().take(64).copied().collect()),
            display_name: Some(display_name.clone()),
        });
        let fmt = self.registry.resolve(&hint);

        // 内容を一度フォーマット経由で正規化 (load -> apply -> serialize)
        let normalized = self.normalize(&fmt, content, actor).await?;
        let blob_id = self.blob.put(&normalized).await?;

        // FTS index: display_name + content (テキストフォーマット時のみ内容も)
        let content_str = if fmt.format_id() == "text/plain" {
            std::str::from_utf8(&normalized).unwrap_or("").to_string()
        } else {
            String::new()
        };
        self.persist_create(
            display_name,
            hint.mime.clone(),
            blob_id,
            normalized.len() as u64,
            fmt.format_id(),
            actor,
            &content_str,
            now,
        )
        .await
    }

    /// 既存ファイルへの新規コミット。
    pub async fn commit(
        &self,
        file_id: FileId,
        new_content: &[u8],
        actor: ActorId,
        message: Option<String>,
    ) -> Result<Commit, VersioningError> {
        let mut file = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or_else(|| VersioningError::NotFound(file_id))?;

        let hint = FormatHint {
            extension: ext_of(&file.display_name),
            mime: file.mime.clone(),
            first_bytes: Some(new_content.iter().take(64).copied().collect()),
            display_name: Some(file.display_name.clone()),
        };
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
        self.persist_commit(
            &mut file,
            blob_id,
            serialized.len() as u64,
            fmt.format_id(),
            actor,
            message,
            &content_str,
            now,
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
        hint_override: Option<FormatHint>,
    ) -> Result<(FileMeta, Commit), VersioningError> {
        let display_name = display_name.into();
        let hint = hint_override.clone().unwrap_or_else(|| FormatHint {
            extension: ext_of(&display_name),
            mime: None,
            first_bytes: None,
            display_name: Some(display_name.clone()),
        });
        let fmt = self.registry.resolve(&hint);

        if fmt.supports_streaming() {
            let now = time::OffsetDateTime::now_utc();
            let (blob_id, size) = self.blob.put_stream(stream).await?;
            self.persist_create(
                display_name,
                hint.mime.clone(),
                blob_id,
                size,
                fmt.format_id(),
                actor,
                "",
                now,
            )
            .await
        } else {
            let buf = collect_stream(stream).await?;
            self.create_file(display_name, &buf, actor, hint_override)
                .await
        }
    }

    /// 既存ファイルへの新規コミットをストリームから行う。
    /// バイナリ（LWW=全置換）は直前 blob を読まずにそのまま保存する。
    pub async fn commit_streaming(
        &self,
        file_id: FileId,
        stream: ByteStream,
        actor: ActorId,
        message: Option<String>,
    ) -> Result<Commit, VersioningError> {
        let mut file = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or(VersioningError::NotFound(file_id))?;

        let hint = FormatHint {
            extension: ext_of(&file.display_name),
            mime: file.mime.clone(),
            first_bytes: None,
            display_name: Some(file.display_name.clone()),
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
                message,
                "",
                now,
            )
            .await
        } else {
            let buf = collect_stream(stream).await?;
            self.commit(file_id, &buf, actor, message).await
        }
    }

    /// 新規ファイルの DB 反映（file + commit + current_commit 更新 + FTS）。
    /// buffered/streaming 両経路の共通部。
    #[allow(clippy::too_many_arguments)]
    async fn persist_create(
        &self,
        display_name: String,
        mime: Option<String>,
        blob_id: BlobId,
        size: u64,
        format_id: &str,
        actor: ActorId,
        fts_content: &str,
        now: time::OffsetDateTime,
    ) -> Result<(FileMeta, Commit), VersioningError> {
        let file = FileMeta {
            id: FileId::new(),
            display_name,
            size,
            mime,
            current_commit: None,
            created_at: now,
            updated_at: now,
            deleted: false,
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
        };
        self.meta.insert_commit(&commit).await?;

        let mut updated = file.clone();
        updated.current_commit = Some(commit.id);
        updated.updated_at = now;
        self.meta.update_file(&updated).await?;

        let _ = self
            .meta
            .upsert_fts(&updated.id, &updated.display_name, "", fts_content)
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
        message: Option<String>,
        fts_content: &str,
        now: time::OffsetDateTime,
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
        };
        self.meta.insert_commit(&commit).await?;

        file.current_commit = Some(commit.id);
        file.size = size;
        file.updated_at = now;
        self.meta.update_file(file).await?;

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
        message: Option<String>,
    ) -> Result<Commit, VersioningError> {
        let bytes = self.read_at_commit(file_id, commit_id).await?;
        let msg = message.unwrap_or_else(|| format!("rollback to {commit_id}"));
        self.commit(file_id, &bytes, actor, Some(msg)).await
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
            .create_file("note.md", b"hello", ActorId::new(), None)
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
            .create_file("doc.txt", b"v1", actor, None)
            .await
            .unwrap();
        let c2 = eng
            .commit(file.id, b"v2", actor, Some("update".into()))
            .await
            .unwrap();
        let c3 = eng
            .commit(file.id, b"v3", actor, None)
            .await
            .unwrap();
        assert_eq!(c2.parent, Some(c1.id));
        assert_eq!(c3.parent, Some(c2.id));
        assert_eq!(eng.read_current(file.id).await.unwrap(), b"v3");

        let log = eng.meta.list_commits(&file.id).await.unwrap();
        assert_eq!(log.len(), 3);
    }

    #[tokio::test]
    async fn lww_fallback_for_binary() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        let bytes = vec![0xFFu8, 0xD8, 0xFF]; // JPEG マジック
        let (file, commit) = eng
            .create_file("photo.jpg", &bytes, actor, None)
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
            .create_file_streaming("movie.bin", stream, actor, None)
            .await
            .unwrap();
        assert_eq!(commit.format_id, "_/lww");
        assert_eq!(file.size, 5);
        assert_eq!(eng.read_current(file.id).await.unwrap(), vec![0, 1, 2, 3, 4]);

        // 続けて commit_streaming で全置換できる。
        let c2 = eng
            .commit_streaming(file.id, byte_stream(vec![b"\xAA\xBB"]), actor, None)
            .await
            .unwrap();
        assert_eq!(c2.parent, Some(commit.id));
        assert_eq!(eng.read_current(file.id).await.unwrap(), vec![0xAA, 0xBB]);
    }

    #[tokio::test]
    async fn streaming_text_falls_back_to_buffered_crdt() {
        let (eng, _td) = engine().await;
        let actor = ActorId::new();
        // テキスト（.md）は CRDT 経路へフォールバックし text/plain になる。
        let stream = byte_stream(vec![b"hello ", b"world"]);
        let (file, commit) = eng
            .create_file_streaming("note.md", stream, actor, None)
            .await
            .unwrap();
        assert_eq!(commit.format_id, "text/plain");
        assert_eq!(eng.read_current(file.id).await.unwrap(), b"hello world");
    }
}
