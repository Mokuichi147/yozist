//! yozist-api — REST API + AI エンドポイント + WebUI 配信を一手に担う層。
//!
//! ユーザー原案の「api / web-ui（インターフェース層）」を 1 クレートに集約。
//! `axum::Router` 上に REST と SSR ハンドラを同居させる。
//!
//! # 設計原則
//! - **同じビュー**: SMB / WebUI / REST すべて同じ `MetaStore` クエリを使う
//! - **書き込みは versioning 経由**: REST 書き込みも必ず `commit()` を呼ぶ
//!
//! # TODO
//! - [ ] `leptos` 統合（現状は静的プレースホルダ）
//! - [ ] OpenAPI ドキュメント（`utoipa`）
//! - [ ] saved-query share 発行 API（`POST /api/shares`）
//! - [ ] WebSocket での変更通知（`yozist-versioning` の broadcast に接続）
//! - [ ] 共有 URL（期限付き、JWT）配信エンドポイント `/api/shared/<token>`

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use yozist_auth::{AuthService, AuthToken};
use yozist_core::{ActorId, FileId, FileMeta, Tag, TagId, TagKind};
use yozist_db::SharedMetaStore;
use yozist_versioning::VersioningEngine;

/// API ハンドラが共有する状態。
#[derive(Clone)]
pub struct ApiState {
    pub meta: SharedMetaStore,
    pub engine: Arc<VersioningEngine>,
    pub auth: Arc<dyn AuthService>,
}

/// ルーター生成。
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/files", get(list_files).post(create_file))
        .route("/api/files/:id", get(get_file))
        .route("/api/files/:id/content", get(get_content).post(commit_file))
        .route("/api/files/:id/history", get(history))
        .route("/api/files/:id/tags", post(attach_tag))
        .route("/api/tags", post(upsert_tag))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateFileQuery {
    name: String,
    /// 任意で actor を指定（未指定なら新規生成）
    actor: Option<String>,
}

async fn list_files(State(s): State<ApiState>) -> Result<Json<Vec<FileMeta>>, ApiError> {
    let files = s.meta.list_files(100, 0).await.map_err(ApiError::from_db)?;
    Ok(Json(files))
}

async fn create_file(
    State(s): State<ApiState>,
    Query(q): Query<CreateFileQuery>,
    body: Bytes,
) -> Result<(StatusCode, Json<FileMeta>), ApiError> {
    let actor = parse_actor(q.actor.as_deref()).unwrap_or_else(ActorId::new);
    let (file, _commit) = s
        .engine
        .create_file(q.name, &body, actor, None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(file)))
}

async fn get_file(
    State(s): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<FileMeta>, ApiError> {
    let id = parse_file_id(&id)?;
    let meta = s
        .meta
        .get_file(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(meta))
}

async fn get_content(
    State(s): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Vec<u8>, ApiError> {
    let id = parse_file_id(&id)?;
    s.engine
        .read_current(id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
}

#[derive(Deserialize)]
struct CommitQuery {
    actor: Option<String>,
    message: Option<String>,
}

async fn commit_file(
    State(s): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<CommitQuery>,
    body: Bytes,
) -> Result<Json<yozist_core::Commit>, ApiError> {
    let id = parse_file_id(&id)?;
    let actor = parse_actor(q.actor.as_deref()).unwrap_or_else(ActorId::new);
    let commit = s
        .engine
        .commit(id, &body, actor, q.message)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(commit))
}

async fn history(
    State(s): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<yozist_core::Commit>>, ApiError> {
    let id = parse_file_id(&id)?;
    let log = s.meta.list_commits(&id).await.map_err(ApiError::from_db)?;
    Ok(Json(log))
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TagInput {
    name: String,
    kind: Option<String>, // system | ai | manual (default manual)
    confidence: Option<f32>,
}

#[derive(Serialize)]
struct TagCreated {
    id: TagId,
}

async fn upsert_tag(
    State(s): State<ApiState>,
    Json(input): Json<TagInput>,
) -> Result<Json<TagCreated>, ApiError> {
    let kind = match input.kind.as_deref().unwrap_or("manual") {
        "system" => TagKind::System,
        "ai" => TagKind::Ai,
        "manual" => TagKind::Manual,
        other => {
            return Err(ApiError::BadRequest(format!("unknown tag kind: {other}")))
        }
    };
    let tag = Tag {
        id: TagId::new(),
        name: input.name,
        kind,
        confidence: input.confidence,
    };
    let id = s.meta.upsert_tag(&tag).await.map_err(ApiError::from_db)?;
    Ok(Json(TagCreated { id }))
}

#[derive(Deserialize)]
struct AttachTagInput {
    tag_id: String,
}

async fn attach_tag(
    State(s): State<ApiState>,
    Path(file_id): Path<String>,
    Json(input): Json<AttachTagInput>,
) -> Result<StatusCode, ApiError> {
    let file_id = parse_file_id(&file_id)?;
    let tag_id = uuid::Uuid::parse_str(&input.tag_id)
        .map(TagId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("tag_id: {e}")))?;
    s.meta
        .attach_tag(&file_id, &tag_id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuthInput {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct AuthResponse {
    token: String,
}

async fn register(
    State(s): State<ApiState>,
    Json(input): Json<AuthInput>,
) -> Result<(StatusCode, Json<yozist_auth::User>), ApiError> {
    let user = s
        .auth
        .create_user(&input.username, &input.password)
        .await
        .map_err(|e| match e {
            yozist_auth::AuthError::UsernameTaken => ApiError::Conflict,
            other => ApiError::Internal(other.to_string()),
        })?;
    Ok((StatusCode::CREATED, Json(user)))
}

async fn login(
    State(s): State<ApiState>,
    Json(input): Json<AuthInput>,
) -> Result<Json<AuthResponse>, ApiError> {
    match s
        .auth
        .authenticate(&input.username, &input.password)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    {
        Some(AuthToken(token)) => Ok(Json(AuthResponse { token })),
        None => Err(ApiError::Unauthorized),
    }
}

// ---------------------------------------------------------------------------
// Helpers / errors
// ---------------------------------------------------------------------------

fn parse_file_id(s: &str) -> Result<FileId, ApiError> {
    uuid::Uuid::parse_str(s)
        .map(FileId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("file id: {e}")))
}

fn parse_actor(s: Option<&str>) -> Option<ActorId> {
    s.and_then(|raw| uuid::Uuid::parse_str(raw).ok().map(ActorId::from_uuid))
}

#[derive(Debug)]
pub enum ApiError {
    NotFound,
    BadRequest(String),
    Unauthorized,
    Conflict,
    Internal(String),
}

impl ApiError {
    fn from_db(e: yozist_db::DbError) -> Self {
        match e {
            yozist_db::DbError::NotFound => ApiError::NotFound,
            yozist_db::DbError::Conflict(_) => ApiError::Conflict,
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, body) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            ApiError::Conflict => (StatusCode::CONFLICT, "conflict".to_string()),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yozist_db::SqliteMetaStore;
    use yozist_storage::FsBlobStore;
    use yozist_versioning::CrdtRegistry;

    async fn make_state() -> (ApiState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMetaStore::open_in_memory().await.unwrap();
        let pool = store.pool().clone();
        let blob = Arc::new(FsBlobStore::new(dir.path().join("blobs")).await.unwrap());
        let meta: SharedMetaStore = Arc::new(store);
        let registry = Arc::new(CrdtRegistry::with_defaults());
        let engine = Arc::new(VersioningEngine::new(registry, blob, meta.clone()));
        let auth: Arc<dyn AuthService> = Arc::new(
            yozist_auth::SqliteAuthService::new(pool, b"test".to_vec()),
        );
        (
            ApiState {
                meta,
                engine,
                auth,
            },
            dir,
        )
    }

    #[tokio::test]
    async fn router_serves_health() {
        let (state, _td) = make_state().await;
        let app = router(state);
        let resp = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .uri("/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn create_then_get_file_via_engine_directly() {
        // ハンドラ経由でなく engine 直接 — エンドツーエンドは serve テストで。
        let (state, _td) = make_state().await;
        let (file, _c) = state
            .engine
            .create_file("a.txt", b"hi", ActorId::new(), None)
            .await
            .unwrap();
        let got = state.meta.get_file(&file.id).await.unwrap().unwrap();
        assert_eq!(got.display_name, "a.txt");
    }
}
