//! リサイズ + 圧縮による軽量プレビュー生成。
//!
//! compressor 自体は解像度リサイズ機能を持たないため、リサイズは `image` crate で
//! 行い、最終圧縮は compressor の既存 `pub` API（`rgb_image`/`rgba_image` の
//! `path2compress`）に委ねる。compressor は bytes を直接返す API を持たないため、
//! リサイズ結果を一旦 PNG として一時ファイルに書き出し、それを compressor に渡す。
//!
//! 中間ファイルは全て `.tmp-` 前置で同じディレクトリに置く。最終成果物だけは
//! rename で差し替えるため、配信中のパスに書きかけのバイト列が現れることはない。

use image::imageops::FilterType;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::{OutputFormat, VariantConfig};

/// デコードを許可する 1 辺の最大 px。
///
/// 入力は利用者がアップロードした任意のバイト列なので、寸法ヘッダだけ巨大な
/// 画像（decompression bomb）を無制限にデコードさせない。`image` の既定は
/// 寸法無制限・`max_alloc` 512MiB のみで、ワーカー本数ぶん並行するとその
/// 何倍もの一時確保が起きうる。寸法制限は strict limit なので、確保前に弾ける。
///
/// 1 億 6 千万画素相当。現行の民生機（最大でも 1 億画素級）には十分な余裕がある。
const MAX_DECODE_EDGE_PX: u32 = 16_384;

/// デコーダが一度に確保できるバイト数の上限。`image` の既定値と同じだが、
/// 既定の変更に左右されないよう明示する。
const MAX_DECODE_ALLOC_BYTES: u64 = 512 * 1024 * 1024;

/// 上限付きで画像をデコードする。
fn decode_limited(bytes: &[u8]) -> Result<image::DynamicImage, GenError> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE_PX);
    limits.max_image_height = Some(MAX_DECODE_EDGE_PX);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);

    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| GenError::Unsupported(e.to_string()))?;
    reader.limits(limits);
    // 上限超過も「この入力は扱えない」という恒久失敗なので Unsupported に畳む
    // （リトライしても同じ結果にしかならない）。
    reader
        .decode()
        .map_err(|e| GenError::Unsupported(e.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum GenError {
    #[error("unsupported or undecodable image: {0}")]
    Unsupported(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct GeneratedPreview {
    pub path: PathBuf,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub byte_size: u64,
}

pub struct PreviewGenerator;

impl PreviewGenerator {
    /// `bytes`（元画像）から軽量プレビューを生成し、`dest_dir/<base_name>.{jpg,png}`
    /// に書き込む。呼び出し側（CPU バウンドなので）は `spawn_blocking` で包むこと。
    pub fn generate(
        bytes: &[u8],
        dest_dir: &Path,
        base_name: &str,
        cfg: VariantConfig,
    ) -> Result<GeneratedPreview, GenError> {
        std::fs::create_dir_all(dest_dir)?;

        let img = decode_limited(bytes)?;
        let (orig_w, orig_h) = (img.width(), img.height());
        if orig_w == 0 || orig_h == 0 {
            return Err(GenError::Unsupported("empty image dimensions".into()));
        }

        let long_edge = orig_w.max(orig_h);
        let resized = if long_edge > cfg.max_edge_px {
            let scale = cfg.max_edge_px as f32 / long_edge as f32;
            let new_w = ((orig_w as f32 * scale).round() as u32).max(1);
            let new_h = ((orig_h as f32 * scale).round() as u32).max(1);
            img.resize(new_w, new_h, FilterType::Lanczos3)
        } else {
            img
        };
        let (width, height) = (resized.width(), resized.height());
        let has_alpha = resized.color().has_alpha();

        // 中間ファイルは 1 回の生成で共通の stem を使い、掃除の目印として
        // `.tmp-` を前置する。
        let stem = Uuid::new_v4().simple().to_string();

        // compressor の path2compress 系は入力をファイルパスで受け取るため、
        // リサイズ結果を一旦 PNG（可逆）で一時ファイルに書き出す。
        let tmp_src = dest_dir.join(format!(".tmp-{stem}-src.png"));
        let write_result = resized.save_with_format(&tmp_src, image::ImageFormat::Png);
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp_src);
            return Err(GenError::Unsupported(format!("failed to encode intermediate png: {e}")));
        }

        let (ext, mime) = match (cfg.format, has_alpha) {
            (OutputFormat::Webp, _) => ("webp", "image/webp"),
            (OutputFormat::Auto, true) => ("png", "image/png"),
            (OutputFormat::Auto, false) => ("jpg", "image/jpeg"),
        };

        // 圧縮結果も一旦一時ファイルへ出す。配信中のパスへ直接書くと、
        // `get_preview` が書きかけのバイト列を読んで壊れた画像を返しうる。
        // 固着ジョブの回収（`yozist_jobs::STALLED_LEASE`）が生きているジョブを
        // 二重に走らせた場合も、両者が同じ出力パスへ同時に書き込むことになる。
        let tmp_out = dest_dir.join(format!(".tmp-{stem}-out.{ext}"));
        match (cfg.format, has_alpha) {
            // アルファ付き非可逆 WebP は compressor が未公開のため可逆で出す
            // （Mokuichi147/compressor#3）。それでも PNG よりは小さい。
            (OutputFormat::Webp, true) => {
                compressor::webp_image::path2compress_lossless(&tmp_src, &tmp_out)
            }
            (OutputFormat::Webp, false) => {
                compressor::webp_image::path2compress_lossy(&tmp_src, &tmp_out, cfg.quality)
            }
            (OutputFormat::Auto, true) => compressor::rgba_image::path2compress(&tmp_src, &tmp_out),
            (OutputFormat::Auto, false) => {
                compressor::rgb_image::path2compress(&tmp_src, &tmp_out, cfg.quality)
            }
        }

        let _ = std::fs::remove_file(&tmp_src);

        // compressor は失敗しても戻り値で知らせず、出力ファイルを作らないまま
        // 返ることがある（Mokuichi147/compressor#3）。metadata の失敗を
        // 「圧縮に失敗した」と解釈する。
        let byte_size = std::fs::metadata(&tmp_out)
            .map_err(|e| {
                GenError::Io(std::io::Error::new(
                    e.kind(),
                    format!("圧縮結果が生成されませんでした ({}): {e}", tmp_out.display()),
                ))
            })?
            .len();

        // 同一ファイルシステム内の rename は atomic。これで初めて
        // 「同じ出力パスへの上書き再生成は冪等」が実際に成り立つ。
        let final_path = dest_dir.join(format!("{base_name}.{ext}"));
        if let Err(e) = std::fs::rename(&tmp_out, &final_path) {
            let _ = std::fs::remove_file(&tmp_out);
            return Err(GenError::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "圧縮結果を配置できませんでした ({}): {e}",
                    final_path.display()
                ),
            )));
        }

        Ok(GeneratedPreview {
            path: final_path,
            mime,
            width,
            height,
            byte_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let img = if alpha {
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                w,
                h,
                image::Rgba([200, 100, 50, 128]),
            ))
        } else {
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                w,
                h,
                image::Rgb([200, 100, 50]),
            ))
        };
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn resizes_and_compresses_rgb_to_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = sample_png(2000, 1000, false);
        let cfg = VariantConfig {
            max_edge_px: 400,
            quality: 75.0,
            format: OutputFormat::Auto,
        };
        let out = PreviewGenerator::generate(&bytes, dir.path(), "case1", cfg).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        assert_eq!(out.width, 400);
        assert_eq!(out.height, 200);
        assert!(out.path.extension().unwrap() == "jpg");
        assert!(out.byte_size > 0);
        assert!((out.byte_size as usize) < bytes.len());
    }

    #[test]
    fn keeps_alpha_as_png() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = sample_png(300, 200, true);
        let cfg = VariantConfig {
            max_edge_px: 1600,
            quality: 80.0,
            format: OutputFormat::Auto,
        };
        let out = PreviewGenerator::generate(&bytes, dir.path(), "case2", cfg).unwrap();
        assert_eq!(out.mime, "image/png");
        assert_eq!(out.width, 300);
        assert_eq!(out.height, 200);
    }

    /// サムネイル既定の `OutputFormat::Webp` は、アルファの有無に関わらず
    /// WebP を出す（一覧グリッドの転送量を揃えるため）。
    #[test]
    fn webp_format_used_regardless_of_alpha() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = VariantConfig {
            max_edge_px: 480,
            quality: 75.0,
            format: OutputFormat::Webp,
        };

        for (alpha, name) in [(false, "opaque"), (true, "alpha")] {
            let bytes = sample_png(1200, 600, alpha);
            let out = PreviewGenerator::generate(&bytes, dir.path(), name, cfg).unwrap();
            assert_eq!(out.mime, "image/webp", "alpha={alpha}");
            assert_eq!(out.path.extension().unwrap(), "webp", "alpha={alpha}");
            assert_eq!((out.width, out.height), (480, 240), "alpha={alpha}");
            assert!(out.byte_size > 0, "alpha={alpha}");
        }
    }

    /// thumbnail 既定は WebP、preview 既定は従来どおり JPEG/PNG。
    #[test]
    fn default_variant_configs_pick_expected_formats() {
        assert_eq!(VariantConfig::DEFAULT_THUMBNAIL.format, OutputFormat::Webp);
        assert_eq!(VariantConfig::DEFAULT_PREVIEW.format, OutputFormat::Auto);
    }

    /// 中間ファイルを配信ディレクトリに置きっぱなしにしない。残ると
    /// 起動時スイーパが回収するまで SSD を無駄に食う。
    #[test]
    fn leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = VariantConfig {
            max_edge_px: 400,
            quality: 75.0,
            format: OutputFormat::Auto,
        };
        PreviewGenerator::generate(&sample_png(800, 600, false), dir.path(), "case", cfg).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "中間ファイルが残っている: {leftovers:?}");
    }

    /// 再生成は配信中のパスを rename で差し替える。書きかけのバイト列が
    /// 見えてはいけないので、旧内容が残ることも中途半端に混ざることもない。
    #[test]
    fn regeneration_replaces_existing_output() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = VariantConfig {
            max_edge_px: 400,
            quality: 75.0,
            format: OutputFormat::Auto,
        };
        let first =
            PreviewGenerator::generate(&sample_png(800, 600, false), dir.path(), "case", cfg)
                .unwrap();
        // 同じ base_name・同じフォーマットなので出力パスは一致する。
        let second =
            PreviewGenerator::generate(&sample_png(600, 400, false), dir.path(), "case", cfg)
                .unwrap();
        assert_eq!(first.path, second.path, "同条件の再生成は同じパスへ出る");

        // rename 後のファイルは 2 回目の生成結果として完全な画像である。
        let decoded = image::load_from_memory(&std::fs::read(&second.path).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (second.width, second.height));
    }

    /// 寸法上限を超える画像はデコードさせない（decompression bomb 対策）。
    /// 上限は strict limit なので、画素を確保する前に弾かれる。リトライしても
    /// 結果は変わらないため Unsupported（恒久失敗）に落とす。
    #[test]
    fn rejects_images_beyond_decode_limits() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = VariantConfig {
            max_edge_px: 480,
            quality: 75.0,
            format: OutputFormat::Auto,
        };
        // 1px 高なら実データ量は僅かなまま、幅だけ上限を超えさせられる。
        let bytes = sample_png(MAX_DECODE_EDGE_PX + 1, 1, false);
        let err = PreviewGenerator::generate(&bytes, dir.path(), "bomb", cfg).unwrap_err();
        assert!(
            matches!(err, GenError::Unsupported(_)),
            "上限超過は恒久失敗として扱う: {err:?}"
        );

        // 上限内なら同じ形でも通る（上限そのものが厳しすぎないことの確認）。
        let ok = sample_png(MAX_DECODE_EDGE_PX, 1, false);
        assert!(PreviewGenerator::generate(&ok, dir.path(), "edge", cfg).is_ok());
    }

    #[test]
    fn rejects_undecodable_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = VariantConfig {
            max_edge_px: 400,
            quality: 75.0,
            format: OutputFormat::Auto,
        };
        let err = PreviewGenerator::generate(b"not an image", dir.path(), "case3", cfg)
            .unwrap_err();
        assert!(matches!(err, GenError::Unsupported(_)));
    }
}
