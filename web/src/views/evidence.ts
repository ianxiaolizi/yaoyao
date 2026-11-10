import type { SessionSummary } from '../types';
import type { PanelState } from '../state';
import { dueTime, esc, fmtTokens, mdText, relativeTime } from '../format';
import { asRecord } from './helpers';

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



export { panelBody, sessionRow, subagentNodes, whyBody };
