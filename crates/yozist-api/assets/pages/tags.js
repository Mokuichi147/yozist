// @ts-check
// タグ一覧ページ（/ui/tags）のロジック。tags.html のインライン <script> から切り出した静的ファイル（issue #50）。
// /ui/pages/tags.js で配信される。
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {
let tags = [];
// 選択中タグ ID の集合（合流対象）
const selected = new Set();

async function init() {
  const me = await requireAuth();
  if (!me) return;
  $('main').classList.remove('hidden');
  await refresh();
}

// 一覧と一括割り当ての対象件数はどちらも AI の実行で変わるので、まとめて取り直す。
async function refresh() {
  await Promise.all([loadTags(), loadAiStatus()]);
}

// 配色は file_detail / files 一覧と統一する。AI タグに専用色は当てない
// （このページでの区別はカードの分割が担う）。
function tagVariant(kind) {
  return kind === 'system' ? 'badge-neutral' : 'badge-primary';
}

async function loadTags() {
  try {
    // システムタグ（拡張子・種別など自動付与）は管理対象外。手動 / AI タグのみ扱う。
    tags = (await json('/api/tags/stats')).filter(t => t.kind !== 'system');
    // 存在しなくなったタグを選択から除去
    const ids = new Set(tags.map(t => t.id));
    for (const id of [...selected]) if (!ids.has(id)) selected.delete(id);
    sortTags();
    render();
  } catch (e) {
    for (const id of ['manual-tag-list', 'ai-tag-list']) {
      $(id).replaceChildren(el('div', { class: 'opacity-50 text-xs' }, '取得失敗'));
    }
  }
}

// 選択中の基準・方向で tags を並べ替える。既定は「件数の降順 → 名前の昇順」。
function sortTags() {
  const key = /** @type {HTMLSelectElement} */ ($('sort-key')).value;
  const sign = /** @type {HTMLSelectElement} */ ($('sort-dir')).value === 'desc' ? -1 : 1;
  tags.sort((a, b) => {
    const primary = key === 'count'
      ? a.count - b.count
      : a.name.localeCompare(b.name, 'ja');
    if (primary !== 0) return primary * sign;
    // 副キーの名前は方向によらず常に昇順。ここに sign を掛けると、件数の降順
    // （既定）のときだけ同数のタグが名前の逆順に並び、目当ての名前を探しにくい。
    return a.name.localeCompare(b.name, 'ja');
  });
}

// 基準変更時は方向を既定値（名前→昇順 / 件数→降順）に合わせてから並べ替える。
// 方向はその後ユーザーが手動で上書きできる。
function onSortKeyChange() {
  /** @type {HTMLSelectElement} */ ($('sort-dir')).value =
    /** @type {HTMLSelectElement} */ ($('sort-key')).value === 'count' ? 'desc' : 'asc';
  applySort();
}

// 並べ替えコントロール変更時。再取得せず手元の一覧だけ並べ替える。
function applySort() {
  sortTags();
  render();
}

// 手動タグと自動生成タグはできることが違う（改名・削除・合流は手動タグだけ）
// ので、リストごと分ける。混ぜて行ごとに操作を出し分けると、押せる行と押せない
// 行が入り混じって読みづらい。
function render() {
  renderList($('manual-tag-list'), tags.filter(t => t.kind !== 'ai'), true);
  renderList($('ai-tag-list'), tags.filter(t => t.kind === 'ai'), false);
  updateMergeBar();
}

function renderList(box, list, editable) {
  if (list.length === 0) {
    box.replaceChildren(el('div', { class: 'opacity-50 text-xs' }, 'タグなし'));
    return;
  }
  box.replaceChildren(...list.map(t =>
    el('div', { class: 'flex items-center gap-2 row-compact' }, [
      // 自動生成タグは選択できない（合流はサーバ側でも拒否される）。
      editable
        ? el('input', {
            type: 'checkbox', class: 'checkbox checkbox-xs',
            checked: selected.has(t.id),
            onchange: (/** @type {Event} */ e) =>
              toggleSelect(t.id, /** @type {HTMLInputElement} */ (e.target).checked),
          })
        : null,
      // 種別はカードの見出しで分かるので、バッジにアイコンは付けない。
      el('span', { class: `badge badge-sm ${tagVariant(t.kind)} gap-1` }, t.name),
      el('span', { class: 'text-xs opacity-50' }, `${t.count} 件`),
      el('span', { class: 'flex-1' }),
      editable
        ? el('button', { class: 'btn btn-xs btn-ghost', onclick: () => renameTag(t.id) }, '名前変更')
        : null,
      editable
        ? el('button', { class: 'btn btn-xs btn-error btn-outline', onclick: () => deleteTag(t.id) }, '削除')
        : null,
    ])));
}

// ---- AI タグの一括割り当て ----
// スコープごとの対象件数。選択肢の表示と確認ダイアログに出すため保持する。
let aiCounts = { missing: 0, stale: 0, all: 0 };
// 現在のモデル名。画面には出さず、確認ダイアログでだけ見せる。
let aiModel = '';

const AI_SCOPE_LABEL = {
  missing: '未割り当てのみ',
  stale: '未割り当て + モデル変更分',
  all: 'すべて',
};

async function loadAiStatus() {
  const box = $('ai-assign');
  let info;
  try {
    info = await json('/api/ai-tags');
  } catch (e) {
    box.classList.add('hidden');
    return;
  }
  // AI が無効なら実行しても何も起きないので、操作自体を出さない。
  if (!info.enabled) { box.classList.add('hidden'); return; }
  box.classList.remove('hidden');
  aiCounts = { missing: info.missing, stale: info.stale, all: info.all };
  aiModel = info.current_model;
  // どれを選ぶと何件動くのかを、選ぶ前に選択肢そのもので見せる。
  for (const opt of /** @type {HTMLSelectElement} */ ($('ai-scope')).options) {
    opt.textContent = `${AI_SCOPE_LABEL[opt.value]}（${aiCounts[opt.value] ?? 0} 件）`;
  }
}

async function runAiAssign() {
  const scope = /** @type {HTMLSelectElement} */ ($('ai-scope')).value;
  const count = aiCounts[scope] ?? 0;
  if (count === 0) { uiToast('対象のファイルがありません', 'info'); return; }
  const ok = await uiConfirm(
    `${AI_SCOPE_LABEL[scope]}の ${count} 件に AI タグを割り当て直しますか？\n` +
    `モデル: ${aiModel} / 1 枚あたり数十秒かかります。`,
    // すべては生成済みの分まで作り直す（時間と API 費用がかかる）ので警告色にする。
    { danger: scope === 'all', okText: '開始' }
  );
  if (!ok) return;

  try {
    const r = await json('/api/ai-tags/regenerate', { method: 'POST', body: { scope } });
    uiToast(
      `${r.enqueued} 件を投入しました` +
      (r.already_queued ? `（${r.already_queued} 件は処理待ち）` : ''),
      'success'
    );
    await loadAiStatus();
  } catch (e) {
    uiToast('投入に失敗しました: ' + e.message, 'error');
  }
}

function tagById(id) { return tags.find(t => t.id === id); }

function toggleSelect(id, on) {
  if (on) selected.add(id); else selected.delete(id);
  updateMergeBar();
}

function clearSelection() {
  selected.clear();
  render();
}

function updateMergeBar() {
  const bar = $('merge-bar');
  const active = selected.size > 0;
  $('merge-count').textContent = String(selected.size);
  bar.classList.toggle('hidden', !active);
  // 固定バーが最下行に重ならないよう、表示中はコンテナ下部に余白を確保する
  $('main').classList.toggle('pb-24', active);
  // 合流は 2 件以上で有効
  /** @type {HTMLButtonElement} */ ($('merge-btn')).disabled = selected.size < 2;
}

// ---- 追加 ----
async function createTag() {
  const r = await uiPrompt({
    title: 'タグの作成', okText: '作成',
    fields: [{ name: 'name', label: 'タグ名', placeholder: '例: 仕事' }],
  });
  if (!r || !r.name.trim()) return;
  try {
    await json('/api/tags', { method: 'POST', body: { name: r.name.trim() } });
    uiToast('タグを作成しました', 'success');
    await loadTags();
  } catch (e) { uiToast('作成に失敗しました: ' + e.message, 'error'); }
}

// ---- 名前変更 ----
async function renameTag(id) {
  const t = tagById(id);
  if (!t) return;
  const r = await uiPrompt({
    title: 'タグ名の変更', okText: '変更',
    fields: [{ name: 'name', label: 'タグ名', value: t.name }],
  });
  if (!r || !r.name.trim() || r.name.trim() === t.name) return;
  try {
    await json('/api/tags/' + id, { method: 'PATCH', body: { name: r.name.trim() } });
    uiToast('タグ名を変更しました', 'success');
    await loadTags();
  } catch (e) { uiToast('変更に失敗しました: ' + e.message, 'error'); }
}

// ---- 削除 ----
async function deleteTag(id) {
  const t = tagById(id);
  if (!t) return;
  const note = t.count > 0 ? `\n${t.count} 件のファイルからこのタグが外れます。` : '';
  if (!await uiConfirm(`タグ「${t.name}」を削除しますか？${note}`, { danger: true, okText: '削除' })) return;
  try {
    await json('/api/tags/' + id, { method: 'DELETE' });
    selected.delete(id);
    uiToast('タグを削除しました', 'success');
    await loadTags();
  } catch (e) { uiToast('削除に失敗しました: ' + e.message, 'error'); }
}

// ---- 合流 ----
function openMerge() {
  if (selected.size < 2) return;
  const chosen = tags.filter(t => selected.has(t.id));
  // 既定の合流先は割り当て数が最も多いタグ
  let defaultTarget = chosen[0];
  for (const t of chosen) if (t.count > defaultTarget.count) defaultTarget = t;
  $('merge-options').replaceChildren(...chosen.map(t =>
    el('label', { class: 'flex items-center gap-2 row-compact cursor-pointer' }, [
      el('input', {
        type: 'radio', name: 'merge-target', class: 'radio radio-xs',
        value: t.id, checked: t.id === defaultTarget.id,
      }),
      // 合流できるのは手動タグだけなので、種別アイコンは要らない。
      el('span', { class: `badge badge-sm ${tagVariant(t.kind)} gap-1` }, t.name),
      el('span', { class: 'text-xs opacity-50' }, `${t.count} 件`),
    ])));
  /** @type {HTMLDialogElement} */ ($('merge-modal')).showModal();
}

async function confirmMerge() {
  const sel = /** @type {HTMLInputElement|null} */ (document.querySelector('input[name="merge-target"]:checked'));
  if (!sel) return;
  const targetId = sel.value;
  const sourceIds = [...selected].filter(id => id !== targetId);
  if (sourceIds.length === 0) return;
  try {
    await json('/api/tags/merge', {
      method: 'POST',
      body: { source_ids: sourceIds, target_id: targetId },
    });
    /** @type {HTMLDialogElement} */ ($('merge-modal')).close();
    const target = tagById(targetId);
    uiToast(`${sourceIds.length} 件のタグを「${target ? target.name : ''}」に合流しました`, 'success');
    selected.clear();
    await loadTags();
  } catch (e) { uiToast('合流に失敗しました: ' + e.message, 'error'); }
}

$('merge-cancel').onclick = () => /** @type {HTMLDialogElement} */ ($('merge-modal')).close();
$('merge-confirm').onclick = confirmMerge;
init();

// テンプレートのインライン onclick/onchange から参照される関数を明示的に公開する。
// (toggleSelect / renameTag / deleteTag は el() のクロージャ直結になり公開不要)
Object.assign(window, {
  refresh, onSortKeyChange, applySort, clearSelection, createTag, openMerge, runAiAssign,
});
})();
