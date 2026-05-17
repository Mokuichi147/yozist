//! 最小 WebUI。`/ui` 配下にブラウザから閲覧・操作可能な単一ページを提供する。
//!
//! - SSR は使わず、静的 HTML + JS で REST API を叩く SPA 風実装。
//! - SMB / REST / WebUI のいずれも同じ MetaStore を参照することを実証する。
//!
//! # TODO
//! - [ ] leptos / askama 等のテンプレートエンジン統合
//! - [ ] ファイルアップロード UI
//! - [ ] タグ・シリーズの GUI 編集
//! - [ ] 共有 URL（期限付き）の発行 UI

use axum::{
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};

pub fn router() -> Router<crate::ApiState> {
    Router::new()
        .route("/", get(index))
        .route("/files", get(files_page))
        .route("/files/:id", get(file_detail_page))
        .route("/files/:id/histories/:cid", get(file_commit_page))
}

async fn index() -> Response {
    Html(INDEX_HTML).into_response()
}

async fn files_page() -> Response {
    Html(FILES_HTML).into_response()
}

async fn file_detail_page() -> Response {
    Html(FILE_DETAIL_HTML).into_response()
}

async fn file_commit_page() -> Response {
    Html(FILE_COMMIT_HTML).into_response()
}

const INDEX_HTML: &str = include_str!("ui/index.html");
const FILES_HTML: &str = include_str!("ui/files.html");
const FILE_DETAIL_HTML: &str = include_str!("ui/file_detail.html");
const FILE_COMMIT_HTML: &str = include_str!("ui/file_commit.html");
