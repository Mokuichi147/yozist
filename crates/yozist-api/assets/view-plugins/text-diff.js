// @ts-check
// core/text ビュープラグイン（行差分: unified / split）。
// base.html 共有 ViewRuntime に登録。内部関数は IIFE スコープに隔離され、
// 外部とはホスト基盤(ViewRuntime.host)と registerView のみで接続する。
ViewRuntime.registerView((() => {
  const { escapeHtml, decodeBytes, looksBinaryText, diffKeyed } = ViewRuntime.host;
  // 中間領域に LCS を許す上限（行数の積）。超えるとブロック置換へ降格。
  const LCS_MAX_PRODUCT = 4_000_000;
  // ブロック置換表示の上限行数（DOM 保護）。
  const MAX_CHANGE_ROWS = 1000;
  const CONTEXT = 3;
  // equal 領域の展開上限（DOM 保護）。
  const MAX_EXPAND = 5000;

  // 1 行ペアの語句単位ハイライト。文字配列にもガード付き LCS（diffKeyed、base.html
  // 共有）を使う（要素比較が `===` で成り立てば行でも文字でも使える）。
  function inlineDiff(oldStr, newStr) {
    if (oldStr.length > 400 || newStr.length > 400) {
      return {
        oldHtml: `<span class="diff-del-inline">${escapeHtml(oldStr)}</span>`,
        newHtml: `<span class="diff-add-inline">${escapeHtml(newStr)}</span>`,
      };
    }
    const oldChars = [...oldStr], newChars = [...newStr];
    const { segs } = diffKeyed(oldChars, newChars, { maxProduct: Infinity });
    let oldHtml = '', newHtml = '';
    for (const seg of segs) {
      if (seg.type === 'equal') {
        const esc = escapeHtml(oldChars.slice(seg.ao, seg.ao + seg.count).join(''));
        oldHtml += esc; newHtml += esc;
      } else {
        if (seg.dels.length) {
          oldHtml += `<span class="diff-del-inline">${escapeHtml(seg.dels.map(i => oldChars[i]).join(''))}</span>`;
        }
        if (seg.adds.length) {
          newHtml += `<span class="diff-add-inline">${escapeHtml(seg.adds.map(i => newChars[i]).join(''))}</span>`;
        }
      }
    }
    return { oldHtml, newHtml };
  }

  // 行差分をセグメント列にする。共通プレフィックス/サフィックスの除去・ガード付き
  // LCS・巨大差分のブロック置換フォールバックは base.html 共有の diffKeyed に委譲する
  // （table-csv.js の行差分と同じ実装を再利用し、DOM/メモリ保護ロジックの重複を防ぐ）。
  function diffSegments(oldLines, newLines) {
    return diffKeyed(oldLines, newLines, { maxProduct: LCS_MAX_PRODUCT, maxChangeRows: MAX_CHANGE_ROWS });
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
  // seg.dels/seg.adds は行インデックスの配列（diffKeyed は行を materialize しない）。
  // k 番目のペアを oldLines/newLines から実データへ写像して {old/new, text} にする。
  const changePair = (oldLines, newLines, seg, k) => ({
    d: seg.dels[k] != null ? { old: seg.dels[k], text: oldLines[seg.dels[k]] } : null,
    a: seg.adds[k] != null ? { new: seg.adds[k], text: newLines[seg.adds[k]] } : null,
  });
  function renderChangeUnified(oldLines, newLines, seg) {
    let out = '';
    const max = Math.max(seg.dels.length, seg.adds.length);
    for (let k = 0; k < max; k++) {
      const { d, a } = changePair(oldLines, newLines, seg, k);
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
  function renderChangeSplit(oldLines, newLines, seg) {
    let out = '';
    const max = Math.max(seg.dels.length, seg.adds.length);
    for (let k = 0; k < max; k++) {
      const { d, a } = changePair(oldLines, newLines, seg, k);
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
    const { segs, oldLines, newLines, expanded } = cache;
    let body = '';
    segs.forEach((seg, idx) => {
      const isFirst = idx === 0, isLast = idx === segs.length - 1;
      if (mode === 'unified') {
        body += seg.type === 'equal'
          ? renderEqualUnified(oldLines, expanded, seg, idx, isFirst, isLast)
          : renderChangeUnified(oldLines, newLines, seg);
      } else {
        body += seg.type === 'equal'
          ? renderEqualSplit(oldLines, expanded, seg, idx, isFirst, isLast)
          : renderChangeSplit(oldLines, newLines, seg);
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
    if (looksBinaryText(oldText) || looksBinaryText(newText)) {
      cache = { o: oldModel, n: newModel, binary: true };
    } else if (oldText === newText) {
      cache = { o: oldModel, n: newModel, equal: true };
    } else {
      const oldLines = oldText.split('\n'), newLines = newText.split('\n');
      const { segs, added, removed, coarse } = diffSegments(oldLines, newLines);
      cache = { o: oldModel, n: newModel, oldLines, newLines, segs, added, removed, coarse, expanded: new Set() };
    }
    return cache;
  }

  return {
    kind: 'core/text',
    label: 'テキスト',
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
