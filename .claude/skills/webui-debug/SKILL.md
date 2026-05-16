---
name: webui-debug
description: yozist の WebUI (`/ui`, Tailwind v4 + daisyUI v5) をプレビューサーバ経由で起動し、ブラウザコンソール / DOM / 計算済みスタイルを実際に確認しながらデバッグする手順。CSS が効かない・レイアウトが崩れる・JS エラーが出るなど、ユーザーが「実際に見て確認してほしい」と要求した時に使う。
---

# WebUI デバッグ手順

yozist の WebUI は `crates/yozist-api/src/ui/index.html` を `include_str!` で埋め込み、`/ui` で配信する単一 SPA。Rust 側を再ビルドしないと変更が反映されない点に注意。

## 1. プレビューサーバを起動する

`mcp__Claude_Preview__preview_start` を使って `yozist-server` を起動する。設定は `.claude/launch.json`:

```json
{
  "version": "0.0.1",
  "configurations": [
    {
      "name": "yozist-server",
      "runtimeExecutable": "cargo",
      "runtimeArgs": [
        "run", "-p", "yozist-server", "--",
        "--smb-listen", "",
        "serve"
      ],
      "port": 7878
    }
  ]
}
```

ポイント:
- `--smb-listen ""` で SMB を無効化（権限・ポート競合の回避）
- `--smb-listen` は `serve` サブコマンドの**前**に置く（親 CLI の引数）
- データディレクトリは `./data`、`yozist.sqlite` は既にマイグレ済みであることを確認 (`ls data/`)

UI のパスは `/ui`（**末尾スラッシュなし**。`/ui/` は 404）。

## 2. ブラウザで開く

```
mcp__Claude_Preview__preview_eval (serverId, "location.href = '/ui'")
```

その後 3〜4 秒待つ（CDN 読込 + Tailwind Browser CDN の処理待ち）。

## 3. ブラウザの状態を見る

`preview_screenshot` で外観確認、`preview_console_logs` で JS エラー確認。

DOM / 計算済みスタイルは `preview_eval` で取得:

```js
JSON.stringify({
  navbar_display: getComputedStyle(document.querySelector('.navbar')).display,
  navbar_align: getComputedStyle(document.querySelector('.navbar')).alignItems,
  flex1_grow: getComputedStyle(document.querySelector('.navbar > div.flex-1')).flexGrow,
  main_max_width: getComputedStyle(document.getElementById('main')).maxWidth,
  body_bg: getComputedStyle(document.body).backgroundColor,
  theme: document.documentElement.getAttribute('data-theme'),
})
```

- `.navbar` の `display` が `block` のまま → daisyUI の CSS が当たっていない
- `.flex-1` の `flexGrow` が `0` → Tailwind ユーティリティが当たっていない

注入済みの全スタイルシートを列挙して原因を切り分ける:

```js
[...document.querySelectorAll('style, link[rel=stylesheet]')].map(s => ({
  tag: s.tagName, type: s.type, href: s.href,
  len: (s.textContent || '').length,
  sample: (s.textContent || '').slice(0, 80),
}))
```

Tailwind v4 Browser CDN は `<style>` タグを動的に挿入するので、これで実際に何が読まれているかが分かる。

## 4. CDN URL を疎通確認

```js
fetch('https://cdn.jsdelivr.net/npm/daisyui@5/daisyui.css', { method: 'HEAD' })
  .then(r => r.status + ' ' + r.headers.get('content-type'))
```

200 + `text/css` であれば配信されている。

## 5. 変更を反映する

`crates/yozist-api/src/ui/index.html` を編集した後は、必ずプレビューサーバを **stop → start で再起動**する（`include_str!` のため再ビルドが必要）。

```
preview_stop(serverId) → preview_start("yozist-server")
```

## 既知の落とし穴

- **`@plugin "daisyui"` は Tailwind v4 Browser CDN ではサイレントに無視される**。daisyUI コンポーネント (`.navbar`, `.btn`, `.card` 等) を使うなら `<link href="https://cdn.jsdelivr.net/npm/daisyui@5/daisyui.css">` と `themes.css` を直接読み込む必要がある。
- daisyUI の `data-theme="auto"` は無効値。`prefers-color-scheme` を読んで JS で `dark` / `light` を明示的にセットする。
- `<style type="text/tailwindcss">` ブロックは Tailwind CDN によって処理されるが、`@plugin` は機能しない。`@apply` は使えるが、確実性のため重要なクラスは純 CSS で書くのが安全。
- daisyUI `.menu` を `<ul>` に付けるとき、`block` ユーティリティを併用すると `flex-direction: column` を上書きしてレイアウトが崩れる。
- `.hidden` は Tailwind 読込前は効かないので、`<style>` で `.hidden { display: none !important; }` を head 先頭にインライン定義し、初期表示のフラッシュを防ぐ。
