//! `yozist-view` — プラガブルなビュー／変換レジストリ。
//!
//! 表示・差分の「形式 → ビューモデル」変換を担うバックエンド層。設計は
//! `yozist-versioning` の `CrdtFormat` ＋ `CrdtRegistry` を意図的に踏襲する
//! （`detect(hint)` が真を返す最初の変換を採用、無ければフォールバック）。
//!
//! # 2 層プラグインのうちの「変換層」
//! ```text
//!   生バイト ──(ViewConverter)──▶ ViewModel ──(フロントの ViewPlugin)──▶ 描画/差分
//! ```
//! 変換が産出する `kind`（ViewKind）が、フロントのビュープラグインと接続する
//! 唯一のキー。コア側は ViewKind の意味を一切持たない（単なる照合キー）。
//!
//! # 拡張のしかた
//! `Arc::new(MyConverter)` を [`ViewRegistry::register`] するだけ。どんな未知形式も
//! 最終的に [`BinaryConverter`]（常に `detect=true`）へ着地するため行き止まりが無い。

use async_trait::async_trait;
use std::sync::Arc;
use yozist_core::FormatHint;

/// 第一者組込みのビュー種別 ID。
///
/// ViewKind は閉じた enum ではなく**開いた文字列名前空間**。衝突回避のため
/// `namespace/name` 規約を用いる（`CrdtFormat::format_id` の `"_/lww"` と同流儀）。
pub mod kinds {
    /// プレーンテキスト（行差分・テキスト表示）。payload は生 UTF-8 バイト。
    pub const TEXT: &str = "core/text";
    /// ラスタ／ベクタ画像。payload は画像バイト、`content_type` が実 MIME。
    pub const IMAGE: &str = "core/image";
    /// 汎用バイナリ（メタ情報・16 進）。常に着地するフォールバック。
    pub const BINARY: &str = "core/binary";
}

/// 変換エラー。
#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    /// 入力が当該変換の想定形式と一致しない。
    #[error("format mismatch: {0}")]
    FormatMismatch(String),
    /// 変換処理中の内部エラー。
    #[error("convert failed: {0}")]
    Convert(String),
}

/// ビューが描画する正規化済みデータ（変換の産物）。
#[derive(Debug, Clone)]
pub struct ViewModel {
    /// 描画するビュー種別（[`kinds`]）。フロントの ViewPlugin と照合する。
    pub kind: String,
    /// `payload` の MIME。画像なら実 MIME、テキストなら `text/plain; charset=utf-8` 等。
    pub content_type: String,
    /// 種別固有の正規化データ。解釈は ViewKind の取り決め（コアは不問）。
    pub payload: Vec<u8>,
    /// 表示補助メタ（寸法・行数・検出 MIME 等）。空でよい。
    pub meta: serde_json::Value,
}

/// 変換プラグイン。`CrdtFormat` と同型。
#[async_trait]
pub trait ViewConverter: Send + Sync {
    /// 一意な変換 ID（ログ・診断・キャッシュキー用）。例 `"core/text"`。
    fn converter_id(&self) -> &'static str;

    /// この変換が `hint` を対象とするか。レジストリは先勝ちで採用する。
    fn detect(&self, hint: &FormatHint) -> bool;

    /// 産出する ViewKind（[`kinds`]）。
    fn target_view(&self) -> &'static str;

    /// 生バイト → [`ViewModel`]。重い変換（CAD→メッシュ等）はここに集約する。
    async fn convert(&self, bytes: &[u8], hint: &FormatHint) -> Result<ViewModel, ViewError>;

    /// 恒等変換か（`payload` が入力バイトそのもの）。真なら API は生バイトを
    /// そのまま流用でき、変換キャッシュも不要。
    fn is_passthrough(&self) -> bool {
        false
    }
}

/// プラガブルなビュー変換レジストリ。
pub struct ViewRegistry {
    converters: Vec<Arc<dyn ViewConverter>>,
    fallback: Arc<BinaryConverter>,
}

impl ViewRegistry {
    /// 空のレジストリ（フォールバックのみ）。
    pub fn new() -> Self {
        Self {
            converters: Vec::new(),
            fallback: Arc::new(BinaryConverter),
        }
    }

    /// 第一者組込みの変換を全て登録した状態（画像 → テキスト → バイナリ）。
    ///
    /// 画像はマジックナンバーで厳密に判定できるため先に試し、次にテキスト
    /// （ヌルバイトを含まない）、いずれでもなければバイナリへ落とす。
    pub fn with_defaults() -> Self {
        let mut reg = Self::new();
        reg.register(Arc::new(ImageConverter));
        reg.register(Arc::new(TextConverter));
        reg
    }

    pub fn register(&mut self, c: Arc<dyn ViewConverter>) {
        self.converters.push(c);
    }

    /// `detect()` が真を返す最初の変換を採用。無ければ [`BinaryConverter`]。
    pub fn resolve(&self, hint: &FormatHint) -> Arc<dyn ViewConverter> {
        for c in &self.converters {
            if c.detect(hint) {
                return c.clone();
            }
        }
        self.fallback.clone()
    }
}

impl Default for ViewRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ---------------------------------------------------------------------------
// 検出ヘルパ（フロントの sniff と同一規約）
// ---------------------------------------------------------------------------

/// 先頭バイトから画像 MIME を推定する（フロント `sniffImageMime` と一致）。
/// SVG はテキストでマジックナンバーが無いため、先頭付近の `<svg` で判定する。
pub fn sniff_image_mime(b: &[u8]) -> Option<&'static str> {
    if b.len() >= 4 && b[0] == 0x89 && b[1] == 0x50 && b[2] == 0x4E && b[3] == 0x47 {
        return Some("image/png");
    }
    if b.len() >= 3 && b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF {
        return Some("image/jpeg");
    }
    if b.len() >= 4 && b[0] == 0x47 && b[1] == 0x49 && b[2] == 0x46 && b[3] == 0x38 {
        return Some("image/gif");
    }
    if b.len() >= 12
        && &b[0..4] == b"RIFF"
        && b[8] == 0x57
        && b[9] == 0x45
        && b[10] == 0x42
        && b[11] == 0x50
    {
        return Some("image/webp");
    }
    if b.len() >= 2 && b[0] == 0x42 && b[1] == 0x4D {
        return Some("image/bmp");
    }
    if b.len() >= 4 && b[0] == 0x00 && b[1] == 0x00 && b[2] == 0x01 && b[3] == 0x00 {
        return Some("image/x-icon");
    }
    // SVG: 先頭 512 バイトに `<svg` があれば画像扱い。
    let head = &b[..b.len().min(512)];
    let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    if lower.contains("<svg") {
        return Some("image/svg+xml");
    }
    None
}

/// 先頭にヌルバイトを含むものはバイナリとみなす（フロント `bytesLookBinary` と一致）。
pub fn looks_binary(b: &[u8]) -> bool {
    let lim = b.len().min(8192);
    b[..lim].iter().any(|&c| c == 0)
}

// ---------------------------------------------------------------------------
// TextConverter — プレーンテキスト（恒等）
// ---------------------------------------------------------------------------

/// ヌルバイトを含まない（テキストらしい）入力を `core/text` として通す。
/// payload は生 UTF-8 バイトのまま（charset 再エンコードはフロント／既存経路が担う）。
pub struct TextConverter;

#[async_trait]
impl ViewConverter for TextConverter {
    fn converter_id(&self) -> &'static str {
        "core/text"
    }
    fn detect(&self, hint: &FormatHint) -> bool {
        match &hint.first_bytes {
            Some(b) => !looks_binary(b),
            // バイト未知のときは mime ヒントで判断（text/* や代表的なテキスト系）。
            None => hint
                .mime
                .as_deref()
                .map(|m| {
                    let m = m.to_ascii_lowercase();
                    m.starts_with("text/")
                        || m.contains("json")
                        || m.contains("xml")
                        || m.contains("javascript")
                        || m.contains("csv")
                        || m.contains("yaml")
                })
                .unwrap_or(false),
        }
    }
    fn target_view(&self) -> &'static str {
        kinds::TEXT
    }
    fn is_passthrough(&self) -> bool {
        true
    }
    async fn convert(&self, bytes: &[u8], hint: &FormatHint) -> Result<ViewModel, ViewError> {
        let ct = hint
            .mime
            .clone()
            .filter(|m| m.to_ascii_lowercase().contains("charset="))
            .unwrap_or_else(|| "text/plain; charset=utf-8".to_string());
        Ok(ViewModel {
            kind: kinds::TEXT.to_string(),
            content_type: ct,
            payload: bytes.to_vec(),
            meta: serde_json::json!({}),
        })
    }
}

// ---------------------------------------------------------------------------
// ImageConverter — ラスタ／ベクタ画像（恒等）
// ---------------------------------------------------------------------------

/// マジックナンバー／`<svg` で画像と判定し、`core/image` として通す。
pub struct ImageConverter;

#[async_trait]
impl ViewConverter for ImageConverter {
    fn converter_id(&self) -> &'static str {
        "core/image"
    }
    fn detect(&self, hint: &FormatHint) -> bool {
        if let Some(b) = &hint.first_bytes {
            if sniff_image_mime(b).is_some() {
                return true;
            }
        }
        // バイト未知でも image/* の mime ヒントがあれば画像扱い。
        hint.mime
            .as_deref()
            .map(|m| m.to_ascii_lowercase().starts_with("image/"))
            .unwrap_or(false)
    }
    fn target_view(&self) -> &'static str {
        kinds::IMAGE
    }
    fn is_passthrough(&self) -> bool {
        true
    }
    async fn convert(&self, bytes: &[u8], hint: &FormatHint) -> Result<ViewModel, ViewError> {
        let sniffed = sniff_image_mime(bytes);
        let ct = sniffed
            .map(|s| s.to_string())
            .or_else(|| hint.mime.clone())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        Ok(ViewModel {
            kind: kinds::IMAGE.to_string(),
            content_type: ct,
            payload: bytes.to_vec(),
            meta: serde_json::json!({ "sniffed_mime": sniffed }),
        })
    }
}

// ---------------------------------------------------------------------------
// BinaryConverter — フォールバック（任意バイナリ、メタ／16 進）
// ---------------------------------------------------------------------------

/// 常に `detect=true` のフォールバック。未知形式も必ずここへ着地する。
pub struct BinaryConverter;

#[async_trait]
impl ViewConverter for BinaryConverter {
    fn converter_id(&self) -> &'static str {
        "core/binary"
    }
    fn detect(&self, _hint: &FormatHint) -> bool {
        true
    }
    fn target_view(&self) -> &'static str {
        kinds::BINARY
    }
    fn is_passthrough(&self) -> bool {
        true
    }
    async fn convert(&self, bytes: &[u8], hint: &FormatHint) -> Result<ViewModel, ViewError> {
        let ct = hint
            .mime
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string());
        Ok(ViewModel {
            kind: kinds::BINARY.to_string(),
            content_type: ct,
            payload: bytes.to_vec(),
            meta: serde_json::json!({ "size": bytes.len() }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(bytes: &[u8], mime: Option<&str>) -> FormatHint {
        FormatHint {
            extension: None,
            mime: mime.map(|s| s.to_string()),
            first_bytes: Some(bytes.to_vec()),
            display_name: None,
        }
    }

    #[test]
    fn png_resolves_to_image() {
        let reg = ViewRegistry::with_defaults();
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(reg.resolve(&hint(&png, None)).target_view(), kinds::IMAGE);
    }

    #[test]
    fn svg_text_resolves_to_image() {
        let reg = ViewRegistry::with_defaults();
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg'></svg>";
        assert_eq!(reg.resolve(&hint(svg, None)).target_view(), kinds::IMAGE);
    }

    #[test]
    fn plain_text_resolves_to_text() {
        let reg = ViewRegistry::with_defaults();
        let txt = b"hello\nworld\n";
        assert_eq!(reg.resolve(&hint(txt, None)).target_view(), kinds::TEXT);
    }

    #[test]
    fn null_bytes_resolve_to_binary() {
        let reg = ViewRegistry::with_defaults();
        let bin = [0x00, 0x01, 0x02, 0x00, 0xFF];
        assert_eq!(reg.resolve(&hint(&bin, None)).target_view(), kinds::BINARY);
    }

    #[test]
    fn empty_resolves_to_text() {
        // 空ファイルはヌルバイトを含まない → テキスト扱い（既存挙動と一致）。
        let reg = ViewRegistry::with_defaults();
        assert_eq!(reg.resolve(&hint(b"", None)).target_view(), kinds::TEXT);
    }

    #[tokio::test]
    async fn passthrough_preserves_bytes_and_kind() {
        let reg = ViewRegistry::with_defaults();
        let txt = b"abc\ndef";
        let h = hint(txt, None);
        let conv = reg.resolve(&h);
        let model = conv.convert(txt, &h).await.unwrap();
        assert_eq!(model.kind, kinds::TEXT);
        assert_eq!(model.payload, txt);
        assert!(conv.is_passthrough());
    }

    #[test]
    fn mime_hint_without_bytes() {
        let reg = ViewRegistry::with_defaults();
        let h = FormatHint {
            extension: None,
            mime: Some("image/png".to_string()),
            first_bytes: None,
            display_name: None,
        };
        assert_eq!(reg.resolve(&h).target_view(), kinds::IMAGE);
    }
}
