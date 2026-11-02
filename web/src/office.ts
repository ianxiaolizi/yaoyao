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

export interface OfficeBlock {
  text: string;
  /** 1–6 for a Word heading style; absent for body text. */
  heading?: number;
}

export interface MergeRange {
  r1: number;
  c1: number;
  r2: number;
  c2: number;
}

export interface OfficeText {
  kind: 'docx' | 'xlsx';
  paragraphs?: string[];
  blocks?: OfficeBlock[];
  rows?: string[][];
  merges?: MergeRange[];
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
  if (lower.endsWith('.docx')) {
    const blocks = readDocx(files);
    return { kind: 'docx', paragraphs: blocks.map((block) => block.text), blocks };
  }
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

/** Paragraph styles that mean "heading"; Word stores the level in the name. */
function docxHeadingStyles(files: Map<string, Uint8Array>): Map<string, number> {
  const xml = text(files, 'word/styles.xml');
  const styles = new Map<string, number>();
  for (const style of xml.matchAll(/<w:style\b[^>]*>[\s\S]*?<\/w:style>/g)) {
    const attributes = /<w:style\b([^>]*)>/.exec(style[0])?.[1] ?? '';
    if (!/w:type="paragraph"/.test(attributes)) continue;
    const id = /w:styleId="([^"]+)"/.exec(attributes)?.[1];
    if (!id) continue;
    const name = xmlText(/<w:name w:val="([^"]+)"/.exec(style[0])?.[1] ?? '');
    const level = /heading\s*([1-6])/i.exec(name)?.[1] ?? (/^title$/i.test(name) ? '1' : null);
    if (level) styles.set(id, Number(level));
  }
  return styles;
}

function readDocx(files: Map<string, Uint8Array>): OfficeBlock[] {
  const xml = text(files, 'word/document.xml');
  if (!xml) throw new Error('docx 里没有 word/document.xml');
  const styles = docxHeadingStyles(files);
  const blocks: OfficeBlock[] = [];
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
    const styleId = /<w:pStyle w:val="([^"]+)"/.exec(fragment)?.[1];
    const heading = styleId ? styles.get(styleId) : undefined;
    blocks.push({ text: line, heading });
  }
  return blocks;
}

const DATE_FORMAT_IDS = new Set([
  14, 15, 16, 17, 18, 19, 20, 21, 22, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 45, 46, 47, 50, 51,
  52, 53, 54, 55, 56, 57, 58,
]);

/** Which cell styles render a number as a date, from `xl/styles.xml`. */
function xlsxDateStyles(files: Map<string, Uint8Array>): boolean[] {
  const xml = text(files, 'xl/styles.xml');
  if (!xml) return [];
  const custom = new Map<number, string>();
  for (const match of xml.matchAll(/<numFmt\b[^>]*numFmtId="(\d+)"[^>]*formatCode="([^"]*)"/g)) {
    custom.set(Number(match[1]), xmlText(match[2]));
  }
  const styles: boolean[] = [];
  const cellXfs = /<cellXfs\b[^>]*>([\s\S]*?)<\/cellXfs>/.exec(xml)?.[1] ?? '';
  for (const match of cellXfs.matchAll(/<xf\b[^>]*numFmtId="(\d+)"/g)) {
    const id = Number(match[1]);
    const code = custom.get(id);
    styles.push(code ? /[ymdhs]/i.test(code.replace(/"[^"]*"/g, '')) : DATE_FORMAT_IDS.has(id));
  }
  return styles;
}

/** Excel's day 0 is 1899-12-30; serial 60 is the fake 1900-02-29. */
function excelDate(serial: number): string {
  const at = new Date(Date.UTC(1899, 11, 30) + Math.round(serial * 86_400_000));
  const pad = (value: number): string => String(value).padStart(2, '0');
  const date = `${at.getUTCFullYear()}-${pad(at.getUTCMonth() + 1)}-${pad(at.getUTCDate())}`;
  const time = `${pad(at.getUTCHours())}:${pad(at.getUTCMinutes())}`;
  return time === '00:00' ? date : `${date} ${time}`;
}

function readXlsx(files: Map<string, Uint8Array>): {
  rows: string[][];
  merges: MergeRange[];
  sheet: string;
} {
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
  const dates = xlsxDateStyles(files);
  const rows: string[][] = [];
  for (const row of xml.matchAll(/<row([^>]*)>([\s\S]*?)<\/row>/g)) {
    const number = Number(/r="(\d+)"/.exec(row[1])?.[1] ?? 0);
    if (number > MAX_ROWS) break;
    // Keep the sheet's own row numbers: a merge is written as A1:B2, so a
    // leading blank row that was dropped would shift every span below it.
    while (rows.length < (number > 0 ? number - 1 : rows.length)) rows.push([]);
    if (rows.length >= MAX_ROWS) break;
    const cells: string[] = [];
    for (const cell of row[2].matchAll(/<c([^>]*)>([\s\S]*?)<\/c>/g)) {
      const attributes = cell[1];
      const column = columnIndex(/r="([A-Z]+)\d+"/.exec(attributes)?.[1] ?? '');
      if (column >= MAX_COLUMNS) continue;
      const type = /t="([^"]+)"/.exec(attributes)?.[1] ?? '';
      const style = Number(/s="(\d+)"/.exec(attributes)?.[1] ?? -1);
      let value = '';
      if (type === 'inlineStr') {
        for (const run of cell[2].matchAll(/<t(?: [^>]*)?>([\s\S]*?)<\/t>/g)) value += xmlText(run[1]);
      } else {
        const raw = /<v>([\s\S]*?)<\/v>/.exec(cell[2])?.[1] ?? '';
        value =
          type === 's'
            ? (shared[Number(raw)] ?? '')
            : type === 'b'
              ? raw === '1'
                ? 'TRUE'
                : 'FALSE'
              : style >= 0 && dates[style] && raw !== '' && Number.isFinite(Number(raw))
                ? excelDate(Number(raw))
                : xmlText(raw);
      }
      cells[column] = value;
    }
    // A row of empty cells is spacing, not data; keep interior gaps so columns
    // line up.
    for (let index = 0; index < cells.length; index += 1) if (cells[index] === undefined) cells[index] = '';
    if (cells.some((value) => value !== '')) rows.push(cells);
    else if (rows.length) rows.push([]);
  }
  const merges: MergeRange[] = [];
  for (const match of xml.matchAll(/<mergeCell\b[^>]*ref="([A-Z]+)(\d+):([A-Z]+)(\d+)"/g)) {
    const r1 = Number(match[2]) - 1;
    const c1 = columnIndex(match[1]);
    const r2 = Number(match[4]) - 1;
    const c2 = columnIndex(match[3]);
    if (r1 < MAX_ROWS && c1 < MAX_COLUMNS && r2 < MAX_ROWS && c2 < MAX_COLUMNS) {
      merges.push({ r1, c1, r2, c2 });
    }
  }
  return { rows, merges, sheet: sheetName.split('/').pop() ?? sheetName };
}

function columnIndex(letters: string): number {
  let index = 0;
  for (const letter of letters) index = index * 26 + (letter.charCodeAt(0) - 64);
  return Math.max(0, index - 1);
}
