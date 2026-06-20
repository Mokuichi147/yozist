//! yozist-storage — ファイル実体（blob）の保存抽象。
//!
//! # 設計原則
//! - **CAS (Content-Addressed Storage)**: blob はハッシュで識別され、書き換え不可。
//!   「リネーム」「移動」は MetaStore 側で表現する。
//! - **一元管理**: SMB / API / AI どこから書く場合も必ず `BlobStore::put` を経由。
//!   OS ファイルシステムからの直接アクセスは保護されない（ハッシュ名のため非可読）。
//! - **並行性**: 同じ blob を複数タスクから同時に put しても安全（冪等）。
//!
//! # TODO
//! - [ ] `LayeredBlobStore`（小ファイル DB / 大ファイル FS）
//! - [ ] チャンク分割保存とリシール（大容量対応）
//! - [ ] 暗号化レイヤー（at-rest encryption）
//! - [ ] GC（参照されない blob の回収）

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use yozist_core::BlobId;

pub mod fs;
pub use fs::FsBlobStore;

/// blob をストリーミング保存する際の入力ストリーム。
/// HTTP ボディ等を 1 チャンクずつ流し込み、メモリに全体を載せずに保存する。
pub type ByteStream = BoxStream<'static, Result<Bytes, StorageError>>;

/// blob 保存のための統一インターフェース。
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// blob を保存し、コンテンツアドレスを返す。冪等。
    async fn put(&self, content: &[u8]) -> Result<BlobId, StorageError>;
    /// blob を取得する。
    async fn get(&self, id: &BlobId) -> Result<Bytes, StorageError>;
    /// blob の存在確認。
    async fn exists(&self, id: &BlobId) -> Result<bool, StorageError>;

    /// ストリームを逐次保存し、`(コンテンツアドレス, 生バイト長)` を返す。
    ///
    /// デフォルト実装はストリームをメモリに集約してから `put` を呼ぶ
    /// （後方互換のためのフォールバック）。大容量を一定メモリで扱う実装は
    /// このメソッドをオーバーライドする（`FsBlobStore` 参照）。
    async fn put_stream(&self, mut stream: ByteStream) -> Result<(BlobId, u64), StorageError> {
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
        }
        let id = self.put(&buf).await?;
        Ok((id, buf.len() as u64))
    }
}

/// 動的ディスパッチ用の共有エイリアス。
pub type SharedBlobStore = Arc<dyn BlobStore>;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("blob not found: {0}")]
    NotFound(BlobId),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid path: {0}")]
    InvalidPath(PathBuf),
    #[error("other: {0}")]
    Other(String),
}

impl From<StorageError> for yozist_core::Error {
    fn from(e: StorageError) -> Self {
        yozist_core::Error::Storage(e.to_string())
    }
}
