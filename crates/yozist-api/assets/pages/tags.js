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
  await loadTags();
}

function tagVariant(kind) {
  return kind === 'system' ? 'badge-neutral'
       : kind === 'ai' ? 'badge-warning'
       : 'badge-primary';
}
function tagIcon(kind) {
  return kind === 'system' ? ' ⚙' : kind === 'ai' ? ' 🤖' : '';
}

async function loadTags() {
  const el = $('tag-list');
  try {
    // システムタグ（拡張子・種別など自動付与）は管理対象外。手動 / AI タグのみ扱う。
    tags = (await json('/api/tags/stats')).filter(t => t.kind !== 'system');
    // 存在しなくなったタグを選択から除去
    const ids = new Set(tags.map(t => t.id));
    for (const id of [...selected]) if (!ids.has(id)) selected.delete(id);
    sortTags();
    render();
  } catch (e) {
    el.innerHTML = '<div class="opacity-50 text-xs">取得失敗</div>';
  }
}

// 選択中の基準・方向で tags を並べ替える。件数が同じ場合は名前で安定させる。
function sortTags() {
  const key = $('sort-key').value;
  const sign = $('sort-dir').value === 'desc' ? -1 : 1;
  tags.sort((a, b) => {
    let d;
    if (key === 'count') d = a.count - b.count;
    else d = a.name.localeCompare(b.name, 'ja');
    if (d === 0 && key !== 'name') d = a.name.localeCompare(b.name, 'ja');
    return d * sign;
  });
}

// 基準変更時は方向を既定値（名前→昇順 / 件数→降順）に合わせてから並べ替える。
// 方向はその後ユーザーが手動で上書きできる。
function onSortKeyChange() {
  $('sort-dir').value = $('sort-key').value === 'count' ? 'desc' : 'asc';
  applySort();
}

// 並べ替えコントロール変更時。再取得せず手元の一覧だけ並べ替える。
function applySort() {
  sortTags();
  render();
}

function render() {
  const el = $('tag-list');
  if (tags.length === 0) {
    el.innerHTML = '<div class="opacity-50 text-xs">タグなし</div>';
    updateMergeBar();
    return;
  }
  el.innerHTML = tags.map(t => {
    const checked = selected.has(t.id) ? 'checked' : '';
    return `<div class="flex items-center gap-2 row-compact">
      <input type="checkbox" class="checkbox checkbox-xs" ${checked}
             onchange="toggleSelect('${t.id}', this.checked)">
      <span class="badge badge-sm ${tagVariant(t.kind)} gap-1">${escapeHtml(t.name)}${tagIcon(t.kind)}</span>
      <span class="text-xs opacity-50">${t.count} 件</span>
      <span class="flex-1"></span>
      <button class="btn btn-xs btn-ghost" onclick="renameTag('${t.id}')">名前変更</button>
      <button class="btn btn-xs btn-error btn-outline" onclick="deleteTag('${t.id}')">削除</button>
    </div>`;
  }).join('');
  updateMergeBar();
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
  $('merge-count').textContent = selected.size;
  bar.classList.toggle('hidden', !active);
  // 固定バーが最下行に重ならないよう、表示中はコンテナ下部に余白を確保する
  $('main').classList.toggle('pb-24', active);
  // 合流は 2 件以上で有効
  $('merge-btn').disabled = selected.size < 2;
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
  $('merge-options').innerHTML = chosen.map(t => `
    <label class="flex items-center gap-2 row-compact cursor-pointer">
      <input type="radio" name="merge-target" class="radio radio-xs" value="${t.id}"
             ${t.id === defaultTarget.id ? 'checked' : ''}>
      <span class="badge badge-sm ${tagVariant(t.kind)} gap-1">${escapeHtml(t.name)}${tagIcon(t.kind)}</span>
      <span class="text-xs opacity-50">${t.count} 件</span>
    </label>`).join('');
  $('merge-modal').showModal();
}

async function confirmMerge() {
  const sel = document.querySelector('input[name="merge-target"]:checked');
  if (!sel) return;
  const targetId = sel.value;
  const sourceIds = [...selected].filter(id => id !== targetId);
  if (sourceIds.length === 0) return;
  try {
    await json('/api/tags/merge', {
      method: 'POST',
      body: { source_ids: sourceIds, target_id: targetId },
    });
    $('merge-modal').close();
    const target = tagById(targetId);
    uiToast(`${sourceIds.length} 件のタグを「${target ? target.name : ''}」に合流しました`, 'success');
    selected.clear();
    await loadTags();
  } catch (e) { uiToast('合流に失敗しました: ' + e.message, 'error'); }
}

$('merge-cancel').onclick = () => $('merge-modal').close();
$('merge-confirm').onclick = confirmMerge;
init();

// テンプレート／生成 HTML のインライン onclick/onchange から参照される関数を明示的に公開する。
Object.assign(window, {
  loadTags, onSortKeyChange, applySort, toggleSelect, clearSelection,
  createTag, renameTag, deleteTag, openMerge,
});
})();
