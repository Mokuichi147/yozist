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
    fn supports_streaming(&self) -> bool {
        // LWW は load→serialize が恒等なので、正規化を介さず生バイトを
        // そのまま blob へストリーム保存してよい。
        true
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
// PlainTextCrdt — UTF-8 テキスト用 (yrs バックエンド)
// ---------------------------------------------------------------------------

/// UTF-8 テキスト用 CRDT。内部表現は `yrs::Doc`。
///
/// # 動作
/// - `load(bytes)`: bytes を文字列とみなして yrs Doc に初期コンテンツとして挿入
/// - `apply_ops(state, ops)`: 各 op の bytes を「新しい目標テキスト」として扱い、
///   現在の Doc 内テキストとの差分を yrs Text 操作（insert/remove）に翻訳して適用
/// - `serialize(state)`: yrs Doc 内のテキストをそのまま UTF-8 で返す（blob には
///   平文を格納 — 他ツールから読める互換性を保つ）
/// - `merge(a, b)`: 真の CRDT マージ。両 Doc の state vector を交換し、
///   フレッシュな Doc にどちらの編集も適用して両方の挿入を保持する結果を返す。
///
/// # TODO
/// - [ ] yrs Doc 状態を blob に保存し、コミット毎に状態ベクトル送信で真の差分同期
/// - [ ] BOM / 改行コードの保持
/// - [ ] 巨大ファイルの分割管理
/// - [ ] 同一 file に対する並行コミット（同じ parent からの分岐）を検出して自動マージ
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
        let doc = make_doc_with_text(s);
        Ok(CrdtState { inner: Box::new(doc) })
    }

    async fn apply_ops(
        &self,
        state: &mut CrdtState,
        ops: &[CrdtOp],
    ) -> Result<(), VersioningError> {
        use yrs::{GetString, Transact};

        let doc: &mut yrs::Doc = state
            .inner
            .downcast_mut::<yrs::Doc>()
            .ok_or_else(|| VersioningError::FormatMismatch("yrs::Doc".into()))?;

        for op in ops {
            let new_text = std::str::from_utf8(&op.bytes)
                .map_err(|e| VersioningError::FormatMismatch(e.to_string()))?;
            let text = doc.get_or_insert_text("content");
            let current = {
                let txn = doc.transact();
                text.get_string(&txn)
            };
            // 差分を計算して挿入・削除に翻訳。
            apply_diff_to_text(doc, &text, &current, new_text, op.actor);
        }
        Ok(())
    }

    async fn serialize(&self, state: &CrdtState) -> Result<Vec<u8>, VersioningError> {
        use yrs::{GetString, Transact};
        let doc = state
            .inner
            .downcast_ref::<yrs::Doc>()
            .ok_or_else(|| VersioningError::FormatMismatch("yrs::Doc".into()))?;
        let text = doc.get_or_insert_text("content");
        let txn = doc.transact();
        Ok(text.get_string(&txn).into_bytes())
    }

    async fn merge(
        &self,
        a: &CrdtState,
        b: &CrdtState,
    ) -> Result<CrdtState, VersioningError> {
        use yrs::updates::decoder::Decode;
        use yrs::{ReadTxn, StateVector, Transact, Update};

        let doc_a = a
            .inner
            .downcast_ref::<yrs::Doc>()
            .ok_or_else(|| VersioningError::FormatMismatch("yrs::Doc".into()))?;
        let doc_b = b
            .inner
            .downcast_ref::<yrs::Doc>()
            .ok_or_else(|| VersioningError::FormatMismatch("yrs::Doc".into()))?;

        let update_a = {
            let txn = doc_a.transact();
            txn.encode_state_as_update_v1(&StateVector::default())
        };
        let update_b = {
            let txn = doc_b.transact();
            txn.encode_state_as_update_v1(&StateVector::default())
        };

        let merged = new_doc();
        // "content" Text を事前に作成（両更新内の Text と key が一致する必要がある）
        let _ = merged.get_or_insert_text("content");
        {
            let mut txn = merged.transact_mut();
            let ua = Update::decode_v1(&update_a)
                .map_err(|e| VersioningError::Conflict(format!("decode a: {e}")))?;
            txn.apply_update(ua)
                .map_err(|e| VersioningError::Conflict(format!("apply a: {e}")))?;
            let ub = Update::decode_v1(&update_b)
                .map_err(|e| VersioningError::Conflict(format!("decode b: {e}")))?;
            txn.apply_update(ub)
                .map_err(|e| VersioningError::Conflict(format!("apply b: {e}")))?;
        }
        Ok(CrdtState {
            inner: Box::new(merged),
        })
    }
}

fn make_doc_with_text(text: &str) -> yrs::Doc {
    use yrs::{Text, Transact};
    let doc = new_doc();
    let t = doc.get_or_insert_text("content");
    if !text.is_empty() {
        let mut txn = doc.transact_mut();
        t.insert(&mut txn, 0, text);
    }
    doc
}

/// yrs Doc のデフォルト `OffsetKind` は `Bytes` だが、本クレートでは
/// `apply_diff_to_text` が UTF-16 単位でオフセットを計算しているため、
/// `Utf16` を指定しないと非 ASCII を含むテキストで `remove_range` 等が
/// 文字境界を割って panic する (yrs 内部 `block_offset` のアンダーフロー)。
fn new_doc() -> yrs::Doc {
    use yrs::{OffsetKind, Options};
    let mut opts = Options::default();
    opts.offset_kind = OffsetKind::Utf16;
    yrs::Doc::with_options(opts)
}

/// 現在のテキスト `cur` と目標テキスト `new_text` の差分を yrs Text 操作に翻訳。
///
/// `similar` の `TextDiff` で逐次差分を取り、挿入/削除を Text に直接適用する。
/// yrs Doc の挿入位置は UTF-16 単位なので慎重に変換する。
fn apply_diff_to_text(
    doc: &yrs::Doc,
    text: &yrs::TextRef,
    cur: &str,
    new_text: &str,
    _actor: yozist_core::ActorId,
) {
    use similar::{ChangeTag, TextDiff};
    use yrs::{Text, Transact};

    // 文字単位の差分（UTF-16 では絵文字等で位置がずれるが、テキスト編集としては
    // 「char 列」の挿入/削除の方が直感的）。
    let diff = TextDiff::from_chars(cur, new_text);

    let mut txn = doc.transact_mut();
    let mut pos: u32 = 0; // yrs Text 内の現在位置（UTF-16 unit）
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                let value = change.value();
                pos += utf16_len(value);
            }
            ChangeTag::Delete => {
                let len = utf16_len(change.value());
                text.remove_range(&mut txn, pos, len);
            }
            ChangeTag::Insert => {
                let value = change.value();
                text.insert(&mut txn, pos, value);
                pos += utf16_len(value);
            }
        }
    }
}

fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
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

    use crate::CrdtOp;
    use yozist_core::ActorId;

    #[tokio::test]
    async fn plain_text_load_serialize_roundtrip() {
        let f = PlainTextCrdt;
        let s = f.load(b"hello world").await.unwrap();
        let bytes = f.serialize(&s).await.unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[tokio::test]
    async fn plain_text_apply_diff_replaces_content() {
        let f = PlainTextCrdt;
        let mut s = f.load(b"hello world").await.unwrap();
        f.apply_ops(
            &mut s,
            &[CrdtOp {
                actor: ActorId::new(),
                bytes: bytes::Bytes::from_static(b"hello rust world"),
            }],
        )
        .await
        .unwrap();
        let bytes = f.serialize(&s).await.unwrap();
        assert_eq!(bytes, b"hello rust world");
    }

    #[tokio::test]
    async fn plain_text_apply_diff_handles_multibyte() {
        let f = PlainTextCrdt;
        let mut s = f.load("こんにちは世界".as_bytes()).await.unwrap();
        f.apply_ops(
            &mut s,
            &[CrdtOp {
                actor: ActorId::new(),
                bytes: bytes::Bytes::from_static("こんにちは、世界🎉".as_bytes()),
            }],
        )
        .await
        .unwrap();
        let bytes = f.serialize(&s).await.unwrap();
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "こんにちは、世界🎉");
    }

    #[tokio::test]
    async fn plain_text_merge_preserves_both_inserts() {
        // 共通の base から alice と bob が並行にそれぞれ別の場所に挿入したと仮定。
        // 真の CRDT であれば両方の挿入が結果に残るはず（順序は CRDT 決定）。
        let f = PlainTextCrdt;
        let base = b"hello world";

        let mut a = f.load(base).await.unwrap();
        f.apply_ops(
            &mut a,
            &[CrdtOp {
                actor: ActorId::new(),
                bytes: bytes::Bytes::from_static(b"hello alice world"),
            }],
        )
        .await
        .unwrap();

        let mut b = f.load(base).await.unwrap();
        f.apply_ops(
            &mut b,
            &[CrdtOp {
                actor: ActorId::new(),
                bytes: bytes::Bytes::from_static(b"hello world bob"),
            }],
        )
        .await
        .unwrap();

        let merged = f.merge(&a, &b).await.unwrap();
        let result = f.serialize(&merged).await.unwrap();
        let s = std::str::from_utf8(&result).unwrap();
        // 両方の actor の挿入が含まれていること
        assert!(s.contains("alice"), "result lost alice: {s}");
        assert!(s.contains("bob"), "result lost bob: {s}");
    }
}
