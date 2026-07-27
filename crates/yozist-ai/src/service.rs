//! AI タグ生成ジョブの投入口。
//!
//! REST ハンドラ・CLI の一括コマンド・アップロード時の自動投入がすべてここを
//! 通る（投入と `ai_tag_runs` の `pending` 記録を対で行う一箇所に保つため）。

use std::sync::Arc;
use yozist_core::{CommitId, FileId};
use yozist_db::{AiTagScope, DbError, SharedMetaStore};
use yozist_jobs::JobStore;

use crate::job::{AiTagJobPayload, AI_TAG_JOB_KIND};

/// 一括投入の結果。`already_queued` は未完了ジョブが既にあって dedup で弾かれた
/// 件数（報告が実態とずれないよう分けて数える）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnqueueSummary {
    pub targets: usize,
    pub enqueued: usize,
    pub already_queued: usize,
}

pub struct AiTagService {
    job_store: Arc<JobStore>,
    meta: SharedMetaStore,
    /// 現在の設定で使う LLM モデル名。`ai_tag_runs.model` がこれと違うファイルが
    /// 付け直しの対象（`AiTagScope::Stale`）。
    model: String,
}

impl AiTagService {
    pub fn new(job_store: Arc<JobStore>, meta: SharedMetaStore, model: String) -> Self {
        Self {
            job_store,
            meta,
            model,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn meta(&self) -> &SharedMetaStore {
        &self.meta
    }

    /// 1 ファイル分を投入する。戻り値は「実際にジョブ行が入ったか」
    /// （既に同じ未完了ジョブが積まれていれば `false`）。
    ///
    /// 投入できてもできなくても `ai_tag_runs` は `pending` にする: 既存ジョブが
    /// 拾うので結果は出るし、UI に「生成中」を出す根拠が必要。
    pub async fn enqueue(
        &self,
        file_id: &FileId,
        commit_id: &CommitId,
    ) -> Result<bool, AiTagEnqueueError> {
        let file_s = file_id.to_string();
        let commit_s = commit_id.to_string();
        let dedup = AiTagJobPayload::dedup_key(&file_s, &commit_s);
        let payload = AiTagJobPayload::new(&file_s, &commit_s);
        let inserted = self
            .job_store
            .enqueue(AI_TAG_JOB_KIND, Some(&dedup), &payload)
            .await
            .map_err(|e| AiTagEnqueueError::Queue(e.to_string()))?;
        self.meta
            .mark_ai_tag_pending(file_id, commit_id, &self.model)
            .await?;
        Ok(inserted)
    }

    /// 対象範囲のファイルをまとめて投入する。
    pub async fn enqueue_scope(
        &self,
        scope: AiTagScope,
    ) -> Result<EnqueueSummary, AiTagEnqueueError> {
        let targets = self.meta.list_ai_tag_targets(scope, &self.model).await?;
        let mut summary = EnqueueSummary {
            targets: targets.len(),
            ..Default::default()
        };
        for (file_id, commit_id) in targets {
            if self.enqueue(&file_id, &commit_id).await? {
                summary.enqueued += 1;
            } else {
                summary.already_queued += 1;
            }
        }
        Ok(summary)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AiTagEnqueueError {
    #[error("ジョブキューへの投入に失敗しました: {0}")]
    Queue(String),
    #[error(transparent)]
    Db(#[from] DbError),
}
