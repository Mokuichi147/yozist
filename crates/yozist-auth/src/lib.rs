//! yozist-auth — 認証 + 認可（ACL）。
//!
//! ユーザー自作の Python 製 [`UserPermission`](https://github.com/Mokuichi147/UserPermission)
//! を Rust に移植する形で実装する。
//!
//! # マッピング
//! - aiosqlite → sqlx (yozist-db 共有 DB)
//! - pwdlib (Argon2) → argon2 クレート
//! - PyJWT → jsonwebtoken クレート
//!
//! # 設計原則
//! - **共有 DB**: ユーザー／グループ／ACL は `yozist-db` の同一 DB に格納する
//!   （ファイル管理と統合）
//! - **細粒度 ACL**: share / tag / series / file / query 各レベルで設定可能
//! - **動的パス発行**: REST から saved-query share を作成、期限付き発行に対応
//!
//! # TODO
//! - [ ] 元 `UserPermission` の API カバレッジ 100%（更新・削除・グループ一覧 等）
//! - [ ] `smb-server::ConfigHandle` 連携で SMB ユーザーを動的同期
//! - [ ] グループ階層（ネスト）対応
//! - [ ] Authorizer の本実装（ACL ルール評価エンジン）
//! - [ ] 監査ログ（誰がいつ何にアクセス）

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use yozist_core::{GroupId, UserId};

pub mod authorizer;
pub mod permission;
pub mod sqlite;
pub use authorizer::DbAuthorizer;
pub use permission::{Permission, PermissionMask, Subject, Target};
pub use sqlite::SqliteAuthService;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub display_name: Option<String>,
    pub is_active: bool,
    pub created_at: time::OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: GroupId,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String, // user id
    pub username: String,
    pub exp: i64,
    pub iat: i64,
}

/// 期限付き共有 URL のトークンに含めるクレーム。
///
/// `kind` でファイル単体共有か保存クエリ共有かを区別する。
/// `target_id` はそれぞれ対応する UUID 文字列。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareClaims {
    /// "file" | "query"
    pub kind: String,
    /// 対象 ID（FileId / SavedQueryId）
    pub target_id: String,
    pub exp: i64,
    pub iat: i64,
    pub iss: Option<String>, // 発行者 username
}

/// 認証リクエストの主体。SMB セッション / API JWT / 内部呼び出しを表現。
#[derive(Debug, Clone)]
pub enum AuthContext {
    Anonymous,
    User { user: User, groups: Vec<GroupId> },
    System,
}

#[async_trait]
pub trait AuthService: Send + Sync {
    async fn create_user(
        &self,
        username: &str,
        password: &str,
    ) -> Result<User, AuthError>;
    async fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<AuthToken>, AuthError>;
    async fn verify_token(&self, token: &str) -> Result<TokenClaims, AuthError>;
    async fn list_users(&self) -> Result<Vec<User>, AuthError>;
    async fn get_user(&self, id: &UserId) -> Result<Option<User>, AuthError>;
    async fn groups_of(&self, user: &UserId) -> Result<Vec<GroupId>, AuthError>;
    async fn add_user_to_group(
        &self,
        user: UserId,
        group: GroupId,
    ) -> Result<(), AuthError>;

    /// 期限付き共有トークンを発行する。
    async fn issue_share_token(
        &self,
        kind: &str,
        target_id: &str,
        ttl_secs: i64,
        issuer: Option<&str>,
    ) -> Result<AuthToken, AuthError>;

    /// 期限付き共有トークンを検証する。
    async fn verify_share_token(&self, token: &str) -> Result<ShareClaims, AuthError>;
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
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("token expired or invalid")]
    InvalidToken,
    #[error("user not found")]
    UserNotFound,
    #[error("username already exists")]
    UsernameTaken,
    #[error("db error: {0}")]
    Db(#[from] yozist_db::DbError),
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("hash error: {0}")]
    Hash(String),
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("other: {0}")]
    Other(String),
}

impl From<AuthError> for yozist_core::Error {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::InvalidCredentials | AuthError::InvalidToken => {
                yozist_core::Error::PermissionDenied(e.to_string())
            }
            AuthError::UserNotFound => yozist_core::Error::NotFound("user".into()),
            _ => yozist_core::Error::Other(yozist_core::anyhow_compat::AnyError::new(
                e.to_string(),
            )),
        }
    }
}
