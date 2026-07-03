// シリーズ設定ページ（/ui/series/:id）のロジック。series_settings.html のインライン <script> から切り出した静的ファイル（issue #50）。
// /ui/pages/series_settings.js で配信される。
// URL: /ui/series/<id>
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {
const seriesId = decodeURIComponent((location.pathname.match(/\/ui\/series\/([^/]+)/) || [])[1] || '');
let members = [];          // [{ file_id, display_name }] 現在の表示順
let savedOrder = [];       // 最後に保存された file_id の順序（dirty 判定用）
let sortOrder = 'created_asc'; // 現在の並び順設定
let dragIndex = null;      // ドラッグ中の行インデックス

async function init() {
  const me = await requireAuth();
  if (!me) return;
  if (!seriesId) { uiToast('シリーズIDが不正です', 'error'); return; }
  $('main').classList.remove('hidden');
  await loadDetail();
}

async function loadDetail() {
  let data;
  try {
    data = await json(`/api/series/${seriesId}/detail`);
  } catch (e) {
    uiToast('シリーズの取得に失敗しました: ' + e.message, 'error');
    return;
  }
  $('ss-name').value = data.name || '';
  $('ss-desc').value = data.description || '';
  sortOrder = data.sort_order || 'created_asc';
  $('ss-sort').value = sortOrder;
  // 戻り先はこのシリーズのファイル一覧にしておく
  $('ss-back').href = `/ui/files?series=${encodeURIComponent(seriesId)}`;
  members = (data.members || []).map(m => ({ file_id: m.file_id, display_name: m.display_name }));
  savedOrder = members.map(m => m.file_id);
  renderList();
}

const isManual = () => sortOrder === 'manual';

function isDirty() {
  const cur = members.map(m => m.file_id);
  return cur.length !== savedOrder.length || cur.some((id, i) => id !== savedOrder[i]);
}

function refreshDirty() {
  // 保存対象があるのはマニュアル順かつ並びが変わっているときだけ。
  $('ss-save-order').disabled = !(isManual() && isDirty());
}

// 並び順ドロップダウンの変更。マニュアル以外は即時適用し、サーバ並びを再取得する。
// マニュアルを選んだ場合は現在の表示順をそのまま手動順として確定する。
async function onSortChange() {
  const val = $('ss-sort').value;
  if (val === 'manual') {
    await saveOrder('マニュアル順に切り替えました');
    return;
  }
  try {
    await json(`/api/series/${seriesId}/sort`, { method: 'PUT', body: { sort_order: val } });
    uiToast('並び順を変更しました', 'success');
    await loadDetail();
  } catch (e) {
    uiToast('並び順の変更に失敗しました: ' + e.message, 'error');
    $('ss-sort').value = sortOrder; // 失敗時は元に戻す
  }
}

function renderList() {
  const el = $('ss-list');
  if (!members.length) {
    el.innerHTML = '<li class="opacity-50 text-sm py-2">このシリーズにはまだ項目がありません。</li>';
    refreshDirty();
    return;
  }
  el.innerHTML = members.map((m, i) => `
    <li class="ss-row flex items-center gap-2 rounded bg-base-200/60 px-2 py-1.5" draggable="true" data-idx="${i}">
      <span class="ss-handle opacity-50 select-none" title="ドラッグで並び替え">⠿</span>
      <span class="text-xs opacity-60 tabular-nums w-6 text-right shrink-0">${i + 1}</span>
      <a href="/ui/files/${m.file_id}" class="text-sm link link-hover truncate flex-1 min-w-0"
         title="${escapeHtml(m.display_name)}">${escapeHtml(m.display_name)}</a>
      <div class="join shrink-0">
        <button type="button" class="btn btn-xs join-item" title="上へ"
                onclick="move(${i}, -1)" ${i === 0 ? 'disabled' : ''}>▲</button>
        <button type="button" class="btn btn-xs join-item" title="下へ"
                onclick="move(${i}, 1)" ${i === members.length - 1 ? 'disabled' : ''}>▼</button>
      </div>
      <button type="button" class="btn btn-xs btn-ghost shrink-0" title="このシリーズから外す"
              onclick="removeMember(${i})">×</button>
    </li>`).join('');
  bindDnd();
  refreshDirty();
}

// 手作業で並び替えたら並び順設定を「マニュアル順」に切り替える。
function markManual() {
  if (sortOrder !== 'manual') {
    sortOrder = 'manual';
    $('ss-sort').value = 'manual';
  }
}

// ---- ▲▼ による移動 ----
function move(i, dir) {
  const j = i + dir;
  if (j < 0 || j >= members.length) return;
  markManual();
  [members[i], members[j]] = [members[j], members[i]];
  renderList();
}

// ---- HTML5 ドラッグ＆ドロップ ----
function bindDnd() {
  $('ss-list').querySelectorAll('.ss-row').forEach(row => {
    row.addEventListener('dragstart', e => {
      dragIndex = parseInt(row.dataset.idx, 10);
      row.classList.add('dragging');
      e.dataTransfer.effectAllowed = 'move';
    });
    row.addEventListener('dragend', () => {
      row.classList.remove('dragging');
      $('ss-list').querySelectorAll('.ss-row').forEach(r => r.classList.remove('drag-over'));
    });
    row.addEventListener('dragover', e => {
      e.preventDefault();
      e.dataTransfer.dropEffect = 'move';
      row.classList.add('drag-over');
    });
    row.addEventListener('dragleave', () => row.classList.remove('drag-over'));
    row.addEventListener('drop', e => {
      e.preventDefault();
      const to = parseInt(row.dataset.idx, 10);
      if (dragIndex === null || dragIndex === to) return;
      markManual();
      const [moved] = members.splice(dragIndex, 1);
      members.splice(to, 0, moved);
      dragIndex = null;
      renderList();
    });
  });
}

// ---- 保存系 ----
async function saveName() {
  const name = $('ss-name').value.trim();
  if (!name) { uiToast('シリーズ名を入力してください', 'warning'); return; }
  const description = $('ss-desc').value.trim();
  try {
    await json(`/api/series/${seriesId}`, {
      method: 'PATCH',
      body: { name, description: description || null },
    });
    uiToast('名称を保存しました', 'success');
  } catch (e) {
    uiToast('名称の保存に失敗しました: ' + e.message, 'error');
  }
}

// 現在の表示順を手動順として保存する。サーバ側で sort_order が manual に切り替わる。
async function saveOrder(successMsg) {
  const file_ids = members.map(m => m.file_id);
  try {
    await json(`/api/series/${seriesId}/members/order`, {
      method: 'PUT',
      body: { file_ids },
    });
    uiToast(successMsg || '並び順を保存しました', 'success');
    await loadDetail();
  } catch (e) {
    uiToast('並び順の保存に失敗しました: ' + e.message, 'error');
  }
}

async function removeMember(i) {
  const m = members[i];
  if (!m) return;
  if (!await uiConfirm(`「${m.display_name}」をこのシリーズから外しますか？`, { danger: true })) return;
  try {
    await api(`/api/series/${seriesId}/members/${m.file_id}`, { method: 'DELETE' });
    uiToast('シリーズから外しました', 'success');
    await loadDetail();
  } catch (e) {
    uiToast('取り外しに失敗しました: ' + e.message, 'error');
  }
}

init();

// テンプレート／生成 HTML のインライン onclick/onchange から参照される関数を明示的に公開する。
Object.assign(window, { onSortChange, saveName, saveOrder, move, removeMember });
})();
