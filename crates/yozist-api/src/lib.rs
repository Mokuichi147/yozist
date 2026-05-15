//! yozist-api — REST API + AI エンドポイント + WebUI 配信を一手に担う層。
//!
//! ユーザー原案の「api / web-ui（インターフェース層）」を 1 クレートに集約。
//! `axum::Router` 上に REST と SSR ハンドラを同居させる。
//!
//! # 設計原則
//! - **同じビュー**: SMB / WebUI / REST すべて同じ `MetaStore` クエリを使う
//! - **書き込みは versioning 経由**: REST 書き込みも必ず `commit()` を呼ぶ
//! - **権限チェック**: 書き込み系エンドポイントは必ず `Authorizer::check` を経由
//!
//! # TODO
//! - [ ] `leptos` 統合（現状は静的プレースホルダ）
//! - [ ] OpenAPI ドキュメント（`utoipa`）
//! - [ ] saved-query share 発行 API（`POST /api/shares`）
//! - [ ] WebSocket での変更通知（`yozist-versioning` の broadcast に接続）
//! - [ ] 共有 URL（期限付き、JWT）配信エンドポイント `/api/shared/<token>`

use axum::{
    body::Bytes,
    extract::{FromRef, FromRequestParts, Path, Query, State},
    http::{request::Parts, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use yozist_auth::{
    AuthContext, AuthService, AuthToken, Authorizer, DbAuthorizer, Permission, PermissionMask,
    Subject, Target,
};

pub mod ui;
use yozist_core::{
    ActorId, FileId, FileMeta, QueryDef, SavedQuery, SavedQueryId, Series, SeriesId,
    SeriesMember, Tag, TagId, TagKind, UserId,
};
use yozist_db::SharedMetaStore;
use yozist_versioning::VersioningEngine;

/// API ハンドラが共有する状態。
#[derive(Clone)]
pub struct ApiState {
    // フィールドは Clone なのでこの struct も Arc 越しに自由に clone できる。
    pub meta: SharedMetaStore,
    pub engine: Arc<VersioningEngine>,
    pub auth: Arc<dyn AuthService>,
    pub authz: Arc<dyn Authorizer>,
    /// ACL ルール CRUD 用の具象参照（同じインスタンスを `authz` と共有）。
    pub acl_admin: Arc<DbAuthorizer>,
}

/// ルーター生成。
pub fn router(state: ApiState) -> Router {
    Router::new()
        .nest("/ui", ui::router())
        .route("/", get(redirect_to_ui))
        .route("/health", get(health))
        .route("/api/files", get(list_files).post(create_file))
        .route("/api/files/:id", get(get_file))
        .route("/api/files/:id/content", get(get_content).post(commit_file))
        .route("/api/files/:id/history", get(history))
        .route(
            "/api/files/:id/tags",
            get(list_file_tags).post(attach_tag),
        )
        .route("/api/files/:id/tags/:tag_id", axum::routing::delete(detach_tag))
        .route("/api/tags", get(list_tags).post(upsert_tag))
        .route("/api/files/by-tags", get(list_files_by_tags))
        .route("/api/series", get(list_series).post(create_series))
        .route("/api/series/:id", get(get_series))
        .route(
            "/api/series/:id/members",
            get(list_series_members).post(add_series_member),
        )
        .route(
            "/api/series/:id/members/:file_id",
            axum::routing::delete(remove_series_member),
        )
        .route("/api/queries", get(list_saved_queries).post(create_saved_query))
        .route(
            "/api/queries/:id",
            get(get_saved_query).delete(delete_saved_query),
        )
        .route("/api/queries/:id/files", get(query_files))
        .route("/api/acl", post(add_acl_rule))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/me", get(me))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// AuthContext extractor
// ---------------------------------------------------------------------------

/// `Authorization: Bearer <jwt>` ヘッダから `AuthContext` を解決するエクストラクタ。
/// ヘッダが無い場合は `Anonymous`、無効トークンは 401。
pub struct AuthCtx(pub AuthContext);

#[axum::async_trait]
impl<S> FromRequestParts<S> for AuthCtx
where
    S: Send + Sync,
    ApiState: FromRef<S>,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let api: ApiState = ApiState::from_ref(state);
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());

        let Some(raw) = header else {
            return Ok(AuthCtx(AuthContext::Anonymous));
        };
        let token = raw
            .strip_prefix("Bearer ")
            .or_else(|| raw.strip_prefix("bearer "))
            .ok_or(ApiError::Unauthorized)?;

        let claims = api
            .auth
            .verify_token(token)
            .await
            .map_err(|_| ApiError::Unauthorized)?;
        let user_id = uuid::Uuid::parse_str(&claims.sub)
            .map(UserId::from_uuid)
            .map_err(|_| ApiError::Unauthorized)?;
        let user = api
            .auth
            .get_user(&user_id)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .ok_or(ApiError::Unauthorized)?;
        let groups = api
            .auth
            .groups_of(&user_id)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        Ok(AuthCtx(AuthContext::User { user, groups }))
    }
}

async fn require_authenticated(ctx: &AuthContext) -> Result<(), ApiError> {
    match ctx {
        AuthContext::Anonymous => Err(ApiError::Unauthorized),
        _ => Ok(()),
    }
}

async fn require_permission(
    authz: &dyn Authorizer,
    ctx: &AuthContext,
    target: &Target,
    mask: PermissionMask,
) -> Result<(), ApiError> {
    let ok = authz
        .check(ctx, target, mask)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if ok {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
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

async fn redirect_to_ui() -> axum::response::Redirect {
    axum::response::Redirect::permanent("/ui")
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateFileQuery {
    name: String,
    actor: Option<String>,
}

async fn list_files(State(s): State<ApiState>) -> Result<Json<Vec<FileMeta>>, ApiError> {
    let files = s.meta.list_files(100, 0).await.map_err(ApiError::from_db)?;
    Ok(Json(files))
}

async fn create_file(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Query(q): Query<CreateFileQuery>,
    body: Bytes,
) -> Result<(StatusCode, Json<FileMeta>), ApiError> {
    require_authenticated(&ctx).await?;
    let actor = parse_actor(q.actor.as_deref()).unwrap_or_else(ActorId::new);
    let (file, _commit) = s
        .engine
        .create_file(q.name, &body, actor, None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // オーナー（作成者）に ADMIN 権限を自動付与。これにより以後 ACL rule を
    // 追加しても作成者は自分のファイルへ常にアクセス可能。
    if let AuthContext::User { user, .. } = &ctx {
        let owner_rule = Permission {
            subject: Subject::User(user.id),
            target: Target::File(file.id),
            mask: PermissionMask::all(),
            allow: true,
            priority: i32::MAX,
            expires_at: None,
        };
        s.acl_admin
            .add_rule(&owner_rule)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }

    Ok((StatusCode::CREATED, Json(file)))
}

async fn get_file(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<FileMeta>, ApiError> {
    let id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::File(id), PermissionMask::VIEW).await?;
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
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<Vec<u8>, ApiError> {
    let id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::File(id), PermissionMask::READ).await?;
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
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Query(q): Query<CommitQuery>,
    body: Bytes,
) -> Result<Json<yozist_core::Commit>, ApiError> {
    let id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::File(id), PermissionMask::WRITE).await?;
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
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Vec<yozist_core::Commit>>, ApiError> {
    let id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::File(id), PermissionMask::READ).await?;
    let log = s.meta.list_commits(&id).await.map_err(ApiError::from_db)?;
    Ok(Json(log))
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TagInput {
    name: String,
    kind: Option<String>,
    confidence: Option<f32>,
}

#[derive(Serialize)]
struct TagCreated {
    id: TagId,
}

async fn list_tags(State(s): State<ApiState>) -> Result<Json<Vec<Tag>>, ApiError> {
    let tags = s.meta.list_tags().await.map_err(ApiError::from_db)?;
    Ok(Json(tags))
}

async fn list_file_tags(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(file_id): Path<String>,
) -> Result<Json<Vec<Tag>>, ApiError> {
    let file_id = parse_file_id(&file_id)?;
    require_permission(&*s.authz, &ctx, &Target::File(file_id), PermissionMask::VIEW).await?;
    let tags = s.meta.list_tags_of(&file_id).await.map_err(ApiError::from_db)?;
    Ok(Json(tags))
}

async fn detach_tag(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path((file_id, tag_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let file_id = parse_file_id(&file_id)?;
    require_permission(
        &*s.authz,
        &ctx,
        &Target::File(file_id),
        PermissionMask::WRITE,
    )
    .await?;
    let tag_id = uuid::Uuid::parse_str(&tag_id)
        .map(TagId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("tag_id: {e}")))?;
    s.meta
        .detach_tag(&file_id, &tag_id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ByTagsQuery {
    /// カンマ区切り（タグ名 or タグ UUID）
    tags: String,
}

async fn list_files_by_tags(
    State(s): State<ApiState>,
    Query(q): Query<ByTagsQuery>,
) -> Result<Json<Vec<FileMeta>>, ApiError> {
    let mut tag_ids = Vec::new();
    for spec in q.tags.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        // UUID として解釈できればそのまま、できなければ名前として lookup。
        if let Ok(u) = uuid::Uuid::parse_str(spec) {
            tag_ids.push(TagId::from_uuid(u));
        } else {
            match s.meta.get_tag_by_name(spec).await.map_err(ApiError::from_db)? {
                Some(t) => tag_ids.push(t.id),
                None => return Ok(Json(vec![])), // 存在しないタグ → 空集合
            }
        }
    }
    let files = s
        .meta
        .list_files_by_tags(&tag_ids)
        .await
        .map_err(ApiError::from_db)?;
    Ok(Json(files))
}

async fn upsert_tag(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Json(input): Json<TagInput>,
) -> Result<Json<TagCreated>, ApiError> {
    require_authenticated(&ctx).await?;
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
    AuthCtx(ctx): AuthCtx,
    Path(file_id): Path<String>,
    Json(input): Json<AttachTagInput>,
) -> Result<StatusCode, ApiError> {
    let file_id = parse_file_id(&file_id)?;
    require_permission(
        &*s.authz,
        &ctx,
        &Target::File(file_id),
        PermissionMask::WRITE,
    )
    .await?;
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
// Series
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateSeriesInput {
    name: String,
    description: Option<String>,
}

async fn list_series(State(s): State<ApiState>) -> Result<Json<Vec<Series>>, ApiError> {
    let list = s.meta.list_series().await.map_err(ApiError::from_db)?;
    Ok(Json(list))
}

async fn create_series(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Json(input): Json<CreateSeriesInput>,
) -> Result<(StatusCode, Json<Series>), ApiError> {
    require_authenticated(&ctx).await?;
    let series = Series {
        id: SeriesId::new(),
        name: input.name,
        description: input.description,
    };
    let id = s.meta.upsert_series(&series).await.map_err(ApiError::from_db)?;
    let saved = Series { id, ..series };
    Ok((StatusCode::CREATED, Json(saved)))
}

async fn get_series(
    State(s): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Series>, ApiError> {
    let id = uuid::Uuid::parse_str(&id)
        .map(SeriesId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("series id: {e}")))?;
    let series = s
        .meta
        .get_series(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(series))
}

#[derive(Deserialize)]
struct AddMemberInput {
    file_id: String,
    /// 未指定なら末尾追加（最大 +1.0）
    order_index: Option<f64>,
}

async fn list_series_members(
    State(s): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<SeriesMember>>, ApiError> {
    let id = uuid::Uuid::parse_str(&id)
        .map(SeriesId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("series id: {e}")))?;
    let members = s
        .meta
        .list_series_members(&id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(Json(members))
}

async fn add_series_member(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Json(input): Json<AddMemberInput>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let series_id = uuid::Uuid::parse_str(&id)
        .map(SeriesId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("series id: {e}")))?;
    let file_id = parse_file_id(&input.file_id)?;
    require_permission(
        &*s.authz,
        &ctx,
        &Target::File(file_id),
        PermissionMask::WRITE,
    )
    .await?;
    let order_index = match input.order_index {
        Some(v) => v,
        None => {
            let existing = s
                .meta
                .list_series_members(&series_id)
                .await
                .map_err(ApiError::from_db)?;
            existing.last().map(|m| m.order_index + 1.0).unwrap_or(10.0)
        }
    };
    s.meta
        .add_to_series(&SeriesMember {
            series_id,
            file_id,
            order_index,
        })
        .await
        .map_err(ApiError::from_db)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_series_member(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path((series_id, file_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let series_id = uuid::Uuid::parse_str(&series_id)
        .map(SeriesId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("series id: {e}")))?;
    let file_id = parse_file_id(&file_id)?;
    require_permission(
        &*s.authz,
        &ctx,
        &Target::File(file_id),
        PermissionMask::WRITE,
    )
    .await?;
    s.meta
        .remove_from_series(&series_id, &file_id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Saved Queries (Shareable Path)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateQueryInput {
    name: String,
    description: Option<String>,
    #[serde(default)]
    tags_and: Vec<String>,
    #[serde(default)]
    tags_not: Vec<String>,
    /// 期限秒数（now + N 秒）。
    expires_in_secs: Option<i64>,
}

async fn list_saved_queries(
    State(s): State<ApiState>,
) -> Result<Json<Vec<SavedQuery>>, ApiError> {
    let list = s.meta.list_saved_queries().await.map_err(ApiError::from_db)?;
    Ok(Json(list))
}

async fn create_saved_query(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Json(input): Json<CreateQueryInput>,
) -> Result<(StatusCode, Json<SavedQuery>), ApiError> {
    require_authenticated(&ctx).await?;
    let now = time::OffsetDateTime::now_utc();
    let created_by = match &ctx {
        AuthContext::User { user, .. } => Some(user.id),
        _ => None,
    };
    let expires_at = input
        .expires_in_secs
        .map(|s| now + time::Duration::seconds(s));
    let q = SavedQuery {
        id: SavedQueryId::new(),
        name: input.name,
        query: QueryDef {
            tags_and: input.tags_and,
            tags_not: input.tags_not,
        },
        description: input.description,
        created_by,
        created_at: now,
        expires_at,
    };
    let id = s.meta.upsert_saved_query(&q).await.map_err(ApiError::from_db)?;
    let saved = SavedQuery { id, ..q };
    Ok((StatusCode::CREATED, Json(saved)))
}

async fn get_saved_query(
    State(s): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<SavedQuery>, ApiError> {
    let id = uuid::Uuid::parse_str(&id)
        .map(SavedQueryId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("query id: {e}")))?;
    let q = s
        .meta
        .get_saved_query(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(q))
}

async fn delete_saved_query(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let id = uuid::Uuid::parse_str(&id)
        .map(SavedQueryId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("query id: {e}")))?;
    s.meta
        .delete_saved_query(&id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn query_files(
    State(s): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<FileMeta>>, ApiError> {
    let id = uuid::Uuid::parse_str(&id)
        .map(SavedQueryId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("query id: {e}")))?;
    let q = s
        .meta
        .get_saved_query(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    let files = resolve_query(&*s.meta, &q.query).await?;
    Ok(Json(files))
}

/// 共通ヘルパ: SavedQuery の定義を解決して FileMeta 一覧を返す。
pub async fn resolve_query(
    meta: &dyn yozist_db::MetaStore,
    q: &QueryDef,
) -> Result<Vec<FileMeta>, ApiError> {
    // タグ名 → TagId 解決
    let mut and_ids = Vec::with_capacity(q.tags_and.len());
    for name in &q.tags_and {
        let tag = meta.get_tag_by_name(name).await.map_err(ApiError::from_db)?;
        match tag {
            Some(t) => and_ids.push(t.id),
            None => return Ok(vec![]), // 存在しないタグ → 空
        }
    }
    let mut not_ids = Vec::with_capacity(q.tags_not.len());
    for name in &q.tags_not {
        if let Some(t) = meta.get_tag_by_name(name).await.map_err(ApiError::from_db)? {
            not_ids.push(t.id);
        }
    }

    let candidates = if and_ids.is_empty() {
        meta.list_files(1000, 0).await.map_err(ApiError::from_db)?
    } else {
        meta.list_files_by_tags(&and_ids)
            .await
            .map_err(ApiError::from_db)?
    };

    // tags_not で除外
    if not_ids.is_empty() {
        return Ok(candidates);
    }
    let mut out = Vec::new();
    for f in candidates {
        let tags = meta.list_tags_of(&f.id).await.map_err(ApiError::from_db)?;
        let has_excluded = tags.iter().any(|t| not_ids.contains(&t.id));
        if !has_excluded {
            out.push(f);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ACL
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AclRuleInput {
    /// "user:<uuid>" / "group:<uuid>"
    subject: String,
    /// "file:<uuid>" / "tag:<uuid>" / "series:<uuid>" / "share:<name>"
    target: String,
    /// bit flags: view=1 read=2 write=4 admin=8
    mask: u32,
    allow: bool,
    #[serde(default)]
    priority: i32,
}

#[derive(Serialize)]
struct AclRuleCreated {
    id: String,
}

async fn add_acl_rule(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Json(input): Json<AclRuleInput>,
) -> Result<Json<AclRuleCreated>, ApiError> {
    require_authenticated(&ctx).await?;
    // ACL 自体は admin 権限が必要（v1 では bootstrap 中はすべて許可）。
    // System コンテキストか、現状 rule が 0 件なら作成可能。
    let (stype, sid) = split_colon(&input.subject)?;
    let subject = match stype {
        "user" => Subject::User(parse_uuid_id(sid)?),
        "group" => Subject::Group(parse_uuid_id(sid)?),
        other => return Err(ApiError::BadRequest(format!("subject type: {other}"))),
    };
    let (ttype, tref) = split_colon(&input.target)?;
    let target = match ttype {
        "file" => Target::File(parse_uuid_id(tref)?),
        "tag" => Target::Tag(parse_uuid_id(tref)?),
        "series" => Target::Series(parse_uuid_id(tref)?),
        "share" => Target::Share(tref.to_string()),
        other => return Err(ApiError::BadRequest(format!("target type: {other}"))),
    };
    let mask = PermissionMask::from_bits_truncate(input.mask);
    let perm = Permission {
        subject,
        target,
        mask,
        allow: input.allow,
        priority: input.priority,
        expires_at: None,
    };
    // TODO: admin 権限の本実装（現状は authenticated を要求）。
    let rule_id = s
        .acl_admin
        .add_rule(&perm)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(AclRuleCreated {
        id: rule_id.to_string(),
    }))
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

#[derive(Serialize)]
struct MeResponse {
    user: Option<yozist_auth::User>,
    anonymous: bool,
}

async fn me(AuthCtx(ctx): AuthCtx) -> Json<MeResponse> {
    match ctx {
        AuthContext::User { user, .. } => Json(MeResponse {
            user: Some(user),
            anonymous: false,
        }),
        _ => Json(MeResponse {
            user: None,
            anonymous: true,
        }),
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

fn split_colon(s: &str) -> Result<(&str, &str), ApiError> {
    s.split_once(':')
        .ok_or_else(|| ApiError::BadRequest(format!("expected '<type>:<value>': {s}")))
}

fn parse_uuid_id<T: yozist_idtype::FromUuid>(s: &str) -> Result<T, ApiError> {
    uuid::Uuid::parse_str(s)
        .map(T::from_uuid_)
        .map_err(|e| ApiError::BadRequest(format!("uuid: {e}")))
}

mod yozist_idtype {
    use uuid::Uuid;
    pub trait FromUuid {
        fn from_uuid_(u: Uuid) -> Self;
    }
    impl FromUuid for yozist_core::UserId {
        fn from_uuid_(u: Uuid) -> Self {
            Self::from_uuid(u)
        }
    }
    impl FromUuid for yozist_core::GroupId {
        fn from_uuid_(u: Uuid) -> Self {
            Self::from_uuid(u)
        }
    }
    impl FromUuid for yozist_core::FileId {
        fn from_uuid_(u: Uuid) -> Self {
            Self::from_uuid(u)
        }
    }
    impl FromUuid for yozist_core::TagId {
        fn from_uuid_(u: Uuid) -> Self {
            Self::from_uuid(u)
        }
    }
    impl FromUuid for yozist_core::SeriesId {
        fn from_uuid_(u: Uuid) -> Self {
            Self::from_uuid(u)
        }
    }
}

#[derive(Debug)]
pub enum ApiError {
    NotFound,
    BadRequest(String),
    Unauthorized,
    Forbidden,
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
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".to_string()),
            ApiError::Conflict => (StatusCode::CONFLICT, "conflict".to_string()),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yozist_auth::DbAuthorizer;
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
            yozist_auth::SqliteAuthService::new(pool.clone(), b"test".to_vec()),
        );
        let db_authz = Arc::new(DbAuthorizer::new(pool));
        let authz: Arc<dyn Authorizer> = db_authz.clone();
        (
            ApiState {
                meta,
                engine,
                auth,
                authz,
                acl_admin: db_authz,
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
    async fn create_file_without_auth_is_unauthorized() {
        let (state, _td) = make_state().await;
        let app = router(state);
        let resp = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/api/files?name=a.txt")
                .body(axum::body::Body::from("hi"))
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
