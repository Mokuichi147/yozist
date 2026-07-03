// ゴミ箱ページ（/ui/trash）のロジック。trash.html のインライン <script> から切り出した静的ファイル（issue #50）。
// /ui/pages/trash.js で配信される。
const PAGE = 100;

let items = [];       // 表示中の削除済みファイル
let hasMore = false;  // まだ続きがあるか (X-Has-More)
let offset = 0;       // 次ページ取得用の DB オフセット

async function init() {
  const me = await requireAuth();
  if (!me) return;
  $('main').classList.remove('hidden');
  await fetchPage();
}

async function fetchPage() {
  let resp;
  try {
    resp = await api(`/api/trash?limit=${PAGE}&offset=${offset}`);
    if (!resp.ok) throw new Error(await resp.text());
  } catch (e) {
    $('trash-list').innerHTML = '<li class="px-2 py-2 text-error text-sm">取得失敗</li>';
    return;
  }
  hasMore = resp.headers.get('x-has-more') === '1';
  const page = await resp.json();
  // 権限フィルタでページが縮むため、次オフセットはサーバが返す DB 上の位置を使う
  const next = parseInt(resp.headers.get('x-next-offset') || '', 10);
  offset = Number.isNaN(next) ? offset + PAGE : next;
  items = items.concat(page);
  render();
}

async function loadMore() {
  const btn = $('load-more');
  btn.disabled = true;
  btn.textContent = '読み込み中…';
  try { await fetchPage(); }
  finally { btn.disabled = false; btn.textContent = 'さらに読み込む'; }
}

// files.html と同じアイコン/サイズ表記を踏襲する
function fmtSize(n) {
  if (n < 1024) return n + ' B';
  const units = ['KB', 'MB', 'GB', 'TB'];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < units.length - 1);
  return n.toFixed(n >= 100 ? 0 : 1) + ' ' + units[i];
}

function fileIcon(f) {
  const m = (f.mime || '').toLowerCase();
  const n = f.display_name.toLowerCase();
  if (m.startsWith('image/')) return '🖼️';
  if (m.startsWith('video/')) return '🎬';
  if (m.startsWith('audio/')) return '🎵';
  if (m.includes('pdf')) return '📕';
  if (m.includes('zip') || m.includes('compressed') || /\.(zip|gz|7z|rar|tar|xz|zst)$/.test(n)) return '🗜️';
  if (/\.(md|markdown|txt)$/.test(n)) return '📝';
  if (/\.(js|ts|jsx|tsx|py|rs|go|java|kt|c|cpp|h|hpp|sh|rb|php|html|css|json|yaml|yml|toml|sql|xml)$/.test(n)) return '💻';
  if (/\.(csv|tsv|xlsx|xls)$/.test(n)) return '📊';
  if (m.startsWith('text/')) return '📄';
  if (m && m !== 'application/octet-stream') return '📦';
  return '📄';
}

// 削除を実行したユーザー（更新者ラベル）。未記録は空。
function actorLabel(f) {
  const who = f.updated_by || f.created_by;
  return who ? ` · ${escapeHtml(who)}` : '';
}

function render() {
  $('trash-count').textContent = `(${items.length}${hasMore ? '+' : ''})`;
  const el = $('trash-list');
  if (items.length === 0) {
    el.innerHTML = '<li class="px-2 py-8 opacity-60 text-sm text-center">ゴミ箱は空です。</li>';
  } else {
    el.innerHTML = items.map(f => `
      <li class="flex items-center gap-3 px-2 py-2 rounded hover:bg-base-200" data-id="${f.id}">
        <span class="text-lg shrink-0" aria-hidden="true">${fileIcon(f)}</span>
        <span class="min-w-0 flex-1">
          <a href="/ui/files/${f.id}" class="font-semibold truncate block link link-hover"
             title="詳細を表示">${escapeHtml(f.display_name)}</a>
          <span class="text-xs opacity-60 block">
            削除: ${fmtTs(f.deleted_at || f.updated_at)}${actorLabel(f)} · ${fmtSize(f.size)}
          </span>
        </span>
        <span class="flex gap-6 shrink-0">
          <button class="btn btn-xs btn-primary btn-outline"
                  onclick="restoreFile('${f.id}')">復元</button>
          <button class="btn btn-xs btn-error btn-outline"
                  onclick="purgeFile('${f.id}')">完全に削除</button>
        </span>
      </li>
    `).join('');
  }
  $('load-more-wrap').classList.toggle('hidden', !hasMore);
  $('empty-trash-btn').disabled = items.length === 0;
}

async function restoreFile(id) {
  const f = items.find(x => x.id === id);
  const name = f ? f.display_name : 'ファイル';
  const r = await api(`/api/files/${id}/restore`, { method: 'POST' });
  if (!r.ok) { uiToast('復元に失敗しました: ' + await r.text(), 'error'); return; }
  items = items.filter(x => x.id !== id);
  render();
  uiToast(`"${name}" を復元しました`, 'success');
}

async function purgeFile(id) {
  const f = items.find(x => x.id === id);
  const name = f ? f.display_name : 'ファイル';
  if (!await uiConfirm(`"${name}" を完全に削除しますか？\nこの操作は元に戻せません。`,
                       { danger: true, okText: '完全に削除' })) return;
  const r = await api(`/api/trash/${id}`, { method: 'DELETE' });
  if (!r.ok) { uiToast('削除に失敗しました: ' + await r.text(), 'error'); return; }
  items = items.filter(x => x.id !== id);
  render();
  uiToast(`"${name}" を完全に削除しました`, 'success');
}

async function emptyTrash() {
  if (items.length === 0) return;
  if (!await uiConfirm('ゴミ箱内のファイルをすべて完全に削除しますか？\nこの操作は元に戻せません。',
                       { danger: true, okText: 'すべて完全に削除' })) return;
  let resp;
  try { resp = await json('/api/trash', { method: 'DELETE' }); }
  catch (e) { uiToast('削除に失敗しました: ' + e.message, 'error'); return; }
  // 物理削除後は一覧を取り直す（権限の無いファイルが残る可能性があるため）。
  items = [];
  offset = 0;
  await fetchPage();
  uiToast(`${resp && resp.purged != null ? resp.purged : 0} 件を完全に削除しました`, 'success');
}

init();
