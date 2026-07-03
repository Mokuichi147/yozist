// @ts-check
// ユーザー設定ページ（/ui/settings）のロジック。settings.html のインライン <script> から切り出した静的ファイル（issue #50）。
// /ui/pages/settings.js で配信される。
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {
function showMsg(el, text, ok) {
  el.textContent = text;
  el.className = 'text-sm mt-2 ' + (ok ? 'text-success' : 'text-error');
}

let currentUser = null;

async function init() {
  const me = await requireAuth();
  if (!me) return;
  currentUser = me.user;
  $('acc-username').textContent = me.user.username;
  /** @type {HTMLInputElement} */ ($('dn-input')).value = me.user.display_name || '';

  const groupsEl = $('acc-groups');
  if (me.groups && me.groups.length) {
    groupsEl.innerHTML = me.groups.map(g => {
      const adminBadge = g.is_admin
        ? ' <span class="badge badge-warning badge-sm">管理者</span>' : '';
      return `<span class="inline-flex items-center gap-1">
        <span class="badge badge-ghost badge-sm">${escapeHtml(g.name)}</span>${adminBadge}</span>`;
    }).join('');
  } else {
    groupsEl.innerHTML = '<span class="opacity-50">所属なし</span>';
  }

  $('main').classList.remove('hidden');
}

async function saveDisplayName() {
  const el = $('dn-msg');
  el.classList.remove('hidden');
  const display_name = /** @type {HTMLInputElement} */ ($('dn-input')).value.trim();
  /** @type {HTMLButtonElement} */ ($('dn-save')).disabled = true;
  try {
    const user = await json('/api/auth/me', { method: 'PATCH', body: { display_name } });
    currentUser = user;
    $('me').textContent = user.display_name || user.username;
    showMsg(el, 'ユーザー名を変更しました。', true);
  } catch (e) {
    showMsg(el, '変更に失敗しました: ' + e.message, false);
  } finally {
    /** @type {HTMLButtonElement} */ ($('dn-save')).disabled = false;
  }
}

async function changePassword(e) {
  e.preventDefault();
  const el = $('pw-msg');
  el.classList.remove('hidden');
  const current = /** @type {HTMLInputElement} */ ($('pw-current')).value;
  const next = /** @type {HTMLInputElement} */ ($('pw-new')).value;
  const confirm = /** @type {HTMLInputElement} */ ($('pw-confirm')).value;
  if (next.length < 8) { showMsg(el, '新パスワードは 8 文字以上で入力してください。', false); return; }
  if (next !== confirm) { showMsg(el, '新パスワード（確認）が一致しません。', false); return; }
  const btn = /** @type {HTMLButtonElement} */ ($('pw-form').querySelector('button[type="submit"]'));
  btn.disabled = true;
  try {
    await json('/api/auth/password', {
      method: 'POST',
      body: { current_password: current, new_password: next },
    });
    /** @type {HTMLFormElement} */ ($('pw-form')).reset();
    showMsg(el, 'パスワードを変更しました。', true);
  } catch (e2) {
    showMsg(el, '変更に失敗しました: ' + e2.message, false);
  } finally {
    btn.disabled = false;
  }
}

$('dn-save').onclick = saveDisplayName;
$('pw-form').addEventListener('submit', changePassword);
init();
})();
