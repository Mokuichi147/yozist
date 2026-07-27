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
  await Promise.all([loadGroups(), loadShares(), loadAudit(), loadAiStatus()]);
}

// ---- ai auto tags ----
// スコープごとの対象件数。確認ダイアログに出すため保持する。
let aiCounts = { missing: 0, stale: 0, all: 0 };

const AI_SCOPE_LABEL = {
  missing: '未生成',
  stale: 'このモデルで未生成',
  all: 'すべて',
};

async function loadAiStatus() {
  const card = $('ai-card');
  let info;
  try {
    info = await json('/api/ai-tags');
  } catch (e) {
    card.classList.add('hidden');
    return;
  }
  if (!info.enabled) { card.classList.add('hidden'); return; }
  card.classList.remove('hidden');
  aiCounts = { missing: info.missing, stale: info.stale, all: info.all };
  $('ai-model').replaceChildren(info.current_model);
  $('ai-result').replaceChildren(
    `未生成 ${info.missing} 件 / このモデルで未生成 ${info.stale} 件 / 全体 ${info.all} 件`
  );
}

async function regenerateAiTags(scope) {
  const count = aiCounts[scope] ?? 0;
  if (count === 0) { uiToast('対象のファイルがありません', 'info'); return; }
  const label = AI_SCOPE_LABEL[scope] || scope;
  const ok = await uiConfirm(
    `${label}の ${count} 件について AI タグを生成し直しますか？\n` +
    '1 枚あたり数十秒かかります。',
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

// ---- groups ----
async function loadGroups() {
  const box = $('group-list');
  try {
    const list = await json('/api/groups');
    if (list.length === 0) {
      box.replaceChildren(el('div', { class: 'opacity-50 text-xs' }, 'グループなし'));
      return;
    }
    box.replaceChildren(...list.map(g =>
      el('div', { class: 'flex items-center justify-between row-compact' }, [
        el('div', {}, el('span', { class: 'font-semibold' }, g.name)),
        el('button', { class: 'btn btn-xs', onclick: () => manageGroupMembers(g.id, g.name) }, 'メンバー'),
      ])));
  } catch (e) {
    box.replaceChildren(el('div', { class: 'opacity-50 text-xs' }, '取得失敗'));
  }
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
  const mBox = $('gm-members');
  if (members.length === 0) {
    mBox.replaceChildren(el('div', { class: 'opacity-50 text-xs' }, 'メンバーなし'));
  } else {
    mBox.replaceChildren(...members.map(uid =>
      el('div', { class: 'flex items-center justify-between row-compact' }, [
        el('span', {}, usersById[uid] || uid.slice(0, 8)),
        el('button', {
          class: 'btn btn-xs btn-error btn-outline',
          onclick: () => removeMember(uid),
        }, '除外'),
      ])));
  }
  // 追加候補 = 未所属ユーザー
  const memberSet = new Set(members);
  const candidates = users.filter(u => !memberSet.has(u.id));
  const sel = /** @type {HTMLSelectElement} */ ($('gm-add-select'));
  sel.replaceChildren(...(candidates.length === 0
    ? [el('option', { value: '' }, '(追加できるユーザーなし)')]
    : candidates.map(u => el('option', { value: u.id }, u.username))));
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
  const box = $('share-list');
  try {
    const list = await json('/api/shares');
    if (list.length === 0) {
      box.replaceChildren(el('div', { class: 'opacity-50 text-xs' }, '発行済み共有なし'));
      return;
    }
    box.replaceChildren(...list.map(s =>
      el('div', { class: 'flex items-center justify-between row-compact' }, [
        el('span', {}, [
          el('span', { class: 'badge badge-ghost badge-sm' }, s.kind), ' ',
          el('span', { class: 'opacity-70' }, s.target_id.slice(0, 8) + '…'),
          s.revoked_at && ' ',
          s.revoked_at && el('span', { class: 'badge badge-error badge-sm' }, '失効'),
        ]),
        !s.revoked_at && el('button', {
          class: 'btn btn-xs btn-error btn-outline',
          onclick: () => revokeShare(s.jti),
        }, '失効'),
      ])));
  } catch (e) {
    box.replaceChildren(el('div', { class: 'opacity-50 text-xs' }, '取得失敗'));
  }
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
  const box = $('audit-list');
  try {
    const list = await json('/api/audit?limit=15');
    if (list.length === 0) {
      box.replaceChildren(el('div', { class: 'opacity-50 text-xs' }, '監査記録なし'));
      return;
    }
    box.replaceChildren(...list.map(a => {
      const ok = a.result.startsWith('ok');
      return el('div', { class: 'row-compact text-xs' }, [
        ok ? el('span', { class: 'text-success' }, '✓') : el('span', { class: 'text-error' }, '✗'),
        ' ',
        el('span', { class: 'opacity-70' }, fmtTs(a.timestamp)), ' ',
        el('span', { class: 'font-semibold' }, a.actor_label || '-'), ' ',
        `${a.action} ${a.target_type || ''} `,
        a.target_ref && el('span', { class: 'opacity-60' }, a.target_ref.slice(0, 8) + '…'),
      ]);
    }));
  } catch (e) {
    box.replaceChildren(el('div', { class: 'opacity-50 text-xs' }, '取得失敗'));
  }
}

$('gm-close').onclick = () => /** @type {HTMLDialogElement} */ ($('gm-modal')).close();
$('gm-add-btn').onclick = addMember;
init();

// テンプレートのインライン onclick から参照される関数を明示的に公開する。
// (manageGroupMembers / removeMember / revokeShare は el() のクロージャ直結になり公開不要)
Object.assign(window, { createGroup, loadShares, loadAudit, regenerateAiTags });
})();
