//! yozist-server — 全レイヤーを束ねるバイナリ。
//!
//! サブコマンド:
//! - `serve`   … REST API を起動（SMB は次フェーズで統合）
//! - `migrate` … DB マイグレーション実行
//! - `version` … バージョン表示
//!
//! # 設定優先順位
//! 1. CLI 引数
//! 2. 環境変数 `YOZIST_*`
//! 3. 設定ファイル（`--config` で指定）
//! 4. デフォルト値

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

use yozist_api::ApiState;
use yozist_auth::{AuthService, Authorizer, DbAuthorizer, SqliteAuthService};
use yozist_db::{AuditLog, SharedMetaStore, SqliteMetaStore};
use yozist_smb::{ShareDeps, SmbConfig};
use yozist_storage::{FsBlobStore, SharedBlobStore};
use yozist_versioning::{CrdtRegistry, VersioningEngine};

#[derive(Parser, Debug)]
#[command(name = "yozist", version, about = "Intelligent file platform")]
struct Cli {
    /// 設定ファイル（TOML）
    #[arg(long, default_value = "yozist.toml")]
    config: PathBuf,

    /// データディレクトリ（DB と blob を格納）
    #[arg(long, env = "YOZIST_DATA", default_value = "./data")]
    data: PathBuf,

    /// API listen アドレス
    #[arg(long, env = "YOZIST_LISTEN", default_value = "127.0.0.1:7878")]
    listen: String,

    /// SMB listen アドレス（空文字列で無効化）
    #[arg(long, env = "YOZIST_SMB_LISTEN", default_value = "127.0.0.1:4445")]
    smb_listen: String,

    /// 初期 SMB ユーザー (`user:password`)。複数指定可。
    #[arg(long, env = "YOZIST_SMB_USER")]
    smb_user: Vec<String>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// REST API サーバー起動
    Serve,
    /// DB マイグレーション
    Migrate,
    /// バージョン表示
    Version,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Version => {
            println!("yozist {}", env!("CARGO_PKG_VERSION"));
        }
        Cmd::Migrate => {
            tokio::fs::create_dir_all(&cli.data).await?;
            let db_path = cli.data.join("yozist.sqlite");
            let _store = SqliteMetaStore::open(&db_path).await?;
            println!("migrations applied to {}", db_path.display());
        }
        Cmd::Serve => {
            tokio::fs::create_dir_all(&cli.data).await?;
            let db_path = cli.data.join("yozist.sqlite");
            let blob_path = cli.data.join("blobs");

            tracing::info!("opening db: {}", db_path.display());
            let store = SqliteMetaStore::open(&db_path).await?;
            let pool = store.pool().clone();
            let meta: SharedMetaStore = Arc::new(store);

            let blob: SharedBlobStore = Arc::new(FsBlobStore::new(&blob_path).await?);
            let registry = Arc::new(CrdtRegistry::with_defaults());
            let engine = Arc::new(VersioningEngine::new(
                registry,
                blob.clone(),
                meta.clone(),
            ));

            let secret_path = cli.data.join("jwt-secret.bin");
            let secret = load_or_create_secret(&secret_path).await?;
            let auth: Arc<dyn AuthService> =
                Arc::new(SqliteAuthService::new(pool.clone(), secret));

            let db_authz = Arc::new(DbAuthorizer::new(pool.clone()));
            let authz: Arc<dyn Authorizer> = db_authz.clone();

            let audit = Arc::new(AuditLog::new(pool.clone()));
            let state = ApiState {
                meta: meta.clone(),
                engine: engine.clone(),
                auth: auth.clone(),
                authz: authz.clone(),
                acl_admin: db_authz.clone(),
                audit: audit.clone(),
            };
            let app = yozist_api::router(state);

            // SMB を別タスクで起動
            let smb_task = if cli.smb_listen.is_empty() {
                tracing::info!("SMB disabled (--smb-listen is empty)");
                None
            } else {
                let smb_addr: std::net::SocketAddr = cli
                    .smb_listen
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid SMB listen addr: {e}"))?;
                let users: Vec<(String, String)> = cli
                    .smb_user
                    .iter()
                    .filter_map(|s| s.split_once(':').map(|(u, p)| (u.into(), p.into())))
                    .collect();
                if users.is_empty() {
                    tracing::warn!(
                        "SMB starting in public mode (no --smb-user provided); \
                         use --smb-user user:pw to require authentication"
                    );
                }
                let cfg = SmbConfig {
                    listen: smb_addr,
                    initial_users: users,
                };
                let deps = ShareDeps {
                    meta,
                    blob,
                    engine,
                    authz,
                    auth,
                    acl_admin: db_authz,
                };
                Some(tokio::spawn(async move {
                    if let Err(e) = yozist_smb::serve(cfg, deps).await {
                        tracing::error!("SMB server failed: {e}");
                    }
                }))
            };

            let listener = TcpListener::bind(&cli.listen).await?;
            tracing::info!("listening on {}", cli.listen);
            let api_result = axum::serve(listener, app).await;

            if let Some(t) = smb_task {
                t.abort();
            }
            api_result?;
        }
    }
    Ok(())
}

async fn load_or_create_secret(path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    if path.exists() {
        Ok(tokio::fs::read(path).await?)
    } else {
        use rand::RngCore;
        let mut buf = vec![0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        tokio::fs::write(path, &buf).await?;
        Ok(buf)
    }
}
