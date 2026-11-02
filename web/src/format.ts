export function esc(value: unknown): string {
  return String(value ?? '')
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

/** Enough Markdown for a transcript: bold, code spans, paragraphs, line breaks. */
export function mdText(text: string): string {
  return `<p>${esc(text)
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\n\n/g, '</p><p>')
    .replace(/\n/g, '<br>')}</p>`;
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
