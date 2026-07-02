//! テンプレート(.html)の変更を cargo に確実に検知させるためのビルドスクリプト。
//!
//! Askama はテンプレートをビルド時にバイナリへ埋め込むが、`.rs` を伴わない
//! テンプレートのみの変更では cargo が再コンパイルを検知せず、古いテンプレートが
//! 残ってしまうことがある（WebUI を直したのに反映されない原因になる）。
//! `templates/` 配下の変更で必ず再ビルドが走るようにする。
//!
//! 併せて `assets/` 配下の静的 JS を列挙し、`ui.rs` の配信ハンドラが参照する
//! 静的マニフェストを生成する。JS を 1 ファイル追加するだけで配信対象に加わり、
//! `ui.rs` 側の許可名リスト（match アーム）を編集する必要がなくなる：
//! - `assets/view-plugins/*.js` → `VIEW_PLUGIN_ASSETS`（`/ui/plugins/:name`）
//! - `assets/pages/*.js`        → `PAGE_ASSETS`（`/ui/pages/:name`）
fn main() {
    println!("cargo:rerun-if-changed=templates");
    generate_js_manifest("assets/view-plugins", "VIEW_PLUGIN_ASSETS", "view_plugin_manifest.rs");
    generate_js_manifest("assets/pages", "PAGE_ASSETS", "page_asset_manifest.rs");
}

/// `subdir` 内の `.js` を列挙し、`(ファイル名, include_str! で埋め込んだ内容)` の
/// 静的スライス `const_name` を `OUT_DIR/out_file` に生成する。
/// ディレクトリが無い場合は空のマニフェストになる（段階的な切り出し中でも壊れない）。
fn generate_js_manifest(subdir: &str, const_name: &str, out_file: &str) {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let dir = std::path::Path::new(&manifest_dir).join(subdir);
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut names: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.ends_with(".js").then_some(name)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();

    let mut out = format!(
        "// build.rs が {subdir}/*.js から自動生成する。手で編集しない。\n\
         pub static {const_name}: &[(&str, &str)] = &[\n"
    );
    for name in &names {
        out.push_str(&format!(
            "    ({name:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/{subdir}/{name}\"))),\n"
        ));
    }
    out.push_str("];\n");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = std::path::Path::new(&out_dir).join(out_file);
    std::fs::write(&dest, out)
        .unwrap_or_else(|e| panic!("{} の書き込みに失敗: {e}", dest.display()));
}
