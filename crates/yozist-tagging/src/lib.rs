//! yozist-tagging — 3 層タグ（System / AI / Manual）+ シリーズ管理。
//!
//! # 設計原則
//! - 優先度: **Manual > AI > System**。同名タグの kind 衝突は Manual を勝者とする。
//! - シリーズの `order_index` は f64。中間挿入は前後値の中点。
//!
//! # TODO
//! - [ ] システムタグ自動生成（拡張子・パス由来）
//! - [ ] AI タグの信頼スコア閾値で表示／非表示切替
//! - [ ] `order_index` のオーバーフロー時再採番アルゴリズム
//! - [ ] AND フィルタクエリのインデックス最適化

use yozist_core::{FormatHint, Tag, TagId, TagKind};

/// 拡張子・パスから自動付与すべきシステムタグの候補を返す。
pub fn system_tags_for(hint: &FormatHint) -> Vec<Tag> {
    let mut out = Vec::new();
    if let Some(ext) = &hint.extension {
        let ext = ext.to_ascii_lowercase();
        out.push(Tag {
            id: TagId::new(),
            name: format!("ext:{}", ext),
            kind: TagKind::System,
            confidence: None,
        });
    }
    if let Some(mime) = &hint.mime {
        let category = mime.split('/').next().unwrap_or(mime);
        out.push(Tag {
            id: TagId::new(),
            name: format!("type:{}", category),
            kind: TagKind::System,
            confidence: None,
        });
    }
    out
}

/// アップロード元（`rest` / `web` / `smb` など）を示すシステムタグを返す。
/// 名前は `src:<source>`（小文字化）。`ext:` / `type:` と同じ System 種別で、
/// フィルタや by-tags 絞り込みからアップロード経路を辿れるようにする。
pub fn source_tag(source: &str) -> Tag {
    Tag {
        id: TagId::new(),
        name: format!("src:{}", source.trim().to_ascii_lowercase()),
        kind: TagKind::System,
        confidence: None,
    }
}

/// f64 の中間挿入アルゴリズム。`a < b` 前提で中点を返す。
pub fn midpoint_order(a: f64, b: f64) -> f64 {
    (a + b) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_tags_include_extension() {
        let hint = FormatHint {
            extension: Some("md".into()),
            mime: Some("text/markdown".into()),
            ..Default::default()
        };
        let tags = system_tags_for(&hint);
        assert!(tags.iter().any(|t| t.name == "ext:md"));
        assert!(tags.iter().any(|t| t.name == "type:text"));
    }

    #[test]
    fn source_tag_uses_prefix_and_system_kind() {
        let t = source_tag("REST");
        assert_eq!(t.name, "src:rest");
        assert!(matches!(t.kind, TagKind::System));
    }

    #[test]
    fn midpoint_between_two_values() {
        assert_eq!(midpoint_order(10.0, 20.0), 15.0);
    }
}
