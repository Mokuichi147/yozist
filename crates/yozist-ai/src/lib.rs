//! yozist-ai — AI 解析プロバイダ（タグ推測、要約、検索）。
//!
//! # 設計原則
//! - AI が書き込む場合も必ず `yozist-versioning` / `yozist-tagging` の公開 API 経由。
//!   独自パスは作らない。
//! - プロバイダはプラガブル（ローカル llama / OpenAI / 独自エンドポイント）。
//! - 生成結果（タグ）は**メタ DB に永続化する**。1 枚あたり LLM 推論 20 秒級の
//!   コストがかかるユーザーデータであり、CPU で作り直せるプレビューキャッシュと
//!   同じ扱い（`<cache_dir>` へ置いて随時破棄）にはできない。
//!
//! # TODO
//! - [ ] `llama-cpp-rs` 連携（ローカル推論）
//! - [ ] 画像以外（テキスト・PDF）のタグ推測
//! - [ ] 要約（`summarize`）の実装と保存先

use async_trait::async_trait;

mod job;
mod normalize;
mod service;
mod vision;

pub use job::{AiTagJobHandler, AiTagJobPayload, AiTagSettings, AI_TAG_JOB_KIND};
pub use normalize::{ResolvedTag, TagNormalizer};
pub use service::{AiTagEnqueueError, AiTagService, EnqueueSummary};
pub use vision::OpenAiVisionProvider;

/// 表記ゆれ統合しきい値の既定値。narashi 側の推奨値をそのまま使う
/// （バックエンドによって適切な値が違うため、定数を再宣言せず追随する）。
pub const DEFAULT_SIMILARITY_THRESHOLD: f32 = narashi::DEFAULT_THRESHOLD;

#[derive(Debug, Clone)]
pub struct TagSuggestion {
    pub name: String,
    pub confidence: f32,
}

#[async_trait]
pub trait AiProvider: Send + Sync {
    /// ファイル内容からタグ候補を提案する。`mime` は `content` の形式
    /// （プロバイダが vision 入力を組み立てるのに使う）。
    async fn suggest_tags(
        &self,
        content: &[u8],
        mime: &str,
    ) -> Result<Vec<TagSuggestion>, AiError>;
    /// ファイル内容を要約する。
    async fn summarize(&self, content: &[u8]) -> Result<String, AiError>;
}

/// 何もしないスタブ実装。
pub struct NoopAiProvider;

#[async_trait]
impl AiProvider for NoopAiProvider {
    async fn suggest_tags(
        &self,
        _content: &[u8],
        _mime: &str,
    ) -> Result<Vec<TagSuggestion>, AiError> {
        Ok(vec![])
    }
    async fn summarize(&self, _content: &[u8]) -> Result<String, AiError> {
        Ok(String::new())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// 再試行しても結果が変わらない失敗（入力が非対応・リクエストが不正・
    /// 応答が期待した形式でない）。
    #[error("permanent error: {0}")]
    Permanent(String),
    /// 一時的な失敗（接続不能・タイムアウト・5xx・レート制限）。
    #[error("provider error: {0}")]
    Provider(String),
    #[error("not implemented")]
    NotImplemented,
}
