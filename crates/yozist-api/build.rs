//! テンプレート(.html)の変更を cargo に確実に検知させるためのビルドスクリプト。
//!
//! Askama はテンプレートをビルド時にバイナリへ埋め込むが、`.rs` を伴わない
//! テンプレートのみの変更では cargo が再コンパイルを検知せず、古いテンプレートが
//! 残ってしまうことがある（WebUI を直したのに反映されない原因になる）。
//! `templates/` 配下の変更で必ず再ビルドが走るようにする。
//!
//! 併せて `assets/view-plugins/*.js` を列挙し、`ui.rs` の配信ハンドラ（`/ui/plugins/:name`）
//! が参照する静的マニフェストを生成する。プラグイン JS を 1 ファイル追加するだけで
//! 配信対象に加わり、`ui.rs` 側の許可名リスト（match アーム）を編集する必要がなくなる
//! （docs/plugin-view-system.md の「1 ファイル追加だけで拡張できる」という設計方針に、
//! 配信経路を実際に一致させる）。
fn main() {
    println!("cargo:rerun-if-changed=templates");
    generate_view_plugin_manifest();
}

fn generate_view_plugin_manifest() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let plugins_dir = std::path::Path::new(&manifest_dir).join("assets/view-plugins");
    println!("cargo:rerun-if-changed={}", plugins_dir.display());

    let mut names: Vec<String> = std::fs::read_dir(&plugins_dir)
        .unwrap_or_else(|e| panic!("{} の読み取りに失敗: {e}", plugins_dir.display()))
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.ends_with(".js").then_some(name)
        })
        .collect();
    names.sort();

    let mut out = String::from(
        "// build.rs が assets/view-plugins/*.js から自動生成する。手で編集しない。\n\
         pub static VIEW_PLUGIN_ASSETS: &[(&str, &str)] = &[\n",
    );
    for name in &names {
        out.push_str(&format!(
            "    ({name:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/assets/view-plugins/{name}\"))),\n"
        ));
    }
    out.push_str("];\n");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = std::path::Path::new(&out_dir).join("view_plugin_manifest.rs");
    std::fs::write(&dest, out)
        .unwrap_or_else(|e| panic!("{} の書き込みに失敗: {e}", dest.display()));
}
