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
| `yozist\filters\<任意の名前>\` | **フィルター**。タグの AND / NOT 条件に任意の名前を付けたファイル群（読取専用） |

**フィルター**は macOS のスマートフォルダのように、タグ（手動 / システム / AI / 種別不問）・シリーズ・種類(MIME)・名前・日付（作成 / 更新）の条件を「すべて(AND) / いずれか(OR)」で組み合わせて定義できる。名前・条件は作成したユーザーが WebUI からいつでも変更でき、作成・編集・削除は専用の **フィルターページ (`/ui/filters`)** で行う。条件評価は REST（一覧）と SMB（`filters\<名前>\`）で共通の `yozist-db::resolve_filter` が担い、DB を都度参照するため変更は即時反映される。Explorer のドラッグ＆ドロップでタグ付けが完結する。

### AI による自動タグ付け

画像ファイルは、vision 対応 LLM（OpenAI 互換エンドポイント）で内容からタグを自動生成できる。
アップロード／コミット時にバックグラウンドのジョブキューへ投入され、生成が終わるとタグが付く。

LLM の出すタグ名は表記ゆれが激しい（「白背景 / 白い背景 / 白バック」）ため、
[narashi](https://crates.io/crates/narashi) の多言語埋め込みで既存タグ語彙へ寄せる（日本語優先）。
「山」が既存の「山岳」に吸収される、といった統合が自動で効く。

- 生成タグは手動タグと同じ `tags` / `file_tags` に載り、検索・フィルタ・SMB からそのまま使える
- どのモデルで生成したかをファイル単位で記録し、モデルを差し替えた分だけ付け直せる
- 生成タグは利用者からは変更できない（更新は再生成のみ）。同名タグを手動で作れば
  優先度ルール（Manual > AI > System）で手動タグへ昇格し、以後は編集できる
- 付け直しは **未生成 / このモデルで未生成 / すべて** の 3 段階で、
  WebUI の管理ページと CLI (`ai-tag-generate --scope`) の両方から実行できる
- 生成はほぼ全部がネットワーク待ちなので、サーバ・CLI とも既定で 4 件を同時に
  処理する（`--ai-workers`）。接続先が受けられる同時実行数に合わせて調整する。
  プレビュー生成（CPU バウンド）とはワーカーを分けてあり、互いに詰まらせない

`--ai-endpoint` を指定しない限り機能ごと無効（既存の動作のまま）。設定は CLI 引数と
環境変数のどちらでも渡せる（`--help` を参照）。

```sh
# 接続先とモデルを指定して起動
cargo run -p yozist-server -- --ai-endpoint http://<host>:<port>/v1 --ai-model <model> serve

# 既存ファイルへの一括適用 / モデル変更後の付け直し
cargo run -p yozist-server -- --ai-endpoint ... --ai-model ... ai-tag-generate --scope missing
cargo run -p yozist-server -- --ai-endpoint ... --ai-model ... ai-tag-generate --scope stale
```

### 並行アクセス前提

- テキスト: CRDT で自動マージ（`yrs` ベース）
  - 文字コードは UTF-8 / Shift-JIS / EUC-JP / UTF-16(LE/BE, BOM) 等を自動判定して取り込み（内部・blob は UTF-8 で統一）。元エンコーディングは保持し、ダウンロード／SMB read 時に元の形式へ再エンコードして返す。
- バイナリ: LWW（最終書き込み勝ち）
- メタデータ: 楽観ロック + SQLite WAL モード

### 細粒度の権限とパス発行

ユーザー／グループ単位で share / タグ / シリーズ / ファイル / フィルター各レベルに View/Read/Write/Admin を設定可能。期限付き共有 URL や動的 SMB share の発行も対応予定。

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
│   ├── yozist-ai/         AiProvider trait + vision タグ生成 + narashi 正規化
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

### user-permission-core 0.4.0 へのアップグレード時の注意

`user-permission-core` 0.4.0 で `User.id` が `i64` から UUID v7 に変更された。開発中の DB
のためデータ保全は行わず、アップグレード適用時は `data/auth.db` と `data/yozist.sqlite` を
削除してから起動すること（`files`/`commits`/`filters` の `*_user_id` 列は `INTEGER` から
`TEXT` へスキーマ変更されるため、旧DBのまま起動すると整合しない）。

## ライセンス

未定。
