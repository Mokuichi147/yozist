//! SQLite 製の共有トークンストア。
//!
//! ユーザー/グループ/JWT は upstream `user-permission` に移管したため、
//! 本モジュールは `share_tokens` テーブルの操作のみを担当する。
//! 共有トークン自体は HS256 JWT で、jti を `share_tokens` テーブルに記録して
//! 失効を判定する。

use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;

use crate::{AuthError, AuthToken, ShareClaims, ShareTokenRecord};

pub struct ShareTokenStore {
    pool: SqlitePool,
    secret: Vec<u8>,
}

impl ShareTokenStore {
    pub fn new(pool: SqlitePool, secret: Vec<u8>) -> Self {
        Self { pool, secret }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// 期限付き共有トークンを発行する。
    pub async fn issue_share_token(
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

    /// 期限付き共有トークンを検証する。
    pub async fn verify_share_token(&self, token: &str) -> Result<ShareClaims, AuthError> {
        // 共有URLは期限を厳密に評価（デフォルトの leeway 60 秒は使わない）。
        let mut validation = Validation::default();
        validation.leeway = 0;
        let data = decode::<ShareClaims>(
            token,
            &DecodingKey::from_secret(&self.secret),
            &validation,
        )?;
        let claims = data.claims;

        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT revoked_at FROM share_tokens WHERE jti = ?")
                .bind(&claims.jti)
                .fetch_optional(&self.pool)
                .await?;
        match row {
            Some((Some(_),)) => Err(AuthError::InvalidToken),
            Some((None,)) => Ok(claims),
            None => Err(AuthError::InvalidToken),
        }
    }

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

    /// jti を指定して失効させる。既に失効済みなら not-found 相当のエラー。
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
            return Err(AuthError::Other("share token not found or already revoked".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yozist_db::SqliteMetaStore;

    async fn store() -> ShareTokenStore {
        let s = SqliteMetaStore::open_in_memory().await.unwrap();
        ShareTokenStore::new(s.pool().clone(), b"test-secret".to_vec())
    }

    #[tokio::test]
    async fn issue_and_verify() {
        let s = store().await;
        let tok = s
            .issue_share_token("file", "abc", 60, Some("alice"))
            .await
            .unwrap();
        let claims = s.verify_share_token(&tok.0).await.unwrap();
        assert_eq!(claims.kind, "file");
        assert_eq!(claims.target_id, "abc");
        assert_eq!(claims.iss.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn revoke_invalidates() {
        let s = store().await;
        let tok = s
            .issue_share_token("file", "abc", 60, None)
            .await
            .unwrap();
        let claims = s.verify_share_token(&tok.0).await.unwrap();
        s.revoke_share_token(&claims.jti).await.unwrap();
        assert!(s.verify_share_token(&tok.0).await.is_err());
    }
}
