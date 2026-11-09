import type { ArtifactView, DocxBlock, DocxRun } from '../types';
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

function docxRunHtml(run: DocxRun): string {
  const classes = ['y-run', ...(run.bold ? ['b'] : []), ...(run.italic ? ['i'] : [])];
  const styles: string[] = [];
  if (run.size && Number.isFinite(run.size)) styles.push(`font-size:${Math.min(96, Math.max(1, run.size))}pt`);
  if (run.color && /^[0-9a-f]{6}$/i.test(run.color)) styles.push(`color:#${run.color}`);
  return `<span class="${classes.join(' ')}"${styles.length ? ` style="${styles.join(';')}"` : ''}>${esc(run.text)}</span>`;
}

function docxBlockHtml(block: DocxBlock): string {
  const text = block.runs?.length ? block.runs.map(docxRunHtml).join('') : esc(block.text);
  let image = '';
  if (block.image) {
    const alt = block.image.alt || '图片';
    const src =
      block.image.src && /^data:image\/(?:png|jpeg|gif|webp|bmp);base64,/i.test(block.image.src)
        ? block.image.src
        : '';
    image = src
      ? `<figure class="y-art-docx-image"><img src="${esc(src)}" alt="${esc(alt)}" /><figcaption>${esc(alt)}</figcaption></figure>`
      : `<div class="y-art-image-missing" role="img" aria-label="${esc(alt)}">[图片：${esc(alt)}]</div>`;
  }
  if (!text.trim()) return image || '<p class="gap"></p>';
  const paragraph = block.heading
    ? `<h3 class="y-art-docx-h">${text}</h3>`
    : `<p>${text}</p>`;
  return `${image}${paragraph}`;
}

function previewHtml(view: ArtifactView | null): string {
  if (!view) return '<div class="y-panel-empty">选择一个产出…</div>';
  const bar = `<div class="y-art-bar"><span>${esc(view.name)}</span><span>${esc(view.contentType ?? '')}</span></div>`;
  const warnings = view.warnings?.length
    ? `<div class="y-art-warning" role="status">${view.warnings.map((warning) => esc(warning)).join('<br>')}</div>`
    : '';
  const head = `${bar}${warnings}`;
  if (view.kind === 'image') return `${head}<div class="y-art-image"><img src="${esc(view.url)}" alt="${esc(view.name)}" /></div>`;
  if (view.kind === 'html') return `${head}<iframe class="y-art-frame" sandbox="" srcdoc="${esc(view.text ?? '')}" title="${esc(view.name)}"></iframe>`;
  if (view.kind === 'markdown') return `${head}<div class="y-art-md">${mdDoc(view.text ?? '')}</div>`;
  if (view.kind === 'pdf') return `${head}<div class="y-art-image"><embed class="y-art-pdf" type="application/pdf" src="${esc(view.url)}" title="${esc(view.name)}" /></div>`;
  if (view.kind === 'docx') {
    const blocks: DocxBlock[] = view.blocks?.length ? view.blocks : (view.paragraphs ?? []).map((text) => ({ text }));
    return `${head}<div class="y-art-docx">${
      blocks.map(docxBlockHtml).join('') || '<div class="y-panel-empty">这个文档没有可读的段落。</div>'
    }</div>`;
  }
  if (view.kind === 'xlsx') {
    const rows = view.rows ?? [];
    return `${head}<div class="y-art-sheet"><div class="y-art-sheet-name">${esc(view.sheet ?? '')}</div><table>${sheetTable(rows, view.merges)}</table></div>`;
  }
  return `${head}<pre class="y-art-text">${esc(view.text ?? '')}</pre>`;
}



export { previewHtml, sheetTable };
