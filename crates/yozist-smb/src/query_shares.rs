//! 保存クエリ（条件付き仮想ビュー）を「任意名のトップレベル SMB share」として
//! 稼働中サーバへ動的に増減させるレジストリ。
//!
//! `smb://host/<クエリ名>/` で各クエリ結果へ直接アクセスできるよう、REST から
//! クエリを作成・改名・削除したタイミングで本レジストリ経由で share を add/remove
//! する。share は `AuthenticatedOnly`（読取専用）で、既知の全 SMB ユーザーへ Read を
//! 付与する。新規ユーザー側の付与は [`SmbCredentialSync`](crate::credentials) が
//! `config.share_names()` を引いて行うため、両方向で整合する。

use async_trait::async_trait;
use smb_server::{Access, ConfigHandle, Share};
use yozist_auth::SmbShareController;
use yozist_core::SavedQuery;

use crate::backends::QueryShareBackend;
use crate::credentials::SmbCredentialStore;
use crate::ShareDeps;

/// 動的なクエリ share の登録・撤去を司る。
pub struct QueryShareRegistry {
    config: ConfigHandle,
    deps: ShareDeps,
    creds: SmbCredentialStore,
}

impl QueryShareRegistry {
    pub fn new(config: ConfigHandle, deps: ShareDeps, creds: SmbCredentialStore) -> Self {
        Self {
            config,
            deps,
            creds,
        }
    }

    /// 起動時に DB の全保存クエリを share として復元する。
    pub async fn restore(&self) {
        match self.deps.meta.list_saved_queries().await {
            Ok(queries) => {
                let n = queries.len();
                for q in queries {
                    self.register(&q).await;
                }
                if n > 0 {
                    tracing::info!(count = n, "restored saved-query SMB shares");
                }
            }
            Err(e) => tracing::warn!(error = %e, "loading saved queries for SMB shares failed"),
        }
    }
}

#[async_trait]
impl SmbShareController for QueryShareRegistry {
    async fn register(&self, query: &SavedQuery) {
        let name = query.name.clone();
        // 同名 share があれば置換する（条件のみ更新時にも安全に呼べる）。
        let _ = self.config.remove_share(&name).await;
        let backend = QueryShareBackend::new(self.deps.clone(), name.clone());
        if let Err(e) = self.config.add_share(Share::new(name.clone(), backend)).await {
            tracing::warn!(share = %name, error = %e, "registering query share failed");
            return;
        }
        // AuthenticatedOnly のため既知の全 SMB ユーザーへ Read を付与する。
        match self.creds.list_all().await {
            Ok(users) => {
                for (user, _) in users {
                    if let Err(e) = self
                        .config
                        .grant_share_user(&name, &user, Access::Read)
                        .await
                    {
                        tracing::warn!(share = %name, %user, error = %e, "granting query share access failed");
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "listing SMB users for query share grant failed"),
        }
        tracing::info!(share = %name, "registered saved-query SMB share");
    }

    async fn unregister(&self, name: &str) {
        match self.config.remove_share(name).await {
            Ok(()) => tracing::info!(share = %name, "unregistered saved-query SMB share"),
            Err(e) => {
                tracing::debug!(share = %name, error = %e, "removing query share (likely absent)")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AllBackend;
    use smb_server::{Share, SmbServer};
    use std::sync::Arc;
    use user_permission_core::Database as AuthDb;
    use yozist_auth::{Authorizer, DbAuthorizer};
    use yozist_db::{AuditLog, SharedMetaStore, SqliteMetaStore};
    use yozist_storage::FsBlobStore;
    use yozist_versioning::{CrdtRegistry, VersioningEngine};

    async fn deps_and_pool() -> (ShareDeps, sqlx::SqlitePool, tempfile::TempDir) {
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
        (deps, pool, dir)
    }

    fn server(deps: ShareDeps) -> SmbServer {
        SmbServer::builder()
            .listen("127.0.0.1:0".parse().unwrap())
            .share(Share::new("all", AllBackend::new(deps)))
            .build()
            .unwrap()
    }

    async fn seed(deps: &ShareDeps, name: &str) -> SavedQuery {
        let q = SavedQuery {
            id: yozist_core::SavedQueryId::new(),
            name: name.to_string(),
            query: yozist_core::QueryDef::default(),
            description: None,
            created_by: None,
            created_at: time::OffsetDateTime::now_utc(),
            expires_at: None,
        };
        let id = deps.meta.upsert_saved_query(&q).await.unwrap();
        SavedQuery { id, ..q }
    }

    /// register でクエリ名の share が増え、unregister で消える。
    #[tokio::test]
    async fn register_and_unregister_share() {
        let (deps, pool, _dir) = deps_and_pool().await;
        let srv = server(deps.clone());
        let config = srv.config_handle();
        let reg = QueryShareRegistry::new(config.clone(), deps.clone(), SmbCredentialStore::new(pool));

        let q = seed(&deps, "プロジェクトX").await;
        reg.register(&q).await;
        assert!(config.share_names().await.iter().any(|n| n == "プロジェクトX"));

        reg.unregister("プロジェクトX").await;
        assert!(!config.share_names().await.iter().any(|n| n == "プロジェクトX"));
    }

    /// register は既知の SMB ユーザーへ Read を付与する。
    #[tokio::test]
    async fn register_grants_existing_users() {
        let (deps, pool, _dir) = deps_and_pool().await;
        let srv = server(deps.clone());
        let config = srv.config_handle();
        let creds = SmbCredentialStore::new(pool);

        // 既知ユーザーを稼働中テーブルと永続テーブルの双方へ用意。
        let nt = smb_server::nt_hash("pw");
        creds.upsert("alice", &nt).await.unwrap();
        config
            .add_user_creds("alice", smb_server::UserCreds::from_nt_hash(nt))
            .await
            .unwrap();

        let reg = QueryShareRegistry::new(config.clone(), deps.clone(), creds);
        let q = seed(&deps, "共有ビュー").await;
        reg.register(&q).await;

        let share = srv.state().find_share("共有ビュー").await.expect("share 登録済み");
        let acl = share.acl.read().await;
        assert_eq!(acl.users.get("alice"), Some(&smb_server::Access::Read));
    }

    /// restore は DB の全クエリを share 化する。
    #[tokio::test]
    async fn restore_registers_all() {
        let (deps, pool, _dir) = deps_and_pool().await;
        seed(&deps, "q1").await;
        seed(&deps, "q2").await;
        let srv = server(deps.clone());
        let reg = QueryShareRegistry::new(srv.config_handle(), deps.clone(), SmbCredentialStore::new(pool));
        reg.restore().await;
        let names = srv.config_handle().share_names().await;
        assert!(names.iter().any(|n| n == "q1"));
        assert!(names.iter().any(|n| n == "q2"));
    }
}
