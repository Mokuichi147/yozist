// core/text ビュープラグイン（行差分: unified / split）。
// base.html 共有 ViewRuntime に登録。内部関数は IIFE スコープに隔離され、
// 外部とはホスト基盤(ViewRuntime.host)と registerView のみで接続する。
ViewRuntime.registerView((() => {
  const { escapeHtml, decodeBytes } = ViewRuntime.host;
  // 中間領域に LCS を許す上限（行数の積）。超えるとブロック置換へ降格。
  const LCS_MAX_PRODUCT = 4_000_000;
  // ブロック置換表示の上限行数（DOM 保護）。
  const MAX_CHANGE_ROWS = 1000;
  const CONTEXT = 3;
  // equal 領域の展開上限（DOM 保護）。
  const MAX_EXPAND = 5000;

  function looksBinary(s) {
    if (s.indexOf('\u0000') !== -1) return true;
    let ctrl = 0;
    const lim = Math.min(s.length, 4096);
    for (let i = 0; i < lim; i++) {
      const c = s.charCodeAt(i);
      if (c < 9 || (c > 13 && c < 32)) ctrl++;
    }
    return ctrl > lim * 0.1;
  }

  // --- LCS ベースの行/文字差分 ---
  function lcsDiff(a, b) {
    const n = a.length, m = b.length;
    const dp = [];
    for (let i = 0; i <= n; i++) dp.push(new Int32Array(m + 1));
    for (let i = n - 1; i >= 0; i--) {
      const row = dp[i], next = dp[i + 1];
      for (let j = m - 1; j >= 0; j--) {
        row[j] = a[i] === b[j] ? next[j + 1] + 1 : (next[j] >= row[j + 1] ? next[j] : row[j + 1]);
      }
    }
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

  // 1 行ペアの語句単位ハイライト
  function inlineDiff(oldStr, newStr) {
    if (oldStr.length > 400 || newStr.length > 400) {
      return {
        oldHtml: `<span class="diff-del-inline">${escapeHtml(oldStr)}</span>`,
        newHtml: `<span class="diff-add-inline">${escapeHtml(newStr)}</span>`,
      };
    }
    const ops = lcsDiff([...oldStr], [...newStr]);
    let oldHtml = '', newHtml = '';
    let i = 0;
    while (i < ops.length) {
      const t = ops[i].t;
      let buf = '';
      while (i < ops.length && ops[i].t === t) {
        buf += t === '+' ? newStr[ops[i].b] : (t === '-' ? oldStr[ops[i].a] : oldStr[ops[i].a]);
        i++;
      }
      const esc = escapeHtml(buf);
      if (t === '=') { oldHtml += esc; newHtml += esc; }
      else if (t === '-') { oldHtml += `<span class="diff-del-inline">${esc}</span>`; }
      else { newHtml += `<span class="diff-add-inline">${esc}</span>`; }
    }
    return { oldHtml, newHtml };
  }

  // 行差分をセグメント列にする。共通プレフィックス/サフィックスを O(N) で除き、
  // 変化した中間だけ LCS で対応付ける。中間も巨大ならブロック置換に降格する。
  // equal セグメントは行を materialize せず {ao, bo, count} で持つ（メモリ保護）。
  function diffSegments(oldLines, newLines) {
    const n = oldLines.length, m = newLines.length;
    const minLen = Math.min(n, m);
    let p = 0;
    while (p < minLen && oldLines[p] === newLines[p]) p++;
    let s = 0;
    while (s < minLen - p && oldLines[n - 1 - s] === newLines[m - 1 - s]) s++;

    const segs = [];
    let added = 0, removed = 0, coarse = false;
    if (p > 0) segs.push({ type: 'equal', ao: 0, bo: 0, count: p });
    const oMid = n - s - p, nMid = m - s - p;
    if (oMid > 0 || nMid > 0) {
      if (oMid * nMid > LCS_MAX_PRODUCT) {
        coarse = true;
        const dels = [], adds = [];
        const capD = Math.min(oMid, MAX_CHANGE_ROWS), capA = Math.min(nMid, MAX_CHANGE_ROWS);
        for (let i = 0; i < capD; i++) dels.push({ old: p + i, text: oldLines[p + i] });
        for (let j = 0; j < capA; j++) adds.push({ new: p + j, text: newLines[p + j] });
        segs.push({ type: 'change', dels, adds, moreDels: oMid - capD, moreAdds: nMid - capA });
        removed += oMid;
        added += nMid;
      } else {
        const ops = lcsDiff(oldLines.slice(p, n - s), newLines.slice(p, m - s));
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
              if (ops[i].t === '-') { dels.push({ old: ops[i].a + p, text: oldLines[ops[i].a + p] }); removed++; }
              else { adds.push({ new: ops[i].b + p, text: newLines[ops[i].b + p] }); added++; }
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

  function gapRow(count, segIdx) {
    if (count > MAX_EXPAND) {
      return `<tr class="diff-gap-static"><td colspan="4">⋯ ${count} 行（大きすぎるため展開省略）</td></tr>`;
    }
    return `<tr class="diff-gap" data-seg="${segIdx}">
      <td colspan="4">⋯ ${count} 行を展開</td></tr>`;
  }
  function changeMoreRow(seg) {
    if (!seg.moreDels && !seg.moreAdds) return '';
    return `<tr class="diff-gap-static"><td colspan="4">⋯ 変更が大きいため残り ` +
      `-${seg.moreDels || 0} / +${seg.moreAdds || 0} 行は省略</td></tr>`;
  }
  // equal セグメントの k 行目を組み立てる（行は materialize していない）。
  const equalLine = (oldLines, seg, k) => ({ old: seg.ao + k, new: seg.bo + k, text: oldLines[seg.ao + k] });

  function renderEqualUnified(oldLines, expanded, seg, segIdx, isFirst, isLast) {
    const count = seg.count;
    const full = expanded.has(segIdx) || count <= CONTEXT * 2 + 1;
    let out = '';
    const lineRow = ln =>
      `<tr><td class="diff-ln">${ln.old + 1}</td><td class="diff-ln">${ln.new + 1}</td>` +
      `<td class="diff-marker"> </td><td class="diff-code">${escapeHtml(ln.text)}</td></tr>`;
    if (full) {
      for (let k = 0; k < count; k++) out += lineRow(equalLine(oldLines, seg, k));
      return out;
    }
    const head = isFirst ? 0 : CONTEXT;
    const tail = isLast ? 0 : CONTEXT;
    for (let k = 0; k < head; k++) out += lineRow(equalLine(oldLines, seg, k));
    const hidden = count - head - tail;
    if (hidden > 0) out += gapRow(hidden, segIdx);
    for (let k = count - tail; k < count; k++) out += lineRow(equalLine(oldLines, seg, k));
    return out;
  }
  function renderChangeUnified(seg) {
    let out = '';
    const max = Math.max(seg.dels.length, seg.adds.length);
    for (let k = 0; k < max; k++) {
      const d = seg.dels[k], a = seg.adds[k];
      let dHtml, aHtml;
      if (d && a) { const r = inlineDiff(d.text, a.text); dHtml = r.oldHtml; aHtml = r.newHtml; }
      else { dHtml = d ? escapeHtml(d.text) : null; aHtml = a ? escapeHtml(a.text) : null; }
      if (d) out += `<tr class="diff-del"><td class="diff-ln">${d.old + 1}</td>` +
        `<td class="diff-ln"></td><td class="diff-marker">-</td><td class="diff-code">${dHtml}</td></tr>`;
      if (a) out += `<tr class="diff-add"><td class="diff-ln"></td>` +
        `<td class="diff-ln">${a.new + 1}</td><td class="diff-marker">+</td><td class="diff-code">${aHtml}</td></tr>`;
    }
    out += changeMoreRow(seg);
    return out;
  }
  function renderEqualSplit(oldLines, expanded, seg, segIdx, isFirst, isLast) {
    const count = seg.count;
    const full = expanded.has(segIdx) || count <= CONTEXT * 2 + 1;
    let out = '';
    const lineRow = ln =>
      `<tr><td class="diff-ln">${ln.old + 1}</td><td class="diff-code">${escapeHtml(ln.text)}</td>` +
      `<td class="diff-ln">${ln.new + 1}</td><td class="diff-code">${escapeHtml(ln.text)}</td></tr>`;
    if (full) {
      for (let k = 0; k < count; k++) out += lineRow(equalLine(oldLines, seg, k));
      return out;
    }
    const head = isFirst ? 0 : CONTEXT;
    const tail = isLast ? 0 : CONTEXT;
    for (let k = 0; k < head; k++) out += lineRow(equalLine(oldLines, seg, k));
    const hidden = count - head - tail;
    if (hidden > 0) out += gapRow(hidden, segIdx);
    for (let k = count - tail; k < count; k++) out += lineRow(equalLine(oldLines, seg, k));
    return out;
  }
  function renderChangeSplit(seg) {
    let out = '';
    const max = Math.max(seg.dels.length, seg.adds.length);
    for (let k = 0; k < max; k++) {
      const d = seg.dels[k], a = seg.adds[k];
      let dHtml, aHtml;
      if (d && a) { const r = inlineDiff(d.text, a.text); dHtml = r.oldHtml; aHtml = r.newHtml; }
      else { dHtml = d ? escapeHtml(d.text) : ''; aHtml = a ? escapeHtml(a.text) : ''; }
      const left = d
        ? `<td class="diff-ln">${d.old + 1}</td><td class="diff-code diff-del">${dHtml}</td>`
        : `<td class="diff-ln diff-empty"></td><td class="diff-code diff-empty"></td>`;
      const right = a
        ? `<td class="diff-ln">${a.new + 1}</td><td class="diff-code diff-add">${aHtml}</td>`
        : `<td class="diff-ln diff-empty"></td><td class="diff-code diff-empty"></td>`;
      out += `<tr>${left}${right}</tr>`;
    }
    out += changeMoreRow(seg);
    return out;
  }

  function paint(container, cache, mode) {
    const { segs, oldLines, expanded } = cache;
    let body = '';
    segs.forEach((seg, idx) => {
      const isFirst = idx === 0, isLast = idx === segs.length - 1;
      if (mode === 'unified') {
        body += seg.type === 'equal'
          ? renderEqualUnified(oldLines, expanded, seg, idx, isFirst, isLast)
          : renderChangeUnified(seg);
      } else {
        body += seg.type === 'equal'
          ? renderEqualSplit(oldLines, expanded, seg, idx, isFirst, isLast)
          : renderChangeSplit(seg);
      }
    });
    const colgroup = mode === 'unified'
      ? `<colgroup><col style="width:3rem"><col style="width:3rem"><col style="width:1.5rem"><col></colgroup>`
      : `<colgroup><col style="width:3rem"><col style="width:calc(50% - 3rem)"><col style="width:3rem"><col></colgroup>`;
    container.innerHTML = `<table class="diff-table">${colgroup}${body}</table>`;
    container.querySelectorAll('.diff-gap').forEach(el => {
      el.onclick = () => { expanded.add(parseInt(el.dataset.seg, 10)); paint(container, cache, mode); };
    });
  }

  // モデルペアごとに計算結果を 1 度だけ作る（モード切替で LCS を再計算しない）。
  let cache = { o: null, n: null };
  function ensure(oldModel, newModel, charset) {
    if (cache.o === oldModel && cache.n === newModel) return cache;
    const oldText = decodeBytes(oldModel.payload, charset);
    const newText = decodeBytes(newModel.payload, charset);
    if (looksBinary(oldText) || looksBinary(newText)) {
      cache = { o: oldModel, n: newModel, binary: true };
    } else if (oldText === newText) {
      cache = { o: oldModel, n: newModel, equal: true };
    } else {
      const oldLines = oldText.split('\n'), newLines = newText.split('\n');
      const { segs, added, removed, coarse } = diffSegments(oldLines, newLines);
      cache = { o: oldModel, n: newModel, oldLines, segs, added, removed, coarse, expanded: new Set() };
    }
    return cache;
  }

  return {
    kind: 'core/text',
    diff: {
      modes: [{ id: 'unified', label: 'unified' }, { id: 'split', label: 'split' }],
      render(container, oldModel, newModel, { mode, ctx, stats }) {
        const c = ensure(oldModel, newModel, ctx.charset);
        if (c.binary) {
          stats.textContent = ''; container.innerHTML = '';
          return { message: 'バイナリデータのため差分表示できません。' };
        }
        if (c.equal) {
          stats.innerHTML = '<span class="opacity-60">差分はありません</span>';
          container.innerHTML = '';
          return;
        }
        stats.innerHTML =
          `<span class="text-success font-semibold">+${c.added}</span> ` +
          `<span class="text-error font-semibold">-${c.removed}</span> ` +
          `<span class="opacity-60">行</span>` +
          (c.coarse ? ' <span class="opacity-60">（変更が大きいため行単位の対応付けは省略）</span>' : '');
        paint(container, c, mode);
      },
    },
  };
})());
