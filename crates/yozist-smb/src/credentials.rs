//! SMB(NTLM) 資格情報の永続化と同期。
//!
//! NTLM はサーバ側に NT ハッシュ (`MD4(UTF-16LE(password))`) を要求する。
//! user-permission の Argon2id ハッシュからは導出できないため、平文パスワードを
//! 観測できる REST 認証経路 (register / login / change_password) で NT ハッシュを
//! 導出し、`smb_credentials` テーブルへ保存する。サーバ起動時に [`SmbCredentialSync::restore`]
//! で稼働中の SMB ユーザーテーブルへ復元し、再起動後もログイン無しで接続できる。

use async_trait::async_trait;
use smb_server::{Access, ConfigHandle, UserCreds};
use sqlx::{Row, SqlitePool};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use yozist_auth::SmbCredentialSink;

/// `smb_credentials` テーブルへの NT ハッシュ CRUD。
#[derive(Clone)]
pub struct SmbCredentialStore {
    pool: SqlitePool,
}

impl SmbCredentialStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn upsert(&self, username: &str, nt_hash: &[u8; 16]) -> Result<(), sqlx::Error> {
        let now = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        sqlx::query(
            "INSERT INTO smb_credentials (username, nt_hash, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(username) DO UPDATE SET nt_hash = excluded.nt_hash, \
             updated_at = excluded.updated_at",
        )
        .bind(username)
        .bind(&nt_hash[..])
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete(&self, username: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM smb_credentials WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_all(&self) -> Result<Vec<(String, [u8; 16])>, sqlx::Error> {
        let rows = sqlx::query("SELECT username, nt_hash FROM smb_credentials")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let username: String = row.try_get("username")?;
            let blob: Vec<u8> = row.try_get("nt_hash")?;
            match <[u8; 16]>::try_from(blob.as_slice()) {
                Ok(h) => out.push((username, h)),
                Err(_) => tracing::warn!(
                    user = %username,
                    len = blob.len(),
                    "smb_credentials.nt_hash has unexpected length; skipping"
                ),
            }
        }
        Ok(out)
    }
}

/// REST 認証経路から呼ばれる資格情報シンク。永続化 + 稼働中 SMB への反映を行う。
pub struct SmbCredentialSync {
    store: SmbCredentialStore,
    config: ConfigHandle,
    /// `AuthenticatedOnly` な各 share に対してユーザーへ ReadWrite を付与するための一覧。
    shares: Vec<String>,
}

impl SmbCredentialSync {
    pub fn new(store: SmbCredentialStore, config: ConfigHandle, shares: Vec<String>) -> Self {
        Self {
            store,
            config,
            shares,
        }
    }

    /// 稼働中のユーザーテーブルへ creds を登録し、全 share に ReadWrite を付与する。
    ///
    /// 付与先は「稼働中の全 share」（静的 share に加え、保存クエリから動的に追加
    /// された任意名 share も含む）。`config.share_names()` を都度引くことで、後から
    /// 増えたクエリ share へも新規/復元ユーザーが自動的にアクセスできる。
    async fn apply_to_running(&self, username: &str, creds: UserCreds) {
        if let Err(e) = self.config.add_user_creds(username, creds).await {
            tracing::warn!(user = %username, error = %e, "registering SMB user failed");
            return;
        }
        let mut shares = self.config.share_names().await;
        for s in &self.shares {
            if !shares.iter().any(|x| x.eq_ignore_ascii_case(s)) {
                shares.push(s.clone());
            }
        }
        for share in &shares {
            if let Err(e) = self
                .config
                .grant_share_user(share, username, Access::ReadWrite)
                .await
            {
                tracing::warn!(user = %username, share, error = %e, "granting SMB share access failed");
            }
        }
    }

    /// 永続化済みの NT ハッシュを稼働中の SMB ユーザーテーブルへ復元する。
    pub async fn restore(&self) {
        match self.store.list_all().await {
            Ok(list) => {
                let n = list.len();
                for (username, nt_hash) in list {
                    self.apply_to_running(&username, UserCreds::from_nt_hash(nt_hash))
                        .await;
                }
                if n > 0 {
                    tracing::info!(count = n, "restored persisted SMB credentials");
                }
            }
            Err(e) => tracing::warn!(error = %e, "loading persisted SMB credentials failed"),
        }
    }
}

#[async_trait]
impl SmbCredentialSink for SmbCredentialSync {
    async fn upsert(&self, username: &str, password: &str) {
        let creds = UserCreds::from_password(password);
        // 永続化に失敗しても、当該セッション中は使えるよう稼働中テーブルには反映する。
        if let Err(e) = self.store.upsert(username, &creds.nt_hash).await {
            tracing::warn!(user = %username, error = %e, "persisting SMB credential failed");
        }
        self.apply_to_running(username, creds).await;
    }

    async fn remove(&self, username: &str) {
        if let Err(e) = self.store.delete(username).await {
            tracing::warn!(user = %username, error = %e, "deleting persisted SMB credential failed");
        }
        // 既に存在しない場合 (UnknownUser) は無視してよい。
        let _ = self.config.remove_user(username).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AllBackend, ShareDeps};
    use smb_server::{Share, SmbServer};
    use std::sync::Arc;
    use user_permission_core::Database as AuthDb;
    use yozist_auth::{Authorizer, DbAuthorizer, SmbCredentialSink};
    use yozist_db::{AuditLog, SharedMetaStore, SqliteMetaStore};
    use yozist_storage::FsBlobStore;
    use yozist_versioning::{CrdtRegistry, VersioningEngine};

    async fn test_deps_and_pool() -> (ShareDeps, SqlitePool, tempfile::TempDir) {
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

    fn test_server(deps: ShareDeps) -> SmbServer {
        SmbServer::builder()
            .listen("127.0.0.1:0".parse().unwrap())
            .share(Share::new("all", AllBackend::new(deps)))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn store_round_trip() {
        let (_deps, pool, _dir) = test_deps_and_pool().await;
        let store = SmbCredentialStore::new(pool);

        let h1 = [7u8; 16];
        store.upsert("alice", &h1).await.unwrap();
        assert_eq!(
            store.list_all().await.unwrap(),
            vec![("alice".to_string(), h1)]
        );

        // upsert は同じユーザーを上書きする。
        let h2 = [9u8; 16];
        store.upsert("alice", &h2).await.unwrap();
        assert_eq!(
            store.list_all().await.unwrap(),
            vec![("alice".to_string(), h2)]
        );

        store.delete("alice").await.unwrap();
        assert!(store.list_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_registers_user_and_persists_nt_hash() {
        let (deps, pool, _dir) = test_deps_and_pool().await;
        let server = test_server(deps);
        let sync = SmbCredentialSync::new(
            SmbCredentialStore::new(pool.clone()),
            server.config_handle(),
            vec!["all".to_string()],
        );

        sync.upsert("alice", "password").await;

        // 稼働中の SMB ユーザーテーブルに載っている。
        assert!(server.state().lookup_user("alice").await.is_some());
        // 永続化された NT ハッシュは smb-server の導出と一致する。
        let persisted = SmbCredentialStore::new(pool).list_all().await.unwrap();
        assert_eq!(persisted, vec![("alice".to_string(), smb_server::nt_hash("password"))]);
    }

    #[tokio::test]
    async fn restore_repopulates_running_table() {
        let (deps, pool, _dir) = test_deps_and_pool().await;
        // 再起動前に永続化された資格情報を直接シードする。
        SmbCredentialStore::new(pool.clone())
            .upsert("bob", &smb_server::nt_hash("secret"))
            .await
            .unwrap();

        let server = test_server(deps);
        let sync = SmbCredentialSync::new(
            SmbCredentialStore::new(pool),
            server.config_handle(),
            vec!["all".to_string()],
        );

        // restore 前は未登録。
        assert!(server.state().lookup_user("bob").await.is_none());
        sync.restore().await;
        assert!(server.state().lookup_user("bob").await.is_some());
    }

    #[tokio::test]
    async fn remove_clears_running_and_persisted() {
        let (deps, pool, _dir) = test_deps_and_pool().await;
        let server = test_server(deps);
        let sync = SmbCredentialSync::new(
            SmbCredentialStore::new(pool.clone()),
            server.config_handle(),
            vec!["all".to_string()],
        );

        sync.upsert("carol", "pw").await;
        assert!(server.state().lookup_user("carol").await.is_some());

        sync.remove("carol").await;
        assert!(server.state().lookup_user("carol").await.is_none());
        assert!(SmbCredentialStore::new(pool).list_all().await.unwrap().is_empty());
    }
}
