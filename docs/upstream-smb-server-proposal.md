# Proposal: paltaio/rust-smb-server に外部 ShareBackend 実装の支援を追加

対象: [paltaio/rust-smb-server](https://github.com/paltaio/rust-smb-server) v0.4.x

## 要約

外部クレートから `ShareBackend` を実装できるようにするための小さな
変更 2 点を提案する:

1. **公開 API として必要な型を再エクスポート**
   - `SmbError`, `SmbResult` (現状 `mod error` が private)
   - `SmbPath` (現状 `mod path` が private)
   - `BackendCapabilities`, `FileTimes` (現状 `pub` だが `mod backend` が private なので外部から参照不可)

2. **`ShareBackend` トレイトに認証済み Identity を渡す**
   - `open` / `unlink` / `rename` の各メソッドに `identity: &Identity` パラメータを追加
   - dispatch 側 (`handlers/create.rs`, `handlers/close.rs`, `handlers/set_info.rs`) で
     セッションから identity を取り出し backend に渡すよう変更

## 動機

私たちは [yozist](https://github.com/mokuichi147/yozist) で paltaio/rust-smb-server
を採用し、以下の機能を持つ独自の `ShareBackend` を実装している:

- **タグ／シリーズ中心の仮想ファイルシステム** — 階層パスをタグの AND
  条件として解釈し、メタデータ DB から動的にディレクトリビューを合成
- **CRDT バージョニング** — テキストファイルは yrs ベースの並行マージ
- **細粒度 ACL** — ユーザー／グループ単位、File/Tag/Series 各レベルで
  READ/WRITE/ADMIN を強制

このとき以下の問題に遭遇した:

### 1. 型が再エクスポートされていない

`ShareBackend` トレイトのメソッドは `SmbResult<T>` を返し、
パラメータに `&SmbPath` を受ける。`BackendCapabilities` を返す
`capabilities()` も必要。しかしこれらの型は `lib.rs` で
`pub use` されていないため、外部クレートから `ShareBackend` を
**そもそも実装できない**。

```
mod error;  // private — SmbError, SmbResult を含む
mod path;   // private — SmbPath を含む
mod backend; // private (BackendCapabilities, FileTimes を含む) — DirEntry/FileInfo/Handle/OpenIntent/OpenOptions/ShareBackend のみ pub use 済
```

### 2. SMB セッションのユーザー情報が backend に届かない

`ShareBackend::open` の現行シグネチャ:

```rust
async fn open(&self, path: &SmbPath, opts: OpenOptions) -> SmbResult<Box<dyn Handle>>;
```

ここに**誰のリクエストか**の情報が無い。`smb-server` 内部では
`Session.identity: Identity` を持っているが backend には渡されない。

このため、ユーザー別 ACL を強制する backend は実装不可能。
yozist のように REST/WebUI と統合した認可システムを SMB 経路にも
適用したいケースでは致命的。

## 提案する変更

### 1. 公開 API の追加 (`src/lib.rs`)

```diff
 pub use backend::{
-    DirEntry, FileInfo, Handle, OpenIntent, OpenOptions, ShareBackend
+    BackendCapabilities, DirEntry, FileInfo, FileTimes, Handle, OpenIntent,
+    OpenOptions, ShareBackend
 };
 pub use builder::{Access, Share};
+pub use error::{SmbError, SmbResult};
 #[cfg(feature = "localfs")]
 pub use fs::LocalFsBackend;
+pub use path::SmbPath;
```

### 2. `ShareBackend` トレイトに `&Identity` を追加 (`src/backend.rs`)

```diff
+use crate::proto::auth::ntlm::Identity;

 #[async_trait]
 pub trait ShareBackend: Send + Sync + 'static {
-    async fn open(&self, path: &SmbPath, opts: OpenOptions) -> SmbResult<Box<dyn Handle>>;
-    async fn unlink(&self, path: &SmbPath) -> SmbResult<()>;
-    async fn rename(&self, from: &SmbPath, to: &SmbPath) -> SmbResult<()>;
+    async fn open(
+        &self,
+        identity: &Identity,
+        path: &SmbPath,
+        opts: OpenOptions,
+    ) -> SmbResult<Box<dyn Handle>>;
+    async fn unlink(&self, identity: &Identity, path: &SmbPath) -> SmbResult<()>;
+    async fn rename(
+        &self,
+        identity: &Identity,
+        from: &SmbPath,
+        to: &SmbPath,
+    ) -> SmbResult<()>;
     fn capabilities(&self) -> BackendCapabilities;
 }
```

### 3. dispatch 側の更新

`handlers/shared.rs` に identity 取得ヘルパを追加:

```rust
pub async fn lookup_identity(
    conn: &Arc<Connection>,
    hdr: &Smb2Header,
) -> Result<Identity, u32> {
    let sess_arc = lookup_session(conn, hdr.session_id).await?;
    Ok(sess_arc.read().await.identity.clone())
}
```

`handlers/{create,close,set_info}.rs` で identity を解決して backend に渡す。

`LocalFsBackend` と `NotSupportedBackend` の trait impl も新シグネチャに更新
（identity を無視する形で互換性維持）。

## 後方互換性

トレイトメソッドのシグネチャが変わるため、**v0.5.0 で導入する破壊的変更**
として扱うのが妥当。`LocalFsBackend` を利用するだけのユーザーには影響が
無い（trait シグネチャを直接書いていない限り）。

外部 backend 実装ユーザー (本提案の主受益者) は signature 変更が必要だが、
未使用なら `_identity: &Identity` でシグネチャを満たすだけで済む。

## 完成形のパッチ

[`docs/upstream-smb-server.patch`](upstream-smb-server.patch) に
変更全体を unified diff 形式で含めた (約 350 行)。

## 関連プロジェクト

このパッチは [yozist](https://github.com/mokuichi147/yozist) の
`vendor/smb-server/` に取り込まれ、4 つのカスタム ShareBackend
(`AllBackend`, `TagsBackend`, `SeriesBackend`, `QueriesBackend`) で
本番稼働している。
