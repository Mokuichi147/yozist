//! yozist-versioning — コミット履歴 + プラガブル CRDT/LWW マージエンジン。
//!
//! # 設計原則
//! - **拡張可能**: `CrdtFormat` トレイトを実装すれば対応フォーマットを増やせる。
//!   サードパーティクレートからの登録も想定。
//! - **書き込みの単一経路**: SMB/API/AI のどこから書く場合も `commit()` を経由。
//! - **並行性**: テキストは CRDT（自動マージ）、バイナリは LWW（最終書き込み勝ち）。
//!
//! # TODO
//! - [ ] `PlainTextCrdt`（yrs ベース）の本実装
//! - [ ] Markdown / JSON / CSV CRDT
//! - [ ] commit DAG（merge コミット）対応
//! - [ ] スナップショット圧縮間隔（N コミット毎にフル保存）
//! - [ ] `broadcast` チャネルによる変更通知

use async_trait::async_trait;
use std::sync::Arc;
use yozist_core::{ActorId, BlobId, FormatHint};

pub mod registry;
pub use registry::{CrdtRegistry, LwwFormat, PlainTextCrdt};

/// CRDT 状態。フォーマット実装側が任意の内部表現を保持する。
pub struct CrdtState {
    pub inner: Box<dyn std::any::Any + Send + Sync>,
}

impl std::fmt::Debug for CrdtState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CrdtState{{..}}")
    }
}

/// 編集操作（OpLog の 1 要素）。
#[derive(Debug, Clone)]
pub struct CrdtOp {
    pub actor: ActorId,
    pub bytes: bytes::Bytes,
}

/// 1 つのファイル種別を扱う CRDT/マージ実装。
#[async_trait]
pub trait CrdtFormat: Send + Sync {
    /// MIME 風の識別子（例: `text/plain`, `application/json`, `_/lww`）
    fn format_id(&self) -> &'static str;
    /// このフォーマットで処理すべきかどうか。
    fn detect(&self, hint: &FormatHint) -> bool;
    /// バイト列を CRDT 状態に取り込む。
    async fn load(&self, bytes: &[u8]) -> Result<CrdtState, VersioningError>;
    /// 編集操作（OpLog）を適用。
    async fn apply_ops(
        &self,
        state: &mut CrdtState,
        ops: &[CrdtOp],
    ) -> Result<(), VersioningError>;
    /// CRDT 状態をシリアライズ。
    async fn serialize(&self, state: &CrdtState) -> Result<Vec<u8>, VersioningError>;
    /// 2 つの状態を競合無くマージ。
    async fn merge(
        &self,
        a: &CrdtState,
        b: &CrdtState,
    ) -> Result<CrdtState, VersioningError>;
}

pub type SharedCrdtFormat = Arc<dyn CrdtFormat>;

/// バージョニングエンジン。コミット時にフォーマットを解決し、blob 保存と
/// メタストアへの commit 記録を行う。
pub struct VersioningEngine {
    pub registry: Arc<CrdtRegistry>,
}

impl VersioningEngine {
    pub fn new(registry: Arc<CrdtRegistry>) -> Self {
        Self { registry }
    }

    /// 新規コミット（スケルトン）。
    ///
    /// TODO: blob 保存 + commit 行 insert + 親コミット解決 + CRDT op 生成
    pub async fn commit(
        &self,
        _hint: &FormatHint,
        _new_bytes: &[u8],
        _actor: ActorId,
    ) -> Result<BlobId, VersioningError> {
        Err(VersioningError::NotImplemented("commit"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VersioningError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("format mismatch: {0}")]
    FormatMismatch(String),
    #[error("merge conflict: {0}")]
    Conflict(String),
    #[error("storage error: {0}")]
    Storage(#[from] yozist_storage::StorageError),
    #[error("db error: {0}")]
    Db(#[from] yozist_db::DbError),
}

impl From<VersioningError> for yozist_core::Error {
    fn from(e: VersioningError) -> Self {
        yozist_core::Error::Versioning(e.to_string())
    }
}
