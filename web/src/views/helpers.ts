import type { Item } from '../types';
import { artifactRefs } from '../artifacts';
import { esc, turnStatus } from '../format';

export interface Group {
  turn: string;
  items: Item[];
  user?: Item;
  end?: Item;
  index: number;
}

/** `h:mm:ss` above an hour, `m:ss` below it; the gate's TTL ticks on this. */
function clockText(ms: number): string {
  const total = Math.max(0, Math.ceil(ms / 1000));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  const pad = (value: number): string => String(value).padStart(2, '0');
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(seconds)}` : `${minutes}:${pad(seconds)}`;
}

/** The gate's countdown line. `expired` is the projection's word, not a guess. */
function ttlText(expiresAt: string | undefined, expired: boolean): string {
  if (expired) return '已过期';
  const left = expiresAt ? Date.parse(expiresAt) - Date.now() : 0;
  return left > 0 ? `${clockText(left)} 后失效` : '已过期';
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function groupsOf(items: Item[]): Group[] {
  const sorted = [...items].sort((a, b) => a.seq - b.seq);
  const map = new Map<string, Group>();
  let last = 'session';
  for (const item of sorted) {
    if (item.turn) last = item.turn;
    const key = item.turn || last;
    let group = map.get(key);
    if (!group) {
      group = { turn: key, items: [], index: map.size + 1 };
      map.set(key, group);
    }
    group.items.push(item);
    if (item.kind === 'user' && !group.user) group.user = item;
    if (item.kind === 'turn_end') group.end = item;
  }
  return [...map.values()];
}

// The items map is replaced whenever a durable item arrives, and untouched by
// live-preview frames. Grouping and artifact discovery both walk every item, so
// they are memoised on the map's identity: streaming a long reply redraws many
// times per second but folds the log once, not once per frame.
let groupsCache: { items: Map<string, Item>; groups: Group[] } | null = null;
function groupsFor(items: Map<string, Item>): Group[] {
  if (groupsCache?.items !== items) groupsCache = { items, groups: groupsOf([...items.values()]) };
  return groupsCache.groups;
}

let refsCache: { items: Map<string, Item>; refs: ReturnType<typeof artifactRefs> } | null = null;
function refsFor(items: Map<string, Item>): ReturnType<typeof artifactRefs> {
  if (refsCache?.items !== items) refsCache = { items, refs: artifactRefs(items.values()) };
  return refsCache.refs;
}

function turnTitle(group: Group): string {
  if (group.user && !group.user.resumed && group.user.text?.trim()) {
    return group.user.text.replace(/\s+/g, ' ').trim();
  }
  return '继续上一轮';
}

/** The prompt's clock, for the turn rail; empty on logs written before `at`. */
function clockAt(iso: string | undefined): string {
  if (!iso) return '';
  const at = Date.parse(iso);
  if (!Number.isFinite(at)) return '';
  return new Date(at).toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  });
}

/** What a turn touched, counted from its own action rows. */
function turnFacts(group: Group): { steps: number; files: number } {
  const files = new Set<string>();
  let steps = 0;
  for (const item of group.items) {
    if (item.kind !== 'action') continue;
    steps += 1;
    const args = item.arguments;
    if (args && typeof args === 'object') {
      const path = (args as Record<string, unknown>).path;
      if (typeof path === 'string' && path) files.add(path);
    }
  }
  return { steps, files: files.size };
}

/** The three lines the rail shows on hover / focus / Alt↑↓. */
function tracePopHtml(group: Group): string {
  const status = turnStatus(group.end);
  const at = clockAt(group.user?.at);
  const { steps, files } = turnFacts(group);
  const tone = status.className === 'done' ? '' : status.className === 'run' ? 'run' : 'warn';
  return `<div class="t1"><span>第 ${group.index} 轮</span><b class="${tone}">${status.label}</b>${at ? `<span>${at}</span>` : ''}</div>
    <div class="t2">${esc(shortTitle(turnTitle(group), 160))}</div>
    <div class="t3">${steps} 步${files ? ` · ${files} 个文件` : ''}</div>`;
}

function shortTitle(title: string, max = 90): string {
  return title.length > max ? `${title.slice(0, max - 1)}…` : title;
}



export {
  asRecord,
  clockAt,
  clockText,
  groupsFor,
  groupsOf,
  refsFor,
  shortTitle,
  tracePopHtml,
  ttlText,
  turnFacts,
  turnTitle,
};
