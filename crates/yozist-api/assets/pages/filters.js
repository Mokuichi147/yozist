// @ts-check
// フィルター一覧ページ（/ui/filters）のロジック。filters.html のインライン <script> から切り出した静的ファイル（issue #50）。
// /ui/pages/filters.js で配信される。
// 作成者本人のみ編集・削除を許可（サーバ側でも検証）。
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {
let myUserId = null;
let editingId = null;

// フィールド定義（属性ドロップダウン）と、それぞれの「型」。
const FIELD_TYPE = {
  manual_tag: 'tag', system_tag: 'tag', ai_tag: 'tag', tag: 'tag',
  series: 'series', mime: 'mime', name: 'text', created: 'date', updated: 'date',
};
const FIELD_OPTIONS = `
  <optgroup label="タグ">
    <option value="manual_tag">手動タグ</option>
    <option value="system_tag">システムタグ</option>
    <option value="ai_tag">AIタグ</option>
    <option value="tag">タグ（種別不問）</option>
  </optgroup>
  <optgroup label="属性">
    <option value="series">シリーズ</option>
    <option value="mime">種類</option>
    <option value="name">名前</option>
    <option value="created">作成日</option>
    <option value="updated">更新日</option>
  </optgroup>`;
const MIME_PRESETS = [
  ['pdf', 'PDF'], ['image/', '画像'], ['text/', 'テキスト'], ['video/', '動画'],
  ['audio/', '音声'], ['word', 'Word'], ['sheet', 'Excel'],
  ['presentation', 'PowerPoint'], ['zip', 'ZIP/圧縮'],
];

async function init() {
  const me = await requireAuth();
  if (!me) return;
  myUserId = me.user && me.user.id;
  $('main').classList.remove('hidden');
  await Promise.all([loadQueries(), loadTagOptions(), loadSeriesOptions()]);
}

function canEditQuery(q) {
  return q.created_by == null || q.created_by === myUserId;
}

// ---- 一覧（条件を人間可読に整形） ----
const FIELD_LABEL = {
  manual_tag: '手動タグ', system_tag: 'システムタグ', ai_tag: 'AIタグ', tag: 'タグ',
  series: 'シリーズ', mime: '種類', name: '名前', created: '作成日', updated: '更新日',
};
const OP_LABEL = {
  include: 'を含む', exclude: 'を含まない', contains: 'を含む', not_contains: 'を含まない',
  is: 'と一致', is_not: 'と不一致', starts_with: 'で始まる', ends_with: 'で終わる',
  within: '以内', before: 'より前', after: 'より後',
};
const UNIT_LABEL = { day: '日', month: 'か月', year: '年' };

function condText(c) {
  const f = FIELD_LABEL[c.field] || c.field;
  if (c.field === 'created' || c.field === 'updated') {
    return `${f}が ${c.value}${UNIT_LABEL[c.unit] || ''} ${OP_LABEL[c.op] || c.op}`;
  }
  const mimeLabel = c.field === 'mime'
    ? (MIME_PRESETS.find(p => p[0] === c.value) || [c.value, c.value])[1]
    : c.value;
  return `${f}「${mimeLabel}」${OP_LABEL[c.op] || c.op}`;
}

function queryText(q) {
  const parts = [];
  (q.definition.tags_and || []).forEach(t => parts.push(`タグ「${t}」を含む`));
  (q.definition.tags_not || []).forEach(t => parts.push(`タグ「${t}」を含まない`));
  (q.definition.conditions || []).forEach(c => parts.push(condText(c)));
  if (parts.length === 0) return '全件';
  const join = (q.definition.match_mode === 'any') ? ' または ' : ' かつ ';
  return parts.join(join);
}

async function loadQueries() {
  const el = $('query-list');
  try {
    const list = await json('/api/filters');
    if (list.length === 0) { el.innerHTML = '<div class="opacity-50 text-sm py-2">フィルターはまだありません。「+ 新規作成」から追加してください。</div>'; return; }
    el.innerHTML = list.map(q => {
      const mine = canEditQuery(q);
      const editBtns = mine ? `
          <button class="btn btn-xs" onclick="openQueryModal('edit','${q.id}')">編集</button>
          <button class="btn btn-xs btn-error btn-outline" onclick="deleteQuery('${q.id}','${escapeHtml(q.name)}')">削除</button>` : '';
      return `<div class="flex items-start justify-between row-compact gap-2 border-b border-base-200 last:border-0 py-2">
        <div class="min-w-0">
          <div><a href="/ui/files?filter=${q.id}" class="font-semibold break-all link link-hover"
                  title="この条件でファイル一覧を開く">${escapeHtml(q.name)}</a></div>
          <div class="text-xs opacity-60 break-all">${escapeHtml(q.description || queryText(q))}</div>
        </div>
        <div class="flex gap-1 shrink-0">
          <a href="/ui/files?filter=${q.id}" class="btn btn-xs btn-primary btn-outline"
             title="この条件でファイル一覧を開く">開く</a>
          <button class="btn btn-xs" onclick="shareQuery('${q.id}','${escapeHtml(q.name)}')">共有</button>
          ${editBtns}
        </div>
      </div>`;
    }).join('');
  } catch (e) { el.innerHTML = '<div class="opacity-50 text-xs">取得失敗</div>'; }
}

// 属性（タグ種別）→ 参照するタグ候補 datalist の対応。
const TAG_DATALIST = {
  manual_tag: 'tag-options-manual',
  system_tag: 'tag-options-system',
  ai_tag: 'tag-options-ai',
  tag: 'tag-options-all',
};

async function loadTagOptions() {
  try {
    const tags = await json('/api/tags');
    const opt = ts => ts.map(t => `<option value="${escapeHtml(t.name)}"></option>`).join('');
    // 種別ごとに候補を振り分け、種別不問用には全件を入れる。
    $('tag-options-manual').innerHTML = opt(tags.filter(t => t.kind === 'manual'));
    $('tag-options-system').innerHTML = opt(tags.filter(t => t.kind === 'system'));
    $('tag-options-ai').innerHTML = opt(tags.filter(t => t.kind === 'ai'));
    $('tag-options-all').innerHTML = opt(tags);
  } catch (e) { /* 候補なしでも手入力できる */ }
}
async function loadSeriesOptions() {
  try {
    const series = await json('/api/series');
    $('series-options').innerHTML = series.map(s => `<option value="${escapeHtml(s.name)}"></option>`).join('');
  } catch (e) { /* 同上 */ }
}

// ---- 条件ビルダー ----
function opOptions(type, op) {
  let opts;
  if (type === 'tag' || type === 'series' || type === 'mime') {
    opts = [['include', type === 'mime' ? 'を含む' : 'を含む'], ['exclude', 'を含まない']];
  } else if (type === 'text') {
    opts = [['contains', 'を含む'], ['is', 'と一致'], ['starts_with', 'で始まる'], ['ends_with', 'で終わる'], ['not_contains', 'を含まない']];
  } else { // date
    opts = [['within', '以内'], ['before', 'より前'], ['after', 'より後']];
  }
  return opts.map(([v, l]) => `<option value="${v}"${v === op ? ' selected' : ''}>${l}</option>`).join('');
}

function renderControls(type, op, value, unit, field) {
  const v = escapeHtml(value || '');
  // 演算子（「を含む」等）は日本語として自然になるよう値の右側（末尾）に置く。
  const opSel = w => `<select class="q-op select select-bordered select-sm ${w} shrink-0">${opOptions(type, op)}</select>`;
  if (type === 'tag') {
    const list = TAG_DATALIST[field] || 'tag-options-all';
    return `<input class="q-val input input-bordered input-sm flex-1 min-w-0" list="${list}" placeholder="タグ名" value="${v}" />${opSel('w-32')}`;
  }
  if (type === 'series') {
    return `<input class="q-val input input-bordered input-sm flex-1 min-w-0" list="series-options" placeholder="シリーズ名" value="${v}" />${opSel('w-32')}`;
  }
  if (type === 'mime') {
    const opts = MIME_PRESETS.map(([val, lbl]) => `<option value="${val}"${val === value ? ' selected' : ''}>${lbl}</option>`).join('');
    return `<select class="q-val select select-bordered select-sm flex-1 min-w-0">${opts}</select>${opSel('w-32')}`;
  }
  if (type === 'text') {
    return `<input class="q-val input input-bordered input-sm flex-1 min-w-0" placeholder="文字列" value="${v}" />${opSel('w-32')}`;
  }
  // date: [数値][単位][演算子]
  const units = [['day', '日'], ['month', 'か月'], ['year', '年']]
    .map(([val, lbl]) => `<option value="${val}"${val === (unit || 'day') ? ' selected' : ''}>${lbl}</option>`).join('');
  return `<input type="number" min="0" class="q-val input input-bordered input-sm w-20 shrink-0" value="${v || '1'}" />
    <select class="q-unit select select-bordered select-sm w-24 shrink-0">${units}</select>${opSel('w-28')}`;
}

function makeRow(c) {
  c = c || { field: 'manual_tag', op: 'include', value: '', unit: null };
  const type = FIELD_TYPE[c.field] || 'tag';
  const row = document.createElement('div');
  row.className = 'q-row flex items-center gap-2';
  row.innerHTML = `
    <select class="q-field select select-bordered select-sm w-48 shrink-0">${FIELD_OPTIONS}</select>
    <span class="q-controls flex items-center gap-2 flex-1 min-w-0"></span>
    <button type="button" class="btn btn-sm btn-square btn-ghost q-del shrink-0" title="この条件を削除">−</button>`;
  const fieldSel = /** @type {HTMLSelectElement} */ (row.querySelector('.q-field'));
  fieldSel.value = c.field;
  row.querySelector('.q-controls').innerHTML =
    renderControls(type, c.op, c.value, c.unit, c.field);
  // 属性変更時は演算子・値コントロール（タグ候補の参照先含む）を作り直す。
  fieldSel.onchange = () => {
    const t = FIELD_TYPE[fieldSel.value] || 'tag';
    row.querySelector('.q-controls').innerHTML = renderControls(t, null, '', null, fieldSel.value);
  };
  /** @type {HTMLElement} */ (row.querySelector('.q-del')).onclick = () => {
    const rows = $('q-rows');
    if (rows.children.length > 1) row.remove();
    else row.replaceWith(makeRow()); // 最後の1行は空にリセット
  };
  return row;
}

function setRows(q) {
  const cont = $('q-rows');
  cont.innerHTML = '';
  // レガシー tags_and/tags_not を「タグ」条件へ変換して表示。
  (q.tags_and || []).forEach(t => cont.appendChild(makeRow({ field: 'tag', op: 'include', value: t })));
  (q.tags_not || []).forEach(t => cont.appendChild(makeRow({ field: 'tag', op: 'exclude', value: t })));
  (q.conditions || []).forEach(c => cont.appendChild(makeRow(c)));
  if (cont.children.length === 0) cont.appendChild(makeRow());
}

function collectConditions() {
  const conditions = [];
  $('q-rows').querySelectorAll('.q-row').forEach(row => {
    const field = /** @type {HTMLSelectElement} */ (row.querySelector('.q-field')).value;
    const type = FIELD_TYPE[field] || 'tag';
    const op = /** @type {HTMLSelectElement} */ (row.querySelector('.q-op')).value;
    const valEl = /** @type {HTMLInputElement|HTMLSelectElement} */ (row.querySelector('.q-val'));
    const value = (valEl.value || '').trim();
    if (!value) return; // 値が空の行は無視
    const c = { field, op, value };
    if (type === 'date') c.unit = /** @type {HTMLSelectElement} */ (row.querySelector('.q-unit')).value;
    conditions.push(c);
  });
  return conditions;
}

// ---- モーダル制御 ----
async function openQueryModal(mode, id) {
  editingId = mode === 'edit' ? id : null;
  $('q-title').textContent = mode === 'edit' ? 'フィルターの編集' : 'フィルターの作成';
  if (mode === 'edit') {
    let q;
    try { q = await json(`/api/filters/${id}`); }
    catch (e) { uiToast('取得に失敗しました', 'error'); return; }
    /** @type {HTMLInputElement} */ ($('q-name')).value = q.name;
    /** @type {HTMLTextAreaElement} */ ($('q-desc')).value = q.description || '';
    /** @type {HTMLSelectElement} */ ($('q-match')).value = q.definition.match_mode || 'all';
    setRows(q.definition);
  } else {
    /** @type {HTMLInputElement} */ ($('q-name')).value = '';
    /** @type {HTMLTextAreaElement} */ ($('q-desc')).value = '';
    /** @type {HTMLSelectElement} */ ($('q-match')).value = 'all';
    setRows({});
  }
  /** @type {HTMLDialogElement} */ ($('q-modal')).showModal();
  $('q-name').focus();
}

async function saveQuery() {
  const name = /** @type {HTMLInputElement} */ ($('q-name')).value.trim();
  if (!name) { uiToast('フィルター名を入力してください', 'warning'); return; }
  const desc = /** @type {HTMLTextAreaElement} */ ($('q-desc')).value.trim();
  // すべて conditions で表現し、レガシー tags_* はクリアする。
  const body = {
    name,
    description: desc || null,
    match_mode: /** @type {HTMLSelectElement} */ ($('q-match')).value,
    conditions: collectConditions(),
    tags_and: [],
    tags_not: [],
  };
  try {
    if (editingId) {
      await json(`/api/filters/${editingId}`, { method: 'PATCH', body });
      uiToast('更新しました', 'success');
    } else {
      await json('/api/filters', { method: 'POST', body });
      uiToast('フィルターを作成しました', 'success');
    }
    /** @type {HTMLDialogElement} */ ($('q-modal')).close();
    await loadQueries();
  } catch (e) { uiToast('保存に失敗しました: ' + e.message, 'error'); }
}

async function deleteQuery(id, name) {
  if (!await uiConfirm(`フィルター「${name}」を削除しますか？\n対応する SMB パス (yozist/filters/${name}/) も無効になります。`, { danger: true })) return;
  try {
    await api(`/api/filters/${id}`, { method: 'DELETE' });
    uiToast('削除しました', 'success');
    await loadQueries();
  } catch (e) { uiToast('削除に失敗しました', 'error'); }
}

async function shareQuery(id, name) {
  const r = await uiPrompt({
    title: `「${name}」の共有URLを発行`, okText: '発行',
    fields: [{ name: 'ttl', label: '有効期限 (秒)', type: 'number', value: '3600' }],
  });
  if (!r) return;
  const ttl = parseInt(r.ttl, 10);
  if (!ttl) { uiToast('有効期限を正しく入力してください', 'warning'); return; }
  try {
    const res = await json(`/api/filters/${id}/share`, { method: 'POST', body: { ttl_secs: ttl } });
    await uiCopyUrl(location.origin + res.url, 'フィルター共有URL');
  } catch (e) { uiToast('共有URL発行に失敗しました: ' + e.message, 'error'); }
}

$('q-add-row').onclick = () => $('q-rows').appendChild(makeRow());
$('q-cancel').onclick = () => /** @type {HTMLDialogElement} */ ($('q-modal')).close();
$('q-save').onclick = saveQuery;
init();

// テンプレート／生成 HTML のインライン onclick から参照される関数を明示的に公開する。
Object.assign(window, { openQueryModal, deleteQuery, shareQuery });
})();
