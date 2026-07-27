//! `yozist_jobs::JobHandler` としての AI タグ生成。`kind = "ai.tag"` で
//! `JobRunner` に登録する。
//!
//! プレビュー生成（`yozist-cache` の `PreviewJobHandler`）と同じ骨格だが、
//! 実処理がネットワーク待ち（1 枚あたり数十秒）なので、CPU バウンドな
//! プレビュー生成とはワーカーを分けて運用する想定。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use yozist_core::{CommitId, FileId, Tag, TagId, TagKind};
use yozist_db::{AiFileTag, SharedMetaStore};
use yozist_jobs::{JobError, JobHandler};
use yozist_versioning::VersioningEngine;

use crate::{AiError, AiProvider, TagNormalizer};

pub const AI_TAG_JOB_KIND: &str = "ai.tag";

/// LLM へ渡す画像の長辺。タグ付けに必要な情報は失われず、転送量と推論時間を
/// 抑えられる大きさにする。原本をそのまま送ると数十 MB の写真で破綻する。
const REQUEST_MAX_EDGE_PX: u32 = 1024;

/// 送信画像の JPEG 品質。
const REQUEST_JPEG_QUALITY: u8 = 80;

/// デコードを許可する 1 辺の最大 px（`yozist-cache` の生成側と同じ理由・同じ値）。
/// 寸法ヘッダだけ巨大な画像を無制限にデコードさせない。
const MAX_DECODE_EDGE_PX: u32 = 16_384;

/// デコーダが一度に確保できるバイト数の上限。
const MAX_DECODE_ALLOC_BYTES: u64 = 512 * 1024 * 1024;

/// タグ名の長さ上限。`yozist_tagging::client_tag` と揃える。
const MAX_TAG_LEN: usize = 64;

/// `ai.tag` ジョブのペイロード。`file_id`/`commit_id` は `FileId`/`CommitId` の
/// `Display`（ハイフン付き UUID 文字列）と同じ形式。
///
/// 使用モデルは持たない。設定を変えた後に古いジョブが残っていても、実行時の
/// 設定で走らせたいため（モデル名は実行時に `AiTagSettings` から読む）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiTagJobPayload {
    pub file_id: String,
    pub commit_id: String,
}

impl AiTagJobPayload {
    pub fn new(file_id: &str, commit_id: &str) -> Self {
        Self {
            file_id: file_id.to_string(),
            commit_id: commit_id.to_string(),
        }
    }

    /// 同一ジョブの多重投入を防ぐための `JobStore::enqueue` 用 dedup キー。
    pub fn dedup_key(file_id: &str, commit_id: &str) -> String {
        format!("{file_id}:{commit_id}")
    }
}

#[derive(Debug, Clone)]
pub struct AiTagSettings {
    /// 生成に使う LLM のモデル名。`ai_tag_runs.model` に記録し、付け直し判定に使う。
    pub model: String,
    /// 1 ファイルに付ける最大タグ数。
    pub max_tags: usize,
    /// これ未満の信頼度は捨てる。
    pub min_confidence: f32,
    /// 寄せ先の候補として narashi に渡す既存タグの件数上限（使用数の多い順）。
    pub vocab_limit: usize,
}

pub struct AiTagJobHandler {
    engine: Arc<VersioningEngine>,
    meta: SharedMetaStore,
    provider: Arc<dyn AiProvider>,
    normalizer: Arc<TagNormalizer>,
    settings: AiTagSettings,
}

impl AiTagJobHandler {
    pub fn new(
        engine: Arc<VersioningEngine>,
        meta: SharedMetaStore,
        provider: Arc<dyn AiProvider>,
        normalizer: Arc<TagNormalizer>,
        settings: AiTagSettings,
    ) -> Self {
        Self {
            engine,
            meta,
            provider,
            normalizer,
            settings,
        }
    }

    /// 寄せ先候補になる既存タグ名を、使用数の多い順に返す。
    ///
    /// システムタグ（`ext:` / `type:` / `src:` / `client:`）は機械的な名前空間で、
    /// 意味の似た日本語タグを吸い寄せても嬉しくないので外す。
    async fn vocabulary(&self) -> Vec<String> {
        match self.meta.list_tags_by_usage().await {
            Ok(tags) => tags
                .into_iter()
                .filter(|t| !matches!(t.kind, TagKind::System))
                .map(|t| t.name)
                .filter(|n| !n.contains(':'))
                .take(self.settings.vocab_limit)
                .collect(),
            Err(e) => {
                // 語彙が引けなくても寄せ先が無くなるだけで、生成自体は続けられる。
                tracing::warn!("既存タグ語彙の取得に失敗（寄せ先なしで続行）: {e}");
                Vec::new()
            }
        }
    }
}

#[async_trait]
impl JobHandler for AiTagJobHandler {
    async fn handle(&self, payload: &serde_json::Value) -> Result<(), JobError> {
        let payload: AiTagJobPayload = serde_json::from_value(payload.clone())
            .map_err(|e| JobError::Permanent(format!("invalid payload: {e}")))?;
        let file_uuid = uuid::Uuid::parse_str(&payload.file_id)
            .map_err(|e| JobError::Permanent(format!("invalid file_id: {e}")))?;
        let file_id = FileId::from_uuid(file_uuid);

        let file = self
            .meta
            .get_file(&file_id)
            .await
            .map_err(|e| JobError::Retryable(e.to_string()))?;
        let Some(file) = file else {
            return Err(JobError::Permanent("file not found".into()));
        };

        // 投入後に再コミットされていれば、この commit_id はもう表示対象ではない。
        // 生成せず成功扱いで終える（新しい commit 用のジョブは別途投入される）。
        let Some(current_commit) = file.current_commit else {
            return Ok(());
        };
        if current_commit.to_string() != payload.commit_id {
            return Ok(());
        }

        let Some(mime) = file.mime.as_deref() else {
            return Err(JobError::Permanent("file has no mime".into()));
        };
        if !mime.starts_with("image/") {
            return Err(JobError::Permanent(format!("unsupported mime: {mime}")));
        }

        // 上の検証で使った commit を明示して読む（検証後に再コミットが挟まると
        // 新しいバイト列を旧 commit_id の記録として残すズレが起きる）。
        let bytes = self
            .engine
            .read_at_commit(file_id, current_commit)
            .await
            .map_err(|e| JobError::Retryable(e.to_string()))?;

        let jpeg = tokio::task::spawn_blocking(move || downscale_for_request(&bytes))
            .await
            .map_err(|e| JobError::Retryable(format!("画像変換タスクが落ちました: {e}")))?
            .map_err(JobError::Permanent)?;

        let suggestions = self
            .provider
            .suggest_tags(&jpeg, "image/jpeg")
            .await
            .map_err(to_job_error)?;

        // 信頼度順に整えてから正規化に渡す。narashi 側での重複解消は「先に来た
        // 方を残す」ので、確度の高いタグが代表として残るようにする。
        let mut suggestions: Vec<_> = suggestions
            .into_iter()
            .filter(|s| s.confidence >= self.settings.min_confidence)
            .filter_map(|s| clean_tag_name(&s.name).map(|name| (name, s.confidence)))
            .collect();
        suggestions.sort_by(|a, b| b.1.total_cmp(&a.1));
        suggestions.dedup_by(|a, b| a.0 == b.0);
        suggestions.truncate(self.settings.max_tags);

        let confidence_of: std::collections::HashMap<String, f32> =
            suggestions.iter().cloned().collect();
        let candidates: Vec<String> = suggestions.into_iter().map(|(n, _)| n).collect();

        let resolved = self
            .normalizer
            .resolve(candidates, self.vocabulary().await)
            .await
            .map_err(to_job_error)?;

        let mut ai_tags = Vec::with_capacity(resolved.len());
        for r in resolved {
            let confidence = confidence_of.get(&r.raw_name).copied();
            let tag_id: TagId = self
                .meta
                .upsert_tag(&Tag {
                    id: TagId::new(),
                    name: r.name,
                    kind: TagKind::Ai,
                    confidence,
                })
                .await
                .map_err(|e| JobError::Retryable(e.to_string()))?;
            ai_tags.push(AiFileTag {
                tag_id,
                raw_name: r.raw_name,
                confidence,
            });
        }

        self.meta
            .replace_ai_file_tags(&file_id, &self.settings.model, &ai_tags)
            .await
            .map_err(|e| JobError::Retryable(e.to_string()))?;

        self.refresh_fts(&file_id, &file.display_name).await;

        self.meta
            .mark_ai_tag_ready(&file_id, &current_commit, &self.settings.model)
            .await
            .map_err(|e| JobError::Retryable(e.to_string()))?;
        Ok(())
    }

    /// 生成が二度と行われないと確定したので、`ai_tag_runs` も終端状態へ落とす。
    ///
    /// これを怠ると行は `pending` のまま残り、UI は「生成中」を表示し続ける。
    /// 一度 `failed` にした組み合わせは自動では再試行されない（明示的な再生成か
    /// `--scope missing` の一括投入で復帰する）。
    async fn on_permanent_failure(&self, payload: &serde_json::Value, error: &str) {
        let Ok(payload) = serde_json::from_value::<AiTagJobPayload>(payload.clone()) else {
            return;
        };
        let Ok(file_uuid) = uuid::Uuid::parse_str(&payload.file_id) else {
            return;
        };
        let Ok(commit_uuid) = uuid::Uuid::parse_str(&payload.commit_id) else {
            return;
        };
        if let Err(e) = self
            .meta
            .mark_ai_tag_failed(
                &FileId::from_uuid(file_uuid),
                &CommitId::from_uuid(commit_uuid),
                &self.settings.model,
                error,
            )
            .await
        {
            tracing::warn!("AI タグ生成の恒久失敗を記録できません: {e}");
        }
    }
}

impl AiTagJobHandler {
    /// タグを差し替えた後に FTS の tags 列を張り直す（`yozist-api` の
    /// `refresh_fts_tags` と同じ処理）。content は維持できないので空にする
    /// — テキストファイルは次回コミット時に再投入される。
    async fn refresh_fts(&self, file_id: &FileId, display_name: &str) {
        let tags = self
            .meta
            .list_tags_of(file_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
            .join(" ");
        if let Err(e) = self.meta.upsert_fts(file_id, display_name, &tags, "").await {
            tracing::warn!("FTS のタグ列を更新できません: {e}");
        }
    }
}

/// プロバイダのエラーをジョブの再試行方針へ写す。
fn to_job_error(e: AiError) -> JobError {
    match e {
        // 接続不能・タイムアウト・5xx・429。時間を置けば通る可能性がある。
        AiError::Provider(m) => JobError::Retryable(m),
        AiError::Permanent(m) => JobError::Permanent(m),
        AiError::NotImplemented => JobError::Permanent(AiError::NotImplemented.to_string()),
    }
}

/// LLM へ送るための縮小 JPEG を作る。CPU バウンドなので `spawn_blocking` から
/// 呼ぶこと。失敗は「この入力は扱えない」という恒久失敗として扱う。
fn downscale_for_request(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE_PX);
    limits.max_image_height = Some(MAX_DECODE_EDGE_PX);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);

    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("画像形式を判別できません: {e}"))?;
    reader.limits(limits);
    let img = reader
        .decode()
        .map_err(|e| format!("画像をデコードできません: {e}"))?;

    // 元が小さければ拡大はしない（情報は増えず、送信量だけ増える）。
    let img = if img.width().max(img.height()) > REQUEST_MAX_EDGE_PX {
        img.resize(
            REQUEST_MAX_EDGE_PX,
            REQUEST_MAX_EDGE_PX,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };

    // JPEG はアルファを持てないので RGB へ落とす。
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(
        &mut std::io::Cursor::new(&mut out),
        REQUEST_JPEG_QUALITY,
    )
    .encode_image(&image::DynamicImage::ImageRgb8(rgb))
    .map_err(|e| format!("JPEG に変換できません: {e}"))?;
    Ok(out)
}

/// LLM が返したタグ名を、タグとして登録できる形に整える。
///
/// カンマを落とすのは `GET /api/files/by-tags?tags=a,b` がカンマ区切りで名前を
/// 解決するため（`yozist_tagging::client_tag` と同じ理由）。空になる場合は
/// タグを作らない。
fn clean_tag_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|c| *c != ',' && !c.is_control())
        .take(MAX_TAG_LEN)
        .collect::<String>()
        .trim()
        .to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_tag_name_trims_and_drops_commas() {
        assert_eq!(clean_tag_name("  湖, 山 ").as_deref(), Some("湖 山"));
        assert_eq!(clean_tag_name("   ").as_deref(), None);
        assert_eq!(clean_tag_name(",,").as_deref(), None);
    }

    #[test]
    fn clean_tag_name_limits_length() {
        let long = "あ".repeat(100);
        assert_eq!(clean_tag_name(&long).unwrap().chars().count(), MAX_TAG_LEN);
    }

    /// 一時的な失敗はリトライへ、恒久的な失敗は即終端へ。取り違えると、
    /// LLM が一時的に落ちただけで二度と生成されなくなる（またはその逆）。
    #[test]
    fn provider_errors_map_to_retry_policy() {
        assert!(matches!(
            to_job_error(AiError::Provider("timeout".into())),
            JobError::Retryable(_)
        ));
        assert!(matches!(
            to_job_error(AiError::Permanent("bad request".into())),
            JobError::Permanent(_)
        ));
        assert!(matches!(
            to_job_error(AiError::NotImplemented),
            JobError::Permanent(_)
        ));
    }

    /// 小さい画像を拡大しない（送信量だけ増えて情報は増えない）。
    #[test]
    fn downscale_keeps_small_images_small() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(64, 32));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let jpeg = downscale_for_request(&png).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 32));
    }

    /// 長辺が上限を超える画像は縮める。
    #[test]
    fn downscale_shrinks_large_images() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(4000, 2000));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let jpeg = downscale_for_request(&png).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!(decoded.width(), REQUEST_MAX_EDGE_PX);
        assert_eq!(decoded.height(), REQUEST_MAX_EDGE_PX / 2);
    }

    #[test]
    fn downscale_rejects_non_image_bytes() {
        assert!(downscale_for_request(b"not an image at all").is_err());
    }
}
