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

公開される SMB share は **`yozist` 1 つだけ**（全仮想ビューへの単一エントリ）。`smb://host/yozist/` に接続すると組込みビューが仮想フォルダとして並び、配下を辿って各ビューへアクセスする。

| パス | 内容 |
|------|------|
| `yozist\` | ルート。組込みビュー (all / tags / series / filters) が並ぶ |
| `yozist\all\` | 全ファイルをフラット |
| `yozist\tags\仕事\2026\` | 階層パス = タグの **AND 条件**（「仕事 AND 2026」のファイル群） |
| `yozist\series\` | 配下に `NNNN__name` 形式で順序付きメンバー |
| `yozist\filters\` | 全「条件付きパス」（任意名）が仮想フォルダとして並ぶ |
| `yozist\filters\<任意の名前>\` | **フィルタ**。タグの AND / NOT 条件に任意の名前を付けたファイル群（読取専用） |

**フィルタ**は macOS のスマートフォルダのように、タグ（手動 / システム / AI / 種別不問）・シリーズ・種類(MIME)・名前・日付（作成 / 更新）の条件を「すべて(AND) / いずれか(OR)」で組み合わせて定義できる。名前・条件は作成したユーザーが WebUI からいつでも変更でき、作成・編集・削除は専用の **フィルタページ (`/ui/filters`)** で行う。条件評価は REST（一覧）と SMB（`filters\<名前>\`）で共通の `yozist-db::resolve_filter` が担い、DB を都度参照するため変更は即時反映される。Explorer のドラッグ＆ドロップでタグ付けが完結する。

### 並行アクセス前提

- テキスト: CRDT で自動マージ（`yrs` ベース）
  - 文字コードは UTF-8 / Shift-JIS / EUC-JP / UTF-16(LE/BE, BOM) 等を自動判定して取り込み（内部・blob は UTF-8 で統一）。元エンコーディングは保持し、ダウンロード／SMB read 時に元の形式へ再エンコードして返す。
- バイナリ: LWW（最終書き込み勝ち）
- メタデータ: 楽観ロック + SQLite WAL モード

### 細粒度の権限とパス発行

ユーザー／グループ単位で share / タグ / シリーズ / ファイル / フィルタ各レベルに View/Read/Write/Admin を設定可能。期限付き共有 URL や動的 SMB share の発行も対応予定。

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

未定。
