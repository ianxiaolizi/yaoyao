import type { ArtifactView, Item, ModelOption, PanelKey, SessionSummary } from './types';
import { argsSummary, dueTime, duration, esc, fmtTokens, mdDoc, mdText, relativeTime, sessionStatusLabel, sessionStatusNote, turnStatus } from './format';
import { artifactLabel, artifactRefs } from './artifacts';
import type { PanelState, State, Store } from './state';

export interface Actions {
  selectSession(id: string): void;
  newSession(): void;
  send(text: string): Promise<void>;
  stop(): Promise<void>;
  decide(approvalId: string, decision: 'once' | 'turn' | 'session' | 'deny'): Promise<void>;
  resume(): Promise<void>;
  openPanel(key: PanelKey, turn?: string): void;
  selectArtifact(locator: string): void;
  exportSession(format: 'md' | 'json'): Promise<void>;
  fork(): Promise<void>;
  archive(archived: boolean): Promise<void>;
  remove(): Promise<void>;
  chooseModel(provider: string, model: string | null): Promise<void>;
  createSchedule(when: string, text: string): Promise<void>;
  cancelSchedule(itemId: string): Promise<void>;
}

interface Group {
  turn: string;
  items: Item[];
  user?: Item;
  end?: Item;
  index: number;
}

const ICON = {
  panel: '<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M6 3v10"/>',
  plus: '<path d="M8 3v10M3 8h10"/>',
  arrow: '<path d="M8 12.5v-9M4.2 7.2 8 3.4l3.8 3.8"/>',
  stop: '<rect x="5" y="5" width="6" height="6" rx="1" fill="currentColor" stroke="none"/>',
  layers: '<path d="M8 2.2 14 5.4 8 8.6 2 5.4 8 2.2Z"/><path d="M2 8.2 8 11.4l6-3.2"/><path d="M2 11 8 14.2 14 11"/>',
  close: '<path d="m4 4 8 8M12 4l-8 8"/>',
};

const svg = (path: string, size = 15): string =>
  `<svg width="${size}" height="${size}" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round">${path}</svg>`;

const approvalLabel: Record<string, string> = {
  pending: '待确认',
  granted: '已允许',
  denied: '已拒绝',
  expired: '已过期',
  cancelled: '已取消',
};

/** The sidebar's default row count; the rest sit behind "全部会话". */
const SESSION_RECENT = 6;

const PANEL_TABS: { key: PanelKey; label: string }[] = [
  { key: 'plan', label: '计划' },
  { key: 'diff', label: '改动' },
  { key: 'why', label: '推理链' },
  { key: 'context', label: '上下文' },
  { key: 'subagents', label: '子代理' },
  { key: 'schedules', label: '定时' },
];

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

function actionHtml(item: Item): string {
  const status = item.status ?? 'completed';
  const content = item.content ?? '';
  return `<details class="y-action" data-key="${esc(item.id)}">
    <summary><span class="stat ${esc(status)}"></span><span class="tool">${esc(item.tool ?? 'tool')}</span><span class="args">${esc(argsSummary(item.arguments))}</span><span class="take">${esc(duration(item.duration_ms))}</span></summary>
    ${content ? `<pre>${esc(content)}</pre>` : ''}
  </details>`;
}

function approvalRecord(item: Item): string {
  const state = item.state ?? 'pending';
  const cls = state === 'granted' ? 'ok' : state === 'denied' || state === 'expired' ? 'bad' : '';
  return `<div class="y-record"><span class="${cls}">${state === 'granted' ? '✓' : state === 'denied' ? '✕' : '·'} ${approvalLabel[state] ?? esc(state)}</span><code>${esc(item.tool ?? '')}</code><span>${esc(item.summary ?? '')}</span></div>`;
}

function bodyHtml(group: Group, includePrompt: boolean): string {
  const parts: string[] = [];
  if (includePrompt) {
    parts.push(`<div class="y-note">${esc(turnTitle(group))}</div>`);
  }
  for (const item of group.items) {
    if (item.kind === 'user') continue;
    if (item.kind === 'thinking') {
      parts.push(`<details class="y-thinking" data-key="${esc(item.id)}"><summary>思考 · ${item.steps ?? 1} 步</summary><div class="body">${esc(item.text ?? '')}</div></details>`);
    } else if (item.kind === 'note') {
      parts.push(`<div class="y-note">${esc(item.text ?? '')}</div>`);
    } else if (item.kind === 'action') {
      parts.push(actionHtml(item));
    } else if (item.kind === 'approval') {
      if (item.state !== 'pending' && item.state !== 'expired') parts.push(approvalRecord(item));
    } else if (item.kind === 'answer') {
      parts.push(`<div class="y-answer">${mdText(item.text ?? '')}</div>`);
    } else if (item.kind === 'turn_end') {
      const status = turnStatus(item);
      const usage = item.usage;
      parts.push(
        `<div class="y-end"><span class="pill ${status.className}"><i></i>${status.label}</span><span>${esc(item.stop_reason ?? '')}</span><span>${usage ? `${fmtTokens(usage.total_tokens ?? (usage.input_tokens ?? 0) + (usage.output_tokens ?? 0))} tokens` : ''}</span></div>`,
      );
    }
  }
  parts.push(
    `<div class="y-why-row"><button class="y-why" type="button" data-why="${esc(group.turn)}">推理链</button></div>`,
  );
  return parts.join('');
}

function currentTurnHtml(group: Group, state: State): string {
  const status = turnStatus(group.end);
  const goal = group.user?.resumed ? '继续上一轮' : group.user?.text ?? '（这一轮没有输入）';
  const usage = group.end?.usage;
  const tokens = usage ? `${fmtTokens(usage.total_tokens ?? (usage.input_tokens ?? 0) + (usage.output_tokens ?? 0))} tokens` : '';
  return `<div class="y-turn" data-turn="${esc(group.turn)}">
    <div class="y-turn-meta"><span>第 ${group.index} 轮</span><span class="pill ${state.running ? 'run' : status.className}"><i></i>${state.running ? '运行中' : status.label}</span><span class="right">${tokens}</span></div>
    <h1 class="y-goal clamp">${esc(goal)}</h1>
    ${bodyHtml(group, false)}
    ${state.live?.text ? `<div class="y-answer y-live">${esc(state.live.text)}<span class="cursor"></span></div>` : ''}
    ${state.live?.reasoning ? `<div class="y-thinking"><div class="body">${esc(state.live.reasoning)}</div></div>` : ''}
  </div>`;
}

function pastRowHtml(group: Group, expanded: boolean): string {
  const status = turnStatus(group.end);
  return `<button class="y-past-row" data-turn-toggle="${esc(group.turn)}" aria-expanded="${expanded}">
    <span class="dot"></span>
    <span class="g">${esc(shortTitle(turnTitle(group)))}</span>
    <span class="res">第 ${group.index} 轮 · ${status.label}</span>
  </button>
    ${expanded ? `<div class="y-past-body">${bodyHtml(group, true)}</div>` : ''}`;
}

function gateOf(state: State): Item | null {
  const rows = [...state.items.values()];
  const pending = rows.find((x) => x.kind === 'approval' && x.state === 'pending');
  if (pending) return pending;
  const groups = groupsFor(state.items);
  const currentTurn = groups[groups.length - 1]?.turn;
  return (
    rows.find(
      (x) =>
        x.kind === 'approval' &&
        x.state === 'expired' &&
        (!x.turn || x.turn === currentTurn),
    ) ?? null
  );
}

function gateHtml(state: State): string {
  const approval = gateOf(state);
  if (!approval) return '';
  const expired = approval.state === 'expired';
  const ttl = ttlText(approval.expires_at, expired);
  const tick = expired
    ? ''
    : ` data-expires="${esc(approval.expires_at ?? '')}"`;
  const summary = approval.summary ?? '';
  if (expired) {
    return `<div class="gate">
      <div class="gate-top"><span class="dot"></span><b>${esc(approval.tool ?? 'tool')}</b><span class="ttl">${ttl}</span></div>
      <div class="gate-cmd">${esc(summary)}</div>
      <div class="gate-act"><button class="btn" type="button" disabled>允许一次</button><button class="btn" type="button" disabled>允许本回合</button><span class="sp"></span><button class="btn primary" type="button" data-resume>继续这一轮</button></div>
    </div>`;
  }
  const id = esc(approval.approval_id ?? '');
  return `<div class="gate">
    <div class="gate-top"><span class="dot"></span><b>${esc(approval.tool ?? 'tool')}</b><span class="ttl"${tick}>${ttl}</span></div>
    <div class="gate-cmd">${esc(summary)}</div>
    <div class="gate-act">
      <button class="btn primary" type="button" data-approval="${id}" data-decision="once">允许一次</button>
      <button class="btn" type="button" data-approval="${id}" data-decision="turn">允许本回合</button>
      <button class="btn" type="button" data-approval="${id}" data-decision="session">允许本会话</button>
      <button class="btn ghost" type="button" data-approval="${id}" data-decision="deny">拒绝</button>
    </div>
  </div>`;
}

function sessionRow(session: SessionSummary, active: boolean): string {
  return `<button class="y-session ${active ? 'on' : ''} ${session.archived ? 'arch' : ''}" data-session="${esc(session.id)}" type="button">
    <span class="t">${esc(session.title || session.id)}</span>
    <span class="m"><span>${esc(relativeTime(session.last_activity_at))}</span><span>·</span><span>${session.steps} 步</span></span>
    ${session.pending_approvals ? '<span class="y-badge">待批</span>' : ''}
    ${session.archived ? '<span class="y-badge arch">已归档</span>' : ''}
  </button>`;
}

const REASON_KINDS: Record<string, string> = {
  situation: '情境',
  reading: '读法',
  stance: '立场',
  action: '动作',
  observation: '观察',
  gate: '闸门',
  verdict: '判定',
  switch: '切换',
};

/** The reasoning chain of one turn, read from `/why`. */
function whyBody(data: Record<string, unknown>): string {
  const nodes = Array.isArray(data.nodes) ? (data.nodes as Record<string, unknown>[]) : [];
  const verdict = asRecord(data.verdict);
  const check = typeof data.check === 'string' ? data.check : '';
  const byId = new Map(nodes.filter((node) => typeof node.id === 'string').map((node) => [node.id as string, node]));
  const depthOf = (node: Record<string, unknown>): number => {
    let depth = 0;
    let cursor = node;
    const seen = new Set<string>();
    while (cursor && depth < 3) {
      const basis = Array.isArray(cursor.basis) ? (cursor.basis as Record<string, unknown>[]) : [];
      const next = basis.find((entry) => typeof entry.node === 'string' && byId.has(entry.node));
      const id = typeof next?.node === 'string' ? next.node : '';
      if (!id || seen.has(id)) break;
      seen.add(id);
      cursor = byId.get(id) as Record<string, unknown>;
      depth += 1;
    }
    return depth;
  };
  const out: string[] = [];
  if (typeof verdict.claim === 'string' && verdict.claim) {
    out.push(`<div class="y-why-verdict"><div class="l">判定</div><div class="tx">${mdText(verdict.claim)}</div><div class="rule">${esc(String(verdict.rule ?? ''))}</div></div>`);
  }
  for (const node of nodes) {
    const depth = depthOf(node);
    const basis = (Array.isArray(node.basis) ? (node.basis as Record<string, unknown>[]) : [])
      .map((entry) => (typeof entry.fact === 'string' ? entry.fact : typeof entry.event === 'string' ? entry.event : typeof entry.node === 'string' ? `节点 ${entry.node}` : ''))
      .filter(Boolean)
      .join(' · ');
    const kind = String(node.kind ?? '');
    out.push(`<div class="y-node ${depth === 1 ? 'l2' : depth >= 2 ? 'l3' : ''}">
      <span class="g">${depth >= 2 ? '└' : '├'}</span>
      <div><div class="r1"><span class="kind ${kind === 'verdict' ? 'v' : ''}">${esc(REASON_KINDS[kind] ?? kind)}</span>${node.step ? `<span class="step">第 ${String(node.step)} 步</span>` : ''}</div>
      <div class="tx">${esc(String(node.claim ?? ''))}</div>
      ${basis ? `<div class="ct">${esc(basis.slice(0, 160))}</div>` : typeof node.rule === 'string' && node.rule ? `<div class="ct">${esc(node.rule)}</div>` : ''}</div>
    </div>`);
  }
  if (!out.length) out.push('<div class="y-panel-empty">这一轮没有可读的节点。</div>');
  out.push('<div class="y-panel-note">从判定往下读：节点指向它据以成立的原话、观察或规则。链不进模型上下文，倒读不花 token。</div>');
  if (check && check !== 'ok') out.push(`<div class="y-panel-note bad">链的自检没有全过：${esc(check)}</div>`);
  return out.join('');
}

function panelBody(panel: PanelState): string {
  if (panel.loading) return '<div class="y-panel-empty">读取中…</div>';
  if (panel.error) {
    // `/why` says "no reasoning chain" in the API's words; a reader needs the
    // page's words plus the fact that it is not a bug.
    const empty = panel.key === 'why' && /reasoning chain|no turn/i.test(panel.error);
    if (empty) {
      return `<div class="y-panel-empty"><b>这一轮还没有推理链</b><span>旧日志或未写链的运行会没有，这是正常的。接口说：${esc(panel.error)}</span></div>`;
    }
    return `<div class="y-panel-empty"><b>${esc(panel.error)}</b><span>这是投影的如实回答，不是页面编的。</span></div>`;
  }
  const data = asRecord(panel.data);
  if (panel.key === 'why') return whyBody(data);
  if (panel.key === 'plan') {
    const plan = asRecord(data.plan);
    const goal = typeof plan.goal === 'string' ? plan.goal : '';
    const items = Array.isArray(plan.items) ? (plan.items as Record<string, unknown>[]) : [];
    const facts = Array.isArray(plan.facts) ? (plan.facts as unknown[]) : [];
    const status: Record<string, string> = { done: '完成', doing: '进行中', todo: '待执行', failed: '失败' };
    return `<div class="sh-h">目标</div><div class="y-panel-goal">${esc(goal)}</div>
      <div class="sh-h">步骤 · ${items.filter((x) => x.status === 'done').length} / ${items.length}</div>
      ${items.map((item, index) => `<div class="sh-step"><span class="k">${String(index + 1).padStart(2, '0')}</span><span class="tx">${esc(item.text)}</span><span class="st">${status[String(item.status)] ?? esc(item.status)}</span></div>`).join('')}
      ${facts.length ? `<div class="sh-h">事实</div>${facts.map((fact) => `<div class="y-fact">${esc(fact)}</div>`).join('')}` : ''}`;
  }
  if (panel.key === 'diff') {
    // No repository to observe: the panel falls back to the workspace change
    // ledger, which reports paths, kinds and sizes — never a line patch.
    if (data.source === 'workspace.changes') {
      const changes = Array.isArray(data.changes) ? (data.changes as Record<string, unknown>[]) : [];
      const incomplete =
        data.complete === false
          ? '<div class="y-diff-warning">改动清单不完整：快照比较未能读取全部路径，不能据此认为工作区只有这些改动。</div>'
          : '';
      if (!changes.length) {
        return `<div class="y-panel-empty"><b>没有可显示的改动</b><span>最近一次工作区快照比较没有发现改动。</span></div>${incomplete}`;
      }
      const label: Record<string, string> = { added: '新增', modified: '修改', removed: '删除' };
      return `${incomplete}<div class="sh-h">改动 · ${changes.length} 个文件</div>
        <div class="y-fact">来自工作区变更账本：记录路径、大小与摘要，没有逐行补丁。</div>
        ${changes
          .map((change) => {
            const before = Number(change.before_size ?? 0);
            const after = Number(change.after_size ?? 0);
            const delta = after - before;
            const size = `${delta >= 0 ? '+' : '−'}${Math.abs(delta)} B`;
            return `<div class="sh-file"><span class="n">${esc(change.file)}</span><span><span class="st">${label[String(change.change)] ?? esc(change.change)}</span> <span class="${delta >= 0 ? 'a' : 'd'}">${size}</span></span></div>`;
          })
          .join('')}`;
    }

    const diff = asRecord(data.diff);
    const files = Array.isArray(diff.files) ? (diff.files as Record<string, unknown>[]) : [];
    if (!files.length) return '<div class="y-panel-empty"><b>没有可显示的改动</b><span>这一轮没有 git.diff 观察。</span></div>';
    return `<div class="sh-h">改动 · ${files.length} 个文件</div>
      ${files.map((file) => `<div class="sh-file"><span class="n">${esc(file.file)}</span><span><span class="a">+${Number(file.additions ?? 0)}</span> <span class="d">−${Number(file.deletions ?? 0)}</span></span></div>`).join('')}
      ${files
        .map((file) => {
          const patch = typeof file.patch === 'string' ? file.patch : '';
          if (!patch.trim()) return '';
          return `<div class="diff">${patch
            .split('\n')
            .map((line) => `<span class="${line.startsWith('+') ? 'a' : line.startsWith('-') ? 'd' : ''}">${esc(line)}</span>`)
            .join('\n')}</div>`;
        })
        .join('')}`;
  }
  if (panel.key === 'context') {
    const categories = Array.isArray(data.categories) ? (data.categories as Record<string, unknown>[]) : [];
    const layers = asRecord(data.layers);
    const used = Number(data.used ?? 0);
    const budget = asRecord(data.budget);
    const max = Number(budget.max_input_tokens ?? 0);
    return `<div class="sh-h">已用 · ${fmtTokens(used)}${max ? ` / ${fmtTokens(max)}` : ''}</div>
      ${categories.map((cat) => `<div class="ctx-row"><span>${esc(cat.name)}</span><span>${fmtTokens(cat.tokens)}</span></div><div class="ctx-bar"><i style="width:${max ? Math.min(100, (Number(cat.tokens ?? 0) / max) * 100) : 0}%"></i></div>`).join('')}
      <div class="sh-h">最近一次请求</div>
      ${['state', 'task', 'messages', 'tools'].map((key) => {
        const layer = asRecord(layers[key]);
        const preview = typeof layer.preview === 'string' ? layer.preview : '';
        const chars = Number(layer.chars ?? 0);
        return preview ? `<div class="y-fact"><b>${key}</b><span>${esc(preview.slice(0, 240))}</span><em>${fmtTokens(chars)} chars</em></div>` : '';
      }).join('')}`;
  }
  if (panel.key === 'schedules') {
    const items = Array.isArray(data.items) ? (data.items as Record<string, unknown>[]) : [];
    return `<div class="sh-h">定时 · ${items.length}</div>
      <div class="y-sched-form"><input id="schedWhen" placeholder="+10m 或 RFC3339" aria-label="什么时候运行" /><input id="schedText" placeholder="到点要说什么" aria-label="定时内容" /><button class="btn" type="button" data-schedule-add>排定</button></div>
      ${items.length ? items.map((item) => `<div class="y-sched-row"><div><div class="y-sched-when">${esc(dueTime(String(item.due_at ?? '')))}${item.overdue ? ' · 已过期' : ''}</div><div>${esc(item.text)}</div></div><button class="btn ghost" type="button" data-schedule-cancel="${esc(item.item_id)}">取消</button></div>`).join('') : '<div class="y-panel-empty"><b>没有待运行的定时</b><span>运行仍由 tick / serve 驱动。</span></div>'}
      <div class="y-panel-note">定时条目在 Inbox 里，和 agent schedule 是同一件；Web 只负责排队和撤回。</div>`;
  }
  const children = Array.isArray(data.children) ? (data.children as Record<string, unknown>[]) : [];
  if (!children.length) return '<div class="y-panel-empty"><b>没有子代理</b><span>模型没有委派过子任务。</span></div>';
  return `<div class="sh-h">子代理 · ${children.length}</div>${subagentNodes(children, 0)}`;
}

function subagentNodes(children: Record<string, unknown>[], depth: number): string {
  return children
    .map((child) => {
      const outcome = asRecord(child.outcome);
      const answer = typeof outcome.answer === 'string' ? outcome.answer : '';
      const nested = Array.isArray(child.children) ? (child.children as Record<string, unknown>[]) : [];
      return `<div class="y-sub" style="padding-left:${depth * 14}px"><div class="sh-node"><b>${esc(child.provider)} · ${esc(child.status)}</b>${esc(child.objective)}${answer ? `<br><span style="color:var(--ink-3)">${esc(answer)}</span>` : ''}</div>${nested.length ? subagentNodes(nested, depth + 1) : ''}</div>`;
    })
    .join('');
}

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

export function mount(root: HTMLElement, store: Store, actions: Actions): void {
  root.innerHTML = `<div class="y-app">
    <aside class="y-side" id="side">
      <div class="y-side-head"><span class="mark">爻</span><button class="ibtn y-side-close" id="sideClose" type="button" aria-label="收起会话栏">${svg(ICON.close)}</button></div>
      <button class="y-new" id="newTask" type="button"><span class="plus">＋</span>新任务</button>
      <div class="y-side-scroll"><div class="y-sec">会话<span class="n" id="sessionCount"></span></div><div id="sessionList"></div></div>
      <div class="y-side-foot"><div class="y-foot-row">设置</div></div>
      <div class="y-rz y-rz-side" id="rzSide" role="separator" tabindex="0" aria-orientation="vertical" aria-label="调整会话栏宽度" title="拖动调整；双击复位"></div>
    </aside>
    <div class="y-side-mask" id="sideMask"></div>
    <main class="y-main">
      <header class="y-top">
        <button class="ibtn" id="toggleSide" type="button" aria-label="会话栏">${svg(ICON.panel, 16)}</button>
        <span class="y-title" id="title">爻爻</span>
        <span class="y-sep">/</span>
        <span class="y-round" id="round">第 0 轮</span>
        <span id="statusPill"></span>
        <span class="sp"></span>
        <button class="btn" id="details" type="button" aria-expanded="false">详情</button>
        <button class="ibtn" id="more" type="button" aria-label="更多">⋯</button>
      </header>
      <div class="y-status-note" id="statusNote" hidden></div>
      <div class="y-evpop" id="evpop" hidden>
        <div class="pop-h">本轮详情<span class="sp"></span><span class="esc">Esc</span></div>
        ${PANEL_TABS.map((tab) => `<button class="pop-item" type="button" data-evpanel="${tab.key}">${tab.label}<span class="v">›</span></button>`).join('')}
      </div>
      <div class="y-menu" id="menu" hidden>
        <button class="pop-item" type="button" data-menu="export-md">导出 Markdown</button>
        <button class="pop-item" type="button" data-menu="export-json">导出 JSON</button>
        <button class="pop-item" type="button" data-menu="fork">分叉这一轮</button>
        <button class="pop-item" type="button" data-menu="palette">命令面板 <span class="v">⌘K</span></button>
        <div class="pop-sep"></div>
        <button class="pop-item" type="button" data-menu="archive">归档此会话</button>
        <button class="pop-item danger" type="button" data-menu="remove">删除此会话</button>
      </div>
      <div class="y-stage">
        <div class="y-trace"><div class="y-ticks" id="trace"></div><div class="trace-pop" id="tracePop" role="tooltip" aria-hidden="true"></div></div>
        <div class="y-scroll" id="scroll"><div class="y-col" id="transcript"></div></div>
      </div>
      <aside class="y-sheet" id="sheet" aria-hidden="true" aria-label="本轮详情">
        <div class="y-rz y-rz-sheet" id="rzSheet" role="separator" tabindex="0" aria-orientation="vertical" aria-label="调整详情宽度" title="拖动调整；双击复位"></div>
        <div class="y-sheet-head"><span id="sheetTitle">本轮详情</span><span class="sp"></span><button class="ibtn" id="sheetClose" type="button" aria-label="关闭">${svg(ICON.close)}</button></div>
        <div class="y-sheet-tabs" role="tablist">
          ${PANEL_TABS.map((tab) => `<button type="button" role="tab" data-panel-tab="${tab.key}" aria-selected="false">${tab.label}</button>`).join('')}
        </div>
        <div class="y-sheet-body" id="sheetBody"></div>
      </aside>
      <div class="y-dock"><div class="y-dock-in">
        <section class="y-result" id="result" hidden>
          <div class="y-result-head" id="resultHead">
            <span class="y-result-t">本轮成果</span>
            <span class="y-result-n" id="resultCount"></span>
            <span class="sp"></span>
            <button class="y-result-more" id="resultFull" type="button" aria-label="全屏" aria-pressed="false">⤢</button>
            <button class="y-result-more" id="resultMore" type="button" aria-expanded="false">展开</button>
          </div>
          <div class="y-result-body">
            <div class="y-preview" id="artifactPreview"></div>
            <aside class="y-artifacts" id="artifactList"></aside>
          </div>
        </section>
        <div class="y-gate" id="gate"></div>
        <div class="y-composer">
          <div class="y-model-menu" id="modelMenu" role="menu" aria-label="选择模型" hidden></div>
          <div class="y-model-menu" id="policyMenu" role="menu" aria-label="审批策略" hidden></div>
          <div class="y-env-note" id="envNote" hidden></div>
          <textarea id="input" rows="1" placeholder="继续补充要求，或输入 / 命令…"></textarea>
          <div class="y-comp-row">
            <button class="y-chip" type="button" id="attach" aria-label="附件上传（v1.1）" title="附件上传留在 v1.1" disabled>${svg(ICON.plus, 13)}</button>
            <button class="y-chip" type="button" id="modelChip" aria-haspopup="menu" aria-expanded="false">模型</button>
            <button class="y-chip" type="button" id="policyChip" aria-haspopup="menu" aria-expanded="false" title="审批是逐个动作的门">审批：逐次</button>
            <span class="sp"></span>
            <button class="y-stop" id="stop" type="button" aria-label="停止" hidden>${svg(ICON.stop, 12)}</button>
            <button class="y-send" id="send" type="button" aria-label="发送">${svg(ICON.arrow, 14)}</button>
          </div>
        </div>
      </div></div>
    </main>
    <div class="y-toast" id="toast" role="status" hidden></div>
    <div class="y-palette" id="palette" hidden>
      <div class="y-palette-box" role="dialog" aria-label="命令面板">
        <div class="y-palette-input">${svg('<circle cx="7" cy="7" r="4.2"/><path d="m10.2 10.2 3.3 3.3"/>')}<input id="paletteInput" placeholder="搜索会话、动作、面板…" autocomplete="off" /><span class="kbd">Esc</span></div>
        <div class="y-palette-list" id="paletteList"></div>
        <div class="y-palette-foot">↑↓ 选择 · ↵ 打开 · Esc 关闭</div>
      </div>
    </div>
    <div class="y-auth" id="auth" hidden></div>
  </div>`;

  const $ = <T extends HTMLElement>(selector: string): T => root.querySelector(selector) as T;
  const app = $('.y-app');
  const transcript = $('#transcript');
  const scroll = $('#scroll');
  const gate = $('#gate');
  const toast = $('#toast');
  const input = $<HTMLTextAreaElement>('#input');
  const send = $<HTMLButtonElement>('#send');
  const stop = $<HTMLButtonElement>('#stop');
  const title = $('#title');
  const round = $('#round');
  const statusPill = $('#statusPill');
  const statusNote = $('#statusNote');
  const envNote = $('#envNote');
  const sessionList = $('#sessionList');
  const sessionCount = $('#sessionCount');
  const trace = $('#trace');
  const tracePop = $('#tracePop');
  const modelChip = $('#modelChip');
  const modelMenu = $('#modelMenu');
  const policyChip = $('#policyChip');
  const policyMenu = $('#policyMenu');
  const rzSide = $('#rzSide');
  const rzSheet = $('#rzSheet');
  const details = $('#details');
  const more = $('#more');
  const menu = $('#menu');
  const evpop = $('#evpop');
  const palette = $('#palette');
  const paletteInput = $<HTMLInputElement>('#paletteInput');
  const paletteList = $('#paletteList');
  const sideMask = $('#sideMask');
  const sheet = $('#sheet');
  const sheetClose = $('#sheetClose');
  const sheetBody = $('#sheetBody');
  const sheetTitle = $('#sheetTitle');
  const resultEl = $('#result');
  const resultHead = $('#resultHead');
  const resultMore = $('#resultMore');
  const resultFull = $('#resultFull');
  const resultCount = $('#resultCount');
  const artifactPreview = $('#artifactPreview');
  const artifactList = $('#artifactList');
  let panelTrigger: HTMLElement | null = null;
  let panelSignature = '';
  let resultSignature = '';
  let sidebarSignature = '';
  let traceSignature = '';
  let gateSignature = '';
  let lastActive: string | null = null;
  let lastGroups = 0;
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  // ── layout: the two widths a reader can drag ───────────────────────────
  interface WidthRule {
    variable: '--sidebar-w' | '--sheet-w';
    store: string;
    min: number;
    max: number;
    fallback: number;
    /** 1 when dragging right widens it; -1 when it grows to the left. */
    direction: 1 | -1;
  }

  function bindWidth(handle: HTMLElement, rule: WidthRule): void {
    // Storage is a convenience, not a requirement: a reader in a private window
    // still gets the width for this visit instead of a dead page.
    const read = (key: string): string | null => {
      try {
        return localStorage.getItem(key);
      } catch {
        return null;
      }
    };
    const write = (key: string, value: string | null): void => {
      try {
        if (value == null) localStorage.removeItem(key);
        else localStorage.setItem(key, value);
      } catch {
        // The width still applies through the CSS variable.
      }
    };
    const target = (): HTMLElement | null => handle.parentElement;
    const current = (): number =>
      Math.round(target()?.getBoundingClientRect().width || rule.fallback);
    const apply = (value: number | null): void => {
      if (value == null) {
        root.style.removeProperty(rule.variable);
        write(rule.store, null);
      } else {
        const width = Math.round(Math.min(rule.max, Math.max(rule.min, value)));
        root.style.setProperty(rule.variable, `${width}px`);
        write(rule.store, String(width));
      }
      handle.setAttribute('aria-valuenow', String(current()));
    };
    const stored = Number(read(rule.store));
    if (Number.isFinite(stored) && stored >= rule.min && stored <= rule.max) apply(stored);
    handle.setAttribute('aria-valuemin', String(rule.min));
    handle.setAttribute('aria-valuemax', String(rule.max));
    handle.setAttribute('aria-valuenow', String(current()));

    handle.addEventListener('dblclick', () => apply(null));
    handle.addEventListener('keydown', (event) => {
      if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
      event.preventDefault();
      if (event.key === 'Home') return apply(null);
      if (event.key === 'End') return apply(rule.max);
      const step = event.key === 'ArrowRight' ? 8 : -8;
      apply(current() + step * rule.direction);
    });
    handle.addEventListener('pointerdown', (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      const startWidth = current();
      const startX = event.clientX;
      handle.setPointerCapture(event.pointerId);
      document.body.classList.add('y-resizing');
      const move = (moveEvent: PointerEvent): void =>
        apply(startWidth + (moveEvent.clientX - startX) * rule.direction);
      const end = (): void => {
        document.body.classList.remove('y-resizing');
        handle.removeEventListener('pointermove', move);
        handle.removeEventListener('pointerup', end);
        handle.removeEventListener('pointercancel', end);
      };
      handle.addEventListener('pointermove', move);
      handle.addEventListener('pointerup', end);
      handle.addEventListener('pointercancel', end);
    });
  }

  bindWidth(rzSide, { variable: '--sidebar-w', store: 'yaoyao:layout:side', min: 196, max: 340, fallback: 236, direction: 1 });
  bindWidth(rzSheet, { variable: '--sheet-w', store: 'yaoyao:layout:sheet', min: 320, max: 560, fallback: 420, direction: -1 });

  // ── the model chip ─────────────────────────────────────────────────────
  function modelChoice(state: State): { provider: string; model?: string | null } | null {
    return state.summary?.model ?? state.chosenModel ?? null;
  }

  function modelLabel(state: State): string {
    const choice = modelChoice(state);
    if (choice) {
      const option = (state.meta?.models ?? []).find(
        (row) => row.provider === choice.provider && (row.model ?? '') === (choice.model ?? ''),
      );
      return option?.label ?? (choice.model || choice.provider);
    }
    return state.meta?.model || '模型';
  }

  function renderModelMenu(state: State): void {
    const options: ModelOption[] = state.meta?.models ?? [];
    const choice = modelChoice(state);
    if (!options.length) {
      modelMenu.innerHTML = '<div class="y-panel-empty">这个进程没有可切换的模型。</div>';
      return;
    }
    modelMenu.innerHTML =
      '<div class="pop-h">下一轮用哪个模型</div>' +
      options
        .map((option) => {
          const chosen = choice != null && choice.provider === option.provider && (choice.model ?? '') === (option.model ?? '');
          return `<button class="pop-item" type="button" role="menuitem" data-provider="${esc(option.provider)}" data-model="${esc(option.model ?? '')}">${chosen ? '✓ ' : ''}${esc(option.label)}</button>`;
        })
        .join('');
  }

  function closeModelMenu(): void {
    modelMenu.hidden = true;
    modelChip.setAttribute('aria-expanded', 'false');
  }

  modelChip.addEventListener('click', (event) => {
    event.stopPropagation();
    closePolicyMenu();
    if (!modelMenu.hidden) return closeModelMenu();
    renderModelMenu(store.get());
    modelMenu.hidden = false;
    modelChip.setAttribute('aria-expanded', 'true');
  });
  modelMenu.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-provider]');
    if (!button?.dataset.provider) return;
    closeModelMenu();
    void actions.chooseModel(button.dataset.provider, button.dataset.model || null);
  });

  // ── the approval policy chip ───────────────────────────────────────────
  // This is deliberately a statement, not a switch. Approving is the moment a
  // person vouches for one exact action; an "auto approve" mode would remove
  // the person from that moment, which the permission work forbids.
  function closePolicyMenu(): void {
    policyMenu.hidden = true;
    policyChip.setAttribute('aria-expanded', 'false');
  }

  policyChip.addEventListener('click', (event) => {
    event.stopPropagation();
    closeModelMenu();
    if (!policyMenu.hidden) return closePolicyMenu();
    policyMenu.innerHTML =
      '<div class="pop-h">审批策略</div>' +
      '<button class="pop-item" type="button" role="menuitem" disabled>✓ 逐次确认</button>' +
      '<button class="pop-item" type="button" role="menuitem" disabled>自动批准（本版不提供）</button>' +
      '<div class="pop-note">审批是门：每个动作单独确认，高风险动作只允许一次。自动批准会绕过「有人在看」，所以不提供。</div>';
    policyMenu.hidden = false;
    policyChip.setAttribute('aria-expanded', 'true');
  });

  // ── the two-step delete ────────────────────────────────────────────────
  let removeTimer: ReturnType<typeof setTimeout> | undefined;
  function armRemove(button: HTMLElement): void {
    if (button.dataset.armed === '1') {
      clearTimeout(removeTimer);
      button.dataset.armed = '';
      button.textContent = '删除此会话';
      menu.hidden = true;
      void actions.remove();
      return;
    }
    button.dataset.armed = '1';
    button.textContent = '再点一次删除';
    clearTimeout(removeTimer);
    removeTimer = setTimeout(() => {
      button.dataset.armed = '';
      button.textContent = '删除此会话';
    }, 3000);
  }

  const resize = (): void => {
    input.style.height = 'auto';
    input.style.height = `${Math.min(160, input.scrollHeight)}px`;
  };

  input.addEventListener('input', () => {
    resize();
    send.disabled = !input.value.trim() || !stateHasToken() || Boolean(gateOf(store.get()));
  });
  input.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  });

  async function submit(): Promise<void> {
    const text = input.value.trim();
    if (!text) return;
    if (store.get().waiting || gateOf(store.get())) {
      store.dispatch({ type: 'toast', toast: '先处理上面的审批；这一轮会从那里继续' });
      return;
    }
    input.value = '';
    resize();
    send.disabled = true;
    await actions.send(text);
  }

  send.addEventListener('click', () => void submit());
  stop.addEventListener('click', () => void actions.stop());

  $('#toggleSide').addEventListener('click', () => {
    store.dispatch({ type: 'sidebar/toggle' });
  });
  $('#sideClose').addEventListener('click', () => store.dispatch({ type: 'sidebar/toggle' }));
  $('#newTask').addEventListener('click', () => actions.newSession());

  const lastTurn = (): string | undefined => {
    const groups = groupsFor(store.get().items);
    return groups[groups.length - 1]?.turn;
  };

  details.addEventListener('click', () => {
    evpop.hidden = true;
    if (store.get().panel) {
      store.dispatch({ type: 'panel/close' });
      return;
    }
    panelTrigger = details;
    actions.openPanel('plan');
  });
  sheetClose.addEventListener('click', () => store.dispatch({ type: 'panel/close' }));
  sheet.addEventListener('click', (event) => {
    const tab = (event.target as HTMLElement).closest<HTMLElement>('[data-panel-tab]');
    if (tab?.dataset.panelTab) {
      panelTrigger = details;
      const key = tab.dataset.panelTab as PanelKey;
      actions.openPanel(key, key === 'why' ? lastTurn() : undefined);
      return;
    }
    const add = (event.target as HTMLElement).closest<HTMLElement>('[data-schedule-add]');
    if (add) {
      const when = (sheetBody.querySelector('#schedWhen') as HTMLInputElement | null)?.value.trim() ?? '';
      const text = (sheetBody.querySelector('#schedText') as HTMLInputElement | null)?.value.trim() ?? '';
      if (!when || !text) {
        store.dispatch({ type: 'toast', toast: '时间和内容都要填' });
        return;
      }
      void actions.createSchedule(when, text);
      return;
    }
    const cancel = (event.target as HTMLElement).closest<HTMLElement>('[data-schedule-cancel]');
    if (cancel?.dataset.scheduleCancel) void actions.cancelSchedule(cancel.dataset.scheduleCancel);
  });

  let evTimer: ReturnType<typeof setTimeout> | undefined;
  const showEv = (): void => {
    if (store.get().panel) return;
    clearTimeout(evTimer);
    evpop.hidden = false;
  };
  const hideEv = (): void => {
    clearTimeout(evTimer);
    evTimer = setTimeout(() => {
      evpop.hidden = true;
    }, 140);
  };
  details.addEventListener('mouseenter', showEv);
  details.addEventListener('focus', showEv);
  details.addEventListener('mouseleave', hideEv);
  details.addEventListener('blur', hideEv);
  evpop.addEventListener('mouseenter', () => clearTimeout(evTimer));
  evpop.addEventListener('mouseleave', hideEv);
  evpop.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-evpanel]');
    if (!button?.dataset.evpanel) return;
    evpop.hidden = true;
    const key = button.dataset.evpanel as PanelKey;
    actions.openPanel(key, key === 'why' ? lastTurn() : undefined);
  });

  more.addEventListener('click', (event) => {
    event.stopPropagation();
    evpop.hidden = true;
    menu.hidden = !menu.hidden;
  });
  document.addEventListener('click', (event) => {
    if (!menu.hidden && !menu.contains(event.target as Node) && !more.contains(event.target as Node)) menu.hidden = true;
    if (!evpop.hidden && !evpop.contains(event.target as Node) && !details.contains(event.target as Node)) {
      evpop.hidden = true;
    }
    if (!modelMenu.hidden && !modelMenu.contains(event.target as Node) && !modelChip.contains(event.target as Node)) {
      closeModelMenu();
    }
    if (!policyMenu.hidden && !policyMenu.contains(event.target as Node) && !policyChip.contains(event.target as Node)) {
      closePolicyMenu();
    }
  });
  menu.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-menu]');
    if (!button?.dataset.menu) return;
    const action = button.dataset.menu;
    // Deleting asks twice, so the menu has to survive the first click: closing
    // it would hide the only button that can confirm the removal.
    if (action === 'remove') {
      armRemove(button);
      return;
    }
    menu.hidden = true;
    if (action === 'export-md') void actions.exportSession('md');
    else if (action === 'export-json') void actions.exportSession('json');
    else if (action === 'fork') void actions.fork();
    else if (action === 'palette') openPalette();
    else if (action === 'archive') void actions.archive(!store.get().summary?.archived);
  });

  sideMask.addEventListener('click', () => store.dispatch({ type: 'sidebar/toggle' }));

  const narrow = window.matchMedia('(max-width: 1080px)');
  narrow.addEventListener('change', (event) => {
    if (event.matches && store.get().sidebar) store.dispatch({ type: 'sidebar/toggle' });
    if (!event.matches && !store.get().sidebar) store.dispatch({ type: 'sidebar/toggle' });
  });
  resultHead.addEventListener('click', (event) => {
    if ((event.target as HTMLElement).closest('button')) return;
    store.dispatch({ type: 'result/toggle' });
  });
  resultMore.addEventListener('click', (event) => {
    event.stopPropagation();
    store.dispatch({ type: 'result/toggle' });
  });
  resultFull.addEventListener('click', (event) => {
    event.stopPropagation();
    store.dispatch({ type: 'result/full' });
  });
  artifactList.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-artifact-locator]');
    if (button?.dataset.artifactLocator) actions.selectArtifact(button.dataset.artifactLocator);
  });

  sessionList.addEventListener('click', (event) => {
    const all = (event.target as HTMLElement).closest<HTMLElement>('[data-session-all]');
    if (all) {
      store.dispatch({ type: 'sessions/all' });
      return;
    }
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-session]');
    if (button?.dataset.session) actions.selectSession(button.dataset.session);
  });

  transcript.addEventListener('click', (event) => {
    const goal = (event.target as HTMLElement).closest<HTMLElement>('.y-goal');
    if (goal) {
      goal.classList.toggle('clamp');
      return;
    }
    const more = (event.target as HTMLElement).closest<HTMLElement>('[data-past-toggle]');
    if (more) {
      store.dispatch({ type: 'past/toggle' });
      return;
    }
    const toggle = (event.target as HTMLElement).closest<HTMLElement>('[data-turn-toggle]');
    if (toggle?.dataset.turnToggle) store.dispatch({ type: 'turn/toggle', turn: toggle.dataset.turnToggle });
    const why = (event.target as HTMLElement).closest<HTMLElement>('[data-why]');
    if (why?.dataset.why) {
      panelTrigger = why;
      actions.openPanel('why', why.dataset.why);
    }
  });

  gate.addEventListener('click', (event) => {
    const resume = (event.target as HTMLElement).closest<HTMLElement>('[data-resume]');
    if (resume) {
      gate.querySelectorAll<HTMLButtonElement>('button').forEach((button) => (button.disabled = true));
      void actions.resume();
      return;
    }
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-approval]');
    if (button?.dataset.approval && button.dataset.decision) {
      gate.querySelectorAll<HTMLButtonElement>('button').forEach((item) => (item.disabled = true));
      void actions.decide(button.dataset.approval, button.dataset.decision as 'once' | 'turn' | 'session' | 'deny');
    }
  });

  trace.addEventListener('click', (event) => {
    const tick = (event.target as HTMLElement).closest<HTMLElement>('[data-jump]');
    if (!tick?.dataset.jump) return;
    const target = transcript.querySelector<HTMLElement>(`[data-turn="${CSS.escape(tick.dataset.jump)}"]`);
    target?.scrollIntoView({ block: 'start', behavior: 'smooth' });
  });

  // The rail is a scrubber: hover/focus previews one turn in place, Alt↑↓
  // moves the preview, Enter (a click) jumps. Nothing here changes layout.
  function showTracePop(tick: HTMLElement, group: Group): void {
    tracePop.innerHTML = tracePopHtml(group);
    tracePop.classList.add('is-on');
    tracePop.setAttribute('aria-hidden', 'false');
    tracePop.dataset.turn = group.turn;
    const height = tracePop.offsetHeight;
    const room = trace.parentElement?.clientHeight ?? 0;
    const top = Math.max(4, Math.min(tick.offsetTop - 10, room - height - 8));
    tracePop.style.top = `${top}px`;
  }

  function hideTracePop(): void {
    tracePop.classList.remove('is-on');
    tracePop.setAttribute('aria-hidden', 'true');
    delete tracePop.dataset.turn;
  }

  function traceTickAt(node: EventTarget | null): HTMLElement | null {
    return node instanceof HTMLElement ? node.closest<HTMLElement>('[data-jump]') : null;
  }

  function traceGroup(turn: string): Group | undefined {
    return groupsFor(store.get().items).find((group) => group.turn === turn);
  }

  trace.addEventListener('mouseover', (event) => {
    const tick = traceTickAt(event.target);
    const jump = tick?.dataset.jump;
    const group = jump ? traceGroup(jump) : undefined;
    if (tick && group) showTracePop(tick, group);
  });
  trace.addEventListener('mouseleave', hideTracePop);
  trace.addEventListener('focusin', (event) => {
    const tick = traceTickAt(event.target);
    const jump = tick?.dataset.jump;
    const group = jump ? traceGroup(jump) : undefined;
    if (tick && group) showTracePop(tick, group);
  });
  trace.addEventListener('focusout', (event) => {
    if (!trace.contains(event.relatedTarget as Node | null)) hideTracePop();
  });

  document.addEventListener('keydown', (event) => {
    if (!event.altKey || (event.key !== 'ArrowUp' && event.key !== 'ArrowDown')) return;
    const target = event.target as HTMLElement | null;
    if (target?.closest('input, textarea, select, [contenteditable="true"]')) return;
    const ticks = [...trace.querySelectorAll<HTMLElement>('.y-tick')];
    if (!ticks.length) return;
    event.preventDefault();
    const focused = document.activeElement instanceof HTMLElement ? ticks.indexOf(document.activeElement as HTMLElement) : -1;
    const previewed = tracePop.dataset.turn ? ticks.findIndex((tick) => tick.dataset.jump === tracePop.dataset.turn) : -1;
    let index = focused >= 0 ? focused : previewed >= 0 ? previewed : ticks.length - 1;
    index = Math.max(0, Math.min(ticks.length - 1, index + (event.key === 'ArrowDown' ? 1 : -1)));
    const tick = ticks[index];
    tick.focus({ preventScroll: true });
    const group = traceGroup(tick.dataset.jump ?? '');
    if (group) showTracePop(tick, group);
  });

  document.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    if (!palette.hidden) {
      event.preventDefault();
      closePalette();
    } else if (!menu.hidden) {
      event.preventDefault();
      menu.hidden = true;
    } else if (!modelMenu.hidden) {
      event.preventDefault();
      closeModelMenu();
    } else if (!policyMenu.hidden) {
      event.preventDefault();
      closePolicyMenu();
    } else if (!evpop.hidden) {
      event.preventDefault();
      evpop.hidden = true;
    } else if (store.get().panel) {
      event.preventDefault();
      store.dispatch({ type: 'panel/close' });
    } else if (store.get().resultFull) {
      event.preventDefault();
      store.dispatch({ type: 'result/full' });
    } else if (store.get().result) {
      event.preventDefault();
      store.dispatch({ type: 'result/close' });
    } else if (!store.get().sidebar) {
      store.dispatch({ type: 'sidebar/toggle' });
    }
  });

  function stateHasToken(): boolean {
    return location.hash.startsWith('#t=');
  }

  interface Command {
    label: string;
    hint?: string;
    group: string;
    run: () => void | Promise<void>;
  }

  let paletteCommands: Command[] = [];
  let paletteIndex = 0;
  let paletteTrigger: HTMLElement | null = null;

  function buildCommands(query: string): Command[] {
    const state = store.get();
    const commands: Command[] = [
      { group: '动作', label: '新任务', hint: '从一句话开始', run: () => actions.newSession() },
      { group: '动作', label: '导出 Markdown', hint: '当前会话', run: () => actions.exportSession('md') },
      { group: '动作', label: '导出 JSON', hint: '当前会话', run: () => actions.exportSession('json') },
      { group: '动作', label: '分叉这一轮', run: () => actions.fork() },
      ...PANEL_TABS.map((tab) => ({
        group: '面板',
        label: `打开${tab.label}`,
        run: () => actions.openPanel(tab.key),
      })),
      ...state.sessions.slice(0, 8).map((row) => ({
        group: '会话',
        label: row.title || row.id,
        hint: relativeTime(row.last_activity_at),
        run: () => actions.selectSession(row.id),
      })),
    ];
    if (state.running) commands.splice(4, 0, { group: '动作', label: '停止这一轮', run: () => actions.stop() });
    const needle = query.trim().toLowerCase();
    if (!needle) return commands;
    return commands.filter((command) => command.label.toLowerCase().includes(needle));
  }

  function renderPalette(): void {
    const query = paletteInput.value;
    paletteCommands = buildCommands(query);
    paletteIndex = Math.max(0, Math.min(paletteIndex, paletteCommands.length - 1));
    if (!paletteCommands.length) {
      paletteList.innerHTML = '<div class="y-palette-empty">没有匹配的命令</div>';
      return;
    }
    let html = '';
    let lastGroup = '';
    paletteCommands.forEach((command, index) => {
      if (command.group !== lastGroup) {
        html += `<div class="y-cmd-group">${command.group}</div>`;
        lastGroup = command.group;
      }
      html += `<button class="y-cmd ${index === paletteIndex ? 'is-on' : ''}" type="button" data-cmd="${index}"><span>${esc(command.label)}</span>${command.hint ? `<span class="hint">${esc(command.hint)}</span>` : ''}</button>`;
    });
    paletteList.innerHTML = html;
  }

  function openPalette(): void {
    paletteTrigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    evpop.hidden = true;
    menu.hidden = true;
    palette.hidden = false;
    paletteInput.value = '';
    paletteIndex = 0;
    renderPalette();
    requestAnimationFrame(() => paletteInput.focus());
  }

  function closePalette(): void {
    palette.hidden = true;
    paletteTrigger?.focus();
  }

  function runCommand(command: Command): void {
    closePalette();
    void Promise.resolve(command.run()).catch((error) => store.dispatch({ type: 'toast', toast: (error as Error).message }));
  }

  paletteList.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-cmd]');
    if (!button?.dataset.cmd) return;
    const command = paletteCommands[Number(button.dataset.cmd)];
    if (command) runCommand(command);
  });
  paletteInput.addEventListener('input', () => {
    paletteIndex = 0;
    renderPalette();
  });
  paletteInput.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const delta = event.key === 'ArrowDown' ? 1 : -1;
      paletteIndex = Math.max(0, Math.min(paletteCommands.length - 1, paletteIndex + delta));
      renderPalette();
      paletteList.querySelector<HTMLElement>('.is-on')?.scrollIntoView({ block: 'nearest' });
    } else if (event.key === 'Enter') {
      event.preventDefault();
      const command = paletteCommands[paletteIndex];
      if (command) runCommand(command);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      closePalette();
    }
  });
  palette.addEventListener('click', (event) => {
    if (event.target === palette) closePalette();
  });

  document.addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
      event.preventDefault();
      if (palette.hidden) openPalette();
      else closePalette();
    }
  });

  function renderSidebar(state: State): void {
    const signature = `${state.active}|${state.sessionAll}|${state.sessions.map((row) => `${row.id}:${row.title}:${row.pending_approvals}:${row.archived ? 1 : 0}`).join('|')}`;
    if (signature === sidebarSignature) return;
    sidebarSignature = signature;
    sessionCount.textContent = String(state.sessions.length);
    const live = state.sessions.filter((row) => !row.archived);
    // One workspace per process today, so this is a recency fold rather than a
    // per-workspace tree: six rows, the active one always kept in view, and the
    // rest one click away. See plan/06 §4 for the deferred grouping decision.
    const folded = !state.sessionAll && live.length > SESSION_RECENT;
    let visible = live;
    if (folded) {
      visible = live.slice(0, SESSION_RECENT);
      const active = live.find((row) => row.id === state.active);
      if (active && !visible.some((row) => row.id === active.id)) {
        visible = [...visible.slice(0, SESSION_RECENT - 1), active];
      }
    }
    const archived = state.sessions.filter((row) => row.archived);
    const rows =
      visible.map((row) => sessionRow(row, row.id === state.active)).join('') ||
      '<div class="y-sec" style="margin:8px 6px">还没有会话</div>';
    const more =
      live.length > SESSION_RECENT
        ? `<button class="y-sec-more" type="button" data-session-all>${state.sessionAll ? '收起' : `全部会话 · ${live.length}`}</button>`
        : '';
    // Archived sessions are put away, not older: their own section, at the
    // bottom, still reachable in one click.
    const archivedRows = archived.length
      ? `<div class="y-sec y-sec-arch">已归档<span class="n">${archived.length}</span></div>${archived
          .map((row) => sessionRow(row, row.id === state.active))
          .join('')}`
      : '';
    sessionList.innerHTML = rows + more + archivedRows;
  }

  function renderTranscript(state: State): void {
    const groups = groupsFor(state.items);
    const switched = state.active !== lastActive || lastGroups === 0;
    const nearBottom = !switched && scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 120;
    const open = new Set(
      [...transcript.querySelectorAll<HTMLDetailsElement>('details[open]')].map((node) => node.dataset.key ?? ''),
    );
    if (!state.active) {
      lastActive = null;
      lastGroups = 0;
      transcript.innerHTML = `<div class="y-empty"><span class="mark">爻</span><h3>从左侧选一个会话</h3><p>或者点「新任务」，从一句话开始。</p></div>`;
      return;
    }
    if (!groups.length) {
      lastActive = state.active;
      lastGroups = 0;
      transcript.innerHTML = `<div class="y-empty"><span class="mark">爻</span><h3>新任务</h3><p>${state.error ? esc(state.error) : '说一句话，爻爻会把它记进同一份会话日志。'}</p></div>`;
      return;
    }
    const current = groups[groups.length - 1];
    const past = groups.slice(0, -1);
    const folded = !state.showAllPast && past.length > 2 ? past.slice(0, -2) : [];
    const visiblePast = folded.length ? past.slice(-2) : past;
    const more = folded.length
      ? `<button class="y-past-more" type="button" data-past-toggle>更早的 ${folded.length} 轮 ⌄</button>`
      : state.showAllPast && past.length > 2
        ? '<button class="y-past-more" type="button" data-past-toggle>收起更早的轮次 ⌃</button>'
        : '';
    const pastHtml = visiblePast.map((group) => pastRowHtml(group, state.expanded.has(group.turn))).join('');
    transcript.innerHTML = `<div class="y-past">${more}${pastHtml}</div>${currentTurnHtml(current, state)}`;
    for (const key of open) {
      if (!key) continue;
      const node = transcript.querySelector<HTMLDetailsElement>(`details[data-key="${CSS.escape(key)}"]`);
      if (node) node.open = true;
    }
    lastActive = state.active;
    lastGroups = groups.length;
    if (switched) scroll.scrollTop = 0;
    else if (nearBottom) scroll.scrollTop = scroll.scrollHeight;
  }

  function renderTrace(state: State): void {
    const groups = groupsFor(state.items);
    const signature = groups.map((group) => `${group.turn}:${group.end?.ending ?? ''}`).join('|');
    if (signature === traceSignature) return;
    traceSignature = signature;
    hideTracePop();
    const host = trace.parentElement;
    if (host) host.hidden = groups.length < 3;
    trace.innerHTML = groups
      .map((group, index) => {
        const status = turnStatus(group.end);
        const cls = index === groups.length - 1 ? 'now' : group.end?.ending === 'yielded' ? 'wait' : '';
        const { steps } = turnFacts(group);
        const label = `第 ${group.index} 轮 · ${shortTitle(turnTitle(group), 40)} · ${status.label} · ${steps} 步`;
        return `<button class="y-tick ${cls}" type="button" data-jump="${esc(group.turn)}" aria-label="${esc(label)}" aria-describedby="tracePop"></button>`;
      })
      .join('');
  }

  function renderGate(state: State): void {
    const approval = gateOf(state);
    const signature = approval ? `${approval.id}:${approval.state}` : '';
    if (signature === gateSignature) return;
    gateSignature = signature;
    gate.innerHTML = gateHtml(state);
    // The gate is the card, not the condition: a session can be "waiting" (its
    // summary still says yielded) with no card left to show, and reserving an
    // empty box there reads as a control that failed to load. The status pill
    // already says 等待确认.
    gate.hidden = !approval;
  }

  function renderPanel(state: State, previous: State): void {
    const panel = state.panel;
    sheet.classList.toggle('open', Boolean(panel));
    sheet.setAttribute('aria-hidden', String(!panel));
    details.setAttribute('aria-expanded', String(Boolean(panel)));
    if (!panel) {
      panelSignature = '';
      if (previous.panel && panelTrigger) {
        panelTrigger.focus();
        panelTrigger = null;
      }
      return;
    }
    evpop.hidden = true;
    if (!previous.panel) sheetClose.focus();
    const label = PANEL_TABS.find((tab) => tab.key === panel.key)?.label ?? '本轮详情';
    const turn = typeof asRecord(panel.data).turn === 'string' ? String(asRecord(panel.data).turn) : '';
    const index = turn ? groupsFor(state.items).findIndex((group) => group.turn === turn) : -1;
    sheetTitle.textContent = panel.key === 'why' && index >= 0 ? `${label} · 第 ${index + 1} 轮` : label;
    sheet.querySelectorAll<HTMLElement>('[data-panel-tab]').forEach((tab) => {
      tab.setAttribute('aria-selected', String(tab.dataset.panelTab === panel.key));
    });
    const signature = `${panel.key}:${panel.loading}:${panel.error ?? ''}:${panel.data ? 'data' : ''}`;
    if (signature === panelSignature) return;
    panelSignature = signature;
    sheetBody.innerHTML = panelBody(panel);
  }

  function renderResult(state: State): void {
    const groups = groupsFor(state.items);
    const currentTurn = groups[groups.length - 1]?.turn;
    const refs = refsFor(state.items).filter((ref) => !ref.turn || ref.turn === currentTurn);
    if (!refs.length) {
      resultEl.hidden = true;
      resultSignature = '';
      return;
    }
    resultEl.hidden = false;
    resultEl.classList.toggle('open', state.result);
    resultEl.classList.toggle('full', state.resultFull);
    resultMore.textContent = state.result ? '收起' : '展开';
    resultMore.setAttribute('aria-expanded', String(state.result));
    resultFull.setAttribute('aria-pressed', String(state.resultFull));
    resultCount.textContent = `${refs.length} 个产出`;
    const signature = `${refs.map((ref) => ref.locator).join('|')}|${state.artifact}|${state.artifactView?.kind ?? ''}`;
    if (signature === resultSignature) return;
    resultSignature = signature;
    const turnIndex = new Map(groups.map((group, index) => [group.turn, index + 1]));
    artifactList.innerHTML =
      refs
        .map(
          (ref) =>
            `<button class="y-artifact ${ref.locator === state.artifact ? 'on' : ''}" type="button" data-artifact-locator="${esc(ref.locator)}"><span>${esc(ref.label ?? artifactLabel(ref.locator))}</span><span>${esc(ref.turn && turnIndex.has(ref.turn) ? `第 ${turnIndex.get(ref.turn)} 轮` : '')}</span></button>`,
        )
        .join('') + (state.summary?.usage ? `<div class="y-art-use">${fmtTokens(state.summary.usage.total_tokens ?? (state.summary.usage.input_tokens ?? 0) + (state.summary.usage.output_tokens ?? 0))} tokens</div>` : '');
    artifactPreview.innerHTML = state.artifact
      ? state.artifactView
        ? previewHtml(state.artifactView)
        : '<div class="y-panel-empty">读取 artifact…</div>'
      : '<div class="y-panel-empty">选择一个产出…</div>';
  }

  function render(state: State, previous: State): void {
    const groups = groupsFor(state.items);
    const current = groups[groups.length - 1];
    const status = turnStatus(current?.end);
    const pending = [...state.items.values()].some((item) => item.kind === 'approval' && item.state === 'pending');

    app.classList.toggle('y-side-off', !state.sidebar);
    app.classList.toggle('y-side-open', state.sidebar);
    title.textContent = state.summary?.title || (state.active ? '新任务' : '爻爻');
    round.textContent = `第 ${groups.length} 轮`;
    modelChip.textContent = modelLabel(state);
    modelChip.title = '这个会话用哪个模型；选择在下一轮生效';
    const archived = Boolean(state.summary?.archived);
    const archiveItem = menu.querySelector<HTMLElement>('[data-menu="archive"]');
    if (archiveItem && archiveItem.dataset.armed !== '1') {
      archiveItem.textContent = archived ? '恢复此会话' : '归档此会话';
    }
    // The top pill reports the kernel's fold for the latest session, not the
    // turn's ending. Only a session with a current turn may carry a claim.
    const sessionStatus = current ? state.status : null;
    const sessionLabel = sessionStatusLabel(sessionStatus);
    statusPill.innerHTML = state.running
      ? '<span class="pill run"><i></i>运行中</span>'
      : state.waiting || pending
        ? '<span class="pill wait"><i></i>等待确认</span>'
        : current
          ? `<span class="pill ${sessionStatus ? sessionLabel.className : status.className}"><i></i>${sessionStatus ? sessionLabel.label : status.label}</span>`
          : '';

    // A run in flight has no verdict yet: the axes say "unverified" merely
    // because nothing has been checked, so the note waits for the terminal event.
    const note =
      current && !state.running && !state.waiting && !pending
        ? sessionStatusNote(sessionStatus)
        : null;
    statusNote.hidden = !note;
    statusNote.textContent = note ?? '';
    statusNote.className = note
      ? `y-status-note ${sessionLabel.className === 'unverified' ? 'warn' : 'fail'}`
      : 'y-status-note';

    const local = state.meta?.sandbox === 'local';
    envNote.hidden = !local;
    envNote.textContent = local
      ? '当前是 local 后端：爻爻可以编辑文件，但不会启动命令或渲染进程。需要构建、测试、导出视频时，请用 app-container 启动。'
      : '';
    stop.hidden = !state.running;
    send.disabled = !stateHasToken() || !input.value.trim() || Boolean(gateOf(state));
    details.title = '证据面板（M5）';

    renderSidebar(state);
    renderTrace(state);
    renderGate(state);
    renderPanel(state, previous);
    renderResult(state);

    if (state.items !== previous.items || state.active !== previous.active || state.expanded !== previous.expanded || state.showAllPast !== previous.showAllPast || state.running !== previous.running) {
      renderTranscript(state);
    } else if (state.live && state.live !== previous.live) {
      const live = transcript.querySelector<HTMLElement>('.y-live');
      if (live) live.innerHTML = `${esc(state.live.text ?? '')}<span class="cursor"></span>`;
      else renderTranscript(state);
    }

    toast.hidden = !state.toast;
    toast.textContent = state.toast ?? '';
    toast.classList.toggle('error', Boolean(state.error && state.toast === state.error));
    if (state.toast) {
      clearTimeout(toastTimer);
      toastTimer = setTimeout(() => store.dispatch({ type: 'toast', toast: null }), 2200);
    }
  }

  store.subscribe(render);
  render(store.get(), { ...store.get(), items: new Map(), expanded: new Set() });
  resize();

  // The gate counts down on the page's clock, from the expiry the projection
  // recorded. Nothing is re-requested: the card's own state is the server's,
  // and this only changes the words on its clock.
  setInterval(() => {
    const node = gate.querySelector<HTMLElement>('[data-expires]');
    if (!node) return;
    const expires = node.dataset.expires ?? '';
    const left = expires ? Date.parse(expires) - Date.now() : 0;
    const text = left > 0 ? `${clockText(left)} 后失效` : '已过期';
    if (node.textContent !== text) node.textContent = text;
  }, 1000);
}

export function renderAuth(root: HTMLElement): void {
  root.innerHTML = `<div class="y-auth"><div class="card">
    <span class="mark" style="width:36px;height:36px;border-radius:9px;font-size:17px;display:grid;place-items:center">爻</span>
    <h1>缺少访问令牌</h1>
    <p>用 <code>agent web</code> 打印的完整 URL 打开，令牌在 <code>#t=</code> 后面；页面只把它放进 <code>Authorization</code> 头，不写存储。</p>
  </div></div>`;
}
