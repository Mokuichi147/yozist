// @ts-check
// ファイル一覧（/ui/files）とメディア一覧（/ui/media）が共有する複数選択まわりの
// 部品。選択 UI そのものは一覧の見せ方が違うので各ページが持ち、ここには
//   - 選択済みファイルに対する一括操作（確認ダイアログ・API 呼び出し・成否の集計）
//   - 選択モードへ入る「長押し」の判定
// という、2 ページで完全に同じものだけを置く。
// /ui/pages/bulk_actions.js で配信され、window.BulkActions として公開する。
(() => {

// 1 件ずつ直列に流すと N 件で N 往復ぶん待たされるので少しだけ並列化する。
// 大きくしすぎるとサーバ側の書き込みが詰まるため控えめな値にする。
const CONCURRENCY = 4;
// これ以上の件数を個別ダウンロードするとブラウザ側で抑制されやすいので確認を挟む。
const DOWNLOAD_CONFIRM_OVER = 10;
// 確認ダイアログに実名を並べる最大件数（それ以上は「ほか N 件」に畳む）。
const CONFIRM_NAMES = 5;

/** @typedef {{ id: string, display_name: string, mime?: string|null }} BulkFile */
/** @typedef {{ ok: number, failed: {file: BulkFile, message: string}[] }} BulkResult */

/**
 * files の各要素へ fn を適用し、成功数と失敗内訳を返す。
 * 1 件の失敗で全体を止めない（権限の無いファイルが混ざっていても残りは処理する）。
 * @param {BulkFile[]} files
 * @param {(f: BulkFile) => Promise<*>} fn
 * @param {number} [concurrency] 直列に流したい操作（順序が結果に影響するもの）は 1
 * @returns {Promise<BulkResult>}
 */
async function runAll(files, fn, concurrency = CONCURRENCY) {
  /** @type {{file: BulkFile, message: string}[]} */
  const failed = [];
  let ok = 0;
  let next = 0;
  const worker = async () => {
    while (next < files.length) {
      const f = files[next++];
      try { await fn(f); ok++; }
      catch (e) { failed.push({ file: f, message: (e && e.message) || String(e) }); }
    }
  };
  const workers = Math.max(1, Math.min(concurrency, files.length));
  await Promise.all(Array.from({ length: workers }, worker));
  return { ok, failed };
}

/**
 * 一括処理の結果をトーストで伝える。全件成功なら件数だけ、失敗があれば
 * 内訳と最初の失敗理由を出す（全件分は長くなりすぎるので出さない）。
 * @param {string} action 「削除」「タグ追加」などの動詞
 * @param {BulkResult} res
 */
function reportResult(action, res) {
  if (res.failed.length === 0) {
    uiToast(`${res.ok} 件を${action}しました`, 'success');
    return;
  }
  const first = res.failed[0];
  uiToast(
    `${action}: 成功 ${res.ok} 件 / 失敗 ${res.failed.length} 件` +
    `（"${first.file.display_name}": ${first.message}）`,
    res.ok > 0 ? 'warning' : 'error',
  );
}

/**
 * 確認ダイアログ用に対象ファイル名を数件だけ並べる。
 * @param {BulkFile[]} files
 * @returns {string}
 */
function nameList(files) {
  const head = files.slice(0, CONFIRM_NAMES).map(f => '・' + f.display_name).join('\n');
  return files.length > CONFIRM_NAMES
    ? `${head}\n… ほか ${files.length - CONFIRM_NAMES} 件`
    : head;
}

/**
 * 選択ファイルのタグをまとめて取得する（/api/files/tags は 1000 件上限）。
 * @param {string[]} ids
 * @returns {Promise<Record<string, {id: string, name: string, kind: string}[]>>}
 */
async function fetchTagsOf(ids) {
  /** @type {Record<string, *>} */
  const map = {};
  for (let i = 0; i < ids.length; i += 200) {
    const chunk = ids.slice(i, i + 200);
    Object.assign(map, await json('/api/files/tags?ids=' + encodeURIComponent(chunk.join(','))));
  }
  return map;
}

// ---- 各操作 ----------------------------------------------------------------
// いずれも「一覧の再読み込みが必要か」を返す（キャンセル・全件失敗なら false）。

/**
 * 選択ファイルへ同じタグを付ける。既存タグ名なら再利用、無ければ新規作成する
 * （/api/tags は名前で upsert する）。
 * @param {BulkFile[]} files
 * @returns {Promise<boolean>}
 */
async function addTag(files) {
  if (files.length === 0) return false;
  // 候補は手動タグのみ。システムタグは自動付与、AI タグは再生成でしか変えられない。
  let candidates = [];
  try {
    candidates = (await json('/api/tags?sort=usage')).filter(
      (/** @type {*} */ t) => t.kind !== 'system' && t.kind !== 'ai');
  } catch (e) { candidates = []; }

  const r = await uiPrompt({
    title: `タグを追加（${files.length} 件）`,
    okText: '追加',
    fields: [{
      name: 'name',
      label: 'タグ名',
      placeholder: '例: 旅行',
      options: candidates.map((/** @type {*} */ t) => ({ value: t.name, label: t.name })),
      hint: '既存のタグ名を入力するとそのタグを割り当て、新しい名前なら作成します。',
    }],
  });
  if (!r) return false;
  const name = r.name.trim();
  if (!name) return false;

  let tagId;
  try {
    tagId = (await json('/api/tags', { method: 'POST', body: { name } })).id;
  } catch (e) {
    uiToast('タグの作成に失敗しました: ' + e.message, 'error');
    return false;
  }
  const res = await runAll(files, f =>
    json(`/api/files/${f.id}/tags`, { method: 'POST', body: { tag_id: tagId } }));
  reportResult(`「${name}」を付与`, res);
  return res.ok > 0;
}

/**
 * 選択ファイルから同じタグを外す。候補は選択内に実際に付いているタグだけを、
 * 付いている件数の多い順で出す。
 * @param {BulkFile[]} files
 * @returns {Promise<boolean>}
 */
async function removeTag(files) {
  if (files.length === 0) return false;
  let map;
  try {
    map = await fetchTagsOf(files.map(f => f.id));
  } catch (e) {
    uiToast('タグの取得に失敗しました: ' + e.message, 'error');
    return false;
  }
  // タグ ID → { タグ, 付いているファイル数 }
  /** @type {Map<string, {tag: *, count: number}>} */
  const byId = new Map();
  for (const f of files) {
    for (const t of map[f.id] || []) {
      // システムタグ（拡張子・種別）は自動付与、AI タグは再生成でしか変えられない
      // ので、外せるタグとして出さない（サーバ側でも AI タグの解除は拒否される）。
      if (t.kind === 'system' || t.kind === 'ai') continue;
      const e = byId.get(t.id) || { tag: t, count: 0 };
      e.count++;
      byId.set(t.id, e);
    }
  }
  if (byId.size === 0) {
    uiToast('選択したファイルに外せるタグがありません', 'warning');
    return false;
  }
  const options = [...byId.values()]
    .sort((a, b) => (b.count - a.count) || a.tag.name.localeCompare(b.tag.name, 'ja'))
    .map(e => ({ value: e.tag.id, label: `${e.tag.name}（${e.count} 件）` }));

  const r = await uiPrompt({
    title: `タグを外す（${files.length} 件）`,
    okText: '外す',
    fields: [{
      name: 'tag_id', label: '外すタグ', type: 'select', options,
      hint: 'そのタグが付いているファイルだけが対象になります。',
    }],
  });
  if (!r || !r.tag_id) return false;

  const entry = byId.get(r.tag_id);
  const targets = files.filter(f => (map[f.id] || []).some((/** @type {*} */ t) => t.id === r.tag_id));
  const res = await runAll(targets, f =>
    json(`/api/files/${f.id}/tags/${r.tag_id}`, { method: 'DELETE' }));
  reportResult(`「${entry.tag.name}」を解除`, res);
  return res.ok > 0;
}

/**
 * 選択ファイルをシリーズへまとめて追加する。既存名なら再利用、無ければ作成する
 * （ファイル詳細の「シリーズに追加」と同じ規約）。
 * @param {BulkFile[]} files
 * @returns {Promise<boolean>}
 */
async function addToSeries(files) {
  if (files.length === 0) return false;
  let list = [];
  try { list = await json('/api/series'); } catch (e) { list = []; }

  const r = await uiPrompt({
    title: `シリーズに追加（${files.length} 件）`,
    okText: '追加',
    fields: [{
      name: 'name',
      label: 'シリーズ名',
      placeholder: '例: 2026 沖縄',
      options: list.map((/** @type {*} */ s) => ({ value: s.name, label: s.name })),
      hint: '存在しない名前を入力すると新しいシリーズを作成します。',
    }],
  });
  if (!r) return false;
  const name = r.name.trim();
  if (!name) return false;

  let seriesId = (list.find((/** @type {*} */ s) => s.name === name) || {}).id;
  try {
    if (!seriesId) seriesId = (await json('/api/series', { method: 'POST', body: { name } })).id;
  } catch (e) {
    uiToast('シリーズの作成に失敗しました: ' + e.message, 'error');
    return false;
  }
  // 並列に投げると order_index の採番（既存メンバーの末尾 +1）が衝突して同じ値に
  // なり、シリーズ内の順序が選択順どおりにならない。ここだけは直列に流す。
  const res = await runAll(files, f =>
    json(`/api/series/${seriesId}/members`, { method: 'POST', body: { file_id: f.id } }), 1);
  reportResult(`「${name}」へ追加`, res);
  return res.ok > 0;
}

/**
 * 選択ファイルをまとめて削除する（ゴミ箱へ入る論理削除）。
 * @param {BulkFile[]} files
 * @returns {Promise<boolean>}
 */
async function remove(files) {
  if (files.length === 0) return false;
  const okToGo = await uiConfirm(
    `${files.length} 件を削除しますか？\n（ゴミ箱から復元できます）\n\n${nameList(files)}`,
    { title: 'ファイルの削除', danger: true, okText: '削除' });
  if (!okToGo) return false;
  const res = await runAll(files, f => json(`/api/files/${f.id}`, { method: 'DELETE' }));
  reportResult('削除', res);
  return res.ok > 0;
}

/**
 * 選択ファイルを 1 件ずつダウンロードする。ZIP 化のエンドポイントは無いので
 * 個別保存になる（同時に流すとブラウザに抑制されるため直列）。
 * @param {BulkFile[]} files
 * @returns {Promise<boolean>} 一覧は変わらないので常に false
 */
async function download(files) {
  if (files.length === 0) return false;
  if (files.length > DOWNLOAD_CONFIRM_OVER) {
    const okToGo = await uiConfirm(
      `${files.length} 件を個別にダウンロードします。よろしいですか？\n` +
      '（ブラウザが複数ファイルのダウンロード許可を求めることがあります）',
      { title: 'ダウンロード', okText: 'ダウンロード' });
    if (!okToGo) return false;
  }
  /** @type {{file: BulkFile, message: string}[]} */
  const failed = [];
  let ok = 0;
  for (const f of files) {
    try {
      const r = await api(`/api/files/${f.id}/content`);
      if (!r.ok) throw new Error((await r.text().catch(() => '')) || r.statusText);
      const blob = new Blob([await r.arrayBuffer()],
        { type: f.mime || 'application/octet-stream' });
      const url = URL.createObjectURL(blob);
      const a = el('a', { href: url, download: f.display_name });
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      ok++;
    } catch (e) {
      failed.push({ file: f, message: (e && e.message) || String(e) });
    }
  }
  reportResult('ダウンロード', { ok, failed });
  return false;
}

// ---- 長押しで選択モードへ入る --------------------------------------------

// 長押しと判定するまでの時間。短いとスクロール開始や普通のタップを拾ってしまい、
// 長すぎると「反応しない」と感じるので、モバイル OS の既定に近い値にする。
const LONG_PRESS_MS = 450;
// この距離を超えて動いたらスクロール/ドラッグとみなして長押しを取り消す。
const LONG_PRESS_MOVE_PX = 10;

/**
 * container 内の項目の長押しを検出する。押された要素から idOf でファイル ID を
 * 取り出し、指（マウス）を動かさずに押し続けたら onFire(id) を呼ぶ。
 * 長押しが成立したときは、そのあとに続く click と contextmenu（Android の
 * 長押しメニュー）を飲み込んで、詳細ページへ遷移しないようにする。
 * @param {HTMLElement} container
 * @param {(target: HTMLElement) => string|null|undefined} idOf
 * @param {(id: string) => void} onFire
 */
function longPressSelect(container, idOf, onFire) {
  let timer = 0;
  let startX = 0;
  let startY = 0;
  let fired = false;
  const cancel = () => { if (timer) { clearTimeout(timer); timer = 0; } };

  container.addEventListener('pointerdown', e => {
    cancel();
    fired = false;                       // 前回の長押しの後始末（click が来なかった場合）
    if (e.button !== 0) return;          // 右クリック・戻るボタン等は対象外
    const id = idOf(/** @type {HTMLElement} */ (e.target));
    if (!id) return;
    startX = e.clientX;
    startY = e.clientY;
    timer = window.setTimeout(() => {
      timer = 0;
      fired = true;
      onFire(id);
    }, LONG_PRESS_MS);
  });
  container.addEventListener('pointermove', e => {
    if (!timer) return;
    if (Math.abs(e.clientX - startX) > LONG_PRESS_MOVE_PX ||
        Math.abs(e.clientY - startY) > LONG_PRESS_MOVE_PX) cancel();
  });
  for (const ev of ['pointerup', 'pointercancel', 'pointerleave']) {
    container.addEventListener(ev, cancel);
  }
  // ページ側の click ハンドラより先に止めたいので capture で拾う。
  container.addEventListener('click', e => {
    if (!fired) return;
    fired = false;
    e.preventDefault();
    e.stopPropagation();
  }, true);
  container.addEventListener('contextmenu', e => { if (fired) e.preventDefault(); });
}

/** @type {*} */ (window).BulkActions = {
  addTag, removeTag, addToSeries, remove, download, longPressSelect,
};
})();
