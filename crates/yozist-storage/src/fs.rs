//! ファイルシステム上の CAS (Content-Addressed Storage) 実装。
//!
//! - blob 名は `sha256(content)` の hex。
//! - 拡張: 先頭 2 文字でディレクトリ分割（`ab/cdef...`）。
//! - 圧縮: zstd レベル 3 で透過圧縮（TODO の余地: 既に圧縮済みの形式は素通し）。
//!
//! # TODO
//! - [ ] zstd レベル / 閾値の設定化
//! - [ ] 既圧縮ファイル（mp4/zip 等）の検出と無圧縮保存
//! - [ ] ファイルロック（複数プロセス起動時）

use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use yozist_core::BlobId;

use crate::{BlobStore, StorageError};

const ZSTD_LEVEL: i32 = 3;

/// ローカルファイルシステム上の CAS 実装。
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    pub async fn new(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();
        fs::create_dir_all(&root).await?;
        Ok(Self { root })
    }

    fn blob_path(&self, id: &BlobId) -> PathBuf {
        let s = id.as_str();
        let (a, b) = if s.len() >= 2 { s.split_at(2) } else { (s, "") };
        self.root.join(a).join(b)
    }

    fn hash(content: &[u8]) -> BlobId {
        let digest = Sha256::digest(content);
        BlobId::from_hex(hex_encode(&digest))
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, content: &[u8]) -> Result<BlobId, StorageError> {
        let id = Self::hash(content);
        let path = self.blob_path(&id);

        if fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(id);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let compressed = compress(content)?;
        let tmp = path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp).await?;
            f.write_all(&compressed).await?;
            f.flush().await?;
        }
        fs::rename(&tmp, &path).await?;
        Ok(id)
    }

    async fn get(&self, id: &BlobId) -> Result<Bytes, StorageError> {
        let path = self.blob_path(id);
        let bytes = match fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(id.clone()));
            }
            Err(e) => return Err(StorageError::Io(e)),
        };
        let decompressed = decompress(&bytes)?;
        Ok(Bytes::from(decompressed))
    }

    async fn exists(&self, id: &BlobId) -> Result<bool, StorageError> {
        Ok(fs::try_exists(self.blob_path(id)).await.unwrap_or(false))
    }
}

fn compress(input: &[u8]) -> Result<Vec<u8>, StorageError> {
    zstd::stream::encode_all(input, ZSTD_LEVEL).map_err(StorageError::Io)
}

fn decompress(input: &[u8]) -> Result<Vec<u8>, StorageError> {
    zstd::stream::decode_all(input).map_err(StorageError::Io)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[allow(dead_code)]
fn _root_marker(p: &Path) -> &Path {
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_and_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path()).await.unwrap();
        let id = store.put(b"hello yozist").await.unwrap();
        let got = store.get(&id).await.unwrap();
        assert_eq!(&got[..], b"hello yozist");
        assert!(store.exists(&id).await.unwrap());
    }

    #[tokio::test]
    async fn put_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path()).await.unwrap();
        let a = store.put(b"same").await.unwrap();
        let b = store.put(b"same").await.unwrap();
        assert_eq!(a, b);
    }
}
