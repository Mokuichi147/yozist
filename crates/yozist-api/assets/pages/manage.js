// @ts-check
// 管理ページ（/ui/manage）のロジック。manage.html のインライン <script> から切り出した静的ファイル（issue #50）。
// /ui/pages/manage.js で配信される。
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {
async function init() {
  const me = await requireAuth();
  if (!me) return;
  $('main').classList.remove('hidden');
  await refresh();
}

async function refresh() {
  await Promise.all([loadGroups(), loadShares(), loadAudit()]);
}

// ---- groups ----
async function loadGroups() {
  const el = $('group-list');
  try {
    const list = await json('/api/groups');
    if (list.length === 0) { el.innerHTML = '<div class="opacity-50 text-xs">グループなし</div>'; return; }
    el.innerHTML = list.map(g => {
      return `<div class="flex items-center justify-between row-compact">
        <div><span class="font-semibold">${escapeHtml(g.name)}</span></div>
        <button class="btn btn-xs" onclick="manageGroupMembers('${g.id}','${escapeHtml(g.name)}')">メンバー</button>
      </div>`;
    }).join('');
  } catch (e) { el.innerHTML = '<div class="opacity-50 text-xs">取得失敗</div>'; }
}

async function createGroup() {
  const r = await uiPrompt({
    title: 'グループの作成', okText: '作成',
    fields: [{ name: 'name', label: 'グループ名', placeholder: '例: 編集チーム' }],
  });
  if (!r || !r.name.trim()) return;
  try {
    await json('/api/groups', { method: 'POST', body: { name: r.name.trim() } });
    uiToast('グループを作成しました', 'success');
    await loadGroups();
  } catch (e) { uiToast('作成に失敗しました: ' + e.message, 'error'); }
}

let gmGroupId = null;
async function manageGroupMembers(groupId, groupName) {
  gmGroupId = groupId;
  $('gm-name').textContent = groupName;
  await renderGroupMembers();
  /** @type {HTMLDialogElement} */ ($('gm-modal')).showModal();
}

async function renderGroupMembers() {
  const [members, users] = await Promise.all([
    json(`/api/groups/${gmGroupId}/members`),
    json('/api/users'),
  ]);
  const usersById = Object.fromEntries(users.map(u => [u.id, u.username]));
  const mEl = $('gm-members');
  mEl.innerHTML = members.length === 0
    ? '<div class="opacity-50 text-xs">メンバーなし</div>'
    : members.map(uid => `<div class="flex items-center justify-between row-compact">
        <span>${escapeHtml(usersById[uid] || uid.slice(0, 8))}</span>
        <button class="btn btn-xs btn-error btn-outline" onclick="removeMember('${uid}')">除外</button>
      </div>`).join('');
  // 追加候補 = 未所属ユーザー
  const memberSet = new Set(members);
  const candidates = users.filter(u => !memberSet.has(u.id));
  const sel = /** @type {HTMLSelectElement} */ ($('gm-add-select'));
  sel.innerHTML = candidates.length === 0
    ? '<option value="">(追加できるユーザーなし)</option>'
    : candidates.map(u => `<option value="${u.id}">${escapeHtml(u.username)}</option>`).join('');
  sel.disabled = candidates.length === 0;
  /** @type {HTMLButtonElement} */ ($('gm-add-btn')).disabled = candidates.length === 0;
}

async function addMember() {
  const uid = /** @type {HTMLSelectElement} */ ($('gm-add-select')).value;
  if (!uid) return;
  try {
    await api(`/api/groups/${gmGroupId}/members`, {
      method: 'POST',
      body: JSON.stringify({ user_id: uid }),
      headers: { 'content-type': 'application/json' },
    });
    uiToast('追加しました', 'success');
    await renderGroupMembers();
  } catch (e) { uiToast('追加に失敗しました', 'error'); }
}

async function removeMember(uid) {
  if (!await uiConfirm('このメンバーを除外しますか？', { danger: true })) return;
  try {
    await api(`/api/groups/${gmGroupId}/members/${uid}`, { method: 'DELETE' });
    uiToast('除外しました', 'success');
    await renderGroupMembers();
  } catch (e) { uiToast('除外に失敗しました', 'error'); }
}

// ---- active shares ----
async function loadShares() {
  const el = $('share-list');
  try {
    const list = await json('/api/shares');
    if (list.length === 0) { el.innerHTML = '<div class="opacity-50 text-xs">発行済み共有なし</div>'; return; }
    el.innerHTML = list.map(s => {
      const revoked = s.revoked_at ? ' <span class="badge badge-error badge-sm">失効</span>' : '';
      const fullJti = s.jti;
      return `<div class="flex items-center justify-between row-compact">
        <span><span class="badge badge-ghost badge-sm">${escapeHtml(s.kind)}</span>
          <span class="opacity-70">${s.target_id.slice(0,8)}…</span>${revoked}</span>
        ${!s.revoked_at ? `<button class="btn btn-xs btn-error btn-outline" onclick="revokeShare('${fullJti}')">失効</button>` : ''}
      </div>`;
    }).join('');
  } catch (e) { el.innerHTML = '<div class="opacity-50 text-xs">取得失敗</div>'; }
}

async function revokeShare(jti) {
  if (!await uiConfirm('この共有を失効しますか？', { danger: true })) return;
  const r = await api(`/api/shares/${jti}`, { method: 'DELETE' });
  if (!r.ok) { uiToast('失効に失敗しました', 'error'); return; }
  uiToast('失効しました', 'success');
  await loadShares();
}

// ---- audit log ----
async function loadAudit() {
  const el = $('audit-list');
  try {
    const list = await json('/api/audit?limit=15');
    if (list.length === 0) { el.innerHTML = '<div class="opacity-50 text-xs">監査記録なし</div>'; return; }
    el.innerHTML = list.map(a => {
      const ok = a.result.startsWith('ok');
      const status = ok
        ? '<span class="text-success">✓</span>'
        : '<span class="text-error">✗</span>';
      const time = fmtTs(a.timestamp);
      return `<div class="row-compact text-xs">
        ${status} <span class="opacity-70">${time}</span>
        <span class="font-semibold">${escapeHtml(a.actor_label || '-')}</span>
        ${escapeHtml(a.action)} ${escapeHtml(a.target_type || '')}
        ${a.target_ref ? '<span class="opacity-60">'+a.target_ref.slice(0,8)+'…</span>' : ''}
      </div>`;
    }).join('');
  } catch (e) { el.innerHTML = '<div class="opacity-50 text-xs">取得失敗</div>'; }
}

$('gm-close').onclick = () => /** @type {HTMLDialogElement} */ ($('gm-modal')).close();
$('gm-add-btn').onclick = addMember;
init();

// テンプレート／生成 HTML のインライン onclick から参照される関数を明示的に公開する。
Object.assign(window, {
  createGroup, loadShares, loadAudit, manageGroupMembers, removeMember, revokeShare,
});
})();
