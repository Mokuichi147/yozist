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

use user_permission_core::Database as AuthDb;
use yozist_api::ApiState;
use yozist_auth::{Authorizer, DbAuthorizer, ShareTokenStore};
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

    /// 認証 (ユーザー/グループ/JWT) を中央の user-permission サーバへ中継する
    /// 場合の URL（例: `http://localhost:8001`）。未指定ならローカル SQLite
    /// (`<data>/auth.db`) を使う。
    #[arg(long, env = "YOZIST_AUTH_RELAY")]
    auth_relay: Option<String>,

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

            // 共有トークン用の HMAC シークレット (yozist-auth)。
            let secret_path = cli.data.join("jwt-secret.bin");
            let secret = load_or_create_secret(&secret_path).await?;
            let share_admin = Arc::new(ShareTokenStore::new(pool.clone(), secret));

            // ユーザー / グループ / JWT 認証は upstream user-permission に委譲。
            // --auth-relay が指定されていれば中央サーバへ中継、無ければローカル SQLite。
            let auth_db = if let Some(url) = &cli.auth_relay {
                tracing::info!("auth relay: {url}");
                Arc::new(AuthDb::open_relay(url)?)
            } else {
                let auth_db_path = cli.data.join("auth.db");
                let auth_secret_path = cli.data.join("auth-secret.key");
                tracing::info!("opening auth db: {}", auth_db_path.display());
                Arc::new(AuthDb::open_local(&auth_db_path, Some(&auth_secret_path)).await?)
            };

            let db_authz = Arc::new(DbAuthorizer::new(pool.clone()));
            let authz: Arc<dyn Authorizer> = db_authz.clone();

            let audit = Arc::new(AuditLog::new(pool.clone()));

            // SMB を (有効なら) 先に構築し、REST 認証経路へ渡す資格情報シンクを得る。
            // 認証は user-permission と統合され、平文パスワードが REST 経路を通過した
            // 時に NT ハッシュが smb_credentials テーブルへ保存される。
            let smb_built = if cli.smb_listen.is_empty() {
                tracing::info!("SMB disabled (--smb-listen is empty)");
                None
            } else {
                let smb_addr: std::net::SocketAddr = cli
                    .smb_listen
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid SMB listen addr: {e}"))?;
                let deps = ShareDeps {
                    meta: meta.clone(),
                    blob: blob.clone(),
                    engine: engine.clone(),
                    authz: authz.clone(),
                    auth_db: auth_db.clone(),
                    acl_admin: db_authz.clone(),
                    audit: audit.clone(),
                };
                Some(yozist_smb::build(SmbConfig { listen: smb_addr }, deps, pool.clone()).await?)
            };
            let smb_creds = smb_built.as_ref().map(|b| b.credential_sink());

            let state = ApiState {
                meta: meta.clone(),
                engine: engine.clone(),
                auth_db: auth_db.clone(),
                authz: authz.clone(),
                acl_admin: db_authz.clone(),
                audit: audit.clone(),
                share_admin,
                smb_creds,
                content_cache: std::sync::Arc::new(yozist_api::ContentCache::default()),
            };
            let app = yozist_api::router(state);

            // SMB を別タスクで起動
            let smb_task = smb_built.map(|built| {
                tokio::spawn(async move {
                    if let Err(e) = built.serve().await {
                        tracing::error!("SMB server failed: {e}");
                    }
                })
            });

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
