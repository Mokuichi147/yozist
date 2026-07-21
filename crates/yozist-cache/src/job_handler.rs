//! `yozist_jobs::JobHandler` としてのプレビュー生成。`kind = "preview.generate"`
//! で `JobRunner` に登録する。オリジナルの取得は `VersioningEngine::read_current`
//! （既存 `get_content` エンドポイントと同じ経路）を使う。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use yozist_core::FileId;
use yozist_jobs::{JobError, JobHandler};
use yozist_versioning::VersioningEngine;

use crate::{CacheStore, GenError, PreviewGenerator, Variant, VariantConfigs};

/// `preview.generate` ジョブのペイロード。`file_id`/`commit_id` は
/// `FileId`/`CommitId` の `Display`（ハイフン付き UUID 文字列）と同じ形式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewJobPayload {
    pub file_id: String,
    pub commit_id: String,
    pub variant: String,
}

impl PreviewJobPayload {
    pub fn new(file_id: &str, commit_id: &str, variant: Variant) -> Self {
        Self {
            file_id: file_id.to_string(),
            commit_id: commit_id.to_string(),
            variant: variant.as_str().to_string(),
        }
    }

    /// 同一ジョブの多重投入を防ぐための `JobStore::enqueue` 用 dedup キー。
    pub fn dedup_key(file_id: &str, commit_id: &str, variant: Variant) -> String {
        format!("{file_id}:{commit_id}:{}", variant.as_str())
    }
}

pub struct PreviewJobHandler {
    engine: Arc<VersioningEngine>,
    cache_store: Arc<CacheStore>,
    cache_dir: PathBuf,
    configs: VariantConfigs,
}

impl PreviewJobHandler {
    pub fn new(
        engine: Arc<VersioningEngine>,
        cache_store: Arc<CacheStore>,
        cache_dir: PathBuf,
        configs: VariantConfigs,
    ) -> Self {
        Self {
            engine,
            cache_store,
            cache_dir,
            configs,
        }
    }
}

#[async_trait]
impl JobHandler for PreviewJobHandler {
    async fn handle(&self, payload: &serde_json::Value) -> Result<(), JobError> {
        let payload: PreviewJobPayload = serde_json::from_value(payload.clone())
            .map_err(|e| JobError::Permanent(format!("invalid payload: {e}")))?;

        let Some(variant) = Variant::parse(&payload.variant) else {
            return Err(JobError::Permanent(format!(
                "unknown variant: {}",
                payload.variant
            )));
        };
        let file_uuid = uuid::Uuid::parse_str(&payload.file_id)
            .map_err(|e| JobError::Permanent(format!("invalid file_id: {e}")))?;
        let file_id = FileId::from_uuid(file_uuid);

        let file = self
            .engine
            .meta
            .get_file(&file_id)
            .await
            .map_err(|e| JobError::Retryable(e.to_string()))?;
        let Some(file) = file else {
            return Err(JobError::Permanent("file not found".into()));
        };

        // enqueue 後に再コミットされていれば、この commit_id はもう表示対象では
        // ない。生成せずに成功扱いで終える（新しい commit 用のジョブは、次に
        // その commit のプレビューが要求された時点で別途投入される）。
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

        let bytes = self
            .engine
            .read_current(file_id)
            .await
            .map_err(|e| JobError::Retryable(e.to_string()))?;

        let cfg = self.configs.for_variant(variant);
        let file_hex = file_uuid.simple().to_string();
        let commit_hex = current_commit.as_uuid().simple().to_string();
        let shard = file_hex[0..2].to_string();
        let dest_dir = self.cache_dir.join(&shard);
        let base_name = format!("{file_hex}-{commit_hex}-{}", variant.as_str());
        let cache_dir = self.cache_dir.clone();

        let generated = tokio::task::spawn_blocking(move || {
            PreviewGenerator::generate(&bytes, &dest_dir, &base_name, cfg)
        })
        .await
        .map_err(|e| JobError::Retryable(format!("generation task panicked: {e}")))?;

        match generated {
            Ok(g) => {
                let rel_path = g
                    .path
                    .strip_prefix(&cache_dir)
                    .unwrap_or(&g.path)
                    .to_string_lossy()
                    .to_string();
                self.cache_store
                    .mark_ready(
                        &payload.file_id,
                        &payload.commit_id,
                        variant,
                        &rel_path,
                        g.mime,
                        g.byte_size as i64,
                        g.width as i32,
                        g.height as i32,
                    )
                    .await
                    .map_err(|e| JobError::Retryable(e.to_string()))?;
                Ok(())
            }
            Err(GenError::Unsupported(msg)) => {
                self.cache_store
                    .mark_failed(&payload.file_id, &payload.commit_id, variant, &msg)
                    .await
                    .map_err(|e| JobError::Retryable(e.to_string()))?;
                Err(JobError::Permanent(msg))
            }
            Err(GenError::Io(e)) => Err(JobError::Retryable(e.to_string())),
        }
    }
}
