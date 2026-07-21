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
pub use store::{JobRecord, JobStatus, JobStore, FINISHED_RETENTION, STALLED_LEASE};

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

    /// このジョブがもう二度と実行されないと確定した時に呼ばれる
    /// （`JobError::Permanent`、またはリトライ上限に達した `Retryable`）。
    ///
    /// ドメイン側が「生成中」を表す中間状態を持っている場合、それを終端状態へ
    /// 落とすために使う。実装しないと、リトライ枯渇後もドメイン側は「生成待ち」
    /// のままになり、要求のたびに新しいジョブを投入し続ける（dedup は未完了
    /// ジョブにしか効かないため、恒久失敗した行は重複投入を止められない）。
    async fn on_permanent_failure(&self, _payload: &serde_json::Value, _error: &str) {}
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

/// 終端状態のジョブ行を掃除する間隔（ワーカー 0 番のみが実行する）。
/// 保持期間が日単位なので、頻繁に回しても消える行は無い。
const PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);

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

    async fn purge_finished(&self) {
        match self.store.purge_finished(store::FINISHED_RETENTION).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("保持期間を過ぎた完了ジョブを {n} 件削除"),
            Err(e) => tracing::warn!("完了ジョブの削除に失敗: {e}"),
        }
    }

    async fn worker_loop(&self, worker_id: usize) {
        let kinds: Vec<&str> = self.handlers.keys().map(|s| s.as_str()).collect();
        let mut last_reclaim = tokio::time::Instant::now();
        let mut last_purge = tokio::time::Instant::now();
        loop {
            // ワーカー 0 番だけが定期メンテナンスを担う（全員でやっても同じ行を
            // 奪い合うだけで意味がない）。
            if worker_id == 0 && last_reclaim.elapsed() >= RECLAIM_INTERVAL {
                self.reclaim_stalled().await;
                last_reclaim = tokio::time::Instant::now();
            }
            if worker_id == 0 && last_purge.elapsed() >= PURGE_INTERVAL {
                self.purge_finished().await;
                last_purge = tokio::time::Instant::now();
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
        let spawned = handler.clone();
        let payload = job.payload.clone();
        let outcome = tokio::spawn(async move { spawned.handle(&payload).await }).await;

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
                handler.on_permanent_failure(&job.payload, &msg).await;
            }
            Err(JobError::Retryable(msg)) => {
                match self.store.mark_failed_retry(&job.id, &msg).await {
                    // リトライ上限に達して恒久失敗が確定した。ドメイン側にも
                    // 終端状態を伝えないと「生成待ち」のまま再投入され続ける。
                    Ok(true) => handler.on_permanent_failure(&job.payload, &msg).await,
                    Ok(false) => {}
                    Err(e) => tracing::warn!("failed to mark job {} for retry: {e}", job.id),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// 常に指定のエラーを返し、恒久失敗の通知を記録するだけのハンドラ。
    struct FailingHandler {
        error: JobError,
        permanent_failures: Mutex<Vec<String>>,
    }

    impl FailingHandler {
        fn new(error: JobError) -> Arc<Self> {
            Arc::new(Self {
                error,
                permanent_failures: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl JobHandler for FailingHandler {
        async fn handle(&self, _payload: &serde_json::Value) -> Result<(), JobError> {
            Err(match &self.error {
                JobError::Permanent(m) => JobError::Permanent(m.clone()),
                JobError::Retryable(m) => JobError::Retryable(m.clone()),
            })
        }

        async fn on_permanent_failure(&self, payload: &serde_json::Value, error: &str) {
            self.permanent_failures
                .lock()
                .unwrap()
                .push(format!(
                    "{}:{error}",
                    payload["file_id"].as_str().unwrap_or_default()
                ));
        }
    }

    async fn drain_with(handler: Arc<FailingHandler>, store: Arc<JobStore>) {
        let mut runner = JobRunner::new(store);
        runner.register("t", handler);
        Arc::new(runner).drain().await;
    }

    #[tokio::test]
    async fn permanent_failure_notifies_handler() {
        let store = Arc::new(JobStore::open_in_memory().await.unwrap());
        store
            .enqueue("t", None, &json!({"file_id": "f1"}))
            .await
            .unwrap();

        let handler = FailingHandler::new(JobError::Permanent("unsupported".into()));
        drain_with(handler.clone(), store).await;

        assert_eq!(
            *handler.permanent_failures.lock().unwrap(),
            vec!["f1:unsupported".to_string()]
        );
    }

    /// リトライ枯渇も恒久失敗であり、ドメイン側へ通知しなければならない。
    /// 通知が無いとドメイン側は「生成待ち」のまま残り、要求のたびに新しい
    /// ジョブを投入し続ける（dedup は未完了ジョブにしか効かない）。
    #[tokio::test]
    async fn retry_exhaustion_notifies_handler() {
        let store = Arc::new(JobStore::open_in_memory().await.unwrap());
        store
            .enqueue("t", Some("f1:thumb"), &json!({"file_id": "f1"}))
            .await
            .unwrap();
        // 次の claim で上限に達するよう試行回数を引き上げておく（バックオフの
        // 待ち時間を挟まずに枯渇まで進めるため）。
        store.bump_attempts_to_max("f1:thumb").await;

        let handler = FailingHandler::new(JobError::Retryable("disk full".into()));
        drain_with(handler.clone(), store.clone()).await;

        assert_eq!(
            *handler.permanent_failures.lock().unwrap(),
            vec!["f1:disk full".to_string()],
            "リトライ上限に達した Retryable も恒久失敗として通知される"
        );
        assert_eq!(store.count_incomplete(&["t"]).await.unwrap(), 0);
    }

    /// リトライ余地が残っているうちは恒久失敗を通知しない。
    #[tokio::test]
    async fn retryable_failure_does_not_notify_while_attempts_remain() {
        let store = Arc::new(JobStore::open_in_memory().await.unwrap());
        store
            .enqueue("t", None, &json!({"file_id": "f1"}))
            .await
            .unwrap();

        let handler = FailingHandler::new(JobError::Retryable("transient".into()));
        let mut runner = JobRunner::new(store.clone());
        runner.register("t", handler.clone());
        let runner = Arc::new(runner);
        // drain はバックオフ待ちを待ち続けるので、1 件だけ処理して打ち切る。
        let job = store.claim_next(&["t"]).await.unwrap().unwrap();
        runner.run_one(job).await;

        assert!(handler.permanent_failures.lock().unwrap().is_empty());
        assert_eq!(store.count_incomplete(&["t"]).await.unwrap(), 1);
    }
}
