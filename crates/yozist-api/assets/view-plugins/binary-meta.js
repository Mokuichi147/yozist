// core/binary ビュープラグイン（メタ比較・フォールバック・種別不一致の受け皿）。
(() => {
  const { fmtSize, imageInfo } = ViewRuntime.host;
ViewRuntime.registerView({
  kind: 'core/binary',
  diff: {
    modes: [],
    async render(container, oldModel, newModel, { stats }) {
      stats.textContent = '';
      const kindLabel = k =>
        k === 'core/image' ? '画像' : k === 'core/text' ? 'テキスト' : 'バイナリ';
      const cell = async (model) => {
        let dim = '';
        if (model.kind === 'core/image') {
          const info = await imageInfo(model.id, model.payload, model.contentType);
          if (info.width) dim =
            `<div><dt class="opacity-50">寸法</dt><dd class="font-mono">${info.width}×${info.height}</dd></div>`;
        }
        return `<dl class="space-y-2">
          <div><dt class="opacity-50">種別</dt><dd>${kindLabel(model.kind)}</dd></div>
          <div><dt class="opacity-50">サイズ</dt><dd class="font-mono">${fmtSize(model.payload.length)}</dd></div>
          ${dim}
        </dl>`;
      };
      const [oCell, nCell] = await Promise.all([cell(oldModel), cell(newModel)]);
      container.innerHTML = `
        <div class="grid grid-cols-2 gap-4 p-4 text-xs">
          <div><span class="diff-del px-1 rounded inline-block mb-2 font-semibold">旧</span>${oCell}</div>
          <div><span class="diff-add px-1 rounded inline-block mb-2 font-semibold">新</span>${nCell}</div>
        </div>`;
    },
  },
});
})();
