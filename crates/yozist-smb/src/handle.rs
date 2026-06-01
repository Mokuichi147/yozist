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
        let buffer = engine
            .read_current(file_id)
            .await
            .map_err(|_| smb_server::SmbError::NotFound)?;
        // blob は UTF-8。テキストは元エンコーディング（charset）へ再エンコードして
        // SMB クライアントへ「元の形式」で見せる。サイズ報告(snapshot_info)も
        // この buffer 長を使うため read/サイズとも整合する。
        let buffer = match engine.meta.get_file(&file_id).await {
            Ok(Some(meta)) => match meta.charset {
                Some(cs) => {
                    let text = String::from_utf8_lossy(&buffer);
                    yozist_versioning::encode_text(&text, &cs)
                }
                None => buffer,
            },
            _ => buffer,
        };
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
        let now = system_time_to_filetime(SystemTime::now());
        FileInfo {
            name: st.display_name.clone(),
            end_of_file: st.buffer.len() as u64,
            allocation_size: st.buffer.len() as u64,
            creation_time: now,
            last_access_time: now,
            last_write_time: now,
            change_time: now,
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
        // commit はクローズ時に行う（部分書き込み中のコミットを避ける）。
        Ok(())
    }

    async fn stat(&self) -> smb_server::SmbResult<FileInfo> {
        Ok(self.snapshot_info())
    }

    async fn set_times(&self, _times: FileTimes) -> smb_server::SmbResult<()> {
        // TODO: FileMeta に時刻列を追加するか、commit metadata に保持
        Ok(())
    }

    async fn truncate(&self, len: u64) -> smb_server::SmbResult<()> {
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
        if !dirty {
            return Ok(());
        }
        let inner_result: smb_server::SmbResult<Option<String>> = async {
            match file_id {
                Some(id) => {
                    self.engine
                        .commit(id, &buffer, self.actor, Some("smb".into()))
                        .await
                        .map_err(|e| {
                            smb_server::SmbError::Io(std::io::Error::other(e.to_string()))
                        })?;
                    Ok(Some(id.to_string()))
                }
                None => {
                    let (file, _commit) = self
                        .engine
                        .create_file(name, &buffer, self.actor, None)
                        .await
                        .map_err(|e| {
                            smb_server::SmbError::Io(std::io::Error::other(e.to_string()))
                        })?;
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

/// 仮想ディレクトリ Handle。`list_dir` のみ実装。
pub struct YozistDirHandle {
    name: String,
    entries: Vec<DirEntry>,
}

impl YozistDirHandle {
    pub fn new(name: impl Into<String>, entries: Vec<DirEntry>) -> Self {
        Self {
            name: name.into(),
            entries,
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
        let now = system_time_to_filetime(SystemTime::now());
        Ok(FileInfo {
            name: self.name.clone(),
            end_of_file: 0,
            allocation_size: 0,
            creation_time: now,
            last_access_time: now,
            last_write_time: now,
            change_time: now,
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
        _pattern: Option<&str>,
    ) -> smb_server::SmbResult<Vec<DirEntry>> {
        Ok(self.entries.clone())
    }
    async fn close(self: Box<Self>) -> smb_server::SmbResult<()> {
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
