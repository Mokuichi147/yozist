//! yozist-server — 全レイヤーを束ねるバイナリ。
//!
//! サブコマンド:
//! - `serve` … SMB + REST API を起動
//! - `migrate` … DB マイグレーション実行
//! - `version` … バージョン表示

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "yozist", version, about = "Intelligent file platform")]
struct Cli {
    /// 設定ファイル（TOML）
    #[arg(long, default_value = "yozist.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// サーバー起動
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
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Version => {
            println!("yozist {}", env!("CARGO_PKG_VERSION"));
        }
        Cmd::Migrate => {
            // TODO: 設定読み込み + SqliteMetaStore::open でマイグレーション実行
            let path = std::env::var("YOZIST_DB").unwrap_or_else(|_| "yozist.sqlite".into());
            let _store = yozist_db::SqliteMetaStore::open(&path).await?;
            println!("migrations applied to {}", path);
        }
        Cmd::Serve => {
            // TODO: 全レイヤー組み立て + SMB + axum 起動
            tracing::info!("serve subcommand: not yet implemented (skeleton)");
            tracing::info!("config = {:?}", cli.config);
        }
    }
    Ok(())
}
