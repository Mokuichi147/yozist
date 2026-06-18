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
    ActorId, CommitId, FileId, FileMeta, FilterDef, Filter, FilterId, Series, SeriesId,
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
    /// 展開済み content の小さなキャッシュ。blob はファイル全体が 1 つの zstd
    /// として保存されるため、Range リクエストのたびに全体を展開すると巨大ファイルの
    /// 仮想スクロールが破綻する。直近に読んだ 1 ファイル分を保持し、同一コミットへの
    /// 連続した範囲取得で再展開を避ける。
    pub content_cache: Arc<ContentCache>,
}

/// 展開済み content の単一エントリキャッシュ（直近に読んだファイル）。
/// キーは `(FileId, CommitId)`。コミットが変われば自然にミスして再読込される。
#[derive(Default)]
pub struct ContentCache {
    inner: std::sync::Mutex<Option<(FileId, CommitId, Arc<Vec<u8>>)>>,
}

/// これを超えるサイズはキャッシュしない（サーバメモリ保護）。
const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;

impl ContentCache {
    fn get(&self, file_id: FileId, commit_id: CommitId) -> Option<Arc<Vec<u8>>> {
        let g = self.inner.lock().unwrap();
        match g.as_ref() {
            Some((f, c, b)) if *f == file_id && *c == commit_id => Some(b.clone()),
            _ => None,
        }
    }
    fn put(&self, file_id: FileId, commit_id: CommitId, bytes: Arc<Vec<u8>>) {
        if bytes.len() > MAX_CACHE_BYTES {
            return;
        }
        *self.inner.lock().unwrap() = Some((file_id, commit_id, bytes));
    }
}

/// ルーター生成。
pub fn router(state: ApiState) -> Router {
    Router::new()
        .nest("/ui", ui::router())
        .route("/", get(redirect_to_ui))
        .route("/health", get(health))
        .route("/api/files", get(list_files).post(create_file))
        // 注意: 静的セグメント "tags" は `:id` より優先マッチする（matchit の仕様）。
        .route("/api/files/tags", get(list_tags_batch))
        .route(
            "/api/files/:id",
            get(get_file).patch(rename_file).delete(delete_file),
        )
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
        .route("/api/filters", get(list_filters).post(create_filter))
        .route(
            "/api/filters/:id",
            get(get_filter)
                .patch(update_filter)
                .delete(delete_filter),
        )
        .route("/api/filters/:id/files", get(filter_files))
        .route("/api/files/:id/share", post(issue_file_share))
        .route("/api/filters/:id/share", post(issue_filter_share))
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
    /// アップロードしたクライアントソフトの識別子（任意）。指定時は
    /// `client:<name>` タグを付与し、どのソフト由来かで絞り込めるようにする。
    client: Option<String>,
}

#[derive(Deserialize)]
struct ListFilesQuery {
    /// 1 ページの件数（1〜1000、既定 100）。
    limit: Option<u32>,
    offset: Option<u32>,
    /// "updated"（既定） | "created" | "name" | "size"
    sort: Option<String>,
    /// "asc" | "desc"。省略時は sort に応じた自然な向き
    /// （updated/created/size は desc、name は asc）。
    order: Option<String>,
}

async fn list_files(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Query(q): Query<ListFilesQuery>,
) -> Result<axum::response::Response, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0);
    let sort = match q.sort.as_deref().unwrap_or("updated") {
        "updated" => yozist_db::FileSort::UpdatedAt,
        "created" => yozist_db::FileSort::CreatedAt,
        "name" => yozist_db::FileSort::Name,
        "size" => yozist_db::FileSort::Size,
        other => return Err(ApiError::BadRequest(format!("unknown sort: {other}"))),
    };
    let asc = match q.order.as_deref() {
        Some("asc") => true,
        Some("desc") => false,
        None => matches!(sort, yozist_db::FileSort::Name),
        Some(other) => return Err(ApiError::BadRequest(format!("unknown order: {other}"))),
    };
    // 権限フィルタで空に近いページが返り続けると「0 件なのに続きがある」と
    // 表示され、他ユーザーのファイルの存在が露見する。可視ファイルが limit 件
    // 集まるか DB を読み切るまでページを進めて吸収する（スキャン上限あり）。
    // 総数は権限フィルタ前の値しか安価に得られない（漏えいになる）ため返さない。
    const MAX_SCAN_PAGES: u32 = 10;
    let mut visible = Vec::new();
    let mut db_offset = offset;
    let mut has_more = false;
    for _ in 0..MAX_SCAN_PAGES {
        let files = s
            .meta
            .list_files_sorted(limit, db_offset, sort, asc)
            .await
            .map_err(ApiError::from_db)?;
        let fetched = files.len() as u32;
        visible.extend(filter_visible_files(&*s.authz, &ctx, files).await?);
        db_offset += fetched;
        if fetched < limit {
            has_more = false;
            break;
        }
        has_more = true;
        if visible.len() as u32 >= limit {
            break;
        }
    }
    let mut resp = Json(visible).into_response();
    let h = resp.headers_mut();
    h.insert(
        "x-has-more",
        axum::http::HeaderValue::from_static(if has_more { "1" } else { "0" }),
    );
    // 次ページ取得に使う DB オフセット（権限フィルタでページが縮むため
    // クライアント側では計算できない）。続きが無いときは返さない —
    // 読み切り時の値は「不可視ファイル込みの総行数」であり、漏えいになる。
    if has_more {
        if let Ok(v) = axum::http::HeaderValue::from_str(&db_offset.to_string()) {
            h.insert("x-next-offset", v);
        }
    }
    Ok(resp)
}

#[derive(Deserialize)]
struct TagsBatchQuery {
    /// カンマ区切りの FileId（最大 1000 件）。
    ids: String,
}

/// 複数ファイルのタグを一括取得（一覧ページのタグチップ表示用）。
/// VIEW 権限のないファイルは黙って結果から除外する。
async fn list_tags_batch(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Query(q): Query<TagsBatchQuery>,
) -> Result<Json<std::collections::HashMap<String, Vec<Tag>>>, ApiError> {
    let mut ids = Vec::new();
    for spec in q.ids.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        ids.push(parse_file_id(spec)?);
        if ids.len() > 1000 {
            return Err(ApiError::BadRequest("too many ids (max 1000)".into()));
        }
    }
    let mut visible = Vec::with_capacity(ids.len());
    for id in ids {
        let ok = s
            .authz
            .check(&ctx, &Target::file(id), PermissionMask::VIEW)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if ok {
            visible.push(id);
        }
    }
    let pairs = s
        .meta
        .list_tags_of_many(&visible)
        .await
        .map_err(ApiError::from_db)?;
    let mut out: std::collections::HashMap<String, Vec<Tag>> = std::collections::HashMap::new();
    // タグなしのファイルも空配列で返す（クライアント側の存在チェックを簡単にする）。
    for id in &visible {
        out.entry(id.to_string()).or_default();
    }
    for (file_id, tag) in pairs {
        out.entry(file_id.to_string()).or_default().push(tag);
    }
    Ok(Json(out))
}

/// 書き込み成功後に作成者/更新者ラベルをファイルメタへ記録する（表示用）。
/// 本体の書き込みは既に成功しているため、ここでの失敗は無視して None を返す。
async fn record_file_actor(
    s: &ApiState,
    id: FileId,
    ctx: &AuthContext,
    created: bool,
) -> Option<FileMeta> {
    let AuthContext::User { user, .. } = ctx else { return None };
    let mut meta = s.meta.get_file(&id).await.ok().flatten()?;
    if created {
        meta.created_by = Some(user.username.clone());
        meta.created_by_user_id = Some(user.id);
    }
    meta.updated_by = Some(user.username.clone());
    meta.updated_by_user_id = Some(user.id);
    s.meta.update_file(&meta).await.ok()?;
    Some(meta)
}

/// アップロード経路（`web` / `rest`）を判定する。WebUI は共通 fetch ヘルパで
/// `X-Yozist-Client: web` を必ず送る。ヘッダが無い／別値の場合は外部からの素の
/// REST 呼び出しとみなす。`src:<source>` タグの値に使う。
fn upload_source(headers: &axum::http::HeaderMap) -> &'static str {
    match headers
        .get("x-yozist-client")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    {
        Some("web") => "web",
        _ => "rest",
    }
}

async fn create_file(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Query(q): Query<CreateFileQuery>,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<FileMeta>), ApiError> {
    require_authenticated(&ctx).await?;
    let source = upload_source(&headers);
    let client = q.client.clone();
    let actor = parse_actor(q.actor.as_deref()).unwrap_or_else(ActorId::new);
    let name_for_audit = q.name.clone();
    // ボディをメモリに載せず 1 チャンクずつ blob ストアへ流す。
    let stream = body
        .into_data_stream()
        .map_err(|e| StorageError::Other(e.to_string()))
        .boxed();
    let result = s
        .engine
        .create_file_streaming(
            q.name,
            stream,
            actor,
            committed_by_label(&ctx),
            ctx.user_id(),
            None,
        )
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

    // アップロード元（rest / web）を示すシステムタグ `src:<source>` を付与。
    s.engine.attach_source_tag(file.id, source).await;
    // クライアントソフト指定があれば `client:<name>` タグも付与。
    if let Some(client) = client.as_deref() {
        s.engine.attach_client_tag(file.id, client).await;
    }

    let file = record_file_actor(&s, file.id, &ctx, true).await.unwrap_or(file);
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

#[derive(Deserialize)]
struct RenameFileInput {
    display_name: String,
}

/// ファイル名（display_name）を変更する。内容コミットは作らず、拡張子変更に
/// 追従して mime・system タグ・FTS を更新する（処理は versioning Engine に集約）。
async fn rename_file(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Json(input): Json<RenameFileInput>,
) -> Result<Json<FileMeta>, ApiError> {
    let file_id = parse_file_id(&id)?;
    require_permission(
        &*s.authz,
        &ctx,
        &Target::file(file_id),
        PermissionMask::WRITE,
    )
    .await?;
    let new_name = input.display_name.trim().to_string();
    if new_name.is_empty() {
        return Err(ApiError::BadRequest("ファイル名が空です".into()));
    }
    let updated_by = match &ctx {
        AuthContext::User { user, .. } => Some(user.username.clone()),
        _ => None,
    };
    let res = s
        .engine
        .rename_file(file_id, new_name, updated_by, ctx.user_id())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()));
    audit_event(
        &s,
        &ctx,
        "rename_file",
        Some("file"),
        Some(&id),
        None,
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    Ok(Json(res?))
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
        if let AuthContext::User { user, .. } = &ctx {
            meta.updated_by = Some(user.username.clone());
        }
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

/// 保存済み MIME と本文を、配信用の `(Content-Type, body)` に整える。
///
/// テキストファイル（`charset` あり）は blob に UTF-8 で保存されているため、
/// 取り込み時に判定した元エンコーディングへ再エンコードして「元の形式」で返す。
/// 併せて `Content-Type` に `charset=` を付与し、ブラウザが正しくデコードできる
/// ようにする。`charset` が `None`（バイナリ）はそのまま返す。
fn encode_content(
    mime: Option<String>,
    charset: Option<String>,
    bytes: Vec<u8>,
) -> (String, Vec<u8>) {
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
    (ct, body)
}

/// 保存済み MIME を Content-Type に設定して本文を返す。未設定なら octet-stream。
fn content_response(
    mime: Option<String>,
    charset: Option<String>,
    bytes: Vec<u8>,
) -> impl IntoResponse {
    let (ct, body) = encode_content(mime, charset, bytes);
    ([(axum::http::header::CONTENT_TYPE, ct)], body)
}

/// `Range: bytes=START-END` を解釈して単一レンジ `(start, end)`（両端含む）を返す。
/// 巨大ファイルをフロントが分割取得するための最小実装。複数レンジ・範囲外・
/// 空ファイルは `None`（呼び出し側で 416 か全体返却に振り分ける）。
///
/// - `bytes=0-1023` → `(0, 1023)`
/// - `bytes=1024-`  → `(1024, total-1)`
/// - `bytes=-512`   → 末尾 512 バイト
fn parse_byte_range(header: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = header.trim().strip_prefix("bytes=")?;
    // 複数レンジ（カンマ区切り）は非対応。
    if spec.contains(',') {
        return None;
    }
    let (start_s, end_s) = spec.split_once('-')?;
    let (start, end) = if start_s.is_empty() {
        // suffix レンジ: 末尾 N バイト。
        let n: u64 = end_s.trim().parse().ok()?;
        if n == 0 {
            return None;
        }
        let n = n.min(total);
        (total - n, total - 1)
    } else {
        let start: u64 = start_s.trim().parse().ok()?;
        let end: u64 = if end_s.trim().is_empty() {
            total - 1
        } else {
            end_s.trim().parse().ok()?
        };
        (start, end.min(total - 1))
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end))
}

/// `Range` ヘッダがあれば 206 Partial Content で該当範囲のみ、無ければ 200 で全体を返す。
/// テキスト/メディアともにフロントが必要な分だけ取得できるよう、常に
/// `Accept-Ranges: bytes` を付与する。解釈できるが範囲外の Range は 416 を返す。
fn range_response(
    content_type: String,
    body: &[u8],
    range: Option<&axum::http::HeaderValue>,
) -> axum::response::Response {
    use axum::http::{header, HeaderValue};
    let total = body.len() as u64;
    let ct = HeaderValue::from_str(&content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let accept = HeaderValue::from_static("bytes");

    // 空リソースには範囲が無い。Range の有無に関わらず 200 で空本文を返す。
    // 416 を返すとフロントのテキストビューアが取得に失敗し、本文が空の
    // テキストファイルを一切編集できなくなる。
    if total == 0 {
        let mut resp = Vec::<u8>::new().into_response();
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, ct);
        h.insert(header::ACCEPT_RANGES, accept);
        return resp;
    }

    let parsed = range
        .and_then(|h| h.to_str().ok())
        .map(|s| (s, parse_byte_range(s, total)));

    match parsed {
        Some((_, Some((start, end)))) => {
            let slice = body[start as usize..=end as usize].to_vec();
            let mut resp = (StatusCode::PARTIAL_CONTENT, slice).into_response();
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, ct);
            h.insert(header::ACCEPT_RANGES, accept);
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
                h.insert(header::CONTENT_RANGE, v);
            }
            resp
        }
        Some((_, None)) => {
            // Range ヘッダはあるが解釈不能/範囲外 → 416。
            let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            let h = resp.headers_mut();
            h.insert(header::ACCEPT_RANGES, accept);
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{total}")) {
                h.insert(header::CONTENT_RANGE, v);
            }
            resp
        }
        None => {
            let mut resp = body.to_vec().into_response();
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, ct);
            h.insert(header::ACCEPT_RANGES, accept);
            resp
        }
    }
}

#[derive(Deserialize)]
struct ContentQuery {
    /// 真値のときは charset 再エンコードせず、ストレージの生 UTF-8 を返す
    /// （`Content-Type` は `charset=utf-8`）。仮想スクロールビューアが
    /// チャンク境界の文字割れを安定処理するために使う。
    utf8: Option<String>,
}

async fn get_content(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Query(q): Query<ContentQuery>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    let id = parse_file_id(&id)?;
    require_permission(&*s.authz, &ctx, &Target::file(id), PermissionMask::READ).await?;
    let file = s
        .meta
        .get_file(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    // 展開済み content をキャッシュから取得（なければ展開して保存）。
    // 連続した Range 取得（仮想スクロール）で blob の再展開を避ける。
    let bytes = match file.current_commit.and_then(|cid| s.content_cache.get(id, cid)) {
        Some(b) => b,
        None => {
            let raw = s
                .engine
                .read_current(id)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let arc = Arc::new(raw);
            if let Some(cid) = file.current_commit {
                s.content_cache.put(id, cid, arc.clone());
            }
            arc
        }
    };
    let range = headers.get(axum::http::header::RANGE);
    let want_utf8 = q.utf8.as_deref().is_some_and(|v| !v.is_empty() && v != "0");
    if want_utf8 {
        // blob は常に UTF-8。charset 再エンコードせず生のまま、必要範囲だけ返す。
        let mut ct = file.mime.unwrap_or_else(|| "text/plain".to_string());
        if !ct.to_ascii_lowercase().contains("charset=") {
            ct = format!("{ct}; charset=utf-8");
        }
        Ok(range_response(ct, &bytes, range))
    } else {
        // charset 再エンコードが必要な経路（ダウンロード/メディア/互換）。
        let (ct, body) = encode_content(file.mime, file.charset, (*bytes).clone());
        Ok(range_response(ct, &body, range))
    }
}

#[derive(Deserialize)]
struct CommitQuery {
    actor: Option<String>,
    message: Option<String>,
    /// 指定時はファイル名を更新し、mime/charset を新しい名前＋内容から再判定する
    /// （アップロードによる「内容を更新」用）。テキスト編集等では送らない。
    name: Option<String>,
    /// 部分編集用（範囲置換）。指定時はボディで現行内容の
    /// `[repl_start, repl_end)`（ストレージ UTF-8 バイト空間）を置換し、その前後は
    /// そのまま残して全文をコミットする。巨大ファイルでも表示中の範囲だけ編集できる。
    /// 省略時の既定: repl_start=0, repl_end=現行長（= 全置換 / 末尾保持）。
    repl_start: Option<u64>,
    repl_end: Option<u64>,
    /// 後方互換: repl_start=0, repl_end=keep_from と同義（先頭プレフィックス置換）。
    keep_from: Option<u64>,
}

/// 部分編集で受け取る本文の最大バイト数。
const MAX_EDIT_PREFIX_BYTES: usize = 512 * 1024 * 1024;

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
    // name 指定時（アップロードによる「内容を更新」）は前バージョンとマージせず
    // 全置換する。形式・mime・charset・表示名を新しい名前＋内容から判定し直すため、
    // 別形式へ差し替えても旧バージョンの解釈に引きずられず破損しない。
    // repl_start/repl_end/keep_from 指定時（部分編集）は現行内容の該当範囲のみ置換する。
    // どれも無い（テキスト全文編集など）は従来どおりストリーミング CRDT マージ経路。
    let is_partial = q.repl_start.is_some() || q.repl_end.is_some() || q.keep_from.is_some();
    let result = if let Some(name) = q.name {
        let stream = body
            .into_data_stream()
            .map_err(|e| StorageError::Other(e.to_string()))
            .boxed();
        s.engine
            .replace_streaming(
                id,
                name,
                stream,
                actor,
                committed_by_label(&ctx),
                ctx.user_id(),
                q.message,
            )
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))
    } else if is_partial {
        let repl_start = q.repl_start.unwrap_or(0);
        let repl_end = q.repl_end.or(q.keep_from); // keep_from は repl_end の別名
        commit_partial(
            &s,
            id,
            repl_start,
            repl_end,
            body,
            actor,
            committed_by_label(&ctx),
            ctx.user_id(),
            q.message,
        )
        .await
    } else {
        let stream = body
            .into_data_stream()
            .map_err(|e| StorageError::Other(e.to_string()))
            .boxed();
        s.engine
            .commit_streaming(id, stream, actor, committed_by_label(&ctx), ctx.user_id(), q.message)
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

    let commit = result?;
    record_file_actor(&s, id, &ctx, false).await;
    Ok(Json(commit))
}

/// 部分編集コミット。受信した本文（UTF-8）で現行内容の `[repl_start, repl_end)` を
/// 置換し、その前後を残して全文を作り、通常の CRDT コミットへ渡す。
///
/// 置換は **ストレージの UTF-8 バイト空間** で行う（blob は常に UTF-8）。オフセットは
/// クライアントが読み込んだテキストの UTF-8 バイト位置で文字境界に一致する。安全のため
/// サーバ側でも長さクランプと境界補正を行う。`repl_end=None` は現行長（末尾まで置換）。
async fn commit_partial(
    s: &ApiState,
    id: FileId,
    repl_start: u64,
    repl_end: Option<u64>,
    body: Body,
    actor: ActorId,
    committed_by: Option<String>,
    committed_by_user_id: Option<i64>,
    message: Option<String>,
) -> Result<yozist_core::Commit, ApiError> {
    let body = axum::body::to_bytes(body, MAX_EDIT_PREFIX_BYTES)
        .await
        .map_err(|e| ApiError::BadRequest(format!("body: {e}")))?;
    // 直前まで表示していたファイルなら展開済みキャッシュが温まっているため、
    // 現行内容の再解凍（GB 級で数秒）を避けられる。
    let file = s
        .meta
        .get_file(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    let current = match file.current_commit.and_then(|cid| s.content_cache.get(id, cid)) {
        Some(b) => b,
        None => Arc::new(
            s.engine
                .read_current(id)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?,
        ),
    };
    let end = repl_end.unwrap_or(current.len() as u64);
    let new_full = splice_range(&body, &current, repl_start, end);
    // CRDT 差分を経ず直接 blob としてコミットする。結合済みの最終全文が手元に
    // あるため結果の blob は通常コミットと同一で、巨大ファイルでも軽い。
    let commit = s
        .engine
        .commit_raw(id, &new_full, actor, committed_by, committed_by_user_id, message)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // 保存直後の再表示（Range 取得）が再解凍なしで返るよう、新内容を事前投入する。
    s.content_cache.put(id, commit.id, Arc::new(new_full));
    Ok(commit)
}

/// `current[0..repl_start)` + `body` + `current[repl_end..]` を結合する。
///
/// blob は文字コードに依らず常に UTF-8 保存で、`body` も UTF-8。オフセットは
/// クライアントが読み込んだテキストの UTF-8 バイト位置（文字境界）。元の文字コード
/// （Shift_JIS / EUC-JP / UTF-16 など）に依存せず正しく機能する。念のため
/// サーバ側でも長さクランプと UTF-8 境界補正（継続バイトの途中で切らない）を行う。
fn splice_range(body: &[u8], current: &[u8], repl_start: u64, repl_end: u64) -> Vec<u8> {
    let to_boundary = |mut p: usize| {
        p = p.min(current.len());
        while p > 0 && p < current.len() && (current[p] & 0xC0) == 0x80 {
            p -= 1;
        }
        p
    };
    let start = to_boundary(repl_start as usize);
    let end = to_boundary((repl_end as usize).max(repl_start as usize));
    let mut out = Vec::with_capacity(start + body.len() + (current.len() - end));
    out.extend_from_slice(&current[..start]);
    out.extend_from_slice(body);
    out.extend_from_slice(&current[end..]);
    out
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
    Query(q): Query<ContentQuery>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, ApiError> {
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
    // get_content と同じく展開済みキャッシュ＋Range 対応（過去バージョンの仮想スクロール用）。
    let bytes = match s.content_cache.get(file_id, commit_id) {
        Some(b) => b,
        None => {
            let raw = s
                .engine
                .read_at_commit(file_id, commit_id)
                .await
                .map_err(|e| match e {
                    yozist_versioning::VersioningError::NotFound(_) => ApiError::NotFound,
                    other => ApiError::Internal(other.to_string()),
                })?;
            let arc = Arc::new(raw);
            s.content_cache.put(file_id, commit_id, arc.clone());
            arc
        }
    };
    let range = headers.get(axum::http::header::RANGE);
    let want_utf8 = q.utf8.as_deref().is_some_and(|v| !v.is_empty() && v != "0");
    if want_utf8 {
        // blob は常に UTF-8。charset 再エンコードせず生のまま、必要範囲だけ返す。
        let mut ct = file.mime.unwrap_or_else(|| "text/plain".to_string());
        if !ct.to_ascii_lowercase().contains("charset=") {
            ct = format!("{ct}; charset=utf-8");
        }
        Ok(range_response(ct, &bytes, range))
    } else {
        let (ct, body) = encode_content(file.mime, file.charset, (*bytes).clone());
        Ok(range_response(ct, &body, range))
    }
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
        .rollback_to(file_id, commit_id, actor, committed_by_label(&ctx), ctx.user_id(), q.message)
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
    let commit = res?;
    record_file_actor(&s, file_id, &ctx, false).await;
    Ok(Json(commit))
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

#[derive(Deserialize)]
struct ListTagsQuery {
    /// "usage" を指定すると割り当て数の多い順で返す。未指定は名前昇順。
    sort: Option<String>,
}

async fn list_tags(
    State(s): State<ApiState>,
    Query(q): Query<ListTagsQuery>,
) -> Result<Json<Vec<Tag>>, ApiError> {
    let tags = if q.sort.as_deref() == Some("usage") {
        s.meta.list_tags_by_usage().await
    } else {
        s.meta.list_tags().await
    }
    .map_err(ApiError::from_db)?;
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
struct CreateFilterInput {
    name: String,
    description: Option<String>,
    #[serde(default)]
    tags_and: Vec<String>,
    #[serde(default)]
    tags_not: Vec<String>,
    /// 条件群（タグ種別 / シリーズ / 種類 / 名前 / 日付）。
    #[serde(default)]
    match_mode: yozist_core::MatchMode,
    #[serde(default)]
    conditions: Vec<yozist_core::FilterCondition>,
    /// 期限秒数（now + N 秒）。
    expires_in_secs: Option<i64>,
}

/// フィルター更新入力。指定したフィールドのみ差し替える（`None` は据え置き）。
/// `name` を変えると SMB の `filters\<名前>\` パスも改名される。
#[derive(Deserialize)]
struct UpdateFilterInput {
    name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    description: Option<Option<String>>,
    tags_and: Option<Vec<String>>,
    tags_not: Option<Vec<String>>,
    match_mode: Option<yozist_core::MatchMode>,
    conditions: Option<Vec<yozist_core::FilterCondition>>,
}

/// `Option<Option<T>>` を JSON の「キー欠落=据え置き / null=クリア」に対応させる。
fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(de)?))
}

/// 文字列を JSON 文字列リテラル（前後のダブルクォート込み）へエスケープする。
/// 監査ログの metadata_json 組み立て用。
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// フィルター名のバリデーション。`yozist\filters\<名前>\` の 1 コンポーネントと
/// して使うため、SMB パスで問題になる文字のみ弾く（名前自体は任意）。
/// - 空 / 前後空白のみ
/// - SMB パスで使えない文字（`\\ / : * ? " < > |` と制御文字）
fn validate_filter_name(name: &str) -> Result<String, ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ApiError::BadRequest("フィルター名を入力してください".into()));
    }
    if trimmed.len() > 80 {
        return Err(ApiError::BadRequest("フィルター名が長すぎます（80文字以内）".into()));
    }
    if trimmed.chars().any(|c| {
        matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()
    }) {
        return Err(ApiError::BadRequest(
            r#"フィルター名に使えない文字が含まれています（\ / : * ? " < > | や制御文字）"#.into(),
        ));
    }
    Ok(trimmed.to_string())
}

/// 編集・削除はフィルター作成者のみに許可する（作成者不明の旧データは認証ユーザーへ開放）。
fn require_filter_owner(ctx: &AuthContext, q: &Filter) -> Result<(), ApiError> {
    match (&q.created_by, ctx) {
        (Some(owner), AuthContext::User { user, .. }) if *owner == user.id => Ok(()),
        (None, AuthContext::User { .. }) => Ok(()),
        (_, AuthContext::System) => Ok(()),
        _ => Err(ApiError::Forbidden),
    }
}

async fn list_filters(
    State(s): State<ApiState>,
) -> Result<Json<Vec<Filter>>, ApiError> {
    let list = s.meta.list_filters().await.map_err(ApiError::from_db)?;
    Ok(Json(list))
}

async fn create_filter(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Json(input): Json<CreateFilterInput>,
) -> Result<(StatusCode, Json<Filter>), ApiError> {
    require_authenticated(&ctx).await?;
    let name = validate_filter_name(&input.name)?;
    // 同名の既存フィルターは share 名が衝突するため拒否する。
    if s.meta
        .get_filter_by_name(&name)
        .await
        .map_err(ApiError::from_db)?
        .is_some()
    {
        return Err(ApiError::Conflict);
    }
    let now = time::OffsetDateTime::now_utc();
    let created_by = match &ctx {
        AuthContext::User { user, .. } => Some(user.id),
        _ => None,
    };
    let expires_at = input
        .expires_in_secs
        .map(|s| now + time::Duration::seconds(s));
    let q = Filter {
        id: FilterId::new(),
        name,
        definition: FilterDef {
            tags_and: input.tags_and,
            tags_not: input.tags_not,
            match_mode: input.match_mode,
            conditions: input.conditions,
        },
        description: input.description,
        created_by,
        created_at: now,
        expires_at,
    };
    let id = s.meta.upsert_filter(&q).await.map_err(ApiError::from_db)?;
    let saved = Filter { id, ..q };
    // SMB ハブ share（yozist\<name>\）には DB を即時反映するため追加処理は不要。
    audit_event(
        &s,
        &ctx,
        "create_filter",
        Some("filter"),
        Some(&saved.id.to_string()),
        Some(&format!("{{\"name\":{}}}", json_str(&saved.name))),
        &Ok::<(), String>(()),
    )
    .await;
    Ok((StatusCode::CREATED, Json(saved)))
}

async fn update_filter(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
    Json(input): Json<UpdateFilterInput>,
) -> Result<Json<Filter>, ApiError> {
    require_authenticated(&ctx).await?;
    let qid = uuid::Uuid::parse_str(&id)
        .map(FilterId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("filter id: {e}")))?;
    let existing = s
        .meta
        .get_filter(&qid)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    require_filter_owner(&ctx, &existing)?;

    let old_name = existing.name.clone();
    let new_name = match &input.name {
        Some(n) => validate_filter_name(n)?,
        None => old_name.clone(),
    };
    // 改名先が他のフィルターと衝突しないこと（share 名の一意性）。
    if !new_name.eq_ignore_ascii_case(&old_name) {
        let clash = s
            .meta
            .get_filter_by_name(&new_name)
            .await
            .map_err(ApiError::from_db)?
            .is_some_and(|other| other.id != existing.id);
        if clash {
            return Err(ApiError::Conflict);
        }
    }

    let updated = Filter {
        id: existing.id,
        name: new_name.clone(),
        definition: FilterDef {
            tags_and: input.tags_and.unwrap_or(existing.definition.tags_and),
            tags_not: input.tags_not.unwrap_or(existing.definition.tags_not),
            match_mode: input.match_mode.unwrap_or(existing.definition.match_mode),
            conditions: input.conditions.unwrap_or(existing.definition.conditions),
        },
        description: input.description.unwrap_or(existing.description),
        created_by: existing.created_by,
        created_at: existing.created_at,
        expires_at: existing.expires_at,
    };
    s.meta
        .upsert_filter(&updated)
        .await
        .map_err(ApiError::from_db)?;

    // SMB ハブ share（yozist\<name>\）は DB を都度引くため、改名・条件変更は
    // 次回アクセス時に自動で反映される（再登録などの追加処理は不要）。
    audit_event(
        &s,
        &ctx,
        "update_filter",
        Some("filter"),
        Some(&id),
        Some(&format!("{{\"name\":{}}}", json_str(&new_name))),
        &Ok::<(), String>(()),
    )
    .await;
    Ok(Json(updated))
}

async fn get_filter(
    State(s): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Filter>, ApiError> {
    let id = uuid::Uuid::parse_str(&id)
        .map(FilterId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("filter id: {e}")))?;
    let q = s
        .meta
        .get_filter(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(q))
}

async fn delete_filter(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_authenticated(&ctx).await?;
    let qid = uuid::Uuid::parse_str(&id)
        .map(FilterId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("filter id: {e}")))?;
    let existing = s
        .meta
        .get_filter(&qid)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    require_filter_owner(&ctx, &existing)?;
    let res = s
        .meta
        .delete_filter(&qid)
        .await
        .map_err(ApiError::from_db);
    // SMB ハブ share（yozist\<name>\）からは DB 削除と同時に消える。
    audit_event(
        &s,
        &ctx,
        "delete_filter",
        Some("filter"),
        Some(&id),
        None,
        &res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    )
    .await;
    res?;
    Ok(StatusCode::NO_CONTENT)
}

async fn filter_files(
    State(s): State<ApiState>,
    AuthCtx(ctx): AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Vec<FileMeta>>, ApiError> {
    let id = uuid::Uuid::parse_str(&id)
        .map(FilterId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("filter id: {e}")))?;
    let q = s
        .meta
        .get_filter(&id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    let files = resolve_filter(&*s.meta, &q.definition).await?;
    // 他の一覧系 API（by-tags / search 等）と同様に VIEW 権限で絞り込む。
    // これを欠くと WebUI のファイル一覧に開けないファイルが混ざってしまう。
    let visible = filter_visible_files(&*s.authz, &ctx, files).await?;
    Ok(Json(visible))
}

/// 共通ヘルパ: Filter の定義を解決して FileMeta 一覧を返す。
pub async fn resolve_filter(
    meta: &dyn yozist_db::MetaStore,
    q: &FilterDef,
) -> Result<Vec<FileMeta>, ApiError> {
    // 条件評価は REST / SMB 共通の yozist-db::resolve_filter に委譲する。
    yozist_db::resolve_filter(meta, q)
        .await
        .map_err(ApiError::from_db)
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

/// コミット履歴へ記録する実行ユーザーラベル。CRDT 用の ActorId とは別に
/// 「誰が変更したか」を残す。git の `name <email>` に倣い
/// `表示名 <ユーザーID>`（例: `もくいち <mokuichi147>`）の形で焼き込む。
/// username は UNIQUE なので追跡キーとして機能し、display_name は可読性のために添える。
/// 内部の数値 user.id は露出させない。コミット時点の値で固定されるため、
/// 後の改名は過去コミットへ遡及しない（履歴を壊さない方針）。
/// ログイン済みユーザーのみ記録し、匿名/システムは None（＝履歴上 NULL）とする。
fn committed_by_label(ctx: &AuthContext) -> Option<String> {
    match ctx {
        AuthContext::User { user, .. } => {
            Some(format!("{} <{}>", user.display_name, user.username))
        }
        _ => None,
    }
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

async fn issue_filter_share(
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
        .issue_share_token("filter", &id, input.ttl_secs, issuer)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()));
    let meta = format!("{{\"ttl_secs\":{}}}", input.ttl_secs);
    audit_event(
        &s,
        &ctx,
        "issue_filter_share",
        Some("filter"),
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

/// フィルター共有トークンでマッチするファイル一覧を返す。
async fn list_shared_files(
    State(s): State<ApiState>,
    Path(token): Path<String>,
) -> Result<Json<Vec<FileMeta>>, ApiError> {
    let target_id = verify_share(&s, &token, "filter").await?;
    let query_id = uuid::Uuid::parse_str(&target_id)
        .map(FilterId::from_uuid)
        .map_err(|e| ApiError::BadRequest(format!("filter id: {e}")))?;
    let q = s
        .meta
        .get_filter(&query_id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or(ApiError::NotFound)?;
    let files = resolve_filter(&*s.meta, &q.definition).await?;
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

    #[test]
    fn parse_byte_range_handles_common_forms() {
        // 通常レンジ・両端含む。
        assert_eq!(parse_byte_range("bytes=0-1023", 4096), Some((0, 1023)));
        // 開始のみ → 末尾まで。
        assert_eq!(parse_byte_range("bytes=1024-", 4096), Some((1024, 4095)));
        // 末尾 N バイト。
        assert_eq!(parse_byte_range("bytes=-512", 4096), Some((3584, 4095)));
        // end が total を超える場合は total-1 にクランプ。
        assert_eq!(parse_byte_range("bytes=0-99999", 4096), Some((0, 4095)));
        // suffix が total を超える場合は全体。
        assert_eq!(parse_byte_range("bytes=-99999", 4096), Some((0, 4095)));
        // 範囲外・複数レンジ・空ファイル・不正は None。
        assert_eq!(parse_byte_range("bytes=5000-6000", 4096), None);
        assert_eq!(parse_byte_range("bytes=0-100,200-300", 4096), None);
        assert_eq!(parse_byte_range("bytes=0-100", 0), None);
        assert_eq!(parse_byte_range("items=0-100", 4096), None);
    }

    #[test]
    fn range_response_returns_partial_content() {
        use axum::http::{header, HeaderValue};
        let body = (0u8..=9).collect::<Vec<u8>>();
        let range = HeaderValue::from_static("bytes=2-5");
        let resp = range_response("text/plain".into(), &body, Some(&range));
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 2-5/10"
        );
        assert_eq!(resp.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");

        // Range 無しは 200 で Accept-Ranges のみ付与。
        let full = range_response("text/plain".into(), &body, None);
        assert_eq!(full.status(), StatusCode::OK);
        assert_eq!(full.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");

        // 範囲外は 416。
        let bad = HeaderValue::from_static("bytes=100-200");
        let oob = range_response("text/plain".into(), &body, Some(&bad));
        assert_eq!(oob.status(), StatusCode::RANGE_NOT_SATISFIABLE);

        // 空ファイルへの Range は 416 ではなく 200(空本文)。本文の無いテキスト
        // ファイルをビューア/エディタが読めるようにするため。
        let empty = HeaderValue::from_static("bytes=0-262143");
        let resp = range_response("text/plain".into(), &[], Some(&empty));
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");
    }

    #[test]
    fn splice_range_replaces_region() {
        // 先頭プレフィックス置換（repl_start=0）: [0,4) を "XY" に → "XY"+"EFGH"。
        assert_eq!(splice_range(b"XY", b"ABCDEFGH", 0, 4), b"XYEFGH".to_vec());
        // 中間範囲置換: [2,6) を "xy" に → "AB"+"xy"+"GH"。
        assert_eq!(splice_range(b"xy", b"ABCDEFGH", 2, 6), b"ABxyGH".to_vec());
        // 末尾範囲置換: [6,8) を "Z" に → "ABCDEF"+"Z"。
        assert_eq!(splice_range(b"Z", b"ABCDEFGH", 6, 8), b"ABCDEFZ".to_vec());
        // end=len は末尾保持なし（全置換相当）。
        assert_eq!(splice_range(b"NEW", b"OLD", 0, 3), b"NEW".to_vec());
        // 範囲外はクランプ。
        assert_eq!(splice_range(b"NEW", b"OLD", 0, 999), b"NEW".to_vec());
    }

    #[test]
    fn splice_range_respects_utf8_boundaries() {
        // 「あいうえお」= 各 3 バイトの UTF-8。blob は文字コードに依らず UTF-8 保存。
        let current = "あいうえお".as_bytes(); // 15 bytes
        // [6,9)（「う」）を "X" に置換 → "あいXえお"。
        let spliced = splice_range("X".as_bytes(), current, 6, 9);
        assert_eq!(String::from_utf8(spliced).unwrap(), "あいXえお");
        // 継続バイトの途中(7,10)でも境界(6,9)へ補正され破損しない。
        let spliced = splice_range("X".as_bytes(), current, 7, 10);
        assert_eq!(String::from_utf8(spliced).unwrap(), "あいXえお");
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
                content_cache: Arc::new(ContentCache::default()),
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
