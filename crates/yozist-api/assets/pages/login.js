// ログインページ（/ui/login）のロジック。login.html のインライン <script> から切り出した静的ファイル（issue #50）。
// @ts-check
// /ui/pages/login.js で配信される。
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {
function nextUrl() {
  const p = new URLSearchParams(location.search).get('next');
  if (p && p.startsWith('/ui')) return p;
  return '/ui';
}

function showError(msg) {
  const el = $('lf-error');
  el.textContent = msg;
  el.classList.remove('hidden');
}

async function doLogin() {
  $('lf-error').classList.add('hidden');
  const username = /** @type {HTMLInputElement} */ ($('lf-user')).value;
  const password = /** @type {HTMLInputElement} */ ($('lf-pw')).value;
  try {
    const r = await fetch('/api/auth/login', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ username, password }),
    });
    if (!r.ok) throw new Error('ログイン失敗');
    const d = await r.json();
    localStorage.setItem('yozist_token', d.token);
    // ブラウザのパスワードマネージャーに保存をリクエスト
    // （PasswordCredential は lib.dom に型定義が無いため window 経由で参照する）
    const PasswordCredential = /** @type {*} */ (window).PasswordCredential;
    if (PasswordCredential) {
      try {
        const cred = new PasswordCredential({ id: username, password, name: username });
        await navigator.credentials.store(cred);
      } catch (_) { /* ユーザーが拒否した場合などは無視 */ }
    }
    // ページ遷移することでブラウザのパスワード保存プロンプトを確実に発火させる
    location.href = nextUrl();
  } catch (e) {
    showError(String(e.message || e));
  }
}

async function doRegister() {
  $('lf-error').classList.add('hidden');
  const username = /** @type {HTMLInputElement} */ ($('lf-user')).value;
  const password = /** @type {HTMLInputElement} */ ($('lf-pw')).value;
  if (!username || !password) { showError('username / password を入力してください'); return; }
  try {
    const r = await fetch('/api/auth/register', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ username, password }),
    });
    if (!r.ok) throw new Error('登録失敗: ' + await r.text());
    await doLogin();
  } catch (e) {
    showError(String(e.message || e));
  }
}

$('lf-form').addEventListener('submit', (e) => { e.preventDefault(); doLogin(); });
$('register-btn').addEventListener('click', (e) => { e.preventDefault(); doRegister(); });

// 既にログイン済みなら遷移先に戻す
(async () => {
  const t = localStorage.getItem('yozist_token');
  if (!t) return;
  try {
    const r = await fetch('/api/auth/me', { headers: { Authorization: 'Bearer ' + t } });
    if (!r.ok) return;
    const me = await r.json();
    if (!me.anonymous) location.href = nextUrl();
  } catch (_) {}
})();
})();
