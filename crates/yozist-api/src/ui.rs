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

// `assets/view-plugins/*.js` / `assets/pages/*.js` から build.rs が生成する
// `VIEW_PLUGIN_ASSETS` / `PAGE_ASSETS`（いずれも `&[(&str, &str)]`）。
// 新しい JS はディレクトリへ追加するだけで配信対象になり、ここを編集する必要はない。
include!(concat!(env!("OUT_DIR"), "/view_plugin_manifest.rs"));
include!(concat!(env!("OUT_DIR"), "/page_asset_manifest.rs"));

// 全ページ共有のユーティリティ（base.html から読み込む）。
static COMMON_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/common.js"));

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
        .route("/plugins/:name", get(view_plugin_asset))
        .route("/pages/:name", get(page_asset))
        .route("/assets/common.js", get(common_js_asset))
}

/// JS レスポンスを組み立てる（バイナリ埋め込みの静的ファイル配信）。
fn js_response(body: &'static str) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// 全ページ共有ユーティリティ（`assets/common.js`）を配信する。base.html が読み込む。
/// テンプレート内のインライン JS を静的ファイルへ切り出したもの（issue #50）。
async fn common_js_asset() -> Response {
    js_response(COMMON_JS)
}

/// ページ固有ロジックの JS（`assets/pages/*.js`）を配信する。各ページテンプレートの
/// `{% block script %}` が `<script src="/ui/pages/…">` で読み込む。配信対象は
/// build.rs が生成する `PAGE_ASSETS` から引く（ディレクトリ内の全 `.js` が自動的に
/// 候補になり、このハンドラを編集する必要はない）。
async fn page_asset(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    let Some(&(_, body)) = PAGE_ASSETS.iter().find(|(n, _)| *n == name) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    js_response(body)
}

/// ビュープラグインの JS を配信する。各プラグインは `assets/view-plugins/` 配下の
/// 独立ファイルで、バイナリへ埋め込んで（`include_str!`）配信する。配信対象は
/// `build.rs` が生成する `VIEW_PLUGIN_ASSETS` から引く（ディレクトリ内の全 `.js` が
/// 自動的に候補になり、このハンドラを編集する必要はない）。
/// 共有 ViewRuntime（base.html）へ自己登録する classic script として読み込まれる。
///
/// NOTE: どのページがどのプラグインを読み込むかはテンプレート側の `<script src>` が
/// 決める（差分専用プラグインを単一表示ページへ不要に読ませない等の理由で、
/// ページごとに必要な組み合わせが異なる）。プラグイン追加時にテンプレートの
/// 配線は別途必要（docs/plugin-view-system.md 参照）。
async fn view_plugin_asset(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let Some(&(_, body)) = VIEW_PLUGIN_ASSETS.iter().find(|(n, _)| *n == name) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    js_response(body)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_plugin_manifest_lists_known_plugins() {
        // build.rs が assets/view-plugins/*.js を列挙して生成するマニフェスト。
        // ディレクトリに .js を追加するだけで、このリストと配信ハンドラの両方が
        // 自動的に更新されることの回帰テスト。
        let names: Vec<&str> = VIEW_PLUGIN_ASSETS.iter().map(|(n, _)| *n).collect();
        for expected in [
            "text-diff.js",
            "image-diff.js",
            "binary-meta.js",
            "table-csv.js",
            "viewer-media.js",
        ] {
            assert!(names.contains(&expected), "missing plugin: {expected}");
        }
    }

    #[tokio::test]
    async fn view_plugin_asset_serves_known_and_404s_unknown() {
        let known = view_plugin_asset(axum::extract::Path("text-diff.js".to_string())).await;
        assert_eq!(known.status(), StatusCode::OK);

        let unknown = view_plugin_asset(axum::extract::Path("does-not-exist.js".to_string())).await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn page_asset_manifest_lists_known_pages() {
        // build.rs が assets/pages/*.js を列挙して生成するマニフェスト。
        // テンプレートから切り出したページ JS が配信対象に含まれることの回帰テスト。
        let names: Vec<&str> = PAGE_ASSETS.iter().map(|(n, _)| *n).collect();
        for expected in [
            "file_detail.js",
            "file_commit.js",
            "files.js",
            "compare.js",
            "index.js",
            "login.js",
            "settings.js",
            "manage.js",
            "tags.js",
            "filters.js",
            "trash.js",
            "series_settings.js",
        ] {
            assert!(names.contains(&expected), "missing page script: {expected}");
        }
    }

    #[tokio::test]
    async fn page_asset_serves_known_and_404s_unknown() {
        let known = page_asset(axum::extract::Path("file_detail.js".to_string())).await;
        assert_eq!(known.status(), StatusCode::OK);

        let unknown = page_asset(axum::extract::Path("does-not-exist.js".to_string())).await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn common_js_asset_serves_shared_utilities() {
        let res = common_js_asset().await;
        assert_eq!(res.status(), StatusCode::OK);
        // base.html から切り出した共有ユーティリティが埋め込まれていること。
        // （#53 で window への代入に JSDoc キャストが付いたため後方一致で検証する）
        assert!(COMMON_JS.contains("(window).ViewRuntime = ViewRuntime;"));
    }
}
