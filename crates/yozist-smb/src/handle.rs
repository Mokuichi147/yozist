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

/// 既存ファイル or 新規ファイル用の汎用 Handle。
pub struct YozistFileHandle {
    inner: Mutex<HandleState>,
    engine: Arc<VersioningEngine>,
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
        Ok(Self {
            inner: Mutex::new(HandleState {
                file_id: Some(file_id),
                display_name,
                buffer,
                dirty: false,
                readable,
                writable,
            }),
            engine,
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
            }),
            engine,
            actor: ActorId::new(),
        }
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
        // dirty なら commit を発火。
        let (file_id, name, buffer, dirty) = {
            let st = self.inner.lock();
            (st.file_id, st.display_name.clone(), st.buffer.clone(), st.dirty)
        };
        if !dirty {
            return Ok(());
        }
        match file_id {
            Some(id) => {
                self.engine
                    .commit(id, &buffer, self.actor, Some("smb".into()))
                    .await
                    .map_err(|e| {
                        smb_server::SmbError::Io(std::io::Error::other(e.to_string()))
                    })?;
            }
            None => {
                self.engine
                    .create_file(name, &buffer, self.actor, None)
                    .await
                    .map_err(|e| {
                        smb_server::SmbError::Io(std::io::Error::other(e.to_string()))
                    })?;
            }
        }
        Ok(())
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
