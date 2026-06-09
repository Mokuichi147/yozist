//! zip アーカイブの中身を「フォルダのように」閲覧・操作するためのヘルパー。
//!
//! yozist は zip を `application/zip` の不透明な blob（LWW）として 1 ファイル単位で
//! 保持する。本モジュールは blob バイト列を入力に、エントリ一覧の取得・個別エントリの
//! 取り出し・エントリの移動/削除/追加を行う純粋関数群を提供する。
//!
//! # 編集の保存方針
//! 移動・削除・追加は「zip を再構築した新しいバイト列」を返すだけで、永続化は行わない。
//! 呼び出し側（API ハンドラ）が `VersioningEngine::commit` を通して新コミット 1 件として
//! 記録する（= 履歴が残り、書き込みの単一経路を守る）。
//!
//! # 圧縮の保持
//! 変更しないエントリは `raw_copy_file*` で再圧縮せずそのままコピーする。新規追加分のみ
//! Deflate で圧縮する。これで巨大アーカイブの一部編集でも全再圧縮を避けられる。

use std::io::{Cursor, Read, Write};

use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

/// zip エントリ 1 件のメタ情報（WebUI へ JSON で返す）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ZipEntry {
    /// zip 内のフルパス（例: `dir/sub/file.txt`）。ディレクトリは末尾 `/` を除いた形。
    pub path: String,
    /// ディレクトリエントリかどうか。
    pub is_dir: bool,
    /// 非圧縮サイズ（bytes）。
    pub size: u64,
    /// 圧縮後サイズ（bytes）。
    pub compressed_size: u64,
    /// 最終更新日時（`YYYY-MM-DD HH:MM:SS`）。取得できなければ `None`。
    pub modified: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ZipOpError {
    #[error("zip ではない、または壊れています: {0}")]
    Invalid(String),
    #[error("エントリが見つかりません: {0}")]
    NotFound(String),
    #[error("不正なパスです: {0}")]
    BadPath(String),
    #[error("既に存在します: {0}")]
    Exists(String),
    #[error("io error: {0}")]
    Io(String),
}

impl From<zip::result::ZipError> for ZipOpError {
    fn from(e: zip::result::ZipError) -> Self {
        match e {
            zip::result::ZipError::FileNotFound => ZipOpError::NotFound("(no name)".into()),
            other => ZipOpError::Invalid(other.to_string()),
        }
    }
}

impl From<std::io::Error> for ZipOpError {
    fn from(e: std::io::Error) -> Self {
        ZipOpError::Io(e.to_string())
    }
}

/// 先頭バイトが zip のローカルファイルヘッダ（`PK\x03\x04`）か、空 zip（`PK\x05\x06`）か。
pub fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[0..2] == b"PK" && matches!(bytes[2], 0x03 | 0x05 | 0x07)
}

/// パスを正規化し、安全性を検証する。
///
/// - バックスラッシュを `/` に統一
/// - 先頭/末尾の `/` を除去
/// - 空・絶対パス・`..` を含むものは拒否（zip slip 対策）
fn normalize_path(raw: &str) -> Result<String, ZipOpError> {
    let p = raw.replace('\\', "/");
    let p = p.trim_matches('/').to_string();
    if p.is_empty() {
        return Err(ZipOpError::BadPath("空のパス".into()));
    }
    for comp in p.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." {
            return Err(ZipOpError::BadPath(raw.to_string()));
        }
    }
    Ok(p)
}

/// zip のバイト列からエントリ一覧を返す。
pub fn list_entries(bytes: &[u8]) -> Result<Vec<ZipEntry>, ZipOpError> {
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| ZipOpError::Invalid(e.to_string()))?;
    let mut out = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let f = archive.by_index(i)?;
        let is_dir = f.is_dir();
        let name = f.name().trim_end_matches('/').to_string();
        let modified = f.last_modified().and_then(format_dt);
        out.push(ZipEntry {
            path: name,
            is_dir,
            size: f.size(),
            compressed_size: f.compressed_size(),
            modified,
        });
    }
    Ok(out)
}

/// 単一エントリの非圧縮バイト列を返す。
pub fn read_entry(bytes: &[u8], path: &str) -> Result<Vec<u8>, ZipOpError> {
    let path = normalize_path(path)?;
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| ZipOpError::Invalid(e.to_string()))?;
    let mut f = archive
        .by_name(&path)
        .map_err(|_| ZipOpError::NotFound(path.clone()))?;
    if f.is_dir() {
        return Err(ZipOpError::BadPath(format!("{path} はディレクトリです")));
    }
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// 指定パス（ファイル or ディレクトリ）を削除した新しい zip バイト列を返す。
///
/// `target` がディレクトリの場合、その配下のエントリもすべて削除する。
pub fn delete_path(bytes: &[u8], target: &str) -> Result<Vec<u8>, ZipOpError> {
    let target = normalize_path(target)?;
    let dir_prefix = format!("{target}/");
    let mut removed = false;
    let out = rebuild(bytes, |name| {
        let n = name.trim_end_matches('/');
        if n == target || name.starts_with(&dir_prefix) {
            removed = true;
            CopyAction::Drop
        } else {
            CopyAction::Keep
        }
    })?;
    if !removed {
        return Err(ZipOpError::NotFound(target));
    }
    Ok(out)
}

/// エントリ（ファイル or ディレクトリ）を移動/リネームした新しい zip バイト列を返す。
///
/// `from` がディレクトリの場合、配下のエントリのパス接頭辞をまとめて付け替える。
pub fn move_path(bytes: &[u8], from: &str, to: &str) -> Result<Vec<u8>, ZipOpError> {
    let from = normalize_path(from)?;
    let to = normalize_path(to)?;
    if from == to {
        return Err(ZipOpError::BadPath("移動元と移動先が同じです".into()));
    }
    let from_dir = format!("{from}/");
    let to_dir = format!("{to}/");

    // 移動先に既存エントリがあると衝突するため事前に検査する。
    {
        let archive =
            ZipArchive::new(Cursor::new(bytes)).map_err(|e| ZipOpError::Invalid(e.to_string()))?;
        let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
        for n in &names {
            let trimmed = n.trim_end_matches('/');
            if trimmed == to || n.starts_with(&to_dir) {
                return Err(ZipOpError::Exists(to.clone()));
            }
        }
        // from が存在するかも確認。
        let exists = names.iter().any(|n| {
            let t = n.trim_end_matches('/');
            t == from || n.starts_with(&from_dir)
        });
        drop(archive);
        if !exists {
            return Err(ZipOpError::NotFound(from.clone()));
        }
    }

    rebuild(bytes, |name| {
        let trimmed = name.trim_end_matches('/');
        let trailing = if name.ends_with('/') { "/" } else { "" };
        if trimmed == from {
            CopyAction::Rename(format!("{to}{trailing}"))
        } else if name.starts_with(&from_dir) {
            let rest = &name[from_dir.len()..];
            CopyAction::Rename(format!("{to_dir}{rest}"))
        } else {
            CopyAction::Keep
        }
    })
}

/// エントリを追加（同名があれば置換）した新しい zip バイト列を返す。
///
/// `path` が末尾 `/` ならディレクトリ作成。それ以外は `data` を本文とするファイル。
pub fn add_entry(bytes: &[u8], path: &str, data: &[u8]) -> Result<Vec<u8>, ZipOpError> {
    let is_dir = path.ends_with('/');
    let norm = normalize_path(path)?;
    let entry_name = if is_dir {
        format!("{norm}/")
    } else {
        norm.clone()
    };

    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let buf = Vec::new();
    let mut writer = ZipWriter::new(Cursor::new(buf));

    // 既存エントリを再圧縮せずコピー（同名は捨てて後で新規追加 = 置換）。
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| ZipOpError::Invalid(e.to_string()))?;
    for i in 0..archive.len() {
        let raw = archive.by_index_raw(i)?;
        if raw.name().trim_end_matches('/') == norm {
            continue; // 同名は置換するためスキップ
        }
        writer.raw_copy_file(raw)?;
    }

    if is_dir {
        writer.add_directory(&entry_name, opts)?;
    } else {
        writer.start_file(&entry_name, opts)?;
        writer.write_all(data)?;
    }
    let cursor = writer.finish().map_err(|e| ZipOpError::Io(e.to_string()))?;
    Ok(cursor.into_inner())
}

/// 各エントリに対する操作。
enum CopyAction {
    Keep,
    Drop,
    Rename(String),
}

/// 既存エントリを再圧縮せずに走査し、`decide` の指示でコピー/除外/改名して
/// 新しい zip バイト列を構築する共通ルーチン。
fn rebuild(
    bytes: &[u8],
    mut decide: impl FnMut(&str) -> CopyAction,
) -> Result<Vec<u8>, ZipOpError> {
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| ZipOpError::Invalid(e.to_string()))?;
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..archive.len() {
        let raw = archive.by_index_raw(i)?;
        match decide(raw.name()) {
            CopyAction::Keep => writer.raw_copy_file(raw)?,
            CopyAction::Drop => {}
            CopyAction::Rename(new_name) => writer.raw_copy_file_rename(raw, &new_name)?,
        }
    }
    let cursor = writer.finish().map_err(|e| ZipOpError::Io(e.to_string()))?;
    Ok(cursor.into_inner())
}

/// zip の `DateTime` を `YYYY-MM-DD HH:MM:SS` 文字列へ。範囲外なら `None`。
fn format_dt(dt: zip::DateTime) -> Option<String> {
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用に最小の zip を組み立てる。
    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in files {
            if name.ends_with('/') {
                w.add_directory(*name, opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn list_and_read_roundtrip() {
        let z = make_zip(&[("a.txt", b"hello"), ("dir/b.txt", b"world")]);
        assert!(looks_like_zip(&z));
        let entries = list_entries(&z).unwrap();
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"a.txt"));
        assert!(paths.contains(&"dir/b.txt"));
        assert_eq!(read_entry(&z, "a.txt").unwrap(), b"hello");
        assert_eq!(read_entry(&z, "dir/b.txt").unwrap(), b"world");
    }

    #[test]
    fn delete_file_and_dir() {
        let z = make_zip(&[("a.txt", b"a"), ("dir/b.txt", b"b"), ("dir/c.txt", b"c")]);
        let z2 = delete_path(&z, "a.txt").unwrap();
        assert!(read_entry(&z2, "a.txt").is_err());
        assert_eq!(read_entry(&z2, "dir/b.txt").unwrap(), b"b");

        // ディレクトリ削除は配下も消える。
        let z3 = delete_path(&z, "dir").unwrap();
        assert!(read_entry(&z3, "dir/b.txt").is_err());
        assert_eq!(read_entry(&z3, "a.txt").unwrap(), b"a");

        assert!(delete_path(&z, "missing.txt").is_err());
    }

    #[test]
    fn move_file_and_dir() {
        let z = make_zip(&[("a.txt", b"a"), ("dir/b.txt", b"b")]);
        let z2 = move_path(&z, "a.txt", "renamed.txt").unwrap();
        assert!(read_entry(&z2, "a.txt").is_err());
        assert_eq!(read_entry(&z2, "renamed.txt").unwrap(), b"a");

        // ディレクトリ配下の接頭辞ごと付け替え。
        let z3 = move_path(&z, "dir", "moved").unwrap();
        assert_eq!(read_entry(&z3, "moved/b.txt").unwrap(), b"b");
        assert!(read_entry(&z3, "dir/b.txt").is_err());

        // 衝突は拒否。
        let z4 = make_zip(&[("a.txt", b"a"), ("b.txt", b"b")]);
        assert!(matches!(
            move_path(&z4, "a.txt", "b.txt"),
            Err(ZipOpError::Exists(_))
        ));
    }

    #[test]
    fn add_and_replace() {
        let z = make_zip(&[("a.txt", b"a")]);
        let z2 = add_entry(&z, "new/x.txt", b"x").unwrap();
        assert_eq!(read_entry(&z2, "new/x.txt").unwrap(), b"x");
        assert_eq!(read_entry(&z2, "a.txt").unwrap(), b"a");

        // 同名は置換。
        let z3 = add_entry(&z, "a.txt", b"updated").unwrap();
        assert_eq!(read_entry(&z3, "a.txt").unwrap(), b"updated");
    }

    #[test]
    fn rejects_path_traversal() {
        let z = make_zip(&[("a.txt", b"a")]);
        assert!(read_entry(&z, "../etc/passwd").is_err());
        assert!(delete_path(&z, "../x").is_err());
        assert!(add_entry(&z, "a/../../x", b"x").is_err());
        // 先頭スラッシュは除去され安全な相対パスとして扱われる（traversal ではない）。
        assert_eq!(
            add_entry(&z, "/abs", b"x")
                .map(|z| read_entry(&z, "abs").unwrap())
                .unwrap(),
            b"x"
        );
    }
}
