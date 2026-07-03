// @ts-check
// core/binary ビュープラグイン（メタ比較・フォールバック・種別不一致の受け皿）。
(() => {
  const { fmtSize, imageInfo } = ViewRuntime.host;
ViewRuntime.registerView({
  kind: 'core/binary',
  label: 'バイナリ',
  diff: {
    modes: [],
    async render(container, oldModel, newModel, { stats }) {
      stats.textContent = '';
      // 開いた名前空間の設計に合わせ、種別ラベルは各ビューの登録情報（label）から
      // 引く。未登録の種別（このページで読み込んでいないプラグイン）だけ種別キー
      // そのものを見せる（誤って「バイナリ」と表示しない）。
      const kindLabel = k => {
        const v = ViewRuntime.views.get(k);
        return (v && v.label) || k;
      };
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
