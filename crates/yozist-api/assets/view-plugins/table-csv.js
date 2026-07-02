// table/csv ビュープラグイン（拡張例）。
//
// CSV/TSV を「テーブル」として解釈し、行 LCS ＋ セル単位ハイライトで差分表示する。
// 差分アルゴリズムがビュー固有である（テキストの行差分ではなく、表の行・セル差分）
// ことを示す実例。base.html 共有 ViewRuntime に「変換」と「ビュー」を自己登録する。
// このファイルを追加し compare.html から読み込むだけで、コアを改変せず CSV 差分が
// 増える（種別 table/csv はコアにとって単なる文字列キー）。
(() => {
  const { escapeHtml, decodeBytes, bytesLookBinary, looksBinaryText, diffKeyed } = ViewRuntime.host;

  // text-diff.js と同じ安全弁（DOM/メモリ保護）を共有する。
  const LCS_MAX_PRODUCT = 4_000_000;
  const MAX_CHANGE_ROWS = 1000;
  const CONTEXT = 3;
  const MAX_EXPAND = 5000;

  // --- 最小 CSV/TSV パーサ（RFC4180 風: 引用符・エスケープ・改行入りセル対応）---
  function parseDelimited(text, delim) {
    const rows = [];
    let row = [], cell = '', i = 0, inQ = false, sawAny = false;
    while (i < text.length) {
      const c = text[i];
      if (inQ) {
        if (c === '"') {
          if (text[i + 1] === '"') { cell += '"'; i += 2; continue; }
          inQ = false; i++; continue;
        }
        cell += c; i++; continue;
      }
      if (c === '"') { inQ = true; sawAny = true; i++; continue; }
      if (c === delim) { row.push(cell); cell = ''; sawAny = true; i++; continue; }
      if (c === '\n') { row.push(cell); rows.push(row); row = []; cell = ''; sawAny = false; i++; continue; }
      if (c === '\r') { i++; continue; }
      cell += c; sawAny = true; i++;
    }
    if (sawAny || cell !== '' || row.length) { row.push(cell); rows.push(row); }
    return rows;
  }

  const extOf = name => {
    const m = /\.([^.]+)$/.exec(name || '');
    return m ? m[1].toLowerCase() : '';
  };

  // 内容が実際に区切り文字付きの表形式かを判定する。拡張子は「候補の絞り込み」に
  // しか使わず、採否は内容で決める。
  // 理由: compare.html はコミット時点のファイル名を持たず、常に現行の display_name を
  // 変換ヒントとして両コミットへ使い回す。リネームされたファイル（例: report.txt →
  // report.csv）を比較すると、拡張子がテキストだった時代の旧コミットにも .csv の
  // ヒントが渡ってしまう。拡張子だけで判定すると、その散文テキストが誤って
  // parseDelimited に強制通過し、崩れたテーブル差分になる（内容ベースの構造チェックで
  // これを防ぐ）。
  function looksTabular(text, delim) {
    const lines = text.split('\n').filter(l => l.length > 0).slice(0, 20);
    if (lines.length < 2) return false;
    const re = delim === '\t' ? /\t/g : /,/g;
    const counts = lines.map(l => (l.match(re) || []).length);
    if (counts.every(c => c === 0)) return false; // 区切り文字が一度も出現しない
    const mode = counts[0];
    const consistent = counts.filter(c => c === mode).length;
    return consistent >= Math.ceil(lines.length * 0.7); // 列数が概ね揃っている
  }

  // --- 変換: 拡張子候補 + 内容の表形式チェックで CSV/TSV を判定し、グリッド
  // （行配列）に展開する ---
  ViewRuntime.registerConverter({
    converterId: 'table/csv',
    targetView: 'table/csv',
    // core/text より先に登録されるため先勝ちで拾う。バイナリ（ヌルバイト）だけでなく
    // 破損データ（ヌルバイトを含まない高制御文字率の非UTF-8）も looksBinaryText で
    // 拒否し、拡張子だけでなく内容の構造も見て判定する。
    detect: (bytes, hint) => {
      const e = extOf(hint && hint.name);
      if (e !== 'csv' && e !== 'tsv') return false;
      if (bytesLookBinary(bytes)) return false;
      const text = decodeBytes(bytes, hint && hint.charset);
      if (looksBinaryText(text)) return false;
      return looksTabular(text, e === 'tsv' ? '\t' : ',');
    },
    convert: (bytes, hint) => {
      const delim = extOf(hint && hint.name) === 'tsv' ? '\t' : ',';
      const text = decodeBytes(bytes, hint && hint.charset);
      return {
        kind: 'table/csv', payload: bytes, contentType: 'text/csv', meta: {},
        rows: parseDelimited(text, delim), delim,
      };
    },
  });

  const td = (html, cls) => `<td class="diff-code${cls ? ' ' + cls : ''}">${html}</td>`;
  const lnTd = n => `<td class="diff-ln">${n == null ? '' : n + 1}</td>`;

  function plainRow(row, ncols, rowCls, oldLn, newLn) {
    let cells = '';
    for (let c = 0; c < ncols; c++) cells += td(escapeHtml(row[c] != null ? row[c] : ''));
    return `<tr class="${rowCls}">${lnTd(oldLn)}${lnTd(newLn)}${cells}</tr>`;
  }
  // 変更行: 同じ列位置で値が違うセルだけをハイライトする。
  function changedRow(oldR, newR, ncols, oldLn, newLn) {
    let cells = '';
    for (let c = 0; c < ncols; c++) {
      const ov = oldR[c] != null ? oldR[c] : '', nv = newR[c] != null ? newR[c] : '';
      if (ov === nv) cells += td(escapeHtml(nv));
      else cells += td(`<span class="diff-add-inline">${escapeHtml(nv)}</span>`, '');
    }
    return `<tr>${lnTd(oldLn)}${lnTd(newLn)}${cells}</tr>`;
  }
  function gapRow(count, segIdx, ncols) {
    const span = ncols + 2;
    if (count > MAX_EXPAND) {
      return `<tr class="diff-gap-static"><td colspan="${span}">⋯ ${count} 行（大きすぎるため展開省略）</td></tr>`;
    }
    return `<tr class="diff-gap" data-seg="${segIdx}"><td colspan="${span}">⋯ ${count} 行を展開</td></tr>`;
  }
  function changeMoreRow(seg, ncols) {
    if (!seg.moreDels && !seg.moreAdds) return '';
    return `<tr class="diff-gap-static"><td colspan="${ncols + 2}">⋯ 変更が大きいため残り ` +
      `-${seg.moreDels || 0} / +${seg.moreAdds || 0} 行は省略</td></tr>`;
  }

  // Math.max(1, ...rows.map(r => r.length)) は行数がエンジンの引数展開上限
  // （V8 等で約 6.5〜12.5 万）を超えると RangeError（Maximum call stack size exceeded）
  // になる。ループで最大値を求める。
  function maxRowLen(rows) {
    let m = 0;
    for (const r of rows) if (r.length > m) m = r.length;
    return Math.max(1, m);
  }

  // モデルペアごとに差分計算結果を 1 度だけ作る（モード切替・行展開クリックで
  // 再計算しない。text-diff.js と同じキャッシュ方式）。
  let cache = { o: null, n: null };
  function ensure(oldModel, newModel) {
    if (cache.o === oldModel && cache.n === newModel) return cache;
    const A = oldModel.rows, B = newModel.rows;
    // 行シグネチャ＝行を JSON 化した文字列で対応付ける。
    const sa = A.map(r => JSON.stringify(r)), sb = B.map(r => JSON.stringify(r));
    const { segs, coarse } = diffKeyed(sa, sb, { maxProduct: LCS_MAX_PRODUCT, maxChangeRows: MAX_CHANGE_ROWS });
    cache = {
      o: oldModel, n: newModel, A, B, segs, coarse,
      ncols: Math.max(maxRowLen(A), maxRowLen(B)),
      expanded: new Set(),
    };
    return cache;
  }

  // セグメント列から表示行（HTML）を組み立てる。equal 領域は text-diff.js と同じ
  // 前後 CONTEXT 行だけを見せ、残りは折りたたむ（クリックで展開）。change 領域は
  // 同じ位置の削除/追加をペアリングしてセル単位ハイライトにする（table 固有）。
  function paint(cache, mode) {
    const { A, B, segs, ncols, expanded } = cache;
    let added = 0, removed = 0, changed = 0;
    const rows = []; // { cls: 'eq'|'gap'|'del'|'add'|'chg'|'more', html }
    segs.forEach((seg, idx) => {
      if (seg.type === 'equal') {
        const count = seg.count;
        const isFirst = idx === 0, isLast = idx === segs.length - 1;
        const full = expanded.has(idx) || count <= CONTEXT * 2 + 1;
        const head = full ? count : (isFirst ? 0 : CONTEXT);
        const tail = full ? 0 : (isLast ? 0 : CONTEXT);
        for (let k = 0; k < head; k++) {
          rows.push({ cls: 'eq', html: plainRow(B[seg.bo + k], ncols, '', seg.ao + k, seg.bo + k) });
        }
        const hidden = count - head - tail;
        if (hidden > 0) rows.push({ cls: 'gap', html: gapRow(hidden, idx, ncols) });
        for (let k = count - tail; k < count; k++) {
          rows.push({ cls: 'eq', html: plainRow(B[seg.bo + k], ncols, '', seg.ao + k, seg.bo + k) });
        }
      } else {
        const pairs = Math.max(seg.dels.length, seg.adds.length);
        for (let p = 0; p < pairs; p++) {
          const di = seg.dels[p], aj = seg.adds[p];
          if (di != null && aj != null) {
            rows.push({ cls: 'chg', html: changedRow(A[di], B[aj], ncols, di, aj) });
            changed++;
          } else if (di != null) {
            rows.push({ cls: 'del', html: plainRow(A[di], ncols, 'diff-del', di, null) });
            removed++;
          } else {
            rows.push({ cls: 'add', html: plainRow(B[aj], ncols, 'diff-add', null, aj) });
            added++;
          }
        }
        const more = changeMoreRow(seg, ncols);
        if (more) rows.push({ cls: 'more', html: more });
      }
    });
    return { rows, added, removed, changed };
  }

  ViewRuntime.registerView({
    kind: 'table/csv',
    label: 'CSV/TSV',
    diff: {
      modes: [{ id: 'all', label: '全行' }, { id: 'changes', label: '変更のみ' }],
      render(container, oldModel, newModel, { mode, stats }) {
        const c = ensure(oldModel, newModel);
        const run = () => {
          const { rows, added, removed, changed } = paint(c, mode);

          if (added + removed + changed === 0) {
            stats.innerHTML = '<span class="opacity-60">差分はありません</span>';
          } else {
            stats.innerHTML =
              `<span class="text-success font-semibold">+${added}</span> ` +
              `<span class="text-error font-semibold">-${removed}</span> ` +
              `<span class="opacity-60">行</span>` +
              (changed ? ` <span class="opacity-60">/ 変更 ${changed} 行</span>` : '') +
              (c.coarse ? ' <span class="opacity-60">（変更が大きいため行単位の対応付けは省略）</span>' : '');
          }

          // 「変更のみ」表示では等値行・折りたたみプレースホルダの両方を除く
          // （旧実装と同じく等値行は完全非表示。フォールディングは「全行」表示のみの
          // DOM 保護策）。
          const visible = mode === 'changes' ? rows.filter(r => r.cls !== 'eq' && r.cls !== 'gap') : rows;
          const colgroup = `<colgroup><col style="width:3rem"><col style="width:3rem"></colgroup>`;
          container.innerHTML =
            `<table class="diff-table">${colgroup}${visible.map(r => r.html).join('')}</table>`;
          container.querySelectorAll('.diff-gap').forEach(el => {
            el.onclick = () => { c.expanded.add(parseInt(el.dataset.seg, 10)); run(); };
          });
        };
        run();
      },
    },
    // 単一表示（file_detail）用。現状は compare のみ使用するが、ビューア統合に備えて
    // グリッド描画も提供しておく（同一プラグインが表示と差分の両方を担う）。
    async mount(container, model) {
      const rows = model.rows || [];
      const ncols = maxRowLen(rows);
      const body = rows.map(r => {
        let cells = '';
        for (let c = 0; c < ncols; c++) cells += `<td class="diff-code">${escapeHtml(r[c] != null ? r[c] : '')}</td>`;
        return `<tr>${cells}</tr>`;
      }).join('');
      container.innerHTML = `<table class="diff-table">${body}</table>`;
    },
  });
})();
