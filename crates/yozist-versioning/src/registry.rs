//! `CrdtRegistry` と初期同梱フォーマット。
//!
//! 拡張するには `Arc::new(MyFormat)` を `register()` で登録するだけ。
//! 該当なしの場合は `LwwFormat` にフォールバック。

use async_trait::async_trait;
use std::sync::Arc;
use yozist_core::FormatHint;

use crate::{CrdtFormat, CrdtOp, CrdtState, VersioningError};

/// プラガブル CRDT レジストリ。
pub struct CrdtRegistry {
    formats: Vec<Arc<dyn CrdtFormat>>,
    fallback: Arc<LwwFormat>,
}

impl CrdtRegistry {
    pub fn new() -> Self {
        Self {
            formats: Vec::new(),
            fallback: Arc::new(LwwFormat),
        }
    }

    /// 初期同梱フォーマットを全て登録した状態。
    pub fn with_defaults() -> Self {
        let mut reg = Self::new();
        reg.register(Arc::new(PlainTextCrdt));
        reg
    }

    pub fn register(&mut self, fmt: Arc<dyn CrdtFormat>) {
        self.formats.push(fmt);
    }

    /// `detect()` が true を返す最初のフォーマットを採用。なければ LWW。
    pub fn resolve(&self, hint: &FormatHint) -> Arc<dyn CrdtFormat> {
        for f in &self.formats {
            if f.detect(hint) {
                return f.clone();
            }
        }
        self.fallback.clone()
    }
}

impl Default for CrdtRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ---------------------------------------------------------------------------
// LwwFormat — フォールバック（任意バイナリ、最終書き込み勝ち）
// ---------------------------------------------------------------------------

pub struct LwwFormat;

#[async_trait]
impl CrdtFormat for LwwFormat {
    fn format_id(&self) -> &'static str {
        "_/lww"
    }
    fn detect(&self, _hint: &FormatHint) -> bool {
        true // フォールバックなので常に true
    }
    async fn load(&self, bytes: &[u8]) -> Result<CrdtState, VersioningError> {
        Ok(CrdtState {
            inner: Box::new(bytes.to_vec()),
        })
    }
    async fn apply_ops(
        &self,
        state: &mut CrdtState,
        ops: &[CrdtOp],
    ) -> Result<(), VersioningError> {
        // LWW: 最新の op で完全置き換え
        if let Some(last) = ops.last() {
            state.inner = Box::new(last.bytes.to_vec());
        }
        Ok(())
    }
    async fn serialize(&self, state: &CrdtState) -> Result<Vec<u8>, VersioningError> {
        state
            .inner
            .downcast_ref::<Vec<u8>>()
            .cloned()
            .ok_or_else(|| VersioningError::FormatMismatch("LWW state".into()))
    }
    async fn merge(
        &self,
        _a: &CrdtState,
        b: &CrdtState,
    ) -> Result<CrdtState, VersioningError> {
        // LWW: 後勝ち
        let bytes = b
            .inner
            .downcast_ref::<Vec<u8>>()
            .cloned()
            .ok_or_else(|| VersioningError::FormatMismatch("LWW state".into()))?;
        Ok(CrdtState {
            inner: Box::new(bytes),
        })
    }
}

// ---------------------------------------------------------------------------
// PlainTextCrdt — UTF-8 テキスト用（スケルトン）
// ---------------------------------------------------------------------------

/// UTF-8 テキスト用 CRDT。
///
/// # TODO
/// - [ ] yrs クレート統合（Yjs Rust port）で真の並行編集対応
/// - [ ] BOM / 改行コードの保持
/// - [ ] 巨大ファイルの分割管理
pub struct PlainTextCrdt;

#[async_trait]
impl CrdtFormat for PlainTextCrdt {
    fn format_id(&self) -> &'static str {
        "text/plain"
    }
    fn detect(&self, hint: &FormatHint) -> bool {
        if let Some(m) = &hint.mime {
            if m.starts_with("text/") {
                return true;
            }
        }
        if let Some(ext) = &hint.extension {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "txt" | "md" | "markdown" | "rs" | "ts" | "js" | "py" | "go" | "c" | "h"
                    | "cpp" | "hpp" | "java" | "kt" | "swift" | "rb" | "css" | "html"
                    | "xml" | "yaml" | "yml" | "toml" | "ini" | "csv" | "tsv" | "log"
            )
        } else {
            false
        }
    }
    async fn load(&self, bytes: &[u8]) -> Result<CrdtState, VersioningError> {
        let s = std::str::from_utf8(bytes)
            .map_err(|e| VersioningError::FormatMismatch(format!("invalid utf-8: {e}")))?;
        Ok(CrdtState {
            inner: Box::new(s.to_string()),
        })
    }
    async fn apply_ops(
        &self,
        state: &mut CrdtState,
        ops: &[CrdtOp],
    ) -> Result<(), VersioningError> {
        // スケルトン: 最終 op で置き換え。
        // TODO: yrs ベースの真の OpLog 適用に置換。
        if let Some(last) = ops.last() {
            let s = std::str::from_utf8(&last.bytes)
                .map_err(|e| VersioningError::FormatMismatch(e.to_string()))?
                .to_string();
            state.inner = Box::new(s);
        }
        Ok(())
    }
    async fn serialize(&self, state: &CrdtState) -> Result<Vec<u8>, VersioningError> {
        state
            .inner
            .downcast_ref::<String>()
            .map(|s| s.as_bytes().to_vec())
            .ok_or_else(|| VersioningError::FormatMismatch("text state".into()))
    }
    async fn merge(
        &self,
        _a: &CrdtState,
        b: &CrdtState,
    ) -> Result<CrdtState, VersioningError> {
        // スケルトン: 後勝ち。TODO: yrs の真のマージへ。
        let s = b
            .inner
            .downcast_ref::<String>()
            .cloned()
            .ok_or_else(|| VersioningError::FormatMismatch("text state".into()))?;
        Ok(CrdtState { inner: Box::new(s) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_falls_back_to_lww_for_unknown() {
        let reg = CrdtRegistry::with_defaults();
        let hint = FormatHint {
            extension: Some("bin".into()),
            ..Default::default()
        };
        let f = reg.resolve(&hint);
        assert_eq!(f.format_id(), "_/lww");
    }

    #[test]
    fn registry_picks_plain_text_for_md() {
        let reg = CrdtRegistry::with_defaults();
        let hint = FormatHint {
            extension: Some("md".into()),
            ..Default::default()
        };
        let f = reg.resolve(&hint);
        assert_eq!(f.format_id(), "text/plain");
    }
}
