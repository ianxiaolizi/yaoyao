import './styles/shell.css';
import {
  ApiError,
  TOKEN,
  archiveSession,
  cancelSchedule as cancelScheduleApi,
  chooseModel as chooseModelApi,
  createSchedule as createScheduleApi,
  decide,
  exportSession as exportSessionApi,
  fetchArtifact,
  forkSession,
  interrupt,
  loadContext,
  loadDiff,
  loadMeta,
  loadSchedules,
  loadSessions,
  loadSession,
  loadPlan,
  loadSubagents,
  loadTranscript,
  loadWhy,
  removeSession,
  resume,
  startTurn,
  steer,
  streamSession,
} from './api';
import { artifactLabel, artifactRefs } from './artifacts';
import { looksOffice, readOffice } from './office';
import type { ArtifactView, PanelKey, SessionSummary } from './types';
import { initialState, Store } from './state';
import { mount, renderAuth, type Actions } from './view';

const root = document.getElementById('app') as HTMLElement;
const store = new Store(initialState());
const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

let streamController: AbortController | null = null;
let generation = 0;
let artifactUrl: string | null = null;

function say(message: string, error = false): void {
  store.dispatch({ type: 'toast', toast: message });
  store.dispatch({ type: 'error', error: error ? message : null });
}

function newSessionId(): string {
  const now = new Date();
  const stamp = [
    now.getFullYear(),
    String(now.getMonth() + 1).padStart(2, '0'),
    String(now.getDate()).padStart(2, '0'),
    String(now.getHours()).padStart(2, '0'),
    String(now.getMinutes()).padStart(2, '0'),
    String(now.getSeconds()).padStart(2, '0'),
  ].join('');
  return `web-${stamp}-${Math.random().toString(36).slice(2, 6)}`;
}

async function refreshSessions(): Promise<void> {
  try {
    const sessions = await loadSessions();
    store.dispatch({ type: 'sessions/loaded', sessions });
  } catch (error) {
    say((error as Error).message || '会话列表没有读出来', true);
  }
}

async function refreshSummary(id: string): Promise<void> {
  try {
    const summary = await loadSession(id);
    if (store.get().active !== id) return;
    store.dispatch({ type: 'summary/loaded', summary });
  } catch {
    // the list refresh will catch up
  }
}

const panelLoaders: Record<Exclude<PanelKey, 'subagents' | 'why'>, (id: string) => Promise<unknown>> = {
  plan: loadPlan,
  diff: loadDiff,
  context: loadContext,
  schedules: loadSchedules,
};

async function subagentTree(id: string, depth = 2, seen = new Set<string>()): Promise<unknown> {
  // `depth` counts the generations *below* this one that still get read: depth 2
  // is the session and its children, and the grandchildren come back inside the
  // child's own response. Fetching at depth 0 would ask a leaf for children it
  // has already told us it does not have, and a leaf whose log does not exist
  // would answer 404 for a question the panel never needed to ask.
  if (depth <= 0 || seen.has(id)) return { children: [] };
  seen.add(id);
  const root = (await loadSubagents(id)) as { children?: Record<string, unknown>[] };
  const children = Array.isArray(root.children) ? root.children : [];
  const nested = await Promise.all(
    children.map(async (child) => {
      const childId = typeof child.child_session_id === 'string' ? child.child_session_id : '';
      if (!childId) return child;
      try {
        const grand = await subagentTree(childId, depth - 1, seen);
        return { ...child, children: (grand as { children?: unknown[] }).children ?? [] };
      } catch {
        return child;
      }
    }),
  );
  return { ...root, children: nested };
}

async function openPanel(key: PanelKey, turn?: string): Promise<void> {
  const id = store.get().active;
  if (!id) return;
  store.dispatch({ type: 'panel/open', key, turn });
  try {
    const data =
      key === 'why'
        ? await loadWhy(id, turn ?? null)
        : key === 'subagents'
          ? await subagentTree(id)
          : await panelLoaders[key](id);
    if (store.get().active !== id || store.get().panel?.key !== key) return;
    store.dispatch({ type: 'panel/loaded', data });
  } catch (error) {
    if (store.get().active !== id || store.get().panel?.key !== key) return;
    store.dispatch({ type: 'panel/failed', error: (error as Error).message || '面板没有读出来' });
  }
}

async function showArtifact(locator: string): Promise<void> {
  try {
    const blob = await fetchArtifact(locator);
    if (store.get().artifact !== locator) return;
    const ref = artifactRefs(store.get().items.values()).find((row) => row.locator === locator);
    const name = artifactLabel(locator, ref?.label);
    let view: ArtifactView;
    if (blob.type.startsWith('image/')) {
      if (artifactUrl) URL.revokeObjectURL(artifactUrl);
      artifactUrl = URL.createObjectURL(blob);
      view = { kind: 'image', name, url: artifactUrl, contentType: blob.type };
    } else if (blob.type === 'application/pdf') {
      // The viewer is the browser's; the blob URL keeps the bytes on this
      // origin and out of the document.
      if (artifactUrl) URL.revokeObjectURL(artifactUrl);
      artifactUrl = URL.createObjectURL(blob);
      view = { kind: 'pdf', name, url: artifactUrl, contentType: blob.type };
    } else if (/\.(docx|xlsx)$/i.test(name)) {
      // Office files are ZIP containers; the page unpacks and reads the one
      // part a preview needs (see office.ts).
      const bytes = new Uint8Array(await blob.arrayBuffer());
      if (!looksOffice(bytes)) throw new Error('这个文件不是有效的 Office 压缩包');
      const office = await readOffice(bytes.buffer, name);
      view = {
        kind: office.kind,
        name,
        paragraphs: office.paragraphs,
        rows: office.rows,
        sheet: office.sheet,
        contentType: blob.type,
      };
    } else {
      const text = await blob.text();
      if (store.get().artifact !== locator) return;
      if (/^\s*<(!doctype|html)/i.test(text)) view = { kind: 'html', name, text, contentType: blob.type };
      else if (/\.(md|markdown)$/i.test(name) || /^#\s/m.test(text)) view = { kind: 'markdown', name, text, contentType: blob.type };
      else view = { kind: 'text', name, text, contentType: blob.type };
    }
    if (store.get().artifact !== locator) return;
    store.dispatch({ type: 'artifact/loaded', view });
  } catch (error) {
    store.dispatch({ type: 'toast', toast: (error as Error).message || 'artifact 打不开' });
  }
}

function syncArtifacts(): void {
  const refs = artifactRefs(store.get().items.values());
  const current = store.get().artifact;
  const keep = current && refs.some((ref) => ref.locator === current);
  if (!refs.length) {
    if (current) store.dispatch({ type: 'artifact/select', artifact: null });
    return;
  }
  if (!keep) {
    store.dispatch({ type: 'artifact/select', artifact: refs[0].locator });
    void showArtifact(refs[0].locator);
  }
}

function connect(id: string): void {
  generation += 1;
  const run = generation;
  streamController?.abort();
  const controller = new AbortController();
  streamController = controller;

  void (async () => {
    let attempt = 0;
    while (run === generation && store.get().active === id) {
      const from = Math.max(1, store.get().head + 1);
      await streamSession(
        id,
        from,
        {
          onHello: (head) => {
            if (store.get().active === id) store.dispatch({ type: 'head', head });
          },
          onItem: (item) => {
            if (store.get().active !== id) return;
            store.dispatch({ type: 'item/upsert', item });
            if (item.kind === 'action') syncArtifacts();
            if (item.kind === 'answer' || item.kind === 'turn_end') store.dispatch({ type: 'live', live: null });
            if (item.kind === 'thinking') {
              const live = store.get().live;
              if (live?.reasoning) store.dispatch({ type: 'live', live: { ...live, reasoning: '' } });
            }
            if (item.kind === 'approval') {
              store.dispatch({ type: 'waiting', waiting: item.state === 'pending' || item.state === 'expired' });
            }
            if (item.kind === 'turn_end') {
              store.dispatch({ type: 'running', running: false });
              store.dispatch({ type: 'waiting', waiting: false });
              void refreshSummary(id);
              void refreshSessions();
            }
          },
          onLive: (live) => {
            if (store.get().active !== id) return;
            const text = live.text ?? '';
            // A recorded decision with no yielded turn comes back as an internal
            // status line; it is a toast, not something the model said.
            if (text.startsWith('审批没有续跑')) {
              say(text, true);
              return;
            }
            const current = store.get().live ?? {};
            if (live.kind === 'reasoning') {
              store.dispatch({ type: 'live', live: { ...current, reasoning: (current.reasoning ?? '') + text } });
            } else {
              store.dispatch({ type: 'live', live: { ...current, text: (current.text ?? '') + text } });
            }
            store.dispatch({ type: 'running', running: true });
          },
          onHeartbeat: (head) => {
            if (store.get().active === id) store.dispatch({ type: 'head', head });
          },
          onClose: (error) => {
            if (error && run === generation && attempt === 2) say('事件流断开，正在重连', true);
          },
        },
        controller.signal,
      );
      if (run !== generation || controller.signal.aborted) return;
      attempt = Math.min(5, attempt + 1);
      await sleep(Math.min(8000, 400 * 2 ** attempt));
    }
  })();
}

async function openSession(id: string, summary: SessionSummary | null = null): Promise<void> {
  generation += 1;
  streamController?.abort();
  store.dispatch({ type: 'session/select', id, summary });
  try {
    const [fresh, transcript] = await Promise.all([loadSession(id), loadTranscript(id)]);
    if (store.get().active !== id) return;
    store.dispatch({ type: 'summary/loaded', summary: fresh });
    store.dispatch({ type: 'transcript/loaded', transcript });
    store.dispatch({ type: 'running', running: Boolean(fresh.open_turn) && !fresh.yielded });
    store.dispatch({ type: 'waiting', waiting: Boolean(fresh.yielded) || Number(fresh.pending_approvals) > 0 });
    syncArtifacts();
    connect(id);
  } catch (error) {
    say((error as Error).message || '会话没有读出来', true);
  }
}

const actions: Actions = {
  selectSession(id) {
    void openSession(id, store.get().sessions.find((row) => row.id === id) ?? null);
  },

  newSession() {
    generation += 1;
    streamController?.abort();
    store.dispatch({ type: 'session/new', id: newSessionId() });
    document.getElementById('input')?.focus();
    say('新任务：说一句话开始');
  },

  async send(text) {
    if (store.get().waiting) {
      say('先处理上面的审批；这一轮会从那里继续');
      return;
    }
    let id = store.get().active;
    if (!id) {
      id = newSessionId();
      store.dispatch({ type: 'session/new', id });
    }
    const wasRunning = store.get().running;
    try {
      if (wasRunning) await steer(id, text);
      else await startTurn(id, text);
      store.dispatch({ type: 'error', error: null });
      store.dispatch({ type: 'running', running: true });
      if (wasRunning) say('已插话；会在下一个步骤边界生效');
      connect(id);
      void refreshSessions();
    } catch (error) {
      const apiError = error as ApiError;
      store.dispatch({ type: 'running', running: false });
      if (apiError.code === 'awaiting_decision') store.dispatch({ type: 'waiting', waiting: true });
      if (apiError.code === 'turn_in_progress') store.dispatch({ type: 'running', running: true });
      say(apiError.message || '这一轮没有发出去', true);
    }
  },

  async stop() {
    const id = store.get().active;
    if (!id) return;
    try {
      await interrupt(id);
      say('已请求停止');
    } catch (error) {
      say((error as Error).message || '停止请求没有送达', true);
    }
  },

  async decide(approvalId, decision) {
    const id = store.get().active;
    if (!id) return;
    try {
      await decide(id, approvalId, decision);
      store.dispatch({ type: 'live', live: null });
      store.dispatch({ type: 'waiting', waiting: false });
      store.dispatch({ type: 'running', running: true });
      say(decision === 'deny' ? '已拒绝：这一轮会带着「没有派发」继续' : '已允许：正在从停下的地方继续');
      connect(id);
      setTimeout(() => void refreshSummary(id), 800);
      void refreshSessions();
    } catch (error) {
      say((error as Error).message || '审批没有记录成功', true);
    }
  },

  async resume() {
    const id = store.get().active;
    if (!id) return;
    try {
      await resume(id);
      store.dispatch({ type: 'live', live: null });
      store.dispatch({ type: 'waiting', waiting: false });
      store.dispatch({ type: 'running', running: true });
      say('已继续这一轮');
      connect(id);
      setTimeout(() => void refreshSummary(id), 800);
    } catch (error) {
      say((error as Error).message || '续跑请求没有送达', true);
    }
  },

  openPanel(key, turn) {
    void openPanel(key, turn);
  },

  async archive(archived) {
    const id = store.get().active;
    if (!id) return;
    try {
      await archiveSession(id, archived);
      say(archived ? '已归档：日志还在，随时可以恢复' : '已恢复');
      await refreshSessions();
      await refreshSummary(id);
    } catch (error) {
      say((error as Error).message || '归档状态没有写进去', true);
    }
  },

  async remove() {
    const id = store.get().active;
    if (!id) return;
    try {
      await removeSession(id);
      // The stream belongs to a log that no longer exists; drop it before the
      // list is refreshed, so the reconnect loop cannot read a 404 as a fault.
      generation += 1;
      streamController?.abort();
      streamController = null;
      const sessions = (await loadSessions()).filter((row) => row.id !== id);
      store.dispatch({ type: 'sessions/loaded', sessions });
      say('已删除这个会话');
      const next = sessions.find((row) => !row.archived);
      if (next) await openSession(next.id, next);
      else actions.newSession();
    } catch (error) {
      say((error as Error).message || '没有删掉这个会话', true);
    }
  },

  async chooseModel(provider, model) {
    let id = store.get().active;
    if (!id) {
      id = newSessionId();
      store.dispatch({ type: 'session/new', id });
    }
    try {
      const choice = await chooseModelApi(id, provider, model);
      store.dispatch({ type: 'model/select', model: choice });
      const label = choice.model || choice.provider;
      say(
        store.get().running
          ? `这个会话改用 ${label}；正在跑的这轮仍然用原来的模型，下一轮生效`
          : `这个会话改用 ${label}；下一轮生效`,
      );
      void refreshSessions();
    } catch (error) {
      say((error as Error).message || '模型没有切成', true);
    }
  },

  selectArtifact(locator) {
    store.dispatch({ type: 'artifact/select', artifact: locator });
    void showArtifact(locator);
  },

  async exportSession(format) {
    const id = store.get().active;
    if (!id) return;
    try {
      const blob = await exportSessionApi(id, format);
      const url = URL.createObjectURL(blob);
      const link = document.createElement('a');
      link.href = url;
      link.download = `${id}.${format === 'json' ? 'json' : 'md'}`;
      document.body.appendChild(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 30_000);
      say(`已导出 ${format === 'json' ? 'JSON' : 'Markdown'}`);
    } catch (error) {
      say((error as Error).message || '导出失败', true);
    }
  },

  async fork() {
    const id = store.get().active;
    if (!id) return;
    try {
      const result = await forkSession(id);
      if (!result.forked || !result.session_id) {
        say('没有可分叉的已闭合回合边界', true);
        return;
      }
      say(`已从这一轮分叉出 ${result.session_id}`);
      await refreshSessions();
      await openSession(result.session_id);
    } catch (error) {
      say((error as Error).message || '分叉失败', true);
    }
  },

  async createSchedule(when, text) {
    const id = store.get().active;
    if (!id) return;
    try {
      await createScheduleApi(id, when, text);
      say('已排定');
      await openPanel('schedules');
    } catch (error) {
      say((error as Error).message || '排定失败', true);
    }
  },

  async cancelSchedule(itemId) {
    const id = store.get().active;
    if (!id) return;
    try {
      await cancelScheduleApi(id, itemId);
      say('已取消定时');
      await openPanel('schedules');
    } catch (error) {
      say((error as Error).message || '取消失败', true);
    }
  },
};

async function boot(): Promise<void> {
  if (!TOKEN) {
    renderAuth(root);
    return;
  }
  mount(root, store, actions);
  try {
    const [meta, sessions] = await Promise.all([loadMeta(), loadSessions()]);
    store.dispatch({ type: 'meta/loaded', meta });
    store.dispatch({ type: 'sessions/loaded', sessions });
    const wanted = new URLSearchParams(location.search).get('session');
    const first = sessions.find((row) => row.id === wanted) ?? sessions.find((row) => !row.archived);
    if (first) await openSession(first.id, first);
    else actions.newSession();
  } catch (error) {
    say((error as Error).message || '打不开这个会话', true);
  }
}

void boot();
