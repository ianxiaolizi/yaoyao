/* 爻爻 · the page.
 *
 * The page is a projection of the session log. It owns no facts: items arrive
 * from `/transcript` and then from the SSE stream, are upserted by id, and are
 * rendered by turn. Everything else here — selection, widths, theme, the open
 * inspector — is disposable UI state, which is why only those things go into
 * localStorage.
 */
(() => {
  'use strict';

  const $ = (id) => document.getElementById(id);
  const root = document.documentElement;

  const TOKEN = location.hash.startsWith('#t=')
    ? decodeURIComponent(location.hash.slice(3))
    : '';
  const params = new URLSearchParams(location.search);

  const state = {
    token: TOKEN,
    meta: null,
    sessions: [],
    /// Whether the archived section is unfolded. Folded by default: archived
    /// sessions are out of the way until someone asks for them.
    showArchived: false,
    active: null,
    activeSummary: null,
    items: new Map(),
    head: 0,
    live: { text: '', reasoning: '' },
    liveTurn: null,
    running: false,
    waiting: false,
    pendingUser: '',
    forceRunningUntil: 0,
    settling: new Set(),
    connection: 'idle',
    why: null,
    whyFor: null,
    whyWant: null,
    diff: null,
    diffFor: null,
    context: null,
    contextFor: null,
    plan: null,
    planFor: null,
    subagents: null,
    subagentsFor: null,
    schedules: null,
    schedulesFor: null,
    artifactUrl: '',
    controller: null,
    streamGeneration: 0,
    reconnectTimer: null,
    reconnectAttempt: 0,
    search: '',
    pendingRender: 0,
    refreshTimer: null,
    whyTimer: null,
    lastError: '',
  };

  const BUDGET_FALLBACK = 32_000;
  const STORE = {
    theme: 'yaoyao.theme',
    session: 'yaoyao.session',
    side: 'yaoyao.side',
    panel: 'yaoyao.panel',
    doc: 'yaoyao.doc',
    composer: 'yaoyao.composer',
  };

  // ────────────────────────────────────────────────────────────── text ──

  function esc(value) {
    return String(value ?? '')
      .replaceAll('&', '&amp;')
      .replaceAll('<', '&lt;')
      .replaceAll('>', '&gt;')
      .replaceAll('"', '&quot;')
      .replaceAll("'", '&#39;');
  }

  function truncate(text, limit = 120) {
    const one = String(text ?? '').replace(/\s+/g, ' ').trim();
    if (one.length <= limit) return one;
    return one.slice(0, Math.max(0, limit - 1)) + '…';
  }

  function headline(text) {
    return truncate(String(text ?? '').split('\n').find((line) => line.trim()) || '', 90);
  }

  function fmtTokens(value) {
    const n = Number(value || 0);
    if (!n) return '0';
    if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, '') + 'm';
    if (n >= 1_000) return (n / 1_000).toFixed(1).replace(/\.0$/, '') + 'k';
    return String(n);
  }

  function fmtDuration(ms) {
    if (ms == null) return '';
    if (ms <= 0) return '<1ms';
    if (ms < 1000) return `${Math.round(ms)}ms`;
    if (ms < 60_000) return `${(ms / 1000).toFixed(1).replace(/\.0$/, '')}s`;
    return `${Math.floor(ms / 60_000)}m${Math.round((ms % 60_000) / 1000)}s`;
  }

  function parseDate(value) {
    if (!value) return null;
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? null : date;
  }

  function dayKey(date) {
    return `${date.getFullYear()}-${date.getMonth() + 1}-${date.getDate()}`;
  }

  function timeShort(value) {
    const date = parseDate(value);
    if (!date) return '';
    const now = new Date();
    if (dayKey(date) === dayKey(now)) {
      return date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false });
    }
    const yesterday = new Date(now);
    yesterday.setDate(now.getDate() - 1);
    if (dayKey(date) === dayKey(yesterday)) return '昨天';
    return `${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
  }

  // Ledger timestamps are UTC; a schedule's due time is worth reading in the
  // clock the person is actually sitting in front of.
  function whenText(value) {
    const date = parseDate(value);
    if (!date) return '';
    const day = dayKey(date) === dayKey(new Date())
      ? '今天'
      : `${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
    const time = date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false });
    return `${day} ${time}`;
  }

  function inline(text) {
    let out = esc(text);
    out = out.replace(/`([^`]+)`/g, '<code>$1</code>');
    out = out.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
    out = out.replace(/\[([^\]]+)\]\((https:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noreferrer">$1</a>');
    return out;
  }

  /* A deliberately small Markdown reader: paragraphs, fenced code, inline code,
     bold, headings and lists. Escaping happens before any tag is produced, so a
     tool result that contains HTML is shown, not executed. */
  function md(text) {
    const lines = String(text ?? '').replace(/\r\n?/g, '\n').split('\n');
    const out = [];
    let paragraph = [];
    let list = null;
    let code = null;

    const flushParagraph = () => {
      if (!paragraph.length) return;
      out.push(`<p>${paragraph.map(inline).join('<br>')}</p>`);
      paragraph = [];
    };
    const closeList = () => {
      if (list) {
        out.push(`</${list}>`);
        list = null;
      }
    };

    for (const line of lines) {
      if (code !== null) {
        if (/^\s*```/.test(line)) {
          out.push(`<pre class="code">${code.map(esc).join('\n')}</pre>`);
          code = null;
        } else {
          code.push(line);
        }
        continue;
      }
      if (/^\s*```/.test(line)) {
        flushParagraph();
        closeList();
        code = [];
        continue;
      }
      if (!line.trim()) {
        flushParagraph();
        closeList();
        continue;
      }
      const heading = line.match(/^(#{1,3})\s+(.*)$/);
      if (heading) {
        flushParagraph();
        closeList();
        out.push(`<h3>${inline(heading[2])}</h3>`);
        continue;
      }
      const bullet = line.match(/^\s*[-*]\s+(.*)$/);
      if (bullet) {
        flushParagraph();
        if (list !== 'ul') {
          closeList();
          out.push('<ul>');
          list = 'ul';
        }
        out.push(`<li>${inline(bullet[1])}</li>`);
        continue;
      }
      const numbered = line.match(/^\s*\d+[.)]\s+(.*)$/);
      if (numbered) {
        flushParagraph();
        if (list !== 'ol') {
          closeList();
          out.push('<ol>');
          list = 'ol';
        }
        out.push(`<li>${inline(numbered[1])}</li>`);
        continue;
      }
      closeList();
      paragraph.push(line);
    }
    if (code !== null) out.push(`<pre class="code">${code.map(esc).join('\n')}</pre>`);
    flushParagraph();
    closeList();
    return out.join('');
  }

  // ─────────────────────────────────────────────────────────────── HTTP ──

  class ApiError extends Error {
    constructor(status, body) {
      super(body?.message || `HTTP ${status}`);
      this.status = status;
      this.code = body?.code || 'http_error';
      this.body = body;
    }
  }

  async function api(path, options = {}) {
    const headers = { ...(options.headers || {}) };
    if (state.token) headers.Authorization = `Bearer ${state.token}`;
    let body = options.body;
    if (body && typeof body !== 'string') {
      headers['Content-Type'] = 'application/json';
      body = JSON.stringify(body);
    }
    const response = await fetch(path, { ...options, headers, body });
    const text = await response.text();
    let payload = null;
    try {
      payload = text ? JSON.parse(text) : null;
    } catch {
      payload = { message: text };
    }
    if (!response.ok) throw new ApiError(response.status, payload);
    return payload;
  }

  // ────────────────────────────────────────────────────────────── toast ──

  let toastTimer = 0;
  function toast(message, kind = '') {
    const el = $('toast');
    el.textContent = message;
    el.className = `toast show${kind === 'error' ? ' err' : ''}`;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.remove('show'), 4200);
  }

  function composerError(message = '') {
    const el = $('composerError');
    el.textContent = message;
    el.hidden = !message;
  }

  // ──────────────────────────────────────────────────────────── sessions ──

  function sessionTitle(summary) {
    if (!summary) return '新任务';
    return summary.title || summary.id;
  }

  function usedTokens(summary) {
    const usage = summary?.usage || {};
    return Number(usage.total_tokens ?? ((usage.input_tokens || 0) + (usage.output_tokens || 0)) ?? 0);
  }

  function budgetTokens() {
    return Number(state.meta?.budget || 0) || BUDGET_FALLBACK;
  }

  // What the composer and the header call a session's model. A session that
  // chose one shows the choice; otherwise the server's startup label applies.
  function modelLabel(choice) {
    if (!choice) return state.meta?.model || '—';
    if (!choice.model) return choice.provider === 'echo' ? 'echo' : choice.provider;
    return choice.model;
  }

  // The models this server can actually build. The host supplies the list, so
  // the menu never offers a choice whose only outcome is an error.
  function renderModelOptions() {
    const host = $('modelOptions');
    if (!host) return;
    const options = state.meta?.models || [];
    if (!options.length) {
      host.innerHTML = '<div class="menu-note">这个服务没有可切换的模型</div>';
      return;
    }
    const current = state.activeSummary?.model || null;
    host.innerHTML = options
      .map((option) => {
        const chosen =
          current &&
          current.provider === option.provider &&
          (current.model || '') === (option.model || '');
        return `<button data-provider="${esc(option.provider)}" data-model="${esc(option.model || '')}">${chosen ? '✓ ' : ''}${esc(option.label)}</button>`;
      })
      .join('');
  }

  async function chooseModel(provider, model) {
    $('modelMenu').open = false;
    // A model can be chosen before the first word: the choice is what the
    // session's first turn will be assembled with, so starting the session
    // here is the same act as typing. The id is minted locally, exactly as the
    // new-task button does it, and the choice creates the log.
    if (!state.active) newSession();
    try {
      const result = await api(`/api/sessions/${encodeURIComponent(state.active)}/model`, {
        method: 'POST',
        body: { v: 1, provider, model: model || null },
      });
      const choice = result?.model || { provider, model: model || null };
      // The label has to change on this click, not when the first turn lands.
      // A fresh session has no summary yet, so stand one up around the choice;
      // the first refresh replaces it with the server's own.
      state.activeSummary = { ...(state.activeSummary || { id: state.active }), model: choice };
      updateHeader();
      // The switch belongs to the next turn: a turn already running keeps the
      // model it was assembled with, and saying so avoids a "why no change" click.
      toast(
        state.running
          ? `这个会话改用 ${modelLabel(choice)}；正在跑的这轮仍然用原来的模型，下一轮生效`
          : `这个会话改用 ${modelLabel(choice)}；下一轮生效`,
      );
      scheduleSessionsRefresh();
    } catch (error) {
      toast(error.message || '模型没有切成', 'error');
    }
  }

  // One row. A row is a container rather than a button because the archive and
  // delete actions are buttons themselves, and a button inside a button is not
  // something a browser will parse.
  function sessionRow(summary) {
    const used = usedTokens(summary);
    const share = Math.max(2, Math.min(100, (used / budgetTokens()) * 100));
    const pending = Number(summary.pending_approvals || 0);
    const badge = pending
      ? '<span class="badge ask">待批</span>'
      : summary.open_turn
        ? '<span class="badge">进行中</span>'
        : summary.yielded
          ? '<span class="badge">待续</span>'
          : '';
    const active = summary.id === state.active ? ' on' : '';
    const id = esc(summary.id);
    return `
      <div class="t${active}" data-row="${id}" title="${id}">
        <button class="tmain" data-session="${id}">
          <div class="r1"><span class="ti">${esc(sessionTitle(summary))}</span>${badge}</div>
          <div class="snip">${esc(summary.snippet || '—')}</div>
          <div class="r2">
            <span>${Number(summary.steps || 0)} 步</span><i class="dotsep"></i>
            <span>${esc(timeShort(summary.last_activity_at))}</span><i class="dotsep"></i>
            <span>${fmtTokens(used)}</span>
            <span class="ctxbar" title="本轮用量 / 上下文预算"><i style="width:${share.toFixed(0)}%"></i></span>
          </div>
          ${summary.forked_from ? `<div class="forkline">↳ 分叉自 ${esc(summary.forked_from)}</div>` : ''}
        </button>
        <div class="tact">
          <button data-act="${summary.archived ? 'restore' : 'archive'}" data-for="${id}">${summary.archived ? '恢复' : '归档'}</button>
          <button class="danger" data-act="delete" data-for="${id}">删除</button>
        </div>
      </div>`;
  }

  function renderSessions() {
    const host = $('threads');
    const query = state.search.trim().toLowerCase();
    const rows = state.sessions.filter((summary) => {
      if (!query) return true;
      return `${summary.title || ''} ${summary.snippet || ''} ${summary.id}`.toLowerCase().includes(query);
    });
    if (!rows.length) {
      host.innerHTML = `<div class="empty">${state.sessions.length ? '没有匹配的会话' : '还没有会话。点「新任务」开始第一轮。'}</div>`;
      return;
    }

    const now = new Date();
    const yesterday = new Date(now);
    yesterday.setDate(now.getDate() - 1);
    const groups = [
      { label: '今天', rows: [] },
      { label: '昨天', rows: [] },
      { label: '更早', rows: [] },
    ];
    // Archived sessions are not "older", they are put away: they get their own
    // section at the bottom, folded by default, and never count towards a day.
    const archived = rows.filter((summary) => summary.archived);
    for (const summary of rows) {
      if (summary.archived) continue;
      const at = parseDate(summary.last_activity_at);
      let index = 2;
      if (at && dayKey(at) === dayKey(now)) index = 0;
      else if (at && dayKey(at) === dayKey(yesterday)) index = 1;
      groups[index].rows.push(summary);
    }

    const html = [];
    for (const group of groups) {
      if (!group.rows.length) continue;
      html.push(`<div class="grp"><span>${group.label}</span><span>${group.rows.length}</span></div>`);
      html.push(group.rows.map(sessionRow).join(''));
    }
    if (archived.length) {
      html.push(
        `<button class="grp grpbtn" data-archived-toggle title="归档的会话日志完整，随时可以恢复">` +
          `<span>已归档 ${state.showArchived ? '▾' : '▸'}</span><span>${archived.length}</span></button>`,
      );
      if (state.showArchived) html.push(archived.map(sessionRow).join(''));
    }
    host.innerHTML = html.join('');
    host.querySelectorAll('[data-session]').forEach((button) => {
      button.addEventListener('click', () => openSession(button.dataset.session));
    });
    host.querySelectorAll('[data-archived-toggle]').forEach((button) => {
      button.addEventListener('click', () => {
        state.showArchived = !state.showArchived;
        renderSessions();
      });
    });
    host.querySelectorAll('[data-act]').forEach((button) => {
      button.addEventListener('click', (event) => {
        event.stopPropagation();
        const id = button.dataset.for;
        if (button.dataset.act === 'archive') setArchived(id, true);
        else if (button.dataset.act === 'restore') setArchived(id, false);
        else armDelete(button, id);
      });
    });
  }

  // Archiving and deleting are next to each other in a menu the cursor is
  // already on, so delete asks twice: the first click turns the label into the
  // question, and it goes back to "删除" if the question is ignored.
  let deleteArmTimer = 0;
  function armDelete(button, id) {
    if (button.dataset.armed === '1') {
      clearTimeout(deleteArmTimer);
      deleteSession(id);
      return;
    }
    button.dataset.armed = '1';
    button.textContent = '确认删除？';
    clearTimeout(deleteArmTimer);
    deleteArmTimer = setTimeout(() => {
      button.dataset.armed = '0';
      button.textContent = '删除';
    }, 4000);
  }

  async function setArchived(id, archived) {
    try {
      await api(`/api/sessions/${encodeURIComponent(id)}/archive`, {
        method: 'POST',
        body: { v: 1, archived },
      });
      const row = state.sessions.find((summary) => summary.id === id);
      if (row) row.archived = archived;
      if (state.activeSummary?.id === id) state.activeSummary.archived = archived;
      renderSessions();
      updateHeader();
      toast(archived ? '已归档：日志完整，随时可以恢复' : '已恢复到会话列表');
    } catch (error) {
      toast(error.message || '归档没有改成', 'error');
    }
  }

  async function deleteSession(id) {
    // Leaving first matters: this page's own stream holds the log open, and a
    // log that is still being read cannot be removed on Windows.
    const leaving = state.active === id;
    if (leaving && state.controller) state.controller.abort();
    try {
      await api(`/api/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' });
      state.sessions = state.sessions.filter((summary) => summary.id !== id);
      if (leaving) newSession();
      renderSessions();
      toast('会话已删除');
    } catch (error) {
      // The stream was dropped on the way out, and the delete did not happen:
      // the session is still here and so is its turn, so take the stream back.
      if (leaving) connectStream(id);
      const busy = error instanceof ApiError && error.status === 409;
      toast(busy ? '这一轮还在跑，停下来再删' : error.message || '没有删掉', 'error');
      renderSessions();
    }
  }

  function applySummary(summary) {
    state.activeSummary = summary || null;
    if (!summary) return;
    // A POST has already been accepted but its first event may not be in this
    // summary yet. Keep the optimistic "running" until the log catches up, or
    // a refresh in that window would make the stop button disappear.
    state.running =
      Boolean(state.pendingUser) ||
      Date.now() < state.forceRunningUntil ||
      (Boolean(summary.open_turn) && !summary.yielded);
    state.waiting = Boolean(summary.yielded) || Number(summary.pending_approvals || 0) > 0;
    updateHeader();
  }

  function activeTitle() {
    if (state.activeSummary?.title) return state.activeSummary.title;
    const firstUser = [...state.items.values()].find((item) => item.kind === 'user');
    if (firstUser?.text) return headline(firstUser.text);
    return state.active ? '新任务' : '爻爻';
  }

  function hasExpiredApproval() {
    return [...state.items.values()].some(
      (item) => item.kind === 'approval' && item.state === 'expired',
    );
  }

  function updateHeader() {
    const summary = state.activeSummary;
    $('sessionTitle').textContent = state.active ? activeTitle() : '爻爻';
    // The id is a log key, not a title: keep it on hover instead of spending
    // the header on it.
    $('sessionTag').textContent = state.active ? '会话' : '本地';
    $('sessionTag').title = state.active ? (summary?.id || state.active || '') : '';
    $('crumbSep').hidden = !state.active;
    $('archBadge').hidden = !summary?.archived;
    // The model belongs to the session once someone chose one; until then the
    // server's startup label is what the next turn will use.
    const model = modelLabel(summary?.model);
    $('modelName').textContent = model;
    $('modelTag').innerHTML = `<i></i><span>${esc(model)} · ${esc(state.meta?.sandbox || '—')}</span>`;
    renderModelOptions();
    const usage = summary?.usage;
    if (usage) {
      const used = usedTokens(summary);
      $('usageHint').textContent = `${fmtTokens(used)} / ${fmtTokens(budgetTokens())}`;
    } else {
      $('usageHint').textContent = state.meta?.budget ? `预算 ${fmtTokens(state.meta.budget)}` : '—';
    }
    $('sendBtn').disabled = !state.token;
    $('stopBtn').disabled = !state.running;
    document.body.classList.toggle('turn-running', Boolean(state.running));
    const input = $('input');
    const untouched = !state.running && !state.items.size && !state.pendingUser;
    input.placeholder = hasExpiredApproval() && !state.running
      ? '审批已过期，点上面的「继续这一轮」…'
      : state.waiting
        ? '先回答上面的审批，这一轮会从那里继续…'
        : state.running
          ? '插话：在下一个步骤边界生效…'
          : untouched
            ? '想做点什么？直接说给爻爻…'
            : '继续说…';
    input.disabled = !state.token;
  }

  async function refreshSessions() {
    try {
      const payload = await api('/api/sessions');
      state.sessions = payload.sessions || [];
      if (state.active) {
        const summary = state.sessions.find((row) => row.id === state.active);
        if (summary) applySummary(summary);
      }
      renderSessions();
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        connection('closed', '令牌无效');
        toast('令牌无效。请用终端里带 #t= 的地址重新打开。', 'error');
      }
    }
  }

  function scheduleSessionsRefresh() {
    clearTimeout(state.refreshTimer);
    state.refreshTimer = setTimeout(refreshSessions, 900);
  }

  // ──────────────────────────────────────────────────────────────── stream ──

  function connection(kind, text) {
    state.connection = kind;
    const el = $('conn');
    el.className = `conn ${kind}`;
    $('connText').textContent = text || ({ open: '已连接', reconnecting: '重连中', closed: '未连接' }[kind] || kind);
  }

  function parseEventBlock(block) {
    let name = 'message';
    const data = [];
    for (const raw of block.split('\n')) {
      const line = raw.replace(/\r$/, '');
      if (!line || line.startsWith(':')) continue;
      if (line.startsWith('event:')) name = line.slice(6).trim();
      else if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
    }
    if (!data.length) return null;
    const text = data.join('\n');
    try {
      return { name, data: JSON.parse(text) };
    } catch {
      return { name, data: { raw: text } };
    }
  }

  async function connectStream(session, attempt = 0) {
    if (!state.token || !session || state.active !== session) return;
    const generation = ++state.streamGeneration;
    if (state.controller) state.controller.abort();
    const controller = new AbortController();
    state.controller = controller;
    clearTimeout(state.reconnectTimer);
    connection(attempt ? 'reconnecting' : 'idle', attempt ? `重连中 · ${Math.min(attempt, 5)}` : '连接中');

    // The server's SSE cursor is inclusive; ask for the event after the last
    // one we applied, or from 0 when there is no cursor yet.
    const from = state.head ? state.head + 1 : 0;
    let response;
    try {
      response = await fetch(`/api/sessions/${encodeURIComponent(session)}/events?from=${from}`, {
        headers: state.token ? { Authorization: `Bearer ${state.token}` } : {},
        signal: controller.signal,
      });
    } catch (error) {
      if (controller.signal.aborted) return;
      scheduleReconnect(session, attempt);
      return;
    }

    if (generation !== state.streamGeneration) return;

    if (!response.ok) {
      let body = null;
      try { body = await response.json(); } catch { /* not JSON */ }
      if (response.status === 404 && attempt < 8) {
        // A brand-new session has no log until its first turn's append lands.
        state.reconnectTimer = setTimeout(() => connectStream(session, attempt + 1), 350 + attempt * 200);
        return;
      }
      connection('closed', response.status === 401 ? '令牌无效' : '无法连接');
      toast(body?.message || `事件流连接失败（HTTP ${response.status}）`, 'error');
      return;
    }

    connection('open', '已连接');
    state.reconnectAttempt = 0;
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';

    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, '\n');
        let split;
        while ((split = buffer.indexOf('\n\n')) >= 0) {
          const block = buffer.slice(0, split);
          buffer = buffer.slice(split + 2);
          const event = parseEventBlock(block);
          if (event) handleStreamEvent(event, session);
        }
      }
      if (controller.signal.aborted) return;
      connection('reconnecting', '连接断开');
      scheduleReconnect(session, attempt + 1);
    } catch (error) {
      if (controller.signal.aborted) return;
      connection('reconnecting', '连接断开');
      scheduleReconnect(session, attempt + 1);
    }
  }

  function scheduleReconnect(session, attempt) {
    if (!state.token || !session || state.active !== session) return;
    clearTimeout(state.reconnectTimer);
    const wait = Math.min(8000, 500 * 2 ** Math.min(attempt, 4));
    connection('reconnecting', `重连中 · ${Math.min(attempt, 5)}`);
    state.reconnectTimer = setTimeout(() => connectStream(session, attempt + 1), wait);
  }

  function handleStreamEvent(event, session) {
    if (state.active !== session) return;
    const data = event.data || {};
    if (event.name === 'hello') {
      state.head = Math.max(state.head, Number(data.head || 0));
      return;
    }
    if (event.name === 'heartbeat') {
      state.head = Math.max(state.head, Number(data.head || 0));
      return;
    }
    if (event.name === 'item') {
      const item = data.item;
      if (!item || !item.id) return;
      const previous = state.items.get(item.id);
      state.items.set(item.id, item);
      state.head = Math.max(state.head, Number(item.updated_seq ?? item.seq ?? 0));
      if (item.kind === 'answer' || item.kind === 'turn_end') {
        state.live.text = '';
        state.live.reasoning = '';
      }
      if (item.kind === 'thinking') {
        // The durable node has arrived; the preview would be a second copy.
        state.live.reasoning = '';
      }
      if (item.kind === 'user') {
        if (state.pendingUser && String(item.text || '').trim() === state.pendingUser.trim()) {
          state.pendingUser = '';
        }
        updateHeader();
      }
      if (item.kind === 'turn_end') {
        state.running = false;
        state.forceRunningUntil = 0;
        state.waiting = item.ending === 'yielded';
        if (state.waiting) state.pendingUser = '';
        state.context = null;
        state.plan = null;
        if (document.body.classList.contains('panel-open')) {
          if (root.dataset.panel === 'ctx') loadContext();
          if (root.dataset.panel === 'plan') loadPlan();
        }
        updateHeader();
      }
      if (item.kind === 'approval') {
        state.waiting = item.state === 'pending' || item.state === 'expired';
        updateHeader();
      }
      if (!previous || JSON.stringify(previous) !== JSON.stringify(item)) {
        scheduleRender();
        // The sidebar's steps/usage only move at these boundaries. Refolding
        // every session on every action token would make a long tool run pay
        // for a list nobody is looking at.
        if (['user', 'answer', 'turn_end', 'approval'].includes(item.kind)) {
          scheduleSessionsRefresh();
        }
        if (item.kind === 'action' && item.tool === 'git.diff') {
          state.diff = null;
          if (root.dataset.panel === 'diff' && document.body.classList.contains('panel-open')) {
            loadDiff();
          }
        }
        if (
          state.whyFor === item.turn &&
          ['answer', 'turn_end', 'approval'].includes(item.kind) &&
          document.body.classList.contains('panel-open')
        ) {
          clearTimeout(state.whyTimer);
          state.whyTimer = setTimeout(() => {
            state.whyFor = null;
            if (document.body.classList.contains('panel-open')) loadWhy();
          }, 600);
        }
      }
      return;
    }
    if (event.name === 'live') {
      if (data.kind === 'notice') {
        toast(data.text || '运行提示', 'error');
        return;
      }
      if (data.kind === 'text') {
        state.live.text += data.text || '';
        state.running = true;
        updateHeader();
      } else if (data.kind === 'reasoning') {
        state.live.reasoning += data.text || '';
        state.running = true;
        updateHeader();
      }
      scheduleRender();
      return;
    }
    if (event.name === 'error') {
      toast(data.message || '事件流读取出错', 'error');
    }
  }

  // ───────────────────────────────────────────────────────────── timeline ──

  function scheduleRender() {
    if (state.pendingRender) return;
    state.pendingRender = requestAnimationFrame(() => {
      state.pendingRender = 0;
      renderTimeline();
    });
  }

  function groupTurns() {
    const items = [...state.items.values()].sort((a, b) => {
      const seq = Number(a.seq || 0) - Number(b.seq || 0);
      if (seq) return seq;
      return String(a.id).localeCompare(String(b.id));
    });
    const groups = [];
    const byTurn = new Map();
    const loose = [];
    for (const item of items) {
      if (!item.turn) {
        loose.push(item);
        continue;
      }
      let group = byTurn.get(item.turn);
      if (!group) {
        group = {
          turn: item.turn,
          items: [],
          user: null,
          notes: [],
          thinking: [],
          actions: [],
          approvals: [],
          answers: [],
          end: null,
          firstSeq: Number(item.seq || 0),
        };
        byTurn.set(item.turn, group);
        groups.push(group);
      }
      group.items.push(item);
      if (item.kind === 'user') group.user = item;
      else if (item.kind === 'note') group.notes.push(item);
      else if (item.kind === 'thinking') group.thinking.push(item);
      else if (item.kind === 'action') group.actions.push(item);
      else if (item.kind === 'approval') group.approvals.push(item);
      else if (item.kind === 'answer') group.answers.push(item);
      else if (item.kind === 'turn_end') group.end = item;
      group.firstSeq = Math.min(group.firstSeq, Number(item.seq || 0));
    }
    groups.sort((a, b) => a.firstSeq - b.firstSeq);
    return { groups, loose };
  }

  function turnState(group, isLast) {
    if (group.end) {
      // Plain Chinese for the reader: `ending` values are kernel vocabulary.
      switch (group.end.ending) {
        case 'answered': return { label: '完成', cls: 'done' };
        case 'max_steps': return { label: '到达步数上限', cls: 'fail' };
        case 'yielded':
          if (group.approvals.some((row) => row.state === 'pending')) {
            return { label: '等待批准', cls: 'yield' };
          }
          if (group.approvals.some((row) => row.state === 'granted')) {
            return { label: '已批准', cls: 'done' };
          }
          if (group.approvals.some((row) => row.state === 'denied')) {
            return { label: '已拒绝', cls: 'done' };
          }
          if (group.approvals.some((row) => row.state === 'expired')) {
            return { label: '审批过期', cls: 'done' };
          }
          return { label: '主动停下', cls: 'done' };
        case 'cancelled': return { label: '已停止', cls: 'done' };
        case 'failed': return { label: '失败', cls: 'fail' };
        default: return { label: '已结束', cls: 'done' };
      }
    }
    if (group.approvals.some((row) => row.state === 'pending')) return { label: '等待批准', cls: 'yield' };
    if (isLast && (state.running || state.pendingUser)) return { label: '进行中', cls: 'live' };
    return { label: '进行中', cls: 'live' };
  }

  function statusMeta(action) {
    switch (action.status) {
      case 'completed': return '<span class="ok">✓</span>';
      case 'failed': return '<span class="fail">✗</span>';
      case 'refused': return '<span class="ref">⊘</span>';
      case 'waiting': return '<span class="wait">等待批准</span>';
      case 'cancelled': return '<span class="cancel">已取消</span>';
      case 'running': return '<span class="ok">·</span>';
      default: return '';
    }
  }

  function argsSummary(action) {
    const args = action.arguments;
    if (args == null) return '';
    if (typeof args === 'string') return truncate(args, 120);
    if (typeof args !== 'object') return truncate(String(args), 120);
    for (const key of ['command', 'path', 'file_path', 'pattern', 'query', 'url', 'name', 'prompt']) {
      if (typeof args[key] === 'string' && args[key].trim()) return truncate(args[key], 140);
      if (Array.isArray(args[key])) return truncate(args[key].join(' '), 140);
    }
    try {
      return truncate(JSON.stringify(args), 140);
    } catch {
      return '';
    }
  }

  function pretty(value) {
    if (value == null) return '';
    if (typeof value === 'string') return value;
    try { return JSON.stringify(value, null, 2); } catch { return String(value); }
  }

  function actionRow(action, index, isLast) {
    const open = action.status === 'running' || action.status === 'waiting' || isLast;
    const content = action.content == null ? '' : String(action.content);
    const artifact = content.match(/artifact:\/\/sha256\/[0-9a-f]{64}/)?.[0] || '';
    const long = content.length > 2400;
    const shown = long ? content.slice(0, 2400) : content;
    const output = long
      ? `<pre class="code out" data-short>${esc(shown)}\n…</pre>
         <pre class="code out" data-full hidden>${esc(content)}</pre>
         <button class="morex" data-expand>▸ 查看完整输出</button>`
      : content
        ? `<pre class="code out">${esc(content)}</pre>`
        : '';
    const meta = [
      statusMeta(action),
      action.duration_ms != null ? fmtDuration(action.duration_ms) : '',
    ].filter(Boolean).join(' ');
    return `
      <details class="act" ${open ? 'open' : ''}>
        <summary>
          <span class="chev">›</span>
          <span class="idx">${index}</span>
          <span class="nm">${esc(action.tool || 'tool')}</span>
          <span class="arg">${esc(argsSummary(action))}</span>
          <span class="sp"></span>
          <span class="meta">${meta}</span>
        </summary>
        <div class="bd">
          <div class="kv"><span class="k">参数</span><span class="v">${esc(pretty(action.arguments)) || '—'}</span></div>
          ${output}
          ${artifact ? `<button class="art-link" data-artifact="${esc(artifact)}">打开 artifact</button>` : ''}
        </div>
      </details>`;
  }

  function ttlText(expiresAt) {
    const expiry = parseDate(expiresAt);
    if (!expiry) return '等待你决定';
    const left = expiry.getTime() - Date.now();
    if (left <= 0) return '已过期';
    const minutes = Math.ceil(left / 60_000);
    if (minutes < 60) return `${minutes}m 后过期`;
    const hours = Math.floor(minutes / 60);
    return `${hours}h ${minutes % 60}m 后过期`;
  }

  function approvalCard(approval) {
    const key = `${state.active}:${approval.approval_id}`;
    const settling = state.settling.has(key);
    const pending = approval.state === 'pending';
    const expiry = parseDate(approval.expires_at);
    const expired =
      approval.state === 'expired' || (pending && Boolean(expiry) && expiry.getTime() <= Date.now());
    const stateLabel = {
      granted: '已允许 · 这一轮会从这里继续',
      denied: '已拒绝 · 调用没有派发',
      expired: '已过期 · 调用没有派发',
      cancelled: '已取消',
    }[approval.state] || approval.state;
    if (!pending || expired) {
      const label = expired ? '已过期 · 调用没有派发，这一轮需要继续' : stateLabel;
      const stateClass = expired ? 'expired' : approval.state;
      return `
        <div class="ask done ${expired ? 'expired' : ''}">
          <div class="ask-h"><span class="lb">审批</span><span class="ttl">${esc(approval.tool || '')}</span></div>
          <div class="ask-state ${esc(stateClass)}"><span class="dot"></span>${esc(label)}</div>
          ${expired
            ? '<button class="btn" data-expired-resume>继续这一轮</button>'
            : ''}
        </div>`;
    }
    return `
      <div class="ask" data-approval="${esc(approval.approval_id)}">
        <div class="ask-h">
          <span class="lb">需要批准</span>
          <span class="ttl" data-expires="${esc(approval.expires_at || '')}">${ttlText(approval.expires_at)}</span>
        </div>
        <div class="ask-b">
          <div class="ask-cmd">${esc(approval.summary || '（没有提供参数摘要）')}</div>
          <div class="ask-why">策略要求这次调用逐次确认。批准后会从这一轮停下的地方继续；拒绝则把「没有派发」告诉模型。</div>
          <div class="ask-act">
            <button class="btn pri" data-decision="once" ${settling ? 'disabled' : ''}>允许一次</button>
            <button class="btn" data-decision="turn" ${settling ? 'disabled' : ''}>本回合</button>
            <button class="btn" data-decision="session" ${settling ? 'disabled' : ''}>本会话</button>
            <button class="btn no" data-decision="deny" ${settling ? 'disabled' : ''}>拒绝</button>
            ${settling ? '<span class="ttl">正在记录…</span>' : ''}
          </div>
        </div>
      </div>`;
  }

  function renderTurnBody(group, isLast) {
    const out = [];
    // A resumed turn's "prompt" is the kernel's instruction to the model, not
    // something the person typed. Show the continuation, not the internal
    // sentence.
    if (!group.user?.resumed) {
      out.push(`
        <div class="u">
          <div class="ul">你</div>
          <p>${esc(group.user?.text || '（这一轮没有可读的提问）')}</p>
        </div>`);
    }

    const thinkingText = group.thinking.map((item) => item.text).filter(Boolean).join('\n\n');
    if (thinkingText) {
      const segments = thinkingText.split('\n\n').filter(Boolean).length;
      const steps = group.thinking[group.thinking.length - 1]?.steps;
      out.push(`
        <details class="think">
          <summary><span class="chev">›</span> 思考 · ${segments} 段${steps ? ` · ${steps} 步` : ''}</summary>
          <div class="bd">${md(thinkingText)}</div>
        </details>`);
    }

    if (isLast && state.live.reasoning) {
      out.push(`
        <details class="think live-think" open>
          <summary><span class="chev">›</span> 思考 · 正在生成</summary>
          <div class="bd">${esc(state.live.reasoning)}</div>
        </details>`);
    }

    if (group.actions.length || group.approvals.length) {
      const pending = group.approvals.filter((row) => row.state === 'pending').length;
      out.push(`
        <details class="acts" ${isLast ? 'open' : ''}>
          <summary>
            <span class="chev">›</span>
            <span class="lbl">行动</span>
            <span class="cnt">${group.actions.length} 次工具${pending ? ` · ${pending} 待批` : ''}</span>
            <span class="sp"></span>
            ${pending ? `<span class="pend">${pending} 项等你决定</span>` : ''}
          </summary>
          ${group.actions.map((action, index) => actionRow(action, index + 1, isLast && index === group.actions.length - 1 && action.status === 'running')).join('')}
          ${group.approvals.map(approvalCard).join('')}
        </details>`);
    }

    if (isLast && state.live.text) {
      out.push(`
        <div class="ans live-answer"><div class="md">${md(state.live.text)}</div><span class="caret"></span></div>
        <div class="gen"><i></i><i></i><i></i> 正在生成</div>`);
    }

    for (const answer of group.answers) {
      if (!String(answer.text || '').trim()) continue;
      out.push(`<div class="ans"><div class="md">${md(answer.text)}</div></div>`);
    }

    if (group.notes.length) {
      out.push(`<div class="end">${group.notes.map((note) => esc(truncate(note.text, 180))).join('<br>')}</div>`);
    }

    if (group.end) {
      const status = turnState(group, isLast);
      const usage = group.end.usage;
      const cost = usage
        ? `${fmtTokens(usage.total_tokens ?? ((usage.input_tokens || 0) + (usage.output_tokens || 0)))} tokens`
        : '';
      out.push(`
        <div class="end">
          <span>${esc(status.label)}</span>
          <span>${group.actions.length} 次工具</span>
          ${cost ? `<span>${cost}</span>` : ''}
          <span class="sp"></span>
          <a data-why="${esc(group.turn)}">为什么</a>
        </div>`);
    } else if (isLast) {
      const status = turnState(group, isLast);
      out.push(`<div class="turn-h" style="margin-top:16px"><span class="ln"></span><span class="st ${status.cls}">${status.label}</span></div>`);
    }
    return out.join('');
  }

  function renderTimeline() {
    const { groups, loose } = groupTurns();
    const timeline = $('timeline');
    const empty = $('empty');
    // The inbox item lands before its turn exists, so it briefly has no
    // `turn` to group under. Live text belongs to *that* turn, not to the
    // previously answered one: while it is pending, render a synthetic turn at
    // the tail and keep the prior turn's body live-free.
    const looseUser = state.running ? loose.find((item) => item.kind === 'user') : null;
    const pendingText = state.pendingUser || (state.running ? looseUser?.text || '' : '');
    // A resumed turn has no fresh inbox prompt to show; its live text still
    // belongs below the closed turn, not inside it.
    const openTail =
      state.running &&
      (groups.length === 0 || Boolean(groups[groups.length - 1].end)) &&
      (Boolean(state.live.text) || Boolean(state.live.reasoning));
    const showPending = Boolean(pendingText) || openTail;
    const hasAnything = groups.length || loose.length || showPending;

    if (!state.active) {
      empty.hidden = false;
      timeline.innerHTML = '';
      return;
    }
    if (!hasAnything) {
      empty.hidden = false;
      empty.querySelector('h2').textContent = state.activeSummary ? '这一段还没有内容' : '从一句话开始';
      timeline.innerHTML = '';
      return;
    }
    empty.hidden = true;

    // A steering sentence is in the log twice: once as the inbox item that
    // queued it (no turn yet, so it lands loose) and once as the turn/steered
    // note inside the turn. Render the grouped one only.
    const groupedNotes = new Set();
    for (const group of groups) {
      for (const note of group.notes) groupedNotes.add((note.text || '').trim());
    }

    const out = [];
    for (const item of loose) {
      if (item === looseUser && pendingText) continue;
      if (item.kind === 'note' && groupedNotes.has((item.text || '').trim())) continue;
      if (item.kind === 'note') out.push(`<div class="end">${esc(truncate(item.text, 220))}</div>`);
      else if (item.kind === 'answer') out.push(`<div class="ans"><div class="md">${md(item.text)}</div></div>`);
    }

    let turnNo = 0;
    groups.forEach((group, index) => {
      const isLast = index === groups.length - 1;
      const status = turnState(group, isLast && !showPending);
      const resumed = Boolean(group.user?.resumed);
      if (!resumed) turnNo += 1;
      const label = resumed ? '继续' : `第 ${turnNo} 轮`;
      const title = resumed ? '' : headline(group.user?.text || '');
      if (!isLast) {
        out.push(`
          <details class="prev" data-turn="${esc(group.turn)}">
            <summary>
              <span class="chev">›</span>
              <span class="lb">${label}</span>
              ${title ? `<span class="ttl">${esc(title)}</span>` : ''}
              <span class="meta">${group.actions.length} 次工具 · ${esc(status.label)}</span>
            </summary>
            <div class="body">${renderTurnBody(group, false)}</div>
          </details>`);
        return;
      }
      out.push(`
        <div class="turn-h">
          <span class="n">${label}</span>
          <span class="ln"></span>
          <span class="st ${status.cls}">${status.label}</span>
        </div>
        ${renderTurnBody(group, isLast && !showPending)}`);
    });

    if (showPending) {
      out.push(`
        <div class="turn-h"><span class="n">${pendingText ? '新的一轮' : '继续'}</span><span class="ln"></span><span class="st live"><i></i>进行中</span></div>
        ${pendingText ? `<div class="u"><div class="ul">你</div><p>${esc(pendingText)}</p></div>` : ''}
        ${state.live.reasoning ? `<details class="think live-think" open><summary><span class="chev">›</span> 思考 · 正在生成</summary><div class="bd">${esc(state.live.reasoning)}</div></details>` : ''}
        ${state.live.text
          ? `<div class="ans live-answer"><div class="md">${md(state.live.text)}</div><span class="caret"></span></div>`
          : '<div class="gen"><i></i><i></i><i></i> 正在启动这一轮</div>'}`);
    }

    timeline.innerHTML = out.join('');
    bindTimeline();
    maybeScroll();
  }

  function bindTimeline() {
    const timeline = $('timeline');
    timeline.querySelectorAll('[data-expand]').forEach((button) => {
      button.addEventListener('click', () => {
        const host = button.parentElement;
        const short = host?.querySelector('[data-short]');
        const full = host?.querySelector('[data-full]');
        if (!short || !full) return;
        const expanding = full.hidden;
        full.hidden = !expanding;
        short.hidden = expanding;
        button.textContent = expanding ? '▸ 收起输出' : '▸ 查看完整输出';
      });
    });
    timeline.querySelectorAll('[data-decision]').forEach((button) => {
      button.addEventListener('click', () => {
        const card = button.closest('[data-approval]');
        if (!card) return;
        decide(card.dataset.approval, button.dataset.decision);
      });
    });
    timeline.querySelectorAll('[data-expired-resume]').forEach((button) => {
      button.addEventListener('click', resumeExpired);
    });
    timeline.querySelectorAll('[data-artifact]').forEach((button) => {
      button.addEventListener('click', () => openArtifact(button.dataset.artifact));
    });
    timeline.querySelectorAll('[data-why]').forEach((link) => {
      link.addEventListener('click', () => {
        // Open the turn this link sits on, not whichever turn is newest.
        state.whyWant = link.dataset.why || null;
        state.why = null;
        state.whyFor = null;
        openPanel('why');
      });
    });
  }

  let anchored = true;
  function maybeScroll() {
    const scroll = $('scroll');
    if (!anchored) return;
    scroll.scrollTop = scroll.scrollHeight;
  }

  // ─────────────────────────────────────────────────────────────── actions ──

  async function decide(approvalId, decision) {
    if (!state.active) return;
    const key = `${state.active}:${approvalId}`;
    if (state.settling.has(key)) return;
    state.settling.add(key);
    scheduleRender();
    try {
      await api(`/api/sessions/${encodeURIComponent(state.active)}/approvals/${encodeURIComponent(approvalId)}`, {
        method: 'POST',
        body: { v: 1, decision },
      });
      toast(decision === 'deny' ? '已拒绝；这一轮会带着「没有派发」继续' : '已允许；正在从停下的地方继续');
      state.running = true;
      state.forceRunningUntil = Date.now() + 15_000;
      state.waiting = false;
      updateHeader();
    } catch (error) {
      toast(error.message || '审批没有记录成功', 'error');
    } finally {
      state.settling.delete(key);
      scheduleRender();
      scheduleSessionsRefresh();
    }
  }

  async function send() {
    const input = $('input');
    const text = input.value.trim();
    if (!text || !state.token) return;
    if (state.waiting) {
      toast('先回答上面的审批；这一轮会从那里继续', 'error');
      return;
    }
    composerError('');
    if (!state.active) newSession();

    if (state.running) {
      try {
        await api(`/api/sessions/${encodeURIComponent(state.active)}/steer`, {
          method: 'POST',
          body: { v: 1, text },
        });
        input.value = '';
        resizeInput();
        toast('已插话；会在下一个步骤边界生效');
      } catch (error) {
        toast(error.message || '插话没有送进这一轮', 'error');
      }
      return;
    }

    const session = state.active;
    state.pendingUser = text;
    state.running = true;
    state.forceRunningUntil = Date.now() + 15_000;
    input.value = '';
    resizeInput();
    updateHeader();
    scheduleRender();
    try {
      await api(`/api/sessions/${encodeURIComponent(session)}/turns`, {
        method: 'POST',
        body: { v: 1, text },
      });
      state.waiting = false;
      if (state.active === session) {
        connectStream(session, 0);
        scheduleSessionsRefresh();
        setTimeout(refreshSessions, 1600);
        // A turn that cannot even assemble leaves no durable item. If the
        // stream also could not be opened, the page would otherwise show a
        // forever-running turn. Say so once, without pretending to know why.
        setTimeout(() => {
          if (state.active === session && state.running && state.items.size === 0) {
            state.running = false;
            state.pendingUser = '';
            scheduleRender();
            updateHeader();
            toast('没有收到这一轮的任何事件。看看终端里的服务是否报了装配错误。', 'error');
          }
        }, 12_000);
      }
    } catch (error) {
      state.pendingUser = '';
      state.running = false;
      state.forceRunningUntil = 0;
      scheduleRender();
      composerError(error.message || '这一轮没有发出去');
      if (error.code === 'awaiting_decision') state.waiting = true;
      if (error.code === 'turn_in_progress') state.running = true;
    }
  }

  async function downloadExport(format) {
    if (!state.active || !state.token) return;
    try {
      const response = await fetch(
        `/api/sessions/${encodeURIComponent(state.active)}/export?format=${format}`,
        { headers: { Authorization: `Bearer ${state.token}` } },
      );
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        throw new Error(body.message || `导出失败（HTTP ${response.status}）`);
      }
      const blob = await response.blob();
      const url = URL.createObjectURL(blob);
      const link = document.createElement('a');
      link.href = url;
      link.download = `${state.active}.${format === 'json' ? 'json' : 'md'}`;
      document.body.appendChild(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 30_000);
      toast(`已导出 ${format === 'json' ? 'JSON' : 'Markdown'}`);
    } catch (error) {
      toast(error.message || '导出失败', 'error');
    }
  }

  async function openArtifact(locator) {
    // Reserve the tab before awaiting: once the fetch awaits, the click's user
    // activation is gone and window.open is blocked.
    let tab = null;
    try { tab = window.open('', '_blank'); } catch { tab = null; }
    const abandonTab = () => { if (tab) { try { tab.close(); } catch { /* already gone */ } tab = null; } };
    try {
      const response = await fetch(`/api/artifacts/${encodeURIComponent(locator)}`, {
        headers: { Authorization: `Bearer ${state.token}` },
      });
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        throw new Error(body.message || `artifact 读取失败（HTTP ${response.status}）`);
      }
      const blob = await response.blob();
      const url = URL.createObjectURL(blob);
      if (blob.type.startsWith('image/')) {
        abandonTab();
        if (state.artifactUrl) URL.revokeObjectURL(state.artifactUrl);
        state.artifactUrl = url;
        $('artifactImage').src = url;
        $('artifactView').hidden = false;
        return;
      }
      // A tab pointed at a type the browser cannot render (a zip, an opaque
      // octet-stream) does not error: it stays blank. Only navigate to what a
      // tab can actually show.
      const displayable = blob.type.startsWith('image/') || blob.type.startsWith('text/')
        || blob.type === 'application/json' || blob.type === 'application/pdf';
      if (tab && displayable) {
        tab.location.href = url;
        tab.opener = null;
        setTimeout(() => URL.revokeObjectURL(url), 60_000);
        return;
      }
      // Popup blocked, or the bytes are not something a tab can show: hand the
      // bytes over as a download instead of a dead tab.
      const blocked = !tab;
      abandonTab();
      const link = document.createElement('a');
      link.href = url;
      link.download = 'artifact';
      document.body.appendChild(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
      toast(blocked
        ? '浏览器拦住了新标签页，已改为下载 artifact'
        : `这种 artifact（${blob.type || '未知类型'}）不能在标签页里显示，已下载`);
    } catch (error) {
      abandonTab();
      toast(error.message || 'artifact 打不开', 'error');
    }
  }

  function closeArtifact() {
    $('artifactView').hidden = true;
    $('artifactImage').removeAttribute('src');
    if (state.artifactUrl) URL.revokeObjectURL(state.artifactUrl);
    state.artifactUrl = '';
  }

  async function forkSession() {
    if (!state.active) return;
    const reason = window.prompt('分叉原因（可留空）', 'user fork');
    if (reason === null) return;
    try {
      const result = await api(`/api/sessions/${encodeURIComponent(state.active)}/fork`, {
        method: 'POST',
        body: { v: 1, reason: reason.trim() || 'user fork' },
      });
      if (!result.forked || !result.session_id) {
        toast('没有可分叉的已闭合回合边界', 'error');
        return;
      }
      toast(`已分叉出 ${result.session_id}`);
      await refreshSessions();
      await openSession(result.session_id);
    } catch (error) {
      toast(error.message || '分叉失败', 'error');
    }
  }

  async function resumeExpired() {
    if (!state.active) return;
    try {
      await api(`/api/sessions/${encodeURIComponent(state.active)}/resume`, {
        method: 'POST',
        body: { v: 1 },
      });
      state.running = true;
      state.forceRunningUntil = Date.now() + 15_000;
      updateHeader();
      toast('已请求继续；内核会把过期审批记为「没有派发」');
      scheduleSessionsRefresh();
    } catch (error) {
      toast(error.message || '续跑请求没有送达', 'error');
    }
  }

  async function stop() {
    if (!state.active) return;
    try {
      const result = await api(`/api/sessions/${encodeURIComponent(state.active)}/interrupt`, {
        method: 'POST',
        body: { v: 1 },
      });
      // The kernel says whether anything was actually queued; don't promise a
      // stop the turn will never see.
      if (result && result.interrupting) {
        toast('已请求停止；会在下一个步骤边界生效');
      } else {
        toast((result && result.message) || '这一轮没有可打断的步骤，这次停止没有生效', 'error');
      }
    } catch (error) {
      toast(error.message || '停止请求没有送达', 'error');
    }
  }

  function toggleSide(open) {
    document.body.classList.toggle('side-open', open);
    const button = $('sideToggle');
    if (button) button.setAttribute('aria-expanded', String(open));
  }

  function newSession() {
    toggleSide(false);
    if (state.controller) state.controller.abort();
    state.streamGeneration += 1;
    const now = new Date();
    const stamp = [
      now.getFullYear(),
      String(now.getMonth() + 1).padStart(2, '0'),
      String(now.getDate()).padStart(2, '0'),
      String(now.getHours()).padStart(2, '0'),
      String(now.getMinutes()).padStart(2, '0'),
      String(now.getSeconds()).padStart(2, '0'),
    ].join('');
    const random = Math.random().toString(36).slice(2, 6);
    state.active = `web-${stamp}-${random}`;
    state.activeSummary = null;
    state.items.clear();
    state.head = 0;
    state.live = { text: '', reasoning: '' };
    state.pendingUser = '';
    state.running = false;
    state.waiting = false;
    state.forceRunningUntil = 0;
    state.why = null;
    state.whyFor = null;
    state.whyWant = null;
    state.diff = null;
    state.diffFor = null;
    state.context = null;
    state.contextFor = null;
    state.plan = null;
    state.planFor = null;
    state.subagents = null;
    state.subagentsFor = null;
    state.schedules = null;
    state.schedulesFor = null;
    state.pendingUser = '';
    state.connection = 'closed';
    localStorage.setItem(STORE.session, state.active);
    connection('idle', '未开始');
    updateHeader();
    renderSessions();
    renderTimeline();
    $('empty').hidden = false;
    $('empty').querySelector('h2').textContent = '新任务';
    $('sessionTag').title = state.active;
    $('input').focus();
    if (document.body.classList.contains('panel-open')) renderContext();
  }

  // ────────────────────────────────────────────────────────────── sessions ──

  async function openSession(id) {
    if (!id) return;
    toggleSide(false);
    if (state.active === id && state.items.size) {
      return;
    }
    if (state.controller) state.controller.abort();
    state.streamGeneration += 1;
    state.active = id;
    state.activeSummary = state.sessions.find((row) => row.id === id) || null;
    state.items.clear();
    state.head = 0;
    state.live = { text: '', reasoning: '' };
    state.pendingUser = '';
    state.running = Boolean(state.activeSummary?.open_turn) && !state.activeSummary?.yielded;
    state.waiting = Boolean(state.activeSummary?.yielded) || Number(state.activeSummary?.pending_approvals || 0) > 0;
    state.forceRunningUntil = 0;
    state.why = null;
    state.whyFor = null;
    state.whyWant = null;
    state.diff = null;
    state.diffFor = null;
    state.context = null;
    state.contextFor = null;
    state.plan = null;
    state.planFor = null;
    state.subagents = null;
    state.subagentsFor = null;
    state.schedules = null;
    state.schedulesFor = null;
    localStorage.setItem(STORE.session, id);
    updateHeader();
    renderSessions();
    $('empty').hidden = false;
    $('empty').querySelector('h2').textContent = '正在读取…';
    $('timeline').innerHTML = '';
    connection('idle', '连接中');

    let payload;
    try {
      payload = await api(`/api/sessions/${encodeURIComponent(id)}/transcript`);
    } catch (error) {
      if (error.status === 404) {
        $('empty').querySelector('h2').textContent = '这个会话还没有日志';
        toast('服务端还没有这个会话；如果它是刚建的，先发一句话。', 'error');
      } else {
        $('empty').querySelector('h2').textContent = '读不出来';
        toast(error.message || '读取会话失败', 'error');
      }
      return;
    }
    if (state.active !== id) return;
    state.head = Number(payload.head || 0);
    state.items.clear();
    for (const item of payload.items || []) state.items.set(item.id, item);
    const summary = state.sessions.find((row) => row.id === id);
    if (summary) applySummary(summary);
    updateHeader();
    renderTimeline();
    $('empty').hidden = state.items.size > 0;
    connectStream(id, 0);
    if (document.body.classList.contains('panel-open')) loadWhy();
  }

  // ──────────────────────────────────────────────────────────────── panel ──

  function openPanel(tab = 'why') {
    document.body.classList.add('panel-open');
    $(`panelBtn`).classList.add('on');
    showTab(tab);
    if (tab === 'diff') loadDiff();
    if (tab === 'why') loadWhy();
    if (tab === 'plan') loadPlan();
    if (tab === 'sub') loadSubagents();
    if (tab === 'sched') loadSchedules();
    if (tab === 'ctx') loadContext();
    syncDocHalf();
  }

  function closePanel() {
    document.body.classList.remove('panel-open');
    $('panelBtn').classList.remove('on');
    syncDocHalf();
  }

  function showTab(name) {
    document.querySelectorAll('.p-tab').forEach((tab) => tab.classList.toggle('on', tab.dataset.pv === name));
    document.querySelectorAll('.pv').forEach((panel) => panel.classList.toggle('on', panel.id === `pv-${name}`));
    root.dataset.panel = name;
    if (name === 'diff') loadDiff();
    if (name === 'why') loadWhy();
    if (name === 'plan') loadPlan();
    if (name === 'sub') loadSubagents();
    if (name === 'sched') loadSchedules();
    if (name === 'ctx') loadContext();
  }

  function diffLines(patch) {
    const out = [];
    let oldLine = 0;
    let newLine = 0;
    for (const line of String(patch || '').split('\n')) {
      const hunk = line.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
      if (hunk) {
        oldLine = Number(hunk[1]);
        newLine = Number(hunk[2]);
        out.push(`<div class="dl hk"><span class="ln">···</span><span class="cd">${esc(line)}</span></div>`);
        continue;
      }
      if (line.startsWith('+++') || line.startsWith('---') || line.startsWith('diff --git')) {
        out.push(`<div class="dl hk"><span class="ln"></span><span class="cd">${esc(line)}</span></div>`);
        continue;
      }
      if (line.startsWith('+')) {
        out.push(`<div class="dl add"><span class="ln">${newLine || ''}</span><span class="cd">${esc(line)}</span></div>`);
        newLine += 1;
        continue;
      }
      if (line.startsWith('-')) {
        out.push(`<div class="dl del"><span class="ln">${oldLine || ''}</span><span class="cd">${esc(line)}</span></div>`);
        oldLine += 1;
        continue;
      }
      out.push(`<div class="dl"><span class="ln">${newLine || ''}</span><span class="cd">${esc(line)}</span></div>`);
      oldLine += 1;
      newLine += 1;
    }
    return out.join('');
  }

  function renderDiff(data) {
    const diff = data?.diff || {};
    const files = Array.isArray(diff.files) ? diff.files : [];
    const count = Number(diff.file_count ?? files.length);
    if (diff.raw) {
      $('diffBody').innerHTML = `<pre class="patch">${esc(diff.raw)}</pre>`;
      return;
    }
    $('diffTabCount').textContent = count ? String(count) : '';
    if (!files.length) {
      $('diffBody').innerHTML = `
        <div class="why-empty">
          最近一次 <b>git.diff</b> 没有文件。<br>
          工作树可能是干净的，或 <code>path</code> 过滤没有命中。
        </div>`;
      return;
    }
    const add = Number(diff.additions || 0);
    const del = Number(diff.deletions || 0);
    const out = [`
      <div class="diff-sum">
        <span><b>${count}</b> 个文件</span>
        <span class="add">+${add}</span>
        <span class="del">−${del}</span>
        <span class="src">来自 ${esc(data.turn ? `第 ${turnNumber(data.turn)} 轮` : '最近一次')} · git.diff</span>
      </div>`];
    files.forEach((file, index) => {
      out.push(`
        <details class="diff-file" ${index === 0 ? 'open' : ''}>
          <summary>
            <span class="chev">›</span>
            <span class="fp">${esc(file.file || '(unknown)')}</span>
            <span class="meta">${esc(file.status || '')} <span class="add">+${Number(file.additions || 0)}</span> <span class="del">−${Number(file.deletions || 0)}</span></span>
          </summary>
          <div class="patch">${diffLines(file.patch || '')}</div>
        </details>`);
    });
    if (diff.truncated) {
      out.push('<div class="why-empty">这次 diff 被工具输出上限截断，后面的文件没有显示。</div>');
    }
    $('diffBody').innerHTML = out.join('');
  }

  function turnNumber(turn) {
    const { groups } = groupTurns();
    const index = groups.findIndex((group) => group.turn === turn);
    return index >= 0 ? index + 1 : 0;
  }

  async function loadDiff() {
    if (!state.active) {
      $('diffBody').innerHTML = '<div class="why-empty">先选一个会话。</div>';
      return;
    }
    if (state.diffFor === state.active && state.diff) {
      renderDiff(state.diff);
      return;
    }
    $('diffBody').innerHTML = '<div class="why-empty">正在读取最近一次 git.diff…</div>';
    try {
      const data = await api(`/api/sessions/${encodeURIComponent(state.active)}/diff`);
      if (state.active !== data.session) return;
      state.diff = data;
      state.diffFor = data.session;
      renderDiff(data);
    } catch (error) {
      state.diff = null;
      state.diffFor = null;
      $('diffTabCount').textContent = '';
      if (error.status === 404) {
        $('diffBody').innerHTML = `
          <div class="why-empty">
            这个会话还没有 <b>git.diff</b> 观察。<br>
            模型调用过 <code>git.diff</code> 之后，这里会显示它当时真正看到的改动——
            不是页面现跑一次 git。
          </div>`;
      } else {
        $('diffBody').innerHTML = `<div class="why-empty">读不出来：${esc(error.message || '未知错误')}</div>`;
      }
    }
  }

  async function loadWhy() {
    if (!state.active) {
      $('whyBody').innerHTML = '<div class="why-empty">先选一个会话。</div>';
      return;
    }
    const { groups } = groupTurns();
    const latest = groups[groups.length - 1];
    const turn = state.whyWant || latest?.turn || state.activeSummary?.open_turn || null;
    if (!turn) {
      $('whyBody').innerHTML = '<div class="why-empty">这一轮还没有写推理链。<br>推理链由内核在运行中记录；有答案之后再看。</div>';
      return;
    }
    if (state.whyFor === turn && state.why) {
      return;
    }
    $('whyBody').innerHTML = '<div class="why-empty">正在读取推理链…</div>';
    try {
      const data = await api(`/api/sessions/${encodeURIComponent(state.active)}/why${turn ? `?turn=${encodeURIComponent(turn)}` : ''}`);
      if (state.active && data.turn) {
        state.why = data;
        state.whyFor = data.turn;
        renderWhy(data);
        const index = groups.findIndex((group) => String(group.turn) === String(data.turn));
        $('panelTitle').textContent = index >= 0 ? `第 ${index + 1} 轮 · 推理链` : '推理链';
      }
    } catch (error) {
      state.why = null;
      state.whyFor = null;
      if (error.status === 404) {
        $('whyBody').innerHTML = `<div class="why-empty">这一轮还没有推理链。<br><b>不是所有回合都有</b>：旧日志或未写链的运行会没有，这是正常的。</div>`;
      } else {
        $('whyBody').innerHTML = `<div class="why-empty">读不出来：${esc(error.message || '未知错误')}</div>`;
      }
    }
  }

  function renderWhy(data) {
    const kindLabel = {
      situation: '情境', reading: '读法', stance: '立场', action: '动作',
      observation: '观察', gate: '闸门', verdict: '判定', switch: '切换',
    };
    const byId = new Map((data.nodes || []).map((node) => [node.id, node]));
    const depthOf = (node) => {
      let depth = 0;
      let cursor = node;
      const seen = new Set();
      while (cursor && depth < 3) {
        const basis = (cursor.basis || []).find((entry) => entry.node && byId.has(entry.node));
        if (!basis || seen.has(basis.node)) break;
        seen.add(basis.node);
        cursor = byId.get(basis.node);
        depth += 1;
      }
      return depth;
    };
    const out = [];
    if (data.verdict) {
      out.push(`
        <div class="verdict">
          <div class="l">判 定</div>
          <div class="t">${md(data.verdict.claim || '')}</div>
          <div class="why-empty" style="margin-top:3px;font-family:var(--mono);font-size:9px">${esc(data.verdict.rule || '')}</div>
        </div>`);
    }
    for (const node of data.nodes || []) {
      const depth = depthOf(node);
      const basis = (node.basis || []).map((entry) => entry.fact || entry.event || (entry.node ? `节点 ${entry.node}` : '')).filter(Boolean).join(' · ');
      out.push(`
        <div class="node ${depth === 1 ? 'l2' : depth >= 2 ? 'l3' : ''}">
          <span class="g">${depth >= 2 ? '└' : '├'}</span>
          <div>
            <div class="r1"><span class="kind ${node.kind === 'verdict' ? 'v' : ''}">${esc(kindLabel[node.kind] || node.kind || '')}</span></div>
            <div class="tx">${esc(node.claim || '')}</div>
            ${basis ? `<div class="ct">${esc(truncate(basis, 160))}</div>` : node.rule ? `<div class="ct">${esc(node.rule)}</div>` : ''}
          </div>
        </div>`);
    }
    if (!out.length) out.push('<div class="why-empty">这一轮没有可读的节点。</div>');
    const check = data.check && data.check !== 'ok'
      ? `<div class="why-empty" style="color:var(--red);margin-top:8px">链的自检没有全过：${esc(data.check)}</div>`
      : '';
    out.push(`<div class="why-note">从判定往下读：节点指向它据以成立的原话、观察或规则。链不进模型上下文，倒读不花 token。</div>${check}`);
    $('whyBody').innerHTML = out.join('');
  }

  function currentUsage() {
    const groups = groupTurns().groups;
    for (let i = groups.length - 1; i >= 0; i -= 1) {
      const usage = groups[i].end?.usage;
      if (usage) return { usage, turn: groups[i].turn, steps: groups[i].actions.length };
    }
    return { usage: state.activeSummary?.usage || null, turn: null, steps: null };
  }

  async function loadSubagents() {
    if (!state.active) {
      $('subBody').innerHTML = '<div class="why-empty">先选一个会话。</div>';
      return;
    }
    if (state.subagentsFor === state.active && state.subagents) {
      renderSubagents(state.subagents);
      return;
    }
    $('subBody').innerHTML = '<div class="why-empty">正在读取子代理…</div>';
    try {
      const data = await api(`/api/sessions/${encodeURIComponent(state.active)}/subagents`);
      if (state.active !== data.session) return;
      state.subagents = data;
      state.subagentsFor = data.session;
      renderSubagents(data);
    } catch (error) {
      state.subagents = null;
      state.subagentsFor = null;
      if (error.status === 404) {
        $('subBody').innerHTML = `
          <div class="why-empty">这个会话还没有子代理。<br>
          模型调用 <code>subagent.run</code> 时，父日志会记下 fork 出来的子会话。</div>`;
      } else {
        $('subBody').innerHTML = `<div class="why-empty">读不出来：${esc(error.message || '未知错误')}</div>`;
      }
    }
  }

  function renderSubagents(data) {
    const children = Array.isArray(data.children) ? data.children : [];
    $('panelTitle').textContent = '会话 · 子代理';
    if (!children.length) {
      $('subBody').innerHTML = '<div class="why-empty">没有委派记录。</div>';
      return;
    }
    const out = [`<div class="sub-count">${children.length} 个子代理</div>`];
    for (const child of children) {
      const status = child.status || 'unknown';
      const known = state.sessions.some((row) => row.id === child.child_session_id);
      out.push(`
        <div class="sub-row">
          <div class="sub-top">
            <span class="sub-dot ${esc(status)}"></span>
            <span class="sub-provider">${esc(child.provider || 'subagent')}</span>
            <span class="sub-status">${esc(status)}</span>
          </div>
          <div class="sub-objective">${esc(child.objective || '（没有目标）')}</div>
          ${child.child_session_id
            ? `<button class="sub-link" data-child="${esc(child.child_session_id)}" ${known ? '' : 'disabled'}>${esc(child.child_session_id)}</button>`
            : ''}
          ${child.outcome ? `<div class="ct">${esc(truncate(pretty(child.outcome), 200))}</div>` : ''}
        </div>`);
    }
    out.push('<div class="why-note">子代理是 parent 日志里的一条委派事实；点会话 id 会打开子会话。<br>子代理不继承父会话的权限或工具。</div>');
    $('subBody').innerHTML = out.join('');
    document.querySelectorAll('#subBody [data-child]').forEach((button) => {
      button.addEventListener('click', () => {
        if (button.disabled) return;
        openSession(button.dataset.child);
      });
    });
  }

  async function loadSchedules() {
    if (!state.active) {
      $('schedBody').innerHTML = '<div class="why-empty">先选一个会话。</div>';
      return;
    }
    $('schedBody').innerHTML = '<div class="why-empty">正在读取定时条目…</div>';
    try {
      const data = await api(`/api/sessions/${encodeURIComponent(state.active)}/schedules`);
      if (state.active !== data.session) return;
      state.schedules = data;
      state.schedulesFor = data.session;
      renderSchedules(data);
    } catch (error) {
      $('schedBody').innerHTML = `<div class="why-empty">读不出来：${esc(error.message || '未知错误')}</div>`;
    }
  }

  function renderSchedules(data) {
    const items = Array.isArray(data.items) ? data.items : [];
    $('panelTitle').textContent = '会话 · 定时';
    const out = [`
      <div class="sched-form">
        <input id="schedWhen" placeholder="+10m 或 RFC3339" aria-label="什么时候运行">
        <input id="schedText" placeholder="到点要说什么" aria-label="定时内容">
        <button class="btn" id="schedAdd">排定</button>
      </div>`];
    if (!items.length) {
      out.push('<div class="why-empty">没有待运行的定时条目。</div>');
    } else {
      out.push('<div class="sched-list">');
      for (const item of items) {
        out.push(`
          <div class="sched-row ${item.overdue ? 'overdue' : ''}">
            <div class="sched-when" title="${esc(item.due_at || '')}">${esc(whenText(item.due_at) || '时间不可读')}${item.overdue ? ' · 已过期' : ''}</div>
            <div class="sched-text">${esc(item.text || '')}</div>
            <button class="sub-link" data-cancel-schedule="${esc(item.item_id)}" title="取消 ${esc(item.item_id)}">取消</button>
          </div>`);
      }
      out.push('</div>');
    }
    out.push('<div class="why-note">定时条目在 Inbox 里，和 `agent schedule` 是同一份；Web 只负责排队和撤回，真正运行仍由 `tick` / `serve` 驱动。</div>');
    $('schedBody').innerHTML = out.join('');
    $('schedAdd').addEventListener('click', createSchedule);
    document.querySelectorAll('[data-cancel-schedule]').forEach((button) => {
      button.addEventListener('click', () => cancelSchedule(button.dataset.cancelSchedule));
    });
  }

  async function createSchedule() {
    if (!state.active) return;
    const when = $('schedWhen').value.trim();
    const text = $('schedText').value.trim();
    if (!when || !text) {
      toast('时间和内容都要填', 'error');
      return;
    }
    try {
      await api(`/api/sessions/${encodeURIComponent(state.active)}/schedules`, {
        method: 'POST',
        body: { v: 1, when, text },
      });
      toast('已排定');
      state.schedules = null;
      await loadSchedules();
    } catch (error) {
      toast(error.message || '排定失败', 'error');
    }
  }

  async function cancelSchedule(itemId) {
    try {
      await api(
        `/api/sessions/${encodeURIComponent(state.active)}/schedules/${encodeURIComponent(itemId)}`,
        { method: 'DELETE' },
      );
      toast('已取消');
      state.schedules = null;
      await loadSchedules();
    } catch (error) {
      toast(error.message || '取消失败', 'error');
    }
  }

  async function loadPlan() {
    if (!state.active) {
      $('planBody').innerHTML = '<div class="why-empty">先选一个会话。</div>';
      return;
    }
    if (state.planFor === state.active && state.plan) {
      renderPlan(state.plan);
      return;
    }
    $('planBody').innerHTML = '<div class="why-empty">正在读取计划…</div>';
    try {
      const data = await api(`/api/sessions/${encodeURIComponent(state.active)}/plan`);
      if (state.active !== data.session) return;
      state.plan = data;
      state.planFor = data.session;
      renderPlan(data);
    } catch (error) {
      state.plan = null;
      state.planFor = null;
      if (error.status === 404) {
        $('planBody').innerHTML = `
          <div class="why-empty">这个会话还没有计划。<br>
          模型调用 <code>plan.update</code> 之后，这里会显示目标、步骤和已确认的事实。</div>`;
      } else {
        $('planBody').innerHTML = `<div class="why-empty">读不出来：${esc(error.message || '未知错误')}</div>`;
      }
    }
  }

  function renderPlan(data) {
    const plan = data.plan || {};
    const items = Array.isArray(plan.items) ? plan.items : [];
    const facts = Array.isArray(plan.facts) ? plan.facts : [];
    const done = items.filter((item) => item.status === 'done').length;
    $('panelTitle').textContent = '会话 · 计划';
    const out = [];
    out.push(`
      <div class="plan-head">
        <div class="plan-goal">${esc(plan.goal || '（没有写目标）')}</div>
        <div class="plan-meta">${done}/${items.length} 完成${facts.length ? ` · ${facts.length} 条事实` : ''}</div>
      </div>`);
    if (!items.length) {
      out.push('<div class="why-empty">计划里还没有步骤。</div>');
    } else {
      out.push('<div class="plan-list">');
      items.forEach((item) => {
        const status = item.status || 'todo';
        out.push(`
          <div class="plan-item ${esc(status)}">
            <span class="plan-mark">${status === 'done' ? '✓' : status === 'doing' ? '·' : '○'}</span>
            <span class="plan-text">${esc(item.text || '')}</span>
          </div>`);
      });
      out.push('</div>');
    }
    if (facts.length) {
      out.push('<div class="ctx-sub">已确认的事实</div><ul class="plan-facts">');
      for (const fact of facts) out.push(`<li>${esc(fact)}</li>`);
      out.push('</ul>');
    }
    out.push('<div class="why-note">这是模型通过 <code>plan.update</code> 写进日志的账本；页面不编辑、不推断。<br>计划在每一步开始时作为任务层进入它的上下文。</div>');
    $('planBody').innerHTML = out.join('');
  }

  async function loadContext() {
    if (!state.active) {
      $('ctxBody').innerHTML = '<div class="why-empty">先选一个会话。</div>';
      return;
    }
    if (state.contextFor === state.active && state.context) {
      renderContext(state.context);
      return;
    }
    $('ctxBody').innerHTML = '<div class="why-empty">正在读取最近一次模型请求…</div>';
    try {
      const data = await api(`/api/sessions/${encodeURIComponent(state.active)}/context`);
      if (state.active !== data.session) return;
      state.context = data;
      state.contextFor = data.session;
      renderContext(data);
    } catch (error) {
      state.context = null;
      state.contextFor = null;
      if (error.status === 404) {
        renderContext(null);
      } else {
        $('ctxBody').innerHTML = `<div class="why-empty">读不出来：${esc(error.message || '未知错误')}</div>`;
      }
    }
  }

  function renderContext(data) {
    if (!data) {
      // A session that has not prepared a request yet still has a durable
      // provider usage from a completed answer in older logs. Say what is
      // available rather than inventing a category table.
      const { usage, turn } = currentUsage();
      $('panelTitle').textContent = turn ? '本轮 · 用量' : '会话 · 用量';
      if (!usage) {
        $('ctxBody').innerHTML = `
          <div class="why-empty">这个会话还没有记录过模型请求。<br>
          跑完第一步之后，这里会显示 /context 的分类预算。</div>`;
        return;
      }
      const input = Number(usage.input_tokens || 0);
      const output = Number(usage.output_tokens || 0);
      const total = Number(usage.total_tokens ?? input + output);
      $('ctxBody').innerHTML = `
        <div class="ctxrow"><span class="l">输入</span><span class="b"><i style="width:100%"></i></span><span class="v">${fmtTokens(input)}</span></div>
        <div class="ctxrow"><span class="l">输出</span><span class="b"><i style="width:100%"></i></span><span class="v">${fmtTokens(output)}</span></div>
        <div class="why-empty" style="margin:10px 0 4px">最近一次模型用量 ${fmtTokens(total)} tokens；这个旧会话没有 prepared request，所以没有分项。</div>`;
      return;
    }

    const budget = Number(data.budget?.max_input_tokens || state.meta?.budget || 0);
    const used = Number(data.used || 0);
    const ceiling = budget || Math.max(used, 1);
    $('panelTitle').textContent = '本轮 · 上下文';
    const row = (label, value, note = '') => `
      <div class="ctxrow">
        <span class="l">${esc(label)}</span>
        <span class="b"><i style="width:${Math.max(2, Math.min(100, (value / ceiling) * 100)).toFixed(0)}%"></i></span>
        <span class="v">${fmtTokens(value)}</span>
      </div>${note ? `<div class="why-empty" style="margin:-3px 0 7px 64px;font-size:10px">${esc(note)}</div>` : ''}`;

    const out = [];
    for (const category of data.categories || []) {
      out.push(row(category.name || '其它', Number(category.tokens || 0)));
    }
    const dropped = Number(data.messages?.dropped || 0);
    out.push(`
      <div class="why-empty" style="margin:10px 0 4px">
        已用 ${fmtTokens(used)}${budget ? ` / 预算 ${fmtTokens(budget)}` : ''}
        ${data.budget?.reserved_output_tokens ? ` · 预留输出 ${fmtTokens(data.budget.reserved_output_tokens)}` : ''}<br>
        ${data.messages?.count != null ? `请求 ${data.messages.count} 条消息` : ''}
        ${dropped ? ` · 裁掉 ${dropped} 条` : ''}
        ${data.tools?.count != null ? ` · ${data.tools.count} 个工具定义` : ''}
      </div>`);

    const usage = data.usage;
    if (usage) {
      out.push('<div class="ctx-sub">最近一次提供方用量</div>');
      out.push(row('输入', Number(usage.input_tokens || 0)));
      out.push(row('输出', Number(usage.output_tokens || 0)));
      out.push(row('缓存命中', Number(usage.cache_hit_tokens || 0)));
      out.push(row('推理', Number(usage.reasoning_tokens || 0)));
    }

    const layer = (name, value) => value
      ? `<details class="ctx-layer"><summary>${name} · ${fmtTokens(value.chars || 0)} 字符</summary><pre>${esc(value.preview || '')}</pre></details>`
      : '';
    const layerHtml = layer('状态层', data.layers?.state) + layer('任务层', data.layers?.task);
    if (layerHtml) out.push(`<div class="ctx-sub">本轮层</div>${layerHtml}`);

    out.push('<div class="why-note">读的是日志里最近一次 <code>model/request.prepared</code>，不重新估算、不重新请求模型。分类数字来自请求自己记录的估算；没有记录的项不显示成 0。</div>');
    $('ctxBody').innerHTML = out.join('');
  }

  // ─────────────────────────────────────────────────────────────── layout ──

  function applyLayout() {
    const side = localStorage.getItem(STORE.side);
    const panel = localStorage.getItem(STORE.panel);
    const doc = localStorage.getItem(STORE.doc);
    const composer = localStorage.getItem(STORE.composer);
    if (side) root.style.setProperty('--side-w', `${side}px`);
    if (panel) root.style.setProperty('--panel-w', `${panel}px`);
    if (doc) root.style.setProperty('--doc-w', `${doc}px`);
    if (composer) $('input').style.height = `${composer}px`;
    syncDocHalf();
  }

  function syncDocHalf() {
    const col = $('col');
    if (!col) return;
    root.style.setProperty('--doc-half', `${col.getBoundingClientRect().width / 2}px`);
  }

  function setTheme(theme) {
    root.dataset.theme = theme;
    $('themeLabel').textContent = theme === 'dark' ? '浅色' : '深色';
    $('themeIcon').textContent = theme === 'dark' ? '☀' : '☾';
    localStorage.setItem(STORE.theme, theme);
  }

  function startDrag(event, onMove, axis = 'x') {
    event.preventDefault();
    const sx = event.clientX;
    const sy = event.clientY;
    document.body.classList.add(axis === 'y' ? 'resizing-y' : 'resizing');
    const move = (moveEvent) => onMove(moveEvent.clientX - sx, moveEvent.clientY - sy);
    const up = () => {
      document.body.classList.remove('resizing', 'resizing-y');
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  }

  function bindResizers() {
    const clamp = (value, min, max) => Math.min(max, Math.max(min, value));

    $('rzSide').addEventListener('pointerdown', (event) => {
      const start = document.querySelector('.side').getBoundingClientRect().width;
      startDrag(event, (dx) => {
        const width = clamp(start + dx, 196, 340);
        root.style.setProperty('--side-w', `${width}px`);
        localStorage.setItem(STORE.side, String(Math.round(width)));
      });
    });
    $('rzSide').addEventListener('dblclick', () => {
      root.style.removeProperty('--side-w');
      localStorage.removeItem(STORE.side);
    });

    $('rzPanel').addEventListener('pointerdown', (event) => {
      if (!document.body.classList.contains('panel-open')) return;
      const start = $('panel').getBoundingClientRect().width;
      startDrag(event, (dx) => {
        const width = clamp(start - dx, 300, 560);
        root.style.setProperty('--panel-w', `${width}px`);
        localStorage.setItem(STORE.panel, String(Math.round(width)));
      });
    });
    $('rzPanel').addEventListener('dblclick', () => {
      root.style.removeProperty('--panel-w');
      localStorage.removeItem(STORE.panel);
    });

    $('rzDoc').addEventListener('pointerdown', (event) => {
      const start = $('col').getBoundingClientRect().width;
      const available = $('scroll').getBoundingClientRect().width;
      startDrag(event, (dx) => {
        const width = clamp(start + dx * 2, 560, Math.min(1040, available - 48));
        root.style.setProperty('--doc-w', `${width}px`);
        localStorage.setItem(STORE.doc, String(Math.round(width)));
        syncDocHalf();
      });
    });
    $('rzDoc').addEventListener('dblclick', () => {
      root.style.removeProperty('--doc-w');
      localStorage.removeItem(STORE.doc);
      syncDocHalf();
    });

    $('grip').addEventListener('pointerdown', (event) => {
      const start = $('input').getBoundingClientRect().height;
      startDrag(event, (dx, dy) => {
        const height = clamp(start - dy, 46, 220);
        $('input').style.height = `${height}px`;
        localStorage.setItem(STORE.composer, String(Math.round(height)));
      }, 'y');
    });
    $('grip').addEventListener('dblclick', () => {
      $('input').style.height = '';
      localStorage.removeItem(STORE.composer);
    });

    const nudge = (element, setter) => {
      element.addEventListener('keydown', (event) => {
        if (!['ArrowLeft', 'ArrowRight', 'Home'].includes(event.key)) return;
        event.preventDefault();
        if (event.key === 'Home') setter(null);
        else setter((Number(element.dataset.value) || 0) + (event.key === 'ArrowRight' ? 8 : -8));
      });
    };
    nudge($('rzSide'), (value) => {
      if (value == null) {
        root.style.removeProperty('--side-w');
        localStorage.removeItem(STORE.side);
      } else {
        const width = Math.min(340, Math.max(196, value));
        root.style.setProperty('--side-w', `${width}px`);
        localStorage.setItem(STORE.side, String(width));
      }
    });
    nudge($('rzPanel'), (value) => {
      if (value == null) {
        root.style.removeProperty('--panel-w');
        localStorage.removeItem(STORE.panel);
      } else {
        const width = Math.min(560, Math.max(300, value));
        root.style.setProperty('--panel-w', `${width}px`);
        localStorage.setItem(STORE.panel, String(width));
      }
    });

    new ResizeObserver(syncDocHalf).observe($('col'));
    syncDocHalf();
  }

  // ──────────────────────────────────────────────────────────────── input ──

  function resizeInput() {
    const input = $('input');
    input.style.height = 'auto';
    const height = Math.min(220, Math.max(46, input.scrollHeight));
    input.style.height = `${height}px`;
  }

  function bindInput() {
    const input = $('input');
    let composing = false;
    input.addEventListener('compositionstart', () => { composing = true; });
    input.addEventListener('compositionend', () => { composing = false; });
    input.addEventListener('input', resizeInput);
    input.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter' || event.shiftKey || composing || event.isComposing) return;
      event.preventDefault();
      send();
    });
    $('sendBtn').addEventListener('click', send);
    $('stopBtn').addEventListener('click', stop);
    $('modelOptions').addEventListener('click', (event) => {
      const button = event.target.closest('button[data-provider]');
      if (button) chooseModel(button.dataset.provider, button.dataset.model || null);
    });
    // A details element stays open until its summary is clicked again, which
    // would leave the menu over the composer after a click anywhere else.
    document.addEventListener('click', (event) => {
      const menu = $('modelMenu');
      if (menu && menu.open && !menu.contains(event.target)) menu.open = false;
    });
    $('newTask').addEventListener('click', newSession);
    $('sideToggle').addEventListener('click', () => toggleSide(!document.body.classList.contains('side-open')));
    $('sideBackdrop').addEventListener('click', () => toggleSide(false));
    $('search').addEventListener('input', (event) => {
      state.search = event.target.value;
      renderSessions();
    });
    $('themeBtn').addEventListener('click', () => {
      setTheme(root.dataset.theme === 'dark' ? 'light' : 'dark');
    });
    document.querySelectorAll('[data-export]').forEach((button) => {
      button.addEventListener('click', () => {
        $('exportMenu').open = false;
        downloadExport(button.dataset.export);
      });
    });
    $('forkBtn').addEventListener('click', forkSession);
    // The footer entry means "the latest turn"; only a per-turn link pins a turn.
    $('whyFoot').addEventListener('click', () => { state.whyWant = null; openPanel('why'); });
    $('panelBtn').addEventListener('click', () => {
      if (document.body.classList.contains('panel-open')) closePanel();
      else openPanel(root.dataset.panel || 'why');
    });
    $('panelClose').addEventListener('click', closePanel);
    $('artifactClose').addEventListener('click', closeArtifact);
    $('artifactView').addEventListener('click', (event) => {
      if (event.target === $('artifactView')) closeArtifact();
    });
    document.querySelectorAll('.p-tab').forEach((tab) => {
      tab.addEventListener('click', () => {
        if (tab.dataset.pv === 'why') state.whyWant = null;
        showTab(tab.dataset.pv);
      });
    });
    $('scroll').addEventListener('scroll', () => {
      const scroll = $('scroll');
      anchored = scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 80;
    });
    document.addEventListener('keydown', (event) => {
      if (event.ctrlKey && !event.altKey && !event.metaKey && event.key.toLowerCase() === 'n') {
        event.preventDefault();
        newSession();
      } else if (event.ctrlKey && !event.altKey && !event.metaKey && event.key.toLowerCase() === 'j') {
        event.preventDefault();
        setTheme(root.dataset.theme === 'dark' ? 'light' : 'dark');
      } else if (event.key === 'Escape' && !$('artifactView').hidden) {
        closeArtifact();
      } else if (event.key === 'Escape' && document.body.classList.contains('side-open')) {
        toggleSide(false);
      } else if (event.key === 'Escape' && document.body.classList.contains('panel-open')) {
        closePanel();
      }
    });
  }

  // ────────────────────────────────────────────────────────────────── boot ──

  async function boot() {
    setTheme(localStorage.getItem(STORE.theme) || (params.get('theme') === 'dark' ? 'dark' : 'light'));
    applyLayout();
    bindResizers();
    bindInput();
    // A pending approval can outlive the event that created it. Re-render the
    // countdown on a slow tick so the card changes to "已过期" even when no
    // durable event is arriving.
    setInterval(() => {
      const hasTimed = [...state.items.values()].some(
        (item) => item.kind === 'approval' && item.state === 'pending' && item.expires_at,
      );
      if (hasTimed) scheduleRender();
    }, 30_000);

    if (!state.token) {
      updateHeader();
      $('sessionTitle').textContent = '缺少令牌';
      $('empty').querySelector('h2').textContent = '这条地址没有令牌';
      $('empty').querySelector('p').innerHTML = '请从终端里复制 <kbd>agent web</kbd> 打印的完整地址（带 <kbd>#t=</kbd>）。';
      $('threads').innerHTML = '<div class="empty">没有令牌，读不到会话。</div>';
      connection('closed', '缺少令牌');
      return;
    }

    try {
      state.meta = await api('/api/meta');
      $('workspace').textContent = state.meta.workspace || '爻爻';
      $('wsTag').textContent = state.meta.sandbox || '';
      updateHeader();
    } catch (error) {
      connection('closed', '无法连接');
      $('sessionTitle').textContent = '连接失败';
      $('empty').querySelector('h2').textContent = '服务没有回答';
      $('empty').querySelector('p').textContent = error.message || '检查终端里的 URL 与令牌。';
      return;
    }

    await refreshSessions();
    if (!state.sessions.length) {
      $('empty').hidden = false;
      updateHeader();
      return;
    }

    const wanted = params.get('session') || localStorage.getItem(STORE.session);
    const first = state.sessions.find((row) => row.id === wanted) || state.sessions[0];
    if (first) await openSession(first.id);
  }

  boot();
})();
