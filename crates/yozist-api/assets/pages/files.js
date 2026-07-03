// @ts-check
// ファイル一覧ページ（/ui/files）のロジック。files.html のインライン <script> から
// 切り出した静的ファイル（issue #50）。/ui/pages/files.js で配信される。
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {
const PAGE = 100;

let allTags = [];
let selectedTags = new Set();
let allFiles = [];          // 表示中のファイル
let tagsByFile = {};        // file_id -> [Tag]
let hasMore = false;        // ブラウズモード時にまだ続きがあるか (X-Has-More)
let browseOffset = 0;       // ブラウズモードの DB オフセット
let browseMode = true;      // フィルタなし（サーバページング）かどうか

async function init() {
  const me = await requireAuth();
  if (!me) return;
  $('main').classList.remove('hidden');
  await Promise.all([loadTags(), loadSeries(), loadFilters()]);
  restoreFiltersFromUrl();
  await applyFilters();
}

// ---- 左カラム: タグ / シリーズ ----

async function loadTags() {
  try {
    allTags = await json('/api/tags?sort=usage');
    renderTags();
  } catch (e) { allTags = []; }
}

function renderTags() {
  const el = $('f-tags');
  const filter = (/** @type {HTMLInputElement} */ ($('f-tag-search')).value || '').trim().toLowerCase();
  // 選択中タグは絞り込みに関わらず常に先頭へ（解除手段を見失わないように）
  const visible = allTags.filter(t =>
    selectedTags.has(t.name) || !filter || t.name.toLowerCase().includes(filter));
  if (visible.length === 0) {
    el.innerHTML = '<span class="text-xs opacity-50">' +
      (allTags.length === 0 ? 'タグなし' : '該当するタグなし') + '</span>';
    return;
  }
  visible.sort((a, b) =>
    (Number(selectedTags.has(b.name)) - Number(selectedTags.has(a.name))) || a.name.localeCompare(b.name));
  el.innerHTML = '';
  visible.forEach(t => {
    const active = selectedTags.has(t.name);
    const btn = document.createElement('button');
    btn.className = 'badge badge-sm cursor-pointer ' +
      (active ? 'badge-primary' : 'badge-outline');
    const icon = t.kind === 'system' ? ' ⚙' : t.kind === 'ai' ? ' 🤖' : '';
    btn.textContent = t.name + icon;
    btn.onclick = () => toggleTag(t.name);
    el.appendChild(btn);
  });
}

function toggleTag(name) {
  if (selectedTags.has(name)) selectedTags.delete(name);
  else selectedTags.add(name);
  renderTags();
  applyFilters();
}

async function loadSeries() {
  try {
    const list = await json('/api/series');
    const sel = $('f-series');
    sel.innerHTML = '<option value="">(指定なし)</option>' +
      list.map(s => `<option value="${s.id}">${escapeHtml(s.name)}</option>`).join('');
  } catch (e) {}
}

// フィルター一覧ページで作成した条件（SMB の filters/<名前>/ と同じもの）を読み込む。
// 選択するとその条件に一致するファイルへ絞り込める。
async function loadFilters() {
  try {
    const list = await json('/api/filters');
    const sel = $('f-filter');
    sel.innerHTML = '<option value="">(指定なし)</option>' +
      list.map(q => `<option value="${q.id}">${escapeHtml(q.name)}</option>`).join('');
  } catch (e) {}
}

// ---- フィルタ状態 ----

let debounceTimer = null;
function applyFiltersDebounced() {
  clearTimeout(debounceTimer);
  debounceTimer = setTimeout(applyFilters, 250);
}

function resetFilters() {
  /** @type {HTMLInputElement} */ ($('f-search')).value = '';
  /** @type {HTMLInputElement} */ ($('f-name')).value = '';
  /** @type {HTMLSelectElement} */ ($('f-series')).value = '';
  /** @type {HTMLSelectElement} */ ($('f-filter')).value = '';
  selectedTags.clear();
  renderTags();
  applyFilters();
}

function sortVal() { return /** @type {HTMLSelectElement} */ ($('f-sort')).value || 'updated_desc'; }

function saveFiltersToUrl() {
  const params = new URLSearchParams();
  const q = /** @type {HTMLInputElement} */ ($('f-search')).value.trim();
  const name = /** @type {HTMLInputElement} */ ($('f-name')).value.trim();
  const series = /** @type {HTMLSelectElement} */ ($('f-series')).value;
  const filter = /** @type {HTMLSelectElement} */ ($('f-filter')).value;
  if (q) params.set('q', q);
  if (name) params.set('name', name);
  if (series) params.set('series', series);
  if (filter) params.set('filter', filter);
  if (selectedTags.size) params.set('tags', [...selectedTags].join(','));
  if (sortVal() !== 'updated_desc') params.set('sort', sortVal());
  const qs = params.toString();
  history.replaceState(null, '', qs ? '?' + qs : location.pathname);
}

function restoreFiltersFromUrl() {
  const p = new URLSearchParams(location.search);
  if (p.get('q')) /** @type {HTMLInputElement} */ ($('f-search')).value = p.get('q');
  if (p.get('name')) /** @type {HTMLInputElement} */ ($('f-name')).value = p.get('name');
  if (p.get('series')) /** @type {HTMLSelectElement} */ ($('f-series')).value = p.get('series');
  if (p.get('filter')) /** @type {HTMLSelectElement} */ ($('f-filter')).value = p.get('filter');
  if (p.get('tags')) p.get('tags').split(',').filter(Boolean).forEach(t => selectedTags.add(t));
  if (p.get('sort')) /** @type {HTMLSelectElement} */ ($('f-sort')).value = p.get('sort');
  renderTags();
}

// ---- 一覧取得 ----

async function applyFilters() {
  saveFiltersToUrl();
  const q = /** @type {HTMLInputElement} */ ($('f-search')).value.trim();
  const name = /** @type {HTMLInputElement} */ ($('f-name')).value.trim();
  const seriesId = /** @type {HTMLSelectElement} */ ($('f-series')).value;
  const filterId = /** @type {HTMLSelectElement} */ ($('f-filter')).value;
  const tags = [...selectedTags];

  browseMode = !q && tags.length === 0 && !seriesId && !name && !filterId;

  if (browseMode) {
    browseOffset = 0;
    allFiles = [];
    await fetchBrowsePage();
    return;
  }

  // フィルタモード: 各エンドポイントは完全な結果集合を返すのでクライアント側で AND・ソート
  let files;
  try {
    if (q) {
      files = await json('/api/search?q=' + encodeURIComponent(q) + '&limit=500');
    } else if (tags.length > 0) {
      files = await json('/api/files/by-tags?tags=' + encodeURIComponent(tags.join(',')));
    } else if (filterId) {
      // 保存済みフィルター単独 → その条件に一致するファイルを基底集合にする
      files = await json('/api/filters/' + filterId + '/files');
    } else {
      // 名前 / シリーズのみ → ベース集合をソート済みで広めに取得
      files = await json('/api/files?limit=1000&sort=' + sortParams().sort + '&order=' + sortParams().order);
    }
  } catch (e) {
    $('file-list').innerHTML = '<li class="px-2 py-2 text-error text-sm">取得失敗</li>';
    return;
  }

  if (q && tags.length > 0) {
    const tagged = await json('/api/files/by-tags?tags=' + encodeURIComponent(tags.join(','))).catch(() => []);
    const allowed = new Set(tagged.map(f => f.id));
    files = files.filter(f => allowed.has(f.id));
  }

  // 保存済みフィルターを基底集合に使っていない場合は交差（AND）で適用
  if (filterId && (q || tags.length > 0)) {
    const matched = await json('/api/filters/' + filterId + '/files').catch(() => []);
    const allowed = new Set(matched.map(f => f.id));
    files = files.filter(f => allowed.has(f.id));
  }

  if (seriesId) {
    const members = await json('/api/series/' + seriesId + '/members').catch(() => []);
    const allowed = new Set(members.map(m => m.file_id));
    files = files.filter(f => allowed.has(f.id));
  }

  if (name) {
    const lower = name.toLowerCase();
    files = files.filter(f => f.display_name.toLowerCase().includes(lower));
  }

  // FTS は関連度順を保つ。それ以外（または明示的に並び替えた場合）はクライアントでソート
  if (!q || sortVal() !== 'updated_desc') clientSort(files);

  allFiles = files;
  hasMore = false;
  renderFiles();
  fetchTagsFor(files.map(f => f.id));
}

function sortParams() {
  const [key, dir] = sortVal().split('_');
  return { sort: key, order: dir };
}

async function fetchBrowsePage() {
  const p = sortParams();
  let resp;
  try {
    resp = await api(`/api/files?limit=${PAGE}&offset=${browseOffset}&sort=${p.sort}&order=${p.order}`);
    if (!resp.ok) throw new Error(await resp.text());
  } catch (e) {
    $('file-list').innerHTML = '<li class="px-2 py-2 text-error text-sm">取得失敗</li>';
    return;
  }
  hasMore = resp.headers.get('x-has-more') === '1';
  const page = await resp.json();
  // 権限フィルタでページが縮むため、次オフセットはサーバが返す DB 上の位置を使う
  const next = parseInt(resp.headers.get('x-next-offset') || '', 10);
  browseOffset = Number.isNaN(next) ? browseOffset + PAGE : next;
  allFiles = allFiles.concat(page);
  renderFiles();
  fetchTagsFor(page.map(f => f.id));
}

async function loadMore() {
  const btn = /** @type {HTMLButtonElement} */ ($('load-more'));
  btn.disabled = true;
  btn.textContent = '読み込み中…';
  try { await fetchBrowsePage(); }
  finally { btn.disabled = false; btn.textContent = 'さらに読み込む'; }
}

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

// ---- タグ一括取得 ----

async function fetchTagsFor(ids) {
  ids = ids.filter(id => !(id in tagsByFile));
  for (let i = 0; i < ids.length; i += 200) {
    const chunk = ids.slice(i, i + 200);
    try {
      const map = await json('/api/files/tags?ids=' + encodeURIComponent(chunk.join(',')));
      Object.assign(tagsByFile, map);
    } catch (e) { return; }
  }
  // 取得済みタグを表示中の行へ反映
  /** @type {NodeListOf<HTMLElement>} */ (document.querySelectorAll('[data-tags-for]')).forEach(el => {
    renderRowTags(el, el.dataset.tagsFor);
  });
}

function renderRowTags(el, fileId) {
  // システムタグ (ext:* / type:*) は拡張子から自明なので行には出さない
  const tags = (tagsByFile[fileId] || []).filter(t => t.kind !== 'system');
  if (tags.length === 0) { el.innerHTML = ''; return; }
  const MAX = 4;
  const shown = tags.slice(0, MAX);
  el.innerHTML = shown.map(t => {
    const active = selectedTags.has(t.name);
    return `<button class="badge badge-xs ${active ? 'badge-primary' : 'badge-ghost'}"
                    data-tag="${escapeHtml(t.name)}"
                    title="このタグで絞り込み">${escapeHtml(t.name)}</button>`;
  }).join('') +
  (tags.length > MAX ? `<span class="badge badge-xs badge-ghost opacity-60">+${tags.length - MAX}</span>` : '');
}

// ---- 一覧描画 ----

function fmtSize(n) {
  if (n < 1024) return n + ' B';
  const units = ['KB', 'MB', 'GB', 'TB'];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < units.length - 1);
  return n.toFixed(n >= 100 ? 0 : 1) + ' ' + units[i];
}

// 更新者（なければ作成者）を「 · name」形式で返す。未記録（旧データ・SMB 経由）は空。
function actorLabel(f) {
  const who = f.updated_by || f.created_by;
  return who ? ` · ${escapeHtml(who)}` : '';
}

function fileIcon(f) {
  const m = (f.mime || '').toLowerCase();
  const n = f.display_name.toLowerCase();
  if (m.startsWith('image/')) return '🖼️';
  if (m.startsWith('video/')) return '🎬';
  if (m.startsWith('audio/')) return '🎵';
  if (m.includes('pdf')) return '📕';
  if (m.includes('zip') || m.includes('compressed') || /\.(zip|gz|7z|rar|tar|xz|zst)$/.test(n)) return '🗜️';
  if (/\.(md|markdown|txt)$/.test(n)) return '📝';
  if (/\.(js|ts|jsx|tsx|py|rs|go|java|kt|c|cpp|h|hpp|sh|rb|php|html|css|json|yaml|yml|toml|sql|xml)$/.test(n)) return '💻';
  if (/\.(csv|tsv|xlsx|xls)$/.test(n)) return '📊';
  if (m.startsWith('text/')) return '📄';
  if (m && m !== 'application/octet-stream') return '📦';
  return '📄';
}

function renderActiveFilters() {
  const el = $('active-filters');
  const chips = [];
  const q = /** @type {HTMLInputElement} */ ($('f-search')).value.trim();
  const name = /** @type {HTMLInputElement} */ ($('f-name')).value.trim();
  const sel = /** @type {HTMLSelectElement} */ ($('f-series'));
  const fil = /** @type {HTMLSelectElement} */ ($('f-filter'));
  if (q) chips.push({ label: `検索: "${q}"`, clear: () => { /** @type {HTMLInputElement} */ ($('f-search')).value = ''; } });
  if (name) chips.push({ label: `名前: "${name}"`, clear: () => { /** @type {HTMLInputElement} */ ($('f-name')).value = ''; } });
  if (sel.value) chips.push({
    label: 'シリーズ: ' + sel.options[sel.selectedIndex].text,
    clear: () => { sel.value = ''; },
  });
  if (fil.value) chips.push({
    label: 'フィルター: ' + fil.options[fil.selectedIndex].text,
    clear: () => { fil.value = ''; },
  });
  selectedTags.forEach(t => chips.push({
    label: 'タグ: ' + t,
    clear: () => { selectedTags.delete(t); renderTags(); },
  }));
  el.innerHTML = '';
  chips.forEach(c => {
    const b = document.createElement('button');
    b.className = 'badge badge-sm badge-outline gap-1 cursor-pointer hover:badge-error';
    b.title = 'このフィルタを解除';
    b.textContent = c.label + ' ×';
    b.onclick = () => { c.clear(); applyFilters(); };
    el.appendChild(b);
  });
}

function renderFiles() {
  // 総数は権限フィルタ済みの値を安価に出せないため、読み込み済み件数 + 「+」表記のみ
  $('files-count').textContent = `(${allFiles.length}${browseMode && hasMore ? '+' : ''})`;
  renderActiveFilters();

  const el = $('file-list');
  if (allFiles.length === 0) {
    el.innerHTML = browseMode
      ? '<li class="px-2 py-8 opacity-60 text-sm text-center">ファイルがありません。「アップロード」または「新規テキスト」で追加できます。</li>'
      : '<li class="px-2 py-8 opacity-60 text-sm text-center">該当ファイルなし — フィルタ条件を見直してください。</li>';
  } else {
    el.innerHTML = allFiles.map(f => `
      <li>
        <a href="/ui/files/${f.id}" class="flex items-center gap-3 px-2 py-2 rounded hover:bg-base-200">
          <span class="text-lg shrink-0" aria-hidden="true">${fileIcon(f)}</span>
          <span class="min-w-0 flex-1">
            <span class="font-semibold truncate block">${escapeHtml(f.display_name)}</span>
            <span class="flex flex-wrap gap-1 mt-0.5 empty:hidden" data-tags-for="${f.id}"></span>
          </span>
          <!-- base.html の .hidden は !important なので sm:block で上書きできない。max-sm:hidden を使う -->
          <span class="text-xs opacity-60 shrink-0 text-right block max-sm:hidden w-32">
            <span class="block" title="更新日時">${fmtTs(f.updated_at)}</span>
            <span class="block">${fmtSize(f.size)}${actorLabel(f)}</span>
          </span>
        </a>
      </li>
    `).join('');
    // 取得済みタグがあれば即時反映
    /** @type {NodeListOf<HTMLElement>} */ (el.querySelectorAll('[data-tags-for]'))
      .forEach(t => renderRowTags(t, t.dataset.tagsFor));
  }

  $('load-more-wrap').classList.toggle('hidden', !(browseMode && hasMore));
}

// 行内のタグチップはイベントデリゲーションで処理（クリックで絞り込みトグル）
$('file-list').addEventListener('click', e => {
  const tagBtn = /** @type {HTMLElement|null} */
    (/** @type {HTMLElement} */ (e.target).closest('[data-tag]'));
  if (!tagBtn) return;
  e.preventDefault();
  toggleTag(tagBtn.dataset.tag);
});

// ---- アップロード ----

async function uploadFiles(fileList) {
  const files = [...fileList];
  if (files.length === 0) return;
  let ok = 0;
  for (const f of files) {
    try {
      const r = await api('/api/files?client=web&name=' + encodeURIComponent(f.name), {
        method: 'POST',
        body: await f.arrayBuffer(),
        headers: { 'content-type': 'application/octet-stream' },
      });
      if (!r.ok) throw new Error(await r.text());
      ok++;
    } catch (e) {
      uiToast(`"${f.name}" のアップロードに失敗しました: ${e.message}`, 'error');
    }
  }
  if (ok > 0) {
    uiToast(`${ok} 件アップロードしました`, 'success');
    await applyFilters();
  }
}

async function newFile() {
  const r = await uiPrompt({
    title: '新規ファイル', okText: '作成',
    fields: [
      { name: 'name', label: 'ファイル名', placeholder: '例: memo.md' },
    ],
  });
  if (!r || !r.name.trim()) return;
  try {
    // 本文は空で作成し、内容は詳細ページで編集する。
    const resp = await api('/api/files?client=web&name=' + encodeURIComponent(r.name.trim()), {
      method: 'POST',
      body: new ArrayBuffer(0),
      headers: { 'content-type': 'application/octet-stream' },
    });
    if (!resp.ok) throw new Error(await resp.text());
    const meta = await resp.json();
    uiToast('作成しました', 'success');
    location.href = `/ui/files/${meta.id}`;
  } catch (e) {
    uiToast('作成に失敗しました: ' + e.message, 'error');
  }
}

// ドラッグ＆ドロップアップロード
{
  const zone = $('drop-zone');
  let depth = 0;
  zone.addEventListener('dragenter', e => {
    if (![...e.dataTransfer.types].includes('Files')) return;
    e.preventDefault();
    depth++;
    zone.classList.add('drag-over');
  });
  zone.addEventListener('dragover', e => e.preventDefault());
  zone.addEventListener('dragleave', () => {
    if (--depth <= 0) { depth = 0; zone.classList.remove('drag-over'); }
  });
  zone.addEventListener('drop', e => {
    e.preventDefault();
    depth = 0;
    zone.classList.remove('drag-over');
    uploadFiles(e.dataTransfer.files);
  });
}

// "/" で検索ボックスへフォーカス
document.addEventListener('keydown', e => {
  if (e.key !== '/' || e.ctrlKey || e.metaKey || e.altKey) return;
  const t = /** @type {HTMLElement} */ (e.target);
  if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable)) return;
  e.preventDefault();
  $('f-search').focus();
});

init();

// テンプレートのインライン onclick/onchange/oninput から参照される関数を明示的に公開する。
Object.assign(window, {
  applyFilters, applyFiltersDebounced, resetFilters, renderTags,
  loadMore, uploadFiles, newFile,
});
})();
