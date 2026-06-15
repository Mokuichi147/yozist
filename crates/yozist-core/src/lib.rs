//! yozist-core — 全クレート共通の型・ID・エラー定義。
//!
//! # 設計原則
//! - **一元管理 (SSoT)**: ファイルメタデータ・タグ・シリーズ・履歴はすべて
//!   `MetaStore` が唯一の権威。本クレートはその表現型のみを提供する。
//! - **並行性**: すべての公開型は `Send + Sync + Clone` を想定し、`Arc` 共有可能。
//!
//! # TODO
//! - [ ] `ActorId` の発行ルール統一（SMB セッション / API JWT / AI）
//! - [ ] `FormatHint` のフィールド拡張（言語推定、コーデック情報など）
//! - [ ] エラー型の階層分割（永続化系 vs ロジック系）

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// ID newtypes
// ---------------------------------------------------------------------------

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self { Self(Uuid::now_v7()) }
            pub fn from_uuid(u: Uuid) -> Self { Self(u) }
            pub fn as_uuid(&self) -> &Uuid { &self.0 }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_newtype!(/// ファイル論理 ID（実体 blob とは独立）。
FileId);
id_newtype!(/// コミット（変更履歴 1 件）。
CommitId);
id_newtype!(/// タグ。
TagId);
id_newtype!(/// シリーズ。
SeriesId);
id_newtype!(/// アクター（編集操作の主体）。CRDT の `actor_id` に対応。
ActorId);
id_newtype!(/// フィルター（Shareable Path）。
FilterId);

// ユーザー / グループの ID は upstream `user-permission` の `i64` を直接使う。
// 型エイリアスで意図を表現するが、実体は `i64`。
pub type UserId = i64;
pub type GroupId = i64;

/// Blob のコンテンツアドレス（SHA-256 を想定）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobId(pub String);

impl BlobId {
    pub fn from_hex(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// メタデータ型
// ---------------------------------------------------------------------------

/// ファイルメタデータ。物理パスは持たず、`display_name` のみ。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub id: FileId,
    pub display_name: String,
    pub size: u64,
    pub mime: Option<String>,
    /// テキストファイルの元エンコーディング（charset ラベル）。
    /// 例: `"Shift_JIS"`, `"EUC-JP"`, `"UTF-16LE"`, `"UTF-8"`, `"UTF-8-BOM"`。
    /// CRDT/blob は常に UTF-8 で保持し、ダウンロードや SMB read の際に
    /// この charset へ再エンコードして「元の形式」で返す。`None` は
    /// バイナリ（LWW）または charset 未判定を表す。
    #[serde(default)]
    pub charset: Option<String>,
    pub current_commit: Option<CommitId>,
    pub created_at: time::OffsetDateTime,
    pub updated_at: time::OffsetDateTime,
    pub deleted: bool,
    /// 作成者のユーザー名ラベル。ユーザー削除後も表示が残るよう ID ではなく
    /// ラベルを保存する。`None` は記録なし（旧データ・SMB 経由など）。
    #[serde(default)]
    pub created_by: Option<String>,
    /// 最終更新者のユーザー名ラベル。意味は `created_by` と同じ。
    #[serde(default)]
    pub updated_by: Option<String>,
    /// 作成者の不変キー（users.id）。表示ラベル `created_by` とは別に、改名・
    /// 同名再登録に強い内部追跡用に持つ。API/UI には出さない（`serde(skip)`）。
    /// `None` は記録なし（旧データ・SMB/匿名）。
    #[serde(skip)]
    pub created_by_user_id: Option<i64>,
    /// 最終更新者の不変キー（users.id）。意味は `created_by_user_id` と同じ。
    #[serde(skip)]
    pub updated_by_user_id: Option<i64>,
}

/// CRDT/LWW のフォーマット選択ヒント。
#[derive(Debug, Clone, Default)]
pub struct FormatHint {
    pub extension: Option<String>,
    pub mime: Option<String>,
    pub first_bytes: Option<Vec<u8>>,
    pub display_name: Option<String>,
}

/// タグ種別（3層）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TagKind {
    /// 拡張子・パス等から自動付与
    System,
    /// AI 推測（信頼スコア付き）
    Ai,
    /// ユーザー手動付与（最優先）
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: TagId,
    pub name: String,
    pub kind: TagKind,
    /// AI タグの信頼スコア（0.0–1.0）。それ以外は None。
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Series {
    pub id: SeriesId,
    pub name: String,
    pub description: Option<String>,
}

/// 条件のマッチ方法。`All` = すべて(AND)、`Any` = いずれか(OR)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    #[default]
    All,
    Any,
}

/// スマートフォルダ風の 1 条件。`field` / `op` / `value`（+ 日付の `unit`）の
/// フラットな文字列表現にして、フロントエンドとの相互運用と将来の属性追加を
/// 容易にしている。解決は [`yozist_db::resolve_filter`] が行う。
///
/// 対応する `field` と `op`:
/// - `tag` / `manual_tag` / `system_tag` / `ai_tag` … op: `include` | `exclude`、value = タグ名
/// - `series` … op: `include` | `exclude`、value = シリーズ名
/// - `mime`（種類）… op: `include` | `exclude`、value = MIME 部分文字列（例: `pdf`, `image/`）
/// - `name`（名前）… op: `contains` | `not_contains` | `is` | `starts_with` | `ends_with`
/// - `created` / `updated`（作成日 / 更新日）… op: `within` | `before` | `after`、
///   value = 数値、unit = `day` | `month` | `year`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterCondition {
    pub field: String,
    pub op: String,
    #[serde(default)]
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// フィルター条件。`tags_and` / `tags_not` は後方互換のためのレガシー表現で、
/// 解決時には `conditions`（タグ include/exclude）と同等に評価される。
/// 新 UI は `match_mode` + `conditions` を使う。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilterDef {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags_and: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags_not: Vec<String>,
    #[serde(default)]
    pub match_mode: MatchMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<FilterCondition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    pub id: FilterId,
    pub name: String,
    pub definition: FilterDef,
    pub description: Option<String>,
    pub created_by: Option<UserId>,
    pub created_at: time::OffsetDateTime,
    pub expires_at: Option<time::OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeriesMember {
    pub series_id: SeriesId,
    pub file_id: FileId,
    /// シリーズ内の順序。f64 で中間挿入を容易にする。
    pub order_index: f64,
}

/// 1 件の変更履歴。CRDT は OpLog、LWW は parent + blob ポインタで表現。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub id: CommitId,
    pub file_id: FileId,
    pub parent: Option<CommitId>,
    pub actor: ActorId,
    pub blob: BlobId,
    pub format_id: String,
    pub timestamp: time::OffsetDateTime,
    pub message: Option<String>,
    /// コミットを実行したユーザー名ラベル。CRDT マージ用の `actor` とは別物で、
    /// 「誰が変更したか」の表示・監査用。ユーザー削除後も残るよう ID ではなくラベルで保持。
    /// 旧データや SMB 経由の書き込みでは NULL。
    #[serde(default)]
    pub committed_by: Option<String>,
    /// 実行ユーザーの不変キー（users.id）。表示ラベル `committed_by` とは別に、
    /// 改名・同名再登録に強い内部追跡用に持つ。API/UI には出さない（`serde(skip)`）。
    /// 旧データや SMB/匿名では NULL。
    #[serde(skip)]
    pub committed_by_user_id: Option<i64>,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("metadata error: {0}")]
    Metadata(String),
    #[error("versioning error: {0}")]
    Versioning(String),
    #[error(transparent)]
    Other(#[from] anyhow_compat::AnyError),
}

pub type Result<T> = std::result::Result<T, Error>;

/// `anyhow::Error` を持ち込まずに `From` 経由でラップするための薄い受け皿。
pub mod anyhow_compat {
    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    pub struct AnyError(pub String);

    impl AnyError {
        pub fn new(msg: impl Into<String>) -> Self {
            Self(msg.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_unique() {
        let a = FileId::new();
        let b = FileId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn tag_kind_serializes_lowercase() {
        let s = serde_json::to_string(&TagKind::Manual).unwrap();
        assert_eq!(s, "\"manual\"");
    }
}
