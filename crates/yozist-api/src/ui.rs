//! WebUI。`/ui` 配下にブラウザから閲覧・操作可能なページ群を提供する。
//!
//! - HTML は askama テンプレート (`templates/`) で生成し、共通レイアウト・ナビゲーション・
//!   ダイアログ基盤を `base.html` に集約する。各ページはそれを `extends` した静的シェル + JS。
//! - JS から REST API を叩く SPA 風実装。SMB / REST / WebUI のいずれも同じ MetaStore を参照する。
//! - ダイアログは daisyUI のモーダル / トーストで実装し、ブラウザの prompt/confirm/alert は使わない。
//!
//! NOTE: テンプレートのみ変更したときに反映されるよう `build.rs` で `templates/` を
//! 監視している（Askama はテンプレート単独変更を cargo が検知しないことがあるため）。

use askama::Template;
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};

pub fn router() -> Router<crate::ApiState> {
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page))
        .route("/settings", get(settings_page))
        .route("/filters", get(filters_page))
        .route("/tags", get(tags_page))
        .route("/manage", get(manage_page))
        .route("/files", get(files_page))
        .route("/trash", get(trash_page))
        .route("/files/:id", get(file_detail_page))
        .route("/files/:id/compare", get(file_compare_page))
        .route("/files/:id/histories/:cid", get(file_commit_page))
        .route("/series/:id", get(series_settings_page))
}

/// askama テンプレートを描画して HTML レスポンスにする。
fn render(tpl: impl Template) -> Response {
    match tpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("template render error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response()
        }
    }
}

/// 各ページは `base.html` を extends した askama テンプレート。
/// `active` は navbar のタブ強調用 ("" = 強調なし)。
#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "manage.html")]
struct ManageTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "filters.html")]
struct FiltersTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "tags.html")]
struct TagsTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "files.html")]
struct FilesTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "trash.html")]
struct TrashTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "file_detail.html")]
struct FileDetailTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "compare.html")]
struct FileCompareTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "file_commit.html")]
struct FileCommitTemplate {
    active: &'static str,
}

#[derive(Template)]
#[template(path = "series_settings.html")]
struct SeriesSettingsTemplate {
    active: &'static str,
}

async fn index() -> Response {
    render(IndexTemplate { active: "" })
}

async fn login_page() -> Response {
    render(LoginTemplate { active: "login" })
}

async fn settings_page() -> Response {
    render(SettingsTemplate { active: "settings" })
}

async fn manage_page() -> Response {
    render(ManageTemplate { active: "manage" })
}

async fn filters_page() -> Response {
    render(FiltersTemplate { active: "filters" })
}

async fn tags_page() -> Response {
    render(TagsTemplate { active: "tags" })
}

async fn files_page() -> Response {
    render(FilesTemplate { active: "files" })
}

async fn trash_page() -> Response {
    render(TrashTemplate { active: "trash" })
}

async fn file_detail_page() -> Response {
    render(FileDetailTemplate { active: "" })
}

async fn file_compare_page() -> Response {
    render(FileCompareTemplate { active: "" })
}

async fn file_commit_page() -> Response {
    render(FileCommitTemplate { active: "" })
}

async fn series_settings_page() -> Response {
    render(SeriesSettingsTemplate { active: "" })
}
