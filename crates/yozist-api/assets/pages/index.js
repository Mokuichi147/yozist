// @ts-check
// ダッシュボードページ（/ui）のロジック。index.html のインライン <script> から切り出した静的ファイル（issue #50）。
// /ui/pages/index.js で配信される。
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {
async function init() {
  const me = await requireAuth();
  if (!me) return;
  $('main').classList.remove('hidden');
  loadStorage();
}

// ===== ストレージ使用量（種別の内訳 + バージョン別の内訳）=====
// /api/stats/storage が返すサイズはオンディスク実バイト数（圧縮・重複排除後）。
// ドーナツ = 最新版のファイル種別内訳。その下に「最新版を表示する容量」と
// 「過去バージョンを維持する容量」のバージョン別内訳を補足表示する。

// ファイル種別の表示順と色（ライト/ダーク両テーマで視認できる固定色）。
const STORAGE_SEGMENTS = [
  ['画像',            '#3b82f6'],
  ['動画',            '#8b5cf6'],
  ['音声',            '#ec4899'],
  ['ドキュメント',    '#ef4444'],
  ['テキスト/コード', '#10b981'],
  ['アーカイブ',      '#f59e0b'],
  ['その他',          '#9ca3af'],
];

async function loadStorage() {
  let stats;
  try {
    stats = await json('/api/stats/storage');
  } catch (_) {
    $('storage-loading').textContent = '使用量を取得できませんでした';
    return;
  }
  $('storage-loading').classList.add('hidden');

  const currentBytes = stats.current_bytes || 0;
  const historyBytes = stats.history_bytes || 0;
  const grand = currentBytes + historyBytes;
  if (stats.file_count === 0 || grand === 0) {
    $('storage-empty').classList.remove('hidden');
    return;
  }

  // ドーナツ + レジェンド: 最新版のファイル種別内訳（分母は最新版合計）。
  // 凡例・ドーナツともサイズの降順で並べる。
  const bytes = {};
  for (const c of stats.categories || []) bytes[c.category] = c.bytes;
  const segments = STORAGE_SEGMENTS
    .map(([seg, color]) => ({ seg, color, v: bytes[seg] || 0 }))
    .filter(s => s.v > 0)
    .sort((a, b) => b.v - a.v);
  const stops = [];
  const legend = $('storage-legend');
  legend.innerHTML = '';
  let acc = 0;
  for (const { seg, color, v } of segments) {
    const start = currentBytes ? acc / currentBytes * 100 : 0;
    acc += v;
    const end = currentBytes ? acc / currentBytes * 100 : 0;
    stops.push(`${color} ${start}% ${end}%`);

    const pct = currentBytes ? v / currentBytes * 100 : 0;
    legend.appendChild(el('li', { class: 'flex items-center gap-2' }, [
      el('span', { class: 'inline-block w-3 h-3 rounded-sm shrink-0', style: `background:${color}` }),
      el('span', { class: 'flex-1 min-w-0 truncate' }, seg),
      el('span', { class: 'opacity-70 tabular-nums shrink-0' }, fmtSize(v)),
      el('span', { class: 'opacity-50 tabular-nums shrink-0 w-12 text-right' }, pct.toFixed(1) + '%'),
    ]));
  }
  // 最新版が 0B（空ファイルのみ等）でもドーナツが破綻しないよう中立色で埋める。
  $('storage-donut').style.background = stops.length
    ? `conic-gradient(${stops.join(',')})`
    : 'conic-gradient(var(--fallback-b3,#e5e7eb) 0 100%)';
  // 既定の使用容量は過去バージョン込みの実占有サイズ（合計）。
  $('storage-total').textContent = fmtSize(grand);
  $('storage-count').textContent = `${stats.file_count} 件`;

  // バージョン別の内訳: 最新版を表示する容量 vs 過去バージョンを維持する容量
  // （分母は総容量 = 最新版 + 過去バージョン）。
  const curPct = grand ? currentBytes / grand * 100 : 0;
  const hisPct = grand ? historyBytes / grand * 100 : 0;
  $('ver-bar-current').style.width = curPct + '%';
  $('ver-bar-history').style.width = hisPct + '%';
  $('ver-current').textContent = fmtSize(currentBytes);
  $('ver-current-pct').textContent = `(${curPct.toFixed(1)}%)`;
  $('ver-history').textContent = fmtSize(historyBytes);
  $('ver-history-pct').textContent = `(${hisPct.toFixed(1)}%)`;

  $('storage-content').classList.remove('hidden');
}

// ===== アップロード（ドラッグ&ドロップ / 複数ファイル / フォルダ）=====

// 選択済みファイル（File オブジェクトに相対パス relPath を付与して保持）
let selectedFiles = [];

// File の相対パス。フォルダ選択／フォルダドロップ時はフォルダ構造を保つ。
function relPathOf(f) {
  return f.relPath || f.webkitRelativePath || f.name;
}

// 登録に使うファイル名（フォルダ部分を除いたベース名のみ）。
function baseNameOf(f) {
  return relPathOf(f).split('/').pop();
}

// 相対パスに含まれるフォルダ名（タグとして付与する）。ネストは各階層を返す。
function folderTagsOf(f) {
  const parts = relPathOf(f).split('/');
  parts.pop(); // ファイル名を除外
  return parts.filter(p => p && p !== '.');
}

// 複数ファイル選択時のシリーズ名候補（フォルダ名 → 先頭ファイルのベース名）
function guessSeriesName() {
  for (const f of selectedFiles) {
    const p = relPathOf(f);
    const slash = p.indexOf('/');
    if (slash > 0) return p.slice(0, slash);
  }
  const first = selectedFiles[0];
  if (!first) return '';
  return relPathOf(first).split('/').pop().replace(/\.[^.]+$/, '');
}

// 同じ相対パスのファイルは重複登録しない
function addFiles(files) {
  const seen = new Set(selectedFiles.map(relPathOf));
  for (const f of files) {
    const p = relPathOf(f);
    if (seen.has(p)) continue;
    seen.add(p);
    selectedFiles.push(f);
  }
  renderFileList();
}

function clearFiles() {
  selectedFiles = [];
  renderFileList();
}

function renderFileList() {
  const summary = $('file-summary');
  const list = $('file-list');
  const countEl = $('file-count');
  list.innerHTML = '';
  if (selectedFiles.length === 0) {
    summary.classList.add('hidden');
    list.classList.add('hidden');
    return;
  }
  const total = selectedFiles.reduce((a, f) => a + f.size, 0);
  countEl.textContent = `${selectedFiles.length} 件 (${fmtSize(total)})`;
  summary.classList.remove('hidden');
  list.classList.remove('hidden');
  selectedFiles.forEach((f, i) => {
    list.appendChild(el('li', { class: 'flex items-center justify-between gap-2 text-xs px-2 py-1 rounded bg-base-200/50' }, [
      el('span', { class: 'flex items-center gap-1 flex-1 min-w-0', title: relPathOf(f) }, [
        el('span', { class: 'truncate' }, baseNameOf(f)),
        folderTagsOf(f).map(d => el('span', { class: 'badge badge-ghost badge-xs shrink-0' }, d)),
      ]),
      el('span', { class: 'opacity-50 shrink-0' }, fmtSize(f.size)),
      el('button', {
        class: 'btn btn-xs btn-ghost px-1 text-error shrink-0', title: '除外',
        onclick: () => { selectedFiles.splice(i, 1); renderFileList(); },
      }, '×'),
    ]));
  });
}

function fmtSize(n) {
  if (n < 1024) return n + ' B';
  const units = ['KB', 'MB', 'GB', 'TB', 'PB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return v.toFixed(1) + ' ' + units[i];
}

// ---- DataTransfer からフォルダ込みで File を再帰収集 ----
async function filesFromDataTransfer(dt) {
  const entries = [];
  if (dt.items && dt.items.length) {
    for (let i = 0; i < dt.items.length; i++) {
      const it = dt.items[i];
      const entry = it.webkitGetAsEntry && it.webkitGetAsEntry();
      if (entry) entries.push(entry);
    }
  }
  if (entries.length === 0) {
    // フォルダ非対応ブラウザ向けフォールバック
    return Array.from(dt.files || []);
  }
  const out = [];
  for (const entry of entries) await walkEntry(entry, out, '');
  return out;
}

function walkEntry(entry, out, prefix) {
  return new Promise(resolve => {
    if (entry.isFile) {
      entry.file(f => {
        f.relPath = prefix + entry.name;
        out.push(f);
        resolve();
      }, () => resolve());
    } else if (entry.isDirectory) {
      const reader = entry.createReader();
      const collected = [];
      const readBatch = () => reader.readEntries(async batch => {
        if (!batch.length) {
          for (const e of collected) await walkEntry(e, out, prefix + entry.name + '/');
          resolve();
        } else {
          collected.push(...batch);
          readBatch();
        }
      }, () => resolve());
      readBatch();
    } else {
      resolve();
    }
  });
}

function setupUpload() {
  const dz = $('dropzone');
  const fileInput = /** @type {HTMLInputElement} */ ($('upload-file'));
  const folderInput = /** @type {HTMLInputElement} */ ($('upload-folder'));

  dz.onclick = () => fileInput.click();
  $('pick-folder').onclick = e => { e.stopPropagation(); folderInput.click(); };
  $('clear-files').onclick = clearFiles;

  fileInput.onchange = () => { addFiles(Array.from(fileInput.files)); fileInput.value = ''; };
  folderInput.onchange = () => { addFiles(Array.from(folderInput.files)); folderInput.value = ''; };

  ['dragenter', 'dragover'].forEach(ev => dz.addEventListener(ev, e => {
    e.preventDefault();
    dz.classList.add('border-primary', 'bg-base-200/40');
  }));
  ['dragleave', 'dragend'].forEach(ev => dz.addEventListener(ev, e => {
    e.preventDefault();
    dz.classList.remove('border-primary', 'bg-base-200/40');
  }));
  dz.addEventListener('drop', async e => {
    e.preventDefault();
    dz.classList.remove('border-primary', 'bg-base-200/40');
    const files = await filesFromDataTransfer(e.dataTransfer);
    if (files.length) addFiles(files);
  });
}

async function doUpload() {
  if (selectedFiles.length === 0) { uiToast('アップロードするファイルを選択してください', 'warning'); return; }

  const asSeries = /** @type {HTMLInputElement} */ ($('as-series')).checked;
  let seriesName = null;
  if (asSeries) {
    const r = await uiPrompt({
      title: 'シリーズとして登録', okText: '作成',
      fields: [{ name: 'name', label: 'シリーズ名', value: guessSeriesName() }],
    });
    if (!r || !r.name.trim()) return; // キャンセル時は中断
    seriesName = r.name.trim();
  }

  const btn = /** @type {HTMLButtonElement} */ ($('upload-btn'));
  const resultEl = $('upload-result');
  btn.disabled = true;
  btn.classList.add('btn-disabled');
  const total = selectedFiles.length;
  const created = [];
  let failed = 0;
  let tagged = false;

  // タグ名 → tag_id のキャッシュ（同じフォルダ名で重複生成しない）。
  const tagCache = new Map();
  async function ensureTag(name) {
    if (tagCache.has(name)) return tagCache.get(name);
    try {
      const t = await json('/api/tags', { method: 'POST', body: { name } });
      tagCache.set(name, t.id);
      return t.id;
    } catch (_) { return null; }
  }

  for (let i = 0; i < total; i++) {
    const f = selectedFiles[i];
    resultEl.textContent = `アップロード中… ${i + 1} / ${total}`;
    try {
      const buf = await f.arrayBuffer();
      // フォルダ部分を除いたファイル名のみで登録する。
      const res = await api('/api/files?name=' + encodeURIComponent(baseNameOf(f)), {
        method: 'POST', body: buf, headers: { 'content-type': 'application/octet-stream' },
      });
      if (!res.ok) { failed++; continue; }
      const meta = await res.json();
      created.push(meta);
      // フォルダ名をタグとして付与（ネスト時は各階層）。
      for (const dir of folderTagsOf(f)) {
        const tagId = await ensureTag(dir);
        if (!tagId) continue;
        try {
          await json(`/api/files/${meta.id}/tags`, { method: 'POST', body: { tag_id: tagId } });
          tagged = true;
        } catch (_) { /* タグ付け失敗は致命ではないので継続 */ }
      }
    } catch (_) { failed++; }
  }

  if (asSeries && created.length >= 1) {
    try {
      const series = await json('/api/series', { method: 'POST', body: { name: seriesName } });
      for (let i = 0; i < created.length; i++) {
        await json(`/api/series/${series.id}/members`, {
          method: 'POST', body: { file_id: created[i].id, order_index: (i + 1) * 10 },
        });
      }
    } catch (_) {
      uiToast('シリーズへの登録に一部失敗しました', 'error');
    }
  }

  btn.disabled = false;
  btn.classList.remove('btn-disabled');

  if (created.length === 0) {
    resultEl.textContent = '';
    uiToast('アップロードに失敗しました', 'error');
    return;
  }

  clearFiles();
  /** @type {HTMLInputElement} */ ($('as-series')).checked = false;
  const seriesNote = asSeries ? `（シリーズ「${seriesName}」に登録）` : '';
  const tagNote = tagged ? '（フォルダ名をタグとして付与）' : '';
  const failNote = failed > 0 ? ` / ${failed} 件失敗` : '';
  resultEl.replaceChildren(
    `${created.length} 件アップロードしました${failNote} ${seriesNote}${tagNote} `,
    el('a', { class: 'link link-primary', href: '/ui/files' }, 'ファイル一覧へ →'));
  uiToast(`${created.length} 件アップロードしました`, failed > 0 ? 'warning' : 'success');
}

setupUpload();
init();

// テンプレートのインライン onclick から参照される関数を明示的に公開する。
Object.assign(window, { doUpload });
})();
