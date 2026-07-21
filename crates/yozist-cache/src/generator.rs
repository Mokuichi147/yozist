//! リサイズ + 圧縮による軽量プレビュー生成。
//!
//! compressor 自体は解像度リサイズ機能を持たないため、リサイズは `image` crate で
//! 行い、最終圧縮は compressor の既存 `pub` API（`rgb_image`/`rgba_image` の
//! `path2compress`）に委ねる。compressor は bytes を直接返す API を持たないため、
//! リサイズ結果を一旦 PNG として一時ファイルに書き出し、それを compressor に渡す。

use image::imageops::FilterType;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::VariantConfig;

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

        let img = image::load_from_memory(bytes)
            .map_err(|e| GenError::Unsupported(e.to_string()))?;
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

        // compressor の path2compress 系は入力をファイルパスで受け取るため、
        // リサイズ結果を一旦 PNG（可逆）で一時ファイルに書き出す。
        let tmp_path = dest_dir.join(format!(".tmp-{}.png", Uuid::new_v4().simple()));
        let write_result = resized.save_with_format(&tmp_path, image::ImageFormat::Png);
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(GenError::Unsupported(format!("failed to encode intermediate png: {e}")));
        }

        let (final_path, mime) = if has_alpha {
            let out = dest_dir.join(format!("{base_name}.png"));
            compressor::rgba_image::path2compress(&tmp_path, &out);
            (out, "image/png")
        } else {
            let out = dest_dir.join(format!("{base_name}.jpg"));
            compressor::rgb_image::path2compress(&tmp_path, &out, cfg.quality);
            (out, "image/jpeg")
        };

        let _ = std::fs::remove_file(&tmp_path);

        let byte_size = std::fs::metadata(&final_path)?.len();
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
        };
        let out = PreviewGenerator::generate(&bytes, dir.path(), "case2", cfg).unwrap();
        assert_eq!(out.mime, "image/png");
        assert_eq!(out.width, 300);
        assert_eq!(out.height, 200);
    }

    #[test]
    fn rejects_undecodable_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = VariantConfig {
            max_edge_px: 400,
            quality: 75.0,
        };
        let err = PreviewGenerator::generate(b"not an image", dir.path(), "case3", cfg)
            .unwrap_err();
        assert!(matches!(err, GenError::Unsupported(_)));
    }
}
