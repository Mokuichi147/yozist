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

// ---- 複数選択 ----
// file_id → ファイルメタ。ID だけでなく実体を持つのは、フィルタや並び順を変えて
// 一覧から外れた選択項目に対しても操作を続けられるようにするため
// （バーの件数と実際の操作対象が食い違わない）。
const selected = new Map();
let selMode = false;        // 選択モード（チェックボックスを常時表示）
let lastAnchorId = null;    // Shift+クリックによる範囲選択の起点

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
  const box = $('f-tags');
  const filter = (/** @type {HTMLInputElement} */ ($('f-tag-search')).value || '').trim().toLowerCase();
  // 選択中タグは絞り込みに関わらず常に先頭へ（解除手段を見失わないように）
  const visible = allTags.filter(t =>
    selectedTags.has(t.name) || !filter || t.name.toLowerCase().includes(filter));
  if (visible.length === 0) {
    box.replaceChildren(el('span', { class: 'text-xs opacity-50' },
      allTags.length === 0 ? 'タグなし' : '該当するタグなし'));
    return;
  }
  visible.sort((a, b) =>
    (Number(selectedTags.has(b.name)) - Number(selectedTags.has(a.name))) || a.name.localeCompare(b.name));
  box.replaceChildren(...visible.map(t => {
    const active = selectedTags.has(t.name);
    const icon = t.kind === 'system' ? ' ⚙' : t.kind === 'ai' ? ' 🤖' : '';
    return el('button', {
      class: 'badge badge-sm cursor-pointer ' + (active ? 'badge-primary' : 'badge-outline'),
      onclick: () => toggleTag(t.name),
    }, t.name + icon);
  }));
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
    $('f-series').replaceChildren(
      el('option', { value: '' }, '(指定なし)'),
      ...list.map(s => el('option', { value: s.id }, s.name)),
    );
  } catch (e) {}
}

// フィルター一覧ページで作成した条件（SMB の filters/<名前>/ と同じもの）を読み込む。
// 選択するとその条件に一致するファイルへ絞り込める。
async function loadFilters() {
  try {
    const list = await json('/api/filters');
    $('f-filter').replaceChildren(
      el('option', { value: '' }, '(指定なし)'),
      ...list.map(q => el('option', { value: q.id }, q.name)),
    );
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
  const series = /** @type {HTMLSelectElement} */ ($('f-series')).value;
  const filter = /** @type {HTMLSelectElement} */ ($('f-filter')).value;
  if (q) params.set('q', q);
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
  // 旧「ファイル名 (部分一致)」欄の URL パラメータは統合後の検索欄へ引き継ぐ
  else if (p.get('name')) /** @type {HTMLInputElement} */ ($('f-search')).value = p.get('name');
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
  const seriesId = /** @type {HTMLSelectElement} */ ($('f-series')).value;
  const filterId = /** @type {HTMLSelectElement} */ ($('f-filter')).value;
  const tags = [...selectedTags];

  browseMode = !q && tags.length === 0 && !seriesId && !filterId;

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
      // シリーズのみ → ベース集合をソート済みで広めに取得
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
  /** @type {NodeListOf<HTMLElement>} */ (document.querySelectorAll('[data-tags-for]')).forEach(node => {
    renderRowTags(node, node.dataset.tagsFor);
  });
}

function renderRowTags(box, fileId) {
  // システムタグ (ext:* / type:*) は拡張子から自明なので行には出さない
  const tags = (tagsByFile[fileId] || []).filter(t => t.kind !== 'system');
  if (tags.length === 0) { box.replaceChildren(); return; }
  const MAX = 4;
  box.replaceChildren();
  elAppend(box, [
    tags.slice(0, MAX).map(t => el('button', {
      class: `badge badge-xs ${selectedTags.has(t.name) ? 'badge-primary' : 'badge-ghost'}`,
      'data-tag': t.name,
      title: 'このタグで絞り込み',
    }, t.name)),
    tags.length > MAX &&
      el('span', { class: 'badge badge-xs badge-ghost opacity-60' }, `+${tags.length - MAX}`),
  ]);
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
// テキストノードとして挿入する（el() ヘルパー）ためエスケープ不要。
function actorLabel(f) {
  const who = f.updated_by || f.created_by;
  return who ? ` · ${who}` : '';
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
  const box = $('active-filters');
  const chips = [];
  const q = /** @type {HTMLInputElement} */ ($('f-search')).value.trim();
  const sel = /** @type {HTMLSelectElement} */ ($('f-series'));
  const fil = /** @type {HTMLSelectElement} */ ($('f-filter'));
  if (q) chips.push({ label: `検索: "${q}"`, clear: () => { /** @type {HTMLInputElement} */ ($('f-search')).value = ''; } });
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
  box.replaceChildren(...chips.map(c => el('button', {
    class: 'badge badge-sm badge-outline gap-1 cursor-pointer hover:badge-error',
    title: 'このフィルタを解除',
    onclick: () => { c.clear(); applyFilters(); },
  }, c.label + ' ×')));
}

function renderFiles() {
  // 総数は権限フィルタ済みの値を安価に出せないため、読み込み済み件数 + 「+」表記のみ
  $('files-count').textContent = `(${allFiles.length}${browseMode && hasMore ? '+' : ''})`;
  renderActiveFilters();

  const list = $('file-list');
  if (allFiles.length === 0) {
    list.replaceChildren(el('li', { class: 'px-2 py-8 opacity-60 text-sm text-center' },
      browseMode
        ? 'ファイルがありません。「アップロード」または「新規テキスト」で追加できます。'
        : '該当ファイルなし — フィルタ条件を見直してください。'));
  } else {
    list.replaceChildren(...allFiles.map(f =>
      // チェックボックスはリンクの外に置く（中に入れると選択のたびに詳細へ遷移する）。
      // 表示の出し分けは入れ物の span 側で行う（daisyUI の .checkbox が持つ
      // display をこちらで上書きしないため）。
      el('li', { class: 'flex items-center gap-1', 'data-file-id': f.id }, [
        el('span', { class: 'sel-box shrink-0 ml-1' }, el('input', {
          type: 'checkbox',
          // 選択済みの色はメディア一覧のタイル枠と揃える（checkbox-primary）。
          class: 'checkbox checkbox-sm checkbox-primary',
          checked: selected.has(f.id),
          'data-sel': f.id,
          title: '選択（Shift+クリックで範囲選択）',
          'aria-label': f.display_name + ' を選択',
        })),
        el('a', { href: `/ui/files/${f.id}`, class: 'flex items-center gap-3 px-2 py-2 rounded hover:bg-base-200 min-w-0 flex-1' }, [
          el('span', { class: 'text-lg shrink-0', 'aria-hidden': 'true' }, fileIcon(f)),
          el('span', { class: 'min-w-0 flex-1' }, [
            el('span', { class: 'font-semibold truncate block' }, f.display_name),
            el('span', { class: 'flex flex-wrap gap-1 mt-0.5 empty:hidden', 'data-tags-for': f.id }),
          ]),
          // base.html の .hidden は !important なので sm:block で上書きできない。max-sm:hidden を使う
          el('span', { class: 'text-xs opacity-60 shrink-0 text-right block max-sm:hidden w-32' }, [
            el('span', { class: 'block', title: '更新日時' }, fmtTs(f.updated_at)),
            el('span', { class: 'block' }, fmtSize(f.size) + actorLabel(f)),
          ]),
        ]),
      ])));
    // 取得済みタグがあれば即時反映
    /** @type {NodeListOf<HTMLElement>} */ (list.querySelectorAll('[data-tags-for]'))
      .forEach(t => renderRowTags(t, t.dataset.tagsFor));
  }

  $('load-more-wrap').classList.toggle('hidden', !(browseMode && hasMore));
  renderSelectionBar();
}

// 行内のタグチップと選択チェックボックスはイベントデリゲーションで処理する
// （行は applyFilters のたびに作り直されるので、個別にリスナを張らない）。
$('file-list').addEventListener('click', e => {
  const target = /** @type {HTMLElement} */ (e.target);
  const box = /** @type {HTMLInputElement|null} */ (target.closest('[data-sel]'));
  if (box) { onSelectClick(box.dataset.sel, e.shiftKey); return; }
  const tagBtn = /** @type {HTMLElement|null} */ (target.closest('[data-tag]'));
  if (tagBtn) {
    e.preventDefault();
    toggleTag(tagBtn.dataset.tag);
    return;
  }
  // 選択モード中は行のどこを押しても選択のトグル（詳細へは遷移しない）。
  // チェックボックスを狙わなくても選べるようにするため。
  if (!selMode) return;
  const row = /** @type {HTMLElement|null} */ (target.closest('[data-file-id]'));
  if (!row) return;
  e.preventDefault();
  onSelectClick(row.dataset.fileId, e.shiftKey);
});

// ---- 複数選択 ----

// チェックボックスのクリック。Shift 併用時は直前に触れた行との範囲をまとめて
// 同じ状態にする（クリックで既定のトグルは済んでいるので、望む状態は選択集合から求める）。
function onSelectClick(id, shift) {
  const on = !selected.has(id);
  if (shift && lastAnchorId && lastAnchorId !== id) selectRange(lastAnchorId, id, on);
  else setSelected(id, on);
  lastAnchorId = id;
  syncSelectionUi();
}

function setSelected(id, on) {
  if (!on) { selected.delete(id); return; }
  const f = allFiles.find(x => x.id === id);
  if (f) selected.set(id, f);
}

// 一覧上で from〜to の間にある行をまとめて on にする（範囲は表示順で決まる）。
function selectRange(fromId, toId, on) {
  const a = allFiles.findIndex(f => f.id === fromId);
  const b = allFiles.findIndex(f => f.id === toId);
  if (a < 0 || b < 0) { setSelected(toId, on); return; }
  for (let i = Math.min(a, b); i <= Math.max(a, b); i++) setSelected(allFiles[i].id, on);
}

function selectAllVisible() {
  allFiles.forEach(f => selected.set(f.id, f));
  syncSelectionUi();
}

function clearSelection() {
  selected.clear();
  lastAnchorId = null;
  syncSelectionUi();
}

// 選択モード。チェックボックスはこのモードのときだけ現れる（普段の一覧の
// 見た目を変えないため）。入口はツールバーの「☑ 選択」と、行の長押し。
function setSelMode(on) {
  if (selMode === on) return;
  selMode = on;
  $('file-list').classList.toggle('sel-mode', selMode);
  $('sel-toggle').classList.toggle('btn-active', selMode);
  // 抜けるときは選択も捨てる。入るときは未選択でも操作バーを出す（「表示中を
  // 全選択」や終了ボタンへ、1 件選ぶ前に手が届くように）。
  if (!selMode) clearSelection();
  else renderSelectionBar();
}

function toggleSelMode() { setSelMode(!selMode); }
function exitSelMode() { setSelMode(false); }

// 行の長押し: 選択モードへ入り、その行を選択する（続く click は握り潰されるので
// 詳細ページへは遷移しない）。
// 長押しはあくまで「選択モードへの入口」なので、下の 2 つでは検出しない。
// どちらも長押し成立時の click 握り潰しが操作を殺してしまうため:
//   - 選択モード中: 行のタップだけで選択できるので長押しは不要
//   - チェックボックスの上: 狙って押すと 450ms は簡単に超えるので、ここで拾うと
//     「チェックを外せない」状態になる
BulkActions.longPressSelect(
  $('file-list'),
  target => (selMode || target.closest('[data-sel]'))
    ? null
    : target.closest('[data-file-id]')?.dataset.fileId,
  id => {
    setSelMode(true);
    setSelected(id, true);
    lastAnchorId = id;
    syncSelectionUi();
  },
);

// 選択集合を DOM（チェック状態と操作バー）へ反映する。行を作り直さずに済ませる。
function syncSelectionUi() {
  /** @type {NodeListOf<HTMLInputElement>} */ (document.querySelectorAll('#file-list [data-sel]'))
    .forEach(box => { box.checked = selected.has(box.dataset.sel); });
  renderSelectionBar();
}

function renderSelectionBar() {
  const n = selected.size;
  const open = selMode || n > 0;
  $('sel-bar').classList.toggle('hidden', !open);
  // バーは position:fixed で最後の行に重なるので、開いている間だけ下端に余白を作る。
  document.body.classList.toggle('sel-bar-open', open);
  if (!open) return;
  // 一覧に見えている件数も出す。フィルタを変えた後「操作対象が画面にない」ことが
  // 分かるようにするため（選択自体はフィルタをまたいで保持される）。
  const visible = allFiles.filter(f => selected.has(f.id)).length;
  $('sel-count').textContent = n === 0
    ? '項目を選択'
    : visible === n
      ? `${n} 件選択中`
      : `${n} 件選択中（表示中 ${visible} 件）`;
  // 未選択のうちは押しても何も起きないボタンを無効化する。
  /** @type {NodeListOf<HTMLButtonElement>} */
  (document.querySelectorAll('#sel-bar [data-needs-selection]'))
    .forEach(b => { b.disabled = n === 0; });
}

// 一括操作の対象。シリーズ追加の並び順が見た目どおりになるよう一覧の表示順に
// 揃え、フィルタで一覧から外れている選択項目はその後ろへ回す。
function selectedFiles() {
  const inView = allFiles.filter(f => selected.has(f.id));
  const seen = new Set(inView.map(f => f.id));
  const rest = [...selected.values()].filter(f => !seen.has(f.id));
  return [...inView, ...rest];
}

// 一括操作。BulkActions が true（一覧の再読み込みが必要）を返したときだけ引き直す。
async function runBulk(fn, { clearAfter = false } = {}) {
  const files = selectedFiles();
  if (files.length === 0) return;
  const changed = await fn(files);
  if (clearAfter) clearSelection();
  if (changed) {
    // タグは行に出るので、変更された分のキャッシュを捨ててから引き直す。
    // 左カラムのタグ一覧も、新しく作ったタグや使われなくなったタグを反映させる。
    files.forEach(f => { delete tagsByFile[f.id]; });
    await Promise.all([loadTags(), applyFilters()]);
  }
  syncSelectionUi();
}

const bulkAddTag      = () => runBulk(BulkActions.addTag);
const bulkRemoveTag   = () => runBulk(BulkActions.removeTag);
const bulkAddToSeries = () => runBulk(BulkActions.addToSeries);
const bulkDownload    = () => runBulk(BulkActions.download);
// 削除したファイルは一覧から消えるので、選択も残さない。
const bulkDelete      = () => runBulk(BulkActions.remove, { clearAfter: true });

// Escape は 1 回目で選択解除、2 回目で選択モードを抜ける（入力欄の編集中は邪魔しない）。
document.addEventListener('keydown', e => {
  if (e.key !== 'Escape' || (!selMode && selected.size === 0)) return;
  const t = /** @type {HTMLElement} */ (e.target);
  if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
  if (selected.size > 0) clearSelection();
  else setSelMode(false);
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
  toggleSelMode, exitSelMode, selectAllVisible, clearSelection,
  bulkAddTag, bulkRemoveTag, bulkAddToSeries, bulkDownload, bulkDelete,
});
})();
