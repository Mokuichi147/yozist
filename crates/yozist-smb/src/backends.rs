//! Share 別バックエンド実装。
//!
//! - **AllBackend**: 全ファイルをフラットに `<file-id>__<display_name>` で公開
//! - **TagsBackend**: 階層パス = タグ AND 条件（v2 で実装）
//! - **SeriesBackend**: 順序プレフィクス付きメンバー（v2 で実装）
//! - **RecentBackend**: 直近更新の読取専用ビュー（v2 で実装）

use async_trait::async_trait;
use smb_server::{
    BackendCapabilities, DirEntry, FileInfo, Handle, Identity, OpenIntent, OpenOptions,
    ShareBackend, SmbError, SmbPath, SmbResult,
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
        identity: &Identity,
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
            (OpenIntent::Open | OpenIntent::OpenOrCreate, Some(meta)) => {
                if opts.directory {
                    return Err(SmbError::NotADirectory);
                }
                // 読み取りは READ、書き込みも想定する場合は WRITE を要求。
                let mask = if opts.write {
                    yozist_auth::PermissionMask::WRITE | yozist_auth::PermissionMask::READ
                } else {
                    yozist_auth::PermissionMask::READ
                };
                self.deps
                    .require(identity, &yozist_auth::Target::File(meta.id), mask)
                    .await?;
                let h = YozistFileHandle::open_existing(
                    &self.deps,
                    self.deps.engine.clone(),
                    meta.id,
                    meta.display_name.clone(),
                    opts.read,
                    opts.write,
                )
                .await?;
                Ok(Box::new(h.with_smb_audit(identity, self.deps.audit.clone())))
            }
            (OpenIntent::Truncate | OpenIntent::OverwriteOrCreate, Some(meta)) => {
                self.deps
                    .require(
                        identity,
                        &yozist_auth::Target::File(meta.id),
                        yozist_auth::PermissionMask::WRITE,
                    )
                    .await?;
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
                Ok(Box::new(h.with_smb_audit(identity, self.deps.audit.clone())))
            }
            (OpenIntent::Create | OpenIntent::OpenOrCreate | OpenIntent::OverwriteOrCreate, None) => {
                if opts.directory {
                    return Err(SmbError::NotSupported);
                }
                // 新規ファイル: 認証済みのみ許可。close 時にオーナー ACL を付与。
                let ctx = self.deps.identity_to_context(identity).await;
                let owner = match &ctx {
                    yozist_auth::AuthContext::User { user, .. } => user.id,
                    _ => return Err(SmbError::AccessDenied),
                };
                let display_name = match name.split_once(ID_SEP) {
                    Some((_, rest)) => rest.to_string(),
                    None => name.clone(),
                };
                let h = YozistFileHandle::open_new_with_owner(
                    self.deps.engine.clone(),
                    self.deps.meta.clone(),
                    self.deps.acl_admin.clone(),
                    display_name,
                    true,
                    owner,
                    vec![],
                    None,
                );
                Ok(Box::new(h.with_smb_audit(identity, self.deps.audit.clone())))
            }
        }
    }

    async fn unlink(&self, identity: &Identity, path: &SmbPath) -> SmbResult<()> {
        if path.is_root() {
            return Err(SmbError::AccessDenied);
        }
        let components = path.components();
        if components.len() != 1 {
            return Err(SmbError::PathNotFound);
        }
        let (id, _) = Self::parse_filename(&components[0]).ok_or(SmbError::NotFound)?;
        self.deps
            .require(
                identity,
                &yozist_auth::Target::File(id),
                yozist_auth::PermissionMask::WRITE,
            )
            .await?;
        let res = async {
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
            Ok::<_, SmbError>(())
        }
        .await;
        let id_str = id.to_string();
        self.deps
            .audit_smb(identity, "delete_file", Some("file"), Some(&id_str), &res)
            .await;
        res
    }

    async fn rename(
        &self,
        identity: &Identity,
        from: &SmbPath,
        to: &SmbPath,
    ) -> SmbResult<()> {
        let from_comp = from.components();
        let to_comp = to.components();
        if from_comp.len() != 1 || to_comp.len() != 1 {
            return Err(SmbError::PathNotFound);
        }
        let (id, _) = Self::parse_filename(&from_comp[0]).ok_or(SmbError::NotFound)?;
        self.deps
            .require(
                identity,
                &yozist_auth::Target::File(id),
                yozist_auth::PermissionMask::WRITE,
            )
            .await?;
        let res = async {
            let mut meta = self
                .deps
                .meta
                .get_file(&id)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
                .ok_or(SmbError::NotFound)?;
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
            Ok::<_, SmbError>(())
        }
        .await;
        let id_str = id.to_string();
        self.deps
            .audit_smb(identity, "rename_file", Some("file"), Some(&id_str), &res)
            .await;
        res
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: false,
            case_sensitive: false,
        }
    }
}

/// 階層パス＝タグ AND 条件として解釈する share。
///
/// パス例:
/// - `\` → 全タグを subdir として表示
/// - `\work\` → タグ「work」を持つ全ファイルを表示
/// - `\work\urgent\` → 「work」AND「urgent」両方を持つファイル
///
/// 操作セマンティクス:
/// - `mkdir tags\foo` → 新規 Manual タグ作成
/// - `cp file → tags\work\` → 新規ファイル + work タグ付与
/// - `rm tags\work\foo` → そのファイルから work タグを取り外す
///   （ファイル実体は残る）
/// - `mv tags\A\foo → tags\B\foo` → A→B のタグ付け替え
pub struct TagsBackend {
    deps: ShareDeps,
}
impl TagsBackend {
    pub fn new(deps: ShareDeps) -> Self {
        Self { deps }
    }

    /// パスの各コンポーネントをタグとファイルに分解。
    /// 末尾要素が `<uuid>__<name>` ならファイル、それ以外は全てタグ。
    async fn parse_path(
        &self,
        comps: &[String],
    ) -> SmbResult<(Vec<yozist_core::TagId>, Option<(yozist_core::FileId, String)>)> {
        if comps.is_empty() {
            return Ok((vec![], None));
        }
        let (last, rest) = comps.split_last().unwrap();
        // 末尾がファイル形式か判定
        let file = if let Some((id_str, _name)) = last.split_once(ID_SEP) {
            if let Ok(u) = uuid::Uuid::parse_str(id_str) {
                Some((yozist_core::FileId::from_uuid(u), last.clone()))
            } else {
                None
            }
        } else {
            None
        };

        let tag_names: Vec<&str> = if file.is_some() {
            rest.iter().map(String::as_str).collect()
        } else {
            comps.iter().map(String::as_str).collect()
        };

        let mut tag_ids = Vec::with_capacity(tag_names.len());
        for name in tag_names {
            let tag = self
                .deps
                .meta
                .get_tag_by_name(name)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
                .ok_or(SmbError::PathNotFound)?;
            tag_ids.push(tag.id);
        }
        Ok((tag_ids, file))
    }

    async fn list_dir_entries(
        &self,
        tag_ids: &[yozist_core::TagId],
    ) -> SmbResult<Vec<DirEntry>> {
        let mut out = Vec::new();

        // 1. 全タグを subdir として列挙（自分自身を含むタグは除外）
        let all_tags = self
            .deps
            .meta
            .list_tags()
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
        let now = crate::handle::system_time_to_filetime(std::time::SystemTime::now());
        for t in &all_tags {
            if tag_ids.contains(&t.id) {
                continue;
            }
            out.push(DirEntry {
                info: FileInfo {
                    name: t.name.clone(),
                    end_of_file: 0,
                    allocation_size: 0,
                    creation_time: now,
                    last_access_time: now,
                    last_write_time: now,
                    change_time: now,
                    is_directory: true,
                    file_index: 0,
                },
            });
        }

        // 2. tag_ids に該当するファイル一覧
        let files = if tag_ids.is_empty() {
            // ルートではファイル一覧は出さない（タグだけ表示）
            vec![]
        } else {
            self.deps
                .meta
                .list_files_by_tags(tag_ids)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
        };
        for meta in files {
            let name = format!("{}{}{}", meta.id, ID_SEP, meta.display_name);
            out.push(DirEntry {
                info: crate::handle::file_meta_to_info(&meta, name),
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl ShareBackend for TagsBackend {
    async fn open(
        &self,
        identity: &Identity,
        path: &SmbPath,
        opts: OpenOptions,
    ) -> SmbResult<Box<dyn Handle>> {
        let comps = path.components();

        // ルート
        if comps.is_empty() {
            if opts.non_directory {
                return Err(SmbError::IsDirectory);
            }
            let entries = self.list_dir_entries(&[]).await?;
            return Ok(Box::new(YozistDirHandle::new("tags", entries)));
        }

        let (tag_ids, file) = match self.parse_path(comps).await {
            Ok(v) => v,
            Err(SmbError::PathNotFound) => {
                // 存在しないタグ名 → mkdir 系の Create か新規ファイル作成かを判定
                match opts.intent {
                    OpenIntent::Create
                    | OpenIntent::OpenOrCreate
                    | OpenIntent::OverwriteOrCreate => {
                        // 末尾要素を新規タグまたは新規ファイルとして扱う
                        let (last, rest) = comps.split_last().unwrap();
                        // 親パス（rest）は全て既存タグでなければエラー
                        let mut parent_tag_ids = Vec::new();
                        for name in rest {
                            let tag = self
                                .deps
                                .meta
                                .get_tag_by_name(name)
                                .await
                                .map_err(|e| {
                                    SmbError::Io(std::io::Error::other(e.to_string()))
                                })?
                                .ok_or(SmbError::PathNotFound)?;
                            parent_tag_ids.push(tag.id);
                        }
                        // 認証必須（新規タグ/ファイル作成）
                        let ctx = self.deps.identity_to_context(identity).await;
                        let owner = match &ctx {
                            yozist_auth::AuthContext::User { user, .. } => user.id,
                            _ => return Err(SmbError::AccessDenied),
                        };
                        if opts.directory {
                            // mkdir → 新規タグ作成
                            self.deps
                                .meta
                                .upsert_tag(&yozist_core::Tag {
                                    id: yozist_core::TagId::new(),
                                    name: last.clone(),
                                    kind: yozist_core::TagKind::Manual,
                                    confidence: None,
                                })
                                .await
                                .map_err(|e| {
                                    SmbError::Io(std::io::Error::other(e.to_string()))
                                })?;
                            return Ok(Box::new(YozistDirHandle::new(last.clone(), vec![])));
                        }
                        // 新規ファイル: 親タグを自動付与 + オーナー ACL
                        let display = match last.split_once(ID_SEP) {
                            Some((_, rest)) => rest.to_string(),
                            None => last.clone(),
                        };
                        let h = YozistFileHandle::open_new_with_owner(
                            self.deps.engine.clone(),
                            self.deps.meta.clone(),
                            self.deps.acl_admin.clone(),
                            display,
                            true,
                            owner,
                            parent_tag_ids,
                            None,
                        );
                        return Ok(Box::new(h.with_smb_audit(identity, self.deps.audit.clone())));
                    }
                    _ => return Err(SmbError::PathNotFound),
                }
            }
            Err(e) => return Err(e),
        };

        match file {
            None => {
                // ディレクトリオープン
                if opts.non_directory {
                    return Err(SmbError::IsDirectory);
                }
                let entries = self.list_dir_entries(&tag_ids).await?;
                let name = comps.last().cloned().unwrap_or_else(|| "tags".into());
                Ok(Box::new(YozistDirHandle::new(name, entries)))
            }
            Some((file_id, full_name)) => {
                // ファイルオープン: 既存ファイルとして
                if opts.directory {
                    return Err(SmbError::NotADirectory);
                }
                let meta = self
                    .deps
                    .meta
                    .get_file(&file_id)
                    .await
                    .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
                    .ok_or(SmbError::NotFound)?;
                if matches!(opts.intent, OpenIntent::Create) {
                    return Err(SmbError::Exists);
                }
                let mask = if opts.write {
                    yozist_auth::PermissionMask::WRITE | yozist_auth::PermissionMask::READ
                } else {
                    yozist_auth::PermissionMask::READ
                };
                self.deps
                    .require(identity, &yozist_auth::Target::File(meta.id), mask)
                    .await?;
                let mut h = YozistFileHandle::open_existing(
                    &self.deps,
                    self.deps.engine.clone(),
                    meta.id,
                    meta.display_name.clone(),
                    opts.read,
                    opts.write,
                )
                .await?;
                if matches!(opts.intent, OpenIntent::Truncate | OpenIntent::OverwriteOrCreate) {
                    h.set_truncated();
                }
                let _ = full_name;
                Ok(Box::new(h.with_smb_audit(identity, self.deps.audit.clone())))
            }
        }
    }

    async fn unlink(&self, identity: &Identity, path: &SmbPath) -> SmbResult<()> {
        let comps = path.components();
        if comps.is_empty() {
            return Err(SmbError::AccessDenied);
        }
        let (tag_ids, file) = self.parse_path(comps).await?;
        match file {
            Some((file_id, _)) => {
                self.deps
                    .require(
                        identity,
                        &yozist_auth::Target::File(file_id),
                        yozist_auth::PermissionMask::WRITE,
                    )
                    .await?;
                let is_detach = !tag_ids.is_empty();
                let res = async {
                    if !is_detach {
                        let mut meta = self
                            .deps
                            .meta
                            .get_file(&file_id)
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
                    } else {
                        let last_tag = *tag_ids.last().unwrap();
                        self.deps
                            .meta
                            .detach_tag(&file_id, &last_tag)
                            .await
                            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
                    }
                    Ok::<_, SmbError>(())
                }
                .await;
                let id_str = file_id.to_string();
                let action = if is_detach { "detach_tag" } else { "delete_file" };
                self.deps
                    .audit_smb(identity, action, Some("file"), Some(&id_str), &res)
                    .await;
                res
            }
            None => Err(SmbError::NotEmpty),
        }
    }

    async fn rename(
        &self,
        identity: &Identity,
        from: &SmbPath,
        to: &SmbPath,
    ) -> SmbResult<()> {
        let (from_tags, from_file) = self.parse_path(from.components()).await?;
        let (to_tags, to_file) = self.parse_path(to.components()).await?;
        let (file_id, _) = match (from_file, to_file) {
            (Some(f), Some(_)) => f,
            (Some(f), None) => f,
            _ => return Err(SmbError::NotSupported),
        };
        self.deps
            .require(
                identity,
                &yozist_auth::Target::File(file_id),
                yozist_auth::PermissionMask::WRITE,
            )
            .await?;
        let res = async {
            if let Some(last) = from_tags.last() {
                self.deps
                    .meta
                    .detach_tag(&file_id, last)
                    .await
                    .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
            }
            for t in &to_tags {
                self.deps
                    .meta
                    .attach_tag(&file_id, t)
                    .await
                    .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
            }
            Ok::<_, SmbError>(())
        }
        .await;
        let id_str = file_id.to_string();
        self.deps
            .audit_smb(identity, "retag", Some("file"), Some(&id_str), &res)
            .await;
        res
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: false,
            case_sensitive: false,
        }
    }
}

/// シリーズ単位の順序付きビュー。
///
/// パス例:
/// - `\` → 全シリーズを subdir として表示
/// - `\manual\` → シリーズ「manual」のメンバーを order_index 昇順で表示
///   ファイル名は `NNNN__<file-id>__<display_name>` 形式（NNNN は4桁ゼロ詰め）
///
/// 操作セマンティクス:
/// - `mkdir series\foo` → 新規シリーズ作成
/// - `cp file → series\X\` → ファイル登録 + シリーズ X に末尾追加
/// - `rm series\X\foo` → そのファイルをシリーズ X から外す（実体は残る）
/// - リネームで `NNNN__` 部分を変更 → order_index 更新
pub struct SeriesBackend {
    deps: ShareDeps,
}
impl SeriesBackend {
    pub fn new(deps: ShareDeps) -> Self {
        Self { deps }
    }

    /// `NNNN__<file-id>__<name>` を分解。
    fn parse_member_name(
        name: &str,
    ) -> Option<(f64, yozist_core::FileId, String)> {
        let (idx_str, rest) = name.split_once(ID_SEP)?;
        let idx: f64 = idx_str.parse().ok()?;
        let (id_str, _display) = rest.split_once(ID_SEP)?;
        let uuid = uuid::Uuid::parse_str(id_str).ok()?;
        Some((idx, yozist_core::FileId::from_uuid(uuid), name.to_string()))
    }

    fn member_display_name(
        order: f64,
        file: &yozist_core::FileMeta,
    ) -> String {
        format!(
            "{:04}{}{}{}{}",
            order as i64, ID_SEP, file.id, ID_SEP, file.display_name
        )
    }

    async fn series_by_name(&self, name: &str) -> SmbResult<Option<yozist_core::Series>> {
        let list = self
            .deps
            .meta
            .list_series()
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
        Ok(list.into_iter().find(|s| s.name == name))
    }

    async fn list_root(&self) -> SmbResult<Vec<DirEntry>> {
        let list = self
            .deps
            .meta
            .list_series()
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
        let now = crate::handle::system_time_to_filetime(std::time::SystemTime::now());
        Ok(list
            .into_iter()
            .map(|s| DirEntry {
                info: FileInfo {
                    name: s.name,
                    end_of_file: 0,
                    allocation_size: 0,
                    creation_time: now,
                    last_access_time: now,
                    last_write_time: now,
                    change_time: now,
                    is_directory: true,
                    file_index: 0,
                },
            })
            .collect())
    }

    async fn list_series_dir(
        &self,
        series_id: &yozist_core::SeriesId,
    ) -> SmbResult<Vec<DirEntry>> {
        let members = self
            .deps
            .meta
            .list_series_members(series_id)
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
        let mut out = Vec::with_capacity(members.len());
        for m in members {
            let file = self
                .deps
                .meta
                .get_file(&m.file_id)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
            if let Some(f) = file {
                if f.deleted {
                    continue;
                }
                let name = Self::member_display_name(m.order_index, &f);
                out.push(DirEntry {
                    info: crate::handle::file_meta_to_info(&f, name),
                });
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl ShareBackend for SeriesBackend {
    async fn open(
        &self,
        identity: &Identity,
        path: &SmbPath,
        opts: OpenOptions,
    ) -> SmbResult<Box<dyn Handle>> {
        let comps = path.components();

        if comps.is_empty() {
            if opts.non_directory {
                return Err(SmbError::IsDirectory);
            }
            let entries = self.list_root().await?;
            return Ok(Box::new(YozistDirHandle::new("series", entries)));
        }

        // 第 1 階層: シリーズ名
        let series_name = &comps[0];
        let series = self.series_by_name(series_name).await?;

        match (comps.len(), series) {
            (1, Some(s)) => {
                if opts.non_directory {
                    return Err(SmbError::IsDirectory);
                }
                let entries = self.list_series_dir(&s.id).await?;
                Ok(Box::new(YozistDirHandle::new(s.name, entries)))
            }
            (1, None) => {
                // mkdir 系: 新規シリーズ作成
                match opts.intent {
                    OpenIntent::Create
                    | OpenIntent::OpenOrCreate
                    | OpenIntent::OverwriteOrCreate
                        if opts.directory =>
                    {
                        let s = yozist_core::Series {
                            id: yozist_core::SeriesId::new(),
                            name: series_name.clone(),
                            description: None,
                        };
                        self.deps
                            .meta
                            .upsert_series(&s)
                            .await
                            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
                        Ok(Box::new(YozistDirHandle::new(series_name.clone(), vec![])))
                    }
                    _ => Err(SmbError::PathNotFound),
                }
            }
            (2, Some(s)) => {
                let name = &comps[1];
                match Self::parse_member_name(name) {
                    Some((_order, file_id, _)) => {
                        // 既存ファイル
                        if opts.directory {
                            return Err(SmbError::NotADirectory);
                        }
                        let meta = self
                            .deps
                            .meta
                            .get_file(&file_id)
                            .await
                            .map_err(|e| {
                                SmbError::Io(std::io::Error::other(e.to_string()))
                            })?
                            .ok_or(SmbError::NotFound)?;
                        if matches!(opts.intent, OpenIntent::Create) {
                            return Err(SmbError::Exists);
                        }
                        let mask = if opts.write {
                            yozist_auth::PermissionMask::WRITE
                                | yozist_auth::PermissionMask::READ
                        } else {
                            yozist_auth::PermissionMask::READ
                        };
                        self.deps
                            .require(identity, &yozist_auth::Target::File(meta.id), mask)
                            .await?;
                        let mut h = YozistFileHandle::open_existing(
                            &self.deps,
                            self.deps.engine.clone(),
                            meta.id,
                            meta.display_name.clone(),
                            opts.read,
                            opts.write,
                        )
                        .await?;
                        if matches!(
                            opts.intent,
                            OpenIntent::Truncate | OpenIntent::OverwriteOrCreate
                        ) {
                            h.set_truncated();
                        }
                        Ok(Box::new(h.with_smb_audit(identity, self.deps.audit.clone())))
                    }
                    None => {
                        // 新規ファイル: シリーズ S に追加
                        if opts.directory {
                            return Err(SmbError::NotSupported);
                        }
                        let ctx = self.deps.identity_to_context(identity).await;
                        let owner = match &ctx {
                            yozist_auth::AuthContext::User { user, .. } => user.id,
                            _ => return Err(SmbError::AccessDenied),
                        };
                        match opts.intent {
                            OpenIntent::Create
                            | OpenIntent::OpenOrCreate
                            | OpenIntent::OverwriteOrCreate => {
                                // 末尾追加用 order_index
                                let existing = self
                                    .deps
                                    .meta
                                    .list_series_members(&s.id)
                                    .await
                                    .map_err(|e| {
                                        SmbError::Io(std::io::Error::other(e.to_string()))
                                    })?;
                                let order = existing
                                    .last()
                                    .map(|m| m.order_index + 10.0)
                                    .unwrap_or(10.0);
                                let h = YozistFileHandle::open_new_with_owner(
                                    self.deps.engine.clone(),
                                    self.deps.meta.clone(),
                                    self.deps.acl_admin.clone(),
                                    name.clone(),
                                    true,
                                    owner,
                                    vec![],
                                    Some((s.id, order)),
                                );
                                Ok(Box::new(h.with_smb_audit(identity, self.deps.audit.clone())))
                            }
                            _ => Err(SmbError::NotFound),
                        }
                    }
                }
            }
            _ => Err(SmbError::PathNotFound),
        }
    }

    async fn unlink(&self, identity: &Identity, path: &SmbPath) -> SmbResult<()> {
        let comps = path.components();
        if comps.len() != 2 {
            return Err(SmbError::AccessDenied);
        }
        let series = self
            .series_by_name(&comps[0])
            .await?
            .ok_or(SmbError::PathNotFound)?;
        let (_, file_id, _) =
            Self::parse_member_name(&comps[1]).ok_or(SmbError::NotFound)?;
        self.deps
            .require(
                identity,
                &yozist_auth::Target::File(file_id),
                yozist_auth::PermissionMask::WRITE,
            )
            .await?;
        let res = self
            .deps
            .meta
            .remove_from_series(&series.id, &file_id)
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())));
        let id_str = file_id.to_string();
        self.deps
            .audit_smb(
                identity,
                "remove_from_series",
                Some("file"),
                Some(&id_str),
                &res,
            )
            .await;
        res?;
        Ok(())
    }

    async fn rename(
        &self,
        identity: &Identity,
        from: &SmbPath,
        to: &SmbPath,
    ) -> SmbResult<()> {
        let from_comp = from.components();
        let to_comp = to.components();
        if from_comp.len() != 2 || to_comp.len() != 2 {
            return Err(SmbError::NotSupported);
        }
        let from_series = self
            .series_by_name(&from_comp[0])
            .await?
            .ok_or(SmbError::PathNotFound)?;
        let to_series = self
            .series_by_name(&to_comp[0])
            .await?
            .ok_or(SmbError::PathNotFound)?;

        let (_, file_id, _) =
            Self::parse_member_name(&from_comp[1]).ok_or(SmbError::NotFound)?;
        self.deps
            .require(
                identity,
                &yozist_auth::Target::File(file_id),
                yozist_auth::PermissionMask::WRITE,
            )
            .await?;
        let new_order = Self::parse_member_name(&to_comp[1]).map(|(o, _, _)| o);
        let file_id_str = file_id.to_string();

        let res = async {
            if from_series.id == to_series.id {
                if let Some(order) = new_order {
                    self.deps
                        .meta
                        .add_to_series(&yozist_core::SeriesMember {
                            series_id: from_series.id,
                            file_id,
                            order_index: order,
                        })
                        .await
                        .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
                }
                return Ok::<_, SmbError>(());
            }
            self.deps
                .meta
                .remove_from_series(&from_series.id, &file_id)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
            let order = new_order.unwrap_or(10.0);
            self.deps
                .meta
                .add_to_series(&yozist_core::SeriesMember {
                    series_id: to_series.id,
                    file_id,
                    order_index: order,
                })
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
            Ok(())
        }
        .await;
        let action = if from_series.id == to_series.id {
            "reorder_series_member"
        } else {
            "move_series_member"
        };
        self.deps
            .audit_smb(identity, action, Some("file"), Some(&file_id_str), &res)
            .await;
        res
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: false,
            case_sensitive: false,
        }
    }
}

/// 保存クエリを SMB share として公開する読取専用ビュー。
///
/// パス例:
/// - `\` → 全 saved_query を subdir として表示
/// - `\<query-name>\` → そのクエリで絞り込まれたファイル一覧
pub struct QueriesBackend {
    deps: ShareDeps,
}

impl QueriesBackend {
    pub fn new(deps: ShareDeps) -> Self {
        Self { deps }
    }

    async fn list_root(&self) -> SmbResult<Vec<DirEntry>> {
        let queries = self
            .deps
            .meta
            .list_saved_queries()
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
        let now = crate::handle::system_time_to_filetime(std::time::SystemTime::now());
        Ok(queries
            .into_iter()
            .map(|q| DirEntry {
                info: FileInfo {
                    name: q.name,
                    end_of_file: 0,
                    allocation_size: 0,
                    creation_time: now,
                    last_access_time: now,
                    last_write_time: now,
                    change_time: now,
                    is_directory: true,
                    file_index: 0,
                },
            })
            .collect())
    }

    async fn resolve(
        &self,
        q: &yozist_core::QueryDef,
    ) -> SmbResult<Vec<yozist_core::FileMeta>> {
        // タグ名→ID解決 + AND/NOT
        let mut and_ids = Vec::new();
        for name in &q.tags_and {
            let t = self
                .deps
                .meta
                .get_tag_by_name(name)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
            match t {
                Some(t) => and_ids.push(t.id),
                None => return Ok(vec![]),
            }
        }
        let mut not_ids = Vec::new();
        for name in &q.tags_not {
            if let Some(t) = self
                .deps
                .meta
                .get_tag_by_name(name)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
            {
                not_ids.push(t.id);
            }
        }
        let candidates = if and_ids.is_empty() {
            self.deps
                .meta
                .list_files(1000, 0)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
        } else {
            self.deps
                .meta
                .list_files_by_tags(&and_ids)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
        };
        if not_ids.is_empty() {
            return Ok(candidates);
        }
        let mut out = Vec::new();
        for f in candidates {
            let tags = self
                .deps
                .meta
                .list_tags_of(&f.id)
                .await
                .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?;
            if !tags.iter().any(|t| not_ids.contains(&t.id)) {
                out.push(f);
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl ShareBackend for QueriesBackend {
    async fn open(
        &self,
        identity: &Identity,
        path: &SmbPath,
        opts: OpenOptions,
    ) -> SmbResult<Box<dyn Handle>> {
        let comps = path.components();
        if comps.is_empty() {
            if opts.non_directory {
                return Err(SmbError::IsDirectory);
            }
            let entries = self.list_root().await?;
            return Ok(Box::new(YozistDirHandle::new("queries", entries)));
        }
        let query = self
            .deps
            .meta
            .get_saved_query_by_name(&comps[0])
            .await
            .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
            .ok_or(SmbError::PathNotFound)?;

        match comps.len() {
            1 => {
                if opts.non_directory {
                    return Err(SmbError::IsDirectory);
                }
                let files = self.resolve(&query.query).await?;
                let mut entries = Vec::with_capacity(files.len());
                for meta in files {
                    let name = format!("{}{}{}", meta.id, ID_SEP, meta.display_name);
                    entries.push(DirEntry {
                        info: crate::handle::file_meta_to_info(&meta, name),
                    });
                }
                Ok(Box::new(YozistDirHandle::new(query.name, entries)))
            }
            2 => {
                // ファイル開く: <file-id>__<name>
                let (id_str, _) = comps[1]
                    .split_once(ID_SEP)
                    .ok_or(SmbError::NotFound)?;
                let uuid =
                    uuid::Uuid::parse_str(id_str).map_err(|_| SmbError::NotFound)?;
                let file_id = yozist_core::FileId::from_uuid(uuid);
                // 読取専用 share だが READ 権限は確認する
                self.deps
                    .require(
                        identity,
                        &yozist_auth::Target::File(file_id),
                        yozist_auth::PermissionMask::READ,
                    )
                    .await?;
                let meta = self
                    .deps
                    .meta
                    .get_file(&file_id)
                    .await
                    .map_err(|e| SmbError::Io(std::io::Error::other(e.to_string())))?
                    .ok_or(SmbError::NotFound)?;
                let h = YozistFileHandle::open_existing(
                    &self.deps,
                    self.deps.engine.clone(),
                    meta.id,
                    meta.display_name.clone(),
                    opts.read,
                    false, // 読取専用ビュー
                )
                .await?;
                Ok(Box::new(h.with_smb_audit(identity, self.deps.audit.clone())))
            }
            _ => Err(SmbError::PathNotFound),
        }
    }
    async fn unlink(&self, _id: &Identity, _p: &SmbPath) -> SmbResult<()> {
        Err(SmbError::AccessDenied)
    }
    async fn rename(&self, _id: &Identity, _f: &SmbPath, _t: &SmbPath) -> SmbResult<()> {
        Err(SmbError::AccessDenied)
    }
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: true,
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
    async fn open(
        &self,
        _id: &Identity,
        _p: &SmbPath,
        _o: OpenOptions,
    ) -> SmbResult<Box<dyn Handle>> {
        Err(SmbError::NotSupported)
    }
    async fn unlink(&self, _id: &Identity, _p: &SmbPath) -> SmbResult<()> {
        Err(SmbError::NotSupported)
    }
    async fn rename(&self, _id: &Identity, _f: &SmbPath, _t: &SmbPath) -> SmbResult<()> {
        Err(SmbError::NotSupported)
    }
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: true,
            case_sensitive: false,
        }
    }
}
