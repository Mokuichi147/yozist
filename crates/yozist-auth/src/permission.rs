//! 権限モデル: Subject × Target × PermissionMask × allow/deny。
//!
//! `Target` は `(kind, ref_)` の opaque な組で表現する。
//! kind/ref_ の解釈は呼び出し側（yozist-api 等）の責務。

use serde::{Deserialize, Serialize};
use yozist_core::{GroupId, UserId};

/// 権限の主体。ID は user-permission の i64 を直接使う。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Subject {
    User(UserId),
    Group(GroupId),
}

/// 権限の対象。kind/ref_ は任意文字列。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub kind: String,
    pub ref_: String,
}

impl Target {
    pub fn new(kind: impl Into<String>, ref_: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            ref_: ref_.into(),
        }
    }

    /// ファイル単位の Target を組み立てるヘルパー。
    pub fn file(id: impl ToString) -> Self {
        Self::new("file", id.to_string())
    }

    /// 名前付き共有の Target を組み立てるヘルパー。
    pub fn share(name: impl Into<String>) -> Self {
        Self::new("share", name)
    }
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
}
