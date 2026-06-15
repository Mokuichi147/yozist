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
use user_permission_core::Database as AuthDb;
use yozist_core::{GroupId, UserId};

pub mod authorizer;
pub mod permission;
pub mod sqlite;
pub use authorizer::DbAuthorizer;
pub use permission::{Permission, PermissionMask, Subject, Target};
pub use sqlite::ShareTokenStore;

// upstream の型を再エクスポート（呼び出し側は `yozist_auth::User` で参照可能）。
pub use user_permission_core::{Group, User};

/// Bearer JWT から `AuthContext` を解決する。
///
/// local / relay のどちらの backend でも同じコードパスで動く:
/// 1. JWT のペイロードから `sub` (user id) を **未検証で** 取り出す
/// 2. `users().get_by_id(sub, Some(token))` を呼ぶ。
///    - local backend (user-permission >= 0.2.2) は per-call token を検証する
///    - relay backend は token を上流へ転送し、上流が検証する
/// 3. 署名が無効なら get_by_id がエラーになり、ここで `InvalidToken` を返す
///
/// `sub` を JWT 自身のペイロードから取る点が重要: リクエスト引数ではなく
/// トークンの所有者 ID を使うため、有効な署名がある限りなりすましは起きない。
pub async fn resolve_auth_context(
    db: &AuthDb,
    token: &str,
) -> Result<AuthContext, AuthError> {
    let sub = decode_jwt_sub(token).ok_or(AuthError::InvalidToken)?;
    let user = db
        .users()
        .get_by_id(sub, Some(token))
        .await
        .map_err(|_| AuthError::InvalidToken)?
        .ok_or(AuthError::InvalidToken)?;
    let groups = db
        .groups()
        .get_user_groups(sub, Some(token))
        .await
        .map_err(|e| AuthError::Other(e.to_string()))?
        .into_iter()
        .map(|g| g.id)
        .collect();
    Ok(AuthContext::User { user, groups })
}

/// JWT のペイロードから `sub` を **署名検証せずに** 取り出す。
///
/// どの user id のレコードを引くかを決めるためだけに使う。署名の検証は
/// 後続の `get_by_id(sub, Some(token))` が backend 越しに行う。
fn decode_jwt_sub(token: &str) -> Option<UserId> {
    use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};

    #[derive(Deserialize)]
    struct SubClaim {
        sub: String,
    }

    let mut validation = Validation::new(Algorithm::HS256);
    validation.insecure_disable_signature_validation();
    validation.validate_exp = false;
    validation.validate_aud = false;
    validation.required_spec_claims.clear();

    let data = decode::<SubClaim>(token, &DecodingKey::from_secret(b""), &validation).ok()?;
    data.claims.sub.parse::<UserId>().ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken(pub String);

/// 期限付き共有 URL のトークンに含めるクレーム。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareClaims {
    /// "file" | "filter" など、任意の文字列。
    pub kind: String,
    /// 対象 ID（FileId / FilterId 等を文字列化したもの）。
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

/// SMB(NTLM) 認証に必要な NT ハッシュを同期するためのフック。
///
/// user-permission は Argon2id ハッシュしか持たず NTLM では使えないため、REST
/// 認証経路で平文パスワードを観測したタイミングで本トレイト経由 NT ハッシュを
/// 導出・永続化し、稼働中の SMB サーバへ反映する。実装は `yozist-smb` 側にある。
#[async_trait]
pub trait SmbCredentialSink: Send + Sync {
    /// 平文パスワードから NT ハッシュを導出して保存し、稼働中 SMB へ反映する。
    /// 失敗は内部でログに記録し、認証フローは中断しない。
    async fn upsert(&self, username: &str, password: &str);
    /// ユーザーの SMB 資格情報を削除する。
    async fn remove(&self, username: &str);
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
