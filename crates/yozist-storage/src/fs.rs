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

use async_compression::tokio::write::ZstdEncoder;
use async_compression::Level;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use yozist_core::BlobId;

use crate::{BlobStore, ByteStream, StorageError};

const ZSTD_LEVEL: i32 = 3;

/// `put_stream` の一時ファイル名を同一プロセス内で衝突させないための連番。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// パスが載っているファイルシステムの容量情報。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    /// 非特権プロセスが書き込み可能な空きバイト数（`f_bavail`）。
    pub available_bytes: u64,
    /// ファイルシステム全体の総バイト数。
    pub total_bytes: u64,
}

/// `path` が属するファイルシステムの空き／総容量を返す。
///
/// `path` 自体がまだ存在しない場合は、存在する祖先ディレクトリを辿って問い合わせる。
/// 見つからなければカレントディレクトリを使う。
pub fn disk_space(path: &Path) -> Result<DiskSpace, StorageError> {
    let probe = resolve_existing_ancestor(path);
    disk_space_of(&probe)
}

/// 存在する祖先ディレクトリ（または `.`）を返す。
fn resolve_existing_ancestor(path: &Path) -> PathBuf {
    let mut cur = path.to_path_buf();
    loop {
        if cur.exists() {
            return cur;
        }
        if !cur.pop() {
            return PathBuf::from(".");
        }
    }
}

#[cfg(unix)]
fn disk_space_of(path: &Path) -> Result<DiskSpace, StorageError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        StorageError::InvalidPath(path.to_path_buf())
    })?;
    // SAFETY: c_path は NUL 終端、stat はゼロ初期化済み。statvfs は成功時 0 を返す。
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(StorageError::Io(std::io::Error::last_os_error()));
    }
    let frsize = stat.f_frsize as u64;
    Ok(DiskSpace {
        available_bytes: stat.f_bavail as u64 * frsize,
        total_bytes: stat.f_blocks as u64 * frsize,
    })
}

#[cfg(not(unix))]
fn disk_space_of(_path: &Path) -> Result<DiskSpace, StorageError> {
    Err(StorageError::Other(
        "disk space query is not supported on this platform".into(),
    ))
}

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

    /// blob ルートディレクトリのパス。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// blob ルートが載っているファイルシステムの空き／総容量。
    pub fn disk_space(&self) -> Result<DiskSpace, StorageError> {
        disk_space(&self.root)
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

    /// `put_stream` 用の一時ファイル置き場（`<root>/.tmp`）。
    fn tmp_dir(&self) -> PathBuf {
        self.root.join(".tmp")
    }

    /// 同一プロセス内で衝突しない一時ファイルパスを返す。
    fn unique_tmp_path(&self) -> PathBuf {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.tmp_dir()
            .join(format!("{}-{}-{}.tmp", std::process::id(), nanos, seq))
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

    async fn delete(&self, id: &BlobId) -> Result<(), StorageError> {
        match fs::remove_file(self.blob_path(id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    /// ストリームを逐次 zstd 圧縮しながら一時ファイルへ書き込み、生バイトの
    /// sha256 を逐次計算する。完了後にコンテンツアドレスへ rename する。
    /// オンディスク形式は `put` と同じ単一 zstd フレームなので `get` は無変更。
    async fn put_stream(&self, mut stream: ByteStream) -> Result<(BlobId, u64), StorageError> {
        let tmp_dir = self.tmp_dir();
        fs::create_dir_all(&tmp_dir).await?;
        let tmp = self.unique_tmp_path();

        let file = fs::File::create(&tmp).await?;
        let mut encoder = ZstdEncoder::with_quality(file, Level::Precise(ZSTD_LEVEL));
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;

        // 書き込みループ。途中失敗時は一時ファイルを掃除して返す。
        let write_result = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                hasher.update(&chunk);
                total += chunk.len() as u64;
                encoder.write_all(&chunk).await?;
            }
            encoder.shutdown().await?;
            Ok::<(), StorageError>(())
        }
        .await;

        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp).await;
            return Err(e);
        }

        let id = BlobId::from_hex(hex_encode(&hasher.finalize()));
        let path = self.blob_path(&id);

        // 既存（同一内容）なら一時ファイルを破棄して冪等に返す。
        if fs::try_exists(&path).await.unwrap_or(false) {
            let _ = fs::remove_file(&tmp).await;
            return Ok((id, total));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::rename(&tmp, &path).await?;
        Ok((id, total))
    }
}

/// 大きな本文は圧縮レベルを下げて保存（コミット）のレイテンシを抑える。
/// zstd レベル 1 はレベル 3 の概ね 2〜3 倍速く、圧縮率の差は数 % 程度。
/// 巨大ファイルの部分編集でも保存が待たされないことを優先する。
const ZSTD_FAST_THRESHOLD: usize = 8 * 1024 * 1024;
const ZSTD_FAST_LEVEL: i32 = 1;

fn compress(input: &[u8]) -> Result<Vec<u8>, StorageError> {
    let level = if input.len() >= ZSTD_FAST_THRESHOLD {
        ZSTD_FAST_LEVEL
    } else {
        ZSTD_LEVEL
    };
    zstd::stream::encode_all(input, level).map_err(StorageError::Io)
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
    async fn delete_removes_blob_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path()).await.unwrap();
        let id = store.put(b"to be removed").await.unwrap();
        assert!(store.exists(&id).await.unwrap());
        store.delete(&id).await.unwrap();
        assert!(!store.exists(&id).await.unwrap());
        // 既に無い blob の削除は冪等に成功する。
        store.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn put_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path()).await.unwrap();
        let a = store.put(b"same").await.unwrap();
        let b = store.put(b"same").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn put_stream_roundtrip_matches_put() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path()).await.unwrap();

        // 複数チャンクに分割したストリームを保存。
        let chunks: Vec<Result<Bytes, StorageError>> = vec![
            Ok(Bytes::from_static(b"hello ")),
            Ok(Bytes::from_static(b"streamed ")),
            Ok(Bytes::from_static(b"yozist")),
        ];
        let stream = futures::stream::iter(chunks).boxed();
        let (id, size) = store.put_stream(stream).await.unwrap();

        // 内容・サイズが復元でき、同一バイトを `put` した結果と同じアドレスになる。
        let got = store.get(&id).await.unwrap();
        assert_eq!(&got[..], b"hello streamed yozist");
        assert_eq!(size, b"hello streamed yozist".len() as u64);
        let put_id = store.put(b"hello streamed yozist").await.unwrap();
        assert_eq!(id, put_id);
    }

    #[tokio::test]
    async fn put_stream_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path()).await.unwrap();
        let mk = || futures::stream::iter(vec![Ok::<_, StorageError>(Bytes::from_static(b"dup"))]).boxed();
        let (a, _) = store.put_stream(mk()).await.unwrap();
        let (b, _) = store.put_stream(mk()).await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn disk_space_reports_positive_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path()).await.unwrap();
        let space = store.disk_space().unwrap();
        // 総容量は 0 より大きく、空きは総容量以下。
        assert!(space.total_bytes > 0);
        assert!(space.available_bytes <= space.total_bytes);
        // 未作成の子孫パスでも祖先を辿って問い合わせられる。
        let nested = disk_space(&dir.path().join("does-not-exist-yet")).unwrap();
        assert_eq!(nested.total_bytes, space.total_bytes);
    }
}
