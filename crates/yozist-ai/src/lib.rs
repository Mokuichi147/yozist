//! yozist-ai — AI 解析プロバイダ（タグ推測、要約、検索）。
//!
//! # 設計原則
//! - AI が書き込む場合も必ず `yozist-versioning` / `yozist-tagging` の公開 API 経由。
//!   独自パスは作らない。
//! - プロバイダはプラガブル（ローカル llama / OpenAI / 独自エンドポイント）。
//!
//! # TODO
//! - [ ] `llama-cpp-rs` 連携
//! - [ ] 外部 API クライアント（OpenAI / Anthropic 互換）
//! - [ ] 信頼スコアの閾値設定
//! - [ ] 非同期ジョブキュー（バックグラウンドでタグ付け）

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct TagSuggestion {
    pub name: String,
    pub confidence: f32,
}

#[async_trait]
pub trait AiProvider: Send + Sync {
    /// ファイル内容からタグ候補を提案する。
    async fn suggest_tags(&self, content: &[u8]) -> Result<Vec<TagSuggestion>, AiError>;
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
    ) -> Result<Vec<TagSuggestion>, AiError> {
        Ok(vec![])
    }
    async fn summarize(&self, _content: &[u8]) -> Result<String, AiError> {
        Ok(String::new())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("not implemented")]
    NotImplemented,
}
