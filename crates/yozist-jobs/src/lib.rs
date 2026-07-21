//! yozist-jobs — 汎用バックグラウンドジョブキュー。
//!
//! # 設計原則
//! - **ドメイン非依存**: このクレートは「ジョブが実行されたか」だけを管理する。
//!   成果物（生成したプレビュー画像、推測したタグなど）の保存場所や意味は
//!   `JobHandler` を実装するドメイン側クレート（`yozist-cache` など）の責務。
//! - **プラガブル**: `kind` 文字列ごとに `JobHandler` を登録するだけで新しい
//!   バックグラウンド処理を追加できる。プレビュー生成専用に作らず、将来の
//!   AI 自動タグ付け（`yozist-ai` の TODO）もこの基盤に乗せる想定。
//! - **WAL モード必須**: `yozist-db` と同じ理由で SQLite は WAL を使う。

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

mod store;
pub use store::{JobRecord, JobStatus, JobStore, STALLED_LEASE};

/// ジョブハンドラの実行結果エラー。
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// リトライしても無駄な失敗（例: 非対応フォーマット）。即 failed 確定。
    #[error("permanent failure: {0}")]
    Permanent(String),
    /// 一時的な失敗（例: I/O エラー、外部 API のタイムアウト）。バックオフ後リトライ。
    #[error("retryable failure: {0}")]
    Retryable(String),
}

/// ジョブ種別ごとの実処理。`JobRunner::register` で `kind` に紐付けて登録する。
#[async_trait]
pub trait JobHandler: Send + Sync {
    async fn handle(&self, payload: &serde_json::Value) -> Result<(), JobError>;
}

#[derive(Debug, thiserror::Error)]
pub enum JobsError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
}

/// ジョブが見つからなかった時の再ポーリング間隔。
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 固着した `running` ジョブを回収する間隔（ワーカー 0 番のみが実行する）。
const RECLAIM_INTERVAL: Duration = Duration::from_secs(60);

/// `kind → JobHandler` の登録表とワーカー起動を担う。
pub struct JobRunner {
    store: Arc<JobStore>,
    handlers: HashMap<String, Arc<dyn JobHandler>>,
}

impl JobRunner {
    pub fn new(store: Arc<JobStore>) -> Self {
        Self {
            store,
            handlers: HashMap::new(),
        }
    }

    /// ジョブ種別 `kind` のハンドラを登録する。
    pub fn register(&mut self, kind: impl Into<String>, handler: Arc<dyn JobHandler>) {
        self.handlers.insert(kind.into(), handler);
    }

    pub fn store(&self) -> &Arc<JobStore> {
        &self.store
    }

    /// 登録済み全 kind をポーリングするワーカーを `n` 本、バックグラウンドに起動する。
    ///
    /// 起動時に一度 [`JobStore::reclaim_stalled`] を回し、前回プロセスが
    /// ジョブ実行中に落ちて `running` のまま残った行を回収する。
    pub fn spawn_workers(self: &Arc<Self>, n: usize) {
        for worker_id in 0..n {
            let runner = self.clone();
            tokio::spawn(async move {
                if worker_id == 0 {
                    runner.reclaim_stalled().await;
                }
                runner.worker_loop(worker_id).await;
            });
        }
    }

    /// キューが空になるまで自ワーカーだけで処理し、その後戻る。CLI の一括投入
    /// コマンド（cache-warm 等）が「終わったらプロセスを終了する」ために使う。
    ///
    /// バックオフ待ちのジョブがあると `claim_next` は一時的に `None` を返すため、
    /// 「未完了が 0 件」になるまで待つ。戻り値は処理し切れずに残った件数
    /// （0 なら完全に捌けた）。
    pub async fn drain(self: &Arc<Self>) -> i64 {
        let kinds: Vec<&str> = self.handlers.keys().map(|s| s.as_str()).collect();
        self.reclaim_stalled().await;
        loop {
            match self.store.claim_next(&kinds).await {
                Ok(Some(job)) => self.run_one(job).await,
                Ok(None) => {
                    // キューは空に見えるが、リトライのバックオフ待ちが残って
                    // いる可能性がある。未完了が無くなって初めて完了とみなす。
                    match self.store.count_incomplete(&kinds).await {
                        Ok(0) => return 0,
                        Ok(remaining) => {
                            tracing::debug!(remaining, "バックオフ待ちのジョブを待機中");
                            tokio::time::sleep(POLL_INTERVAL).await;
                        }
                        Err(e) => {
                            tracing::warn!("未完了ジョブ数の取得に失敗: {e}");
                            return -1;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("job queue poll failed: {e}");
                    return self.store.count_incomplete(&kinds).await.unwrap_or(-1);
                }
            }
        }
    }

    async fn reclaim_stalled(&self) {
        match self.store.reclaim_stalled(store::STALLED_LEASE).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("固着していた running ジョブを {n} 件回収"),
            Err(e) => tracing::warn!("固着ジョブの回収に失敗: {e}"),
        }
    }

    async fn worker_loop(&self, worker_id: usize) {
        let kinds: Vec<&str> = self.handlers.keys().map(|s| s.as_str()).collect();
        let mut last_reclaim = tokio::time::Instant::now();
        loop {
            // ワーカー 0 番だけが定期回収を担う（全員でやっても同じ行を奪い合う
            // だけで意味がない）。
            if worker_id == 0 && last_reclaim.elapsed() >= RECLAIM_INTERVAL {
                self.reclaim_stalled().await;
                last_reclaim = tokio::time::Instant::now();
            }
            match self.store.claim_next(&kinds).await {
                Ok(Some(job)) => self.run_one(job).await,
                Ok(None) => tokio::time::sleep(POLL_INTERVAL).await,
                Err(e) => {
                    tracing::warn!(worker_id, "job queue poll failed: {e}");
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    async fn run_one(&self, job: JobRecord) {
        let Some(handler) = self.handlers.get(job.kind.as_str()) else {
            tracing::warn!("no handler registered for job kind {}", job.kind);
            let _ = self
                .store
                .mark_failed_permanent(&job.id, "no handler registered")
                .await;
            return;
        };

        // ハンドラは別タスクで実行する。ワーカーループ内で直接 await すると
        // ハンドラの panic がワーカータスクごと巻き込み、ワーカーが静かに全滅
        // してジョブ処理が止まる。別タスクなら panic は JoinError として
        // 受け取れ、通常のリトライ経路に流せる。
        let handler = handler.clone();
        let payload = job.payload.clone();
        let outcome = tokio::spawn(async move { handler.handle(&payload).await }).await;

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => Err(JobError::Retryable(format!("handler panicked: {e}"))),
        };

        match outcome {
            Ok(()) => {
                if let Err(e) = self.store.mark_done(&job.id).await {
                    tracing::warn!("failed to mark job {} done: {e}", job.id);
                }
            }
            Err(JobError::Permanent(msg)) => {
                if let Err(e) = self.store.mark_failed_permanent(&job.id, &msg).await {
                    tracing::warn!("failed to mark job {} failed: {e}", job.id);
                }
            }
            Err(JobError::Retryable(msg)) => {
                if let Err(e) = self.store.mark_failed_retry(&job.id, &msg).await {
                    tracing::warn!("failed to mark job {} for retry: {e}", job.id);
                }
            }
        }
    }
}
