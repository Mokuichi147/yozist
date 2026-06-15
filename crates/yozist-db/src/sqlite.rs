//! SQLite 実装。
//!
//! - UUID は TEXT 列に hex 形式で保存（`as_simple()` を介して `-` を含む標準形）
//! - 時刻は `time::OffsetDateTime` を ISO8601 で `TEXT` 列に保存
//! - 楽観ロックは `files.version` カラムで実現
//!
//! # TODO
//! - [ ] トランザクション API の公開（複数操作を 1 つの commit にまとめる）
//! - [ ] FTS5 によるフルテキスト検索
//! - [ ] ACL を考慮した `list_files_*` のフィルタ（WHERE 句注入ヘルパ）

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use uuid::Uuid;
use yozist_core::{
    ActorId, BlobId, Commit, CommitId, FileId, FileMeta, QueryDef, SavedQuery, SavedQueryId,
    Series, SeriesId, SeriesMember, Tag, TagId, TagKind,
};

use crate::{DbError, FileSort, MetaStore};

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

    /// メモリ DB（テスト用）。
    pub async fn open_in_memory() -> Result<Self, DbError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1) // メモリ DB は接続毎に別 DB なので 1 本固定
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

// ---------------------------------------------------------------------------
// 行 → 型 のマッピング
// ---------------------------------------------------------------------------

fn parse_uuid(s: &str) -> Result<Uuid, DbError> {
    Uuid::parse_str(s).map_err(|e| DbError::Invalid(format!("uuid: {e}")))
}

fn parse_dt(s: &str) -> Result<time::OffsetDateTime, DbError> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map_err(|e| DbError::Invalid(format!("datetime: {e}")))
}

fn fmt_dt(dt: time::OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn row_to_file(row: SqliteRow) -> Result<FileMeta, DbError> {
    let id: String = row.try_get("id")?;
    let display_name: String = row.try_get("display_name")?;
    let size: i64 = row.try_get("size")?;
    let mime: Option<String> = row.try_get("mime")?;
    let charset: Option<String> = row.try_get("charset")?;
    let current_commit: Option<String> = row.try_get("current_commit")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;
    let deleted: i64 = row.try_get("deleted")?;
    let created_by: Option<String> = row.try_get("created_by")?;
    let updated_by: Option<String> = row.try_get("updated_by")?;
    let created_by_user_id: Option<i64> = row.try_get("created_by_user_id")?;
    let updated_by_user_id: Option<i64> = row.try_get("updated_by_user_id")?;
    Ok(FileMeta {
        id: FileId::from_uuid(parse_uuid(&id)?),
        display_name,
        size: size as u64,
        mime,
        charset,
        current_commit: current_commit
            .map(|s| parse_uuid(&s).map(CommitId::from_uuid))
            .transpose()?,
        created_at: parse_dt(&created_at)?,
        updated_at: parse_dt(&updated_at)?,
        deleted: deleted != 0,
        created_by,
        updated_by,
        created_by_user_id,
        updated_by_user_id,
    })
}

fn parse_tag_kind(s: &str) -> Result<TagKind, DbError> {
    match s {
        "system" => Ok(TagKind::System),
        "ai" => Ok(TagKind::Ai),
        "manual" => Ok(TagKind::Manual),
        other => Err(DbError::Invalid(format!("unknown tag kind: {other}"))),
    }
}

fn tag_kind_str(k: TagKind) -> &'static str {
    match k {
        TagKind::System => "system",
        TagKind::Ai => "ai",
        TagKind::Manual => "manual",
    }
}

fn row_to_tag(row: SqliteRow) -> Result<Tag, DbError> {
    let id: String = row.try_get("id")?;
    let name: String = row.try_get("name")?;
    let kind: String = row.try_get("kind")?;
    let confidence: Option<f32> = row.try_get("confidence")?;
    Ok(Tag {
        id: TagId::from_uuid(parse_uuid(&id)?),
        name,
        kind: parse_tag_kind(&kind)?,
        confidence,
    })
}

fn row_to_series(row: SqliteRow) -> Result<Series, DbError> {
    let id: String = row.try_get("id")?;
    let name: String = row.try_get("name")?;
    let description: Option<String> = row.try_get("description")?;
    Ok(Series {
        id: SeriesId::from_uuid(parse_uuid(&id)?),
        name,
        description,
    })
}

fn row_to_saved_query(row: SqliteRow) -> Result<SavedQuery, DbError> {
    let id: String = row.try_get("id")?;
    let name: String = row.try_get("name")?;
    let query_json: String = row.try_get("query_json")?;
    let description: Option<String> = row.try_get("description")?;
    let created_by: Option<i64> = row.try_get("created_by")?;
    let created_at: String = row.try_get("created_at")?;
    let expires_at: Option<String> = row.try_get("expires_at")?;

    let query: QueryDef = serde_json::from_str(&query_json)
        .map_err(|e| DbError::Invalid(format!("query json: {e}")))?;
    Ok(SavedQuery {
        id: SavedQueryId::from_uuid(parse_uuid(&id)?),
        name,
        query,
        description,
        created_by,
        created_at: parse_dt(&created_at)?,
        expires_at: expires_at.map(|s| parse_dt(&s)).transpose()?,
    })
}

fn row_to_commit(row: SqliteRow) -> Result<Commit, DbError> {
    let id: String = row.try_get("id")?;
    let file_id: String = row.try_get("file_id")?;
    let parent: Option<String> = row.try_get("parent")?;
    let actor: String = row.try_get("actor")?;
    let blob: String = row.try_get("blob")?;
    let format_id: String = row.try_get("format_id")?;
    let timestamp: String = row.try_get("timestamp")?;
    let message: Option<String> = row.try_get("message")?;
    let committed_by: Option<String> = row.try_get("committed_by")?;
    let committed_by_user_id: Option<i64> = row.try_get("committed_by_user_id")?;
    Ok(Commit {
        id: CommitId::from_uuid(parse_uuid(&id)?),
        file_id: FileId::from_uuid(parse_uuid(&file_id)?),
        parent: parent
            .map(|s| parse_uuid(&s).map(CommitId::from_uuid))
            .transpose()?,
        actor: ActorId::from_uuid(parse_uuid(&actor)?),
        blob: BlobId::from_hex(blob),
        format_id,
        timestamp: parse_dt(&timestamp)?,
        message,
        committed_by,
        committed_by_user_id,
    })
}

// ---------------------------------------------------------------------------
// MetaStore 実装
// ---------------------------------------------------------------------------

#[async_trait]
impl MetaStore for SqliteMetaStore {
    async fn insert_file(&self, meta: &FileMeta) -> Result<(), DbError> {
        sqlx::query(
            r#"INSERT INTO files
               (id, display_name, size, mime, charset, current_commit,
                created_at, updated_at, deleted, created_by, updated_by,
                created_by_user_id, updated_by_user_id, version)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)"#,
        )
        .bind(meta.id.to_string())
        .bind(&meta.display_name)
        .bind(meta.size as i64)
        .bind(&meta.mime)
        .bind(&meta.charset)
        .bind(meta.current_commit.map(|c| c.to_string()))
        .bind(fmt_dt(meta.created_at))
        .bind(fmt_dt(meta.updated_at))
        .bind(meta.deleted as i64)
        .bind(&meta.created_by)
        .bind(&meta.updated_by)
        .bind(meta.created_by_user_id)
        .bind(meta.updated_by_user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_file(&self, id: &FileId) -> Result<Option<FileMeta>, DbError> {
        let row = sqlx::query("SELECT * FROM files WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_file).transpose()
    }

    async fn update_file(&self, meta: &FileMeta) -> Result<(), DbError> {
        // 楽観ロック: 現行 version を取得し、+1 で更新。
        let res = sqlx::query(
            r#"UPDATE files SET
                 display_name = ?, size = ?, mime = ?, charset = ?,
                 current_commit = ?, updated_at = ?, deleted = ?,
                 created_by = ?, updated_by = ?,
                 created_by_user_id = ?, updated_by_user_id = ?,
                 version = version + 1
               WHERE id = ?"#,
        )
        .bind(&meta.display_name)
        .bind(meta.size as i64)
        .bind(&meta.mime)
        .bind(&meta.charset)
        .bind(meta.current_commit.map(|c| c.to_string()))
        .bind(fmt_dt(meta.updated_at))
        .bind(meta.deleted as i64)
        .bind(&meta.created_by)
        .bind(&meta.updated_by)
        .bind(meta.created_by_user_id)
        .bind(meta.updated_by_user_id)
        .bind(meta.id.to_string())
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    async fn list_files(&self, limit: u32, offset: u32) -> Result<Vec<FileMeta>, DbError> {
        self.list_files_sorted(limit, offset, FileSort::UpdatedAt, false)
            .await
    }

    async fn list_files_sorted(
        &self,
        limit: u32,
        offset: u32,
        sort: FileSort,
        asc: bool,
    ) -> Result<Vec<FileMeta>, DbError> {
        // ORDER BY は固定の候補からの選択のみ（バインド不可のため文字列結合だが注入余地なし）。
        let dir = if asc { "ASC" } else { "DESC" };
        let order = match sort {
            FileSort::UpdatedAt => format!("updated_at {dir}"),
            FileSort::CreatedAt => format!("created_at {dir}"),
            FileSort::Name => format!("display_name COLLATE NOCASE {dir}, updated_at DESC"),
            FileSort::Size => format!("size {dir}, updated_at DESC"),
        };
        let sql = format!(
            "SELECT * FROM files WHERE deleted = 0 ORDER BY {order} LIMIT ? OFFSET ?"
        );
        let rows = sqlx::query(&sql)
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_file).collect()
    }


    async fn upsert_tag(&self, tag: &Tag) -> Result<TagId, DbError> {
        // 同名タグがあればそのIDを返す（kindの優先度: Manual > AI > System）。
        // 新規ならINSERT。
        let existing: Option<(String, String)> =
            sqlx::query_as("SELECT id, kind FROM tags WHERE name = ?")
                .bind(&tag.name)
                .fetch_optional(&self.pool)
                .await?;

        if let Some((existing_id, existing_kind)) = existing {
            let existing_kind = parse_tag_kind(&existing_kind)?;
            // 優先度ルール: 新しい kind が現状より強ければ upgrade。
            if priority(tag.kind) > priority(existing_kind) {
                sqlx::query("UPDATE tags SET kind = ?, confidence = ? WHERE id = ?")
                    .bind(tag_kind_str(tag.kind))
                    .bind(tag.confidence)
                    .bind(&existing_id)
                    .execute(&self.pool)
                    .await?;
            }
            return Ok(TagId::from_uuid(parse_uuid(&existing_id)?));
        }

        sqlx::query("INSERT INTO tags (id, name, kind, confidence) VALUES (?, ?, ?, ?)")
            .bind(tag.id.to_string())
            .bind(&tag.name)
            .bind(tag_kind_str(tag.kind))
            .bind(tag.confidence)
            .execute(&self.pool)
            .await?;
        Ok(tag.id)
    }

    async fn get_tag(&self, id: &TagId) -> Result<Option<Tag>, DbError> {
        let row = sqlx::query("SELECT id, name, kind, confidence FROM tags WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_tag).transpose()
    }

    async fn get_tag_by_name(&self, name: &str) -> Result<Option<Tag>, DbError> {
        let row = sqlx::query("SELECT id, name, kind, confidence FROM tags WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_tag).transpose()
    }

    async fn list_tags(&self) -> Result<Vec<Tag>, DbError> {
        let rows = sqlx::query("SELECT id, name, kind, confidence FROM tags ORDER BY name ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_tag).collect()
    }

    async fn list_tags_by_usage(&self) -> Result<Vec<Tag>, DbError> {
        let rows = sqlx::query(
            r#"SELECT t.id, t.name, t.kind, t.confidence
               FROM tags t
               LEFT JOIN file_tags ft ON ft.tag_id = t.id
               GROUP BY t.id, t.name, t.kind, t.confidence
               ORDER BY COUNT(ft.file_id) DESC, t.name ASC"#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_tag).collect()
    }

    async fn rename_tag(&self, id: &TagId, new_name: &str) -> Result<(), DbError> {
        let res = sqlx::query("UPDATE tags SET name = ? WHERE id = ?")
            .bind(new_name)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    async fn delete_tag(&self, id: &TagId) -> Result<(), DbError> {
        // file_tags は CASCADE で自動削除される
        let res = sqlx::query("DELETE FROM tags WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    async fn list_tags_of(&self, file: &FileId) -> Result<Vec<Tag>, DbError> {
        let rows = sqlx::query(
            r#"SELECT t.id, t.name, t.kind, t.confidence
               FROM tags t
               JOIN file_tags ft ON ft.tag_id = t.id
               WHERE ft.file_id = ?
               ORDER BY t.name ASC"#,
        )
        .bind(file.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_tag).collect()
    }

    async fn list_tags_of_many(
        &self,
        files: &[FileId],
    ) -> Result<Vec<(FileId, Tag)>, DbError> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; files.len()].join(",");
        let sql = format!(
            r#"SELECT ft.file_id AS file_id, t.id, t.name, t.kind, t.confidence
               FROM tags t
               JOIN file_tags ft ON ft.tag_id = t.id
               WHERE ft.file_id IN ({placeholders})
               ORDER BY ft.file_id, t.name ASC"#
        );
        let mut q = sqlx::query(&sql);
        for f in files {
            q = q.bind(f.to_string());
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                let file_id: String = row.try_get("file_id")?;
                let file_id = FileId::from_uuid(parse_uuid(&file_id)?);
                let tag = row_to_tag(row)?;
                Ok((file_id, tag))
            })
            .collect()
    }

    async fn attach_tag(&self, file: &FileId, tag: &TagId) -> Result<(), DbError> {
        sqlx::query(
            r#"INSERT INTO file_tags (file_id, tag_id) VALUES (?, ?)
               ON CONFLICT DO NOTHING"#,
        )
        .bind(file.to_string())
        .bind(tag.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn detach_tag(&self, file: &FileId, tag: &TagId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM file_tags WHERE file_id = ? AND tag_id = ?")
            .bind(file.to_string())
            .bind(tag.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_files_by_tags(&self, tags: &[TagId]) -> Result<Vec<FileMeta>, DbError> {
        if tags.is_empty() {
            return self.list_files(1000, 0).await;
        }
        // すべてのタグを持つ（AND 条件）ファイルを取得。
        // SQLite の placeholder 制限に注意して動的にビルド。
        let placeholders = tags.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            r#"SELECT f.* FROM files f
               WHERE f.deleted = 0
                 AND (
                   SELECT COUNT(DISTINCT ft.tag_id)
                   FROM file_tags ft
                   WHERE ft.file_id = f.id
                     AND ft.tag_id IN ({})
                 ) = ?
               ORDER BY f.updated_at DESC"#,
            placeholders
        );
        let mut q = sqlx::query(&sql);
        for t in tags {
            q = q.bind(t.to_string());
        }
        q = q.bind(tags.len() as i64);
        let rows = q.fetch_all(&self.pool).await?;
        rows.into_iter().map(row_to_file).collect()
    }

    async fn upsert_series(&self, series: &Series) -> Result<SeriesId, DbError> {
        if let Some((existing_id,)) =
            sqlx::query_as::<_, (String,)>("SELECT id FROM series WHERE name = ?")
                .bind(&series.name)
                .fetch_optional(&self.pool)
                .await?
        {
            return Ok(SeriesId::from_uuid(parse_uuid(&existing_id)?));
        }
        sqlx::query("INSERT INTO series (id, name, description) VALUES (?, ?, ?)")
            .bind(series.id.to_string())
            .bind(&series.name)
            .bind(&series.description)
            .execute(&self.pool)
            .await?;
        Ok(series.id)
    }

    async fn get_series(&self, id: &SeriesId) -> Result<Option<Series>, DbError> {
        let row = sqlx::query("SELECT id, name, description FROM series WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_series).transpose()
    }

    async fn list_series(&self) -> Result<Vec<Series>, DbError> {
        let rows = sqlx::query("SELECT id, name, description FROM series ORDER BY name ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_series).collect()
    }

    async fn rename_series(
        &self,
        id: &SeriesId,
        new_name: &str,
        description: Option<&str>,
    ) -> Result<(), DbError> {
        let res = sqlx::query("UPDATE series SET name = ?, description = ? WHERE id = ?")
            .bind(new_name)
            .bind(description)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    async fn delete_series(&self, id: &SeriesId) -> Result<(), DbError> {
        // series_members は CASCADE で自動削除される
        let res = sqlx::query("DELETE FROM series WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    async fn remove_from_series(
        &self,
        series: &SeriesId,
        file: &FileId,
    ) -> Result<(), DbError> {
        sqlx::query("DELETE FROM series_members WHERE series_id = ? AND file_id = ?")
            .bind(series.to_string())
            .bind(file.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn add_to_series(&self, member: &SeriesMember) -> Result<(), DbError> {
        sqlx::query(
            r#"INSERT INTO series_members (series_id, file_id, order_index)
               VALUES (?, ?, ?)
               ON CONFLICT(series_id, file_id)
                 DO UPDATE SET order_index = excluded.order_index"#,
        )
        .bind(member.series_id.to_string())
        .bind(member.file_id.to_string())
        .bind(member.order_index)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_series_members(
        &self,
        series: &SeriesId,
    ) -> Result<Vec<SeriesMember>, DbError> {
        let rows = sqlx::query(
            r#"SELECT series_id, file_id, order_index
               FROM series_members
               WHERE series_id = ?
               ORDER BY order_index ASC"#,
        )
        .bind(series.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let sid: String = row.try_get("series_id")?;
                let fid: String = row.try_get("file_id")?;
                let idx: f64 = row.try_get("order_index")?;
                Ok(SeriesMember {
                    series_id: SeriesId::from_uuid(parse_uuid(&sid)?),
                    file_id: FileId::from_uuid(parse_uuid(&fid)?),
                    order_index: idx,
                })
            })
            .collect()
    }

    async fn insert_commit(&self, commit: &Commit) -> Result<(), DbError> {
        sqlx::query(
            r#"INSERT INTO commits
               (id, file_id, parent, actor, blob, format_id, timestamp, message,
                committed_by, committed_by_user_id)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(commit.id.to_string())
        .bind(commit.file_id.to_string())
        .bind(commit.parent.map(|c| c.to_string()))
        .bind(commit.actor.to_string())
        .bind(commit.blob.as_str())
        .bind(&commit.format_id)
        .bind(fmt_dt(commit.timestamp))
        .bind(&commit.message)
        .bind(&commit.committed_by)
        .bind(commit.committed_by_user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn upsert_saved_query(
        &self,
        q: &SavedQuery,
    ) -> Result<SavedQueryId, DbError> {
        let body = serde_json::to_string(&q.query)
            .map_err(|e| DbError::Invalid(format!("query json: {e}")))?;
        sqlx::query(
            r#"INSERT INTO saved_queries
               (id, name, query_json, description, created_by, created_at, expires_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 query_json = excluded.query_json,
                 description = excluded.description,
                 expires_at = excluded.expires_at"#,
        )
        .bind(q.id.to_string())
        .bind(&q.name)
        .bind(body)
        .bind(&q.description)
        .bind(q.created_by)
        .bind(fmt_dt(q.created_at))
        .bind(q.expires_at.map(fmt_dt))
        .execute(&self.pool)
        .await?;
        Ok(q.id)
    }

    async fn get_saved_query(
        &self,
        id: &SavedQueryId,
    ) -> Result<Option<SavedQuery>, DbError> {
        let row = sqlx::query(
            "SELECT id, name, query_json, description, created_by, created_at, expires_at
             FROM saved_queries WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_saved_query).transpose()
    }

    async fn get_saved_query_by_name(
        &self,
        name: &str,
    ) -> Result<Option<SavedQuery>, DbError> {
        let row = sqlx::query(
            "SELECT id, name, query_json, description, created_by, created_at, expires_at
             FROM saved_queries WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_saved_query).transpose()
    }

    async fn list_saved_queries(&self) -> Result<Vec<SavedQuery>, DbError> {
        let rows = sqlx::query(
            "SELECT id, name, query_json, description, created_by, created_at, expires_at
             FROM saved_queries
             WHERE expires_at IS NULL OR expires_at > datetime('now')
             ORDER BY name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_saved_query).collect()
    }

    async fn delete_saved_query(&self, id: &SavedQueryId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM saved_queries WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn upsert_fts(
        &self,
        file: &FileId,
        display_name: &str,
        tags: &str,
        content: &str,
    ) -> Result<(), DbError> {
        // FTS5 では UPSERT が無いので DELETE → INSERT
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM files_fts WHERE file_id = ?")
            .bind(file.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO files_fts (file_id, display_name, tags, content)
             VALUES (?, ?, ?, ?)",
        )
        .bind(file.to_string())
        .bind(display_name)
        .bind(tags)
        .bind(content)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_fts(&self, file: &FileId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM files_fts WHERE file_id = ?")
            .bind(file.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn search_fts(&self, query: &str, limit: u32) -> Result<Vec<FileId>, DbError> {
        let rows = sqlx::query(
            "SELECT file_id FROM files_fts
             WHERE files_fts MATCH ?
             ORDER BY rank LIMIT ?",
        )
        .bind(query)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let s: String = r.try_get("file_id")?;
                Ok(FileId::from_uuid(parse_uuid(&s)?))
            })
            .collect()
    }

    async fn list_commits(&self, file: &FileId) -> Result<Vec<Commit>, DbError> {
        let rows = sqlx::query(
            r#"SELECT * FROM commits
               WHERE file_id = ?
               ORDER BY timestamp ASC"#,
        )
        .bind(file.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_commit).collect()
    }
}

fn priority(k: TagKind) -> u8 {
    match k {
        TagKind::System => 1,
        TagKind::Ai => 2,
        TagKind::Manual => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    async fn store() -> SqliteMetaStore {
        SqliteMetaStore::open_in_memory().await.unwrap()
    }

    fn sample_file() -> FileMeta {
        let now = OffsetDateTime::now_utc();
        FileMeta {
            id: FileId::new(),
            display_name: "test.md".into(),
            size: 12,
            mime: Some("text/markdown".into()),
            charset: Some("UTF-8".into()),
            current_commit: None,
            created_at: now,
            updated_at: now,
            deleted: false,
            created_by: Some("tester".into()),
            updated_by: Some("tester".into()),
            created_by_user_id: Some(7),
            updated_by_user_id: Some(7),
        }
    }

    #[tokio::test]
    async fn insert_and_get_file() {
        let s = store().await;
        let f = sample_file();
        s.insert_file(&f).await.unwrap();
        let got = s.get_file(&f.id).await.unwrap().unwrap();
        assert_eq!(got.display_name, "test.md");
        assert_eq!(got.size, 12);
    }

    #[tokio::test]
    async fn list_files_excludes_deleted() {
        let s = store().await;
        let mut a = sample_file();
        let mut b = sample_file();
        b.deleted = true;
        s.insert_file(&a).await.unwrap();
        s.insert_file(&b).await.unwrap();
        let list = s.list_files(100, 0).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, a.id);
        a.display_name = "renamed.md".into();
        a.updated_at = OffsetDateTime::now_utc();
        s.update_file(&a).await.unwrap();
        let after = s.get_file(&a.id).await.unwrap().unwrap();
        assert_eq!(after.display_name, "renamed.md");
    }

    #[tokio::test]
    async fn tag_upgrade_respects_priority() {
        let s = store().await;
        let system_tag = Tag {
            id: TagId::new(),
            name: "report".into(),
            kind: TagKind::System,
            confidence: None,
        };
        let id1 = s.upsert_tag(&system_tag).await.unwrap();

        let manual = Tag {
            id: TagId::new(),
            name: "report".into(),
            kind: TagKind::Manual,
            confidence: None,
        };
        let id2 = s.upsert_tag(&manual).await.unwrap();
        // 同名なので同一 ID が返る。
        assert_eq!(id1, id2);

        let (kind,): (String,) = sqlx::query_as("SELECT kind FROM tags WHERE id = ?")
            .bind(id1.to_string())
            .fetch_one(s.pool())
            .await
            .unwrap();
        assert_eq!(kind, "manual");
    }

    #[tokio::test]
    async fn attach_and_list_by_tags_and_filter() {
        let s = store().await;
        let f1 = sample_file();
        let f2 = sample_file();
        s.insert_file(&f1).await.unwrap();
        s.insert_file(&f2).await.unwrap();
        let work = s
            .upsert_tag(&Tag {
                id: TagId::new(),
                name: "work".into(),
                kind: TagKind::Manual,
                confidence: None,
            })
            .await
            .unwrap();
        let urgent = s
            .upsert_tag(&Tag {
                id: TagId::new(),
                name: "urgent".into(),
                kind: TagKind::Manual,
                confidence: None,
            })
            .await
            .unwrap();
        s.attach_tag(&f1.id, &work).await.unwrap();
        s.attach_tag(&f1.id, &urgent).await.unwrap();
        s.attach_tag(&f2.id, &work).await.unwrap();

        let both = s.list_files_by_tags(&[work, urgent]).await.unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].id, f1.id);

        let just_work = s.list_files_by_tags(&[work]).await.unwrap();
        assert_eq!(just_work.len(), 2);

        s.detach_tag(&f1.id, &urgent).await.unwrap();
        let after = s.list_files_by_tags(&[work, urgent]).await.unwrap();
        assert!(after.is_empty());
    }

    #[tokio::test]
    async fn series_ordered_members() {
        let s = store().await;
        let series = Series {
            id: SeriesId::new(),
            name: "manual".into(),
            description: None,
        };
        s.upsert_series(&series).await.unwrap();

        let mut files = vec![];
        for (i, idx) in [10.0_f64, 30.0, 20.0].iter().enumerate() {
            let mut f = sample_file();
            f.display_name = format!("chapter-{i}.md");
            s.insert_file(&f).await.unwrap();
            s.add_to_series(&SeriesMember {
                series_id: series.id,
                file_id: f.id,
                order_index: *idx,
            })
            .await
            .unwrap();
            files.push(f);
        }
        let members = s.list_series_members(&series.id).await.unwrap();
        assert_eq!(members.len(), 3);
        assert_eq!(members[0].file_id, files[0].id); // 10
        assert_eq!(members[1].file_id, files[2].id); // 20
        assert_eq!(members[2].file_id, files[1].id); // 30
    }

    #[tokio::test]
    async fn commits_logged_in_order() {
        let s = store().await;
        let file = sample_file();
        s.insert_file(&file).await.unwrap();
        let actor = ActorId::new();
        let c1 = Commit {
            id: CommitId::new(),
            file_id: file.id,
            parent: None,
            actor,
            blob: BlobId::from_hex("aa"),
            format_id: "text/plain".into(),
            timestamp: OffsetDateTime::now_utc(),
            message: Some("init".into()),
            committed_by: None,
            committed_by_user_id: None,
        };
        let c2 = Commit {
            id: CommitId::new(),
            file_id: file.id,
            parent: Some(c1.id),
            actor,
            blob: BlobId::from_hex("bb"),
            format_id: "text/plain".into(),
            timestamp: OffsetDateTime::now_utc() + time::Duration::seconds(1),
            message: Some("edit".into()),
            committed_by: Some("alice".into()),
            committed_by_user_id: Some(42),
        };
        s.insert_commit(&c1).await.unwrap();
        s.insert_commit(&c2).await.unwrap();
        let log = s.list_commits(&file.id).await.unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].id, c1.id);
        assert_eq!(log[1].id, c2.id);
        // committed_by / committed_by_user_id が往復で保持される（NULL/値の両方）
        assert_eq!(log[0].committed_by, None);
        assert_eq!(log[0].committed_by_user_id, None);
        assert_eq!(log[1].committed_by.as_deref(), Some("alice"));
        assert_eq!(log[1].committed_by_user_id, Some(42));
    }

    #[tokio::test]
    async fn open_creates_db_and_runs_migrations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let store = SqliteMetaStore::open(&path).await.unwrap();
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(row.0, 0);
    }

    /// upsert_saved_query は同一 id での更新（改名・条件変更）を許す。
    /// （ON CONFLICT(id) になっていないと id の PK 衝突で失敗する回帰）
    #[tokio::test]
    async fn upsert_saved_query_updates_in_place_and_renames() {
        let s = store().await;
        let q = SavedQuery {
            id: SavedQueryId::new(),
            name: "仕事メモ".into(),
            query: QueryDef {
                tags_and: vec!["仕事".into()],
                tags_not: vec!["下書き".into()],
                ..Default::default()
            },
            description: Some("初版".into()),
            created_by: None,
            created_at: OffsetDateTime::now_utc(),
            expires_at: None,
        };
        s.upsert_saved_query(&q).await.unwrap();

        // 同一 id で改名 + 条件変更 + 説明クリア。
        let updated = SavedQuery {
            name: "重要メモ".into(),
            query: QueryDef {
                tags_and: vec!["仕事".into()],
                tags_not: vec![],
                ..Default::default()
            },
            description: None,
            ..q.clone()
        };
        s.upsert_saved_query(&updated).await.unwrap();

        // 行は増えず（1 件）、内容が更新されている。
        let all = s.list_saved_queries().await.unwrap();
        assert_eq!(all.len(), 1, "更新で行が増えた");
        let got = s.get_saved_query(&q.id).await.unwrap().unwrap();
        assert_eq!(got.name, "重要メモ");
        assert_eq!(got.query.tags_and, vec!["仕事".to_string()]);
        assert!(got.query.tags_not.is_empty());
        assert_eq!(got.description, None);
        // 旧名では引けない。
        assert!(s.get_saved_query_by_name("仕事メモ").await.unwrap().is_none());
    }
}
