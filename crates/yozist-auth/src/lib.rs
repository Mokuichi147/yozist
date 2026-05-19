//! yozist-auth — 認可 (ACL) + 共有トークン。
//!
//! ユーザー/グループ/JWT 認証は upstream `user-permission` クレートに委譲した。
//! 本クレートは yozist 固有の以下の責務のみを担う:
//!
//! - **ACL ルール** (`acl_rules` テーブル): user/group × target × bitmask × allow/deny
//! - **期限付き共有トークン** (`share_tokens` テーブル): jti 単位の revocation
//!
//! `User` / `Group` 型は `user_permission_core` のものをそのまま再エクスポートする。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use yozist_core::{GroupId, UserId};

pub mod authorizer;
pub mod permission;
pub mod sqlite;
pub use authorizer::DbAuthorizer;
pub use permission::{Permission, PermissionMask, Subject, Target};
pub use sqlite::ShareTokenStore;

// upstream の型を再エクスポート（呼び出し側は `yozist_auth::User` で参照可能）。
pub use user_permission_core::{Group, User};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken(pub String);

/// 期限付き共有 URL のトークンに含めるクレーム。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareClaims {
    /// "file" | "query" など、任意の文字列。
    pub kind: String,
    /// 対象 ID（FileId / SavedQueryId 等を文字列化したもの）。
    pub target_id: String,
    pub exp: i64,
    pub iat: i64,
    pub iss: Option<String>, // 発行者 username
    /// JWT ID — `share_tokens` テーブルで失効を判定する一意鍵。
    pub jti: String,
}

/// `share_tokens` テーブルの 1 行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareTokenRecord {
    pub jti: String,
    pub kind: String,
    pub target_id: String,
    pub issuer: Option<String>,
    pub issued_at: time::OffsetDateTime,
    pub expires_at: Option<time::OffsetDateTime>,
    pub revoked_at: Option<time::OffsetDateTime>,
}

/// 認証リクエストの主体。SMB セッション / API JWT / 内部呼び出しを表現。
#[derive(Debug, Clone)]
pub enum AuthContext {
    Anonymous,
    User { user: User, groups: Vec<GroupId> },
    System,
}

impl AuthContext {
    pub fn user_id(&self) -> Option<UserId> {
        match self {
            AuthContext::User { user, .. } => Some(user.id),
            _ => None,
        }
    }
}

/// 認可（ACL）評価器。
#[async_trait]
pub trait Authorizer: Send + Sync {
    /// アクションが許可されているか判定。
    async fn check(
        &self,
        ctx: &AuthContext,
        target: &Target,
        required: PermissionMask,
    ) -> Result<bool, AuthError>;
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid token")]
    InvalidToken,
    #[error("db error: {0}")]
    Db(#[from] yozist_db::DbError),
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("other: {0}")]
    Other(String),
}

impl From<AuthError> for yozist_core::Error {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::InvalidToken => yozist_core::Error::PermissionDenied(e.to_string()),
            _ => yozist_core::Error::Other(yozist_core::anyhow_compat::AnyError::new(
                e.to_string(),
            )),
        }
    }
}
