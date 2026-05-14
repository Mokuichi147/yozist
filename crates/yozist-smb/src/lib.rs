//! yozist-smb — SMB ネットワーク層。タグ／シリーズ中心の仮想 FS を提供する。
//!
//! # 設計
//! - 採用クレート: [`smb-server`](https://github.com/paltaio/rust-smb-server) v0.4 系
//!   （Edition 2024 / rustc 1.95+。本スケルトンでは toolchain 制約で参照のみ）
//! - 各 share（tags / series / recent / all）ごとに `ShareBackend` 実装を持つ
//! - すべての操作は `yozist-versioning` / `yozist-db` の公開 API 経由
//!
//! # Share 一覧
//! | share | 内容 |
//! |-------|------|
//! | `tags` | 階層パス = タグの AND 条件 |
//! | `series` | 配下に `NNNN__name` 形式で順序付きメンバー |
//! | `recent` | 直近 N 件（読取専用） |
//! | `all` | 全ファイルをフラット |
//!
//! # TODO
//! - [ ] `smb-server` v0.4 統合（rustc 1.95+ 切替後）
//! - [ ] `TagsBackend` の `ShareBackend` impl
//! - [ ] `SeriesBackend` の `ShareBackend` impl
//! - [ ] `RecentBackend` / `AllBackend`
//! - [ ] `AuthContext` を SMB セッションから抽出するアダプタ
//! - [ ] SMB Change Notify による他クライアントへの即時反映

use std::sync::Arc;
use yozist_auth::{AuthContext, Authorizer};
use yozist_db::SharedMetaStore;
use yozist_storage::SharedBlobStore;

pub mod backends;
pub use backends::{AllBackend, RecentBackend, SeriesBackend, TagsBackend};

/// 各 share 実装が共有する依存。
#[derive(Clone)]
pub struct ShareDeps {
    pub meta: SharedMetaStore,
    pub blob: SharedBlobStore,
    pub authz: Arc<dyn Authorizer>,
}

/// バックエンドが受け取る要求コンテキスト。`AuthContext` を引き回す。
#[derive(Clone)]
pub struct RequestCtx {
    pub auth: AuthContext,
}

/// 起動設定。
pub struct SmbConfig {
    pub listen: std::net::SocketAddr,
}

/// SMB サーバー起動エントリ。スケルトン段階では未実装。
pub async fn serve(_cfg: SmbConfig, _deps: ShareDeps) -> Result<(), SmbError> {
    // TODO: smb_server::SmbServer::builder() で全 share を組み立てて起動
    Err(SmbError::NotImplemented)
}

#[derive(Debug, thiserror::Error)]
pub enum SmbError {
    #[error("not implemented (smb-server integration pending toolchain upgrade)")]
    NotImplemented,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
