# ビュー／変換プラグイン基盤 設計

> ステータス: 設計提案（実装前）
> 対象: 単一ファイル表示（`/ui/files/:id`）と差分比較（`/ui/files/:id/compare`）の両方
> 前提: **あらゆるファイル形式**を対象とし、表示方式（ビュー）と形式→ビューの変換を
> どちらも後付けで無制限に追加できる基盤にする。

## 1. 背景と現状

### 現状はハードコード分岐

差分・表示はフロントのインライン JS に種別判定と描画が直書きされている。

| 箇所 | 種別判定 | 描画 |
|------|----------|------|
| `compare.html` | `commitKind()`（ヌルバイト／マジックナンバー sniff）→ `text` / `image` / `binary` | `renderTextDiff` / `renderImageDiff` / `renderMetaOnly` を if 分岐で直接呼ぶ |
| `file_detail.html` | `mediaKind()`（mime＋拡張子）→ `image`/`video`/`audio`/`pdf`/`text`/`unknown` | `renderContent()` 内の if 分岐 |

- 種別は**閉じた集合**で、増やすには各テンプレートの分岐を編集するしかない。
- 差分ロジックは**行（テキスト）前提**が骨格に埋まっている（`diffSegments` / LCS）。
  画像差分は別経路で特別扱いされており、第3の種別を足す入口が無い。
- 表示モードのツールバー（unified/split, 並べて/スライダー/…）も種別ごとに
  ハードコードされた DOM（`#text-views` / `#image-views`）。
- バックエンドは差分・表示に一切関与せず、生バイトを返すだけ。

### 既存のプラガブル規範（これを踏襲する）

本リポジトリのバックエンドは既に「**trait＋レジストリ**」のプラガブル設計を確立している。

- `yozist-versioning`: `CrdtFormat` trait ＋ `CrdtRegistry`（`detect(hint)` が真を返す最初の
  フォーマットを採用、無ければ `LwwFormat` にフォールバック）
- `yozist-storage`: `BlobStore` trait / `yozist-db`: `MetaStore` trait / `yozist-ai`: `AiProvider` trait
- 形式選択ヒント `yozist_core::FormatHint { extension, mime, first_bytes, display_name }`
  は既に存在し `CrdtFormat::detect` が利用している → **本基盤の変換層でも再利用する**。

本設計は、この既存規範をフロント（ビュー）とバックエンド（変換）の双方へ素直に延長する。

### 確定済みの方針

| 論点 | 決定 |
|------|------|
| 実行モデル | **ハイブリッド**: ビュー＝フロント JS、変換＝バックエンド Rust trait レジストリ（＋軽い変換はフロントでも可） |
| 配布・信頼 | **まず第一者組込み**。ただしレジストリ境界・契約を最初から綺麗に定義し、将来サードパーティ／サンドボックスを差し替え可能にする |
| 適用範囲 | **ビューア＋差分を統合**（同じビュー／変換プラグインが単一表示と比較の両方を担う） |

## 2. 設計原則

1. **全形式対応・固定タクソノミー禁止**
   ビュー種別（ViewKind）は `enum` ではなく**開いた文字列名前空間**。`text` も `image` も
   将来の任意形式も、すべて「登録された 1 プラグイン」に過ぎない。コア側に形式固有の
   `if` を一切持たせない。
2. **2 層に分離**
   - 変換層: `生バイト → ViewModel`（形式を解釈し正規化）
   - ビュー層: `ViewModel → 描画 / 差分`（種別ごとの見せ方）
   両層の唯一の接点は **ViewKind**（変換が産出する種別＝ビューが消費する種別）。
3. **普遍的フォールバック**
   どんな未知形式でも必ず何らかのビューに着地する（専用 → 汎用 → メタ/16進ダンプ）。
   「対応していない」は**機能不足**であって**エラーや行き止まりではない**。
4. **同じビュー原則**（README 準拠）
   単一表示と差分は同じプラグインの別メソッド。表示できる形式は比較もできるのが既定。
5. **差分はビューの責務**
   フレームワークは「行差分」「画素差分」等の前提を持たない。差分アルゴリズムは
   各ビューが所有し、フレームワークは ViewModel 2 つ・描画先・モード切替 UI・
   「差分なし」「種別不一致」のフォールバックだけを供給する。
6. **契約の安定 / 実装の差し替え自由**
   第一者組込みもサードパーティも**同じ `ViewConverter` / `ViewPlugin` 契約**を実装する。
   将来 WASM サンドボックスやマニフェスト方式のローダを足しても、契約は不変。

## 3. アーキテクチャ全体像

```
                         ┌──────────────── バックエンド (Rust) ────────────────┐
  commit 生バイト ──────▶│  ViewRegistry.resolve(FormatHint)                    │
   (BlobStore)           │     └ ViewConverter.detect()=true の最初の 1 つ      │
                         │        └ convert(bytes) ─▶ ViewModel(payload+kind)   │
                         └───────────────────────┬─────────────────────────────┘
                          GET …/commits/:cid/view │  (X-View-Kind, payload)
                                                  ▼
                         ┌──────────────── フロントエンド (JS) ────────────────┐
   （軽い形式は          │  view-runtime:                                       │
    フロント変換でも可）─▶│   1. resolveViewKind()  ← フロント converter or 上の API │
                         │   2. lookup ViewPlugin by kind                       │
                         │   3a. 単一表示  : plugin.mount(el, model)            │
                         │   3b. 比較      : plugin.diff.render(el, old, new)   │
                         │       └ 種別不一致 → メタ比較フォールバック          │
                         └──────────────────────────────────────────────────────┘
```

パイプラインは常に `bytes →(変換)→ ViewModel →(ビュー)→ 描画/差分`。
形式が増えても**追加されるのは変換 1 つ（＋必要ならビュー 1 つ）の登録だけ**で、
コアの分岐は一切増えない。

## 4. 中核となる型・契約

### 4.1 ViewKind（開いた名前空間）

ビュー種別は文字列 ID。名前衝突を避けるため `namespace/name` 規約を推奨
（既存 `CrdtFormat::format_id` の `"_/lww"` と同じ流儀）。

- 第一者の例: `core/text`, `core/image`, `core/binary`(=16進/メタ)
- 追加され得る例（あくまで一例、基盤側は何も知らない）:
  任意の `vendor/<形式>` を自由に定義してよい。

コア／レジストリは ViewKind の**意味を一切持たない**。単なる照合キー。

### 4.2 ViewModel（正規化済みデータ）

変換が産出し、ビューが消費する転送物。エンベロープで包む。

```jsonc
{
  "kind": "core/text",            // どのビューが描画するか
  "content_type": "text/plain",   // payload の MIME（任意）
  "payload": <bytes|json>,        // 種別固有の正規化データ
  "meta": { "...": "..." }        // 寸法・行数・要素数など表示補助（任意）
}
```

- `payload` の解釈は **ViewKind の取り決め**（ビューと変換の合意）であり、コアは不問。
- 巨大データはバイナリ転送（既存の content と同様 Range / ストリーム対応の余地）。

### 4.3 変換プラグイン（Rust: `ViewConverter`）

`CrdtFormat` と同型の trait。`FormatHint` を再利用する。

```rust
// crates/yozist-view/src/lib.rs（新規クレート yozist-view を想定）
#[async_trait]
pub trait ViewConverter: Send + Sync {
    /// 一意な変換 ID（ログ・診断用）。例 "core/text".
    fn converter_id(&self) -> &'static str;

    /// この変換が対象とするか。CrdtRegistry と同じく先勝ち。
    fn detect(&self, hint: &FormatHint) -> bool;

    /// 産出する ViewKind。
    fn target_view(&self) -> &'static str;

    /// 生バイト → ViewModel。重い処理（CAD→メッシュ等）はここに集約。
    async fn convert(&self, bytes: &[u8], hint: &FormatHint)
        -> Result<ViewModel, ViewError>;

    /// 恒等変換か（payload が入力バイトそのもの）。
    /// 真なら API はゼロコピー的に生バイトを流用でき、キャッシュも不要。
    fn is_passthrough(&self) -> bool { false }
}
```

```rust
pub struct ViewRegistry {
    converters: Vec<Arc<dyn ViewConverter>>,
    fallback: Arc<BinaryConverter>, // 常に detect=true、core/binary を産出
}
impl ViewRegistry {
    pub fn with_defaults() -> Self { /* text, image, binary を register */ }
    pub fn register(&mut self, c: Arc<dyn ViewConverter>) { /* ... */ }
    pub fn resolve(&self, hint: &FormatHint) -> Arc<dyn ViewConverter> { /* 先勝ち→fallback */ }
}
```

`CrdtRegistry` と構造を意図的に揃える（学習コスト最小・一貫性）。

### 4.4 ビュープラグイン（JS: `ViewPlugin`）

ビュー描画はブラウザ（WebGL/Canvas/DOM）が担うためフロント。

```js
registerView({
  kind: 'core/text',

  // 重い依存（three.js 等）はビューが使われる時だけ動的 import。
  async ensureDeps() { /* optional */ },

  // 単一ファイル表示。container に描画し、後始末用に破棄関数を返してよい。
  async mount(container, model, ctx) { /* ... */ return { destroy() {} }; },

  // 差分。modes はツールバーをフレームワークが汎用生成する。
  diff: {
    modes: [ { id: 'unified', label: 'unified' }, { id: 'split', label: 'split' } ],
    async render(container, oldModel, newModel, { mode, ctx }) { /* ... */ },
    // 完全一致時の見せ方（任意。既定は「差分はありません」）
    onEqual(container, model) { /* optional */ },
  },

  // 任意の能力宣言（編集可否・サイズ上限・バックエンド変換要否など）
  capabilities: { editable: false },
});
```

軽い形式はフロント変換も可能（バックエンド往復を省く）。

```js
registerConverter({
  converterId: 'core/text',
  targetView: 'core/text',
  detect({ name, ext, mime, headBytes }) { /* ... */ },
  async convert(bytes, ctx) { return { kind: 'core/text', payload: text, meta: {...} }; },
});
```

## 5. バックエンド REST

`yozist-api` に 2 エンドポイントを追加（権限・キャッシュは既存 content と同流儀）。

| メソッド・パス | 役割 |
|---|---|
| `GET /api/files/:id/commits/:cid/view-kind` | **軽量プローブ**。`FormatHint` から解決した ViewKind だけを返す（巨大 payload を取得せずビューとツールバーを決められる）。 |
| `GET /api/files/:id/commits/:cid/view` | 変換実行。`X-View-Kind` ヘッダ＋ `content_type` の payload を返す。`is_passthrough` の変換なら生 content を直返し。 |

- 変換結果は重い場合があるためキャッシュ（既存「展開済み content キャッシュ」の隣に
  `(commit_id, converter_id)` キーで）。
- `view` は単一表示・比較の**両方**が使う（比較は 2 コミット分を取得して同じ ViewModel を
  ビューの `diff.render` へ渡すだけ）。

## 6. フロントの解決フロー（viewer / compare 共通）

```js
async function resolveModel(fileId, commitId, hint, bytesMaybe) {
  // 1) フロント変換で解決できるか（軽量・往復不要）
  const fc = frontConverters.find(c => c.detect(hint));
  if (fc) return fc.convert(bytesMaybe ?? await fetchBytes(commitId), hint);

  // 2) バックエンド変換へ委譲
  const kind = await getViewKind(fileId, commitId);     // 軽量プローブ
  const { payload, contentType } = await getView(fileId, commitId);
  return { kind, content_type: contentType, payload };
}

// 単一表示
const model = await resolveModel(...);
const plugin = views.get(model.kind) ?? views.get('core/binary');
await plugin.ensureDeps?.();
await plugin.mount(el, model, ctx);

// 比較
const [o, n] = await Promise.all([resolveModel(base), resolveModel(comp)]);
if (o.kind === n.kind && views.get(o.kind)?.diff) {
  await views.get(o.kind).diff.render(el, o, n, { mode, ctx });
} else {
  renderMetaCompare(el, o, n);   // 種別不一致は現状同様メタ比較へフォールバック
}
```

ツールバー（表示モード切替）は `plugin.diff.modes` から**汎用生成**する。
現在の `#text-views` / `#image-views` のハードコード DOM は廃止。

## 7. 「あらゆる形式」をどう取りこぼさず吸収するか

解決は必ず段階的フォールバックで着地する（行き止まりを作らない）。

```
専用変換（detect 一致） → 汎用変換（text 等の広域 detect） → core/binary（常に一致）
```

- `core/binary` ビューは 16 進ダンプ／メタ情報（種別・サイズ・寸法）で**必ず**表示・比較できる。
  → 未知バイナリでも「サイズが変わった」程度の比較は常に成立する。
- 新形式の対応＝**変換を 1 つ登録するだけ**。既存 ViewKind に載せられるなら（例: 何らかの
  表形式を `table` 系ビューへ写像）ビュー追加すら不要。新しい見せ方が要るときだけ
  ビューを 1 つ足す。独自形式も同様に「変換＋（必要なら）ビュー」の追加のみで閉じる。
- **形式非依存の比較**という強みが生まれる: 例として旧 `A形式` / 新 `B形式` でも、両者が
  同じ ViewKind へ変換されれば**そのまま比較可能**になる（現状は種別が違えば即メタ比較）。

## 8. 既存コードの移行（挙動を変えない検証）

第一者プラグイン化を**現状の挙動を保ったまま**行い、契約の妥当性を実証する。

1. `core/text` ビュー ← `compare.html` の `renderTextDiff`/`diffSegments`/LCS/unified・split、
   および `file_detail.html` のテキスト表示（全文 / 仮想スクロール / 部分編集）を移設。
2. `core/image` ビュー ← `renderImageDiff` と 4 モード（並べて/スライダー/重ね/差分）、
   および画像/動画/音声/PDF プレビュー。
3. `core/binary` ビュー ← `renderMetaOnly` / `renderBinary`（フォールバック）。
4. `compare.html` / `file_detail.html` を **view-runtime 駆動**へ置換し、種別ごとの分岐を撤去。
5. base.html に共有 `view-runtime`（`registerView`/`registerConverter`/解決ループ／汎用ツールバー）
   を追加（既存の `$`/`api`/`json`/`escapeHtml`/`decodeBytes` と同じく全ページ共通スクリプトとして）。

この時点で**新機能ゼロ・挙動同一**だが、以降は形式追加が登録のみで完結する。

## 9. 将来拡張（サードパーティ／サンドボックス）

契約を変えずにローダだけ差し替えられるよう、最初から境界を引いておく。

- 変換: `ViewConverter` を WASM（`wasmtime`/`extism` 等）で実装し、`ViewRegistry` に
  動的登録。マニフェストで対象拡張子・能力・リソース上限を宣言。
- ビュー: `registerView` を満たす JS バンドルをサンドボックス（iframe/worker）に隔離して
  ロード。`ctx` 経由で許可された API（fetch ラッパ等）だけを渡す。
- いずれも本設計の `ViewConverter` / `ViewPlugin` 契約をそのまま実装するため、第一者
  プラグインのコードは変更不要。

## 10. 段階的な実装計画

1. **基盤の骨格**: `yozist-view` クレート（`ViewKind`/`ViewModel`/`ViewConverter`/`ViewRegistry`、
   `BinaryConverter` フォールバック）＋ `view-kind`・`view` REST＋キャッシュ。
2. **フロント runtime**: base.html に `registerView`/`registerConverter`/解決ループ／汎用ツールバー。
3. **第一者プラグイン移植**（§8）。compare/detail を runtime 駆動へ。挙動同一を確認。
4. **新形式の実証**: 任意の 1 形式を「変換＋ビュー」追加だけで載せ、基盤の拡張性を検証。
5. **将来拡張**（§9）は別フェーズ。

## 11. 実装状況（本ブランチ）

| 項目 | 状態 |
|------|------|
| `yozist-view` クレート（`ViewKind`/`ViewModel`/`ViewConverter`/`ViewRegistry`＋Text/Image/Binary 変換、検出ヘルパ、ユニットテスト） | ✅ 実装・テスト済 |
| REST `GET …/commits/:cid/view`（`X-View-Kind` 付き）／`…/view-kind`（軽量プローブ） | ✅ 実装（`ApiState.view_registry` 経由でプラガブル） |
| フロント view-runtime（`registerView`/`registerConverter`/`resolveModel`／汎用モードツールバー） | ✅ `compare.html` に実装 |
| 第一者ビュープラグイン `core/text`・`core/image`・`core/binary`（既存の行差分・画像4モード・メタ比較を移植） | ✅ 挙動を保って移植 |
| 比較ページのオーケストレーション（2 コミット解決 → 同種は専用差分／異種はメタ比較） | ✅ ハードコード分岐を撤去 |
| 単一表示（`file_detail.html`）の runtime 統合 | ✅ 描画ディスパッチをビュープラグイン（`mount`）化。画像/動画/音声/PDF を `core/image`・`media/video`・`media/audio`・`doc/pdf` プラグインへ移植。テキストの仮想スクロール／巨大ファイル編集は温存（`core/text` の mount から既存 `renderTextContent` を呼ぶ）。ブラウザ実機で検証済 |
| view-runtime の `base.html` への抽出（全ページ共有化） | ✅ 純粋レジストリを `base.html` へ。compare(diff) と file_detail(mount) が同一ランタイムを共有 |
| 重い形式のバックエンド変換の実例（フロントから `/view` へ委譲する経路） | ⬜ 口は用意済み（`resolveModel` の差し替え点）。実形式は未追加 |

> 注: バックエンドの全体ビルド（`yozist-server`）は vendor の `smb-server` が rustc 1.95 を要求する
> 既存制約のため本環境では通らない。本変更に関係する `yozist-view` / `yozist-api` は
> ビルド・テストとも green。

## 12. まとめ

- 現状は**プラグイン方式ではない**（フロントのハードコード分岐、行差分前提）。
- 本設計は既存の `CrdtRegistry`/`CrdtFormat` 規範を踏襲し、**変換層（Rust）×ビュー層（JS）**の
  2 層レジストリへ作り替える。
- ViewKind を開いた名前空間とし、**あらゆる形式**を「変換＋（必要なら）ビューの登録」だけで
  追加できる。コアに形式固有分岐を持たせない。
- 普遍的フォールバックにより未知形式も必ず表示・比較に着地する。
- 第一者組込みから始め、同一契約のままサードパーティ／サンドボックスへ拡張できる。
