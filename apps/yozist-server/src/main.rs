//! yozist-server — 全レイヤーを束ねるバイナリ。
//!
//! サブコマンド:
//! - `serve`             … REST API を起動（SMB は次フェーズで統合）
//! - `migrate`            … DB マイグレーション実行
//! - `version`            … バージョン表示
//! - `cache-warm`         … サムネイル/プレビュー軽量化キャッシュの未生成分を一括生成
//! - `cache-regenerate`   … サムネイル/プレビュー軽量化キャッシュを強制的に再生成
//! - `ai-tag-generate`    … 画像の AI 自動タグを一括生成（未生成 / モデル変更分 / 全件）
//!
//! # 設定優先順位
//! 1. CLI 引数
//! 2. 環境変数 `YOZIST_*`
//! 3. 設定ファイル（`--config` で指定）
//! 4. デフォルト値

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;

use user_permission_core::Database as AuthDb;
use yozist_api::ApiState;
use yozist_auth::{Authorizer, DbAuthorizer, ShareTokenStore};
use yozist_core::{FileId, FileMeta};
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

    /// サムネイル/プレビュー軽量化キャッシュの保存先（実ファイル + 索引 DB）。
    /// SSD 等の高速ストレージを指定できるよう `--data` とは独立に指定できる。
    /// 未指定時は `<data>/cache`。
    #[arg(long, env = "YOZIST_CACHE_DIR")]
    cache_dir: Option<PathBuf>,

    /// サムネイル variant（一覧表示用）の長辺上限（px）。未指定時は既定値 480px。
    #[arg(long, env = "YOZIST_CACHE_THUMBNAIL_MAX_PX")]
    cache_thumbnail_max_px: Option<u32>,

    /// プレビュー variant（詳細ページ用）の長辺上限（px）。未指定時は既定値 1600px。
    #[arg(long, env = "YOZIST_CACHE_PREVIEW_MAX_PX")]
    cache_preview_max_px: Option<u32>,

    /// JPEG 出力時の圧縮品質（0-100）。thumbnail/preview 共通で上書きする。
    /// 未指定時は variant ごとの既定値（thumbnail=75, preview=82）。
    #[arg(long, env = "YOZIST_CACHE_QUALITY", value_parser = parse_quality)]
    cache_quality: Option<f32>,

    /// プレビュー生成ワーカーの本数。生成は CPU バウンド（mozjpeg/oxipng）
    /// なので、増やすほど配信中のリクエスト処理と CPU を奪い合う。未指定時は
    /// コア数の半分（最小 1・最大 4）。
    #[arg(long, env = "YOZIST_CACHE_WORKERS", value_parser = clap::value_parser!(u32).range(1..=64))]
    cache_workers: Option<u32>,

    /// AI 自動タグ生成に使う OpenAI 互換エンドポイント（`/v1` まで含むベース URL）。
    /// **未指定なら AI 機能全体が無効**になる（既存の動作のまま）。
    #[arg(long, env = "YOZIST_AI_ENDPOINT")]
    ai_endpoint: Option<String>,

    /// タグ生成に使う vision 対応モデル名。`--ai-endpoint` 指定時は必須。
    /// この値は生成済みタグに記録され、変更すると付け直しの対象になる。
    #[arg(long, env = "YOZIST_AI_MODEL")]
    ai_model: Option<String>,

    /// AI エンドポイントの API キー。キー不要なローカルサーバでは省略できる。
    #[arg(long, env = "YOZIST_AI_API_KEY", hide_env_values = true)]
    ai_api_key: Option<String>,

    /// 表記ゆれ解消（narashi）に使う埋め込みエンドポイント。未指定時は
    /// `--ai-endpoint` と同じ。
    #[arg(long, env = "YOZIST_AI_EMBEDDING_ENDPOINT")]
    ai_embedding_endpoint: Option<String>,

    /// 表記ゆれ解消に使う埋め込みモデル名。
    #[arg(
        long,
        env = "YOZIST_AI_EMBEDDING_MODEL",
        default_value = "text-embedding-qwen3-embedding-4b"
    )]
    ai_embedding_model: String,

    /// 1 ファイルに付ける AI タグの最大数。
    #[arg(long, env = "YOZIST_AI_TAG_MAX", default_value_t = 10,
          value_parser = clap::value_parser!(u32).range(1..=100))]
    ai_tag_max: u32,

    /// これ未満の信頼度（0.0-1.0）の候補は捨てる。
    #[arg(long, env = "YOZIST_AI_TAG_MIN_CONFIDENCE", default_value_t = 0.5,
          value_parser = parse_confidence)]
    ai_tag_min_confidence: f32,

    /// 表記ゆれ統合のしきい値（0-100）。高いほど別タグとして残りやすい。
    #[arg(long, env = "YOZIST_AI_TAG_SIMILARITY",
          default_value_t = yozist_ai::DEFAULT_SIMILARITY_THRESHOLD,
          value_parser = parse_similarity)]
    ai_tag_similarity: f32,

    /// 寄せ先候補として埋め込みに渡す既存タグの上限（使用数の多い順）。
    /// 埋め込みは毎回全件送るため、増やすほど 1 回のリクエストが重くなる。
    #[arg(long, env = "YOZIST_AI_VOCAB_LIMIT", default_value_t = 300,
          value_parser = clap::value_parser!(u32).range(0..=5000))]
    ai_vocab_limit: u32,

    /// AI タグ生成の同時実行数（サーバのワーカー本数、および
    /// `ai-tag-generate` が同時に走らせる件数）。
    ///
    /// 生成はほぼ全部がネットワーク待ち（1 枚あたり数十秒）で、こちらの CPU は
    /// 使わない。直列にすると待ち時間がそのまま件数倍になるため既定で並列に
    /// する。接続先が受けられる同時実行スロット数に合わせて調整する。
    #[arg(long, env = "YOZIST_AI_WORKERS", default_value_t = 4,
          value_parser = clap::value_parser!(u32).range(1..=32))]
    ai_workers: u32,

    /// リクエストに載せる `reasoning_effort`。推論モデルは思考に数百トークン
    /// 使い、`max_tokens` に達すると本文が空のまま打ち切られるため既定で切る。
    /// 受け付ける値はサーバによって違う（空文字を渡すとフィールドごと省略する）。
    #[arg(long, env = "YOZIST_AI_REASONING_EFFORT", default_value = "none")]
    ai_reasoning_effort: String,

    /// AI エンドポイントへの 1 リクエストのタイムアウト（秒）。
    #[arg(long, env = "YOZIST_AI_TIMEOUT_SECS", default_value_t = 180,
          value_parser = clap::value_parser!(u64).range(1..=3600))]
    ai_timeout_secs: u64,

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
    /// サムネイル/プレビュー軽量化キャッシュの未生成分（失敗分含む）を一括生成する。
    CacheWarm {
        /// 対象 variant（`thumbnail` / `preview`）。省略時は両方。
        #[arg(long)]
        variant: Option<String>,
    },
    /// サムネイル/プレビュー軽量化キャッシュを強制的に再生成する。
    CacheRegenerate {
        /// 対象ファイル ID。省略時は --all が必須。
        #[arg(long)]
        file: Option<String>,
        /// 全画像ファイルを対象にする（--file と排他）。
        #[arg(long)]
        all: bool,
        /// 対象 variant（`thumbnail` / `preview`）。省略時は両方。
        #[arg(long)]
        variant: Option<String>,
    },
    /// AI 自動タグを生成する（既に付いているものは差し替える）。
    AiTagGenerate {
        /// 対象範囲。
        /// `missing` = 未生成（記録なし・失敗・取り残された生成待ち）、
        /// `stale` = missing に加えて別モデルで生成した分と再コミット分、
        /// `all` = 全画像ファイル。
        #[arg(long, default_value = "missing")]
        scope: String,
        /// 対象ファイル ID。指定時は --scope を無視して 1 件だけ生成し直す。
        #[arg(long)]
        file: Option<String>,
    },
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
    match &cli.command {
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

            // サムネイル/プレビュー軽量化キャッシュ層。実処理は yozist-cache の
            // PreviewJobHandler が担い、yozist-jobs の汎用ワーカーに乗せる
            // （将来 AI 自動タグ付け等を追加する際も同じ JobRunner に別 kind を
            // 登録するだけでよい）。
            let (job_runner, cache_store, cache_dir) = open_cache_layer(&cli, engine.clone()).await?;
            let workers = cli
                .cache_workers
                .map(|n| n as usize)
                .unwrap_or_else(default_cache_workers);
            tracing::info!("preview cache workers: {workers}");
            job_runner.spawn_workers(workers);
            let job_store = job_runner.store().clone();

            // AI 自動タグ層。プレビューと同じ JobStore を共有しつつ、ワーカーは
            // 別立てにする（生成が数十秒のネットワーク待ちなので、相乗りさせると
            // プレビュー生成が詰まる）。
            let ai = open_ai_layer(&cli, engine.clone(), meta.clone(), job_store.clone()).await?;
            if let Some(ai) = &ai {
                tracing::info!("AI タグ生成ワーカー: {}", cli.ai_workers);
                ai.runner.spawn_workers(cli.ai_workers as usize);
            } else {
                tracing::info!("AI タグ生成は無効（--ai-endpoint 未指定）");
            }

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
                view_registry: std::sync::Arc::new(yozist_view::ViewRegistry::with_defaults()),
                data_dir: cli.data.clone(),
                cache_store: cache_store.clone(),
                job_store,
                cache_dir: cache_dir.clone(),
                ai: ai.as_ref().map(|a| a.service.clone()),
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

            // 孤立 blob スイーパ: デルタ再符号化やファイル完全削除で参照を失った
            // blob を定期回収する。猶予期間を置くことで、候補登録時点で走って
            // いた読み出しやコミットと競合しない。初回 tick は起動直後に発火し、
            // 前回起動時の残骸も回収する。
            let sweep_engine = engine.clone();
            tokio::spawn(async move {
                const SWEEP_INTERVAL: std::time::Duration =
                    std::time::Duration::from_secs(15 * 60);
                let mut tick = tokio::time::interval(SWEEP_INTERVAL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    match sweep_engine.sweep_orphan_blobs(SWEEP_INTERVAL).await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!("孤立 blob を {n} 件回収"),
                        Err(e) => tracing::warn!("孤立 blob の回収に失敗: {e}"),
                    }
                }
            });

            // 陳腐化したプレビューキャッシュのスイーパ: ファイル削除/purge や
            // 再コミットで参照されなくなった preview_cache 行（と実ファイル）を
            // 定期回収する。放置すると再コミットのたびに SSD を消費し続ける。
            let sweep_meta = meta.clone();
            let sweep_cache_store = cache_store.clone();
            let sweep_cache_dir = cache_dir.clone();
            tokio::spawn(async move {
                const SWEEP_INTERVAL: std::time::Duration =
                    std::time::Duration::from_secs(15 * 60);
                let mut tick = tokio::time::interval(SWEEP_INTERVAL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    match sweep_stale_preview_cache(&sweep_meta, &sweep_cache_store, &sweep_cache_dir).await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!("陳腐化したプレビューキャッシュを {n} 件回収"),
                        Err(e) => tracing::warn!("プレビューキャッシュの回収に失敗: {e}"),
                    }
                }
            });

            let listener = TcpListener::bind(&cli.listen).await?;
            tracing::info!("listening on {}", cli.listen);
            let api_result = axum::serve(listener, app).await;

            if let Some(t) = smb_task {
                t.abort();
            }
            api_result?;
        }
        Cmd::CacheWarm { variant } => {
            let (meta, engine) = open_meta_and_engine(&cli.data).await?;
            let (job_runner, cache_store, _cache_dir) = open_cache_layer(&cli, engine).await?;
            let variants = parse_variants(variant.as_deref())?;

            let files = list_image_files(&meta).await?;
            let candidates: Vec<(String, String)> = files
                .iter()
                .filter_map(|f| f.current_commit.map(|c| (f.id.to_string(), c.to_string())))
                .collect();

            let mut enqueued = 0usize;
            let mut skipped = 0usize;
            let mut already_queued = 0usize;
            for v in &variants {
                let missing = cache_store.list_missing_for(&candidates, *v).await?;
                skipped += candidates.len() - missing.len();
                for (file_id, commit_id) in &missing {
                    if enqueue_preview_job(&job_runner, &cache_store, file_id, commit_id, *v).await?
                    {
                        enqueued += 1;
                    } else {
                        already_queued += 1;
                    }
                }
            }
            println!(
                "cache-warm: {enqueued} 件投入、{skipped} 件は生成済みのためスキップ\
                 、{already_queued} 件は投入済みのジョブが処理待ち。処理中..."
            );
            report_drain_result("cache-warm", job_runner.drain().await);
        }
        Cmd::CacheRegenerate { file, all, variant } => {
            if file.is_some() == *all {
                anyhow::bail!("--file <id> か --all のどちらか一方を指定してください");
            }
            let (meta, engine) = open_meta_and_engine(&cli.data).await?;
            let (job_runner, cache_store, _cache_dir) = open_cache_layer(&cli, engine).await?;
            let variants = parse_variants(variant.as_deref())?;

            let targets: Vec<FileMeta> = if let Some(id) = file {
                let uuid = uuid::Uuid::parse_str(id)
                    .map_err(|e| anyhow::anyhow!("invalid file id: {e}"))?;
                let file_id = FileId::from_uuid(uuid);
                let f = meta
                    .get_file(&file_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("file not found: {id}"))?;
                vec![f]
            } else {
                list_image_files(&meta).await?
            };

            let mut count = 0usize;
            for f in &targets {
                let Some(commit) = f.current_commit else {
                    continue;
                };
                let file_id_s = f.id.to_string();
                let commit_id_s = commit.to_string();
                for v in &variants {
                    cache_store.reset_to_pending(&file_id_s, &commit_id_s, *v).await?;
                    // 既に未完了ジョブが積まれていれば dedup で弾かれるが、
                    // そのジョブが同じ組み合わせを生成するので目的は達せられる。
                    enqueue_preview_job(&job_runner, &cache_store, &file_id_s, &commit_id_s, *v)
                        .await?;
                    count += 1;
                }
            }
            println!("cache-regenerate: {count} 件投入。処理中...");
            report_drain_result("cache-regenerate", job_runner.drain().await);
        }
        Cmd::AiTagGenerate { scope, file } => {
            let (meta, engine) = open_meta_and_engine(&cli.data).await?;
            let cache_dir = resolve_cache_dir(&cli).await?;
            let job_store =
                Arc::new(yozist_jobs::JobStore::open(cache_dir.join("jobs.sqlite")).await?);
            let ai = open_ai_layer(&cli, engine, meta.clone(), job_store)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "AI タグ生成が無効です（--ai-endpoint / --ai-model を指定してください）"
                    )
                })?;

            let summary = if let Some(id) = file {
                // ファイル指定は「この 1 件を今すぐ作り直す」意図なので scope は見ない。
                let uuid = uuid::Uuid::parse_str(id)
                    .map_err(|e| anyhow::anyhow!("invalid file id: {e}"))?;
                let file_id = FileId::from_uuid(uuid);
                let f = meta
                    .get_file(&file_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("file not found: {id}"))?;
                let commit = f
                    .current_commit
                    .ok_or_else(|| anyhow::anyhow!("コミットがまだありません: {id}"))?;
                let inserted = ai.service.enqueue(&file_id, &commit).await?;
                yozist_ai::EnqueueSummary {
                    targets: 1,
                    enqueued: usize::from(inserted),
                    already_queued: usize::from(!inserted),
                }
            } else {
                let scope = yozist_db::AiTagScope::parse(scope).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--scope は missing / stale / all のいずれかです: {scope}"
                    )
                })?;
                ai.service.enqueue_scope(scope).await?
            };

            println!(
                "ai-tag-generate: 対象 {} 件のうち {} 件投入、{} 件は投入済みのジョブが処理待ち。\
                 同時 {} 件で処理中...",
                summary.targets, summary.enqueued, summary.already_queued, cli.ai_workers
            );
            // 生成はネットワーク待ちなので直列だと件数倍の時間がかかる。
            report_drain_result(
                "ai-tag-generate",
                ai.runner.drain_with(cli.ai_workers as usize).await,
            );
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

/// メタ DB + blob store + VersioningEngine のみを組み立てる（CLI 一括処理用。
/// `Cmd::Serve` は auth/SMB 等も必要なため個別に構築している）。
async fn open_meta_and_engine(data: &Path) -> anyhow::Result<(SharedMetaStore, Arc<VersioningEngine>)> {
    tokio::fs::create_dir_all(data).await?;
    let db_path = data.join("yozist.sqlite");
    let blob_path = data.join("blobs");
    let store = SqliteMetaStore::open(&db_path).await?;
    let meta: SharedMetaStore = Arc::new(store);
    let blob: SharedBlobStore = Arc::new(FsBlobStore::new(&blob_path).await?);
    let registry = Arc::new(CrdtRegistry::with_defaults());
    let engine = Arc::new(VersioningEngine::new(registry, blob, meta.clone()));
    Ok((meta, engine))
}

/// キャッシュディレクトリ・キャッシュ DB・ジョブキューを開き、
/// `PreviewJobHandler` を `kind = "preview.generate"` として登録した
/// `JobRunner` を返す。`Cmd::Serve` と `cache-warm`/`cache-regenerate` の
/// いずれからも呼ばれる（生成ロジックを二重実装しないため）。
async fn open_cache_layer(
    cli: &Cli,
    engine: Arc<VersioningEngine>,
) -> anyhow::Result<(Arc<yozist_jobs::JobRunner>, Arc<yozist_cache::CacheStore>, PathBuf)> {
    let cache_dir = resolve_cache_dir(cli).await?;

    match sweep_leftover_temp_files(&cache_dir).await {
        Ok(0) => {}
        Ok(n) => tracing::info!("前回の生成中断で残った中間ファイルを {n} 件削除"),
        Err(e) => tracing::warn!("中間ファイルの掃除に失敗: {e}"),
    }

    let job_store = Arc::new(yozist_jobs::JobStore::open(cache_dir.join("jobs.sqlite")).await?);
    let cache_store = Arc::new(yozist_cache::CacheStore::open(cache_dir.join("cache.sqlite")).await?);

    let mut configs = yozist_cache::VariantConfigs::default();
    if let Some(px) = cli.cache_thumbnail_max_px {
        configs.thumbnail.max_edge_px = px;
    }
    if let Some(px) = cli.cache_preview_max_px {
        configs.preview.max_edge_px = px;
    }
    if let Some(q) = cli.cache_quality {
        configs.thumbnail.quality = q;
        configs.preview.quality = q;
    }

    let handler: Arc<dyn yozist_jobs::JobHandler> = Arc::new(yozist_cache::PreviewJobHandler::new(
        engine,
        cache_store.clone(),
        cache_dir.clone(),
        configs,
    ));
    let mut runner = yozist_jobs::JobRunner::new(job_store);
    runner.register("preview.generate", handler);
    let runner = Arc::new(runner);

    Ok((runner, cache_store, cache_dir))
}

/// キャッシュディレクトリを解決して作成する。ここにはプレビュー実体のほか、
/// 全ジョブ種別が共有する `jobs.sqlite` も置かれる。
async fn resolve_cache_dir(cli: &Cli) -> anyhow::Result<PathBuf> {
    let cache_dir = cli
        .cache_dir
        .clone()
        .unwrap_or_else(|| cli.data.join("cache"));
    tokio::fs::create_dir_all(&cache_dir).await?;
    tracing::info!("preview cache dir: {}", cache_dir.display());
    Ok(cache_dir)
}

/// AI 自動タグ層。`--ai-endpoint` 未指定なら組み立てない。
struct AiLayer {
    /// `ai.tag` だけを登録した専用ランナー。プレビュー生成と同じ `JobStore` を
    /// 共有しつつ、ワーカー本数を独立に決めるために runner を分けている
    /// （`JobStore::claim_next` は kind で絞れる）。相乗りさせると、数十秒の
    /// ネットワーク待ちが CPU バウンドなプレビュー生成のワーカーを塞ぐ。
    runner: Arc<yozist_jobs::JobRunner>,
    service: Arc<yozist_ai::AiTagService>,
}

/// AI タグ生成に必要な一式（vision プロバイダ・narashi 正規化・ジョブハンドラ）を
/// 組み立て、`kind = "ai.tag"` として登録した専用ランナーを返す。
/// `--ai-endpoint` 未指定時は `Ok(None)`（AI 機能なしで従来どおり動く）。
async fn open_ai_layer(
    cli: &Cli,
    engine: Arc<VersioningEngine>,
    meta: SharedMetaStore,
    job_store: Arc<yozist_jobs::JobStore>,
) -> anyhow::Result<Option<AiLayer>> {
    let Some(endpoint) = cli.ai_endpoint.as_deref().map(str::trim).filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let model = cli
        .ai_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("--ai-endpoint を指定する場合は --ai-model も必要です")
        })?;

    let embedding_endpoint = cli
        .ai_embedding_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(endpoint);

    tracing::info!("AI タグ生成モデル: {model} / 埋め込み: {}", cli.ai_embedding_model);

    let provider = yozist_ai::OpenAiVisionProvider::new(
        endpoint,
        model,
        cli.ai_api_key.clone(),
        cli.ai_tag_max as usize,
        Some(cli.ai_reasoning_effort.clone()),
        std::time::Duration::from_secs(cli.ai_timeout_secs),
    )?;
    let normalizer = yozist_ai::TagNormalizer::new(
        embedding_endpoint,
        cli.ai_embedding_model.clone(),
        cli.ai_api_key.clone(),
        cli.ai_tag_similarity,
    )?;

    let settings = yozist_ai::AiTagSettings {
        model: model.to_string(),
        max_tags: cli.ai_tag_max as usize,
        min_confidence: cli.ai_tag_min_confidence,
        vocab_limit: cli.ai_vocab_limit as usize,
    };

    let handler: Arc<dyn yozist_jobs::JobHandler> = Arc::new(yozist_ai::AiTagJobHandler::new(
        engine,
        meta.clone(),
        Arc::new(provider),
        Arc::new(normalizer),
        settings,
    ));
    let mut runner = yozist_jobs::JobRunner::new(job_store.clone());
    runner.register(yozist_ai::AI_TAG_JOB_KIND, handler);

    Ok(Some(AiLayer {
        runner: Arc::new(runner),
        service: Arc::new(yozist_ai::AiTagService::new(job_store, meta, model.to_string())),
    }))
}

/// `JobRunner::drain` の戻り値（捌けずに残った件数）を人間向けに報告する。
/// リトライ上限に達して恒久失敗したジョブがあると 0 にならないことがあるため、
/// 「完了」と言い切らずに残件数を出す。
fn report_drain_result(cmd: &str, remaining: i64) {
    match remaining {
        0 => println!("{cmd}: 完了"),
        n if n < 0 => println!("{cmd}: 中断（キューの状態を取得できませんでした）"),
        n => println!("{cmd}: {n} 件が未処理のまま残りました（ログを確認してください）"),
    }
}

/// 圧縮品質は 0-100。範囲外を素通しすると compressor 側の挙動が読めないので
/// 起動時に弾く。
fn parse_quality(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|_| format!("数値ではありません: {s}"))?;
    if !(0.0..=100.0).contains(&v) {
        return Err(format!("品質は 0-100 の範囲で指定してください: {v}"));
    }
    Ok(v)
}

/// 信頼度の閾値は 0.0-1.0。範囲外だと全部落ちる／全部通るのどちらかになり、
/// 設定ミスに気付きにくいので起動時に弾く。
fn parse_confidence(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|_| format!("数値ではありません: {s}"))?;
    if !(0.0..=1.0).contains(&v) {
        return Err(format!("信頼度は 0.0-1.0 の範囲で指定してください: {v}"));
    }
    Ok(v)
}

/// 表記ゆれ統合のしきい値は 0-100（narashi のスコア域）。
fn parse_similarity(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|_| format!("数値ではありません: {s}"))?;
    if !(0.0..=100.0).contains(&v) {
        return Err(format!("しきい値は 0-100 の範囲で指定してください: {v}"));
    }
    Ok(v)
}

/// 既定のワーカー本数。生成は CPU バウンドなので、配信用に余力を残して
/// コア数の半分（最小 1・最大 4）にする。
///
/// NOTE: これは「同時に走る生成ジョブの本数」であって CPU 使用量の上限では
/// ない。oxipng は内部でグローバルな rayon プールへ展開するため、1 本でも
/// 全コアを使いうる。厳密に絞りたい場合は `RAYON_NUM_THREADS` を併用する。
fn default_cache_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 4))
        .unwrap_or(1)
}

fn parse_variants(s: Option<&str>) -> anyhow::Result<Vec<yozist_cache::Variant>> {
    match s {
        None => Ok(vec![yozist_cache::Variant::Thumbnail, yozist_cache::Variant::Preview]),
        Some(s) => {
            let v = yozist_cache::Variant::parse(s)
                .ok_or_else(|| anyhow::anyhow!("unknown variant: {s} (thumbnail か preview を指定)"))?;
            Ok(vec![v])
        }
    }
}

/// 論理削除されておらず、画像 mime を持つファイルを全件取得する。
///
/// ID 順のキーセットページングで走査する。`list_files` の OFFSET ページングは
/// `updated_at` 順なので、走査中に誰かがコミットすると行がページ間を移動し、
/// バックフィル対象を取りこぼす。
async fn list_image_files(meta: &SharedMetaStore) -> anyhow::Result<Vec<FileMeta>> {
    const PAGE: u32 = 500;
    let mut out = Vec::new();
    let mut cursor: Option<FileId> = None;
    loop {
        let page = meta.list_files_after(cursor.as_ref(), PAGE).await?;
        let n = page.len() as u32;
        cursor = page.last().map(|f| f.id);
        out.extend(
            page.into_iter()
                .filter(|f| f.mime.as_deref().is_some_and(|m| m.starts_with("image/"))),
        );
        if n < PAGE {
            break;
        }
    }
    Ok(out)
}

/// 戻り値は「実際にジョブ行が追加されたか」。既に同じ未完了ジョブが積まれて
/// いれば `false`（呼び出し側の件数報告が実態とずれないようにする）。
async fn enqueue_preview_job(
    job_runner: &yozist_jobs::JobRunner,
    cache_store: &yozist_cache::CacheStore,
    file_id: &str,
    commit_id: &str,
    variant: yozist_cache::Variant,
) -> anyhow::Result<bool> {
    let dedup_key = yozist_cache::PreviewJobPayload::dedup_key(file_id, commit_id, variant);
    let payload = yozist_cache::PreviewJobPayload::new(file_id, commit_id, variant);
    let inserted = job_runner
        .store()
        .enqueue("preview.generate", Some(&dedup_key), &payload)
        .await?;
    cache_store.mark_pending(file_id, commit_id, variant).await?;
    Ok(inserted)
}

/// preview_cache のうち「ファイルが削除/purge 済み」または「commit_id が現在の
/// current_commit と異なる（再コミットで陳腐化した旧 variant）」行を削除し、
/// 対応する実ファイルも取り除く。削除件数を返す。
async fn sweep_stale_preview_cache(
    meta: &SharedMetaStore,
    cache_store: &yozist_cache::CacheStore,
    cache_dir: &Path,
) -> anyhow::Result<usize> {
    let file_ids = cache_store.list_distinct_file_ids().await?;

    // キャッシュに載っている ID の現在の状態をまとめて引く。1 件ずつ get_file を
    // 呼ぶと、掃除のたびにキャッシュ行数ぶんのクエリが走る（15 分ごとの定期
    // タスクなので、ファイルが増えるほど無視できなくなる）。
    let parsed: Vec<(String, FileId)> = file_ids
        .into_iter()
        .filter_map(|s| {
            uuid::Uuid::parse_str(&s)
                .ok()
                .map(|u| (s, FileId::from_uuid(u)))
        })
        .collect();
    let known: std::collections::HashMap<FileId, FileMeta> = meta
        .get_files(&parsed.iter().map(|(_, id)| *id).collect::<Vec<_>>())
        .await?
        .into_iter()
        .map(|f| (f.id, f))
        .collect();

    let mut removed = 0usize;
    for (file_id_s, file_id) in parsed {
        let rel_paths = match known.get(&file_id) {
            Some(file) if !file.deleted => match file.current_commit {
                Some(current) => {
                    cache_store
                        .delete_stale(&file_id_s, &current.to_string())
                        .await?
                }
                None => cache_store.delete_by_file(&file_id_s).await?,
            },
            // メタ DB に無い（purge 済み）か論理削除済み。
            _ => cache_store.delete_by_file(&file_id_s).await?,
        };
        for rel in rel_paths {
            let path = cache_dir.join(&rel);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => removed += 1,
                // 既に無いなら回収済み。それ以外の失敗は DB 行だけ消えて実ファイル
                // が孤児として残るため、黙って捨てずに記録する。
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("キャッシュファイルを削除できません {rel}: {e}"),
            }
        }
    }
    Ok(removed)
}

/// 生成中にプロセスが落ちると `PreviewGenerator` の中間ファイル（`.tmp-*`）が
/// シャードディレクトリに残る。DB には現れないため通常のスイーパでは回収
/// できないので、起動時にまとめて掃除する。
///
/// ただし「今まさに生成中のファイル」と「前回の残骸」は名前では区別できない。
/// この関数は `serve` からも `cache-warm`/`cache-regenerate` からも呼ばれるため、
/// 稼働中のサーバの傍らで CLI を叩くと生成途中の中間ファイルを消してしまい、
/// 生成が I/O エラーとして失敗する。更新から十分に時間が経ったものだけを
/// 残骸とみなす（判定基準は固着ジョブの回収と同じ猶予）。
async fn sweep_leftover_temp_files(cache_dir: &Path) -> anyhow::Result<usize> {
    let cutoff = std::time::SystemTime::now() - yozist_jobs::STALLED_LEASE;
    let mut removed = 0usize;
    let mut shards = match tokio::fs::read_dir(cache_dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    while let Some(shard) = shards.next_entry().await? {
        if !shard.file_type().await?.is_dir() {
            continue;
        }
        let mut entries = tokio::fs::read_dir(shard.path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // 中間 PNG（`-src.png`）と圧縮結果（`-out.<ext>`）の両方が対象。
            // 拡張子は出力フォーマット次第で変わるので前置だけで判定する。
            if !name.starts_with(".tmp-") {
                continue;
            }
            // mtime が取れないものは判断材料が無いので触らない（次回に持ち越す）。
            let recent = match entry.metadata().await.and_then(|m| m.modified()) {
                Ok(modified) => modified > cutoff,
                Err(e) => {
                    tracing::warn!("中間ファイルの更新時刻を取得できません {name}: {e}");
                    true
                }
            };
            if recent {
                continue;
            }
            match tokio::fs::remove_file(entry.path()).await {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("中間ファイルを削除できません {name}: {e}"),
            }
        }
    }
    Ok(removed)
}
