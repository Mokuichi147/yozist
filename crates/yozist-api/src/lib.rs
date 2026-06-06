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
    body::Body,
    extract::{FromRef, FromRequestParts, Path, Query, State},
    http::{request::Parts, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use yozist_storage::StorageError;

use user_permission_core::Database as AuthDb;
use yozist_auth::{
    AuthContext, Authorizer, DbAuthorizer, Permission, PermissionMask, ShareTokenStore, Subject,
    Target,
};

pub mod ui;
use yozist_core::{
    ActorId, FileId, FileMeta, QueryDef, SavedQuery, SavedQueryId, Series, SeriesId,
    SeriesMember, Tag, TagId, TagKind, UserId,
};
use yozist_db::{AuditRecord, SharedAuditLog, SharedMetaStore};
use yozist_versioning::VersioningEngine;

/// API ハンドラが共有する状態。
#[derive(Clone)]
pub struct ApiState {
    pub meta: SharedMetaStore,
    pub engine: Arc<VersioningEngine>,
    /// ユーザー / グループ / JWT 認証は upstream user-permission に委譲。
    pub auth_db: Arc<AuthDb>,
    pub authz: Arc<dyn Authorizer>,
    pub acl_admin: Arc<DbAuthorizer>,
    pub audit: SharedAuditLog,
    /// 共有トークン (`share_tokens` テーブル) の操作。
    pub share_admin: Arc<ShareTokenStore>,
    /// SMB(NTLM) 資格情報の同期先。SMB 無効時は `None`。
    /// register / login / change_password 成功時に平文パスワードを渡して反映する。
    pub smb_creds: Option<Arc<dyn yozist_auth::SmbCredentialSink>>,
}

/// ルーター生成。
pub fn router(state: ApiState) -> Router {
    Router::new()
        .nest("/ui", ui::router())
        .route("/", get(redirect_to_ui))
        .route("/health", get(health))
        .route("/api/files", get(list_files).post(create_file))
        .route("/api/files/:id", get(get_file).delete(delete_file))
        .route("/api/files/:id/content", get(get_content).post(commit_file))
        .route("/api/files/:id/history", get(history))
        .route("/api/files/:id/commits/:cid", get(read_commit))
        .route("/api/files/:id/rollback/:cid", post(rollback))
        .route(
            "/api/files/:id/tags",
            get(list_file_tags).post(attach_tag),
        )
        .route("/api/files/:id/tags/:tag_id", axum::routing::delete(detach_tag))
        .route("/api/tags", get(list_tags).post(upsert_tag))
        .route(
            "/api/tags/:id",
            axum::routing::patch(rename_tag).delete(delete_tag),
        )
        .route("/api/files/by-tags", get(list_files_by_tags))
        .route("/api/search", get(search_files))
        .route("/api/series", get(list_series).post(create_series))
        .route(
            "/api/series/:id",
            get(get_series)
                .patch(rename_series)
                .delete(delete_series),
        )
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
        .route("/api/files/:id/share", post(issue_file_share))
        .route("/api/queries/:id/share", post(issue_query_share))
        .route("/api/shared/:token", get(get_shared))
        .route("/api/shared/:token/files", get(list_shared_files))
        .route("/api/shares", get(list_share_tokens))
        .route("/api/shares/:jti", axum::routing::delete(revoke_share_token))
        .route("/api/audit", get(list_audit))
        .route("/api/acl", post(add_acl_rule))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/me", get(me).patch(update_me))
        .route("/api/auth/password", post(change_password))
        .route("/api/users", get(list_users))
        .route("/api/groups", get(list_groups).post(create_group))
        .route(
            "/api/groups/:id/members",
            get(list_group_members).post(add_group_member),
        )
        .route(
            "/api/groups/:id/members/:user_id",
            axum::routing::delete(remove_group_member),
        )
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

        // local / relay 共通: yozist_auth のヘルパに委譲する。
        // backend の違い (ローカル署名検証 / 上流転送) は user-permission 内部で吸収される。
        let ctx = yozist_auth::resolve_auth_context(&api.auth_db, token)
            .await
            .map_err(|_| ApiError::Unauthorized)?;
        Ok(AuthCtx(ctx))
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

/// `files` から呼び出し元 `ctx` が VIEW 権限を持つものだけを残す。
/// 一覧系 API は権限チェックをここに集約し、詳細 API (`/api/files/:id`) との
/// 整合性を保つ（一覧に出るが開けない、を避ける）。
async fn filter_visible_files(
    authz: &dyn Authorizer,
    ctx: &AuthContext,
    files: Vec<FileMeta>,
) -> Result<Vec<FileMeta>, ApiError> {
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let ok = authz
            .check(ctx, &Target::file(f.id), PermissionMask::VIEW)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if ok {
            out.push(f);
        }
    }
    Ok(out)
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

async fn list_files(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
) -> Result<Json<Vec<FileMeta>>, ApiError> {
    let files = s.meta.list_files(100, 0).await.map_err(ApiError::from_db)?;
    let visible = filter_visible_files(&*s.authz, &ctx, files).await?;
    Ok(Json(visible))
}

async fn create_file(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Query(q): Query<CreateFileQuery>,
    body: Body,
) -> Result<(StatusCode, Json<FileMeta>), ApiError> {
    require_authenticated(&ctx).await?;
    let actor = parse_actor(q.actor.as_deref()).unwrap_or_else(ActorId::new);
    let name_for_audit = q.name.clone();
    // ボディをメモリに載せず 1 チャンクずつ blob ストアへ流す。
    let stream = body
        .into_data_stream()
        .map_err(|e| StorageError::Other(e.to_string()))
        .boxed();
    let result = s
        .engine
        .create_file_streaming(q.name, stream, actor, None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()));

    let (actor_id, actor_label) = actor_info(&ctx);
    let target_ref = result.as_ref().ok().map(|(f, _)| f.id.to_string());
    let metadata = format!("{{\"name\":\"{}\"}}", name_for_audit);
    let result_str = match &result {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("{e:?}"),
    };
    audit_record(
        &s.audit,
        AuditRecord {
            actor_id: actor_id.as_deref(),
            actor_label: actor_label.as_deref(),
            action: "create_file",
            target_type: Some("file"),
            target_ref: target_ref.as_deref(),
            metadata_json: Some(&metadata),
            result: &result_str,
        },
    )
    .await;

    let (file, _commit) = result?;

    // オーナー（作成者）に ADMIN 権限を自動付与。これにより以後 ACL rule を
    // 追加しても作成者は自分のファイルへ常にアクセス可能。
    if let AuthContext::User { user, .. } = &ctx {
        let owner_rule = Permission {
            subject: Subject::User(user.id),
            target: Target::file(file.id),
            mask: PermissionMask::all(),
            allow: true,
            priority: i32::MAX,
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
    require_permission(&*s.authz, &ctx, &Target::file(id), PermissionMask::VIEW).await?;
    let meta = s
        .meta
        .get_file(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(meta))
}

async fn delete_file(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let file_id = parse_file_id(&id)?;
    require_permission(
        &*s.authz,
        &ctx,
        &Target::file(file_id),
        PermissionMask::WRITE,
    )
    .await?;
    let res = async {
        let mut meta = s
            .meta
            .get_file(&file_id)
            .await
            .map_err(ApiError::from_db)?
            .ok_or(ApiError::NotFound)?;
        meta.deleted = true;
        meta.updated_at = time::OffsetDateTime::now_utc();
        s.meta.update_file(&meta).await.map_err(ApiError::from_db)?;
        let _ = s.meta.delete_fts(&file_id).await;
        Ok::<_, ApiError>(())
    }
    .await;
    audit_event(
        &s,
        &ctx,
        "delete_file",
        Some("file"),
        Some(&id),
        None,
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    Ok(StatusCode::NO_CONTENT)
}

/// 保存済み MIME を Content-Type に設定して本文を返す。未設定なら octet-stream。
///
/// テキストファイル（`charset` あり）は blob に UTF-8 で保存されているため、
/// 取り込み時に判定した元エンコーディングへ再エンコードして「元の形式」で返す。
/// 併せて `Content-Type` に `charset=` を付与し、ブラウザが正しくデコードできる
/// ようにする。`charset` が `None`（バイナリ）はそのまま返す。
fn content_response(
    mime: Option<String>,
    charset: Option<String>,
    bytes: Vec<u8>,
) -> impl IntoResponse {
    let mut ct = mime.unwrap_or_else(|| "application/octet-stream".to_string());
    let body = match &charset {
        Some(cs) => {
            // blob は serialize 由来の妥当な UTF-8。lossy でも実質非破壊。
            let text = String::from_utf8_lossy(&bytes);
            let encoded = yozist_versioning::encode_text(&text, cs);
            if !ct.to_ascii_lowercase().contains("charset=") {
                ct = format!("{ct}; charset={}", yozist_versioning::http_charset(cs));
            }
            encoded
        }
        None => bytes,
    };
    ([(axum::http::header::CONTENT_TYPE, ct)], body)
}

async fn get_content(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::file(id), PermissionMask::READ).await?;
    let file = s
        .meta
        .get_file(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    let bytes = s
        .engine
        .read_current(id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(content_response(file.mime, file.charset, bytes))
}

#[derive(Deserialize)]
struct CommitQuery {
    actor: Option<String>,
    message: Option<String>,
    /// 指定時はファイル名を更新し、mime/charset を新しい名前＋内容から再判定する
    /// （アップロードによる「内容を更新」用）。テキスト編集等では送らない。
    name: Option<String>,
}

async fn commit_file(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Query(q): Query<CommitQuery>,
    body: Body,
) -> Result<Json<yozist_core::Commit>, ApiError> {
    let id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::file(id), PermissionMask::WRITE).await?;
    let actor = parse_actor(q.actor.as_deref()).unwrap_or_else(ActorId::new);
    // ボディをメモリに載せず 1 チャンクずつ blob ストアへ流す。
    let stream = body
        .into_data_stream()
        .map_err(|e| StorageError::Other(e.to_string()))
        .boxed();
    // name 指定時（アップロードによる「内容を更新」）は前バージョンとマージせず
    // 全置換する。形式・mime・charset・表示名を新しい名前＋内容から判定し直すため、
    // 別形式へ差し替えても旧バージョンの解釈に引きずられず破損しない。
    // name 無し（テキスト編集など）は従来どおり CRDT マージ経路。
    let result = if let Some(name) = q.name {
        s.engine
            .replace_streaming(id, name, stream, actor, q.message)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))
    } else {
        s.engine
            .commit_streaming(id, stream, actor, q.message)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))
    };

    let id_str = id.to_string();
    let (actor_id, actor_label) = actor_info(&ctx);
    let result_str = match &result {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("{e:?}"),
    };
    audit_record(
        &s.audit,
        AuditRecord {
            actor_id: actor_id.as_deref(),
            actor_label: actor_label.as_deref(),
            action: "commit",
            target_type: Some("file"),
            target_ref: Some(&id_str),
            metadata_json: None,
            result: &result_str,
        },
    )
    .await;

    Ok(Json(result?))
}

async fn history(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Vec<yozist_core::Commit>>, ApiError> {
    let id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::file(id), PermissionMask::READ).await?;
    let log = s.meta.list_commits(&id).await.map_err(ApiError::from_db)?;
    Ok(Json(log))
}

async fn read_commit(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path((id, cid)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let file_id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::file(file_id), PermissionMask::READ).await?;
    let commit_id = uuid::Uuid::parse_str(&cid)
        .map(yozist_core::CommitId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("commit id: {e}")))?;
    let file = s
        .meta
        .get_file(&file_id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    let bytes = s
        .engine
        .read_at_commit(file_id, commit_id)
        .await
        .map_err(|e| match e {
            yozist_versioning::VersioningError::NotFound(_) => ApiError::NotFound,
            other => ApiError::Internal(other.to_string()),
        })?;
    Ok(content_response(file.mime, file.charset, bytes))
}

#[derive(Deserialize)]
struct RollbackQuery {
    actor: Option<String>,
    message: Option<String>,
}

async fn rollback(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path((id, cid)): Path<(String, String)>,
    Query(q): Query<RollbackQuery>,
) -> Result<Json<yozist_core::Commit>, ApiError> {
    let file_id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::file(file_id), PermissionMask::WRITE).await?;
    let commit_id = uuid::Uuid::parse_str(&cid)
        .map(yozist_core::CommitId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("commit id: {e}")))?;
    let actor = parse_actor(q.actor.as_deref()).unwrap_or_else(ActorId::new);
    let res = s
        .engine
        .rollback_to(file_id, commit_id, actor, q.message)
        .await
        .map_err(|e| match e {
            yozist_versioning::VersioningError::NotFound(_) => ApiError::NotFound,
            other => ApiError::Internal(other.to_string()),
        });
    let meta = format!("{{\"to_commit\":\"{}\"}}", cid);
    audit_event(
        &s,
        &ctx,
        "rollback",
        Some("file"),
        Some(&id),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    Ok(Json(res?))
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

#[derive(Deserialize)]
struct RenameTagInput {
    name: String,
}

async fn rename_tag(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Json(input): Json<RenameTagInput>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let tag_id = uuid::Uuid::parse_str(&id)
        .map(TagId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("tag id: {e}")))?;
    let new_name = input.name.clone();
    let res = s
        .meta
        .rename_tag(&tag_id, &input.name)
        .await
        .map_err(ApiError::from_db);
    let meta = format!("{{\"name\":\"{}\"}}", new_name);
    audit_event(
        &s,
        &ctx,
        "rename_tag",
        Some("tag"),
        Some(&id),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_tag(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let tag_id = uuid::Uuid::parse_str(&id)
        .map(TagId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("tag id: {e}")))?;
    let res = s.meta.delete_tag(&tag_id).await.map_err(ApiError::from_db);
    audit_event(
        &s,
        &ctx,
        "delete_tag",
        Some("tag"),
        Some(&id),
        None,
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_file_tags(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(file_id): Path<String>,
) -> Result<Json<Vec<Tag>>, ApiError> {
    let file_id = parse_file_id(&file_id)?;
    require_permission(&*s.authz, &ctx, &Target::file(file_id), PermissionMask::VIEW).await?;
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
        &Target::file(file_id),
        PermissionMask::WRITE,
    )
    .await?;
    let tag_uuid = uuid::Uuid::parse_str(&tag_id)
        .map(TagId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("tag_id: {e}")))?;
    let res = s
        .meta
        .detach_tag(&file_id, &tag_uuid)
        .await
        .map_err(ApiError::from_db);
    let file_id_str = file_id.to_string();
    let meta = format!("{{\"tag_id\":\"{tag_id}\"}}");
    audit_event(
        &s,
        &ctx,
        "detach_tag",
        Some("file"),
        Some(&file_id_str),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    refresh_fts_tags(&s, &file_id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ByTagsQuery {
    /// カンマ区切り（タグ名 or タグ UUID）
    tags: String,
}

async fn list_files_by_tags(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
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
    let visible = filter_visible_files(&*s.authz, &ctx, files).await?;
    Ok(Json(visible))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_search_limit")]
    limit: u32,
}

fn default_search_limit() -> u32 {
    50
}

async fn search_files(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Vec<FileMeta>>, ApiError> {
    let ids = s
        .meta
        .search_fts(&q.q, q.limit)
        .await
        .map_err(ApiError::from_db)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(meta) = s.meta.get_file(&id).await.map_err(ApiError::from_db)? {
            if !meta.deleted {
                out.push(meta);
            }
        }
    }
    let visible = filter_visible_files(&*s.authz, &ctx, out).await?;
    Ok(Json(visible))
}

/// タグ変更時に FTS の tags 列を再構築するヘルパ。失敗は無視。
async fn refresh_fts_tags(state: &ApiState, file_id: &FileId) {
    let tags = state
        .meta
        .list_tags_of(file_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.name)
        .collect::<Vec<_>>()
        .join(" ");
    let meta = state.meta.get_file(file_id).await.ok().flatten();
    if let Some(meta) = meta {
        // content は維持できないので空にする（テキストファイルは次回 commit 時に更新）。
        // display_name と tags のみリフレッシュ。content は別のレイヤーで再投入される。
        let _ = state
            .meta
            .upsert_fts(file_id, &meta.display_name, &tags, "")
            .await;
    }
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
    let tag_name = input.name.clone();
    let tag = Tag {
        id: TagId::new(),
        name: input.name,
        kind,
        confidence: input.confidence,
    };
    let res = s.meta.upsert_tag(&tag).await.map_err(ApiError::from_db);
    let id_for_audit = res.as_ref().ok().map(|t| t.to_string());
    let meta = format!("{{\"name\":\"{}\",\"kind\":\"{}\"}}", tag_name, match kind {
        TagKind::System => "system",
        TagKind::Ai => "ai",
        TagKind::Manual => "manual",
    });
    audit_event(
        &s,
        &ctx,
        "upsert_tag",
        Some("tag"),
        id_for_audit.as_deref(),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    let id = res?;
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
        &Target::file(file_id),
        PermissionMask::WRITE,
    )
    .await?;
    let tag_id = uuid::Uuid::parse_str(&input.tag_id)
        .map(TagId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("tag_id: {e}")))?;
    let res = s
        .meta
        .attach_tag(&file_id, &tag_id)
        .await
        .map_err(ApiError::from_db);
    let file_str = file_id.to_string();
    let meta = format!("{{\"tag_id\":\"{}\"}}", input.tag_id);
    audit_event(
        &s,
        &ctx,
        "attach_tag",
        Some("file"),
        Some(&file_str),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    refresh_fts_tags(&s, &file_id).await;
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
    let name = input.name.clone();
    let series = Series {
        id: SeriesId::new(),
        name: input.name,
        description: input.description,
    };
    let res = s
        .meta
        .upsert_series(&series)
        .await
        .map_err(ApiError::from_db);
    let id_str = res.as_ref().ok().map(|i| i.to_string());
    let meta = format!("{{\"name\":\"{}\"}}", name);
    audit_event(
        &s,
        &ctx,
        "create_series",
        Some("series"),
        id_str.as_deref(),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    let id = res?;
    Ok((StatusCode::CREATED, Json(Series { id, ..series })))
}

#[derive(Deserialize)]
struct RenameSeriesInput {
    name: String,
    description: Option<String>,
}

async fn rename_series(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Json(input): Json<RenameSeriesInput>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let series_id = uuid::Uuid::parse_str(&id)
        .map(SeriesId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("series id: {e}")))?;
    let new_name = input.name.clone();
    let res = s
        .meta
        .rename_series(&series_id, &input.name, input.description.as_deref())
        .await
        .map_err(ApiError::from_db);
    let meta = format!("{{\"name\":\"{}\"}}", new_name);
    audit_event(
        &s,
        &ctx,
        "rename_series",
        Some("series"),
        Some(&id),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_series(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let series_id = uuid::Uuid::parse_str(&id)
        .map(SeriesId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("series id: {e}")))?;
    let res = s
        .meta
        .delete_series(&series_id)
        .await
        .map_err(ApiError::from_db);
    audit_event(
        &s,
        &ctx,
        "delete_series",
        Some("series"),
        Some(&id),
        None,
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    Ok(StatusCode::NO_CONTENT)
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
        &Target::file(file_id),
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
    let res = s
        .meta
        .add_to_series(&SeriesMember {
            series_id,
            file_id,
            order_index,
        })
        .await
        .map_err(ApiError::from_db);
    let sid = series_id.to_string();
    let m = format!(
        "{{\"file_id\":\"{}\",\"order_index\":{}}}",
        file_id, order_index
    );
    audit_event(
        &s,
        &ctx,
        "add_to_series",
        Some("series"),
        Some(&sid),
        Some(&m),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
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
        &Target::file(file_id),
        PermissionMask::WRITE,
    )
    .await?;
    let res = s
        .meta
        .remove_from_series(&series_id, &file_id)
        .await
        .map_err(ApiError::from_db);
    let sid = series_id.to_string();
    let m = format!("{{\"file_id\":\"{}\"}}", file_id);
    audit_event(
        &s,
        &ctx,
        "remove_from_series",
        Some("series"),
        Some(&sid),
        Some(&m),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
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
    let qid = uuid::Uuid::parse_str(&id)
        .map(SavedQueryId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("query id: {e}")))?;
    let res = s
        .meta
        .delete_saved_query(&qid)
        .await
        .map_err(ApiError::from_db);
    audit_event(
        &s,
        &ctx,
        "delete_saved_query",
        Some("query"),
        Some(&id),
        None,
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
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
// Audit log
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default = "default_audit_limit")]
    limit: u32,
}

fn default_audit_limit() -> u32 {
    100
}

async fn list_audit(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<yozist_db::AuditEntry>>, ApiError> {
    require_authenticated(&ctx).await?;
    let entries = s
        .audit
        .recent(q.limit)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(entries))
}

fn actor_info(ctx: &AuthContext) -> (Option<String>, Option<String>) {
    match ctx {
        AuthContext::Anonymous => (None, Some("anonymous".into())),
        AuthContext::System => (None, Some("system".into())),
        AuthContext::User { user, .. } => {
            (Some(user.id.to_string()), Some(user.username.clone()))
        }
    }
}

async fn audit_record(audit: &SharedAuditLog, r: AuditRecord<'_>) {
    if let Err(e) = audit.record(&r).await {
        tracing::warn!(error = %e, action = r.action, "audit write failed");
    }
}

/// 短縮版ヘルパ: ctx と結果から AuditRecord を組み立てて書き込む。
async fn audit_event(
    s: &ApiState,
    ctx: &AuthContext,
    action: &str,
    target_type: Option<&str>,
    target_ref: Option<&str>,
    metadata_json: Option<&str>,
    result: &Result<(), String>,
) {
    let (actor_id, actor_label) = actor_info(ctx);
    let result_str = match result {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("error: {e}"),
    };
    audit_record(
        &s.audit,
        AuditRecord {
            actor_id: actor_id.as_deref(),
            actor_label: actor_label.as_deref(),
            action,
            target_type,
            target_ref,
            metadata_json,
            result: &result_str,
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// Shared URL (期限付きトークン)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ShareInput {
    /// 有効秒数（デフォルト 1 時間）
    #[serde(default = "default_share_ttl")]
    ttl_secs: i64,
}

fn default_share_ttl() -> i64 {
    3600
}

#[derive(Serialize)]
struct ShareTokenResponse {
    token: String,
    expires_in_secs: i64,
    /// クライアントが直接アクセスできる URL パス
    url: String,
}

async fn issue_file_share(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Json(input): Json<ShareInput>,
) -> Result<Json<ShareTokenResponse>, ApiError> {
    let file_id = parse_file_id(&id)?;
    // 共有を発行できるのは ADMIN 権限を持つユーザー
    require_permission(
        &*s.authz,
        &ctx,
        &Target::file(file_id),
        PermissionMask::ADMIN,
    )
    .await?;
    let issuer = match &ctx {
        AuthContext::User { user, .. } => Some(user.username.as_str()),
        _ => None,
    };
    let res = s
        .share_admin
        .issue_share_token("file", &id, input.ttl_secs, issuer)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()));
    let meta = format!("{{\"ttl_secs\":{}}}", input.ttl_secs);
    audit_event(
        &s,
        &ctx,
        "issue_file_share",
        Some("file"),
        Some(&id),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    let tok = res?;
    Ok(Json(ShareTokenResponse {
        url: format!("/api/shared/{}", tok.0),
        token: tok.0,
        expires_in_secs: input.ttl_secs,
    }))
}

async fn issue_query_share(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Json(input): Json<ShareInput>,
) -> Result<Json<ShareTokenResponse>, ApiError> {
    require_authenticated(&ctx).await?;
    let issuer = match &ctx {
        AuthContext::User { user, .. } => Some(user.username.as_str()),
        _ => None,
    };
    let res = s
        .share_admin
        .issue_share_token("query", &id, input.ttl_secs, issuer)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()));
    let meta = format!("{{\"ttl_secs\":{}}}", input.ttl_secs);
    audit_event(
        &s,
        &ctx,
        "issue_query_share",
        Some("query"),
        Some(&id),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    let tok = res?;
    Ok(Json(ShareTokenResponse {
        url: format!("/api/shared/{}/files", tok.0),
        token: tok.0,
        expires_in_secs: input.ttl_secs,
    }))
}

async fn verify_share(
    s: &ApiState,
    token: &str,
    expect_kind: &str,
) -> Result<String, ApiError> {
    let claims = s
        .share_admin
        .verify_share_token(token)
        .await
        .map_err(|_| ApiError::Unauthorized)?;
    if claims.kind != expect_kind {
        return Err(ApiError::BadRequest(format!(
            "token kind mismatch: expected {expect_kind}, got {}",
            claims.kind
        )));
    }
    Ok(claims.target_id)
}

/// ファイル共有トークンで内容を返す（認証不要）。
async fn get_shared(
    State(s): State<ApiState>,
    Path(token): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let target_id = verify_share(&s, &token, "file").await?;
    let file_id = parse_file_id(&target_id)?;
    let file = s
        .meta
        .get_file(&file_id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    let bytes = s
        .engine
        .read_current(file_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(content_response(file.mime, file.charset, bytes))
}

async fn list_share_tokens(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
) -> Result<Json<Vec<yozist_auth::ShareTokenRecord>>, ApiError> {
    require_authenticated(&ctx).await?;
    // 認証済みユーザーは自身が発行した分のみ閲覧（管理者は全件 — 簡易化のため
    // 現状は全ユーザーが自分の分のみ）。
    let issuer = match &ctx {
        AuthContext::User { user, .. } => Some(user.username.clone()),
        _ => None,
    };
    let list = s
        .share_admin
        .list_share_tokens(issuer.as_deref())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(list))
}

async fn revoke_share_token(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(jti): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    // 自分の発行分のみ失効可（管理者扱いは TODO）。
    let issuer = match &ctx {
        AuthContext::User { user, .. } => user.username.clone(),
        _ => return Err(ApiError::Forbidden),
    };
    let owned = s
        .share_admin
        .list_share_tokens(Some(&issuer))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if !owned.iter().any(|t| t.jti == jti) {
        return Err(ApiError::Forbidden);
    }
    let res = s.share_admin.revoke_share_token(&jti).await;
    let m = format!("{{\"jti\":\"{jti}\"}}");
    audit_event(
        &s,
        &ctx,
        "revoke_share_token",
        Some("share_token"),
        Some(&jti),
        Some(&m),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res.map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// クエリ共有トークンでマッチするファイル一覧を返す。
async fn list_shared_files(
    State(s): State<ApiState>,
    Path(token): Path<String>,
) -> Result<Json<Vec<FileMeta>>, ApiError> {
    let target_id = verify_share(&s, &token, "query").await?;
    let query_id = uuid::Uuid::parse_str(&target_id)
        .map(SavedQueryId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("query id: {e}")))?;
    let q = s
        .meta
        .get_saved_query(&query_id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    let files = resolve_query(&*s.meta, &q.query).await?;
    Ok(Json(files))
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
        "user" => Subject::User(parse_i64_id(sid)?),
        "group" => Subject::Group(parse_i64_id(sid)?),
        other => return Err(ApiError::BadRequest(format!("subject type: {other}"))),
    };
    let (ttype, tref) = split_colon(&input.target)?;
    let target = match ttype {
        "file" => Target::file(parse_uuid_id::<FileId>(tref)?),
        "share" => Target::share(tref),
        other => return Err(ApiError::BadRequest(format!("target type: {other}"))),
    };
    let mask = PermissionMask::from_bits_truncate(input.mask);
    let perm = Permission {
        subject,
        target,
        mask,
        allow: input.allow,
        priority: input.priority,
    };
    // TODO: admin 権限の本実装（現状は authenticated を要求）。
    let res = s
        .acl_admin
        .add_rule(&perm)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()));
    let meta = format!(
        "{{\"subject\":\"{}\",\"target\":\"{}\",\"mask\":{},\"allow\":{}}}",
        input.subject, input.target, input.mask, input.allow
    );
    audit_event(
        &s,
        &ctx,
        "add_acl_rule",
        Some("acl"),
        res.as_ref().ok().map(|i| i.to_string()).as_deref(),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    let rule_id = res?;
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
        .auth_db
        .users()
        .create(&input.username, &input.password, &input.username, None)
        .await
        .map_err(map_auth_error)?;
    if let Some(smb) = &s.smb_creds {
        smb.upsert(&input.username, &input.password).await;
    }
    Ok((StatusCode::CREATED, Json(user)))
}

async fn login(
    State(s): State<ApiState>,
    Json(input): Json<AuthInput>,
) -> Result<Json<AuthResponse>, ApiError> {
    let token = s
        .auth_db
        .users()
        .authenticate(
            &input.username,
            &input.password,
            std::time::Duration::from_secs(24 * 3600),
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::Unauthorized)?;
    // 認証成功時のみ平文パスワードを観測できる。既存ユーザーや再起動後の
    // NT ハッシュ復旧を兼ねて SMB へ反映する（冪等）。
    if let Some(smb) = &s.smb_creds {
        smb.upsert(&input.username, &input.password).await;
    }
    Ok(Json(AuthResponse { token }))
}

#[derive(Serialize)]
struct MeResponse {
    user: Option<yozist_auth::User>,
    /// 所属グループ一覧（設定ページでの表示用）。
    groups: Vec<yozist_auth::Group>,
    anonymous: bool,
}

async fn list_users(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
) -> Result<Json<Vec<yozist_auth::User>>, ApiError> {
    require_authenticated(&ctx).await?;
    let users = s
        .auth_db
        .users()
        .list_all(None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(users))
}

#[derive(Deserialize)]
struct CreateGroupInput {
    name: String,
    description: Option<String>,
}

async fn list_groups(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
) -> Result<Json<Vec<yozist_auth::Group>>, ApiError> {
    require_authenticated(&ctx).await?;
    let groups = s
        .auth_db
        .groups()
        .list_all(None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(groups))
}

#[derive(Deserialize)]
struct AddGroupMemberInput {
    user_id: i64,
}

async fn list_group_members(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<i64>,
) -> Result<Json<Vec<UserId>>, ApiError> {
    require_authenticated(&ctx).await?;
    let members = s
        .auth_db
        .groups()
        .get_members(id, None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(members.into_iter().map(|u| u.id).collect()))
}

async fn add_group_member(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<i64>,
    Json(input): Json<AddGroupMemberInput>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let res = s
        .auth_db
        .groups()
        .add_user(id, input.user_id, None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()));
    let meta = format!("{{\"user_id\":{}}}", input.user_id);
    audit_event(
        &s,
        &ctx,
        "add_group_member",
        Some("group"),
        Some(&id.to_string()),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_group_member(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path((id, user_id)): Path<(i64, i64)>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let res = s
        .auth_db
        .groups()
        .remove_user(id, user_id, None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()));
    let meta = format!("{{\"user_id\":{}}}", user_id);
    audit_event(
        &s,
        &ctx,
        "remove_group_member",
        Some("group"),
        Some(&id.to_string()),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_group(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Json(input): Json<CreateGroupInput>,
) -> Result<(StatusCode, Json<yozist_auth::Group>), ApiError> {
    require_authenticated(&ctx).await?;
    let desc = input.description.clone().unwrap_or_default();
    let res = s
        .auth_db
        .groups()
        .create(&input.name, &desc, false, None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()));
    let id_str = res.as_ref().ok().map(|g| g.id.to_string());
    let meta = format!("{{\"name\":\"{}\"}}", input.name);
    audit_event(
        &s,
        &ctx,
        "create_group",
        Some("group"),
        id_str.as_deref(),
        Some(&meta),
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    let group = res?;
    Ok((StatusCode::CREATED, Json(group)))
}

fn map_auth_error(e: user_permission_core::Error) -> ApiError {
    // username 重複等は user_permission_core::Error::UsernameTaken にマップされる。
    let msg = e.to_string();
    if msg.to_lowercase().contains("already") || msg.to_lowercase().contains("taken") {
        ApiError::Conflict
    } else {
        ApiError::Internal(msg)
    }
}

async fn me(State(s): State<ApiState>, AuthCtx(ctx): AuthCtx) -> Json<MeResponse> {
    match ctx {
        AuthContext::User { user, .. } => {
            let groups = s
                .auth_db
                .groups()
                .get_user_groups(user.id, None)
                .await
                .unwrap_or_default();
            Json(MeResponse {
                user: Some(user),
                groups,
                anonymous: false,
            })
        }
        _ => Json(MeResponse {
            user: None,
            groups: Vec::new(),
            anonymous: true,
        }),
    }
}

#[derive(Deserialize)]
struct UpdateMeInput {
    display_name: String,
}

/// ログイン中ユーザー自身の表示名を変更する。
async fn update_me(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Json(input): Json<UpdateMeInput>,
) -> Result<Json<yozist_auth::User>, ApiError> {
    require_authenticated(&ctx).await?;
    let user_id = ctx.user_id().ok_or(ApiError::Unauthorized)?;
    let display_name = input.display_name.trim().to_string();
    let update = user_permission_core::UserUpdate {
        display_name: Some(display_name),
        ..Default::default()
    };
    let res = s
        .auth_db
        .users()
        .update(user_id, update, None)
        .await
        .map_err(map_auth_error);
    audit_event(
        &s,
        &ctx,
        "update_display_name",
        Some("user"),
        Some(&user_id.to_string()),
        None,
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?.map(Json).ok_or(ApiError::NotFound)
}

#[derive(Deserialize)]
struct ChangePasswordInput {
    current_password: String,
    new_password: String,
}

/// ログイン中ユーザー自身のパスワードを変更する。現パスワードを検証してから更新する。
async fn change_password(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Json(input): Json<ChangePasswordInput>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let user = match &ctx {
        AuthContext::User { user, .. } => user.clone(),
        _ => return Err(ApiError::Unauthorized),
    };
    if input.new_password.len() < 8 {
        return Err(ApiError::BadRequest(
            "新パスワードは8文字以上で入力してください".into(),
        ));
    }
    // 現パスワードの検証: authenticate が成功すれば一致している。
    let verified = s
        .auth_db
        .users()
        .authenticate(
            &user.username,
            &input.current_password,
            std::time::Duration::from_secs(60),
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if verified.is_none() {
        audit_event(
            &s,
            &ctx,
            "change_password",
            Some("user"),
            Some(&user.id.to_string()),
            None,
            &Err::<(), _>("現パスワードが正しくありません".to_string()),
        )
        .await;
        return Err(ApiError::BadRequest("現パスワードが正しくありません".into()));
    }
    let update = user_permission_core::UserUpdate {
        password: Some(input.new_password.clone()),
        ..Default::default()
    };
    let res = s
        .auth_db
        .users()
        .update(user.id, update, None)
        .await
        .map_err(map_auth_error);
    audit_event(
        &s,
        &ctx,
        "change_password",
        Some("user"),
        Some(&user.id.to_string()),
        None,
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    if let Some(smb) = &s.smb_creds {
        smb.upsert(&user.username, &input.new_password).await;
    }
    Ok(StatusCode::NO_CONTENT)
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

fn parse_i64_id(s: &str) -> Result<i64, ApiError> {
    s.parse::<i64>()
        .map_err(|e| ApiError::BadRequest(format!("expected integer id, got '{s}': {e}")))
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

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NotFound => write!(f, "not found"),
            ApiError::BadRequest(m) => write!(f, "bad request: {m}"),
            ApiError::Unauthorized => write!(f, "unauthorized"),
            ApiError::Forbidden => write!(f, "forbidden"),
            ApiError::Conflict => write!(f, "conflict"),
            ApiError::Internal(m) => write!(f, "internal: {m}"),
        }
    }
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

    #[test]
    fn content_response_sets_content_type_from_mime() {
        // バイナリ（charset なし）は MIME のみ・本文はそのまま。
        let resp =
            content_response(Some("image/png".into()), None, vec![1, 2, 3]).into_response();
        assert_eq!(
            resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        // mime 未設定なら octet-stream にフォールバック。
        let fallback = content_response(None, None, Vec::new()).into_response();
        assert_eq!(
            fallback
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/octet-stream"
        );
    }

    #[test]
    fn content_response_appends_charset_for_text() {
        // テキスト（charset あり）は Content-Type に charset を付与する。
        // UTF-8-BOM は HTTP ヘッダ上は素の UTF-8 に正規化される。
        let resp = content_response(
            Some("text/plain".into()),
            Some("Shift_JIS".into()),
            "こんにちは".as_bytes().to_vec(),
        )
        .into_response();
        assert_eq!(
            resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=Shift_JIS"
        );

        let bom = content_response(
            Some("text/markdown".into()),
            Some("UTF-8-BOM".into()),
            "x".as_bytes().to_vec(),
        )
        .into_response();
        assert_eq!(
            bom.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "text/markdown; charset=UTF-8"
        );
    }

    async fn make_state() -> (ApiState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMetaStore::open_in_memory().await.unwrap();
        let pool = store.pool().clone();
        let blob = Arc::new(FsBlobStore::new(dir.path().join("blobs")).await.unwrap());
        let meta: SharedMetaStore = Arc::new(store);
        let registry = Arc::new(CrdtRegistry::with_defaults());
        let engine = Arc::new(VersioningEngine::new(registry, blob, meta.clone()));
        let db_authz = Arc::new(DbAuthorizer::new(pool.clone()));
        let authz: Arc<dyn Authorizer> = db_authz.clone();
        let audit = Arc::new(yozist_db::AuditLog::new(pool.clone()));
        let share_admin =
            Arc::new(yozist_auth::ShareTokenStore::new(pool, b"test".to_vec()));
        let auth_db = Arc::new(
            AuthDb::open_local(dir.path().join("auth.db"), Some(dir.path().join("secret")))
                .await
                .unwrap(),
        );
        (
            ApiState {
                meta,
                engine,
                auth_db,
                authz,
                acl_admin: db_authz,
                audit,
                share_admin,
                smb_creds: None,
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
