// 比較ページ（/ui/files/:id/compare）のロジック。compare.html のインライン <script> から
// 切り出した静的ファイル（issue #50）。/ui/pages/compare.js で配信される。
// ===========================================================================
// 比較ページ。ビュー／変換プラグイン基盤（docs/plugin-view-system.md）で駆動する。
//   生バイト ──(変換プラグイン)──▶ ViewModel ──(ビュープラグイン)──▶ 差分描画
// 種別判定・差分・画像描画はすべてプラグインに閉じ込め、ページ側は
//   「2 コミットを解決 → 種別が一致すれば該当ビューへ、不一致ならメタ比較へ」
// というオーケストレーションだけを担う。新形式は変換＋ビューの登録で増やせる。
// ===========================================================================
// IIFE で包み、他ページとのグローバル衝突を避ける（issue #53）。
(() => {

const parts = location.pathname.split('/').filter(Boolean);
// /ui/files/:id/compare → ['ui','files',':id','compare']
const fileId = parts[2];
const qs = new URLSearchParams(location.search);

let history = [];
let currentCommitId = null;
let fileCharset = null; // 元エンコーディング。テキスト payload のデコードに使う。
// 表示名。変換プラグインが拡張子で種別判定する「候補の絞り込み」に使う。
// 注意: これは現行の display_name であり、コミットごとの実ファイル名ではない。
// リネームされたファイルを比較すると、旧コミット側にも新しい拡張子のヒントが渡る
// （例: report.txt → report.csv 後に旧コミットを見ると .csv が渡る）。拡張子だけで
// 種別を確定させると、旧コミットの散文テキストが誤って表形式と解釈されうるため、
// 拡張子ベース判定を行うプラグイン（例: table-csv.js）は必ず内容も検証すること。
let fileName = null;
const bytesCache = new Map(); // commitId → Uint8Array（生バイト。変換/種別判定の共通源）

const detailUrl = '/ui/files/' + fileId;
$('back-detail').href = detailUrl;
$('back-link').href = detailUrl;

// ---------------------------------------------------------------------------
// 共有ユーティリティ
// ---------------------------------------------------------------------------

async function fetchBytes(commitId) {
  if (bytesCache.has(commitId)) return bytesCache.get(commitId);
  const r = await api(`/api/files/${fileId}/commits/${commitId}`);
  if (!r.ok) throw r;
  const bytes = new Uint8Array(await r.arrayBuffer());
  bytesCache.set(commitId, bytes);
  return bytes;
}

// ===========================================================================
// ビュー・ランタイムは base.html の共有 ViewRuntime を使う（このページは差分=diff）。
// 比較ページはここで「変換プラグイン」と「差分対応ビュープラグイン」を登録する。
// ===========================================================================

// ---- フロント変換（core 種別はすべて恒等変換）----------------------------
// 画像はマジックナンバーで厳密に判定できるため先に試し、次にテキスト。
ViewRuntime.registerConverter({
  converterId: 'core/image', targetView: 'core/image',
  detect: (bytes) => !!sniffImageMime(bytes),
  convert: (bytes) => ({ kind: 'core/image', payload: bytes,
                         contentType: sniffImageMime(bytes) || 'application/octet-stream', meta: {} }),
});
ViewRuntime.registerConverter({
  converterId: 'core/text', targetView: 'core/text',
  detect: (bytes) => !bytesLookBinary(bytes),
  convert: (bytes) => ({ kind: 'core/text', payload: bytes, contentType: 'text/plain', meta: {} }),
});

// ===========================================================================
// ページのオーケストレーション
// ===========================================================================
let curPlugin = null, curOld = null, curNew = null;
const curMode = {}; // kind → 選択中モード id（種別ごとに保持）

// プラグインの diff.modes から表示モード切替ツールバーを汎用生成する。
function buildModeToolbar(plugin) {
  const box = $('view-modes'), label = $('view-modes-label');
  const modes = (plugin.diff && plugin.diff.modes) || [];
  if (modes.length <= 1) {
    box.innerHTML = ''; box.classList.add('hidden'); label.classList.add('hidden');
    return;
  }
  box.classList.remove('hidden'); label.classList.remove('hidden');
  if (!curMode[plugin.kind]) curMode[plugin.kind] = modes[0].id;
  const sel = curMode[plugin.kind];
  box.innerHTML = modes.map(m =>
    `<button class="btn btn-xs join-item ${m.id === sel ? 'btn-active' : ''}" data-mode="${m.id}">${escapeHtml(m.label)}</button>`
  ).join('');
  box.querySelectorAll('button').forEach(b => b.onclick = () => {
    curMode[plugin.kind] = b.dataset.mode;
    buildModeToolbar(plugin);
    renderCurrent();
  });
}

async function renderCurrent() {
  const mode = curMode[curPlugin.kind] ||
    (curPlugin.diff.modes[0] && curPlugin.diff.modes[0].id);
  const ctx = { fileId, charset: fileCharset };
  const stats = $('cmp-stats'), cont = $('cmp-diff'), msg = $('cmp-message');
  msg.classList.add('hidden');
  try {
    const ret = await curPlugin.diff.render(cont, curOld, curNew, { mode, ctx, stats });
    if (ret && ret.message) { msg.textContent = ret.message; msg.classList.remove('hidden'); }
  } catch (e) {
    stats.textContent = ''; cont.innerHTML = '';
    msg.textContent = '差分の描画に失敗しました。';
    msg.classList.remove('hidden');
  }
}

function update() {
  const baseId = $('sel-base').value;
  const compareId = $('sel-compare').value;
  const newUrl = `${location.pathname}?base=${baseId}&compare=${compareId}`;
  window.history.replaceState(null, '', newUrl);
  $('cmp-message').classList.add('hidden');
  $('cmp-stats').textContent = '';
  $('cmp-diff').innerHTML = '<span class="opacity-50 text-xs p-3 block">読み込み中…</span>';

  Promise.all([fetchBytes(baseId), fetchBytes(compareId)]).then(async ([ob, nb]) => {
    curOld = await ViewRuntime.resolveModel(ob, { id: baseId, charset: fileCharset, name: fileName });
    curNew = await ViewRuntime.resolveModel(nb, { id: compareId, charset: fileCharset, name: fileName });
    // 種別が一致し、そのビューが差分対応なら専用差分。さもなくばメタ比較へ。
    const sameKind = curOld.kind === curNew.kind;
    curPlugin = (sameKind && ViewRuntime.hasView(curOld.kind) && ViewRuntime.getView(curOld.kind).diff)
      ? ViewRuntime.getView(curOld.kind)
      : ViewRuntime.getView('core/binary');
    buildModeToolbar(curPlugin);
    await renderCurrent();
  }).catch(() => {
    $('cmp-stats').textContent = '';
    $('cmp-diff').innerHTML = '';
    $('cmp-message').textContent = 'コミット内容の取得に失敗しました。';
    $('cmp-message').classList.remove('hidden');
  });
}

function optionLabel(c) {
  const time = fmtTs(c.timestamp);
  const star = c.id === currentCommitId ? '★ ' : '';
  const msg = c.message ? ` — ${c.message}` : '';
  return `${star}${time}  ${c.format_id || c.id.slice(0, 8)}${msg}`;
}

function buildSelectors() {
  const opts = history.map(c =>
    `<option value="${c.id}">${escapeHtml(optionLabel(c))}</option>`).join('');
  $('sel-base').innerHTML = opts;
  $('sel-compare').innerHTML = opts;

  // 既定: compare = current、base = それより 1 つ古いコミット
  const sorted = history.slice().sort((a, b) =>
    fmtTs(b.timestamp).localeCompare(fmtTs(a.timestamp)));
  const curIdx = sorted.findIndex(c => c.id === currentCommitId);
  const defCompare = currentCommitId || (sorted[0] && sorted[0].id);
  const defBase = (curIdx >= 0 && sorted[curIdx + 1]) ? sorted[curIdx + 1].id
    : (sorted[1] ? sorted[1].id : defCompare);

  $('sel-base').value = qs.get('base') || defBase || '';
  $('sel-compare').value = qs.get('compare') || defCompare || '';
  // 値が history に無い場合のフォールバック
  if (!$('sel-base').value && history[0]) $('sel-base').value = history[0].id;
  if (!$('sel-compare').value && history[0]) $('sel-compare').value = history[0].id;

  $('sel-base').onchange = update;
  $('sel-compare').onchange = update;
}

async function init() {
  const me = await requireAuth();
  if (!me) return;

  let file;
  try {
    [file, history] = await Promise.all([
      json('/api/files/' + fileId),
      json(`/api/files/${fileId}/history`),
    ]);
  } catch (e) {
    $('not-found-msg').textContent = 'ファイルが見つかりません、または閲覧権限がありません。';
    $('not-found').classList.remove('hidden');
    return;
  }
  if (!history || history.length === 0) {
    $('not-found-msg').textContent = 'このファイルには履歴がありません。';
    $('not-found').classList.remove('hidden');
    return;
  }
  currentCommitId = file.current_commit;
  fileCharset = file.charset;
  fileName = file.display_name;
  document.title = `yozist - ${file.display_name} の比較`;
  $('main').classList.remove('hidden');
  $('cmp-name').textContent = file.display_name;

  buildSelectors();
  update();
}

init();
})();
