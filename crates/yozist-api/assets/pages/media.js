// @ts-check
// メディアギャラリーページ（/ui/media）のロジック。
// 写真・動画のみを Justified Gallery と同じ行詰めアルゴリズムで表示する。
// /ui/pages/media.js で配信される。
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {
/** @typedef {{ id: string, display_name: string, mime?: string|null, size: number,
 *              created_at: *, updated_at: * }} MediaFile */
/** @typedef {{ file: MediaFile, kind: 'image'|'video', ratio: number,
 *              known: boolean, el: HTMLAnchorElement, mediaEl: HTMLImageElement|HTMLVideoElement|null,
 *              objectUrl: string|null, loading: boolean, failed: boolean }} GalleryItem */

// Justified Gallery 互換の既定値（miromannino/Justified-Gallery に近い設定）
const ROW_HEIGHT = 200;       // 目標行高 (px)
const GAP = 4;                // タイル間余白 (px) — JG の margins
const DEFAULT_RATIO = 4 / 3;  // 寸法未取得時の仮アスペクト比
const VIDEO_RATIO = 16 / 9;   // 動画プレースホルダの既定比
const LAST_ROW = 'nojustify'; // 'nojustify' | 'justify' | 'hide' | 'center' | 'right'
const MAX_CONCURRENT = 4;     // 認証付き content 取得の同時実行数

/** @type {MediaFile[]} */
let allMedia = [];
/** @type {GalleryItem[]} */
let items = [];
/** @type {'both'|'image'|'video'} */
let kindFilter = 'both';
/** @type {IntersectionObserver|null} */
let io = null;
let layoutTimer = 0;
/** @type {GalleryItem[]} */
const loadQueue = [];
let activeLoads = 0;

async function init() {
  const me = await requireAuth();
  if (!me) return;
  $('main').classList.remove('hidden');
  restoreFromUrl();
  updateKindButtons();
  await loadMedia();
  window.addEventListener('resize', onResize, { passive: true });
}

// ---- データ取得 ----

async function loadMedia() {
  $('gallery-status').classList.remove('hidden');
  $('gallery-status').textContent = '読み込み中…';
  $('gallery').replaceChildren();

  // システムタグ type:image / type:video で取得し、MIME でもフォールバック判定する。
  /** @type {MediaFile[]} */
  let images = [];
  /** @type {MediaFile[]} */
  let videos = [];
  try {
    [images, videos] = await Promise.all([
      json('/api/files/by-tags?tags=' + encodeURIComponent('type:image')).catch(() => []),
      json('/api/files/by-tags?tags=' + encodeURIComponent('type:video')).catch(() => []),
    ]);
  } catch (e) {
    $('gallery-status').textContent = '取得失敗';
    $('gallery-status').classList.add('text-error');
    return;
  }

  const byId = new Map();
  for (const f of [...images, ...videos]) {
    if (!isMediaFile(f)) continue;
    byId.set(f.id, f);
  }
  allMedia = [...byId.values()];
  applyFilters();
}

/**
 * @param {MediaFile} f
 * @returns {boolean}
 */
function isMediaFile(f) {
  const m = (f.mime || '').toLowerCase();
  if (m.startsWith('image/') || m.startsWith('video/')) return true;
  // MIME 未設定の古いデータ向けに拡張子でも判定
  const n = (f.display_name || '').toLowerCase();
  return /\.(jpe?g|png|gif|webp|avif|bmp|svg|heic|heif|tiff?|mp4|webm|mov|m4v|mkv|avi)$/.test(n);
}

/**
 * @param {MediaFile} f
 * @returns {'image'|'video'}
 */
function mediaKindOf(f) {
  const m = (f.mime || '').toLowerCase();
  if (m.startsWith('video/')) return 'video';
  if (m.startsWith('image/')) return 'image';
  const n = (f.display_name || '').toLowerCase();
  if (/\.(mp4|webm|mov|m4v|mkv|avi)$/.test(n)) return 'video';
  return 'image';
}

// ---- フィルタ / ソート ----

function sortVal() {
  return /** @type {HTMLSelectElement} */ ($('f-sort')).value || 'updated_desc';
}

/**
 * @param {'both'|'image'|'video'} k
 */
function setKind(k) {
  kindFilter = k;
  updateKindButtons();
  applyFilters();
}

function updateKindButtons() {
  for (const [id, k] of /** @type {const} */ ([
    ['f-kind-both', 'both'],
    ['f-kind-image', 'image'],
    ['f-kind-video', 'video'],
  ])) {
    const btn = $(id);
    if (!btn) continue;
    btn.classList.toggle('btn-active', kindFilter === k);
  }
}

function saveToUrl() {
  const params = new URLSearchParams();
  if (kindFilter !== 'both') params.set('kind', kindFilter);
  if (sortVal() !== 'updated_desc') params.set('sort', sortVal());
  const qs = params.toString();
  history.replaceState(null, '', qs ? '?' + qs : location.pathname);
}

function restoreFromUrl() {
  const p = new URLSearchParams(location.search);
  const k = p.get('kind');
  if (k === 'image' || k === 'video' || k === 'both') kindFilter = k;
  if (p.get('sort')) /** @type {HTMLSelectElement} */ ($('f-sort')).value = p.get('sort');
}

function applyFilters() {
  saveToUrl();
  const filtered = allMedia.filter(f => {
    const k = mediaKindOf(f);
    if (kindFilter === 'image') return k === 'image';
    if (kindFilter === 'video') return k === 'video';
    return true;
  });
  clientSort(filtered);
  rebuildItems(filtered);
}

/**
 * @param {MediaFile[]} files
 */
function clientSort(files) {
  const [key, dir] = sortVal().split('_');
  const m = dir === 'asc' ? 1 : -1;
  files.sort((a, b) => {
    let r;
    if (key === 'name') r = a.display_name.localeCompare(b.display_name, 'ja');
    else if (key === 'size') r = a.size - b.size;
    else if (key === 'created') r = fmtTs(a.created_at).localeCompare(fmtTs(b.created_at));
    else r = fmtTs(a.updated_at).localeCompare(fmtTs(b.updated_at));
    return r * m;
  });
}

// ---- ギャラリー構築 ----

/**
 * @param {MediaFile[]} files
 */
function rebuildItems(files) {
  // 既存 object URL を解放
  for (const it of items) revokeItem(it);
  if (io) { io.disconnect(); io = null; }
  loadQueue.length = 0;
  activeLoads = 0;

  items = files.map(f => createItem(f));
  $('media-count').textContent = `(${items.length})`;

  const status = $('gallery-status');
  if (items.length === 0) {
    status.classList.remove('hidden', 'text-error');
    status.textContent = kindFilter === 'image'
      ? '写真がありません。'
      : kindFilter === 'video'
        ? '動画がありません。'
        : '写真・動画がありません。「ファイル」からアップロードできます。';
    $('gallery').replaceChildren();
    return;
  }
  status.classList.add('hidden');

  // IntersectionObserver でビューポート付近のみ content を取得
  io = new IntersectionObserver(onIntersect, {
    root: null,
    rootMargin: '200px 0px',
    threshold: 0.01,
  });
  for (const it of items) io.observe(it.el);

  layout();
}

/**
 * @param {MediaFile} f
 * @returns {GalleryItem}
 */
function createItem(f) {
  const kind = mediaKindOf(f);
  // ?from=media を付けてファイル詳細へ渡し、詳細ページの「戻る」がここに戻れるようにする
  // （file_detail.js の setupBackLink 参照）。
  const a = el('a', {
    href: `/ui/files/${f.id}?from=media`,
    class: 'jg-item',
    title: f.display_name,
  }, [
    el('span', { class: 'jg-placeholder', 'aria-hidden': 'true' },
      kind === 'video' ? '🎬' : '🖼️'),
    kind === 'video' && el('span', { class: 'jg-badge' }, '動画'),
    el('span', { class: 'jg-label' }, f.display_name),
  ]);
  return {
    file: f,
    kind,
    ratio: kind === 'video' ? VIDEO_RATIO : DEFAULT_RATIO,
    known: false,
    el: a,
    mediaEl: null,
    objectUrl: null,
    loading: false,
    failed: false,
  };
}

/**
 * @param {GalleryItem} it
 */
function revokeItem(it) {
  if (it.objectUrl) {
    URL.revokeObjectURL(it.objectUrl);
    it.objectUrl = null;
  }
  it.mediaEl = null;
}

// ---- Justified Gallery アルゴリズム ----
//
// miromannino/Justified-Gallery と同じ考え方:
// 1. 各アイテムを目標行高 (rowHeight) にスケールした理想幅 = rowHeight * aspectRatio とする
// 2. 理想幅の合計 + gap がコンテナ幅を超えるまで行に詰める
// 3. 行が溢れたら、その行の高さを
//      h = (containerWidth - gaps) / Σ(aspectRatio)
//    に合わせ、各アイテム幅を h * aspectRatio にする（行全体がコンテナ幅にジャストフィット）
// 4. 最終行は lastRow オプションに従う（既定 nojustify = 目標高のまま左寄せ）

function layout() {
  const gallery = $('gallery');
  const containerWidth = gallery.clientWidth || gallery.parentElement?.clientWidth || 0;
  if (containerWidth <= 0 || items.length === 0) return;

  gallery.style.setProperty('--jg-gap', GAP + 'px');
  gallery.replaceChildren();

  /** @type {GalleryItem[][]} */
  const rows = [];
  /** @type {GalleryItem[]} */
  let row = [];
  let rowAspectSum = 0;

  for (const it of items) {
    const nextAspect = rowAspectSum + it.ratio;
    const nextCount = row.length + 1;
    // 行に nextCount 個入れたときの高さ（gap を除いた有効幅 / アスペクト比合計）
    const h = (containerWidth - GAP * (nextCount - 1)) / nextAspect;

    // 既存行があり、追加すると目標高を下回る（= 行が満杯）なら行を確定
    if (row.length > 0 && h < ROW_HEIGHT) {
      rows.push(row);
      row = [it];
      rowAspectSum = it.ratio;
    } else {
      row.push(it);
      rowAspectSum = nextAspect;
    }
  }
  if (row.length) rows.push(row);

  // 最終行の処理
  if (LAST_ROW === 'hide' && rows.length > 1) {
    rows.pop();
  }

  rows.forEach((r, idx) => {
    const isLast = idx === rows.length - 1;
    const justify = !isLast || LAST_ROW === 'justify';
    appendRow(gallery, r, containerWidth, justify, isLast);
  });
}

/**
 * @param {HTMLElement} gallery
 * @param {GalleryItem[]} row
 * @param {number} containerWidth
 * @param {boolean} justify  true ならコンテナ幅に合わせて高さを再計算
 * @param {boolean} isLast
 */
function appendRow(gallery, row, containerWidth, justify, isLast) {
  const aspectSum = row.reduce((s, it) => s + it.ratio, 0);
  const gaps = GAP * (row.length - 1);
  // justify: コンテナ幅にジャストフィットする高さ
  // nojustify: 目標行高のまま（最終行の左寄せ）
  const h = justify
    ? (containerWidth - gaps) / aspectSum
    : ROW_HEIGHT;

  // 整数誤差で 1px オーバーしないよう最終アイテム幅を調整
  /** @type {number[]} */
  const widths = row.map(it => h * it.ratio);
  if (justify) {
    const sum = widths.reduce((s, w) => s + w, 0);
    const target = containerWidth - gaps;
    const diff = target - sum;
    widths[widths.length - 1] += diff;
  }

  const rowEl = el('div', { class: 'jg-row' });
  rowEl.style.height = Math.round(h) + 'px';

  if (isLast && !justify) {
    if (LAST_ROW === 'center') rowEl.style.justifyContent = 'center';
    else if (LAST_ROW === 'right') rowEl.style.justifyContent = 'flex-end';
  }

  row.forEach((it, i) => {
    const w = Math.max(1, Math.round(widths[i]));
    it.el.style.width = w + 'px';
    it.el.style.height = Math.round(h) + 'px';
    rowEl.appendChild(it.el);
  });
  gallery.appendChild(rowEl);
}

function onResize() {
  clearTimeout(layoutTimer);
  layoutTimer = window.setTimeout(layout, 120);
}

// ---- 遅延読み込み（認証付き content → object URL） ----

/**
 * @param {IntersectionObserverEntry[]} entries
 */
function onIntersect(entries) {
  for (const e of entries) {
    if (!e.isIntersecting) continue;
    const it = items.find(x => x.el === e.target);
    // 動画は本体取得しない（プレースホルダ表示のみ）
    if (!it || it.kind === 'video' || it.loading || it.mediaEl || it.failed) continue;
    enqueueLoad(it);
  }
}

/**
 * @param {GalleryItem} it
 */
function enqueueLoad(it) {
  if (it.loading || it.mediaEl || it.failed) return;
  it.loading = true;
  loadQueue.push(it);
  pumpQueue();
}

function pumpQueue() {
  while (activeLoads < MAX_CONCURRENT && loadQueue.length > 0) {
    const it = loadQueue.shift();
    if (!it) break;
    activeLoads++;
    loadItem(it).finally(() => {
      activeLoads--;
      pumpQueue();
    });
  }
}

/**
 * @param {GalleryItem} it
 */
async function loadItem(it) {
  try {
    const r = await api(`/api/files/${it.file.id}/content`);
    if (!r.ok) throw new Error(await r.text().catch(() => r.statusText));
    const buf = await r.arrayBuffer();
    const mime = it.file.mime || 'image/jpeg';
    const url = URL.createObjectURL(new Blob([buf], { type: mime }));
    it.objectUrl = url;

    // loading:'lazy' はここでは付けない。この img はまだ DOM に接続されていない
    // （load 完了後に mountMedia で挿入する）ため、native lazy-load は交差判定できず
    // fetch/decode 自体が止まってしまう（IntersectionObserver 側で既に遅延読み込み
    // 制御しているので不要）。
    const img = el('img', {
      src: url,
      alt: it.file.display_name,
      decoding: 'async',
    });
    await waitMediaReady(img, 'load');
    if (img.naturalWidth > 0 && img.naturalHeight > 0) {
      updateRatio(it, img.naturalWidth / img.naturalHeight);
    }
    mountMedia(it, img);
  } catch (e) {
    it.failed = true;
    const ph = it.el.querySelector('.jg-placeholder');
    if (ph) ph.textContent = '⚠';
  } finally {
    it.loading = false;
  }
}

/**
 * @param {HTMLImageElement|HTMLVideoElement} node
 * @param {string} eventName
 * @returns {Promise<void>}
 */
function waitMediaReady(node, eventName) {
  return new Promise((resolve, reject) => {
    // 既に読み込み済みなら即解決
    if (node instanceof HTMLImageElement && node.complete && node.naturalWidth > 0) {
      resolve();
      return;
    }
    if (node instanceof HTMLVideoElement && node.readyState >= 1 && node.videoWidth > 0) {
      resolve();
      return;
    }
    const onOk = () => { cleanup(); resolve(); };
    const onErr = () => { cleanup(); reject(new Error('media load failed')); };
    const cleanup = () => {
      node.removeEventListener(eventName, onOk);
      node.removeEventListener('error', onErr);
    };
    node.addEventListener(eventName, onOk, { once: true });
    node.addEventListener('error', onErr, { once: true });
  });
}

/**
 * @param {GalleryItem} it
 * @param {number} ratio
 */
function updateRatio(it, ratio) {
  if (!(ratio > 0) || !isFinite(ratio)) return;
  const prev = it.ratio;
  it.ratio = ratio;
  it.known = true;
  // 比率が大きく変わったときだけ再レイアウト（チラつき抑制）
  if (Math.abs(prev - ratio) / prev > 0.05) {
    clearTimeout(layoutTimer);
    layoutTimer = window.setTimeout(layout, 50);
  }
}

/**
 * @param {GalleryItem} it
 * @param {HTMLImageElement|HTMLVideoElement} media
 */
function mountMedia(it, media) {
  it.mediaEl = media;
  const ph = it.el.querySelector('.jg-placeholder');
  if (ph) ph.remove();
  // ラベル・バッジの手前にメディアを挿入
  it.el.insertBefore(media, it.el.firstChild);
}

// テンプレートのインライン onclick/onchange から参照される関数を公開
Object.assign(window, {
  setKind, applyFilters,
});

init();
})();
