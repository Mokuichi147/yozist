//! SQLite 製 `AuthService` 実装。
//!
//! - パスワードハッシュ: Argon2id（`argon2` クレート）
//! - トークン: HS256 JWT（`jsonwebtoken` クレート）
//! - DB は `yozist-db` と共有（`SqlitePool` を受け取る）
//!
//! # シークレットキー
//! HMAC 鍵は呼び出し側から `Vec<u8>` で渡す。バイナリ側で初回起動時に
//! 生成・ファイル保存する想定。

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use async_trait::async_trait;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;
use time::OffsetDateTime;
use uuid::Uuid;
use yozist_core::{GroupId, UserId};

use crate::{
    AuthError, AuthService, AuthToken, Group, ShareClaims, ShareTokenRecord, TokenClaims, User,
};

pub struct SqliteAuthService {
    pool: SqlitePool,
    secret: Vec<u8>,
    /// JWT 有効期限（秒）。デフォルト 24h。
    pub token_ttl_secs: i64,
}

impl SqliteAuthService {
    pub fn new(pool: SqlitePool, secret: Vec<u8>) -> Self {
        Self {
            pool,
            secret,
            token_ttl_secs: 24 * 3600,
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// グループを作成。
    pub async fn create_group(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<Group, AuthError> {
        let id = GroupId::new();
        sqlx::query("INSERT INTO groups (id, name, description) VALUES (?, ?, ?)")
            .bind(id.to_string())
            .bind(name)
            .bind(description)
            .execute(&self.pool)
            .await?;
        Ok(Group {
            id,
            name: name.into(),
            description: description.map(str::to_string),
        })
    }

    /// ユーザー名で取得。
    pub async fn get_user_by_username(
        &self,
        username: &str,
    ) -> Result<Option<User>, AuthError> {
        let row = sqlx::query(
            "SELECT id, username, display_name, is_active, created_at FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_user).transpose()
    }

    /// グループ ID 一覧（ユーザーが所属する）。
    pub async fn user_groups(&self, user: &UserId) -> Result<Vec<GroupId>, AuthError> {
        let rows = sqlx::query("SELECT group_id FROM user_groups WHERE user_id = ?")
            .bind(user.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                let s: String = r.try_get("group_id")?;
                Uuid::parse_str(&s)
                    .map(GroupId::from_uuid)
                    .map_err(|e| AuthError::Other(format!("uuid: {e}")))
            })
            .collect()
    }
}

fn row_to_user(row: sqlx::sqlite::SqliteRow) -> Result<User, AuthError> {
    let id: String = row.try_get("id")?;
    let username: String = row.try_get("username")?;
    let display_name: Option<String> = row.try_get("display_name")?;
    let is_active: i64 = row.try_get("is_active")?;
    let created_at: String = row.try_get("created_at")?;
    let dt = OffsetDateTime::parse(&created_at, &time::format_description::well_known::Rfc3339)
        .map_err(|e| AuthError::Other(format!("dt: {e}")))?;
    Ok(User {
        id: UserId::from_uuid(
            Uuid::from_str(&id).map_err(|e| AuthError::Other(format!("uuid: {e}")))?,
        ),
        username,
        display_name,
        is_active: is_active != 0,
        created_at: dt,
    })
}

fn hash_password(pw: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

fn verify_password(pw: &str, hash: &str) -> Result<bool, AuthError> {
    let parsed = PasswordHash::new(hash).map_err(|e| AuthError::Hash(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(pw.as_bytes(), &parsed)
        .is_ok())
}

#[async_trait]
impl AuthService for SqliteAuthService {
    async fn create_user(
        &self,
        username: &str,
        password: &str,
    ) -> Result<User, AuthError> {
        // 同名チェック
        if self.get_user_by_username(username).await?.is_some() {
            return Err(AuthError::UsernameTaken);
        }
        let id = UserId::new();
        let now = OffsetDateTime::now_utc();
        let now_str = now
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| AuthError::Other(format!("dt: {e}")))?;
        let hash = hash_password(password)?;
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash, is_active, created_at)
             VALUES (?, ?, NULL, ?, 1, ?)",
        )
        .bind(id.to_string())
        .bind(username)
        .bind(&hash)
        .bind(&now_str)
        .execute(&self.pool)
        .await?;
        Ok(User {
            id,
            username: username.into(),
            display_name: None,
            is_active: true,
            created_at: now,
        })
    }

    async fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<AuthToken>, AuthError> {
        let row = sqlx::query("SELECT id, password_hash, is_active FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let id: String = row.try_get("id")?;
        let stored: String = row.try_get("password_hash")?;
        let is_active: i64 = row.try_get("is_active")?;
        if is_active == 0 {
            return Ok(None);
        }
        if !verify_password(password, &stored)? {
            return Ok(None);
        }

        let iat = OffsetDateTime::now_utc().unix_timestamp();
        let claims = TokenClaims {
            sub: id,
            username: username.to_string(),
            exp: iat + self.token_ttl_secs,
            iat,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(&self.secret),
        )?;
        Ok(Some(AuthToken(token)))
    }

    async fn verify_token(&self, token: &str) -> Result<TokenClaims, AuthError> {
        let data = decode::<TokenClaims>(
            token,
            &DecodingKey::from_secret(&self.secret),
            &Validation::default(),
        )?;
        Ok(data.claims)
    }

    async fn get_user(&self, id: &UserId) -> Result<Option<User>, AuthError> {
        let row = sqlx::query(
            "SELECT id, username, display_name, is_active, created_at FROM users WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_user).transpose()
    }

    async fn groups_of(&self, user: &UserId) -> Result<Vec<GroupId>, AuthError> {
        self.user_groups(user).await
    }

    async fn list_users(&self) -> Result<Vec<User>, AuthError> {
        let rows = sqlx::query(
            "SELECT id, username, display_name, is_active, created_at
             FROM users ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_user).collect()
    }

    async fn add_user_to_group(
        &self,
        user: UserId,
        group: GroupId,
    ) -> Result<(), AuthError> {
        sqlx::query(
            "INSERT INTO user_groups (user_id, group_id) VALUES (?, ?)
             ON CONFLICT DO NOTHING",
        )
        .bind(user.to_string())
        .bind(group.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn issue_share_token(
        &self,
        kind: &str,
        target_id: &str,
        ttl_secs: i64,
        issuer: Option<&str>,
    ) -> Result<AuthToken, AuthError> {
        let iat_dt = OffsetDateTime::now_utc();
        let iat = iat_dt.unix_timestamp();
        let exp = iat + ttl_secs;
        let jti = uuid::Uuid::now_v7().to_string();

        // share_tokens テーブルに記録
        let iat_str = iat_dt
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| AuthError::Other(format!("dt: {e}")))?;
        let exp_dt = OffsetDateTime::from_unix_timestamp(exp)
            .map_err(|e| AuthError::Other(format!("dt: {e}")))?;
        let exp_str = exp_dt
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| AuthError::Other(format!("dt: {e}")))?;
        sqlx::query(
            "INSERT INTO share_tokens
               (jti, kind, target_id, issuer, issued_at, expires_at, revoked_at)
             VALUES (?, ?, ?, ?, ?, ?, NULL)",
        )
        .bind(&jti)
        .bind(kind)
        .bind(target_id)
        .bind(issuer)
        .bind(iat_str)
        .bind(exp_str)
        .execute(&self.pool)
        .await?;

        let claims = ShareClaims {
            kind: kind.to_string(),
            target_id: target_id.to_string(),
            exp,
            iat,
            iss: issuer.map(str::to_string),
            jti,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(&self.secret),
        )?;
        Ok(AuthToken(token))
    }

    async fn verify_share_token(&self, token: &str) -> Result<ShareClaims, AuthError> {
        // 共有URLは期限を厳密に評価（デフォルトのleeway 60秒は使わない）
        let mut validation = Validation::default();
        validation.leeway = 0;
        let data = decode::<ShareClaims>(
            token,
            &DecodingKey::from_secret(&self.secret),
            &validation,
        )?;
        let claims = data.claims;

        // 失効リストを確認
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT revoked_at FROM share_tokens WHERE jti = ?")
                .bind(&claims.jti)
                .fetch_optional(&self.pool)
                .await?;
        match row {
            Some((Some(_),)) => Err(AuthError::InvalidToken),
            Some((None,)) => Ok(claims),
            None => {
                // DB に記録の無いトークン（古い形式等）は無効扱い
                Err(AuthError::InvalidToken)
            }
        }
    }
}

impl SqliteAuthService {
    /// 発行済み共有トークン一覧（issuer が指定されればそのユーザー分のみ）。
    pub async fn list_share_tokens(
        &self,
        issuer: Option<&str>,
    ) -> Result<Vec<ShareTokenRecord>, AuthError> {
        let rows = if let Some(u) = issuer {
            sqlx::query(
                "SELECT jti, kind, target_id, issuer, issued_at, expires_at, revoked_at
                 FROM share_tokens WHERE issuer = ? ORDER BY issued_at DESC",
            )
            .bind(u)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT jti, kind, target_id, issuer, issued_at, expires_at, revoked_at
                 FROM share_tokens ORDER BY issued_at DESC",
            )
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter()
            .map(|r| {
                let parse_opt = |s: Option<String>| -> Result<Option<OffsetDateTime>, AuthError> {
                    s.map(|v| {
                        OffsetDateTime::parse(
                            &v,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .map_err(|e| AuthError::Other(format!("dt: {e}")))
                    })
                    .transpose()
                };
                let issued: String = r.try_get("issued_at")?;
                Ok(ShareTokenRecord {
                    jti: r.try_get("jti")?,
                    kind: r.try_get("kind")?,
                    target_id: r.try_get("target_id")?,
                    issuer: r.try_get("issuer")?,
                    issued_at: OffsetDateTime::parse(
                        &issued,
                        &time::format_description::well_known::Rfc3339,
                    )
                    .map_err(|e| AuthError::Other(format!("dt: {e}")))?,
                    expires_at: parse_opt(r.try_get("expires_at")?)?,
                    revoked_at: parse_opt(r.try_get("revoked_at")?)?,
                })
            })
            .collect()
    }

    /// jti を指定して失効させる。既に失効済みなら no-op。
    pub async fn revoke_share_token(&self, jti: &str) -> Result<(), AuthError> {
        let now_str = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| AuthError::Other(format!("dt: {e}")))?;
        let res = sqlx::query(
            "UPDATE share_tokens SET revoked_at = ?
             WHERE jti = ? AND revoked_at IS NULL",
        )
        .bind(now_str)
        .bind(jti)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(AuthError::UserNotFound); // 流用: not found 系
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yozist_db::SqliteMetaStore;

    async fn service() -> SqliteAuthService {
        let store = SqliteMetaStore::open_in_memory().await.unwrap();
        SqliteAuthService::new(store.pool().clone(), b"test-secret".to_vec())
    }

    #[tokio::test]
    async fn create_and_authenticate() {
        let s = service().await;
        let user = s.create_user("alice", "password123").await.unwrap();
        assert_eq!(user.username, "alice");

        let token = s
            .authenticate("alice", "password123")
            .await
            .unwrap()
            .expect("authenticated");
        let claims = s.verify_token(&token.0).await.unwrap();
        assert_eq!(claims.username, "alice");
        assert_eq!(claims.sub, user.id.to_string());
    }

    #[tokio::test]
    async fn wrong_password_returns_none() {
        let s = service().await;
        s.create_user("alice", "right").await.unwrap();
        let t = s.authenticate("alice", "wrong").await.unwrap();
        assert!(t.is_none());
    }

    #[tokio::test]
    async fn duplicate_username_rejected() {
        let s = service().await;
        s.create_user("alice", "pw").await.unwrap();
        match s.create_user("alice", "pw2").await {
            Err(AuthError::UsernameTaken) => {}
            other => panic!("expected UsernameTaken, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn group_membership() {
        let s = service().await;
        let user = s.create_user("bob", "pw").await.unwrap();
        let group = s.create_group("admins", None).await.unwrap();
        s.add_user_to_group(user.id, group.id).await.unwrap();
        let groups = s.user_groups(&user.id).await.unwrap();
        assert_eq!(groups, vec![group.id]);
    }

    #[tokio::test]
    async fn list_users_returns_all() {
        let s = service().await;
        s.create_user("a", "pw").await.unwrap();
        s.create_user("b", "pw").await.unwrap();
        let users = s.list_users().await.unwrap();
        assert_eq!(users.len(), 2);
    }
}
