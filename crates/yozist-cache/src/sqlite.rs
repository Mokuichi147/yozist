//! 生成結果の永続化。SQLite に `preview_cache` テーブルとして保存する。
//! UUID・時刻の保存形式は `yozist-db` と同じ（UUID は hex テキスト、時刻は RFC3339）。

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use time::OffsetDateTime;

use crate::{CacheError, Variant};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// キャッシュ済み成果物の情報（`status = 'ready'` のときのみ得られる）。
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub rel_path: String,
    pub mime: String,
    pub byte_size: i64,
    pub width: i32,
    pub height: i32,
}

/// `CacheStore::lookup` の結果。
#[derive(Debug, Clone)]
pub enum Lookup {
    Ready(CacheEntry),
    Pending,
    Failed(Option<String>),
    Missing,
}

pub struct CacheStore {
    pool: SqlitePool,
}

fn fmt_dt(dt: OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .expect("OffsetDateTime を RFC3339 にフォーマット")
}

impl CacheStore {
    /// ファイルパスから接続し、マイグレーション実行 + WAL 有効化。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, CacheError> {
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
    pub async fn open_in_memory() -> Result<Self, CacheError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn lookup(
        &self,
        file_id: &str,
        commit_id: &str,
        variant: Variant,
    ) -> Result<Lookup, CacheError> {
        let row = sqlx::query(
            "SELECT status, rel_path, mime, byte_size, width, height, error \
             FROM preview_cache WHERE file_id = ? AND commit_id = ? AND variant = ?",
        )
        .bind(file_id)
        .bind(commit_id)
        .bind(variant.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(Lookup::Missing);
        };
        let status: String = row.try_get("status")?;
        match status.as_str() {
            "ready" => {
                let rel_path: Option<String> = row.try_get("rel_path")?;
                let mime: Option<String> = row.try_get("mime")?;
                let (Some(rel_path), Some(mime)) = (rel_path, mime) else {
                    return Ok(Lookup::Missing);
                };
                Ok(Lookup::Ready(CacheEntry {
                    rel_path,
                    mime,
                    byte_size: row.try_get("byte_size")?,
                    width: row.try_get("width")?,
                    height: row.try_get("height")?,
                }))
            }
            "failed" => Ok(Lookup::Failed(row.try_get("error")?)),
            _ => Ok(Lookup::Pending),
        }
    }

    /// 生成ジョブを投入したことを記録する（`ready` な行は上書きしない）。
    pub async fn mark_pending(
        &self,
        file_id: &str,
        commit_id: &str,
        variant: Variant,
    ) -> Result<(), CacheError> {
        let now = fmt_dt(OffsetDateTime::now_utc());
        sqlx::query(
            "INSERT INTO preview_cache (file_id, commit_id, variant, status, created_at, updated_at) \
             VALUES (?, ?, ?, 'pending', ?, ?) \
             ON CONFLICT(file_id, commit_id, variant) DO UPDATE SET \
                status = CASE WHEN preview_cache.status = 'ready' THEN preview_cache.status ELSE 'pending' END, \
                updated_at = excluded.updated_at",
        )
        .bind(file_id)
        .bind(commit_id)
        .bind(variant.as_str())
        .bind(now.clone())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 現在の状態に関わらず強制的に `pending` へ戻す（CLI の強制再生成用）。
    /// `mark_pending` と異なり `ready` な行も上書きする。
    pub async fn reset_to_pending(
        &self,
        file_id: &str,
        commit_id: &str,
        variant: Variant,
    ) -> Result<(), CacheError> {
        let now = fmt_dt(OffsetDateTime::now_utc());
        sqlx::query(
            "INSERT INTO preview_cache (file_id, commit_id, variant, status, created_at, updated_at) \
             VALUES (?, ?, ?, 'pending', ?, ?) \
             ON CONFLICT(file_id, commit_id, variant) DO UPDATE SET \
                status = 'pending', updated_at = excluded.updated_at",
        )
        .bind(file_id)
        .bind(commit_id)
        .bind(variant.as_str())
        .bind(now.clone())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn mark_ready(
        &self,
        file_id: &str,
        commit_id: &str,
        variant: Variant,
        rel_path: &str,
        mime: &str,
        byte_size: i64,
        width: i32,
        height: i32,
    ) -> Result<(), CacheError> {
        let now = fmt_dt(OffsetDateTime::now_utc());
        sqlx::query(
            "INSERT INTO preview_cache \
                (file_id, commit_id, variant, status, rel_path, mime, byte_size, width, height, error, created_at, updated_at) \
             VALUES (?, ?, ?, 'ready', ?, ?, ?, ?, ?, NULL, ?, ?) \
             ON CONFLICT(file_id, commit_id, variant) DO UPDATE SET \
                status = 'ready', rel_path = excluded.rel_path, mime = excluded.mime, \
                byte_size = excluded.byte_size, width = excluded.width, height = excluded.height, \
                error = NULL, updated_at = excluded.updated_at",
        )
        .bind(file_id)
        .bind(commit_id)
        .bind(variant.as_str())
        .bind(rel_path)
        .bind(mime)
        .bind(byte_size)
        .bind(width)
        .bind(height)
        .bind(now.clone())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_failed(
        &self,
        file_id: &str,
        commit_id: &str,
        variant: Variant,
        error: &str,
    ) -> Result<(), CacheError> {
        let now = fmt_dt(OffsetDateTime::now_utc());
        sqlx::query(
            "INSERT INTO preview_cache (file_id, commit_id, variant, status, error, created_at, updated_at) \
             VALUES (?, ?, ?, 'failed', ?, ?, ?) \
             ON CONFLICT(file_id, commit_id, variant) DO UPDATE SET \
                status = 'failed', error = excluded.error, updated_at = excluded.updated_at",
        )
        .bind(file_id)
        .bind(commit_id)
        .bind(variant.as_str())
        .bind(error)
        .bind(now.clone())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `file_ids` のうち、指定 variant の `ready` 行を持たないものを返す
    /// （未生成 or 生成失敗 = バックフィル対象）。
    pub async fn list_missing_for(
        &self,
        file_ids: &[String],
        variant: Variant,
    ) -> Result<Vec<String>, CacheError> {
        if file_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT file_id FROM preview_cache WHERE variant = ",
        );
        qb.push_bind(variant.as_str());
        qb.push(" AND status = 'ready' AND file_id IN (");
        {
            let mut sep = qb.separated(", ");
            for id in file_ids {
                sep.push_bind(id);
            }
        }
        qb.push(")");
        let rows = qb.build().fetch_all(&self.pool).await?;
        let ready: std::collections::HashSet<String> = rows
            .into_iter()
            .map(|r| r.try_get::<String, _>("file_id"))
            .collect::<Result<_, _>>()?;
        Ok(file_ids
            .iter()
            .filter(|id| !ready.contains(id.as_str()))
            .cloned()
            .collect())
    }

    /// `file_id` に紐づく全キャッシュ行を削除する。削除した行の `rel_path`
    /// （実ファイル削除用）を返す。
    pub async fn delete_by_file(&self, file_id: &str) -> Result<Vec<String>, CacheError> {
        let rows = sqlx::query("SELECT rel_path FROM preview_cache WHERE file_id = ?")
            .bind(file_id)
            .fetch_all(&self.pool)
            .await?;
        let paths = rows
            .into_iter()
            .filter_map(|r| r.try_get::<Option<String>, _>("rel_path").ok().flatten())
            .collect();
        sqlx::query("DELETE FROM preview_cache WHERE file_id = ?")
            .bind(file_id)
            .execute(&self.pool)
            .await?;
        Ok(paths)
    }

    /// `file_id` に紐づく行のうち `current_commit_id` と異なる（陳腐化した）
    /// ものを削除する。削除した行の `rel_path` を返す。
    pub async fn delete_stale(
        &self,
        file_id: &str,
        current_commit_id: &str,
    ) -> Result<Vec<String>, CacheError> {
        let rows = sqlx::query(
            "SELECT rel_path FROM preview_cache WHERE file_id = ? AND commit_id != ?",
        )
        .bind(file_id)
        .bind(current_commit_id)
        .fetch_all(&self.pool)
        .await?;
        let paths = rows
            .into_iter()
            .filter_map(|r| r.try_get::<Option<String>, _>("rel_path").ok().flatten())
            .collect();
        sqlx::query("DELETE FROM preview_cache WHERE file_id = ? AND commit_id != ?")
            .bind(file_id)
            .bind(current_commit_id)
            .execute(&self.pool)
            .await?;
        Ok(paths)
    }

    /// キャッシュ DB に登場する全 `file_id`（重複なし）。孤立掃除タスクが
    /// メタ DB と突き合わせるために使う。
    pub async fn list_distinct_file_ids(&self) -> Result<Vec<String>, CacheError> {
        let rows = sqlx::query("SELECT DISTINCT file_id FROM preview_cache")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| r.try_get::<String, _>("file_id").map_err(CacheError::from))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_then_ready_roundtrip() {
        let store = CacheStore::open_in_memory().await.unwrap();
        assert!(matches!(
            store.lookup("f1", "c1", Variant::Thumbnail).await.unwrap(),
            Lookup::Missing
        ));

        store.mark_pending("f1", "c1", Variant::Thumbnail).await.unwrap();
        assert!(matches!(
            store.lookup("f1", "c1", Variant::Thumbnail).await.unwrap(),
            Lookup::Pending
        ));

        store
            .mark_ready("f1", "c1", Variant::Thumbnail, "ab/f1-c1-thumbnail.jpg", "image/jpeg", 1234, 480, 320)
            .await
            .unwrap();
        match store.lookup("f1", "c1", Variant::Thumbnail).await.unwrap() {
            Lookup::Ready(entry) => {
                assert_eq!(entry.rel_path, "ab/f1-c1-thumbnail.jpg");
                assert_eq!(entry.mime, "image/jpeg");
            }
            other => panic!("expected Ready, got {other:?}"),
        }

        // ready な行は mark_pending で上書きされない。
        store.mark_pending("f1", "c1", Variant::Thumbnail).await.unwrap();
        assert!(matches!(
            store.lookup("f1", "c1", Variant::Thumbnail).await.unwrap(),
            Lookup::Ready(_)
        ));
    }

    #[tokio::test]
    async fn mark_failed_then_lookup() {
        let store = CacheStore::open_in_memory().await.unwrap();
        store
            .mark_failed("f1", "c1", Variant::Preview, "unsupported format")
            .await
            .unwrap();
        match store.lookup("f1", "c1", Variant::Preview).await.unwrap() {
            Lookup::Failed(Some(msg)) => assert_eq!(msg, "unsupported format"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_missing_for_excludes_ready() {
        let store = CacheStore::open_in_memory().await.unwrap();
        store
            .mark_ready("f1", "c1", Variant::Thumbnail, "p", "image/jpeg", 1, 1, 1)
            .await
            .unwrap();
        store.mark_failed("f2", "c1", Variant::Thumbnail, "err").await.unwrap();

        let ids = vec!["f1".to_string(), "f2".to_string(), "f3".to_string()];
        let missing = store.list_missing_for(&ids, Variant::Thumbnail).await.unwrap();
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&"f2".to_string()));
        assert!(missing.contains(&"f3".to_string()));
    }

    #[tokio::test]
    async fn delete_by_file_removes_all_variants() {
        let store = CacheStore::open_in_memory().await.unwrap();
        store
            .mark_ready("f1", "c1", Variant::Thumbnail, "a.jpg", "image/jpeg", 1, 1, 1)
            .await
            .unwrap();
        store
            .mark_ready("f1", "c1", Variant::Preview, "b.jpg", "image/jpeg", 1, 1, 1)
            .await
            .unwrap();
        let deleted = store.delete_by_file("f1").await.unwrap();
        assert_eq!(deleted.len(), 2);
        assert!(matches!(
            store.lookup("f1", "c1", Variant::Thumbnail).await.unwrap(),
            Lookup::Missing
        ));
    }

    #[tokio::test]
    async fn reset_to_pending_overrides_ready() {
        let store = CacheStore::open_in_memory().await.unwrap();
        store
            .mark_ready("f1", "c1", Variant::Thumbnail, "a.jpg", "image/jpeg", 1, 1, 1)
            .await
            .unwrap();
        store.reset_to_pending("f1", "c1", Variant::Thumbnail).await.unwrap();
        assert!(matches!(
            store.lookup("f1", "c1", Variant::Thumbnail).await.unwrap(),
            Lookup::Pending
        ));
    }

    #[tokio::test]
    async fn delete_stale_keeps_current_commit() {
        let store = CacheStore::open_in_memory().await.unwrap();
        store
            .mark_ready("f1", "c1", Variant::Thumbnail, "old.jpg", "image/jpeg", 1, 1, 1)
            .await
            .unwrap();
        store
            .mark_ready("f1", "c2", Variant::Thumbnail, "new.jpg", "image/jpeg", 1, 1, 1)
            .await
            .unwrap();
        let deleted = store.delete_stale("f1", "c2").await.unwrap();
        assert_eq!(deleted, vec!["old.jpg".to_string()]);
        assert!(matches!(
            store.lookup("f1", "c2", Variant::Thumbnail).await.unwrap(),
            Lookup::Ready(_)
        ));
    }
}
