//! Share 別バックエンド実装。
//!
//! - **AllBackend**: 全ファイルをフラットに `<file-id>__<display_name>` で公開
//! - **TagsBackend**: 階層パス = タグ AND 条件（v2 で実装）
//! - **SeriesBackend**: 順序プレフィクス付きメンバー（v2 で実装）
//! - **RecentBackend**: 直近更新の読取専用ビュー（v2 で実装）

use async_trait::async_trait;
use smb_server::{
    BackendCapabilities, DirEntry, Handle, OpenIntent, OpenOptions, ShareBackend, SmbError,
    SmbPath, SmbResult,
};

use crate::handle::{file_meta_to_info, YozistDirHandle, YozistFileHandle};
use crate::ShareDeps;

const ID_SEP: &str = "__";

/// 全ファイルをフラットに公開する管理用 share。
///
/// パス規則: ファイルは `<file-uuid>__<display_name>` として現れる。
/// `mkdir` は不可。`rmdir` も不可（ルートのみ）。
pub struct AllBackend {
    deps: ShareDeps,
}

impl AllBackend {
    pub fn new(deps: ShareDeps) -> Self {
        Self { deps }
    }

    fn parse_filename(name: &str) -> Option<(yozist_core::FileId, String)> {
        let (id_part, _rest) = name.split_once(ID_SEP)?;
        let uuid = uuid::Uuid::parse_str(id_part).ok()?;
        Some((yozist_core::FileId::from_uuid(uuid), name.to_string()))
    }

    fn display_filename(meta: &yozist_core::FileMeta) -> String {
        format!("{}{}{}", meta.id, ID_SEP, meta.display_name)
    }

    async fn list_root(&self) -> SmbResult<Vec<DirEntry>> {
        let files = self
            .deps
            .meta
            .list_files(1000, 0)
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
        Ok(files
            .into_iter()
            .map(|meta| {
                let name = Self::display_filename(&meta);
                DirEntry {
                    info: file_meta_to_info(&meta, name),
                }
            })
            .collect())
    }
}

#[async_trait]
impl ShareBackend for AllBackend {
    async fn open(
        &self,
        path: &SmbPath,
        opts: OpenOptions,
    ) -> SmbResult<Box<dyn Handle>> {
        if path.is_root() {
            // ルートディレクトリを開く
            if opts.non_directory {
                return Err(SmbError::IsDirectory);
            }
            let entries = self.list_root().await?;
            return Ok(Box::new(YozistDirHandle::new("all", entries)));
        }

        let components = path.components();
        if components.len() != 1 {
            // フラット構造のためサブディレクトリは存在しない
            return Err(SmbError::PathNotFound);
        }
        let name = &components[0];

        // 既存ファイル検索
        let existing = Self::parse_filename(name).and_then(|(id, _)| Some(id));
        let existing_meta = if let Some(id) = existing {
            self.deps
                .meta
                .get_file(&id)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
        } else {
            None
        };

        match (opts.intent, existing_meta) {
            (OpenIntent::Open | OpenIntent::Truncate, None) => Err(SmbError::NotFound),
            (OpenIntent::Create, Some(_)) => Err(SmbError::Exists),
            (OpenIntent::Open, Some(meta)) => {
                if opts.directory {
                    return Err(SmbError::NotADirectory);
                }
                let h = YozistFileHandle::open_existing(
                    &self.deps,
                    self.deps.engine.clone(),
                    meta.id,
                    meta.display_name.clone(),
                    opts.read,
                    opts.write,
                )
                .await?;
                Ok(Box::new(h))
            }
            (OpenIntent::OpenOrCreate, Some(meta)) => {
                let h = YozistFileHandle::open_existing(
                    &self.deps,
                    self.deps.engine.clone(),
                    meta.id,
                    meta.display_name.clone(),
                    opts.read,
                    opts.write,
                )
                .await?;
                Ok(Box::new(h))
            }
            (OpenIntent::Truncate, Some(meta)) => {
                let mut h = YozistFileHandle::open_existing(
                    &self.deps,
                    self.deps.engine.clone(),
                    meta.id,
                    meta.display_name.clone(),
                    opts.read,
                    true,
                )
                .await?;
                h.set_truncated();
                Ok(Box::new(h))
            }
            (OpenIntent::OverwriteOrCreate, Some(meta)) => {
                let mut h = YozistFileHandle::open_existing(
                    &self.deps,
                    self.deps.engine.clone(),
                    meta.id,
                    meta.display_name.clone(),
                    opts.read,
                    true,
                )
                .await?;
                h.set_truncated();
                Ok(Box::new(h))
            }
            (OpenIntent::Create | OpenIntent::OpenOrCreate | OpenIntent::OverwriteOrCreate, None) => {
                if opts.directory {
                    return Err(SmbError::NotSupported); // AllBackend にディレクトリ作成は不要
                }
                // 新規ファイル: ID プレフィクスは閉じる時に確定するので、
                // 入力名から `__` 部分を剥がしてユーザー指定の display_name を使う。
                let display_name = match name.split_once(ID_SEP) {
                    Some((_, rest)) => rest.to_string(),
                    None => name.clone(),
                };
                let h = YozistFileHandle::open_new(
                    self.deps.engine.clone(),
                    display_name,
                    true,
                );
                Ok(Box::new(h))
            }
        }
    }

    async fn unlink(&self, path: &SmbPath) -> SmbResult<()> {
        if path.is_root() {
            return Err(SmbError::AccessDenied);
        }
        let components = path.components();
        if components.len() != 1 {
            return Err(SmbError::PathNotFound);
        }
        let (id, _) = Self::parse_filename(&components[0]).ok_or(SmbError::NotFound)?;
        let mut meta = self
            .deps
            .meta
            .get_file(&id)
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
            .ok_or(SmbError::NotFound)?;
        meta.deleted = true;
        meta.updated_at = time::OffsetDateTime::now_utc();
        self.deps
            .meta
            .update_file(&meta)
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
        Ok(())
    }

    async fn rename(&self, from: &SmbPath, to: &SmbPath) -> SmbResult<()> {
        let from_comp = from.components();
        let to_comp = to.components();
        if from_comp.len() != 1 || to_comp.len() != 1 {
            return Err(SmbError::PathNotFound);
        }
        let (id, _) = Self::parse_filename(&from_comp[0]).ok_or(SmbError::NotFound)?;
        let mut meta = self
            .deps
            .meta
            .get_file(&id)
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
            .ok_or(SmbError::NotFound)?;

        // 新名から ID プレフィクスを剥がして display_name 更新
        let new_name = match to_comp[0].split_once(ID_SEP) {
            Some((_, rest)) => rest.to_string(),
            None => to_comp[0].clone(),
        };
        meta.display_name = new_name;
        meta.updated_at = time::OffsetDateTime::now_utc();
        self.deps
            .meta
            .update_file(&meta)
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
        Ok(())
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: false,
            case_sensitive: false,
        }
    }
}

/// 階層パス＝タグ AND 条件として解釈する share（v2 stub）。
pub struct TagsBackend {
    #[allow(dead_code)]
    deps: ShareDeps,
}
impl TagsBackend {
    pub fn new(deps: ShareDeps) -> Self {
        Self { deps }
    }
}
#[async_trait]
impl ShareBackend for TagsBackend {
    async fn open(&self, _p: &SmbPath, _o: OpenOptions) -> SmbResult<Box<dyn Handle>> {
        Err(SmbError::NotSupported)
    }
    async fn unlink(&self, _p: &SmbPath) -> SmbResult<()> {
        Err(SmbError::NotSupported)
    }
    async fn rename(&self, _f: &SmbPath, _t: &SmbPath) -> SmbResult<()> {
        Err(SmbError::NotSupported)
    }
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: false,
            case_sensitive: false,
        }
    }
}

/// シリーズ単位の順序付きビュー（v2 stub）。
pub struct SeriesBackend {
    #[allow(dead_code)]
    deps: ShareDeps,
}
impl SeriesBackend {
    pub fn new(deps: ShareDeps) -> Self {
        Self { deps }
    }
}
#[async_trait]
impl ShareBackend for SeriesBackend {
    async fn open(&self, _p: &SmbPath, _o: OpenOptions) -> SmbResult<Box<dyn Handle>> {
        Err(SmbError::NotSupported)
    }
    async fn unlink(&self, _p: &SmbPath) -> SmbResult<()> {
        Err(SmbError::NotSupported)
    }
    async fn rename(&self, _f: &SmbPath, _t: &SmbPath) -> SmbResult<()> {
        Err(SmbError::NotSupported)
    }
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: false,
            case_sensitive: false,
        }
    }
}

/// 直近更新の読取専用ビュー（v2 stub）。
pub struct RecentBackend {
    #[allow(dead_code)]
    deps: ShareDeps,
}
impl RecentBackend {
    pub fn new(deps: ShareDeps) -> Self {
        Self { deps }
    }
}
#[async_trait]
impl ShareBackend for RecentBackend {
    async fn open(&self, _p: &SmbPath, _o: OpenOptions) -> SmbResult<Box<dyn Handle>> {
        Err(SmbError::NotSupported)
    }
    async fn unlink(&self, _p: &SmbPath) -> SmbResult<()> {
        Err(SmbError::NotSupported)
    }
    async fn rename(&self, _f: &SmbPath, _t: &SmbPath) -> SmbResult<()> {
        Err(SmbError::NotSupported)
    }
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: true,
            case_sensitive: false,
        }
    }
}
