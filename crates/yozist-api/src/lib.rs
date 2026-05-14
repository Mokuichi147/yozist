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

use axum::{routing::get, Json, Router};
use serde::Serialize;
use std::sync::Arc;

use yozist_auth::AuthService;
use yozist_db::SharedMetaStore;
use yozist_storage::SharedBlobStore;

/// API ハンドラが共有する状態。
#[derive(Clone)]
pub struct ApiState {
    pub meta: SharedMetaStore,
    pub blob: SharedBlobStore,
    pub auth: Arc<dyn AuthService>,
}

/// ルーター生成。
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/files", get(list_files))
        .with_state(state)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn list_files(
    axum::extract::State(state): axum::extract::State<ApiState>,
) -> Json<Vec<yozist_core::FileMeta>> {
    let files = state.meta.list_files(100, 0).await.unwrap_or_default();
    Json(files)
}
