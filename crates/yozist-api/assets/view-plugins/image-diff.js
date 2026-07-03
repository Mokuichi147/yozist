// @ts-check
// core/image ビュープラグイン（並べて / スライダー / 重ね合わせ / 差分）。
ViewRuntime.registerView((() => {
  const { $, fmtSize, bytesEqual, imageInfo } = ViewRuntime.host;
  function imgStageStyle(o, n) {
    const w = Math.max(o.width, n.width) || 800;
    const h = Math.max(o.height, n.height) || 600;
    return `width:${w}px;aspect-ratio:${w}/${h};`;
  }
  function renderImgSide(cont, o, n) {
    cont.innerHTML = `
      <div class="grid grid-cols-1 md:grid-cols-2 gap-3 p-3">
        <div class="img-checker rounded p-1">
          <img src="${o.url}" class="img-side-img" alt="旧">
        </div>
        <div class="img-checker rounded p-1">
          <img src="${n.url}" class="img-side-img" alt="新">
        </div>
      </div>`;
  }
  function renderImgSwipe(cont, o, n) {
    cont.innerHTML = `
      <div class="p-3 space-y-2">
        <div id="img-stage" class="img-stage img-swipe img-checker rounded" style="${imgStageStyle(o, n)}">
          <img id="img-old" src="${o.url}" alt="旧">
          <img id="img-new" src="${n.url}" alt="新">
          <div id="img-handle" class="img-handle" style="left:50%;"></div>
          <span class="img-tag diff-del" style="left:0.25rem;">旧</span>
          <span class="img-tag diff-add" style="right:0.25rem;">新</span>
        </div>
        <p class="text-xs opacity-60">境界線をドラッグして旧/新を切り替えます。</p>
      </div>`;
    const stage = $('img-stage'), oldEl = $('img-old'), newEl = $('img-new'), handle = $('img-handle');
    const apply = v => {
      v = Math.max(0, Math.min(100, v));
      // 旧と新を重ねず境界線で左右に切り分ける（透過画像で下が透けるのを防ぐ）。
      oldEl.style.clipPath = `inset(0 ${100 - v}% 0 0)`;
      newEl.style.clipPath = `inset(0 0 0 ${v}%)`;
      handle.style.left = v + '%';
    };
    const posFromEvent = e => {
      const rect = stage.getBoundingClientRect();
      return ((e.clientX - rect.left) / rect.width) * 100;
    };
    let dragging = false;
    stage.addEventListener('pointerdown', e => {
      dragging = true;
      stage.setPointerCapture(e.pointerId);
      apply(posFromEvent(e));
      e.preventDefault();
    });
    stage.addEventListener('pointermove', e => { if (dragging) apply(posFromEvent(e)); });
    const stop = e => {
      if (!dragging) return;
      dragging = false;
      try { stage.releasePointerCapture(e.pointerId); } catch (_) {}
    };
    stage.addEventListener('pointerup', stop);
    stage.addEventListener('pointercancel', stop);
    apply(50);
  }
  function renderImgOnion(cont, o, n) {
    cont.innerHTML = `
      <div class="p-3 space-y-2">
        <div class="img-stage img-checker rounded" style="${imgStageStyle(o, n)}">
          <img id="img-old" src="${o.url}" alt="旧" style="opacity:0.5;">
          <img id="img-new" src="${n.url}" alt="新" style="opacity:0.5;">
        </div>
        <input id="img-range" type="range" min="0" max="100" value="50" class="range range-xs range-primary">
        <div class="flex justify-between text-xs">
          <span class="diff-del px-1 rounded">旧</span>
          <span class="diff-add px-1 rounded">新</span>
        </div>
      </div>`;
    const range = $('img-range'), oldEl = $('img-old'), newEl = $('img-new');
    // クロスフェード: 新を v、旧を 1-v に。一方を振り切れば他方が消える。
    range.oninput = () => {
      const v = +range.value / 100;
      newEl.style.opacity = v.toFixed(2);
      oldEl.style.opacity = (1 - v).toFixed(2);
    };
  }
  function renderImgDiff(cont, o, n) {
    // 黒背景に新画像を mix-blend-mode:difference で重ねる。変化した画素だけが浮かぶ。
    cont.innerHTML = `
      <div class="p-3 space-y-2">
        <div class="img-stage rounded" style="${imgStageStyle(o, n)}background:#000;">
          <img src="${o.url}" alt="旧">
          <img src="${n.url}" alt="新" style="mix-blend-mode:difference;">
        </div>
        <p class="text-xs opacity-60">明るい部分が変化した画素です（一致部分は黒）。</p>
      </div>`;
  }

  let cache = { o: null, n: null };
  async function ensure(oldModel, newModel) {
    if (cache.o === oldModel && cache.n === newModel) return cache;
    const oInfo = await imageInfo(oldModel.id, oldModel.payload, oldModel.contentType);
    const nInfo = await imageInfo(newModel.id, newModel.payload, newModel.contentType);
    cache = { o: oldModel, n: newModel, oInfo, nInfo };
    return cache;
  }

  return {
    kind: 'core/image',
    label: '画像',
    diff: {
      modes: [
        { id: 'side', label: '並べて' }, { id: 'swipe', label: 'スライダー' },
        { id: 'onion', label: '重ね合わせ' }, { id: 'diff', label: '差分' },
      ],
      async render(container, oldModel, newModel, { mode, stats }) {
        const { oInfo, nInfo } = await ensure(oldModel, newModel);
        const same = bytesEqual(oInfo.bytes, nInfo.bytes);
        const dim = im => im.width ? `${im.width}×${im.height}` : '—';
        if (same) {
          stats.innerHTML = '<span class="opacity-60">差分はありません</span>';
          container.innerHTML = `<div class="img-checker p-3 rounded">` +
            `<img src="${oInfo.url}" class="img-side-img" alt=""></div>`;
          return;
        }
        stats.innerHTML =
          `<span class="opacity-60">寸法</span> ` +
          `<span class="font-mono">${dim(oInfo)}</span> → <span class="font-mono">${dim(nInfo)}</span>` +
          `<span class="opacity-60 ml-3">サイズ</span> ` +
          `<span class="text-error font-semibold">${fmtSize(oInfo.size)}</span> → ` +
          `<span class="text-success font-semibold">${fmtSize(nInfo.size)}</span>`;
        if (mode === 'side') renderImgSide(container, oInfo, nInfo);
        else if (mode === 'swipe') renderImgSwipe(container, oInfo, nInfo);
        else if (mode === 'onion') renderImgOnion(container, oInfo, nInfo);
        else renderImgDiff(container, oInfo, nInfo);
      },
    },
  };
})());
