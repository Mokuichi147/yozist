//! SQLite 実装（スケルトン）。
//!
//! 主要メソッドは `unimplemented!()` で TODO を明示し、スキーマ初期化と
//! プール生成・WAL 有効化のみ実装する。次タスクで穴埋めする。
//!
//! # TODO
//! - [ ] CRUD メソッドの実装（sqlx クエリ）
//! - [ ] トランザクション境界（複数操作を 1 つの commit で）
//! - [ ] 並行性テスト（複数 task からの insert）

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;
use yozist_core::{Commit, FileId, FileMeta, Series, SeriesId, SeriesMember, Tag, TagId};

use crate::{DbError, MetaStore};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub struct SqliteMetaStore {
    pool: SqlitePool,
}

impl SqliteMetaStore {
    /// ファイルパスから接続し、マイグレーション実行 + WAL 有効化。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
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

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl MetaStore for SqliteMetaStore {
    async fn insert_file(&self, _meta: &FileMeta) -> Result<(), DbError> {
        // TODO: implement
        Err(DbError::Invalid("insert_file not yet implemented".into()))
    }
    async fn get_file(&self, _id: &FileId) -> Result<Option<FileMeta>, DbError> {
        // TODO: implement
        Ok(None)
    }
    async fn update_file(&self, _meta: &FileMeta) -> Result<(), DbError> {
        Err(DbError::Invalid("update_file not yet implemented".into()))
    }
    async fn list_files(&self, _limit: u32, _offset: u32) -> Result<Vec<FileMeta>, DbError> {
        Ok(vec![])
    }

    async fn upsert_tag(&self, tag: &Tag) -> Result<TagId, DbError> {
        // TODO: implement
        Ok(tag.id)
    }
    async fn attach_tag(&self, _file: &FileId, _tag: &TagId) -> Result<(), DbError> {
        Err(DbError::Invalid("attach_tag not yet implemented".into()))
    }
    async fn detach_tag(&self, _file: &FileId, _tag: &TagId) -> Result<(), DbError> {
        Err(DbError::Invalid("detach_tag not yet implemented".into()))
    }
    async fn list_files_by_tags(&self, _tags: &[TagId]) -> Result<Vec<FileMeta>, DbError> {
        Ok(vec![])
    }

    async fn upsert_series(&self, series: &Series) -> Result<SeriesId, DbError> {
        Ok(series.id)
    }
    async fn add_to_series(&self, _member: &SeriesMember) -> Result<(), DbError> {
        Err(DbError::Invalid("add_to_series not yet implemented".into()))
    }
    async fn list_series_members(
        &self,
        _series: &SeriesId,
    ) -> Result<Vec<SeriesMember>, DbError> {
        Ok(vec![])
    }

    async fn insert_commit(&self, _commit: &Commit) -> Result<(), DbError> {
        Err(DbError::Invalid("insert_commit not yet implemented".into()))
    }
    async fn list_commits(&self, _file: &FileId) -> Result<Vec<Commit>, DbError> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_creates_db_and_runs_migrations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let store = SqliteMetaStore::open(&path).await.unwrap();
        // 接続できればOK。スキーマ存在確認。
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(row.0, 0);
    }
}
