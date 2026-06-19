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
    Commit, FileId, FileMeta, Filter, FilterId, Series, SeriesId, SeriesMember, SeriesSort,
    Tag, TagId,
};

pub mod audit;
pub mod resolver;
pub mod sqlite;
pub use audit::{AuditEntry, AuditLog, AuditRecord, SharedAuditLog};
pub use resolver::resolve_filter;
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
    /// 論理削除済みファイル（ゴミ箱）の一覧。削除日時の新しい順に返す。
    async fn list_deleted_files(&self, limit: u32, offset: u32) -> Result<Vec<FileMeta>, DbError>;
    /// ファイルを物理削除する（ゴミ箱からの完全削除）。関連する commits / file_tags /
    /// series_members / blob_refs は FK の ON DELETE CASCADE で同時に消える。blob 本体は
    /// CAS（共有・GC なし）のため残す。存在しなければ `NotFound`。
    async fn purge_file(&self, id: &FileId) -> Result<(), DbError>;

    // ---- tags ----
    async fn upsert_tag(&self, tag: &Tag) -> Result<TagId, DbError>;
    async fn get_tag(&self, id: &TagId) -> Result<Option<Tag>, DbError>;
    async fn get_tag_by_name(&self, name: &str) -> Result<Option<Tag>, DbError>;
    async fn list_tags(&self) -> Result<Vec<Tag>, DbError>;
    /// 割り当て数の多い順（同数は名前昇順）にタグを返す。タグ候補の提示に使う。
    async fn list_tags_by_usage(&self) -> Result<Vec<Tag>, DbError>;
    /// 各タグと割り当てファイル数を名前昇順で返す。タグ管理ページの一覧表示に使う。
    async fn list_tags_with_counts(&self) -> Result<Vec<(Tag, u64)>, DbError>;
    async fn rename_tag(&self, id: &TagId, new_name: &str) -> Result<(), DbError>;
    async fn delete_tag(&self, id: &TagId) -> Result<(), DbError>;
    /// `source` タグを `target` タグへ合流する。`source` を付けていたファイルはすべて
    /// `target` に付け替え（重複は無視）、`source` タグ自体を削除する。
    /// `source` が存在しなければ `NotFound`、`target` が存在しなければ `Invalid`。
    async fn merge_tags(&self, source: &TagId, target: &TagId) -> Result<(), DbError>;
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
    /// シリーズの並び順設定のみを更新する。
    async fn set_series_sort(&self, id: &SeriesId, sort: SeriesSort) -> Result<(), DbError>;
    async fn add_to_series(&self, member: &SeriesMember) -> Result<(), DbError>;
    async fn remove_from_series(
        &self,
        series: &SeriesId,
        file: &FileId,
    ) -> Result<(), DbError>;
    async fn list_series_members(&self, series: &SeriesId) -> Result<Vec<SeriesMember>, DbError>;
    /// 指定ファイルが所属するシリーズ一覧（名前順）を返す。
    async fn list_series_of_file(&self, file: &FileId) -> Result<Vec<Series>, DbError>;
    /// 指定シリーズのメンバーを表示名付きで返す（削除済みファイルは除外）。
    /// 並び順はシリーズの `sort_order` 設定に従う（登録日時 / 名前 / 手動）。
    async fn list_series_members_named(
        &self,
        series: &SeriesId,
    ) -> Result<Vec<(FileId, String)>, DbError>;

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

    // ---- filters ----
    async fn upsert_filter(&self, query: &Filter) -> Result<FilterId, DbError>;
    async fn get_filter(
        &self,
        id: &FilterId,
    ) -> Result<Option<Filter>, DbError>;
    async fn get_filter_by_name(
        &self,
        name: &str,
    ) -> Result<Option<Filter>, DbError>;
    async fn list_filters(&self) -> Result<Vec<Filter>, DbError>;
    async fn delete_filter(&self, id: &FilterId) -> Result<(), DbError>;
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
