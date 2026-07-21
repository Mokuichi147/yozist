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
    ActorId, BlobId, Commit, CommitId, FileId, FileMeta, FilterDef, Filter, FilterId,
    Series, SeriesId, SeriesMember, SeriesSort, Tag, TagId, TagKind,
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
    let deleted_at: Option<String> = row.try_get("deleted_at")?;
    let created_by: Option<String> = row.try_get("created_by")?;
    let updated_by: Option<String> = row.try_get("updated_by")?;
    let created_by_user_id: Option<String> = row.try_get("created_by_user_id")?;
    let updated_by_user_id: Option<String> = row.try_get("updated_by_user_id")?;
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
        deleted_at: deleted_at.map(|s| parse_dt(&s)).transpose()?,
        created_by,
        updated_by,
        created_by_user_id: created_by_user_id.map(|s| parse_uuid(&s)).transpose()?,
        updated_by_user_id: updated_by_user_id.map(|s| parse_uuid(&s)).transpose()?,
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
    let sort_order: String = row.try_get("sort_order")?;
    Ok(Series {
        id: SeriesId::from_uuid(parse_uuid(&id)?),
        name,
        description,
        sort_order: SeriesSort::from_str_lenient(&sort_order),
    })
}

fn row_to_filter(row: SqliteRow) -> Result<Filter, DbError> {
    let id: String = row.try_get("id")?;
    let name: String = row.try_get("name")?;
    let definition_json: String = row.try_get("definition_json")?;
    let description: Option<String> = row.try_get("description")?;
    let created_by: Option<String> = row.try_get("created_by")?;
    let created_at: String = row.try_get("created_at")?;
    let expires_at: Option<String> = row.try_get("expires_at")?;

    let definition: FilterDef = serde_json::from_str(&definition_json)
        .map_err(|e| DbError::Invalid(format!("filter definition json: {e}")))?;
    Ok(Filter {
        id: FilterId::from_uuid(parse_uuid(&id)?),
        name,
        definition,
        description,
        created_by: created_by.map(|s| parse_uuid(&s)).transpose()?,
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
    let size: i64 = row.try_get("size")?;
    let committed_by: Option<String> = row.try_get("committed_by")?;
    let committed_by_user_id: Option<String> = row.try_get("committed_by_user_id")?;
    let delta_base: Option<String> = row.try_get("delta_base")?;
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
        size: size.max(0) as u64,
        committed_by,
        committed_by_user_id: committed_by_user_id.map(|s| parse_uuid(&s)).transpose()?,
        delta_base: delta_base
            .map(|s| parse_uuid(&s).map(CommitId::from_uuid))
            .transpose()?,
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
                created_at, updated_at, deleted, deleted_at, created_by, updated_by,
                created_by_user_id, updated_by_user_id, version)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)"#,
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
        .bind(meta.deleted_at.map(fmt_dt))
        .bind(&meta.created_by)
        .bind(&meta.updated_by)
        .bind(meta.created_by_user_id.map(|u| u.to_string()))
        .bind(meta.updated_by_user_id.map(|u| u.to_string()))
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

    async fn get_files(&self, ids: &[FileId]) -> Result<Vec<FileMeta>, DbError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // SQLite のバインド変数上限（既定 32766）に収まるよう分割して問い合わせる。
        const CHUNK: usize = 500;
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(CHUNK) {
            let mut qb = sqlx::QueryBuilder::new("SELECT * FROM files WHERE id IN (");
            {
                let mut sep = qb.separated(", ");
                for id in chunk {
                    sep.push_bind(id.to_string());
                }
            }
            qb.push(")");
            for row in qb.build().fetch_all(&self.pool).await? {
                out.push(row_to_file(row)?);
            }
        }
        Ok(out)
    }

    async fn list_files_after(
        &self,
        after: Option<&FileId>,
        limit: u32,
    ) -> Result<Vec<FileMeta>, DbError> {
        let rows = match after {
            Some(after) => {
                sqlx::query(
                    "SELECT * FROM files WHERE deleted = 0 AND id > ? ORDER BY id LIMIT ?",
                )
                .bind(after.to_string())
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query("SELECT * FROM files WHERE deleted = 0 ORDER BY id LIMIT ?")
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        rows.into_iter().map(row_to_file).collect()
    }

    async fn update_file(&self, meta: &FileMeta) -> Result<(), DbError> {
        // 楽観ロック: 現行 version を取得し、+1 で更新。
        let res = sqlx::query(
            r#"UPDATE files SET
                 display_name = ?, size = ?, mime = ?, charset = ?,
                 current_commit = ?, updated_at = ?, deleted = ?, deleted_at = ?,
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
        .bind(meta.deleted_at.map(fmt_dt))
        .bind(&meta.created_by)
        .bind(&meta.updated_by)
        .bind(meta.created_by_user_id.map(|u| u.to_string()))
        .bind(meta.updated_by_user_id.map(|u| u.to_string()))
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

    async fn list_deleted_files(&self, limit: u32, offset: u32) -> Result<Vec<FileMeta>, DbError> {
        // ゴミ箱: 論理削除済みを削除日時の新しい順で。削除時刻不明の旧データは末尾へ。
        let rows = sqlx::query(
            "SELECT * FROM files WHERE deleted = 1 \
             ORDER BY deleted_at IS NULL, deleted_at DESC, updated_at DESC \
             LIMIT ? OFFSET ?",
        )
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_file).collect()
    }

    async fn purge_file(&self, id: &FileId) -> Result<(), DbError> {
        // files 行を物理削除。commits / file_tags / series_members / blob_refs は
        // ON DELETE CASCADE で同時に消える（foreign_keys=ON 前提）。
        // 消えるコミットの blob は同一トランザクションで削除候補（blob_orphans）へ
        // 登録し、スイーパが参照残無しを確認してから実体を回収する。
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"INSERT INTO blob_orphans (blob_id, orphaned_at)
               SELECT DISTINCT blob, ? FROM commits WHERE file_id = ?
               ON CONFLICT(blob_id) DO NOTHING"#,
        )
        .bind(fmt_dt(time::OffsetDateTime::now_utc()))
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        let res = sqlx::query("DELETE FROM files WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        tx.commit().await?;
        Ok(())
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

    async fn list_tags_with_counts(&self) -> Result<Vec<(Tag, u64)>, DbError> {
        let rows = sqlx::query(
            r#"SELECT t.id, t.name, t.kind, t.confidence, COUNT(ft.file_id) AS cnt
               FROM tags t
               LEFT JOIN file_tags ft ON ft.tag_id = t.id
               GROUP BY t.id, t.name, t.kind, t.confidence
               ORDER BY t.name ASC"#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let cnt: i64 = row.try_get("cnt")?;
                let tag = row_to_tag(row)?;
                Ok((tag, cnt.max(0) as u64))
            })
            .collect()
    }

    async fn merge_tags(&self, source: &TagId, target: &TagId) -> Result<(), DbError> {
        if source == target {
            return Err(DbError::Invalid("source と target が同一です".into()));
        }
        let source_s = source.to_string();
        let target_s = target.to_string();

        let mut tx = self.pool.begin().await?;

        // source / target の存在確認。source が無ければ NotFound、target が無ければ Invalid。
        let source_exists: Option<String> = sqlx::query_scalar("SELECT id FROM tags WHERE id = ?")
            .bind(&source_s)
            .fetch_optional(&mut *tx)
            .await?;
        if source_exists.is_none() {
            return Err(DbError::NotFound);
        }
        let target_exists: Option<String> = sqlx::query_scalar("SELECT id FROM tags WHERE id = ?")
            .bind(&target_s)
            .fetch_optional(&mut *tx)
            .await?;
        if target_exists.is_none() {
            return Err(DbError::Invalid("合流先のタグが存在しません".into()));
        }

        // source を付けていたファイルを target に付け替え（既に target 付きなら無視）。
        sqlx::query(
            r#"INSERT OR IGNORE INTO file_tags (file_id, tag_id)
               SELECT file_id, ? FROM file_tags WHERE tag_id = ?"#,
        )
        .bind(&target_s)
        .bind(&source_s)
        .execute(&mut *tx)
        .await?;

        // source タグを削除（残った source の file_tags は CASCADE で消える）。
        sqlx::query("DELETE FROM tags WHERE id = ?")
            .bind(&source_s)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
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
        sqlx::query("INSERT INTO series (id, name, description, sort_order) VALUES (?, ?, ?, ?)")
            .bind(series.id.to_string())
            .bind(&series.name)
            .bind(&series.description)
            .bind(series.sort_order.as_str())
            .execute(&self.pool)
            .await?;
        Ok(series.id)
    }

    async fn get_series(&self, id: &SeriesId) -> Result<Option<Series>, DbError> {
        let row = sqlx::query("SELECT id, name, description, sort_order FROM series WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_series).transpose()
    }

    async fn list_series(&self) -> Result<Vec<Series>, DbError> {
        let rows =
            sqlx::query("SELECT id, name, description, sort_order FROM series ORDER BY name ASC")
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

    async fn set_series_sort(&self, id: &SeriesId, sort: SeriesSort) -> Result<(), DbError> {
        let res = sqlx::query("UPDATE series SET sort_order = ? WHERE id = ?")
            .bind(sort.as_str())
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

    async fn list_series_of_file(&self, file: &FileId) -> Result<Vec<Series>, DbError> {
        let rows = sqlx::query(
            r#"SELECT s.id, s.name, s.description, s.sort_order
               FROM series s
               JOIN series_members m ON m.series_id = s.id
               WHERE m.file_id = ?
               ORDER BY s.name COLLATE NOCASE ASC"#,
        )
        .bind(file.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_series).collect()
    }

    async fn list_series_members_named(
        &self,
        series: &SeriesId,
    ) -> Result<Vec<(FileId, String)>, DbError> {
        // 並び順はシリーズの sort_order 設定に従う。ORDER BY 句は固定の列・方向に
        // しか展開しない（ユーザー入力を SQL に埋め込まない）ため安全。
        let sort = match sqlx::query_as::<_, (String,)>("SELECT sort_order FROM series WHERE id = ?")
            .bind(series.to_string())
            .fetch_optional(&self.pool)
            .await?
        {
            Some((s,)) => SeriesSort::from_str_lenient(&s),
            None => SeriesSort::default(),
        };
        let order_by = match sort {
            SeriesSort::CreatedAsc => "f.created_at ASC, m.order_index ASC",
            SeriesSort::CreatedDesc => "f.created_at DESC, m.order_index ASC",
            SeriesSort::NameAsc => "f.display_name COLLATE NOCASE ASC",
            SeriesSort::NameDesc => "f.display_name COLLATE NOCASE DESC",
            SeriesSort::Manual => "m.order_index ASC",
        };
        let sql = format!(
            r#"SELECT m.file_id, f.display_name
               FROM series_members m
               JOIN files f ON f.id = m.file_id
               WHERE m.series_id = ? AND f.deleted = 0
               ORDER BY {order_by}"#,
        );
        let rows = sqlx::query(&sql)
            .bind(series.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let fid: String = row.try_get("file_id")?;
                let name: String = row.try_get("display_name")?;
                Ok((FileId::from_uuid(parse_uuid(&fid)?), name))
            })
            .collect()
    }

    async fn insert_commit(&self, commit: &Commit) -> Result<(), DbError> {
        sqlx::query(
            r#"INSERT INTO commits
               (id, file_id, parent, actor, blob, format_id, timestamp, message,
                size, committed_by, committed_by_user_id, delta_base)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(commit.id.to_string())
        .bind(commit.file_id.to_string())
        .bind(commit.parent.map(|c| c.to_string()))
        .bind(commit.actor.to_string())
        .bind(commit.blob.as_str())
        .bind(&commit.format_id)
        .bind(fmt_dt(commit.timestamp))
        .bind(&commit.message)
        .bind(commit.size as i64)
        .bind(&commit.committed_by)
        .bind(commit.committed_by_user_id.map(|u| u.to_string()))
        .bind(commit.delta_base.map(|c| c.to_string()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn upsert_filter(
        &self,
        q: &Filter,
    ) -> Result<FilterId, DbError> {
        let body = serde_json::to_string(&q.definition)
            .map_err(|e| DbError::Invalid(format!("filter definition json: {e}")))?;
        sqlx::query(
            r#"INSERT INTO filters
               (id, name, definition_json, description, created_by, created_at, expires_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 definition_json = excluded.definition_json,
                 description = excluded.description,
                 expires_at = excluded.expires_at"#,
        )
        .bind(q.id.to_string())
        .bind(&q.name)
        .bind(body)
        .bind(&q.description)
        .bind(q.created_by.map(|u| u.to_string()))
        .bind(fmt_dt(q.created_at))
        .bind(q.expires_at.map(fmt_dt))
        .execute(&self.pool)
        .await?;
        Ok(q.id)
    }

    async fn get_filter(
        &self,
        id: &FilterId,
    ) -> Result<Option<Filter>, DbError> {
        let row = sqlx::query(
            "SELECT id, name, definition_json, description, created_by, created_at, expires_at
             FROM filters WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_filter).transpose()
    }

    async fn get_filter_by_name(
        &self,
        name: &str,
    ) -> Result<Option<Filter>, DbError> {
        let row = sqlx::query(
            "SELECT id, name, definition_json, description, created_by, created_at, expires_at
             FROM filters WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_filter).transpose()
    }

    async fn list_filters(&self) -> Result<Vec<Filter>, DbError> {
        let rows = sqlx::query(
            "SELECT id, name, definition_json, description, created_by, created_at, expires_at
             FROM filters
             WHERE expires_at IS NULL OR expires_at > datetime('now')
             ORDER BY name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_filter).collect()
    }

    async fn delete_filter(&self, id: &FilterId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM filters WHERE id = ?")
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
        // 入力は FTS5 クエリ構文としてではなく「空白区切りの語句の AND」として扱う。
        // 生のまま MATCH に渡すと日本語や記号（"C++" 等）で構文エラーになるため、
        // 各語をフレーズ文字列として引用する。
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // trigram トークナイザは 3 文字未満の語句に一致を返せないため、
        // 短い語を含む場合は LIKE 走査へフォールバックする（bm25 は失うが
        // 「東京」「メモ」のような 2 文字の日本語検索を取りこぼさない）。
        let rows = if terms.iter().all(|t| t.chars().count() >= 3) {
            let match_query = terms
                .iter()
                .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" ");
            // bm25 の列重み: ファイル名・タグでの一致を本文一致より上位に出す
            // （引数は列定義順。UNINDEXED の file_id は 0）。値が小さいほど上位。
            sqlx::query(
                "SELECT file_id FROM files_fts
                 WHERE files_fts MATCH ?
                 ORDER BY bm25(files_fts, 0.0, 8.0, 4.0, 1.0) LIMIT ?",
            )
            .bind(match_query)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            // MATCH 経路と同じ優先度（ファイル名 > タグ > 本文）を
            // 語ごとの CASE スコアの合計で近似する。
            let mut sql = String::from("SELECT file_id FROM files_fts WHERE 1=1");
            for _ in &terms {
                sql.push_str(
                    " AND (display_name LIKE ? ESCAPE '\\'
                       OR tags LIKE ? ESCAPE '\\'
                       OR content LIKE ? ESCAPE '\\')",
                );
            }
            sql.push_str(" ORDER BY ");
            for (i, _) in terms.iter().enumerate() {
                if i > 0 {
                    sql.push_str(" + ");
                }
                sql.push_str(
                    "(CASE WHEN display_name LIKE ? ESCAPE '\\' THEN 8
                           WHEN tags LIKE ? ESCAPE '\\' THEN 4
                           ELSE 1 END)",
                );
            }
            sql.push_str(" DESC LIMIT ?");
            let patterns: Vec<String> = terms
                .iter()
                .map(|t| {
                    format!(
                        "%{}%",
                        t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
                    )
                })
                .collect();
            let mut q = sqlx::query(&sql);
            // バインドは SQL 中の ? の出現順: WHERE（語ごとに 3 つ）→ ORDER BY（語ごとに 2 つ）。
            for p in &patterns {
                q = q.bind(p.clone()).bind(p.clone()).bind(p.clone());
            }
            for p in &patterns {
                q = q.bind(p.clone()).bind(p.clone());
            }
            q.bind(limit as i64).fetch_all(&self.pool).await?
        };
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

    async fn update_commit_storage(
        &self,
        commit: &CommitId,
        blob: &BlobId,
        delta_base: Option<CommitId>,
    ) -> Result<(), DbError> {
        let res = sqlx::query("UPDATE commits SET blob = ?, delta_base = ? WHERE id = ?")
            .bind(blob.as_str())
            .bind(delta_base.map(|c| c.to_string()))
            .bind(commit.to_string())
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    async fn count_commits_referencing_blob(&self, blob: &BlobId) -> Result<u64, DbError> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM commits WHERE blob = ?")
            .bind(blob.as_str())
            .fetch_one(&self.pool)
            .await?;
        Ok(n.max(0) as u64)
    }

    async fn insert_blob_orphan(
        &self,
        blob: &BlobId,
        at: time::OffsetDateTime,
    ) -> Result<(), DbError> {
        sqlx::query(
            r#"INSERT INTO blob_orphans (blob_id, orphaned_at) VALUES (?, ?)
               ON CONFLICT(blob_id) DO NOTHING"#,
        )
        .bind(blob.as_str())
        .bind(fmt_dt(at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_blob_orphans(
        &self,
        before: time::OffsetDateTime,
    ) -> Result<Vec<BlobId>, DbError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT blob_id FROM blob_orphans WHERE orphaned_at < ? ORDER BY orphaned_at ASC",
        )
        .bind(fmt_dt(before))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(s,)| BlobId::from_hex(s)).collect())
    }

    async fn remove_blob_orphan(&self, blob: &BlobId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM blob_orphans WHERE blob_id = ?")
            .bind(blob.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
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
            deleted_at: None,
            created_by: Some("tester".into()),
            updated_by: Some("tester".into()),
            created_by_user_id: Some(Uuid::now_v7()),
            updated_by_user_id: Some(Uuid::now_v7()),
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

    /// バッチ取得は `get_file` と同じ意味論（論理削除済みも返し、存在しない ID は
    /// 落ちる）。N+1 を避けるバッチ処理がここに依存する。
    #[tokio::test]
    async fn get_files_returns_known_ids_including_deleted() {
        let s = store().await;
        let alive = sample_file();
        let mut removed = sample_file();
        removed.deleted = true;
        s.insert_file(&alive).await.unwrap();
        s.insert_file(&removed).await.unwrap();
        let unknown = FileId::new();

        let got = s
            .get_files(&[alive.id, removed.id, unknown])
            .await
            .unwrap();
        let ids: std::collections::HashSet<_> = got.iter().map(|f| f.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&alive.id));
        assert!(ids.contains(&removed.id), "論理削除済みも返す");
        assert!(!ids.contains(&unknown), "存在しない ID は結果から落ちる");

        assert!(s.get_files(&[]).await.unwrap().is_empty());
    }

    /// キーセットページングは走査中に他の行が更新されても取りこぼさない。
    /// `list_files` の OFFSET + updated_at 順ではここで行が移動して漏れる。
    #[tokio::test]
    async fn list_files_after_paginates_stably_under_updates() {
        let s = store().await;
        let mut files = Vec::new();
        for _ in 0..5 {
            let f = sample_file();
            s.insert_file(&f).await.unwrap();
            files.push(f);
        }
        files.sort_by_key(|f| f.id.to_string());

        let first = s.list_files_after(None, 2).await.unwrap();
        assert_eq!(
            first.iter().map(|f| f.id).collect::<Vec<_>>(),
            files[..2].iter().map(|f| f.id).collect::<Vec<_>>()
        );

        // 走査の途中で、まだ読んでいない行を更新する（updated_at が動く）。
        let mut touched = files[4].clone();
        touched.display_name = "touched.md".into();
        touched.updated_at = OffsetDateTime::now_utc();
        s.update_file(&touched).await.unwrap();

        let rest = s.list_files_after(Some(&first[1].id), 100).await.unwrap();
        assert_eq!(
            rest.iter().map(|f| f.id).collect::<Vec<_>>(),
            files[2..].iter().map(|f| f.id).collect::<Vec<_>>(),
            "更新が挟まっても残りを取りこぼさない"
        );
    }

    #[tokio::test]
    async fn list_files_after_excludes_deleted() {
        let s = store().await;
        let alive = sample_file();
        let mut removed = sample_file();
        removed.deleted = true;
        s.insert_file(&alive).await.unwrap();
        s.insert_file(&removed).await.unwrap();

        let got = s.list_files_after(None, 100).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, alive.id);
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
    async fn trash_lists_deleted_and_preserves_deleted_at() {
        let s = store().await;
        let live = sample_file();
        let mut gone = sample_file();
        let deleted_at = OffsetDateTime::now_utc();
        gone.deleted = true;
        gone.deleted_at = Some(deleted_at);
        s.insert_file(&live).await.unwrap();
        s.insert_file(&gone).await.unwrap();

        // ゴミ箱には削除済みのみが並ぶ
        let trash = s.list_deleted_files(100, 0).await.unwrap();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].id, gone.id);
        // deleted_at が往復で保持される（秒精度）
        assert_eq!(
            trash[0].deleted_at.map(|d| d.unix_timestamp()),
            Some(deleted_at.unix_timestamp()),
        );

        // 復元すると一覧から外れ deleted_at も消える
        let mut restored = trash.into_iter().next().unwrap();
        restored.deleted = false;
        restored.deleted_at = None;
        s.update_file(&restored).await.unwrap();
        assert!(s.list_deleted_files(100, 0).await.unwrap().is_empty());
        let back = s.get_file(&restored.id).await.unwrap().unwrap();
        assert!(!back.deleted);
        assert!(back.deleted_at.is_none());
    }

    #[tokio::test]
    async fn purge_file_removes_row_and_cascades() {
        let s = store().await;
        let file = sample_file();
        s.insert_file(&file).await.unwrap();
        // タグとコミットを関連付け、CASCADE で消えることを確認する
        let tag_id = s
            .upsert_tag(&Tag {
                id: TagId::new(),
                name: "keep".into(),
                kind: TagKind::Manual,
                confidence: None,
            })
            .await
            .unwrap();
        s.attach_tag(&file.id, &tag_id).await.unwrap();
        let commit = Commit {
            id: CommitId::new(),
            file_id: file.id,
            parent: None,
            actor: ActorId::new(),
            blob: BlobId::from_hex("aa"),
            format_id: "text/plain".into(),
            timestamp: OffsetDateTime::now_utc(),
            message: Some("init".into()),
            size: 0,
            committed_by: None,
            committed_by_user_id: None,
            delta_base: None,
        };
        s.insert_commit(&commit).await.unwrap();

        // 物理削除: ファイル行が消え、関連も CASCADE で消える
        s.purge_file(&file.id).await.unwrap();
        assert!(s.get_file(&file.id).await.unwrap().is_none());
        assert!(s.list_commits(&file.id).await.unwrap().is_empty());
        assert!(s.list_tags_of(&file.id).await.unwrap().is_empty());
        // タグ定義自体は残る（共有されうるため）
        assert!(s.list_tags().await.unwrap().iter().any(|t| t.id == tag_id));
        // 存在しないファイルの purge は NotFound
        assert!(matches!(
            s.purge_file(&file.id).await,
            Err(DbError::NotFound)
        ));
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
    async fn merge_tags_repoints_files_and_removes_source() {
        let s = store().await;
        let f1 = sample_file();
        let f2 = sample_file();
        let f3 = sample_file();
        s.insert_file(&f1).await.unwrap();
        s.insert_file(&f2).await.unwrap();
        s.insert_file(&f3).await.unwrap();
        let mk = |name: &str| Tag {
            id: TagId::new(),
            name: name.into(),
            kind: TagKind::Manual,
            confidence: None,
        };
        let keep = s.upsert_tag(&mk("仕事")).await.unwrap();
        let dup = s.upsert_tag(&mk("work")).await.unwrap();
        // f1: 両方 / f2: dup のみ / f3: keep のみ
        s.attach_tag(&f1.id, &keep).await.unwrap();
        s.attach_tag(&f1.id, &dup).await.unwrap();
        s.attach_tag(&f2.id, &dup).await.unwrap();
        s.attach_tag(&f3.id, &keep).await.unwrap();

        s.merge_tags(&dup, &keep).await.unwrap();

        // source タグは消えている
        assert!(s.get_tag(&dup).await.unwrap().is_none());
        // keep は f1/f2/f3 すべてに付く（f1 の重複は 1 件に集約）
        let files = s.list_files_by_tags(&[keep]).await.unwrap();
        assert_eq!(files.len(), 3);
        // 件数も 3
        let stats = s.list_tags_with_counts().await.unwrap();
        let (_, cnt) = stats.iter().find(|(t, _)| t.id == keep).unwrap();
        assert_eq!(*cnt, 3);
    }

    #[tokio::test]
    async fn merge_tags_validates_ids() {
        let s = store().await;
        let only = s
            .upsert_tag(&Tag {
                id: TagId::new(),
                name: "only".into(),
                kind: TagKind::Manual,
                confidence: None,
            })
            .await
            .unwrap();
        // 同一 ID は Invalid
        assert!(matches!(
            s.merge_tags(&only, &only).await,
            Err(DbError::Invalid(_))
        ));
        // source 不在は NotFound
        let missing = TagId::new();
        assert!(matches!(
            s.merge_tags(&missing, &only).await,
            Err(DbError::NotFound)
        ));
        // target 不在は Invalid
        assert!(matches!(
            s.merge_tags(&only, &missing).await,
            Err(DbError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn series_ordered_members() {
        let s = store().await;
        let series = Series {
            id: SeriesId::new(),
            name: "manual".into(),
            description: None,
            sort_order: SeriesSort::Manual,
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
    async fn series_of_file_and_named_members() {
        let s = store().await;
        let series = Series {
            id: SeriesId::new(),
            name: "saga".into(),
            description: None,
            sort_order: SeriesSort::Manual,
        };
        s.upsert_series(&series).await.unwrap();

        // 順序 30 → 10 → 20 で追加し、order_index 昇順に並ぶことを確認する。
        let mut files = vec![];
        for (i, idx) in [30.0_f64, 10.0, 20.0].iter().enumerate() {
            let mut f = sample_file();
            f.display_name = format!("part-{i}.md");
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

        // 所属シリーズの逆引き。
        let of = s.list_series_of_file(&files[1].id).await.unwrap();
        assert_eq!(of.len(), 1);
        assert_eq!(of[0].id, series.id);

        // 表示名付き・順序付きメンバー。
        let named = s.list_series_members_named(&series.id).await.unwrap();
        assert_eq!(named.len(), 3);
        assert_eq!(named[0].0, files[1].id); // idx 10 → part-1
        assert_eq!(named[0].1, "part-1.md");
        assert_eq!(named[1].0, files[2].id); // idx 20 → part-2
        assert_eq!(named[2].0, files[0].id); // idx 30 → part-0

        // 削除済みファイルは除外される。
        let mut gone = files[2].clone();
        gone.deleted = true;
        s.update_file(&gone).await.unwrap();
        let named = s.list_series_members_named(&series.id).await.unwrap();
        assert_eq!(named.len(), 2);
        assert!(named.iter().all(|(fid, _)| *fid != gone.id));

        // どのシリーズにも属さないファイルは空。
        let lonely = sample_file();
        s.insert_file(&lonely).await.unwrap();
        assert!(s.list_series_of_file(&lonely.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn series_named_members_honor_sort_order() {
        let s = store().await;
        let series = Series {
            id: SeriesId::new(),
            name: "並び替え".into(),
            description: None,
            sort_order: SeriesSort::default(), // 既定は登録日時の昇順
        };
        s.upsert_series(&series).await.unwrap();

        // created_at と名前を意図的に逆相関させ、order_index も別の順序にする。
        // f0: 古い / "charlie" / idx 30
        // f1: 中間 / "bravo"   / idx 10
        // f2: 新しい/ "alpha"   / idx 20
        let base = OffsetDateTime::now_utc();
        let specs = [
            ("charlie.md", base - time::Duration::hours(2), 30.0_f64),
            ("bravo.md", base - time::Duration::hours(1), 10.0),
            ("alpha.md", base, 20.0),
        ];
        let mut ids = vec![];
        for (name, created, idx) in specs {
            let mut f = sample_file();
            f.display_name = name.into();
            f.created_at = created;
            s.insert_file(&f).await.unwrap();
            s.add_to_series(&SeriesMember {
                series_id: series.id,
                file_id: f.id,
                order_index: idx,
            })
            .await
            .unwrap();
            ids.push(f.id);
        }
        let names = |v: Vec<(FileId, String)>| v.into_iter().map(|(_, n)| n).collect::<Vec<_>>();

        // 既定（登録日時の昇順）: charlie, bravo, alpha
        let got = s.list_series_members_named(&series.id).await.unwrap();
        assert_eq!(names(got), vec!["charlie.md", "bravo.md", "alpha.md"]);

        // 登録日時の降順
        s.set_series_sort(&series.id, SeriesSort::CreatedDesc).await.unwrap();
        let got = s.list_series_members_named(&series.id).await.unwrap();
        assert_eq!(names(got), vec!["alpha.md", "bravo.md", "charlie.md"]);

        // 名前の昇順
        s.set_series_sort(&series.id, SeriesSort::NameAsc).await.unwrap();
        let got = s.list_series_members_named(&series.id).await.unwrap();
        assert_eq!(names(got), vec!["alpha.md", "bravo.md", "charlie.md"]);

        // 名前の降順
        s.set_series_sort(&series.id, SeriesSort::NameDesc).await.unwrap();
        let got = s.list_series_members_named(&series.id).await.unwrap();
        assert_eq!(names(got), vec!["charlie.md", "bravo.md", "alpha.md"]);

        // 手動順（order_index 昇順）: bravo(10), alpha(20), charlie(30)
        s.set_series_sort(&series.id, SeriesSort::Manual).await.unwrap();
        let got = s.list_series_members_named(&series.id).await.unwrap();
        assert_eq!(names(got), vec!["bravo.md", "alpha.md", "charlie.md"]);

        // set_series_sort は get_series にも反映される。
        let reloaded = s.get_series(&series.id).await.unwrap().unwrap();
        assert_eq!(reloaded.sort_order, SeriesSort::Manual);
    }

    #[tokio::test]
    async fn commits_logged_in_order() {
        let s = store().await;
        let file = sample_file();
        s.insert_file(&file).await.unwrap();
        let actor = ActorId::new();
        let alice_id = Uuid::now_v7();
        let c1 = Commit {
            id: CommitId::new(),
            file_id: file.id,
            parent: None,
            actor,
            blob: BlobId::from_hex("aa"),
            format_id: "text/plain".into(),
            timestamp: OffsetDateTime::now_utc(),
            message: Some("init".into()),
            size: 100,
            committed_by: None,
            committed_by_user_id: None,
            delta_base: None,
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
            size: 250,
            committed_by: Some("alice".into()),
            committed_by_user_id: Some(alice_id),
            delta_base: Some(c1.id),
        };
        s.insert_commit(&c1).await.unwrap();
        s.insert_commit(&c2).await.unwrap();
        let log = s.list_commits(&file.id).await.unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].id, c1.id);
        assert_eq!(log[1].id, c2.id);
        // size が往復で保持される
        assert_eq!(log[0].size, 100);
        assert_eq!(log[1].size, 250);
        // committed_by / committed_by_user_id が往復で保持される（NULL/値の両方）
        assert_eq!(log[0].committed_by, None);
        assert_eq!(log[0].committed_by_user_id, None);
        assert_eq!(log[1].committed_by.as_deref(), Some("alice"));
        assert_eq!(log[1].committed_by_user_id, Some(alice_id));
        // delta_base が往復で保持される（NULL/値の両方）
        assert_eq!(log[0].delta_base, None);
        assert_eq!(log[1].delta_base, Some(c1.id));
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

    /// upsert_filter は同一 id での更新（改名・条件変更）を許す。
    /// （ON CONFLICT(id) になっていないと id の PK 衝突で失敗する回帰）
    #[tokio::test]
    async fn upsert_filter_updates_in_place_and_renames() {
        let s = store().await;
        let q = Filter {
            id: FilterId::new(),
            name: "仕事メモ".into(),
            definition: FilterDef {
                tags_and: vec!["仕事".into()],
                tags_not: vec!["下書き".into()],
                ..Default::default()
            },
            description: Some("初版".into()),
            created_by: None,
            created_at: OffsetDateTime::now_utc(),
            expires_at: None,
        };
        s.upsert_filter(&q).await.unwrap();

        // 同一 id で改名 + 条件変更 + 説明クリア。
        let updated = Filter {
            name: "重要メモ".into(),
            definition: FilterDef {
                tags_and: vec!["仕事".into()],
                tags_not: vec![],
                ..Default::default()
            },
            description: None,
            ..q.clone()
        };
        s.upsert_filter(&updated).await.unwrap();

        // 行は増えず（1 件）、内容が更新されている。
        let all = s.list_filters().await.unwrap();
        assert_eq!(all.len(), 1, "更新で行が増えた");
        let got = s.get_filter(&q.id).await.unwrap().unwrap();
        assert_eq!(got.name, "重要メモ");
        assert_eq!(got.definition.tags_and, vec!["仕事".to_string()]);
        assert!(got.definition.tags_not.is_empty());
        assert_eq!(got.description, None);
        // 旧名では引けない。
        assert!(s.get_filter_by_name("仕事メモ").await.unwrap().is_none());
    }

    /// 日本語本文の部分一致検索（unicode61 では文全体が 1 トークンになり
    /// 常に 0 件だった回帰）。trigram + 短語 LIKE フォールバックで一致する。
    #[tokio::test]
    async fn search_fts_matches_japanese_substrings() {
        let s = store().await;
        let f = sample_file();
        s.insert_file(&f).await.unwrap();
        s.upsert_fts(&f.id, "議事録.txt", "仕事", "これは日本語のテスト文章です")
            .await
            .unwrap();

        // 3 文字以上 → trigram MATCH
        assert_eq!(s.search_fts("テスト", 10).await.unwrap(), vec![f.id]);
        assert_eq!(s.search_fts("日本語", 10).await.unwrap(), vec![f.id]);
        // 2 文字 → LIKE フォールバック
        assert_eq!(s.search_fts("文章", 10).await.unwrap(), vec![f.id]);
        assert_eq!(s.search_fts("仕事", 10).await.unwrap(), vec![f.id]);
        // 複数語は AND
        assert_eq!(s.search_fts("日本語 テスト", 10).await.unwrap(), vec![f.id]);
        assert!(s.search_fts("日本語 存在しない", 10).await.unwrap().is_empty());
        // 一致しない語
        assert!(s.search_fts("英語", 10).await.unwrap().is_empty());
        // 記号を含む入力が構文エラーにならない（フレーズ引用の確認）
        assert!(s.search_fts("C++ \"quote", 10).await.unwrap().is_empty());
        // 空クエリは空結果
        assert!(s.search_fts("   ", 10).await.unwrap().is_empty());
    }

    /// ファイル名・タグでの一致が本文のみの一致より上位に並ぶ
    /// （MATCH 経路 = bm25 列重み、LIKE 経路 = CASE スコアの両方）。
    #[tokio::test]
    async fn search_fts_ranks_name_and_tag_matches_first() {
        let s = store().await;
        let by_content = sample_file();
        let by_tag = sample_file();
        let by_name = sample_file();
        for f in [&by_content, &by_tag, &by_name] {
            s.insert_file(f).await.unwrap();
        }
        s.upsert_fts(&by_content.id, "z.txt", "", "本文にだけ検索対象という語がある")
            .await
            .unwrap();
        s.upsert_fts(&by_tag.id, "y.txt", "検索対象", "無関係な本文")
            .await
            .unwrap();
        s.upsert_fts(&by_name.id, "検索対象.txt", "", "無関係な本文")
            .await
            .unwrap();

        // 3 文字以上 → MATCH 経路（bm25 重み）
        let got = s.search_fts("検索対象", 10).await.unwrap();
        assert_eq!(got, vec![by_name.id, by_tag.id, by_content.id]);

        // 2 文字 → LIKE 経路（CASE スコア）
        let got = s.search_fts("検索", 10).await.unwrap();
        assert_eq!(got, vec![by_name.id, by_tag.id, by_content.id]);
    }

    /// LIKE フォールバックで % や _ がワイルドカードとして解釈されない。
    #[tokio::test]
    async fn search_fts_like_escapes_wildcards() {
        let s = store().await;
        let f = sample_file();
        s.insert_file(&f).await.unwrap();
        s.upsert_fts(&f.id, "a.txt", "", "100%完了").await.unwrap();

        assert_eq!(s.search_fts("0%", 10).await.unwrap(), vec![f.id]);
        // "%" 単体はどの本文にも literal に無ければヒットしない
        let g = sample_file();
        s.insert_file(&g).await.unwrap();
        s.upsert_fts(&g.id, "b.txt", "", "percent なし").await.unwrap();
        assert_eq!(s.search_fts("0%", 10).await.unwrap(), vec![f.id]);
    }
}
