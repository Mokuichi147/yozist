//! yozist-cache — サムネイル/プレビュー軽量化キャッシュ。
//!
//! # 設計原則
//! - **`yozist-jobs` の一利用者**: バックグラウンド実行は `yozist-jobs::JobRunner`
//!   に `kind = "preview.generate"` のハンドラ（[`PreviewJobHandler`]）として
//!   登録する。キュー・ワーカー基盤自体はこのクレートでは持たない。
//! - **成果物の所在はこのクレートの責務**: どのファイル・コミット・variant が
//!   どこに生成済みかは `preview_cache` テーブル（[`CacheStore`]）で管理する。
//! - **compressor は無改造で使う**: 解像度リサイズは `image` crate で行い、
//!   最終圧縮のみ compressor の既存 `pub` API（`path2compress` 系）に委ねる。

mod generator;
mod job_handler;
mod sqlite;

pub use generator::{GenError, GeneratedPreview, PreviewGenerator};

pub use job_handler::{PreviewJobHandler, PreviewJobPayload};
pub use sqlite::{CacheEntry, CacheStore, Lookup};

/// プレビュー画像の用途別バリエーション。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// ギャラリー等の小さいグリッド表示用。
    Thumbnail,
    /// ファイル詳細ページの大きい表示用。
    Preview,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Thumbnail => "thumbnail",
            Variant::Preview => "preview",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "thumbnail" => Some(Variant::Thumbnail),
            "preview" => Some(Variant::Preview),
            _ => None,
        }
    }
}

/// 出力フォーマットの選び方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// アルファの有無で PNG（可逆）／JPEG（非可逆）を選ぶ。
    /// 元画像に近い形式のまま残るので、詳細ページ向け。
    Auto,
    /// 常に WebP。アルファがあれば可逆、無ければ非可逆で出力する。
    ///
    /// 一覧グリッド向け。`Auto` だとアルファチャンネルを持つだけの
    /// （実際は不透明な）スクリーンショット PNG が可逆 PNG のまま残り、
    /// サムネイルが期待ほど小さくならない。WebP なら同じ可逆でも PNG より
    /// 小さく、不透明画像は非可逆でさらに小さくできる。
    ///
    /// NOTE: アルファ付き非可逆 WebP を使えれば前者もさらに縮められるが、
    /// compressor が API を公開していないため現状は可逆にフォールバック
    /// している（Mokuichi147/compressor#3）。
    Webp,
}

/// variant ごとの生成パラメータ。
#[derive(Debug, Clone, Copy)]
pub struct VariantConfig {
    /// 長辺の上限（px）。これを超える場合のみリサイズする。
    pub max_edge_px: u32,
    /// 非可逆出力時の品質（0-100）。可逆出力（PNG / 可逆 WebP）では未使用。
    pub quality: f32,
    /// 出力フォーマットの選び方。
    pub format: OutputFormat,
}

impl VariantConfig {
    pub const DEFAULT_THUMBNAIL: VariantConfig = VariantConfig {
        max_edge_px: 480,
        quality: 75.0,
        format: OutputFormat::Webp,
    };
    pub const DEFAULT_PREVIEW: VariantConfig = VariantConfig {
        max_edge_px: 1600,
        quality: 82.0,
        format: OutputFormat::Auto,
    };
}

/// variant ごとの生成パラメータをまとめたもの。CLI から上書きされた値を保持し、
/// `PreviewJobHandler` へ渡す。
#[derive(Debug, Clone, Copy)]
pub struct VariantConfigs {
    pub thumbnail: VariantConfig,
    pub preview: VariantConfig,
}

impl Default for VariantConfigs {
    fn default() -> Self {
        Self {
            thumbnail: VariantConfig::DEFAULT_THUMBNAIL,
            preview: VariantConfig::DEFAULT_PREVIEW,
        }
    }
}

impl VariantConfigs {
    pub fn for_variant(&self, variant: Variant) -> VariantConfig {
        match variant {
            Variant::Thumbnail => self.thumbnail,
            Variant::Preview => self.preview,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
}
