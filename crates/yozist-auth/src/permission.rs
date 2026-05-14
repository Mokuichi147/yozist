//! 権限モデル: Subject × Target × PermissionMask × allow/deny。

use serde::{Deserialize, Serialize};
use yozist_core::{FileId, GroupId, SeriesId, TagId, UserId};

/// 権限の主体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Subject {
    User(UserId),
    Group(GroupId),
}

/// 権限の対象。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Target {
    Share(String),
    Tag(TagId),
    Series(SeriesId),
    File(FileId),
    /// 動的クエリ（saved path）。query は serde_json::Value 想定。
    Query(serde_json::Value),
}

bitflags::bitflags! {
    /// 権限ビット。複数を OR で組み合わせる。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct PermissionMask: u32 {
        const VIEW  = 0b0001;
        const READ  = 0b0010;
        const WRITE = 0b0100;
        const ADMIN = 0b1000;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permission {
    pub subject: Subject,
    pub target: Target,
    pub mask: PermissionMask,
    pub allow: bool,
    pub priority: i32,
    pub expires_at: Option<time::OffsetDateTime>,
}
