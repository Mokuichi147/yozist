// table/csv ビュープラグイン（拡張例）。
//
// CSV/TSV を「テーブル」として解釈し、行 LCS ＋ セル単位ハイライトで差分表示する。
// 差分アルゴリズムがビュー固有である（テキストの行差分ではなく、表の行・セル差分）
// ことを示す実例。base.html 共有 ViewRuntime に「変換」と「ビュー」を自己登録する。
// このファイルを追加し compare.html から読み込むだけで、コアを改変せず CSV 差分が
// 増える（種別 table/csv はコアにとって単なる文字列キー）。
(() => {
  const { escapeHtml, decodeBytes, bytesLookBinary } = ViewRuntime.host;

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

  // --- 変換: 拡張子で CSV/TSV を判定し、グリッド（行配列）に展開する ---
  ViewRuntime.registerConverter({
    converterId: 'table/csv',
    targetView: 'table/csv',
    // 拡張子で判定（バイナリは除外）。core/text より先に登録されるため先勝ちで拾う。
    detect: (bytes, hint) => {
      const e = extOf(hint && hint.name);
      return (e === 'csv' || e === 'tsv') && !bytesLookBinary(bytes);
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

  // --- 行 LCS（行シグネチャ＝行を JSON 化した文字列で対応付け）---
  function lcsOps(a, b) {
    const n = a.length, m = b.length;
    const dp = [];
    for (let i = 0; i <= n; i++) dp.push(new Int32Array(m + 1));
    for (let i = n - 1; i >= 0; i--) {
      for (let j = m - 1; j >= 0; j--) {
        dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1
          : (dp[i + 1][j] >= dp[i][j + 1] ? dp[i + 1][j] : dp[i][j + 1]);
      }
    }
    const ops = [];
    let i = 0, j = 0;
    while (i < n && j < m) {
      if (a[i] === b[j]) ops.push({ t: '=', i: i++, j: j++ });
      else if (dp[i + 1][j] >= dp[i][j + 1]) ops.push({ t: '-', i: i++ });
      else ops.push({ t: '+', j: j++ });
    }
    while (i < n) ops.push({ t: '-', i: i++ });
    while (j < m) ops.push({ t: '+', j: j++ });
    return ops;
  }

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

  ViewRuntime.registerView({
    kind: 'table/csv',
    diff: {
      modes: [{ id: 'all', label: '全行' }, { id: 'changes', label: '変更のみ' }],
      render(container, oldModel, newModel, { mode, stats }) {
        const A = oldModel.rows, B = newModel.rows;
        const sa = A.map(r => JSON.stringify(r)), sb = B.map(r => JSON.stringify(r));
        const ops = lcsOps(sa, sb);
        const ncols = Math.max(1,
          ...A.map(r => r.length), ...B.map(r => r.length));

        let added = 0, removed = 0, changed = 0;
        const out = []; // { cls, html }
        let k = 0;
        while (k < ops.length) {
          if (ops[k].t === '=') {
            const o = ops[k];
            out.push({ cls: 'eq', html: plainRow(B[o.j], ncols, '', o.i, o.j) });
            k++;
          } else {
            const dels = [], adds = [];
            while (k < ops.length && ops[k].t !== '=') {
              if (ops[k].t === '-') dels.push(ops[k].i); else adds.push(ops[k].j);
              k++;
            }
            const pairs = Math.max(dels.length, adds.length);
            for (let p = 0; p < pairs; p++) {
              const di = dels[p], aj = adds[p];
              if (di != null && aj != null) {
                out.push({ cls: 'chg', html: changedRow(A[di], B[aj], ncols, di, aj) });
                changed++;
              } else if (di != null) {
                out.push({ cls: 'del', html: plainRow(A[di], ncols, 'diff-del', di, null) });
                removed++;
              } else {
                out.push({ cls: 'add', html: plainRow(B[aj], ncols, 'diff-add', null, aj) });
                added++;
              }
            }
          }
        }

        if (added + removed + changed === 0) {
          stats.innerHTML = '<span class="opacity-60">差分はありません</span>';
        } else {
          stats.innerHTML =
            `<span class="text-success font-semibold">+${added}</span> ` +
            `<span class="text-error font-semibold">-${removed}</span> ` +
            `<span class="opacity-60">行</span>` +
            (changed ? ` <span class="opacity-60">/ 変更 ${changed} 行</span>` : '');
        }

        const visible = mode === 'changes' ? out.filter(r => r.cls !== 'eq') : out;
        const colgroup = `<colgroup><col style="width:3rem"><col style="width:3rem"></colgroup>`;
        container.innerHTML =
          `<table class="diff-table">${colgroup}${visible.map(r => r.html).join('')}</table>`;
      },
    },
    // 単一表示（file_detail）用。現状は compare のみ使用するが、ビューア統合に備えて
    // グリッド描画も提供しておく（同一プラグインが表示と差分の両方を担う）。
    async mount(container, model) {
      const rows = model.rows || [];
      const ncols = Math.max(1, ...rows.map(r => r.length));
      const body = rows.map(r => {
        let cells = '';
        for (let c = 0; c < ncols; c++) cells += `<td class="diff-code">${escapeHtml(r[c] != null ? r[c] : '')}</td>`;
        return `<tr>${cells}</tr>`;
      }).join('');
      container.innerHTML = `<table class="diff-table">${body}</table>`;
    },
  });
})();
