//! yozist-smb — SMB ネットワーク層。タグ／シリーズ中心の仮想 FS を提供する。
//!
//! # 設計
//! - 採用クレート: [`smb-server`](https://github.com/paltaio/rust-smb-server) v0.4 系
//! - 各 share（all / tags / series / recent）ごとに `ShareBackend` 実装を持つ
//! - すべての操作は `yozist-versioning` / `yozist-db` の公開 API 経由
//!
//! # Share 一覧
//! 公開 share は全仮想ビューへの単一エントリ `yozist`（[`HubBackend`]）のみ。
//! 組込みビューと条件付きパスはすべてその配下に現れる:
//!
//! | パス | 内容 |
//! |------|------|
//! | `yozist\` | ルート。組込みビュー (all / tags / series / filters) が並ぶ |
//! | `yozist\all\` | 全ファイルをフラット |
//! | `yozist\tags\…` | 階層パス = タグの AND 条件 |
//! | `yozist\series\…` | 配下に `NNNN__name` 形式で順序付きメンバー |
//! | `yozist\filters\` | 全フィルター（任意名）が並ぶ |
//! | `yozist\filters\<任意の名前>\` | フィルターの結果へアクセス |
//!
//! # TODO
//! - [ ] RecentBackend の本実装
//! - [ ] SMB Change Notify による他クライアントへの即時反映
//! - [ ] truncate / set_times の完全対応
//! - [ ] `smb://host`（share 名なし）での share 列挙（srvsvc NetrShareEnum）。
//!       現状は `yozist` ハブ share へ接続して全ビューを辿る運用で代替している。

use smb_server::{Share, SmbServer};
use sqlx::SqlitePool;
use std::sync::Arc;
use user_permission_core::Database as AuthDb;
use yozist_auth::{AuthContext, Authorizer, DbAuthorizer};
use yozist_db::{AuditRecord, SharedAuditLog, SharedMetaStore};
use yozist_storage::SharedBlobStore;
use yozist_versioning::VersioningEngine;

pub mod backends;
pub mod credentials;
pub mod handle;
pub use backends::{
    AllBackend, HubBackend, FiltersBackend, RecentBackend, SeriesBackend, TagsBackend,
};
pub use credentials::{SmbCredentialStore, SmbCredentialSync};

/// 各 share 実装が共有する依存。
#[derive(Clone)]
pub struct ShareDeps {
    pub meta: SharedMetaStore,
    pub blob: SharedBlobStore,
    pub engine: Arc<VersioningEngine>,
    pub authz: Arc<dyn Authorizer>,
    pub auth_db: Arc<AuthDb>,
    /// ACL ルール CRUD 用の具象参照（新規ファイル作成時のオーナー ACL 発行に使用）。
    pub acl_admin: Arc<DbAuthorizer>,
    /// 監査ログ（REST/SMB 共通）。SMB 経路は actor_label を `smb:<user>` で記録。
    pub audit: SharedAuditLog,
}

impl ShareDeps {
    /// SMB の `Identity` を yozist の `AuthContext` に解決する。
    pub async fn identity_to_context(
        &self,
        identity: &smb_server::Identity,
    ) -> AuthContext {
        match identity {
            smb_server::Identity::Anonymous => AuthContext::Anonymous,
            smb_server::Identity::User { user, .. } => {
                if let Ok(Some(u)) = self.auth_db.users().get_by_username(user, None).await {
                    let groups = self
                        .auth_db
                        .groups()
                        .get_user_groups(u.id, None)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|g| g.id)
                        .collect();
                    AuthContext::User { user: u, groups }
                } else {
                    AuthContext::Anonymous
                }
            }
        }
    }

    /// SMB 操作を audit に記録する。`actor_label` は `smb:<user>` 形式。
    pub async fn audit_smb<R, E>(
        &self,
        identity: &smb_server::Identity,
        action: &str,
        target_type: Option<&str>,
        target_ref: Option<&str>,
        result: &Result<R, E>,
    ) where
        E: std::fmt::Display,
    {
        let (actor_id, label_owned) = match identity {
            smb_server::Identity::Anonymous => (None, "smb:anonymous".to_string()),
            smb_server::Identity::User { user, .. } => {
                let ctx = self.identity_to_context(identity).await;
                let id = if let AuthContext::User { user: u, .. } = &ctx {
                    Some(u.id.to_string())
                } else {
                    None
                };
                (id, format!("smb:{}", user))
            }
        };
        let result_str = match result {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("error: {e}"),
        };
        if let Err(e) = self
            .audit
            .record(&AuditRecord {
                actor_id: actor_id.as_deref(),
                actor_label: Some(&label_owned),
                action,
                target_type,
                target_ref,
                metadata_json: None,
                result: &result_str,
            })
            .await
        {
            tracing::warn!(error = %e, action, "SMB audit write failed");
        }
    }

    /// 共通の権限チェック。失敗時は `SmbError::AccessDenied` を返す。
    pub async fn require(
        &self,
        identity: &smb_server::Identity,
        target: &yozist_auth::Target,
        mask: yozist_auth::PermissionMask,
    ) -> smb_server::SmbResult<()> {
        let ctx = self.identity_to_context(identity).await;
        match self.authz.check(&ctx, target, mask).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(smb_server::SmbError::AccessDenied),
            Err(e) => Err(smb_server::SmbError::Io(std::io::Error::other(
                e.to_string(),
            ))),
        }
    }
}

/// バックエンドが受け取る要求コンテキスト。`AuthContext` を引き回す。
#[derive(Clone)]
pub struct RequestCtx {
    pub auth: AuthContext,
}

/// 起動設定。
pub struct SmbConfig {
    pub listen: std::net::SocketAddr,
}

/// 公開する固定 share 名の一覧（すべて `AuthenticatedOnly`）。
/// 公開 share は全仮想ビューへの単一エントリ `yozist` のみ。組込みビューと条件付き
/// パスはすべて `yozist\<...>\`（例: `yozist\all\`、`yozist\<任意の名前>\`）に現れる。
const SHARE_NAMES: [&str; 1] = ["yozist"];

/// ビルド済み（未起動）の SMB サーバーと、REST 認証経路へ渡す資格情報シンク。
pub struct BuiltSmb {
    server: SmbServer,
    sync: Arc<SmbCredentialSync>,
}

impl BuiltSmb {
    /// REST 認証ハンドラ（register / login / change_password）が利用する資格情報シンク。
    pub fn credential_sink(&self) -> Arc<dyn yozist_auth::SmbCredentialSink> {
        self.sync.clone()
    }

    /// listen アドレスへ bind し、シャットダウンまでサーブする。
    pub async fn serve(self) -> Result<(), SmbError> {
        let BuiltSmb { server, .. } = self;
        let bound = server
            .bind()
            .await
            .map_err(|e| SmbError::Bind(e.to_string()))?;
        tracing::info!("SMB server listening on {bound}");
        server
            .serve()
            .await
            .map_err(|e| SmbError::Serve(e.to_string()))?;
        Ok(())
    }
}

/// SMB サーバーを構築する。
///
/// share はすべて `AuthenticatedOnly`。ユーザー認証は user-permission と統合され、
/// 平文パスワードが REST 認証経路を通過した時に NT ハッシュが `pool` の
/// `smb_credentials` テーブルへ保存される（[`SmbCredentialSink`](yozist_auth::SmbCredentialSink)）。
/// 構築時に永続化済みの資格情報を稼働中テーブルへ復元するため、再起動後も
/// 既存ユーザーはログイン無しで接続できる。
pub async fn build(cfg: SmbConfig, deps: ShareDeps, pool: SqlitePool) -> Result<BuiltSmb, SmbError> {
    // 公開する share は `yozist` ハブのみ。組込みビュー（all / tags / series /
    // filters）と各フィルター（任意名）はすべて `yozist\<...>\` の配下に現れる。
    let server = SmbServer::builder()
        .listen(cfg.listen)
        .share(Share::new("yozist", HubBackend::new(deps.clone())))
        .build()
        .map_err(|e| SmbError::Build(e.to_string()))?;

    let shares = SHARE_NAMES.iter().map(|s| s.to_string()).collect();
    let sync = Arc::new(SmbCredentialSync::new(
        SmbCredentialStore::new(pool),
        server.config_handle(),
        shares,
    ));
    sync.restore().await;

    Ok(BuiltSmb { server, sync })
}

#[derive(Debug, thiserror::Error)]
pub enum SmbError {
    #[error("build error: {0}")]
    Build(String),
    #[error("bind error: {0}")]
    Bind(String),
    #[error("serve error: {0}")]
    Serve(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
