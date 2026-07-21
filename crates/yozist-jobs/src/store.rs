//! ジョブの永続化。SQLite に `jobs` テーブルとして保存する。
//!
//! - UUID・時刻の保存形式は `yozist-db` と同じ（UUID は hex テキスト、時刻は RFC3339）。
//! - WAL モード必須（`yozist-db` と同じ理由: 並行アクセス対応）。

use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::JobsError;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// ジョブの実行状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Pending,
    Running,
    Done,
    Failed,
}

/// `claim_next` が返す 1 件分のジョブ情報。
#[derive(Debug, Clone)]
pub struct JobRecord {
    pub id: String,
    pub kind: String,
    pub dedup_key: Option<String>,
    pub payload: serde_json::Value,
    pub status: JobStatus,
    pub attempts: i64,
    pub max_attempts: i64,
    pub error: Option<String>,
}

/// 恒久失敗までのリトライ間隔（秒）。試行回数に応じて段階的に伸ばす。
const RETRY_BACKOFF_SECS: [i64; 3] = [10, 60, 300];
const DEFAULT_MAX_ATTEMPTS: i64 = 3;

/// `running` のまま放置されたジョブを回収するまでの猶予。
///
/// `claim_next` は `pending` しか拾わず、dedup の部分インデックスは
/// `pending`/`running` を対象にするため、`running` のまま残った行は
/// 「再取得もされず、同じ dedup_key の再投入も弾く」状態で固着する。
/// プロセスがジョブ実行中に落ちると必ずこうなるので、一定時間を過ぎた
/// `running` は落ちた実行の残骸とみなして `pending` に戻す。
///
/// プレビュー生成は通常数秒で終わるため 10 分あれば十分な余裕がある。
/// 万一生きているジョブを再取得しても、生成は同じ出力パスへの上書きで
/// 冪等なので実害は無い（無駄な再実行のみ）。
pub const STALLED_LEASE: Duration = Duration::from_secs(10 * 60);

pub struct JobStore {
    pool: SqlitePool,
}

fn fmt_dt(dt: OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .expect("OffsetDateTime を RFC3339 にフォーマット")
}

impl JobStore {
    /// ファイルパスから接続し、マイグレーション実行 + WAL 有効化。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, JobsError> {
        let url = format!("sqlite://{}?mode=rwc", path.as_ref().display());
        let opts = SqliteConnectOptions::from_str(&url)?
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;

        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    /// メモリ DB（テスト用）。
    pub async fn open_in_memory() -> Result<Self, JobsError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    /// ジョブを投入する。`dedup_key` が既存の未完了ジョブ（pending/running）と
    /// 衝突する場合は何もしない（同一ジョブの多重投入を防ぐ）。
    pub async fn enqueue(
        &self,
        kind: &str,
        dedup_key: Option<&str>,
        payload: &impl Serialize,
    ) -> Result<(), JobsError> {
        let payload_json = serde_json::to_string(payload)
            .map_err(|e| JobsError::InvalidPayload(e.to_string()))?;
        let id = Uuid::now_v7().simple().to_string();
        let now = fmt_dt(OffsetDateTime::now_utc());
        sqlx::query(
            "INSERT INTO jobs
                (id, kind, dedup_key, payload, status, attempts, max_attempts, run_after, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, ?, ?)
             ON CONFLICT(kind, dedup_key) WHERE dedup_key IS NOT NULL AND status IN ('pending', 'running') DO NOTHING",
        )
        .bind(id)
        .bind(kind)
        .bind(dedup_key)
        .bind(payload_json)
        .bind(DEFAULT_MAX_ATTEMPTS)
        .bind(now.clone())
        .bind(now.clone())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `updated_at` が `lease` より古い `running` ジョブを `pending` に戻す。
    /// 戻した件数を返す。詳細は [`STALLED_LEASE`] を参照。
    ///
    /// 起動直後（前回プロセスの残骸を回収）と、ワーカーの定期実行から呼ぶ。
    /// `attempts` は `claim_next` 側で増えているので、ここで戻した行も
    /// `max_attempts` に達すれば通常どおり恒久失敗として確定する。
    pub async fn reclaim_stalled(&self, lease: Duration) -> Result<u64, JobsError> {
        let now = OffsetDateTime::now_utc();
        let cutoff = fmt_dt(now - time::Duration::seconds(lease.as_secs() as i64));
        let result = sqlx::query(
            "UPDATE jobs SET status = 'pending', run_after = ?, updated_at = ?, \
                error = COALESCE(error, 'reclaimed after stalled run') \
             WHERE status = 'running' AND updated_at < ?",
        )
        .bind(fmt_dt(now))
        .bind(fmt_dt(now))
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `kinds` のいずれかに一致し、実行可能（`run_after` を過ぎた pending）な
    /// ジョブを 1 件 `running` にして取得する。無ければ `None`。
    pub async fn claim_next(&self, kinds: &[&str]) -> Result<Option<JobRecord>, JobsError> {
        if kinds.is_empty() {
            return Ok(None);
        }
        let now = fmt_dt(OffsetDateTime::now_utc());

        // 複数ワーカーが同時にポーリングするため、素朴な deferred transaction
        // （SELECT 後に UPDATE で書き込みへ昇格）だと、2 本が同時に昇格しようと
        // して "database is locked" になりやすい。`begin_with("BEGIN IMMEDIATE")`
        // で開始時点から書き込みロックを取得し、busy_timeout の待ち合わせに委ねる
        // （sqlx のトランザクション管理と正しく統合される公式 API。生 SQL で
        // "BEGIN"/"COMMIT" を直接 execute するとプール返却時の状態追跡が
        // 壊れてデータ消失につながるため使わない）。
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let mut qb = sqlx::QueryBuilder::new(
            "SELECT id, kind, dedup_key, payload, attempts, max_attempts, error FROM jobs \
             WHERE status = 'pending' AND run_after <= ",
        );
        qb.push_bind(now.clone());
        qb.push(" AND kind IN (");
        {
            let mut sep = qb.separated(", ");
            for k in kinds {
                sep.push_bind(*k);
            }
        }
        qb.push(") ORDER BY created_at LIMIT 1");

        let row = qb.build().fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };

        let id: String = row.try_get("id")?;
        sqlx::query(
            "UPDATE jobs SET status = 'running', attempts = attempts + 1, updated_at = ? WHERE id = ?",
        )
        .bind(now)
        .bind(id.clone())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let payload_str: String = row.try_get("payload")?;
        let payload = serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
        let attempts: i64 = row.try_get("attempts")?;
        Ok(Some(JobRecord {
            id,
            kind: row.try_get("kind")?,
            dedup_key: row.try_get("dedup_key")?,
            payload,
            status: JobStatus::Running,
            attempts: attempts + 1,
            max_attempts: row.try_get("max_attempts")?,
            error: row.try_get("error")?,
        }))
    }

    pub async fn mark_done(&self, id: &str) -> Result<(), JobsError> {
        let now = fmt_dt(OffsetDateTime::now_utc());
        sqlx::query("UPDATE jobs SET status = 'done', error = NULL, updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// リトライ可能な失敗。試行回数が上限に達していれば恒久失敗として確定する。
    pub async fn mark_failed_retry(&self, id: &str, error: &str) -> Result<(), JobsError> {
        let row = sqlx::query("SELECT attempts, max_attempts FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(());
        };
        let attempts: i64 = row.try_get("attempts")?;
        let max_attempts: i64 = row.try_get("max_attempts")?;
        if attempts >= max_attempts {
            return self.mark_failed_permanent(id, error).await;
        }
        let idx = ((attempts.max(1) - 1) as usize).min(RETRY_BACKOFF_SECS.len() - 1);
        let backoff_secs = RETRY_BACKOFF_SECS[idx];
        let now = OffsetDateTime::now_utc();
        let run_after = fmt_dt(now + time::Duration::seconds(backoff_secs));
        sqlx::query(
            "UPDATE jobs SET status = 'pending', error = ?, run_after = ?, updated_at = ? WHERE id = ?",
        )
        .bind(error)
        .bind(run_after)
        .bind(fmt_dt(now))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_failed_permanent(&self, id: &str, error: &str) -> Result<(), JobsError> {
        let now = fmt_dt(OffsetDateTime::now_utc());
        sqlx::query("UPDATE jobs SET status = 'failed', error = ?, updated_at = ? WHERE id = ?")
            .bind(error)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 未処理（pending/running）のジョブが 1 件も無くなるまでの目安として使う件数。
    /// CLI の一括投入コマンドが「処理完了」を判定するために使う。
    pub async fn count_incomplete(&self, kinds: &[&str]) -> Result<i64, JobsError> {
        if kinds.is_empty() {
            return Ok(0);
        }
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT COUNT(*) as c FROM jobs WHERE status IN ('pending', 'running') AND kind IN (",
        );
        {
            let mut sep = qb.separated(", ");
            for k in kinds {
                sep.push_bind(*k);
            }
        }
        qb.push(")");
        let row = qb.build().fetch_one(&self.pool).await?;
        Ok(row.try_get("c")?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn enqueue_dedups_pending_jobs() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", Some("f1:c1:thumbnail"), &json!({"n": 1}))
            .await
            .unwrap();
        store
            .enqueue("preview.generate", Some("f1:c1:thumbnail"), &json!({"n": 2}))
            .await
            .unwrap();
        assert_eq!(
            store.count_incomplete(&["preview.generate"]).await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn claim_next_marks_running_and_returns_payload() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", None, &json!({"file_id": "abc"}))
            .await
            .unwrap();

        let job = store
            .claim_next(&["preview.generate"])
            .await
            .unwrap()
            .expect("job should be claimable");
        assert_eq!(job.kind, "preview.generate");
        assert_eq!(job.payload["file_id"], "abc");
        assert_eq!(job.attempts, 1);

        // すでに running のジョブは再取得されない。
        assert!(store
            .claim_next(&["preview.generate"])
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn mark_done_clears_incomplete_count() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", None, &json!({}))
            .await
            .unwrap();
        let job = store.claim_next(&["preview.generate"]).await.unwrap().unwrap();
        store.mark_done(&job.id).await.unwrap();
        assert_eq!(
            store.count_incomplete(&["preview.generate"]).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn mark_failed_retry_backs_off_before_next_claim() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", None, &json!({}))
            .await
            .unwrap();

        let job = store.claim_next(&["preview.generate"]).await.unwrap().unwrap();
        store.mark_failed_retry(&job.id, "transient").await.unwrap();

        // バックオフでまだ run_after に達していないため、すぐには再取得できない。
        assert!(store
            .claim_next(&["preview.generate"])
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store.count_incomplete(&["preview.generate"]).await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn mark_failed_retry_becomes_permanent_after_max_attempts() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", None, &json!({}))
            .await
            .unwrap();

        // DEFAULT_MAX_ATTEMPTS = 3。attempts を直接引き上げて上限到達を再現する。
        sqlx::query("UPDATE jobs SET attempts = max_attempts")
            .execute(&store.pool)
            .await
            .unwrap();
        let job = store.claim_next(&["preview.generate"]).await.unwrap().unwrap();
        store.mark_failed_retry(&job.id, "transient").await.unwrap();

        assert_eq!(
            store.count_incomplete(&["preview.generate"]).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn dedup_does_not_block_reenqueue_after_completion() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", Some("f1:c1:thumbnail"), &json!({}))
            .await
            .unwrap();
        let job = store.claim_next(&["preview.generate"]).await.unwrap().unwrap();
        store.mark_done(&job.id).await.unwrap();

        // done になった後は同じ dedup_key で再投入できる（cache-warm の再試行、
        // cache-regenerate の強制再生成に必要）。
        store
            .enqueue("preview.generate", Some("f1:c1:thumbnail"), &json!({}))
            .await
            .unwrap();
        assert_eq!(
            store.count_incomplete(&["preview.generate"]).await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn dedup_does_not_block_reenqueue_after_permanent_failure() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", Some("f1:c1:thumbnail"), &json!({}))
            .await
            .unwrap();
        let job = store.claim_next(&["preview.generate"]).await.unwrap().unwrap();
        store.mark_failed_permanent(&job.id, "unsupported").await.unwrap();

        store
            .enqueue("preview.generate", Some("f1:c1:thumbnail"), &json!({}))
            .await
            .unwrap();
        assert_eq!(
            store.count_incomplete(&["preview.generate"]).await.unwrap(),
            1
        );
    }

    /// プロセスがジョブ実行中に落ちると `running` の行が残る。この行は
    /// `claim_next`（pending のみ対象）にも拾われず、dedup の部分インデックスが
    /// `running` を含むので同じ dedup_key の再投入も弾かれる。回収機構が無いと
    /// その (file, commit, variant) は永久に生成されなくなる。
    #[tokio::test]
    async fn stalled_running_job_is_reclaimed() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", Some("f1:c1:thumbnail"), &json!({}))
            .await
            .unwrap();
        let job = store.claim_next(&["preview.generate"]).await.unwrap().unwrap();

        // ここでプロセスが落ちたとする。running のまま固着し、
        // 再投入も再取得もできない。
        store
            .enqueue("preview.generate", Some("f1:c1:thumbnail"), &json!({}))
            .await
            .unwrap();
        assert_eq!(
            store.count_incomplete(&["preview.generate"]).await.unwrap(),
            1,
            "dedup により再投入は弾かれる"
        );
        assert!(
            store.claim_next(&["preview.generate"]).await.unwrap().is_none(),
            "running のままなので再取得もされない"
        );

        // リース切れとして回収すれば再び処理できる。
        assert_eq!(store.reclaim_stalled(Duration::ZERO).await.unwrap(), 1);
        let reclaimed = store
            .claim_next(&["preview.generate"])
            .await
            .unwrap()
            .expect("回収後は再取得できる");
        assert_eq!(reclaimed.id, job.id);
        assert_eq!(reclaimed.attempts, 2, "回収後の再実行も試行回数に数える");
    }

    /// リース内で実行中のジョブは回収しない（二重実行を避ける）。
    #[tokio::test]
    async fn reclaim_leaves_recently_claimed_jobs_alone() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", None, &json!({}))
            .await
            .unwrap();
        store.claim_next(&["preview.generate"]).await.unwrap().unwrap();

        assert_eq!(store.reclaim_stalled(STALLED_LEASE).await.unwrap(), 0);
        assert!(store.claim_next(&["preview.generate"]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mark_failed_permanent_removes_from_incomplete() {
        let store = JobStore::open_in_memory().await.unwrap();
        store
            .enqueue("preview.generate", None, &json!({}))
            .await
            .unwrap();
        let job = store.claim_next(&["preview.generate"]).await.unwrap().unwrap();
        store.mark_failed_permanent(&job.id, "unsupported").await.unwrap();
        assert_eq!(
            store.count_incomplete(&["preview.generate"]).await.unwrap(),
            0
        );
    }
}
