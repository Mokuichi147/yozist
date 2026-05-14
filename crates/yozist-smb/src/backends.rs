//! Share 別バックエンドのスケルトン。
//!
//! 本実装では `smb_server::ShareBackend` トレイトを満たす形にする。
//! 現段階では依存を共有する空構造体のみ。

use crate::ShareDeps;

macro_rules! share_backend {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        pub struct $name {
            #[allow(dead_code)]
            deps: ShareDeps,
        }
        impl $name {
            pub fn new(deps: ShareDeps) -> Self {
                Self { deps }
            }
        }
    };
}

share_backend!(
    /// 階層パスをタグ AND 条件として解釈する share。
    ///
    /// # 操作セマンティクス
    /// - `mkdir tags/A` → タグ A を作成
    /// - `cp file tags/A/` → ファイル登録 + タグ A 付与
    /// - `mv tags/A/file tags/B/` → タグ A→B 置換
    /// - `rm tags/A/file` → タグ A 取り外し（ファイル実体は残る）
    TagsBackend
);

share_backend!(
    /// シリーズ単位のビュー。ファイル名先頭に `order_index` ゼロ詰めプレフィクス。
    SeriesBackend
);

share_backend!(
    /// 直近更新の読取専用ビュー。
    RecentBackend
);

share_backend!(
    /// 管理用フラットビュー（全ファイル）。
    AllBackend
);
