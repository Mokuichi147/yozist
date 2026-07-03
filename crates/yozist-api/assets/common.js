// @ts-check
// base.html の共有スクリプト（全ページ共有）。base.html のインライン <script> から
// 切り出した静的ファイル（issue #50）。/ui/assets/common.js で配信される。
/**
 * id で DOM 要素を取得する薄いヘルパ。ページ側は要素の存在を前提に使う。
 * `.value` や `.showModal()` などサブタイプ固有のプロパティが必要な呼び出し側は、
 * HTMLInputElement 等へのインライン JSDoc キャストを付けて使う。
 * @template {HTMLElement} [T=HTMLElement]
 * @param {string} id
 * @returns {T}
 */
const $ = id => /** @type {T} */ (document.getElementById(id));
let token = localStorage.getItem('yozist_token') || '';

/**
 * Bearer トークン付きで fetch する（未ログイン時は素の fetch と同じ）。
 * @param {string} path
 * @param {RequestInit} [opts]
 * @returns {Promise<Response>}
 */
function api(path, opts = {}) {
  opts.headers = Object.assign({}, opts.headers, token ? { Authorization: 'Bearer ' + token } : {});
  return fetch(path, opts);
}
/**
 * json() が reject する API エラー。HTTP ステータスと Response を持つ。
 * @typedef {Error & { status: number, response: Response }} ApiError
 */
/**
 * JSON API 呼び出し。body がオブジェクトなら JSON 化して content-type を付け、
 * 失敗レスポンスは ApiError で reject する。204 / 空ボディは null を返す。
 * @param {string} path
 * @param {Omit<RequestInit, 'body'> & { body?: * }} [opts]
 * @returns {Promise<*>} パース済みレスポンス JSON
 */
function json(path, opts) {
  opts = opts || {};
  if (opts.body && typeof opts.body === 'object' && !(opts.body instanceof ArrayBuffer)) {
    opts.body = JSON.stringify(opts.body);
    opts.headers = Object.assign({}, opts.headers, { 'content-type': 'application/json' });
  }
  return api(path, opts).then(async r => {
    if (!r.ok) {
      const text = await r.text().catch(() => '');
      const err = /** @type {ApiError} */ (new Error(text || r.statusText));
      err.status = r.status;
      err.response = r;
      return Promise.reject(err);
    }
    if (r.status === 204) return null;
    const text = await r.text();
    return text ? JSON.parse(text) : null;
  });
}

/**
 * HTML への埋め込み用に特殊文字を実体参照へエスケープする。
 * @param {*} s 文字列以外は String() で文字列化する
 * @returns {string}
 */
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
  })[c]);
}

// /content・/commits は元エンコーディング（charset）で本文を返すため、その charset で
// デコードする。Fetch の Response.text() は常に UTF-8 デコードで charset を無視するので、
// arrayBuffer を取得して TextDecoder へ渡す必要がある。TextDecoder は既定で BOM を除去する。
// 独自ラベル "UTF-8-BOM" は素の utf-8 に、未対応ラベルや charset 無し（旧データ・バイナリ）は
// utf-8 にフォールバックする。
/**
 * @param {ArrayBuffer|Uint8Array} buf
 * @param {string|null|undefined} charset 元エンコーディングのラベル（無ければ utf-8）
 * @returns {string}
 */
function decodeBytes(buf, charset) {
  let label = 'utf-8';
  if (charset) {
    label = (charset.toLowerCase() === 'utf-8-bom') ? 'utf-8' : charset;
  }
  try {
    return new TextDecoder(label, { fatal: false }).decode(buf);
  } catch (_) {
    return new TextDecoder('utf-8', { fatal: false }).decode(buf);
  }
}

// API は timestamp を ISO 文字列で返す場合と
// time::OffsetDateTime のシリアライズ形式 ([year, day_of_year, h, m, s, ns, ...]) で返す場合がある
/**
 * @param {string|number[]|null|undefined} ts
 * @returns {string} "YYYY-MM-DD hh:mm:ss" 形式（解釈できない値は String() 化）
 */
function fmtTs(ts) {
  if (typeof ts === 'string') return ts.replace('T', ' ').slice(0, 19);
  if (Array.isArray(ts) && ts.length >= 5) {
    const [y, doy, h, m, s] = ts;
    const d = new Date(Date.UTC(y, 0, doy));
    const mm = String(d.getUTCMonth() + 1).padStart(2, '0');
    const dd = String(d.getUTCDate()).padStart(2, '0');
    const pad = n => String(n).padStart(2, '0');
    return `${y}-${mm}-${dd} ${pad(h)}:${pad(m)}:${pad(s)}`;
  }
  return String(ts ?? '');
}

function redirectToLogin() {
  const next = encodeURIComponent(location.pathname + location.search);
  location.href = '/ui/login?next=' + next;
}
function logout() {
  localStorage.removeItem('yozist_token');
  token = '';
  redirectToLogin();
}
/** @type {NodeListOf<HTMLElement>} */ (document.querySelectorAll('[data-logout]'))
  .forEach(b => b.onclick = logout);

// ログイン必須ページの共通初期化。認証OKなら me を返し、navbar のユーザー名を反映する。
/**
 * @returns {Promise<{user: {id: string, username: string, display_name?: string|null},
 *                    groups?: {name: string, is_admin: boolean}[]} | null>}
 *          未認証（リダイレクト）時は null
 */
async function requireAuth() {
  if (!token) { redirectToLogin(); return null; }
  try {
    const me = await json('/api/auth/me');
    if (me.anonymous) { logout(); return null; }
    const name = me.user.display_name || me.user.username;
    if ($('me')) $('me').textContent = name;
    if ($('me-mobile')) $('me-mobile').textContent = name;
    return me;
  } catch (e) { logout(); return null; }
}

// ---- トースト通知 (alert の置換) ----
/**
 * @param {string} msg
 * @param {'info'|'success'|'error'|'warning'} [type]
 */
function uiToast(msg, type = 'info') {
  const cont = $('ui-toasts');
  const cls = type === 'success' ? 'alert-success'
            : type === 'error' ? 'alert-error'
            : type === 'warning' ? 'alert-warning' : 'alert-info';
  const el = document.createElement('div');
  el.className = 'alert ' + cls + ' py-2 text-sm shadow';
  el.setAttribute('role', 'alert');
  el.textContent = msg;
  cont.appendChild(el);
  setTimeout(() => el.remove(), 3500);
}

// ---- 確認ダイアログ (confirm の置換) → Promise<boolean> ----
/**
 * @param {string} message
 * @param {{title?: string, danger?: boolean, okText?: string, cancelText?: string,
 *          extraText?: string}} [opts] extraText 指定時は第3の選択肢を表示する
 * @returns {Promise<boolean|'extra'>} OK=true / キャンセル・close=false / 第3選択肢='extra'
 */
function uiConfirm(message, opts = {}) {
  return new Promise(resolve => {
    const dlg = /** @type {HTMLDialogElement} */ ($('ui-confirm'));
    $('ui-confirm-title').textContent = opts.title || '確認';
    $('ui-confirm-msg').textContent = message;
    const okBtn = $('ui-confirm-ok');
    okBtn.className = 'btn btn-sm ' + (opts.danger ? 'btn-error' : 'btn-primary');
    okBtn.textContent = opts.okText || 'OK';
    const cancelBtn = $('ui-confirm-cancel');
    cancelBtn.textContent = opts.cancelText || 'キャンセル';
    // 第3の選択肢（extraText 指定時のみ表示）。resolve 値は 'extra'。
    const extraBtn = $('ui-confirm-extra');
    if (opts.extraText) {
      extraBtn.textContent = opts.extraText;
      extraBtn.classList.remove('hidden');
    } else {
      extraBtn.classList.add('hidden');
    }
    let done = false;
    const finish = v => { if (done) return; done = true; dlg.close(); resolve(v); };
    okBtn.onclick = () => finish(true);
    cancelBtn.onclick = () => finish(false);
    extraBtn.onclick = () => finish('extra');
    dlg.onclose = () => finish(false);
    dlg.showModal();
  });
}

// ---- 入力ダイアログ (prompt の置換) → Promise<object|null> ----
// fields: [{ name, label, type?('text'|'textarea'|'select'|'password'), value?, placeholder?, hint?, options?, readonly? }]
/**
 * @typedef {Object} PromptField
 * @property {string} name 返り値オブジェクトのキー
 * @property {string} [label]
 * @property {'text'|'textarea'|'select'|'password'|'number'} [type]
 * @property {string} [value]
 * @property {string} [placeholder]
 * @property {string} [hint]
 * @property {number} [rows] textarea の行数
 * @property {{value: string, label: string}[]} [options] select の選択肢
 * @property {boolean} [readonly]
 */
/**
 * @param {{title?: string, okText?: string, fields?: PromptField[]}} opts
 * @returns {Promise<Record<string, string>|null>} name → 入力値。キャンセル時は null
 */
function uiPrompt(opts) {
  return new Promise(resolve => {
    const dlg = /** @type {HTMLDialogElement} */ ($('ui-prompt'));
    $('ui-prompt-title').textContent = opts.title || '';
    $('ui-prompt-ok').textContent = opts.okText || 'OK';
    const fieldsEl = $('ui-prompt-fields');
    fieldsEl.innerHTML = '';
    /** @type {Record<string, HTMLInputElement|HTMLTextAreaElement|HTMLSelectElement>} */
    const inputs = {};
    (opts.fields || []).forEach(f => {
      const wrap = document.createElement('div');
      const label = document.createElement('label');
      label.className = 'block text-sm mb-1';
      label.textContent = f.label || f.name;
      wrap.appendChild(label);
      let input;
      if (f.type === 'textarea') {
        input = document.createElement('textarea');
        input.className = 'textarea textarea-bordered w-full text-sm';
        input.rows = f.rows || 3;
      } else if (f.type === 'select') {
        input = document.createElement('select');
        input.className = 'select select-bordered select-sm w-full';
        (f.options || []).forEach(o => {
          const opt = document.createElement('option');
          opt.value = o.value;
          opt.textContent = o.label;
          input.appendChild(opt);
        });
      } else {
        input = document.createElement('input');
        input.type = f.type || 'text';
        input.className = 'input input-bordered input-sm w-full';
      }
      // placeholder / readOnly は input・textarea のみ持つ（select には従来どおり無効果）
      if (f.placeholder) /** @type {HTMLInputElement|HTMLTextAreaElement} */ (input).placeholder = f.placeholder;
      if (f.value != null) input.value = f.value;
      if (f.readonly) /** @type {HTMLInputElement|HTMLTextAreaElement} */ (input).readOnly = true;
      inputs[f.name] = input;
      wrap.appendChild(input);
      if (f.hint) {
        const h = document.createElement('p');
        h.className = 'text-xs opacity-60 mt-1';
        h.textContent = f.hint;
        wrap.appendChild(h);
      }
      fieldsEl.appendChild(wrap);
    });
    const form = $('ui-prompt-form');
    let done = false;
    const finish = v => { if (done) return; done = true; dlg.close(); resolve(v); };
    form.onsubmit = e => {
      e.preventDefault();
      const out = {};
      for (const k in inputs) out[k] = inputs[k].value;
      finish(out);
    };
    $('ui-prompt-cancel').onclick = () => finish(null);
    dlg.onclose = () => finish(null);
    dlg.showModal();
    const first = /** @type {HTMLInputElement|HTMLTextAreaElement|null} */
      (fieldsEl.querySelector('input,textarea,select'));
    if (first) setTimeout(() => { first.focus(); if (first.readOnly) first.select(); }, 50);
  });
}

// ---- 共有URL 表示＋コピー (旧 prompt によるURL表示の置換) ----
/**
 * @param {string} url
 * @param {string} [title]
 */
async function uiCopyUrl(url, title) {
  const r = await uiPrompt({
    title: title || '共有URL',
    okText: 'コピー',
    fields: [{ name: 'url', label: '以下のURLをコピーしてください', value: url, readonly: true }],
  });
  if (r) {
    try {
      await navigator.clipboard.writeText(url);
      uiToast('URLをコピーしました', 'success');
    } catch (e) {
      uiToast('コピーに失敗しました。手動で選択してください。', 'warning');
    }
  }
}

// ===========================================================================
// 共有ビューヘルパ（ホスト基盤）。プラグインはこれらをページ実装ではなく
// base.html の共有関数として参照する（ページへの依存を断つ）。ViewRuntime.host
// にも束ねて、将来 ES モジュール化したときの注入点を明示しておく。
// ===========================================================================
/**
 * @param {number|string|null|undefined} n バイト数（数値化できない値は 0 扱い）
 * @returns {string} "1.5 MB" のような人間可読表記
 */
function fmtSize(n) {
  n = Number(n) || 0;
  if (n < 1024) return n + ' B';
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
  if (n < 1024 * 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + ' MB';
  return (n / 1024 / 1024 / 1024).toFixed(1) + ' GB';
}
/**
 * @param {Uint8Array} a
 * @param {Uint8Array} b
 * @returns {boolean}
 */
function bytesEqual(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}
// コミット単位の mime は保持していないため、実バイトから種別を判定する。
/**
 * @param {Uint8Array} b
 * @returns {string|null} 判定できた画像 MIME。画像でなければ null
 */
function sniffImageMime(b) {
  if (b.length >= 4 && b[0]===0x89 && b[1]===0x50 && b[2]===0x4E && b[3]===0x47) return 'image/png';
  if (b.length >= 3 && b[0]===0xFF && b[1]===0xD8 && b[2]===0xFF) return 'image/jpeg';
  if (b.length >= 4 && b[0]===0x47 && b[1]===0x49 && b[2]===0x46 && b[3]===0x38) return 'image/gif';
  if (b.length >= 12 && b[0]===0x52 && b[1]===0x49 && b[2]===0x46 && b[3]===0x46 &&
      b[8]===0x57 && b[9]===0x45 && b[10]===0x42 && b[11]===0x50) return 'image/webp';
  if (b.length >= 2 && b[0]===0x42 && b[1]===0x4D) return 'image/bmp';
  if (b.length >= 4 && b[0]===0x00 && b[1]===0x00 && b[2]===0x01 && b[3]===0x00) return 'image/x-icon';
  const head = new TextDecoder('utf-8', { fatal: false }).decode(b.subarray(0, 512)).toLowerCase();
  if (head.includes('<svg')) return 'image/svg+xml';
  return null;
}
// 先頭にヌルバイトを含むものはバイナリとみなす（UTF-8/ASCII テキストは含まない）。
/**
 * @param {Uint8Array} b
 * @returns {boolean}
 */
function bytesLookBinary(b) {
  const lim = Math.min(b.length, 8192);
  for (let i = 0; i < lim; i++) if (b[i] === 0) return true;
  return false;
}
// デコード済みテキストが実質バイナリ（制御文字比率が高い）かを判定する。
// bytesLookBinary（ヌルバイトのみ）より強い判定で、ヌルバイトを含まない非UTF-8の
// 破損データも弾く。text-diff.js / table-csv.js のバイナリ拒否ガードで共有する。
/**
 * @param {string} s
 * @returns {boolean}
 */
function looksBinaryText(s) {
  if (s.indexOf('\u0000') !== -1) return true;
  let ctrl = 0;
  const lim = Math.min(s.length, 4096);
  for (let i = 0; i < lim; i++) {
    const c = s.charCodeAt(i);
    if (c < 9 || (c > 13 && c < 32)) ctrl++;
  }
  return ctrl > lim * 0.1;
}
// ガード付き LCS（キー配列の対応付け）。中間領域の DP 行列が大きすぎる場合は
// ブロック置換へ降格し、メモリ/CPU 爆発を防ぐ。
/**
 * @param {string[]} a
 * @param {string[]} b
 * @returns {{t: '='|'-'|'+', a?: number, b?: number}[]} 編集操作列（index は入力配列上）
 */
function lcsDiffKeyed(a, b) {
  const n = a.length, m = b.length;
  const dp = [];
  for (let i = 0; i <= n; i++) dp.push(new Int32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    const row = dp[i], next = dp[i + 1];
    for (let j = m - 1; j >= 0; j--) {
      row[j] = a[i] === b[j] ? next[j + 1] + 1 : (next[j] >= row[j + 1] ? next[j] : row[j + 1]);
    }
  }
  /** @type {{t: '='|'-'|'+', a?: number, b?: number}[]} */
  const ops = [];
  let i = 0, j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) ops.push({ t: '=', a: i++, b: j++ });
    else if (dp[i + 1][j] >= dp[i][j + 1]) ops.push({ t: '-', a: i++ });
    else ops.push({ t: '+', b: j++ });
  }
  while (i < n) ops.push({ t: '-', a: i++ });
  while (j < m) ops.push({ t: '+', b: j++ });
  return ops;
}
// oldKeys/newKeys（比較用キー文字列の配列）から差分セグメント列を作る。
// 共通プレフィックス/サフィックスを O(N) で除き、変化した中間だけ LCS で対応付ける。
// 中間が大きすぎる（maxProduct 超）ならブロック置換に降格し、変更行数も
// maxChangeRows で切り詰める（DOM・メモリ保護）。text-diff.js / table-csv.js で共有。
// セグメントは「行そのもの」を持たず oldKeys/newKeys 上のインデックスだけを持つ。
// 呼び出し側が索引を実データ（行テキストや行配列）に写像して描画する。
/**
 * @typedef {{type: 'equal', ao: number, bo: number, count: number} |
 *           {type: 'change', dels: number[], adds: number[],
 *            moreDels?: number, moreAdds?: number}} DiffSegment
 */
/**
 * @param {string[]} oldKeys 旧側の比較用キー列
 * @param {string[]} newKeys 新側の比較用キー列
 * @param {{maxProduct?: number, maxChangeRows?: number}} [opts]
 * @returns {{segs: DiffSegment[], added: number, removed: number, coarse: boolean}}
 */
function diffKeyed(oldKeys, newKeys, opts) {
  const maxProduct = (opts && opts.maxProduct) || 4_000_000;
  const maxChangeRows = (opts && opts.maxChangeRows) || 1000;
  const n = oldKeys.length, m = newKeys.length;
  const minLen = Math.min(n, m);
  let p = 0;
  while (p < minLen && oldKeys[p] === newKeys[p]) p++;
  let s = 0;
  while (s < minLen - p && oldKeys[n - 1 - s] === newKeys[m - 1 - s]) s++;

  /** @type {DiffSegment[]} */
  const segs = [];
  let added = 0, removed = 0, coarse = false;
  if (p > 0) segs.push({ type: 'equal', ao: 0, bo: 0, count: p });
  const oMid = n - s - p, nMid = m - s - p;
  if (oMid > 0 || nMid > 0) {
    if (oMid * nMid > maxProduct) {
      coarse = true;
      const capD = Math.min(oMid, maxChangeRows), capA = Math.min(nMid, maxChangeRows);
      const dels = [], adds = [];
      for (let i = 0; i < capD; i++) dels.push(p + i);
      for (let j = 0; j < capA; j++) adds.push(p + j);
      segs.push({ type: 'change', dels, adds, moreDels: oMid - capD, moreAdds: nMid - capA });
      removed += oMid; added += nMid;
    } else {
      const ops = lcsDiffKeyed(oldKeys.slice(p, n - s), newKeys.slice(p, m - s));
      let i = 0;
      while (i < ops.length) {
        if (ops[i].t === '=') {
          const ao = ops[i].a + p, bo = ops[i].b + p;
          let count = 0;
          while (i < ops.length && ops[i].t === '=') { count++; i++; }
          segs.push({ type: 'equal', ao, bo, count });
        } else {
          const dels = [], adds = [];
          while (i < ops.length && ops[i].t !== '=') {
            if (ops[i].t === '-') { dels.push(ops[i].a + p); removed++; }
            else { adds.push(ops[i].b + p); added++; }
            i++;
          }
          segs.push({ type: 'change', dels, adds });
        }
      }
    }
  }
  if (s > 0) segs.push({ type: 'equal', ao: n - s, bo: m - s, count: s });
  return { segs, added, removed, coarse };
}
// 画像情報（object URL・寸法）。Bearer 認証のため <img src> 直参照ができず Blob を使う。
/**
 * @typedef {{url: string, bytes: Uint8Array, size: number,
 *            width: number, height: number}} ImageInfo
 */
/** @type {Map<*, ImageInfo>} key(commitId) → ImageInfo */
const imgInfoCache = new Map();
/**
 * @param {string} url
 * @returns {Promise<{width: number, height: number}>} 読み込み失敗時は 0×0
 */
function loadImageMeta(url) {
  return new Promise(resolve => {
    const im = new Image();
    im.onload = () => resolve({ width: im.naturalWidth, height: im.naturalHeight });
    im.onerror = () => resolve({ width: 0, height: 0 });
    im.src = url;
  });
}
/**
 * 画像バイト列の object URL・寸法をキャッシュ付きで取得する。
 * @param {*} key キャッシュキー（commitId 等。null ならキャッシュしない）
 * @param {Uint8Array} bytes
 * @param {string|null|undefined} mime
 * @returns {Promise<ImageInfo>}
 */
async function imageInfo(key, bytes, mime) {
  if (key != null && imgInfoCache.has(key)) return imgInfoCache.get(key);
  const url = URL.createObjectURL(
    new Blob([/** @type {BlobPart} */ (bytes)], { type: mime || 'application/octet-stream' }));
  const meta = await loadImageMeta(url);
  const info = { url, bytes, size: bytes.length, width: meta.width, height: meta.height };
  if (key != null) imgInfoCache.set(key, info);
  return info;
}

// ---------------------------------------------------------------------------
// 単一表示（file_detail / file_commit）用の種別判定。両ページで完全に同一の実装を
// 持っていた（判定ルールの変更に2箇所同時修正が必要だった）ため、ここに一本化する。
// ---------------------------------------------------------------------------
// 拡張子 → MIME（mime 未設定ファイルのフォールバック）
const EXT_MIME = {
  png:'image/png', jpg:'image/jpeg', jpeg:'image/jpeg', gif:'image/gif',
  webp:'image/webp', svg:'image/svg+xml', bmp:'image/bmp', ico:'image/x-icon', avif:'image/avif',
  mp4:'video/mp4', webm:'video/webm', ogv:'video/ogg', mov:'video/quicktime', m4v:'video/x-m4v', mkv:'video/x-matroska',
  mp3:'audio/mpeg', wav:'audio/wav', ogg:'audio/ogg', oga:'audio/ogg', flac:'audio/flac', m4a:'audio/mp4', aac:'audio/aac',
  pdf:'application/pdf',
};
// 拡張子だけでテキストと判断できるもの（mime 未設定でもプレビューする）
const TEXT_EXT = new Set([
  'txt','md','markdown','json','xml','html','htm','css','js','mjs','ts','tsx','jsx',
  'csv','tsv','yaml','yml','toml','ini','cfg','conf','log','sh','bash','zsh','py',
  'rs','go','c','h','cpp','hpp','cc','java','kt','rb','php','sql','svg',
]);
/**
 * @param {string|null|undefined} name
 * @returns {string} 小文字の拡張子（無ければファイル名全体の小文字）
 */
function extOf(name) { return (name || '').split('.').pop().toLowerCase(); }
// content の表示種別を判定（mime 優先、なければ拡張子）
/**
 * @param {string|null|undefined} mime
 * @param {string|null|undefined} name
 * @returns {'image'|'video'|'audio'|'pdf'|'text'|'unknown'}
 */
function mediaKind(mime, name) {
  const m = (mime || '').toLowerCase();
  const ext = extOf(name);
  if (m.startsWith('image/') || /^(png|jpe?g|gif|webp|svg|bmp|ico|avif)$/.test(ext)) return 'image';
  if (m.startsWith('video/') || /^(mp4|webm|ogv|mov|m4v|mkv)$/.test(ext)) return 'video';
  if (m.startsWith('audio/') || /^(mp3|wav|ogg|oga|flac|m4a|aac)$/.test(ext)) return 'audio';
  if (m === 'application/pdf' || ext === 'pdf') return 'pdf';
  if (m.startsWith('text/') || /(json|xml|javascript|csv|yaml|x-sh)/.test(m) || TEXT_EXT.has(ext)) return 'text';
  return 'unknown';
}
// mediaKind を ViewRuntime のビュー種別（ViewKind）名へ写像する。単一表示は巨大
// ファイルでのバイト取得を避けるため mime/拡張子だけで判定する（compare の
// resolveModel とは異なり、内容スニッフィングは行わない）。
/**
 * @param {string|null|undefined} mime
 * @param {string|null|undefined} name
 * @returns {string} ViewRuntime のビュー種別名（'core/image' 等）
 */
function viewerKind(mime, name) {
  switch (mediaKind(mime, name)) {
    case 'image': return 'core/image';
    case 'video': return 'media/video';
    case 'audio': return 'media/audio';
    case 'pdf':   return 'doc/pdf';
    default:      return 'core/text'; // text / unknown（バイナリ判定は描画側）
  }
}

// ===========================================================================
// ビュー／変換プラグイン基盤の共有ランタイム（docs/plugin-view-system.md）。
//   生バイト ──(変換プラグイン)──▶ ViewModel ──(ビュープラグイン)──▶ 表示/差分
// ビュー（表示=mount / 差分=diff）と変換（形式→ViewModel）の登録・解決だけを担う
// 純粋なレジストリ。DOM には触れない。各ページが必要なプラグインを登録して使う
// （比較ページは diff、ファイル詳細は mount）。プラグイン未使用のページでは何も
// しない（不活性）。新形式は変換＋ビューの登録だけで増やせる。
/**
 * 表示・差分の入力となる中間表現（docs/plugin-view-system.md）。
 * @typedef {Object} ViewModel
 * @property {string} kind ビュー種別（'core/text' / 'core/image' 等）
 * @property {*} payload 生バイトや変換済みデータ（種別ごとに異なる）
 * @property {string} contentType
 * @property {Object} meta
 * @property {*} [id] 由来コミット ID 等（キャッシュキーに使う）
 */
/**
 * ビュープラグイン。mount（単一表示）と diff（差分表示）は任意実装。
 * @typedef {Object} ViewPlugin
 * @property {string} kind
 * @property {string} [label] 種別の表示名（メタ比較などで使う）
 * @property {(container: HTMLElement, ctx: *) => (void|Promise<void>)} [mount]
 * @property {{modes: {id: string, label: string}[],
 *             render: (container: HTMLElement, oldModel: ViewModel, newModel: ViewModel,
 *                      opts: {mode?: string, ctx?: *, stats: HTMLElement}) =>
 *                     (void|{message?: string}|Promise<void|{message?: string}>)}} [diff]
 */
/**
 * フロント変換プラグイン（生バイト → ViewModel。detect 先勝ち）。
 * @typedef {Object} ViewConverter
 * @property {string} converterId
 * @property {string} targetView
 * @property {(bytes: Uint8Array, hint: *) => boolean} detect
 * @property {(bytes: Uint8Array, hint: *) => (ViewModel|Promise<ViewModel>)} convert
 */
const ViewRuntime = (() => {
  /** @type {Map<string, ViewPlugin>} */
  const views = new Map();   // kind → ViewPlugin
  /** @type {ViewConverter[]} */
  const converters = [];     // フロント変換（順に detect、先勝ち）
  /** @param {ViewPlugin} p */
  function registerView(p) { views.set(p.kind, p); }
  /** @param {ViewConverter} c */
  function registerConverter(c) { converters.push(c); }
  /** @param {string} kind */
  function hasView(kind) { return views.has(kind); }
  /**
   * @param {string} kind
   * @returns {ViewPlugin} 未登録種別は core/binary へフォールバックする
   */
  function getView(kind) { return views.get(kind) || views.get('core/binary'); }
  // 生バイト → ViewModel。フロント変換を先勝ちで適用し、無ければ core/binary へ。
  // 重い形式は将来 GET …/commits/:cid/view（X-View-Kind）でバックエンド変換へ
  // 委譲する。その差し替え口がここ。
  /**
   * @param {Uint8Array} bytes
   * @param {{id?: *, mime?: string|null, charset?: string|null, name?: string|null}} [hint]
   * @returns {Promise<ViewModel>}
   */
  async function resolveModel(bytes, hint) {
    hint = hint || {};
    for (const c of converters) {
      if (c.detect(bytes, hint)) {
        const m = await c.convert(bytes, hint);
        m.id = hint.id;
        return m;
      }
    }
    return { kind: 'core/binary', payload: bytes,
             contentType: (hint.mime || 'application/octet-stream'), meta: {}, id: hint.id };
  }
  return { registerView, registerConverter, hasView, getView, resolveModel, views };
})();
// プラグインが参照する共有ヘルパ（注入点）。外部ファイルのプラグインは
// ページ実装ではなくこのホスト基盤に依存する。
// （tsc は同一ファイル内の `ViewRuntime.host = ...` 代入を expando プロパティとして
//   型に取り込むため、プラグイン側から ViewRuntime.host が型解決できる。）
ViewRuntime.host = {
  escapeHtml, decodeBytes, $, fmtSize, bytesEqual,
  sniffImageMime, bytesLookBinary, looksBinaryText, diffKeyed, imageInfo, loadImageMeta,
  mediaKind, viewerKind,
};
// 外部プラグイン（/ui/plugins/*.js）からも参照できるよう公開する。
/** @type {*} */ (window).ViewRuntime = ViewRuntime;
