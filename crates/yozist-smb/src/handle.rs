//! 書き込みバッファリングを行う共通 Handle 実装。
//!
//! - 既存ファイル: open 時に現在内容を読み込み、書き込みはメモリに buffer
//! - 新規ファイル: open 時に空 buffer を作る
//! - close 時 or flush 時に `VersioningEngine::commit` / `create_file` を発火
//!
//! # 並行性
//! 同じ FileId に対して複数 Handle が並行に開かれる可能性がある。
//! 各 Handle は独立した buffer を持ち、close 時のコミットで履歴に追加される
//! （競合は yozist-versioning 側の CRDT/LWW で吸収される）。

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use smb_server::{DirEntry, FileInfo, FileTimes, Handle};
use std::sync::Arc;
use std::time::SystemTime;
use time::OffsetDateTime;
use yozist_core::{ActorId, FileId};
use yozist_versioning::VersioningEngine;

use crate::ShareDeps;
use tracing::debug;
use yozist_auth::{DbAuthorizer, Permission, PermissionMask, Subject, Target};
use yozist_db::{AuditRecord, SharedAuditLog, SharedMetaStore};

/// 既存ファイル or 新規ファイル用の汎用 Handle。
pub struct YozistFileHandle {
    inner: Mutex<HandleState>,
    engine: Arc<VersioningEngine>,
    meta: Option<SharedMetaStore>,
    /// 新規ファイル作成時、close 時にオーナー ACL を発行するための authorizer。
    acl_admin: Option<Arc<DbAuthorizer>>,
    /// 新規作成時のファイルオーナー（ADMIN権限の自動付与先）。
    owner: Option<yozist_core::UserId>,
    /// SMB セッションのユーザー名（audit_label 用、`smb:<user>` 形式）。
    smb_actor_label: Option<String>,
    /// 監査ログ書き込み先（close 時に成功・失敗を記録）。
    audit: Option<SharedAuditLog>,
    actor: ActorId,
    /// SMB へ報告する作成時刻（FILETIME）。既存ファイルは `created_at`。
    creation_ft: u64,
    /// SMB へ報告する更新時刻（FILETIME）。既存ファイルは `updated_at`。
    ///
    /// stat の度に `now()` を返すと、NSDocument（macOS のプレビュー等）が
    /// 「ファイルが別アプリに変更された」と誤検知して保存できなくなるため、
    /// open 時点の値でセッション中は固定する。
    modified_ft: u64,
}

struct HandleState {
    /// `Some(id)` 既存、`None` 新規（close 時に create_file）。
    file_id: Option<FileId>,
    /// 新規時の表示名。
    display_name: String,
    /// 現在のバッファ（読み書き共通）。
    buffer: Vec<u8>,
    /// 書き込みがあったか。
    dirty: bool,
    /// 読み取り可能か。
    readable: bool,
    /// 書き込み可能か。
    writable: bool,
    /// 新規作成時に自動付与するタグ。
    pending_tags: Vec<yozist_core::TagId>,
    /// 新規作成時に追加するシリーズと order_index。
    pending_series: Option<(yozist_core::SeriesId, f64)>,
}

impl YozistFileHandle {
    /// 既存ファイルを開く。
    pub async fn open_existing(
        deps: &ShareDeps,
        engine: Arc<VersioningEngine>,
        file_id: FileId,
        display_name: String,
        readable: bool,
        writable: bool,
    ) -> Result<Self, smb_server::SmbError> {
        let _ = deps;
        let raw = engine
            .read_current(file_id)
            .await
            .map_err(|_| smb_server::SmbError::NotFound)?;
        let meta = engine.meta.get_file(&file_id).await.ok().flatten();
        // blob は UTF-8。テキストは元エンコーディング（charset）へ再エンコードして
        // SMB クライアントへ「元の形式」で見せる。サイズ報告(snapshot_info)も
        // この buffer 長を使うため read/サイズとも整合する。
        let buffer = match meta.as_ref().and_then(|m| m.charset.as_deref()) {
            Some(cs) => {
                let text = String::from_utf8_lossy(&raw);
                yozist_versioning::encode_text(&text, cs)
            }
            None => raw,
        };
        // 時刻はストア上の値で固定する。stat の度に now() を返すと NSDocument が
        // 競合と誤検知し保存できなくなる（[[project_smb_safe_save]] 参照）。
        let (creation_ft, modified_ft) = match &meta {
            Some(m) => (
                offset_dt_to_filetime(m.created_at),
                offset_dt_to_filetime(m.updated_at),
            ),
            None => {
                let now = system_time_to_filetime(SystemTime::now());
                (now, now)
            }
        };
        // 既存ファイルの size が提示サイズ（buffer 長）と食い違う場合は自己修復する。
        // charset 対応前に作られた等で UTF-8 blob 長が記録されていると、一覧
        // (file_meta_to_info=meta.size) と open(snapshot_info=buffer.len) が食い違い、
        // macOS が folder 上のサイズと実体を reconcile できずループする。buffer は
        // 既に読み込み済みなので追加 I/O は無い。updated_at は変えない（mtime 安定）。
        if let Some(m) = meta.as_ref().filter(|m| m.size != buffer.len() as u64) {
            let mut fixed = m.clone();
            fixed.size = buffer.len() as u64;
            let _ = engine.meta.update_file(&fixed).await;
        }
        Ok(Self {
            inner: Mutex::new(HandleState {
                file_id: Some(file_id),
                display_name,
                buffer,
                dirty: false,
                readable,
                writable,
                pending_tags: Vec::new(),
                pending_series: None,
            }),
            engine,
            meta: None,
            acl_admin: None,
            owner: None,
            smb_actor_label: None,
            audit: None,
            actor: ActorId::new(),
            creation_ft,
            modified_ft,
        })
    }

    /// 新規ファイル（空 buffer）を開く。close 時に create_file。
    pub fn open_new(
        engine: Arc<VersioningEngine>,
        display_name: String,
        writable: bool,
    ) -> Self {
        Self {
            inner: Mutex::new(HandleState {
                file_id: None,
                display_name,
                buffer: Vec::new(),
                dirty: false,
                readable: true,
                writable,
                pending_tags: Vec::new(),
                pending_series: None,
            }),
            engine,
            meta: None,
            acl_admin: None,
            owner: None,
            smb_actor_label: None,
            audit: None,
            actor: ActorId::new(),
            // 新規ファイルは作成時刻のみ持つ。close 時の create_file までは
            // ストア上に時刻が無いため open 時の現在時刻で固定する。
            creation_ft: system_time_to_filetime(SystemTime::now()),
            modified_ft: system_time_to_filetime(SystemTime::now()),
        }
    }

    /// 新規ファイルを開き、close 時にタグも自動付与する。
    pub fn open_new_with_tags(
        engine: Arc<VersioningEngine>,
        meta: SharedMetaStore,
        display_name: String,
        writable: bool,
        tags: Vec<yozist_core::TagId>,
    ) -> Self {
        Self {
            inner: Mutex::new(HandleState {
                file_id: None,
                display_name,
                buffer: Vec::new(),
                dirty: false,
                readable: true,
                writable,
                pending_tags: tags,
                pending_series: None,
            }),
            engine,
            meta: Some(meta),
            acl_admin: None,
            owner: None,
            smb_actor_label: None,
            audit: None,
            actor: ActorId::new(),
            // 新規ファイルは作成時刻のみ持つ。close 時の create_file までは
            // ストア上に時刻が無いため open 時の現在時刻で固定する。
            creation_ft: system_time_to_filetime(SystemTime::now()),
            modified_ft: system_time_to_filetime(SystemTime::now()),
        }
    }

    /// 新規ファイルを開き、close 時にシリーズへ追加する。
    pub fn open_new_with_series(
        engine: Arc<VersioningEngine>,
        meta: SharedMetaStore,
        display_name: String,
        writable: bool,
        series_id: yozist_core::SeriesId,
        order_index: f64,
    ) -> Self {
        Self {
            inner: Mutex::new(HandleState {
                file_id: None,
                display_name,
                buffer: Vec::new(),
                dirty: false,
                readable: true,
                writable,
                pending_tags: Vec::new(),
                pending_series: Some((series_id, order_index)),
            }),
            engine,
            meta: Some(meta),
            acl_admin: None,
            owner: None,
            smb_actor_label: None,
            audit: None,
            actor: ActorId::new(),
            // 新規ファイルは作成時刻のみ持つ。close 時の create_file までは
            // ストア上に時刻が無いため open 時の現在時刻で固定する。
            creation_ft: system_time_to_filetime(SystemTime::now()),
            modified_ft: system_time_to_filetime(SystemTime::now()),
        }
    }

    /// 新規ファイルを開き、close 時にオーナー ACL を発行する。
    pub fn open_new_with_owner(
        engine: Arc<VersioningEngine>,
        meta: SharedMetaStore,
        acl_admin: Arc<DbAuthorizer>,
        display_name: String,
        writable: bool,
        owner: yozist_core::UserId,
        pending_tags: Vec<yozist_core::TagId>,
        pending_series: Option<(yozist_core::SeriesId, f64)>,
    ) -> Self {
        Self {
            inner: Mutex::new(HandleState {
                file_id: None,
                display_name,
                buffer: Vec::new(),
                dirty: false,
                readable: true,
                writable,
                pending_tags,
                pending_series,
            }),
            engine,
            meta: Some(meta),
            acl_admin: Some(acl_admin),
            owner: Some(owner),
            smb_actor_label: None,
            audit: None,
            actor: ActorId::new(),
            // 新規ファイルは作成時刻のみ持つ。close 時の create_file までは
            // ストア上に時刻が無いため open 時の現在時刻で固定する。
            creation_ft: system_time_to_filetime(SystemTime::now()),
            modified_ft: system_time_to_filetime(SystemTime::now()),
        }
    }

    /// `smb_actor_label`/`audit` を後付け設定する。バックエンドが open 時に呼ぶ。
    pub fn with_smb_audit(
        mut self,
        identity: &smb_server::Identity,
        audit: SharedAuditLog,
    ) -> Self {
        let label = match identity {
            smb_server::Identity::Anonymous => "smb:anonymous".to_string(),
            smb_server::Identity::User { user, .. } => format!("smb:{}", user),
        };
        self.smb_actor_label = Some(label);
        self.audit = Some(audit);
        self
    }

    pub fn set_truncated(&mut self) {
        let mut st = self.inner.lock();
        st.buffer.clear();
        st.dirty = true;
    }

    fn snapshot_info(&self) -> FileInfo {
        let st = self.inner.lock();
        FileInfo {
            name: st.display_name.clone(),
            end_of_file: st.buffer.len() as u64,
            allocation_size: st.buffer.len() as u64,
            // open 時に固定した時刻を返す（stat ごとに変えない）。
            creation_time: self.creation_ft,
            last_access_time: self.modified_ft,
            last_write_time: self.modified_ft,
            change_time: self.modified_ft,
            is_directory: false,
            file_index: 0,
        }
    }
}

#[async_trait]
impl Handle for YozistFileHandle {
    async fn read(&self, offset: u64, len: u32) -> smb_server::SmbResult<Bytes> {
        let st = self.inner.lock();
        if !st.readable {
            return Err(smb_server::SmbError::AccessDenied);
        }
        if offset as usize >= st.buffer.len() {
            return Ok(Bytes::new());
        }
        let end = ((offset as usize) + len as usize).min(st.buffer.len());
        Ok(Bytes::copy_from_slice(&st.buffer[offset as usize..end]))
    }

    async fn write(&self, offset: u64, data: &[u8]) -> smb_server::SmbResult<u32> {
        debug!(offset, len = data.len(), "YozistFileHandle::write");
        let mut st = self.inner.lock();
        if !st.writable {
            return Err(smb_server::SmbError::AccessDenied);
        }
        let end = offset as usize + data.len();
        if end > st.buffer.len() {
            st.buffer.resize(end, 0);
        }
        st.buffer[offset as usize..end].copy_from_slice(data);
        st.dirty = true;
        Ok(data.len() as u32)
    }

    async fn flush(&self) -> smb_server::SmbResult<()> {
        // macOS（NSDocument: TextEdit/プレビュー等）は保存時に
        // 「write → flush → 別ハンドルで再オープンして内容を検証」する。
        // commit を close 時だけに行うと、その検証 read が `read_current`
        // （＝コミット済み＝旧内容）を返し、アプリは「保存できていない」と
        // 判断して延々リトライ（クルクル）する。よって既存ファイルは flush
        // 時点で永続化し、再オープンで新内容が見えるようにする。
        // （[[project_smb_safe_save]]）
        let (file_id, buffer, dirty) = {
            let st = self.inner.lock();
            (st.file_id, st.buffer.clone(), st.dirty)
        };
        if !dirty {
            return Ok(());
        }
        // 新規ファイル（file_id=None）は close 時の create_file に委ねる。
        if let Some(id) = file_id {
            self.engine
                .commit(id, &buffer, self.actor, None, None, Some("smb".into()))
                .await
                .map_err(|e| {
                    smb_server::SmbError::Io(std::io::Error::other(e.to_string()))
                })?;
            // 直近 write 分はコミット済み。以降 write が無ければ close で再コミットしない。
            self.inner.lock().dirty = false;
            debug!(file_id = %id, bytes = buffer.len(), "YozistFileHandle::flush committed");
        }
        Ok(())
    }

    async fn stat(&self) -> smb_server::SmbResult<FileInfo> {
        Ok(self.snapshot_info())
    }

    async fn set_times(&self, times: FileTimes) -> smb_server::SmbResult<()> {
        // TODO: FileMeta に時刻列を追加するか、commit metadata に保持
        debug!(
            creation = times.creation_time.is_some(),
            last_write = times.last_write_time.is_some(),
            change = times.change_time.is_some(),
            "YozistFileHandle::set_times (no-op)"
        );
        Ok(())
    }

    async fn truncate(&self, len: u64) -> smb_server::SmbResult<()> {
        debug!(len, "YozistFileHandle::truncate");
        let mut st = self.inner.lock();
        if !st.writable {
            return Err(smb_server::SmbError::AccessDenied);
        }
        st.buffer.resize(len as usize, 0);
        st.dirty = true;
        Ok(())
    }

    async fn list_dir(
        &self,
        _pattern: Option<&str>,
    ) -> smb_server::SmbResult<Vec<DirEntry>> {
        Err(smb_server::SmbError::NotADirectory)
    }

    async fn close(self: Box<Self>) -> smb_server::SmbResult<()> {
        let (file_id, name, buffer, dirty, pending_tags, pending_series) = {
            let st = self.inner.lock();
            (
                st.file_id,
                st.display_name.clone(),
                st.buffer.clone(),
                st.dirty,
                st.pending_tags.clone(),
                st.pending_series,
            )
        };
        let label = self.smb_actor_label.clone();
        let audit = self.audit.clone();
        let action_label: &str = if file_id.is_some() { "commit" } else { "create_file" };
        debug!(
            file_id = ?file_id,
            dirty,
            bytes = buffer.len(),
            action = action_label,
            "YozistFileHandle::close 開始"
        );
        if !dirty {
            return Ok(());
        }
        let inner_result: smb_server::SmbResult<Option<String>> = async {
            match file_id {
                Some(id) => {
                    self.engine
                        .commit(id, &buffer, self.actor, None, None, Some("smb".into()))
                        .await
                        .map_err(|e| {
                            smb_server::SmbError::Io(std::io::Error::other(e.to_string()))
                        })?;
                    Ok(Some(id.to_string()))
                }
                None => {
                    let (file, _commit) = self
                        .engine
                        .create_file(name, &buffer, self.actor, None, None, None)
                        .await
                        .map_err(|e| {
                            smb_server::SmbError::Io(std::io::Error::other(e.to_string()))
                        })?;
                    // アップロード元を示すシステムタグ `src:smb` を付与。
                    self.engine.attach_source_tag(file.id, "smb").await;
                    if let Some(meta) = &self.meta {
                        for t in &pending_tags {
                            meta.attach_tag(&file.id, t).await.map_err(|e| {
                                smb_server::SmbError::Io(std::io::Error::other(
                                    e.to_string(),
                                ))
                            })?;
                        }
                        if let Some((series_id, order_index)) = pending_series {
                            meta.add_to_series(&yozist_core::SeriesMember {
                                series_id,
                                file_id: file.id,
                                order_index,
                            })
                            .await
                            .map_err(|e| {
                                smb_server::SmbError::Io(std::io::Error::other(e.to_string()))
                            })?;
                        }
                    }
                    if let (Some(acl_admin), Some(owner_id)) =
                        (&self.acl_admin, self.owner)
                    {
                        let owner_rule = Permission {
                            subject: Subject::User(owner_id),
                            target: Target::file(file.id),
                            mask: PermissionMask::all(),
                            allow: true,
                            priority: i32::MAX,
                        };
                        acl_admin.add_rule(&owner_rule).await.map_err(|e| {
                            smb_server::SmbError::Io(std::io::Error::other(e.to_string()))
                        })?;
                    }
                    Ok(Some(file.id.to_string()))
                }
            }
        }
        .await;
        debug!(
            ok = inner_result.is_ok(),
            action = action_label,
            "YozistFileHandle::close commit/create 完了"
        );

        // 監査記録（SMB 経由のみ）
        if let (Some(label), Some(audit)) = (label, audit) {
            let result_str = match &inner_result {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("error: {e}"),
            };
            let target_ref = inner_result.as_ref().ok().and_then(|x| x.clone());
            let _ = audit
                .record(&AuditRecord {
                    actor_id: None,
                    actor_label: Some(&label),
                    action: action_label,
                    target_type: Some("file"),
                    target_ref: target_ref.as_deref(),
                    metadata_json: None,
                    result: &result_str,
                })
                .await;
        }
        inner_result.map(|_| ())
    }
}

/// 仮想ディレクトリの時刻フォールバック（配下が空などで最大値が取れない時）。
/// 固定値（約 2022-06）。stat の度に変わらなければよく、値自体に意味は無い。
const STABLE_DIR_FILETIME: u64 = 133_000_000_000_000_000;

/// 仮想ディレクトリ Handle。`list_dir` のみ実装。
pub struct YozistDirHandle {
    name: String,
    entries: Vec<DirEntry>,
    creation_ft: u64,
    modified_ft: u64,
}

impl YozistDirHandle {
    pub fn new(name: impl Into<String>, entries: Vec<DirEntry>) -> Self {
        // ディレクトリの時刻は配下エントリの時刻から導く。stat の度に now() を
        // 返すと、macOS が「ディレクトリが変化し続けている」と誤認して列挙を
        // 無限ループする（[[project_smb_safe_save]]）。配下が変わらなければ
        // 安定し、ファイル追加/更新時のみ変化する。
        let modified_ft = entries
            .iter()
            .map(|e| e.info.last_write_time)
            .filter(|&t| t != 0)
            .max()
            .unwrap_or(STABLE_DIR_FILETIME);
        let creation_ft = entries
            .iter()
            .map(|e| e.info.creation_time)
            .filter(|&t| t != 0)
            .min()
            .unwrap_or(STABLE_DIR_FILETIME);
        let name = name.into();
        // この値が stat の度（＝ディレクトリ open の度）に変わると macOS が
        // 再列挙ループする。診断用に出力（安定していれば毎回同じ値のはず）。
        debug!(
            dir = %name,
            entries = entries.len(),
            modified_ft,
            creation_ft,
            "YozistDirHandle::new (dir mtime 安定化済み build)"
        );
        Self {
            name,
            entries,
            creation_ft,
            modified_ft,
        }
    }
}

#[async_trait]
impl Handle for YozistDirHandle {
    async fn read(&self, _offset: u64, _len: u32) -> smb_server::SmbResult<Bytes> {
        Err(smb_server::SmbError::IsDirectory)
    }
    async fn write(&self, _offset: u64, _data: &[u8]) -> smb_server::SmbResult<u32> {
        Err(smb_server::SmbError::IsDirectory)
    }
    async fn flush(&self) -> smb_server::SmbResult<()> {
        Ok(())
    }
    async fn stat(&self) -> smb_server::SmbResult<FileInfo> {
        Ok(FileInfo {
            name: self.name.clone(),
            end_of_file: 0,
            allocation_size: 0,
            // 配下から導いた安定時刻を返す（stat ごとに変えない）。
            creation_time: self.creation_ft,
            last_access_time: self.modified_ft,
            last_write_time: self.modified_ft,
            change_time: self.modified_ft,
            is_directory: true,
            file_index: 0,
        })
    }
    async fn set_times(&self, _times: FileTimes) -> smb_server::SmbResult<()> {
        Ok(())
    }
    async fn truncate(&self, _len: u64) -> smb_server::SmbResult<()> {
        Err(smb_server::SmbError::IsDirectory)
    }
    async fn list_dir(
        &self,
        pattern: Option<&str>,
    ) -> smb_server::SmbResult<Vec<DirEntry>> {
        // SMB の QUERY_DIRECTORY は検索パターンで絞り込んで返す必要がある。
        // 絞らずに全件返すと、クライアント（macOS）が「特定名の存在確認」を
        // パターン検索で行ったとき、存在しない一時ファイル名(`*.sb-…`)に対しても
        // 全件が返って「存在する」と誤認し、保存用 temp 名を連番で無限に探し続け
        // （save が永久にスピン）てしまう。([[project_smb_safe_save]])
        match pattern {
            None => Ok(self.entries.clone()),
            Some(pat) if pat == "*" || pat == "*.*" => Ok(self.entries.clone()),
            Some(pat) => Ok(self
                .entries
                .iter()
                .filter(|e| dos_glob_match(pat, &e.info.name))
                .cloned()
                .collect()),
        }
    }
    async fn close(self: Box<Self>) -> smb_server::SmbResult<()> {
        Ok(())
    }
}

/// DOS 風ワイルドカード照合（`*`=任意の並び, `?`=任意の1文字, 大文字小文字無視）。
/// SMB の QUERY_DIRECTORY のパターン絞り込みに使う。
fn dos_glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    // 反復的ワイルドカード照合（`*` をバックトラッキング）。
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star_pi, mut star_ni): (Option<usize>, usize) = (None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            star_ni = ni;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------------------
// スクラッチ FS（macOS のアトミック保存用・一時サブディレクトリの仮想サポート）
// ---------------------------------------------------------------------------

/// `/all` はフラットだが、macOS のドキュメント保存（`replaceItemAtURL:`）は
/// 保存先と同じ場所に一時**サブディレクトリ**（`<本体>.sb-…` 等、任意名）を
/// 作り、その中に新内容を書いて最後に本体へ差し替える。これを受け止めるための
/// メモリ上スクラッチ FS。実体はメモリにのみ存在し、rename（スクラッチ内
/// ファイル → 本体）で本体ファイルへ fold される。`[[project_smb_safe_save]]`
#[derive(Default)]
pub struct ScratchFs {
    /// 仮想ディレクトリ名（パスの第1成分）。
    pub dirs: std::collections::HashSet<String>,
    /// `"dir/file"` → 内容。
    pub files: std::collections::HashMap<String, Vec<u8>>,
}

impl ScratchFs {
    /// `dir` 配下のファイル名一覧。
    pub fn entries_in(&self, dir: &str) -> Vec<String> {
        let prefix = format!("{dir}/");
        self.files
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(|s| s.to_string()))
            .collect()
    }
}

pub type SharedScratch = Arc<Mutex<ScratchFs>>;

/// スクラッチ FS 上のファイルハンドル。read/write/truncate はメモリ上の
/// バッファを操作し、close してもスクラッチには残る（rename/unlink/rmdir で
/// 破棄または本体へ fold される）。
pub struct ScratchFileHandle {
    scratch: SharedScratch,
    key: String,
    name: String,
    readable: bool,
    writable: bool,
    ft: u64,
}

impl ScratchFileHandle {
    pub fn new(
        scratch: SharedScratch,
        key: String,
        name: String,
        readable: bool,
        writable: bool,
    ) -> Self {
        Self {
            scratch,
            key,
            name,
            readable,
            writable,
            ft: system_time_to_filetime(SystemTime::now()),
        }
    }
}

#[async_trait]
impl Handle for ScratchFileHandle {
    async fn read(&self, offset: u64, len: u32) -> smb_server::SmbResult<Bytes> {
        if !self.readable {
            return Err(smb_server::SmbError::AccessDenied);
        }
        let sc = self.scratch.lock();
        match sc.files.get(&self.key) {
            Some(b) if (offset as usize) < b.len() => {
                let end = ((offset as usize) + len as usize).min(b.len());
                Ok(Bytes::copy_from_slice(&b[offset as usize..end]))
            }
            _ => Ok(Bytes::new()),
        }
    }
    async fn write(&self, offset: u64, data: &[u8]) -> smb_server::SmbResult<u32> {
        if !self.writable {
            return Err(smb_server::SmbError::AccessDenied);
        }
        let mut sc = self.scratch.lock();
        let buf = sc.files.entry(self.key.clone()).or_default();
        let end = offset as usize + data.len();
        if end > buf.len() {
            buf.resize(end, 0);
        }
        buf[offset as usize..end].copy_from_slice(data);
        Ok(data.len() as u32)
    }
    async fn flush(&self) -> smb_server::SmbResult<()> {
        Ok(())
    }
    async fn stat(&self) -> smb_server::SmbResult<FileInfo> {
        let size = self
            .scratch
            .lock()
            .files
            .get(&self.key)
            .map(|b| b.len())
            .unwrap_or(0) as u64;
        Ok(FileInfo {
            name: self.name.clone(),
            end_of_file: size,
            allocation_size: size,
            creation_time: self.ft,
            last_access_time: self.ft,
            last_write_time: self.ft,
            change_time: self.ft,
            is_directory: false,
            file_index: 0,
        })
    }
    async fn set_times(&self, _times: FileTimes) -> smb_server::SmbResult<()> {
        Ok(())
    }
    async fn truncate(&self, len: u64) -> smb_server::SmbResult<()> {
        if !self.writable {
            return Err(smb_server::SmbError::AccessDenied);
        }
        let mut sc = self.scratch.lock();
        sc.files.entry(self.key.clone()).or_default().resize(len as usize, 0);
        Ok(())
    }
    async fn list_dir(
        &self,
        _pattern: Option<&str>,
    ) -> smb_server::SmbResult<Vec<DirEntry>> {
        Err(smb_server::SmbError::NotADirectory)
    }
    async fn close(self: Box<Self>) -> smb_server::SmbResult<()> {
        // スクラッチはここでは破棄しない（rename で本体へ fold される）。
        Ok(())
    }
}

pub fn system_time_to_filetime(t: SystemTime) -> u64 {
    // FILETIME: 100ns ticks since 1601-01-01 UTC
    let dur = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let unix_100ns = dur.as_nanos() / 100;
    // UNIX EPOCH から 1601 までの差: 11644473600 秒 × 1e7
    const EPOCH_OFFSET_100NS: u128 = 116_444_736_000_000_000;
    (unix_100ns + EPOCH_OFFSET_100NS) as u64
}

pub fn offset_dt_to_filetime(dt: OffsetDateTime) -> u64 {
    let unix_ns = dt.unix_timestamp_nanos();
    let unix_100ns = (unix_ns / 100) as u128;
    const EPOCH_OFFSET_100NS: u128 = 116_444_736_000_000_000;
    (unix_100ns + EPOCH_OFFSET_100NS) as u64
}

pub fn file_meta_to_info(meta: &yozist_core::FileMeta, name: String) -> FileInfo {
    let ct = offset_dt_to_filetime(meta.created_at);
    let mt = offset_dt_to_filetime(meta.updated_at);
    FileInfo {
        name,
        end_of_file: meta.size,
        allocation_size: meta.size,
        creation_time: ct,
        last_access_time: mt,
        last_write_time: mt,
        change_time: mt,
        is_directory: false,
        file_index: 0,
    }
}
