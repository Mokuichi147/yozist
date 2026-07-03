// @ts-check
// 単一表示用の共有メディアビュープラグイン（image / video / audio / pdf）。
//
// ファイル詳細（file_detail）と単一コミット表示（file_commit）の双方が読み込み、
// 同じ描画ロジックを共有する（旧来の重複を解消）。content の取得方法はページごとに
// 異なる（現在ファイル or 特定コミット）ため、object URL の生成は ctx.objectUrlFor で
// 注入する。テキスト/不明・編集など各ページ固有の描画は各ページが別途登録する。
(() => {
  const { escapeHtml } = ViewRuntime.host;

  ViewRuntime.registerView({ kind: 'core/image', label: '画像', async mount(cont, ctx) {
    const url = await ctx.objectUrlFor(ctx.mime);
    cont.innerHTML = `<img src="${url}" alt="${escapeHtml(ctx.file.display_name)}"
      class="max-w-full max-h-[70vh] object-contain mx-auto block rounded bg-base-300/30">`;
  } });
  ViewRuntime.registerView({ kind: 'media/video', label: '動画', async mount(cont, ctx) {
    const url = await ctx.objectUrlFor(ctx.mime);
    cont.innerHTML = `<video src="${url}" controls
      class="max-w-full max-h-[70vh] mx-auto block rounded bg-black"></video>`;
  } });
  ViewRuntime.registerView({ kind: 'media/audio', label: '音声', async mount(cont, ctx) {
    const url = await ctx.objectUrlFor(ctx.mime);
    cont.innerHTML = `<audio src="${url}" controls class="w-full mt-2"></audio>`;
  } });
  ViewRuntime.registerView({ kind: 'doc/pdf', label: 'PDF', async mount(cont, ctx) {
    const url = await ctx.objectUrlFor(ctx.mime);
    cont.innerHTML = `<iframe src="${url}" title="${escapeHtml(ctx.file.display_name)}"
      class="w-full h-[70vh] rounded border border-base-300"></iframe>`;
  } });
})();
