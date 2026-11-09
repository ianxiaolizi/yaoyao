import type { Item } from '../types';
import type { State } from '../state';
import { argsSummary, duration, esc, fmtTokens, mdText, turnStatus } from '../format';
import { groupsFor, shortTitle, turnTitle, ttlText, type Group } from './helpers';

const approvalLabel: Record<string, string> = {
  pending: '待确认',
  granted: '已允许',
  denied: '已拒绝',
  expired: '已过期',
  cancelled: '已取消',
};

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



export {
  actionHtml,
  approvalRecord,
  bodyHtml,
  currentTurnHtml,
  gateHtml,
  gateOf,
  pastRowHtml,
};
