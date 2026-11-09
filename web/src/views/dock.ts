import type { ArtifactView } from '../types';
import { esc, mdDoc } from '../format';

/** A worksheet as a table, with merged ranges turned into colspan/rowspan. */
function sheetTable(rows: string[][], merges: ArtifactView['merges']): string {
  const spans = new Map<string, { colspan: number; rowspan: number }>();
  const covered = new Set<string>();
  for (const merge of merges ?? []) {
    if (merge.r1 === merge.r2 && merge.c1 === merge.c2) continue;
    spans.set(`${merge.r1}:${merge.c1}`, {
      colspan: merge.c2 - merge.c1 + 1,
      rowspan: merge.r2 - merge.r1 + 1,
    });
    for (let row = merge.r1; row <= merge.r2; row += 1) {
      for (let column = merge.c1; column <= merge.c2; column += 1) {
        if (row !== merge.r1 || column !== merge.c1) covered.add(`${row}:${column}`);
      }
    }
  }
  const columns = Math.max(
    1,
    ...rows.map((row) => row.length),
    ...(merges ?? []).map((merge) => merge.c2 + 1),
  );
  const body = rows
    .map((row, rowIndex) => {
      let cells = '';
      for (let column = 0; column < columns; column += 1) {
        if (covered.has(`${rowIndex}:${column}`)) continue;
        const span = spans.get(`${rowIndex}:${column}`);
        const size = span ? ` colspan="${span.colspan}" rowspan="${span.rowspan}"` : '';
        cells += `<td${size}>${esc(row[column] ?? '')}</td>`;
      }
      return `<tr>${cells}</tr>`;
    })
    .join('');
  return body || '<tr><td>这张表是空的。</td></tr>';
}

function previewHtml(view: ArtifactView | null): string {
  if (!view) return '<div class="y-panel-empty">选择一个产出…</div>';
  const bar = `<div class="y-art-bar"><span>${esc(view.name)}</span><span>${esc(view.contentType ?? '')}</span></div>`;
  if (view.kind === 'image') return `${bar}<div class="y-art-image"><img src="${esc(view.url)}" alt="${esc(view.name)}" /></div>`;
  if (view.kind === 'html') return `${bar}<iframe class="y-art-frame" sandbox="" srcdoc="${esc(view.text ?? '')}" title="${esc(view.name)}"></iframe>`;
  if (view.kind === 'markdown') return `${bar}<div class="y-art-md">${mdDoc(view.text ?? '')}</div>`;
  if (view.kind === 'pdf') return `${bar}<div class="y-art-image"><embed class="y-art-pdf" type="application/pdf" src="${esc(view.url)}" title="${esc(view.name)}" /></div>`;
  if (view.kind === 'docx') {
    const blocks: { text: string; heading?: number }[] =
      view.blocks?.length ? view.blocks : (view.paragraphs ?? []).map((text) => ({ text }));
    return `${bar}<div class="y-art-docx">${
      blocks
        .map((block) =>
          !block.text.trim()
            ? '<p class="gap"></p>'
            : block.heading
              ? `<h3 class="y-art-docx-h">${esc(block.text)}</h3>`
              : `<p>${esc(block.text)}</p>`,
        )
        .join('') || '<div class="y-panel-empty">这个文档没有可读的段落。</div>'
    }</div>`;
  }
  if (view.kind === 'xlsx') {
    const rows = view.rows ?? [];
    return `${bar}<div class="y-art-sheet"><div class="y-art-sheet-name">${esc(view.sheet ?? '')}</div><table>${sheetTable(rows, view.merges)}</table></div>`;
  }
  return `${bar}<pre class="y-art-text">${esc(view.text ?? '')}</pre>`;
}



export { previewHtml, sheetTable };
