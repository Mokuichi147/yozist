//! yozist-db — メタデータ DB 抽象。`MetaStore` トレイトと SQLite 実装を提供。
//!
//! # 設計原則
//! - **メタデータの権威性**: ファイル・タグ・シリーズ・コミット・ACL すべての
//!   真実の所有者は `MetaStore`。blob 自体は何のメタも持たない。
//! - **抽象化**: 初期は SQLite。Postgres 等への切替は feature flag で。
//! - **WAL モード必須**: 並行アクセスに対応するため SQLite は `journal_mode=WAL`。
//!
//! # TODO
//! - [ ] PostgresMetaStore（feature `postgres`）
//! - [ ] スキーマバージョン管理（マイグレーションテーブル）
//! - [ ] フルテキスト検索（FTS5 / pg_trgm）
//! - [ ] ACL クエリの WHERE 句注入ヘルパ

use async_trait::async_trait;
use std::sync::Arc;
use yozist_core::{
    Commit, FileId, FileMeta, SavedQuery, SavedQueryId, Series, SeriesId, SeriesMember, Tag,
    TagId,
};

pub mod audit;
pub mod sqlite;
pub use audit::{AuditEntry, AuditLog, AuditRecord, SharedAuditLog};
pub use sqlite::SqliteMetaStore;

/// ファイル一覧のソートキー。`list_files_sorted` で使用する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileSort {
    /// 更新日時（既定）
    #[default]
    UpdatedAt,
    /// 作成日時
    CreatedAt,
    /// 表示名（大文字小文字を無視）
    Name,
    /// サイズ
    Size,
}

/// メタデータ保存の統一インターフェース。
#[async_trait]
pub trait MetaStore: Send + Sync {
    // ---- files ----
    async fn insert_file(&self, meta: &FileMeta) -> Result<(), DbError>;
    async fn get_file(&self, id: &FileId) -> Result<Option<FileMeta>, DbError>;
    async fn update_file(&self, meta: &FileMeta) -> Result<(), DbError>;
    async fn list_files(&self, limit: u32, offset: u32) -> Result<Vec<FileMeta>, DbError>;
    /// ソートキー・昇降順を指定した一覧。ページングと組み合わせて WebUI が使う。
    async fn list_files_sorted(
        &self,
        limit: u32,
        offset: u32,
        sort: FileSort,
        asc: bool,
    ) -> Result<Vec<FileMeta>, DbError>;

    // ---- tags ----
    async fn upsert_tag(&self, tag: &Tag) -> Result<TagId, DbError>;
    async fn get_tag(&self, id: &TagId) -> Result<Option<Tag>, DbError>;
    async fn get_tag_by_name(&self, name: &str) -> Result<Option<Tag>, DbError>;
    async fn list_tags(&self) -> Result<Vec<Tag>, DbError>;
    /// 割り当て数の多い順（同数は名前昇順）にタグを返す。タグ候補の提示に使う。
    async fn list_tags_by_usage(&self) -> Result<Vec<Tag>, DbError>;
    async fn rename_tag(&self, id: &TagId, new_name: &str) -> Result<(), DbError>;
    async fn delete_tag(&self, id: &TagId) -> Result<(), DbError>;
    async fn attach_tag(&self, file: &FileId, tag: &TagId) -> Result<(), DbError>;
    async fn detach_tag(&self, file: &FileId, tag: &TagId) -> Result<(), DbError>;
    async fn list_tags_of(&self, file: &FileId) -> Result<Vec<Tag>, DbError>;
    /// 複数ファイルのタグを 1 クエリでまとめて取得する（一覧ページのタグ表示用）。
    async fn list_tags_of_many(
        &self,
        files: &[FileId],
    ) -> Result<Vec<(FileId, Tag)>, DbError>;
    async fn list_files_by_tags(&self, tags: &[TagId]) -> Result<Vec<FileMeta>, DbError>;

    // ---- series ----
    async fn upsert_series(&self, series: &Series) -> Result<SeriesId, DbError>;
    async fn get_series(&self, id: &SeriesId) -> Result<Option<Series>, DbError>;
    async fn list_series(&self) -> Result<Vec<Series>, DbError>;
    async fn rename_series(
        &self,
        id: &SeriesId,
        new_name: &str,
        description: Option<&str>,
    ) -> Result<(), DbError>;
    async fn delete_series(&self, id: &SeriesId) -> Result<(), DbError>;
    async fn add_to_series(&self, member: &SeriesMember) -> Result<(), DbError>;
    async fn remove_from_series(
        &self,
        series: &SeriesId,
        file: &FileId,
    ) -> Result<(), DbError>;
    async fn list_series_members(&self, series: &SeriesId) -> Result<Vec<SeriesMember>, DbError>;

    // ---- commits ----
    async fn insert_commit(&self, commit: &Commit) -> Result<(), DbError>;
    async fn list_commits(&self, file: &FileId) -> Result<Vec<Commit>, DbError>;

    // ---- full-text search (FTS5) ----
    /// FTS の対応行を upsert。`display_name` / `tags` / `content` のいずれも空文字可。
    async fn upsert_fts(
        &self,
        file: &FileId,
        display_name: &str,
        tags: &str,
        content: &str,
    ) -> Result<(), DbError>;
    /// FTS から削除（ファイル削除時など）。
    async fn delete_fts(&self, file: &FileId) -> Result<(), DbError>;
    /// MATCH クエリで一致する `FileId` を新しい順で返す。
    async fn search_fts(&self, query: &str, limit: u32) -> Result<Vec<FileId>, DbError>;

    // ---- saved queries ----
    async fn upsert_saved_query(&self, query: &SavedQuery) -> Result<SavedQueryId, DbError>;
    async fn get_saved_query(
        &self,
        id: &SavedQueryId,
    ) -> Result<Option<SavedQuery>, DbError>;
    async fn get_saved_query_by_name(
        &self,
        name: &str,
    ) -> Result<Option<SavedQuery>, DbError>;
    async fn list_saved_queries(&self) -> Result<Vec<SavedQuery>, DbError>;
    async fn delete_saved_query(&self, id: &SavedQueryId) -> Result<(), DbError>;
}

pub type SharedMetaStore = Arc<dyn MetaStore>;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid data: {0}")]
    Invalid(String),
}

impl From<DbError> for yozist_core::Error {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound => yozist_core::Error::NotFound("metadata".into()),
            DbError::Conflict(m) => yozist_core::Error::Conflict(m),
            _ => yozist_core::Error::Metadata(e.to_string()),
        }
    }
}
