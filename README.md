# yozist

**インテリジェント・ファイル・プラットフォーム**

SMB プロトコルで OS から透過アクセスできる「使いやすさ」と、データベース／CRDT／AI による「堅牢さ・知能性」を両立させた次世代ファイル管理基盤。

## 設計原則

### 🔑 ファイルの一元管理（Single Source of Truth）

yozist が扱うすべてのファイルは、`BlobStore` + `MetaStore` が**唯一の真実の所有者**。SMB / REST / WebUI / AI / CLI のすべてがこの一元化された store を経由してのみアクセスする。

- バイパス禁止（OS で直接触っても yozist の状態は壊れない設計）
- メタデータ（名前・パス・タグ・順序・履歴）はすべて DB に
- 書き込みは必ず `yozist-versioning::commit` を経由
- 読み出しはどの経路でも同じビュー

### タグ／シリーズ中心の仮想 FS

SMB 上に見えるのは「従来のフォルダ階層」ではなく、タグとシリーズに最適化された仮想ビュー。

| Share | 内容 |
|-------|------|
| `tags` | 階層パス = タグの **AND 条件**。`tags\仕事\2026\` で「仕事 AND 2026」のファイル群 |
| `series` | 配下に `NNNN__name` 形式で順序付きメンバー |
| `recent` | 直近 N 件（読取専用） |
| `all` | 全ファイルをフラット |

Explorer のドラッグ＆ドロップでタグ付けが完結する。

### 並行アクセス前提

- テキスト: CRDT で自動マージ（`yrs` 採用予定、現状スケルトン）
- バイナリ: LWW（最終書き込み勝ち）
- メタデータ: 楽観ロック + SQLite WAL モード

### 細粒度の権限とパス発行

ユーザー／グループ単位で share / タグ / シリーズ / ファイル / クエリ各レベルに View/Read/Write/Admin を設定可能。期限付き共有 URL や動的 SMB share の発行も対応予定。

## アーキテクチャ

```
yozist/
├── crates/
│   ├── yozist-core/       共通型・エラー・ID
│   ├── yozist-storage/    BlobStore trait + FsBlobStore (CAS + zstd)
│   ├── yozist-db/         MetaStore trait + SqliteMetaStore + migrations
│   ├── yozist-versioning/ CrdtFormat trait + CrdtRegistry（プラガブル）
│   ├── yozist-tagging/    3 層タグ + シリーズ
│   ├── yozist-auth/       UserPermission の Rust 移植 + ACL
│   ├── yozist-ai/         AiProvider trait
│   ├── yozist-smb/        タグ／シリーズ別仮想 share
│   └── yozist-api/        axum REST + WebUI（leptos）
└── apps/
    └── yozist-server/     all-in-one バイナリ
```

## ビルド

rustc 1.95+ が必要（`rust-toolchain.toml` で `stable` を指定済）。

```sh
cargo build --workspace
cargo test --workspace
```

## 起動

```sh
# DB マイグレーション
cargo run -p yozist-server -- migrate

# サーバー起動（現状はスケルトン）
cargo run -p yozist-server -- serve
```

## ライセンス

MIT
