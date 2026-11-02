export function esc(value: unknown): string {
  return String(value ?? '')
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

/**
 * One URL policy for Markdown: links may leave the page, images may only be
 * bytes the page already holds. Artifact text is untrusted, so a Markdown image
 * must never make the page fetch a third-party URL; external images stay links.
 */
function linkUrl(raw: string): string | null {
  const url = raw.trim();
  if (/^(https?:|mailto:)/i.test(url)) return url;
  // A document-relative link cannot reach another origin and is useful in
  // transcripts; a protocol-relative `//host` could, so it is excluded.
  if (/^(\.{0,2}\/|\/)(?!\/)/.test(url)) return url;
  return null;
}

function imageUrl(raw: string): string | null {
  const url = raw.trim();
  if (/^blob:/i.test(url)) return url;
  // SVG can carry script; the `<img>` context does not run it, but the store
  // has no reason to accept an active format when the raster ones cover it.
  if (/^data:image\/(png|jpeg|gif|webp);base64,[A-Za-z0-9+/=]+$/i.test(url)) return url;
  return null;
}

const CODE_MARK = '\uE000';

/** Inline Markdown on already-escaped text: code, images, links, bold. */
function inlineMd(text: string): string {
  const code: string[] = [];
  let out = esc(text).replace(/`([^`]+)`/g, (_match, body: string) => {
    code.push(body);
    return `${CODE_MARK}${code.length - 1}${CODE_MARK}`;
  });

  out = out.replace(
    /!\[([^\]]*)\]\(([^)\s]+)\)/g,
    (match, alt: string, href: string) => {
      const src = imageUrl(href);
      if (src) return `<img src="${src}" alt="${alt}" loading="lazy" />`;
      const link = linkUrl(href);
      return link
        ? `<a href="${link}" target="_blank" rel="noreferrer noopener">图片：${alt || '外链'}</a>`
        : alt || match;
    },
  );

  out = out.replace(
    /\[([^\]]+)\]\(([^)\s]+)\)/g,
    (match, label: string, href: string) => {
      const url = linkUrl(href);
      return url
        ? `<a href="${url}" target="_blank" rel="noreferrer noopener">${label}</a>`
        : match;
    },
  );

  out = out.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  out = out.replace(/\*([^*\n]+)\*/g, '<em>$1</em>');

  return out.replace(new RegExp(`${CODE_MARK}(\\d+)${CODE_MARK}`, 'g'), (_match, index: string) => `<code>${code[Number(index)]}</code>`);
}

/**
 * Enough Markdown for a transcript and an artifact: headings, paragraphs,
 * fenced code, unordered/ordered lists, tables, code spans, bold/italic, links
 * and images. It is deliberately a renderer, not an HTML passthrough.
 */
export function mdDoc(text: string): string {
  const lines = String(text ?? '').split(/\r?\n/);
  const out: string[] = [];
  let paragraph: string[] = [];
  let list: 'ul' | 'ol' | null = null;
  let fence = false;
  let code: string[] = [];

  const closeList = (): void => {
    if (list) {
      out.push(`</${list}>`);
      list = null;
    }
  };
  const closeParagraph = (): void => {
    if (paragraph.length) {
      out.push(`<p>${paragraph.join('<br>')}</p>`);
      paragraph = [];
    }
  };
  const closeBlocks = (): void => {
    closeParagraph();
    closeList();
  };

  const cells = (line: string): string[] =>
    line
      .trim()
      .replace(/^\|/, '')
      .replace(/\|$/, '')
      .split('|')
      .map((cell) => cell.trim());
  const separator = (line: string): boolean => {
    const row = cells(line);
    return row.length > 0 && row.every((cell) => /^:?-{3,}:?$/.test(cell));
  };
  const tableStart = (line: string, next: string | undefined): boolean =>
    line.includes('|') && next !== undefined && separator(next);

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];

    if (fence) {
      if (/^```/.test(line.trim())) {
        out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`);
        code = [];
        fence = false;
      } else {
        code.push(line);
      }
      continue;
    }
    if (/^```/.test(line.trim())) {
      closeBlocks();
      fence = true;
      continue;
    }

    if (tableStart(line, lines[i + 1])) {
      closeBlocks();
      const head = cells(line);
      i += 2;
      const body: string[][] = [];
      while (i < lines.length && lines[i].includes('|') && lines[i].trim()) {
        body.push(cells(lines[i]));
        i += 1;
      }
      i -= 1;
      out.push(
        `<table><thead><tr>${head.map((cell) => `<th>${inlineMd(cell)}</th>`).join('')}</tr></thead><tbody>${body
          .map((row) => `<tr>${row.map((cell) => `<td>${inlineMd(cell)}</td>`).join('')}</tr>`)
          .join('')}</tbody></table>`,
      );
      continue;
    }

    if (/^#{1,6}\s+/.test(line)) {
      closeBlocks();
      const body = line.replace(/^#{1,6}\s+/, '');
      out.push(`<h3>${inlineMd(body)}</h3>`);
      continue;
    }

    const ordered = line.match(/^\s*\d+\.\s+(.*)$/);
    const bullet = line.match(/^\s*[-*]\s+(.*)$/);
    if (ordered || bullet) {
      closeParagraph();
      const kind = ordered ? 'ol' : 'ul';
      if (list !== kind) {
        closeList();
        out.push(`<${kind}>`);
        list = kind;
      }
      out.push(`<li>${inlineMd((ordered ?? bullet)?.[1] ?? '')}</li>`);
      continue;
    }

    if (!line.trim()) {
      closeBlocks();
      continue;
    }

    paragraph.push(inlineMd(line));
  }

  if (fence) out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`);
  closeBlocks();
  return out.join('');
}

/** A single transcript answer; kept as the old name for call sites. */
export function mdText(text: string): string {
  return mdDoc(text);
}

export function argsSummary(value: unknown): string {
  if (value == null) return '';
  if (typeof value === 'string') return value;
  const text = JSON.stringify(value);
  return text.length > 96 ? `${text.slice(0, 93)}…` : text;
}

export function fmtTokens(value: unknown): string {
  const n = Number(value || 0);
  if (!n) return '0';
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, '')}m`;
  if (n >= 1000) return `${(n / 1000).toFixed(1).replace(/\.0$/, '')}k`;
  return String(n);
}

export function relativeTime(iso: string | undefined): string {
  if (!iso) return '';
  const then = Date.parse(iso);
  if (!Number.isFinite(then)) return '';
  const seconds = Math.max(0, Math.round((Date.now() - then) / 1000));
  if (seconds < 60) return '刚刚';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.round(hours / 24);
  if (days < 7) return `${days} 天前`;
  return new Date(then).toLocaleDateString('zh-CN', { month: 'numeric', day: 'numeric' });
}

export function dueTime(iso: string | undefined): string {
  if (!iso) return '';
  const when = Date.parse(iso);
  if (!Number.isFinite(when)) return iso;
  return new Date(when).toLocaleString('zh-CN', {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  });
}

export function duration(ms: number | undefined): string {
  if (!ms) return '';
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

export function turnStatus(item: { ending?: string; stop_reason?: string } | undefined): {
  label: string;
  className: string;
} {
  if (!item) return { label: '进行中', className: 'run' };
  switch (item.ending) {
    case 'answered':
      return { label: '完成', className: 'done' };
    case 'max_steps':
      return { label: '步数用尽', className: 'fail' };
    case 'limit_reached':
      return { label: '达到上限', className: 'fail' };
    case 'content_filtered':
      return { label: '被过滤', className: 'fail' };
    case 'yielded':
      return { label: '等待确认', className: 'wait' };
    case 'failed':
      return { label: '失败', className: 'fail' };
    case 'cancelled':
      return { label: '已停止', className: 'rej' };
    default:
      return { label: item.ending ?? '完成', className: 'done' };
  }
}
