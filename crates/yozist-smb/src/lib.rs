//! yozist-smb — SMB ネットワーク層。タグ／シリーズ中心の仮想 FS を提供する。
//!
//! # 設計
//! - 採用クレート: [`smb-server`](https://github.com/paltaio/rust-smb-server) v0.4 系
//! - 各 share（all / tags / series / recent）ごとに `ShareBackend` 実装を持つ
//! - すべての操作は `yozist-versioning` / `yozist-db` の公開 API 経由
//!
//! # Share 一覧
//! | share | 内容 |
//! |-------|------|
//! | `all` | 全ファイルをフラット (v1) |
//! | `tags` | 階層パス = タグの AND 条件 (v2 TODO) |
//! | `series` | 配下に `NNNN__name` 形式で順序付きメンバー (v2 TODO) |
//! | `recent` | 直近 N 件（読取専用） (v2 TODO) |
//!
//! # TODO
//! - [ ] TagsBackend / SeriesBackend / RecentBackend の本実装
//! - [ ] `AuthContext` を SMB セッションから抽出するアダプタ
//! - [ ] SMB Change Notify による他クライアントへの即時反映
//! - [ ] truncate / set_times の完全対応

use smb_server::{Access, Share, SmbServer};
use std::sync::Arc;
use yozist_auth::{AuthContext, Authorizer};
use yozist_db::SharedMetaStore;
use yozist_storage::SharedBlobStore;
use yozist_versioning::VersioningEngine;

pub mod backends;
pub mod handle;
pub use backends::{AllBackend, QueriesBackend, RecentBackend, SeriesBackend, TagsBackend};

/// 各 share 実装が共有する依存。
#[derive(Clone)]
pub struct ShareDeps {
    pub meta: SharedMetaStore,
    pub blob: SharedBlobStore,
    pub engine: Arc<VersioningEngine>,
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
    /// 初期ユーザー（user, password）。`smb-server` 組込み認証で利用。
    pub initial_users: Vec<(String, String)>,
}

/// SMB サーバー起動エントリ。
pub async fn serve(cfg: SmbConfig, deps: ShareDeps) -> Result<(), SmbError> {
    let mut builder = SmbServer::builder().listen(cfg.listen);
    for (u, p) in &cfg.initial_users {
        builder = builder.user(u, p);
    }

    let setup_share = |_name: &str, share: Share| {
        if cfg.initial_users.is_empty() {
            share.public()
        } else {
            cfg.initial_users
                .iter()
                .fold(share, |sh, (u, _)| sh.user(u, Access::ReadWrite))
        }
    };
    let all_share = setup_share("all", Share::new("all", AllBackend::new(deps.clone())));
    let tags_share = setup_share("tags", Share::new("tags", TagsBackend::new(deps.clone())));
    let series_share = setup_share(
        "series",
        Share::new("series", SeriesBackend::new(deps.clone())),
    );
    let queries_share = setup_share(
        "queries",
        Share::new("queries", QueriesBackend::new(deps.clone())),
    );

    let server = builder
        .share(all_share)
        .share(tags_share)
        .share(series_share)
        .share(queries_share)
        .build()
        .map_err(|e| SmbError::Build(e.to_string()))?;

    let bound = server
        .bind()
        .await
        .map_err(|e| SmbError::Bind(e.to_string()))?;
    tracing::info!("SMB server listening on {} (bound={bound})", cfg.listen);
    server
        .serve()
        .await
        .map_err(|e| SmbError::Serve(e.to_string()))?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum SmbError {
    #[error("build error: {0}")]
    Build(String),
    #[error("bind error: {0}")]
    Bind(String),
    #[error("serve error: {0}")]
    Serve(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
