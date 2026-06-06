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

use crate::handle::{
    file_meta_to_info, ScratchFileHandle, ScratchFs, SharedScratch,
    YozistDirHandle, YozistFileHandle,
};
use crate::ShareDeps;
use parking_lot::Mutex;
use std::sync::Arc;
use tracing::debug;

const ID_SEP: &str = "__";

/// フラット名前空間を display_name で線形検索する際の上限。
const ALL_LIST_LIMIT: u32 = 1000;
/// スクラッチ（一時保存）エントリの固定 FILETIME。listing 毎に揺らさない。
const SCRATCH_FILETIME: u64 = 133_000_000_000_000_000;

/// macOS が同階層に撒く付随メタデータ（AppleDouble `._*` と `.DS_Store`）を
/// ルート直下で受けるための予約スクラッチ・ディレクトリ。SMB 名に現れ得ない
/// NUL を含めることで実ディレクトリ名と衝突せず、`list_root` にも出さない
/// （`dirs` には登録しない）。`[[project_smb_safe_save]]`
const EPHEMERAL_DIR: &str = "\u{0}meta";

/// macOS が本体ファイルに付随して撒くメタデータ名か。AppleDouble（`._*`、
/// 拡張属性/リソースフォーク）と `.DS_Store`（フォルダ表示状態）が該当する。
/// これらは yozist の実ファイルとして永続化する価値がないため、メモリ上の
/// ephemeral スクラッチに閉じ込め、接続が切れれば消えるようにする。
fn is_apple_metadata(name: &str) -> bool {
    name.starts_with("._") || name == ".DS_Store"
}

/// `DbError` 等の文字列化可能なエラーを SMB の IO エラーへ畳み込む共通ヘルパ。
fn io_err<E: std::fmt::Display>(e: E) -> SmbError {
    SmbError::Io(std::io::Error::other(e.to_string()))
}

/// 全ファイルをフラットに公開する管理用 share。
///
/// パス規則: ファイルは `<file-uuid>__<display_name>` として現れる。
/// `mkdir` は不可。`rmdir` も不可（ルートのみ）。
pub struct AllBackend {
    deps: ShareDeps,
    /// macOS のアトミック保存が作る一時サブディレクトリ（任意名）を受け止める
    /// メモリ上スクラッチ FS。
    scratch: SharedScratch,
}

impl AllBackend {
    pub fn new(deps: ShareDeps) -> Self {
        Self {
            deps,
            scratch: Arc::new(Mutex::new(ScratchFs::default())),
        }
    }

    /// スクラッチ FS 上の `dir/file` を表す DirEntry を作る（list_dir 用）。
    fn scratch_entry(name: &str, size: u64) -> DirEntry {
        // 時刻は固定値。listing 毎に now() で揺らすと macOS smbfs が
        // 「ファイルが変化し続けている」と誤認してキャッシュを破棄するため。
        DirEntry {
            info: FileInfo {
                name: name.to_string(),
                end_of_file: size,
                allocation_size: size,
                creation_time: SCRATCH_FILETIME,
                last_access_time: SCRATCH_FILETIME,
                last_write_time: SCRATCH_FILETIME,
                change_time: SCRATCH_FILETIME,
                is_directory: false,
                file_index: 0,
            },
        }
    }

    /// スクラッチ・ディレクトリ自身の DirEntry（ルート列挙用）。
    fn scratch_dir_entry(name: &str) -> DirEntry {
        DirEntry {
            info: FileInfo {
                name: name.to_string(),
                end_of_file: 0,
                allocation_size: 0,
                creation_time: SCRATCH_FILETIME,
                last_access_time: SCRATCH_FILETIME,
                last_write_time: SCRATCH_FILETIME,
                change_time: SCRATCH_FILETIME,
                is_directory: true,
                file_index: 0,
            },
        }
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
            .list_files(ALL_LIST_LIMIT, 0)
            .await
            .map_err(io_err)?;
        // ドット始まり（旧版で永続化されてしまった `._*`/`.DS_Store` 等）も隠さず
        // そのまま一覧へ出す。隠すと Finder から手動で消せず残骸を管理できないため。
        // 新規の `._*`/`.DS_Store` は永続化せず ephemeral に閉じ込めるので、ここに
        // 現れるのは過去の残骸のみ（ユーザーが見て削除できる）。
        let mut entries: Vec<DirEntry> = files
            .into_iter()
            .map(|meta| {
                let name = Self::display_filename(&meta);
                DirEntry {
                    info: file_meta_to_info(&meta, name),
                }
            })
            .collect();
        // 仮想スクラッチ・ディレクトリ（macOS のアトミック保存 temp）も列挙する。
        // これが無いと、temp フォルダを mkdir した直後にルートを再列挙した
        // macOS が「作ったはずの temp が消えた」と誤認し、保存を中断して
        // 「保存できませんでした」エラーになる（rename による保存確定に進めない）。
        for dir in self.scratch.lock().dirs.iter() {
            entries.push(Self::scratch_dir_entry(dir));
        }
        Ok(entries)
    }

    /// SMB 上の名前を既存ファイルへ解決する。
    ///
    /// 1. 正規形 `<uuid>__<display_name>`（一覧で見える形）に完全一致するものを優先。
    /// 2. それ以外は display_name 完全一致でフラット名前空間を検索する。
    ///
    /// 2 があることで、クライアントが作成・rename に用いる「自分が付けた名前」
    /// （macOS の安全保存が作る一時ファイル名など）でも開き直せる。
    async fn resolve_existing(
        &self,
        name: &str,
    ) -> SmbResult<Option<yozist_core::FileMeta>> {
        if let Some((id, _)) = Self::parse_filename(name) {
            let canonical = self
                .deps
                .meta
                .get_file(&id)
                .await
                .map_err(io_err)?
                .filter(|m| !m.deleted && Self::display_filename(m) == name);
            if canonical.is_some() {
                return Ok(canonical);
            }
        }
        let files = self
            .deps
            .meta
            .list_files(ALL_LIST_LIMIT, 0)
            .await
            .map_err(io_err)?;
        Ok(files.into_iter().find(|m| m.display_name == name))
    }

    /// 名前に埋め込まれた UUID から既存ファイルを引く（display_name / deleted は
    /// 問わない）。macOS の安全保存スワップ（原本を別名へ rename → 新内容を
    /// 正規名へ rename → 脇に避けた原本を削除）では、正規名に埋まった UUID こそが
    /// 本来の identity。原本が脇へ避けられても、ここから identity を回復して
    /// 履歴を本体に積み続ける。`[[project_smb_safe_save]]`
    async fn resolve_by_embedded_id(
        &self,
        name: &str,
    ) -> SmbResult<Option<yozist_core::FileMeta>> {
        let Some((id, _)) = Self::parse_filename(name) else {
            return Ok(None);
        };
        self.deps.meta.get_file(&id).await.map_err(io_err)
    }

    /// 正規名 `<uuid>__<display>` の display 部分を取り出す。UUID 接頭辞が無い
    /// 名前（`._…` AppleDouble 等）は丸ごと display として扱う（接頭辞を誤って
    /// 剥がして実ファイル名に化けさせない）。
    fn display_part(name: &str) -> String {
        match Self::parse_filename(name) {
            Some(_) => name
                .split_once(ID_SEP)
                .map(|(_, r)| r.to_string())
                .unwrap_or_else(|| name.to_string()),
            None => name.to_string(),
        }
    }

    /// スクラッチ・ディレクトリ配下のファイル一覧（DirEntry）。
    fn scratch_dir_entries(&self, dir: &str) -> Vec<DirEntry> {
        let sc = self.scratch.lock();
        sc.entries_in(dir)
            .into_iter()
            .map(|f| {
                let size = sc
                    .files
                    .get(&format!("{dir}/{f}"))
                    .map(|b| b.len())
                    .unwrap_or(0) as u64;
                Self::scratch_entry(&f, size)
            })
            .collect()
    }

    /// スクラッチ FS 上のファイル（`dir/file`）を開く。
    async fn open_scratch_file(
        &self,
        dir: &str,
        file: &str,
        opts: OpenOptions,
    ) -> SmbResult<Box<dyn Handle>> {
        if opts.directory {
            return Err(SmbError::NotSupported); // ネストしたディレクトリは非対応
        }
        let key = format!("{dir}/{file}");
        debug!(
            share = "all",
            dir,
            file,
            intent = ?opts.intent,
            write = opts.write,
            "scratch file open"
        );
        let mut sc = self.scratch.lock();
        let exists = sc.files.contains_key(&key);
        match (opts.intent, exists) {
            (OpenIntent::Open, false) | (OpenIntent::Truncate, false) => Err(SmbError::NotFound),
            (OpenIntent::Create, true) => Err(SmbError::Exists),
            _ => {
                sc.dirs.insert(dir.to_string());
                let buf = sc.files.entry(key.clone()).or_default();
                if matches!(opts.intent, OpenIntent::Truncate | OpenIntent::OverwriteOrCreate) {
                    buf.clear();
                }
                drop(sc);
                Ok(Box::new(ScratchFileHandle::new(
                    self.scratch.clone(),
                    key,
                    file.to_string(),
                    opts.read,
                    opts.write,
                )))
            }
        }
    }

    /// ルート直下の AppleDouble / `.DS_Store` をメモリ上 ephemeral として開く。
    /// `open_scratch_file` と違い `dirs` には登録しないので `list_root` には
    /// 現れず、DB/ストレージにも一切永続化されない。接続が切れれば消える。
    async fn open_ephemeral_file(
        &self,
        name: &str,
        opts: OpenOptions,
    ) -> SmbResult<Box<dyn Handle>> {
        if opts.directory {
            return Err(SmbError::NotSupported);
        }
        let key = format!("{EPHEMERAL_DIR}/{name}");
        debug!(
            share = "all",
            name,
            intent = ?opts.intent,
            write = opts.write,
            "ephemeral (AppleDouble/.DS_Store) open"
        );
        let mut sc = self.scratch.lock();
        let exists = sc.files.contains_key(&key);
        match (opts.intent, exists) {
            (OpenIntent::Open, false) | (OpenIntent::Truncate, false) => Err(SmbError::NotFound),
            (OpenIntent::Create, true) => Err(SmbError::Exists),
            _ => {
                let buf = sc.files.entry(key.clone()).or_default();
                if matches!(opts.intent, OpenIntent::Truncate | OpenIntent::OverwriteOrCreate) {
                    buf.clear();
                }
                drop(sc);
                Ok(Box::new(ScratchFileHandle::new(
                    self.scratch.clone(),
                    key,
                    name.to_string(),
                    opts.read,
                    opts.write,
                )))
            }
        }
    }

    /// スクラッチ内ファイルの内容を本体ファイル `to_name` へ反映する（保存の確定）。
    /// 既存ファイルなら commit、無ければ新規 create_file。
    async fn fold_scratch_into_file(
        &self,
        identity: &Identity,
        from_key: &str,
        to_name: &str,
    ) -> SmbResult<()> {
        let content = {
            self.scratch.lock().files.get(from_key).cloned()
        };
        let content = content.ok_or(SmbError::NotFound)?;
        // 正規名一致を優先。見つからなければ「正規名に埋め込まれた UUID」で
        // 既存ファイルを回復する（macOS の安全保存スワップで原本が脇へ避け
        // られていても、同じ識別子に履歴を積み続けるため）。
        let target = match self.resolve_existing(to_name).await? {
            Some(m) => Some(m),
            None => self.resolve_by_embedded_id(to_name).await?,
        };
        let res = match target {
            Some(target) => {
                self.deps
                    .require(
                        identity,
                        &yozist_auth::Target::file(target.id),
                        yozist_auth::PermissionMask::WRITE,
                    )
                    .await?;
                let id = target.id;
                // 脇へ避けられた／論理削除された原本を正規名へ戻す
                // （commit が live なメタを前提とするため先に復元する）。
                let display = Self::display_part(to_name);
                if target.deleted || target.display_name != display {
                    let mut m = target.clone();
                    m.deleted = false;
                    m.display_name = display;
                    m.updated_at = time::OffsetDateTime::now_utc();
                    if let Err(e) = self.deps.meta.update_file(&m).await.map_err(io_err) {
                        self.scratch.lock().files.remove(from_key);
                        return Err(e);
                    }
                }
                let r = self
                    .deps
                    .engine
                    .commit(id, &content, yozist_core::ActorId::new(), Some("smb".into()))
                    .await
                    .map_err(io_err)
                    .map(|_| id.to_string());
                self.deps
                    .audit_smb(identity, "save_via_scratch", Some("file"), r.as_ref().ok().map(|s| s.as_str()), &r)
                    .await;
                r.map(|_| ())
            }
            None => {
                // 新規本体ファイル。
                let ctx = self.deps.identity_to_context(identity).await;
                let owner = match &ctx {
                    yozist_auth::AuthContext::User { user, .. } => user.id,
                    _ => return Err(SmbError::AccessDenied),
                };
                let display = Self::display_part(to_name);
                let r = self
                    .deps
                    .engine
                    .create_file(display, &content, yozist_core::ActorId::new(), None)
                    .await
                    .map_err(io_err);
                match r {
                    Ok((file, _)) => {
                        let owner_rule = yozist_auth::Permission {
                            subject: yozist_auth::Subject::User(owner),
                            target: yozist_auth::Target::file(file.id),
                            mask: yozist_auth::PermissionMask::all(),
                            allow: true,
                            priority: i32::MAX,
                        };
                        let _ = self.deps.acl_admin.add_rule(&owner_rule).await;
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        };
        // 反映できたらスクラッチから除去。
        if res.is_ok() {
            self.scratch.lock().files.remove(from_key);
        }
        res
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

        // --- スクラッチ FS（macOS のアトミック保存用・一時サブディレクトリ）経路 ---
        if components.len() == 2 {
            return self
                .open_scratch_file(&components[0], &components[1], opts)
                .await;
        }
        if components.len() != 1 {
            return Err(SmbError::PathNotFound);
        }
        let name = &components[0];

        // macOS の付随メタデータ（AppleDouble `._*` / `.DS_Store`）はメモリ上の
        // ephemeral に閉じ込め、DB/ストレージへ永続化しない。これが無いと
        // 保存のたびに `._<uuid>__<filename>` が実ファイルとして残り続ける。
        if is_apple_metadata(name) {
            return self.open_ephemeral_file(name, opts).await;
        }

        // 既存のスクラッチ・ディレクトリを開く。
        if self.scratch.lock().dirs.contains(name) {
            if opts.non_directory {
                return Err(SmbError::IsDirectory);
            }
            debug!(share = "all", dir = name.as_str(), "scratch dir open");
            let entries = self.scratch_dir_entries(name);
            return Ok(Box::new(YozistDirHandle::new(name.clone(), entries)));
        }
        // 任意名の mkdir を仮想ディレクトリとして受理（保存用 temp dir）。
        if opts.directory {
            return match opts.intent {
                OpenIntent::Create | OpenIntent::OpenOrCreate | OpenIntent::OverwriteOrCreate => {
                    self.scratch.lock().dirs.insert(name.clone());
                    debug!(share = "all", dir = name.as_str(), intent = ?opts.intent, "scratch mkdir");
                    Ok(Box::new(YozistDirHandle::new(name.clone(), vec![])))
                }
                _ => Err(SmbError::NotFound),
            };
        }

        // 既存ファイル検索: 正規形 `<uuid>__name` だけでなく、クライアントが付けた
        // 名前（display_name 一致）でも解決する。これで macOS 等の
        // 「一時ファイル作成 → アトミック rename」型の安全保存に追従できる。
        let existing_meta = self.resolve_existing(name).await?;
        debug!(
            share = "all",
            name = name.as_str(),
            intent = ?opts.intent,
            write = opts.write,
            resolved = existing_meta.as_ref().map(|m| m.id.to_string()),
            "AllBackend::open"
        );

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
                    .require(identity, &yozist_auth::Target::file(meta.id), mask)
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
                        &yozist_auth::Target::file(meta.id),
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
                // 表示名はクライアントが指定した名前をそのまま使う（接頭辞を
                // 削らない）。これで「作成した名前で開き直す／rename する」操作が
                // round-trip し、`my__notes.txt` のような名前も保持される。
                let display_name = name.clone();
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
        // スクラッチ内ファイルの削除。
        if components.len() == 2 {
            let key = format!("{}/{}", components[0], components[1]);
            self.scratch.lock().files.remove(&key);
            debug!(share = "all", key = key.as_str(), "scratch unlink file");
            return Ok(());
        }
        if components.len() != 1 {
            return Err(SmbError::PathNotFound);
        }
        // スクラッチ・ディレクトリの rmdir。
        let is_scratch_dir = self.scratch.lock().dirs.contains(&components[0]);
        if is_scratch_dir {
            let dir = &components[0];
            // セーフティネット: macOS が rename(temp→本体) を出さずに
            // 書き込んだ temp フォルダを rmdir で畳もうとする（保存中断）
            // ケースに備え、フォルダ内に残っている既存本体宛ての内容を
            // 破棄前に本体へ fold する。これで「rename が出ない」変種でも
            // 編集内容が失われない。新規（本体未存在）の名前は救済しない。
            let prefix = format!("{dir}/");
            let pending: Vec<(String, String)> = {
                let sc = self.scratch.lock();
                sc.files
                    .keys()
                    .filter_map(|k| {
                        k.strip_prefix(&prefix)
                            .map(|name| (k.clone(), name.to_string()))
                    })
                    .collect()
            };
            for (key, name) in pending {
                // 既存本体（正規名 or 埋め込み UUID で実在）宛ての内容のみ救済する。
                // 新規名の打ち捨て temp から stray ファイルを生まないため。
                let maps_to_existing = self.resolve_existing(&name).await?.is_some()
                    || self.resolve_by_embedded_id(&name).await?.is_some();
                if maps_to_existing {
                    if let Err(e) = self.fold_scratch_into_file(identity, &key, &name).await {
                        debug!(share = "all", key = key.as_str(), error = %e, "scratch rmdir salvage failed");
                    } else {
                        debug!(share = "all", key = key.as_str(), name = name.as_str(), "scratch rmdir salvage");
                    }
                }
            }
            let mut sc = self.scratch.lock();
            sc.dirs.remove(dir);
            sc.files.retain(|k, _| !k.starts_with(&prefix));
            debug!(share = "all", dir = dir.as_str(), "scratch rmdir");
            return Ok(());
        }
        // AppleDouble / `.DS_Store` はメモリ上 ephemeral から除去する。旧版で
        // 永続化されてしまった legacy 実体は、続く通常削除（冪等）で後始末する。
        if is_apple_metadata(&components[0]) {
            let key = format!("{EPHEMERAL_DIR}/{}", components[0]);
            self.scratch.lock().files.remove(&key);
        }

        // 既に存在しない名前の削除は冪等に成功扱い（`rm -f` 相当）。macOS の
        // 安全保存スワップ後始末（identity 回復後に残骸 `.smbdelete…` を削除しに
        // 来る）が NotFound エラーにならないようにする。
        let mut meta = match self.resolve_existing(&components[0]).await? {
            Some(m) => m,
            None => {
                debug!(share = "all", name = components[0].as_str(), "AllBackend::unlink (対象なし・冪等成功)");
                return Ok(());
            }
        };
        let id = meta.id;
        debug!(share = "all", name = components[0].as_str(), id = %id, "AllBackend::unlink");
        self.deps
            .require(
                identity,
                &yozist_auth::Target::file(id),
                yozist_auth::PermissionMask::WRITE,
            )
            .await?;
        let res = async {
            meta.deleted = true;
            meta.updated_at = time::OffsetDateTime::now_utc();
            self.deps.meta.update_file(&meta).await.map_err(io_err)?;
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

        // --- ルート直下の AppleDouble / `.DS_Store`（メモリ上 ephemeral）---
        // これらは永続化しないので、from が ephemeral の rename はバッファ移動で
        // 完結させる（DB を経由させない）。移動先も付随メタデータなら内容を運び、
        // そうでなければ内容を捨てて冪等成功にする（実ファイルへ化けさせない）。
        if from_comp.len() == 1 && is_apple_metadata(&from_comp[0]) {
            let fk = format!("{EPHEMERAL_DIR}/{}", from_comp[0]);
            let content = self.scratch.lock().files.remove(&fk);
            if to_comp.len() == 1 && is_apple_metadata(&to_comp[0]) {
                if let Some(c) = content {
                    let tk = format!("{EPHEMERAL_DIR}/{}", to_comp[0]);
                    self.scratch.lock().files.insert(tk, c);
                }
            }
            debug!(share = "all", ?from_comp, ?to_comp, "AllBackend::rename (ephemeral)");
            return Ok(());
        }

        // --- スクラッチ FS（一時サブディレクトリ）が絡む rename ---
        let from_scratch = from_comp.len() == 2;
        let to_scratch = to_comp.len() == 2;
        if from_scratch || to_scratch {
            debug!(share = "all", ?from_comp, ?to_comp, "AllBackend::rename (scratch)");
        }
        if from_scratch && to_comp.len() == 1 {
            let from_key = format!("{}/{}", from_comp[0], from_comp[1]);
            // 宛先が AppleDouble / `.DS_Store` の場合は本体へ fold せず ephemeral へ。
            // macOS のアトミック保存は temp サブディレクトリ内に作った `._<canonical>`
            // を最後にルートへ rename するため、ここを fold すると `._<uuid>__<filename>`
            // が実ファイルとして永続化されてしまう（これが残存の主因）。
            if is_apple_metadata(&to_comp[0]) {
                let content = self.scratch.lock().files.remove(&from_key);
                if let Some(c) = content {
                    let tk = format!("{EPHEMERAL_DIR}/{}", to_comp[0]);
                    self.scratch.lock().files.insert(tk, c);
                }
                debug!(share = "all", ?from_comp, ?to_comp, "AllBackend::rename (scratch→ephemeral)");
                return Ok(());
            }
            // 新内容（スクラッチ内ファイル）→ 本体: これが「保存の確定」。
            return self
                .fold_scratch_into_file(identity, &from_key, &to_comp[0])
                .await;
        }
        if from_scratch && to_scratch {
            // スクラッチ内（dir 間含む）の rename: バッファを移動。
            let fk = format!("{}/{}", from_comp[0], from_comp[1]);
            let tk = format!("{}/{}", to_comp[0], to_comp[1]);
            let mut sc = self.scratch.lock();
            let content = sc.files.remove(&fk).ok_or(SmbError::NotFound)?;
            sc.dirs.insert(to_comp[0].clone());
            sc.files.insert(tk, content);
            return Ok(());
        }
        if from_comp.len() == 1 && to_scratch {
            // 本体 → スクラッチ（バックアップ）。本体は変更せず内容をスクラッチへ複写。
            let tk = format!("{}/{}", to_comp[0], to_comp[1]);
            let meta = self
                .resolve_existing(&from_comp[0])
                .await?
                .ok_or(SmbError::NotFound)?;
            let content = self
                .deps
                .engine
                .read_current(meta.id)
                .await
                .map_err(io_err)?;
            let mut sc = self.scratch.lock();
            sc.dirs.insert(to_comp[0].clone());
            sc.files.insert(tk, content);
            return Ok(());
        }

        if from_comp.len() != 1 || to_comp.len() != 1 {
            return Err(SmbError::PathNotFound);
        }
        let from_meta = match self.resolve_existing(&from_comp[0]).await? {
            Some(m) => m,
            None => {
                // identity 回復後など、原本が既に正規名から退避済みで見つからない
                // 場合の後始末 rename（脇ファイル → `.smbdelete…` 等の隠し名）は
                // 冪等に成功扱いにする。実在ファイルを隠し名へ移す通常 rename は
                // 引き続き NotFound にしない（from が実在すればここを通らない）。
                if to_comp[0].starts_with('.') {
                    debug!(share = "all", from = from_comp[0].as_str(), to = to_comp[0].as_str(), "AllBackend::rename (対象なし・後始末・冪等成功)");
                    return Ok(());
                }
                return Err(SmbError::NotFound);
            }
        };
        let to_existing = self.resolve_existing(&to_comp[0]).await?;
        debug!(
            share = "all",
            from = from_comp[0].as_str(),
            to = to_comp[0].as_str(),
            from_id = %from_meta.id,
            to_id = to_existing.as_ref().map(|m| m.id.to_string()),
            "AllBackend::rename"
        );
        self.deps
            .require(
                identity,
                &yozist_auth::Target::file(from_meta.id),
                yozist_auth::PermissionMask::WRITE,
            )
            .await?;

        match to_existing {
            // rename 先が既存の別ファイル = 上書き保存（macOS 等の安全保存の
            // 最終段）。`from`（一時ファイル）の内容を `to` へ新規コミットし、
            // `from` を論理削除する。これで編集が元ファイルの履歴に積まれる。
            Some(target) if target.id != from_meta.id => {
                self.deps
                    .require(
                        identity,
                        &yozist_auth::Target::file(target.id),
                        yozist_auth::PermissionMask::WRITE,
                    )
                    .await?;
                let res = async {
                    let content = self
                        .deps
                        .engine
                        .read_current(from_meta.id)
                        .await
                        .map_err(io_err)?;
                    self.deps
                        .engine
                        .commit(
                            target.id,
                            &content,
                            yozist_core::ActorId::new(),
                            Some("smb safe-save".into()),
                        )
                        .await
                        .map_err(io_err)?;
                    let mut tmp = from_meta.clone();
                    tmp.deleted = true;
                    tmp.updated_at = time::OffsetDateTime::now_utc();
                    self.deps.meta.update_file(&tmp).await.map_err(io_err)?;
                    Ok::<_, SmbError>(())
                }
                .await;
                let id_str = target.id.to_string();
                self.deps
                    .audit_smb(
                        identity,
                        "replace_via_rename",
                        Some("file"),
                        Some(&id_str),
                        &res,
                    )
                    .await;
                res
            }
            // 同一ファイルへの rename、または存在しない名前 → 表示名の変更。
            _ => {
                let new_name = Self::display_part(&to_comp[0]);
                let res = async {
                    let mut meta = from_meta.clone();
                    meta.display_name = new_name;
                    meta.updated_at = time::OffsetDateTime::now_utc();
                    self.deps.meta.update_file(&meta).await.map_err(io_err)?;
                    Ok::<_, SmbError>(())
                }
                .await;
                let id_str = from_meta.id.to_string();
                self.deps
                    .audit_smb(identity, "rename_file", Some("file"), Some(&id_str), &res)
                    .await;
                res
            }
        }
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
                    .require(identity, &yozist_auth::Target::file(meta.id), mask)
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
                        &yozist_auth::Target::file(file_id),
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
                &yozist_auth::Target::file(file_id),
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
                            .require(identity, &yozist_auth::Target::file(meta.id), mask)
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
                &yozist_auth::Target::file(file_id),
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
                &yozist_auth::Target::file(file_id),
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
                        &yozist_auth::Target::file(file_id),
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

#[cfg(test)]
mod all_backend_tests {
    use super::*;
    use std::sync::Arc;
    use user_permission_core::Database as AuthDb;
    use yozist_auth::{Authorizer, DbAuthorizer, Permission, PermissionMask, Subject, Target};
    use yozist_core::ActorId;
    use yozist_db::{AuditLog, SharedMetaStore, SqliteMetaStore};
    use yozist_storage::FsBlobStore;
    use yozist_versioning::{CrdtRegistry, VersioningEngine};

    async fn test_deps() -> (ShareDeps, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMetaStore::open_in_memory().await.unwrap();
        let pool = store.pool().clone();
        let blob = Arc::new(FsBlobStore::new(dir.path().join("blobs")).await.unwrap());
        let meta: SharedMetaStore = Arc::new(store);
        let registry = Arc::new(CrdtRegistry::with_defaults());
        let engine = Arc::new(VersioningEngine::new(registry, blob.clone(), meta.clone()));
        let db_authz = Arc::new(DbAuthorizer::new(pool.clone()));
        let authz: Arc<dyn Authorizer> = db_authz.clone();
        let audit = Arc::new(AuditLog::new(pool.clone()));
        let auth_db = Arc::new(
            AuthDb::open_local(dir.path().join("auth.db"), Some(dir.path().join("secret")))
                .await
                .unwrap(),
        );
        let deps = ShareDeps {
            meta,
            blob,
            engine,
            authz,
            auth_db,
            acl_admin: db_authz,
            audit,
        };
        (deps, dir)
    }

    fn user_identity(name: &str) -> Identity {
        Identity::User {
            user: name.to_string(),
            domain: String::new(),
        }
    }

    fn write_opts(intent: OpenIntent) -> OpenOptions {
        OpenOptions {
            read: true,
            write: true,
            intent,
            directory: false,
            non_directory: false,
            delete_on_close: false,
        }
    }

    fn p(name: &str) -> SmbPath {
        name.parse().unwrap()
    }

    /// macOS 等の「一時ファイル作成 → write → close → rename(一時→本体)」型の
    /// 安全保存が `/all` で成立し、編集が元ファイルへコミットされること。
    /// （修正前は rename 元が UUID 形式でないため NotFound で必ず失敗していた）
    #[tokio::test]
    async fn atomic_safe_save_folds_temp_into_original() {
        let (deps, _dir) = test_deps().await;
        let alice = deps
            .auth_db
            .users()
            .create("alice", "pw", "alice", None)
            .await
            .unwrap();
        let id = user_identity("alice");

        // WebUI アップロード相当: 元ファイル作成 + オーナー ACL 付与。
        let (orig, _) = deps
            .engine
            .create_file("report.txt", b"original", ActorId::new(), None)
            .await
            .unwrap();
        deps.acl_admin
            .add_rule(&Permission {
                subject: Subject::User(alice.id),
                target: Target::file(orig.id),
                mask: PermissionMask::all(),
                allow: true,
                priority: i32::MAX,
            })
            .await
            .unwrap();
        let orig_name = format!("{}{}{}", orig.id, ID_SEP, "report.txt");
        let temp_name = "report.txt.sb-12345";

        let be = AllBackend::new(deps.clone());

        // 1) 一時ファイル作成 → 新内容を書き込み → close（create_file 永続化）。
        let h = be
            .open(&id, &p(temp_name), write_opts(OpenIntent::OverwriteOrCreate))
            .await
            .expect("一時ファイルの作成に失敗");
        h.write(0, b"edited!!").await.unwrap();
        h.close().await.unwrap();

        // 2) 「作成した名前」で開き直せる（display_name 解決）。
        let h2 = be
            .open(&id, &p(temp_name), write_opts(OpenIntent::Open))
            .await
            .expect("一時ファイルを作成名で開き直せない");

        // 3) 一時 → 本体（正規名）へ rename = 上書き保存。
        be.rename(&id, &p(temp_name), &p(&orig_name))
            .await
            .expect("rename による上書き保存が失敗");
        h2.close().await.unwrap();

        // 元ファイルの内容が編集後になっている。
        let content = deps.engine.read_current(orig.id).await.unwrap();
        assert_eq!(content, b"edited!!", "編集が元ファイルへコミットされていない");

        // 一時ファイルは一覧から消え、元ファイルは残る。
        let root = be
            .open(&id, &SmbPath::root(), OpenOptions::default())
            .await
            .unwrap();
        let names: Vec<String> = root
            .list_dir(None)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.info.name)
            .collect();
        assert!(
            !names.iter().any(|n| n.contains("sb-12345")),
            "一時ファイルが残存: {names:?}"
        );
        assert!(names.iter().any(|n| n == &orig_name), "元ファイルが消えた: {names:?}");
    }

    /// macOS が本体に付随して撒く AppleDouble（`._*`）/ `.DS_Store` が、
    /// 書き込めて読み戻せる一方で、DB/ストレージへは一切永続化されず接続に
    /// 閉じた ephemeral として扱われること（修正前は実ファイルとして残った）。
    #[tokio::test]
    async fn apple_double_files_are_ephemeral_not_persisted() {
        let (deps, _dir) = test_deps().await;
        deps.auth_db
            .users()
            .create("alice", "pw", "alice", None)
            .await
            .unwrap();
        let id = user_identity("alice");
        let be = AllBackend::new(deps.clone());

        for meta_name in ["._abcd1234__report.txt", ".DS_Store"] {
            // 作成 → 書き込み → close。
            let h = be
                .open(&id, &p(meta_name), write_opts(OpenIntent::OverwriteOrCreate))
                .await
                .unwrap_or_else(|e| panic!("{meta_name} の作成に失敗: {e:?}"));
            h.write(0, b"\x00\x05\x16\x07Mac OS X").await.unwrap();
            h.close().await.unwrap();

            // 同一接続中は開き直して読み戻せる（ephemeral に保持されている）。
            let h2 = be
                .open(&id, &p(meta_name), write_opts(OpenIntent::Open))
                .await
                .unwrap_or_else(|e| panic!("{meta_name} を開き直せない: {e:?}"));
            let read = h2.read(0, 64).await.unwrap();
            assert_eq!(&read[..], b"\x00\x05\x16\x07Mac OS X");
            h2.close().await.unwrap();
        }

        // DB には実ファイルが 1 件も作られていない。
        let persisted = deps.meta.list_files(ALL_LIST_LIMIT, 0).await.unwrap();
        assert!(
            persisted.is_empty(),
            "AppleDouble/.DS_Store が永続化された: {:?}",
            persisted.iter().map(|m| &m.display_name).collect::<Vec<_>>()
        );

        // unlink は冪等成功（macOS の後始末がエラーにならない）。
        be.unlink(&id, &p("._abcd1234__report.txt")).await.unwrap();
        be.unlink(&id, &p(".DS_Store")).await.unwrap();
    }

    /// macOS のアトミック保存（temp サブディレクトリ → ルートへ rename）で運ばれる
    /// AppleDouble が、本体へ fold されず実ファイルとして永続化されないこと。
    /// （これが「修正したのにまだ `._<uuid>__<filename>` が残る」の主因だった）
    #[tokio::test]
    async fn apple_double_via_atomic_save_temp_dir_is_not_persisted() {
        let (deps, _dir) = test_deps().await;
        let alice = deps
            .auth_db
            .users()
            .create("alice", "pw", "alice", None)
            .await
            .unwrap();
        let id = user_identity("alice");

        // 本体ファイルを用意（オーナー ACL 付き）。
        let (orig, _) = deps
            .engine
            .create_file("report.txt", b"original", ActorId::new(), None)
            .await
            .unwrap();
        deps.acl_admin
            .add_rule(&Permission {
                subject: Subject::User(alice.id),
                target: Target::file(orig.id),
                mask: PermissionMask::all(),
                allow: true,
                priority: i32::MAX,
            })
            .await
            .unwrap();
        let canonical = format!("{}{}{}", orig.id, ID_SEP, "report.txt");
        let appledouble = format!("._{canonical}");
        let tempdir = format!("{canonical}.sb-12345");

        let be = AllBackend::new(deps.clone());

        // 1) temp サブディレクトリを mkdir。
        let mkdir_opts = OpenOptions {
            read: true,
            write: true,
            intent: OpenIntent::Create,
            directory: true,
            non_directory: false,
            delete_on_close: false,
        };
        be.open(&id, &p(&tempdir), mkdir_opts).await.unwrap();

        // 2) temp 内に新内容 + AppleDouble を書き込み。
        let inner_data = format!("{tempdir}/{canonical}");
        let h = be
            .open(&id, &p(&inner_data), write_opts(OpenIntent::OverwriteOrCreate))
            .await
            .unwrap();
        h.write(0, b"edited!!").await.unwrap();
        h.close().await.unwrap();
        let inner_ad = format!("{tempdir}/{appledouble}");
        let h = be
            .open(&id, &p(&inner_ad), write_opts(OpenIntent::OverwriteOrCreate))
            .await
            .unwrap();
        h.write(0, b"\x00\x05\x16\x07Mac OS X").await.unwrap();
        h.close().await.unwrap();

        // 3) temp 内 → ルートへ rename（本体 fold ＋ AppleDouble の移動）。
        be.rename(&id, &p(&inner_data), &p(&canonical)).await.unwrap();
        be.rename(&id, &p(&inner_ad), &p(&appledouble)).await.unwrap();

        // 本体は編集後の内容で更新されている。
        let content = deps.engine.read_current(orig.id).await.unwrap();
        assert_eq!(content, b"edited!!");

        // AppleDouble は実ファイルとして永続化されていない（本体 1 件のみ）。
        let persisted = deps.meta.list_files(ALL_LIST_LIMIT, 0).await.unwrap();
        let names: Vec<&String> = persisted.iter().map(|m| &m.display_name).collect();
        assert_eq!(persisted.len(), 1, "余計なファイルが永続化された: {names:?}");
        assert_eq!(persisted[0].display_name, "report.txt", "本体名が変わった: {names:?}");
    }

    /// インプレース上書き（truncate→write→close）は従来どおりコミットされる。
    #[tokio::test]
    async fn inplace_overwrite_commits() {
        let (deps, _dir) = test_deps().await;
        deps.auth_db
            .users()
            .create("bob", "pw", "bob", None)
            .await
            .unwrap();
        let id = user_identity("bob");
        let (orig, _) = deps
            .engine
            .create_file("note.txt", b"v1", ActorId::new(), None)
            .await
            .unwrap();
        let orig_name = format!("{}{}{}", orig.id, ID_SEP, "note.txt");
        let be = AllBackend::new(deps.clone());

        let h = be
            .open(&id, &p(&orig_name), write_opts(OpenIntent::OverwriteOrCreate))
            .await
            .expect("既存ファイルを上書きオープンできない");
        h.write(0, b"v2-new").await.unwrap();
        h.close().await.unwrap();

        let content = deps.engine.read_current(orig.id).await.unwrap();
        assert_eq!(content, b"v2-new");
    }

    /// クライアントが一覧で見えた正規名（`<uuid>__name`）で開けること（回帰防止）。
    #[tokio::test]
    async fn open_by_canonical_name() {
        let (deps, _dir) = test_deps().await;
        deps.auth_db
            .users()
            .create("carol", "pw", "carol", None)
            .await
            .unwrap();
        let id = user_identity("carol");
        let (orig, _) = deps
            .engine
            .create_file("a.txt", b"hi", ActorId::new(), None)
            .await
            .unwrap();
        let canonical = format!("{}{}{}", orig.id, ID_SEP, "a.txt");
        let be = AllBackend::new(deps.clone());
        let h = be
            .open(&id, &p(&canonical), write_opts(OpenIntent::Open))
            .await
            .expect("正規名で開けない");
        let got = h.read(0, 64).await.unwrap();
        assert_eq!(&got[..], b"hi");
        h.close().await.unwrap();
    }

    /// 開いているファイルの stat は呼ぶ度に同じ時刻を返し、ストアの
    /// created_at/updated_at と一致する。`now()` を返すと macOS プレビュー
    /// (NSDocument) が「他アプリが変更した」と誤検知して保存できなくなる。
    #[tokio::test]
    async fn open_file_stat_timestamps_are_stable_and_from_store() {
        let (deps, _dir) = test_deps().await;
        deps.auth_db
            .users()
            .create("dave", "pw", "dave", None)
            .await
            .unwrap();
        let id = user_identity("dave");
        let (orig, _) = deps
            .engine
            .create_file("photo.jpg", b"\xff\xd8\xff\xe0jpegbytes", ActorId::new(), None)
            .await
            .unwrap();
        let name = format!("{}{}{}", orig.id, ID_SEP, "photo.jpg");
        let be = AllBackend::new(deps.clone());

        let h = be
            .open(&id, &p(&name), write_opts(OpenIntent::Open))
            .await
            .unwrap();
        let s1 = h.stat().await.unwrap();
        let s2 = h.stat().await.unwrap();

        let want_mtime = crate::handle::offset_dt_to_filetime(orig.updated_at);
        let want_ctime = crate::handle::offset_dt_to_filetime(orig.created_at);
        assert_eq!(s1.last_write_time, want_mtime, "報告 mtime がストアと不一致");
        assert_eq!(
            s1.last_write_time, s2.last_write_time,
            "stat の度に mtime が変化している"
        );
        assert_eq!(s1.creation_time, want_ctime);
        assert_eq!(s1.change_time, want_mtime);
        h.close().await.unwrap();
    }

    /// Shift-JIS 等のテキストは「一覧(QUERY_DIRECTORY)の size」と「open 時の
    /// stat の size」が一致しなければならない。不一致だと macOS が folder 上の
    /// サイズと開いたファイルのサイズを reconcile できず、open→close→再list を
    /// 無限ループ（スピナー）してしまう。
    #[tokio::test]
    async fn charset_text_listing_and_open_size_match() {
        let (deps, _dir) = test_deps().await;
        deps.auth_db
            .users()
            .create("eve", "pw", "eve", None)
            .await
            .unwrap();
        let id = user_identity("eve");
        // Shift-JIS の日本語テキスト（detect_charset が Shift_JIS を返す程度の長さ）。
        let text = "これはテスト用の日本語テキストです。エンコーディング保持の確認に使います。";
        let sjis = yozist_versioning::encode_text(text, "Shift_JIS");
        let (meta, _) = deps
            .engine
            .create_file("ja.txt", &sjis, ActorId::new(), None)
            .await
            .unwrap();
        assert_eq!(
            meta.charset.as_deref(),
            Some("Shift_JIS"),
            "前提: charset が Shift_JIS として検出される"
        );
        let name = format!("{}{}{}", meta.id, ID_SEP, "ja.txt");
        let be = AllBackend::new(deps.clone());

        // 一覧が報告するサイズ。
        let root = be
            .open(&id, &SmbPath::root(), OpenOptions::default())
            .await
            .unwrap();
        let listing_size = root
            .list_dir(None)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.info.name == name)
            .expect("一覧に出る")
            .info
            .end_of_file;
        // open 時の stat が報告するサイズ。
        let h = be
            .open(&id, &p(&name), write_opts(OpenIntent::Open))
            .await
            .unwrap();
        let open_size = h.stat().await.unwrap().end_of_file;
        let read_len = h.read(0, 100_000).await.unwrap().len() as u64;
        h.close().await.unwrap();

        assert_eq!(
            listing_size, open_size,
            "一覧サイズ({listing_size})と open サイズ({open_size})が不一致 → macOS が reconcile ループする"
        );
        assert_eq!(
            open_size, read_len,
            "stat サイズ({open_size})と実際に読める長さ({read_len})が不一致"
        );
    }

    /// 仮想ディレクトリの stat は呼ぶ度に同じ時刻を返さねばならない（配下の
    /// 最大 mtime 由来）。now() を返すと macOS が「ディレクトリが変化し続けて
    /// いる」と誤認し、列挙(QUERY_DIRECTORY)を無限ループ（スピナー）する。
    #[tokio::test]
    async fn dir_stat_timestamps_are_stable() {
        let mk = |t: u64| DirEntry {
            info: FileInfo {
                name: "x".into(),
                end_of_file: 0,
                allocation_size: 0,
                creation_time: t,
                last_access_time: t,
                last_write_time: t,
                change_time: t,
                is_directory: false,
                file_index: 0,
            },
        };
        let dir = YozistDirHandle::new("all", vec![mk(100), mk(300), mk(200)]);
        let s1 = dir.stat().await.unwrap();
        let s2 = dir.stat().await.unwrap();
        assert_eq!(s1.last_write_time, 300, "配下の最大 mtime を返す");
        assert_eq!(
            s1.last_write_time, s2.last_write_time,
            "stat の度に変わってはいけない"
        );
        assert_eq!(s1.creation_time, 100, "配下の最古 ctime");

        // 空ディレクトリは固定フォールバック（0 でも now でもない安定値）。
        let empty = YozistDirHandle::new("all", vec![]);
        let e1 = empty.stat().await.unwrap();
        let e2 = empty.stat().await.unwrap();
        assert_eq!(e1.last_write_time, e2.last_write_time);
        assert_ne!(e1.last_write_time, 0);
    }

    /// macOS の保存検証（write → flush → 別ハンドルで再オープンして read）を再現。
    /// flush 時にコミットしないと、再オープンが旧内容を返し「保存できていない」と
    /// 判断されて延々リトライ（クルクル）する。flush 後は新内容が読めること。
    #[tokio::test]
    async fn flush_persists_so_reopen_sees_new_content() {
        let (deps, _dir) = test_deps().await;
        deps.auth_db
            .users()
            .create("gina", "pw", "gina", None)
            .await
            .unwrap();
        let id = user_identity("gina");
        let (orig, _) = deps
            .engine
            .create_file("doc.txt", b"old content here", ActorId::new(), None)
            .await
            .unwrap();
        let name = format!("{}{}{}", orig.id, ID_SEP, "doc.txt");
        let be = AllBackend::new(deps.clone());

        // 書き込みハンドル: 上書きして flush（close せず開いたまま）。
        let hw = be
            .open(&id, &p(&name), write_opts(OpenIntent::Open))
            .await
            .unwrap();
        hw.truncate(0).await.unwrap();
        hw.write(0, b"NEW!").await.unwrap();
        hw.flush().await.unwrap();

        // 別ハンドルで再オープンして検証 read（hw はまだ開いている）。
        let hr = be
            .open(&id, &p(&name), write_opts(OpenIntent::Open))
            .await
            .unwrap();
        let got = hr.read(0, 1000).await.unwrap();
        assert_eq!(
            &got[..],
            b"NEW!",
            "flush 後の再オープンが新内容を返さない＝保存未完了に見える"
        );
        hr.close().await.unwrap();
        hw.close().await.unwrap();

        // ストアにも反映されている。
        let current = deps.engine.read_current(orig.id).await.unwrap();
        assert_eq!(current, b"NEW!");
    }

    /// QUERY_DIRECTORY は検索パターンで絞り込む。特に macOS の保存は
    /// 一時ファイル名 `<本体>.sb-<id>-<連番>` の存在をパターン検索で確認し、
    /// 空かない名前を temp として使う。絞らず全件返すと、存在しない temp 名まで
    /// 「存在する」と返り、Preview が連番で無限に探し続けて保存が永久にスピンする。
    #[tokio::test]
    async fn dir_list_filters_by_search_pattern() {
        let mk = |name: &str| DirEntry {
            info: FileInfo {
                name: name.to_string(),
                end_of_file: 0,
                allocation_size: 0,
                creation_time: 1,
                last_access_time: 1,
                last_write_time: 1,
                change_time: 1,
                is_directory: false,
                file_index: 0,
            },
        };
        let dir = YozistDirHandle::new(
            "all",
            vec![mk("019e884e__DSC06610.jpg"), mk("019e884e__note.txt")],
        );

        // None / "*" / "*.*" は全件。
        assert_eq!(dir.list_dir(None).await.unwrap().len(), 2);
        assert_eq!(dir.list_dir(Some("*")).await.unwrap().len(), 2);
        assert_eq!(dir.list_dir(Some("*.*")).await.unwrap().len(), 2);
        // 完全一致はその1件だけ。
        assert_eq!(
            dir.list_dir(Some("019e884e__DSC06610.jpg")).await.unwrap().len(),
            1
        );
        // 存在しない一時ファイル名は空でなければならない（保存ループ防止）。
        assert!(dir
            .list_dir(Some("019e884e__DSC06610.jpg.sb-df292883-004Uvz"))
            .await
            .unwrap()
            .is_empty());
        // ワイルドカード。
        assert_eq!(dir.list_dir(Some("*.jpg")).await.unwrap().len(), 1);
        assert_eq!(dir.list_dir(Some("*.txt")).await.unwrap().len(), 1);
    }

    /// macOS のアトミック保存（temp サブディレクトリ）を再現:
    /// mkdir `<本体>.sb-…` → その中に新内容を write → rename(temp内→本体) で
    /// 本体へ fold → rmdir。本体に新内容がバイト等価で入ること。
    #[tokio::test]
    async fn scratch_subdir_atomic_save_folds_into_canonical() {
        let (deps, _dir) = test_deps().await;
        let hana = deps
            .auth_db
            .users()
            .create("hana", "pw", "hana", None)
            .await
            .unwrap();
        let id = user_identity("hana");
        let (orig, _) = deps
            .engine
            .create_file("pic.jpg", b"OLD-IMAGE-BYTES", ActorId::new(), None)
            .await
            .unwrap();
        deps.acl_admin
            .add_rule(&Permission {
                subject: Subject::User(hana.id),
                target: Target::file(orig.id),
                mask: PermissionMask::all(),
                allow: true,
                priority: i32::MAX,
            })
            .await
            .unwrap();
        let canonical = format!("{}{}{}", orig.id, ID_SEP, "pic.jpg");
        let tempdir = format!("{canonical}.sb-abc123-XYZ");
        let inner = format!("{tempdir}/{canonical}");
        let be = AllBackend::new(deps.clone());

        // 1) mkdir <本体>.sb-…
        let mkdir_opts = OpenOptions {
            read: true,
            write: false,
            intent: OpenIntent::Create,
            directory: true,
            non_directory: false,
            delete_on_close: false,
        };
        be.open(&id, &p(&tempdir), mkdir_opts)
            .await
            .expect("temp サブディレクトリの mkdir に失敗")
            .close()
            .await
            .unwrap();

        // 2) その中に新内容を書く
        let fh = be
            .open(&id, &p(&inner), write_opts(OpenIntent::OverwriteOrCreate))
            .await
            .expect("temp 内ファイルの作成に失敗");
        fh.write(0, b"BRAND-NEW-IMAGE!").await.unwrap();
        fh.close().await.unwrap();

        // 3) rename(temp内ファイル → 本体)＝保存の確定
        be.rename(&id, &p(&inner), &p(&canonical))
            .await
            .expect("temp→本体の rename(fold) に失敗");

        // 4) 本体に新内容がバイト等価で入っている
        let content = deps.engine.read_current(orig.id).await.unwrap();
        assert_eq!(content, b"BRAND-NEW-IMAGE!", "本体に新内容が反映されていない");

        // 5) rmdir
        be.unlink(&id, &p(&tempdir)).await.unwrap();
    }

    /// mkdir した temp サブディレクトリは、ルート（/all）の列挙に現れなければ
    /// ならない。現れないと、macOS が「作ったはずの temp が消えた」と誤認して
    /// 保存を中断する（log13 で実証した「保存できませんでした」の真因）。
    #[tokio::test]
    async fn root_listing_includes_scratch_dir() {
        let (deps, _dir) = test_deps().await;
        let id = user_identity("anon");
        let be = AllBackend::new(deps.clone());
        let tempdir = "019e884e__pic.jpg.sb-abc123-XYZ";
        let mkdir_opts = OpenOptions {
            read: true,
            write: false,
            intent: OpenIntent::Create,
            directory: true,
            non_directory: false,
            delete_on_close: false,
        };
        be.open(&id, &p(tempdir), mkdir_opts)
            .await
            .unwrap()
            .close()
            .await
            .unwrap();

        let root = be.open(&id, &SmbPath::root(), OpenOptions::default()).await.unwrap();
        let entries = root.list_dir(None).await.unwrap();
        let found = entries.iter().find(|e| e.info.name == tempdir);
        assert!(found.is_some(), "ルート列挙に temp サブディレクトリが含まれていない");
        assert!(found.unwrap().info.is_directory, "temp はディレクトリとして列挙されるべき");
    }

    /// rename(temp→本体) を出さずに rmdir で temp を畳むクライアント変種でも、
    /// temp 内に書かれた既存本体宛ての内容は破棄前に本体へ救済される。
    #[tokio::test]
    async fn scratch_rmdir_salvages_unrenamed_content() {
        let (deps, _dir) = test_deps().await;
        let hana = deps
            .auth_db
            .users()
            .create("hana", "pw", "hana", None)
            .await
            .unwrap();
        let id = user_identity("hana");
        let (orig, _) = deps
            .engine
            .create_file("pic.jpg", b"OLD-IMAGE-BYTES", ActorId::new(), None)
            .await
            .unwrap();
        deps.acl_admin
            .add_rule(&Permission {
                subject: Subject::User(hana.id),
                target: Target::file(orig.id),
                mask: PermissionMask::all(),
                allow: true,
                priority: i32::MAX,
            })
            .await
            .unwrap();
        let canonical = format!("{}{}{}", orig.id, ID_SEP, "pic.jpg");
        let tempdir = format!("{canonical}.sb-abc123-XYZ");
        let inner = format!("{tempdir}/{canonical}");
        let be = AllBackend::new(deps.clone());

        let mkdir_opts = OpenOptions {
            read: true,
            write: false,
            intent: OpenIntent::Create,
            directory: true,
            non_directory: false,
            delete_on_close: false,
        };
        be.open(&id, &p(&tempdir), mkdir_opts).await.unwrap().close().await.unwrap();
        let fh = be
            .open(&id, &p(&inner), write_opts(OpenIntent::OverwriteOrCreate))
            .await
            .unwrap();
        fh.write(0, b"SALVAGED-IMAGE!").await.unwrap();
        fh.close().await.unwrap();

        // rename を出さずに rmdir → 救済で本体へ反映される。
        be.unlink(&id, &p(&tempdir)).await.unwrap();

        let content = deps.engine.read_current(orig.id).await.unwrap();
        assert_eq!(content, b"SALVAGED-IMAGE!", "rmdir 救済で本体に反映されていない");
    }

    /// macOS の安全保存スワップ全体を再現:
    ///   1) 原本(正規名) → 脇名へ rename（退避）
    ///   2) temp内の新内容 → 正規名へ rename（fold）
    ///   3) 脇名 → `.smbdelete…` へ rename → 削除
    /// 期待: 新内容は**元ファイルの identity に commit され履歴が保たれる**。
    /// 新規ファイルが量産されず、最終的に元ファイルが生き残ること（log14 で
    /// 確認した「新規作成・履歴喪失」の回帰防止）。
    #[tokio::test]
    async fn macos_safe_save_swap_preserves_identity_and_history() {
        let (deps, _dir) = test_deps().await;
        let hana = deps
            .auth_db
            .users()
            .create("hana", "pw", "hana", None)
            .await
            .unwrap();
        let id = user_identity("hana");
        let (orig, _) = deps
            .engine
            .create_file("pic.jpg", b"OLD-IMAGE-BYTES", ActorId::new(), None)
            .await
            .unwrap();
        deps.acl_admin
            .add_rule(&Permission {
                subject: Subject::User(hana.id),
                target: Target::file(orig.id),
                mask: PermissionMask::all(),
                allow: true,
                priority: i32::MAX,
            })
            .await
            .unwrap();
        let commits_before = deps.meta.list_commits(&orig.id).await.unwrap().len();

        let canonical = format!("{}{}{}", orig.id, ID_SEP, "pic.jpg");
        let aside = format!("{canonical}.sb-4d1de4c2-Kx0EtD");
        let tempdir = format!("{canonical}.sb-4d1de4c2-UsnIgR");
        let inner = format!("{tempdir}/{canonical}");
        let smbdelete = ".smbdeleteAAA02a34.4";
        let be = AllBackend::new(deps.clone());

        let mkdir_opts = OpenOptions {
            read: true,
            write: false,
            intent: OpenIntent::Create,
            directory: true,
            non_directory: false,
            delete_on_close: false,
        };
        // temp フォルダ + 新内容
        be.open(&id, &p(&tempdir), mkdir_opts).await.unwrap().close().await.unwrap();
        let fh = be
            .open(&id, &p(&inner), write_opts(OpenIntent::OverwriteOrCreate))
            .await
            .unwrap();
        fh.write(0, b"EDITED-IMAGE-CONTENT").await.unwrap();
        fh.close().await.unwrap();

        // 1) 原本を脇名へ退避
        be.rename(&id, &p(&canonical), &p(&aside)).await.unwrap();
        // 2) temp内 → 正規名（fold）: 埋め込み UUID で原本を回復して commit
        be.rename(&id, &p(&inner), &p(&canonical)).await.unwrap();
        // 3) 脇名 → .smbdelete → 削除（後始末は冪等成功）
        be.rename(&id, &p(&aside), &p(smbdelete)).await.unwrap();
        be.unlink(&id, &p(smbdelete)).await.unwrap();

        // 元 identity に新内容が入っており、生きている。
        let meta = deps.meta.get_file(&orig.id).await.unwrap().expect("元ファイルが消えた");
        assert!(!meta.deleted, "元ファイルが削除されてしまった");
        assert_eq!(meta.display_name, "pic.jpg", "正規名へ戻っていない");
        let content = deps.engine.read_current(orig.id).await.unwrap();
        assert_eq!(content, b"EDITED-IMAGE-CONTENT", "新内容が元 identity に反映されていない");

        // 履歴が積まれている（新規ファイル化していない）。
        let commits_after = deps.meta.list_commits(&orig.id).await.unwrap().len();
        assert!(commits_after > commits_before, "履歴(commit)が増えていない＝新規作成扱いになっている");

        // pic.jpg という表示名の生存ファイルは元の1件だけ（量産されていない）。
        let live_pics = deps
            .meta
            .list_files(1000, 0)
            .await
            .unwrap()
            .into_iter()
            .filter(|m| !m.deleted && m.display_name == "pic.jpg")
            .count();
        assert_eq!(live_pics, 1, "pic.jpg が複数生成されている（新規作成の量産）");
    }
}
