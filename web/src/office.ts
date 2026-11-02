// .docx / .xlsx previews, read where the decompressor already is: the browser.
//
// The artifact store is content-addressed and the server has no document
// converter, so a Word or Excel file arrives as bytes. Both formats are ZIP
// containers of XML; `DecompressionStream('deflate-raw')` is part of the
// platform, and a preview needs one part of each — `word/document.xml` for
// paragraphs, `xl/sharedStrings.xml` + a worksheet for cells. No styles, no
// images, no second renderer: enough to see what the file says, which is the
// same promise the Markdown preview keeps.
//
// Everything here treats the bytes as untrusted: sizes are capped, names are
// looked up rather than globbed, and the text is returned as strings that the
// view escapes before it reaches the DOM.

export interface OfficeText {
  kind: 'docx' | 'xlsx';
  paragraphs?: string[];
  rows?: string[][];
  sheet?: string;
}

const EOCD_SIGNATURE = 0x06054b50;
const CENTRAL_SIGNATURE = 0x02014b50;
const LOCAL_SIGNATURE = 0x04034b50;
const MAX_ARCHIVE = 32 * 1024 * 1024;
const MAX_ROWS = 200;
const MAX_COLUMNS = 30;

export function looksOffice(bytes: Uint8Array): boolean {
  return bytes.length > 4 && bytes[0] === 0x50 && bytes[1] === 0x4b && bytes[2] === 0x03 && bytes[3] === 0x04;
}

export async function readOffice(buffer: ArrayBuffer, name: string): Promise<OfficeText> {
  if (buffer.byteLength > MAX_ARCHIVE) throw new Error('文件太大，超过 32 MB 不预览');
  const files = await unzip(new Uint8Array(buffer));
  const lower = name.toLowerCase();
  if (lower.endsWith('.docx')) return { kind: 'docx', paragraphs: readDocx(files) };
  if (lower.endsWith('.xlsx')) return { kind: 'xlsx', ...readXlsx(files) };
  throw new Error('只预览 .docx 与 .xlsx');
}

/** The named entries this preview needs, decompressed. */
async function unzip(bytes: Uint8Array): Promise<Map<string, Uint8Array>> {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const end = findEndOfCentralDirectory(view);
  const count = view.getUint16(end + 10, true);
  const directory = view.getUint32(end + 16, true);
  const out = new Map<string, Uint8Array>();
  let cursor = directory;
  for (let index = 0; index < count; index += 1) {
    if (cursor + 46 > bytes.length || view.getUint32(cursor, true) !== CENTRAL_SIGNATURE) {
      throw new Error('ZIP 中央目录读不出来');
    }
    const method = view.getUint16(cursor + 10, true);
    const compressed = view.getUint32(cursor + 20, true);
    const nameLength = view.getUint16(cursor + 28, true);
    const extraLength = view.getUint16(cursor + 30, true);
    const commentLength = view.getUint16(cursor + 32, true);
    const localOffset = view.getUint32(cursor + 42, true);
    const name = new TextDecoder().decode(bytes.subarray(cursor + 46, cursor + 46 + nameLength));
    if (localOffset + 30 > bytes.length || view.getUint32(localOffset, true) !== LOCAL_SIGNATURE) {
      throw new Error('ZIP 条目头读不出来');
    }
    const localName = view.getUint16(localOffset + 26, true);
    const localExtra = view.getUint16(localOffset + 28, true);
    const start = localOffset + 30 + localName + localExtra;
    const data = bytes.subarray(start, start + compressed);
    out.set(name, method === 0 ? data : method === 8 ? await inflateRaw(data) : new Uint8Array());
    cursor += 46 + nameLength + extraLength + commentLength;
  }
  return out;
}

function findEndOfCentralDirectory(view: DataView): number {
  const earliest = Math.max(0, view.byteLength - 65_557);
  for (let at = view.byteLength - 22; at >= earliest; at -= 1) {
    if (view.getUint32(at, true) === EOCD_SIGNATURE) return at;
  }
  throw new Error('这不是一个完整的 ZIP（找不到中央目录）');
}

async function inflateRaw(data: Uint8Array): Promise<Uint8Array> {
  if (typeof DecompressionStream === 'undefined') throw new Error('这个浏览器不支持解压');
  // A copy pins the bytes to one plain ArrayBuffer; `Blob` will not take a
  // view whose buffer the type system cannot prove is not shared.
  const copy = new Uint8Array(data.length);
  copy.set(data);
  const stream = new Blob([copy.buffer]).stream().pipeThrough(new DecompressionStream('deflate-raw'));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

function xmlText(xml: string): string {
  return xml.replace(/&(#x?[0-9a-f]+|[a-z]+);/gi, (whole, code: string) => {
    const lower = code.toLowerCase();
    if (lower === 'lt') return '<';
    if (lower === 'gt') return '>';
    if (lower === 'amp') return '&';
    if (lower === 'quot') return '"';
    if (lower === 'apos') return "'";
    const numeric = lower.startsWith('#x') ? Number.parseInt(lower.slice(2), 16) : Number.parseInt(lower.slice(1), 10);
    return Number.isFinite(numeric) ? String.fromCodePoint(numeric) : whole;
  });
}

function text(files: Map<string, Uint8Array>, name: string): string {
  const bytes = files.get(name);
  return bytes ? new TextDecoder('utf-8').decode(bytes) : '';
}

function readDocx(files: Map<string, Uint8Array>): string[] {
  const xml = text(files, 'word/document.xml');
  if (!xml) throw new Error('docx 里没有 word/document.xml');
  const paragraphs: string[] = [];
  for (const match of xml.matchAll(/<w:p[ >][\s\S]*?<\/w:p>/g)) {
    const fragment = match[0];
    let line = '';
    const tokens = /<w:t(?: [^>]*)?>([^<]*)<\/w:t>|<w:tab\/>|<w:br\/>/g;
    let token: RegExpExecArray | null;
    while ((token = tokens.exec(fragment)) !== null) {
      if (token[1] !== undefined) line += xmlText(token[1]);
      else if (token[0] === '<w:tab/>') line += '\t';
      else line += '\n';
    }
    paragraphs.push(line);
  }
  return paragraphs;
}

function readXlsx(files: Map<string, Uint8Array>): { rows: string[][]; sheet: string } {
  const sharedXml = text(files, 'xl/sharedStrings.xml');
  const shared: string[] = [];
  for (const item of sharedXml.matchAll(/<si>([\s\S]*?)<\/si>/g)) {
    let value = '';
    for (const run of item[1].matchAll(/<t(?: [^>]*)?>([\s\S]*?)<\/t>/g)) value += xmlText(run[1]);
    shared.push(value);
  }
  const sheets = [...files.keys()].filter((entry) => /^xl\/worksheets\/[^/]+\.xml$/.test(entry)).sort();
  const sheetName = sheets[0] ?? '';
  if (!sheetName) throw new Error('xlsx 里没有工作表');
  const xml = text(files, sheetName);
  const rows: string[][] = [];
  for (const row of xml.matchAll(/<row[^>]*>([\s\S]*?)<\/row>/g)) {
    if (rows.length >= MAX_ROWS) break;
    const cells: string[] = [];
    for (const cell of row[1].matchAll(/<c([^>]*)>([\s\S]*?)<\/c>/g)) {
      const attributes = cell[1];
      const column = columnIndex(/r="([A-Z]+)\d+"/.exec(attributes)?.[1] ?? '');
      if (column >= MAX_COLUMNS) continue;
      const type = /t="([^"]+)"/.exec(attributes)?.[1] ?? '';
      let value = '';
      if (type === 'inlineStr') {
        for (const run of cell[2].matchAll(/<t(?: [^>]*)?>([\s\S]*?)<\/t>/g)) value += xmlText(run[1]);
      } else {
        const raw = /<v>([\s\S]*?)<\/v>/.exec(cell[2])?.[1] ?? '';
        value = type === 's' ? (shared[Number(raw)] ?? '') : type === 'b' ? (raw === '1' ? 'TRUE' : 'FALSE') : xmlText(raw);
      }
      cells[column] = value;
    }
    // A row of empty cells is spacing, not data; keep interior gaps so columns
    // line up.
    for (let index = 0; index < cells.length; index += 1) if (cells[index] === undefined) cells[index] = '';
    if (cells.some((value) => value !== '')) rows.push(cells);
    else if (rows.length) rows.push([]);
  }
  return { rows, sheet: sheetName.split('/').pop() ?? sheetName };
}

function columnIndex(letters: string): number {
  let index = 0;
  for (const letter of letters) index = index * 26 + (letter.charCodeAt(0) - 64);
  return Math.max(0, index - 1);
}
