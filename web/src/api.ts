import type { Item, Meta, ModelChoice, SessionSummary, Transcript, WhyReport } from './types';

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;
  constructor(message: string, code = 'http_error', status = 0) {
    super(message);
    this.code = code;
    this.status = status;
  }
}

export const TOKEN = location.hash.startsWith('#t=') ? decodeURIComponent(location.hash.slice(3)) : '';

interface ApiInit {
  method?: string;
  body?: unknown;
  headers?: HeadersInit;
  signal?: AbortSignal;
}

async function api<T>(path: string, init: ApiInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  if (TOKEN) headers.set('Authorization', `Bearer ${TOKEN}`);
  let body: BodyInit | undefined;
  if (init.body !== undefined) {
    headers.set('Content-Type', 'application/json');
    body = JSON.stringify(init.body);
  }
  const response = await fetch(path, { method: init.method, headers, body, signal: init.signal });
  const text = await response.text();
  let payload: unknown = null;
  try {
    payload = text ? JSON.parse(text) : null;
  } catch {
    // keep null
  }
  if (!response.ok) {
    const row = (payload ?? {}) as { message?: string; code?: string };
    throw new ApiError(row.message || `HTTP ${response.status}`, row.code || 'http_error', response.status);
  }
  return payload as T;
}

export async function loadMeta(): Promise<Meta> {
  const payload = await api<{ v: number; meta?: Meta } & Meta>('/api/meta');
  return (payload.meta ?? payload) as Meta;
}

export async function loadSessions(): Promise<SessionSummary[]> {
  const payload = await api<{ v: number; sessions: SessionSummary[] }>('/api/sessions');
  return payload.sessions;
}

export async function loadSession(id: string): Promise<SessionSummary> {
  const payload = await api<{ v: number; session: SessionSummary }>(`/api/sessions/${encodeURIComponent(id)}`);
  return payload.session;
}

export async function loadTranscript(id: string): Promise<Transcript> {
  return api<Transcript>(`/api/sessions/${encodeURIComponent(id)}/transcript`);
}

export async function startTurn(id: string, text: string): Promise<void> {
  await api(`/api/sessions/${encodeURIComponent(id)}/turns`, {
    method: 'POST',
    body: { v: 1, text },
  });
}

export async function steer(id: string, text: string): Promise<void> {
  await api(`/api/sessions/${encodeURIComponent(id)}/steer`, {
    method: 'POST',
    body: { v: 1, text },
  });
}

export async function interrupt(id: string): Promise<void> {
  await api(`/api/sessions/${encodeURIComponent(id)}/interrupt`, {
    method: 'POST',
    body: { v: 1 },
  });
}

export async function decide(id: string, approvalId: string, decision: 'once' | 'turn' | 'session' | 'deny'): Promise<void> {
  await api(`/api/sessions/${encodeURIComponent(id)}/approvals/${encodeURIComponent(approvalId)}`, {
    method: 'POST',
    body: { v: 1, decision },
  });
}

export async function resume(id: string): Promise<void> {
  await api(`/api/sessions/${encodeURIComponent(id)}/resume`, {
    method: 'POST',
    body: { v: 1 },
  });
}

export async function loadPlan(id: string): Promise<unknown> {
  return api(`/api/sessions/${encodeURIComponent(id)}/plan`);
}

export async function loadDiff(id: string): Promise<unknown> {
  return api(`/api/sessions/${encodeURIComponent(id)}/diff`);
}

export async function loadContext(id: string): Promise<unknown> {
  return api(`/api/sessions/${encodeURIComponent(id)}/context`);
}

export async function loadSubagents(id: string): Promise<unknown> {
  return api(`/api/sessions/${encodeURIComponent(id)}/subagents`);
}

export async function loadWhy(id: string, turn?: string | null): Promise<WhyReport> {
  const query = turn ? `?turn=${encodeURIComponent(turn)}` : '';
  return api<WhyReport>(`/api/sessions/${encodeURIComponent(id)}/why${query}`);
}

export async function archiveSession(id: string, archived: boolean): Promise<void> {
  await api(`/api/sessions/${encodeURIComponent(id)}/archive`, {
    method: 'POST',
    body: { v: 1, archived },
  });
}

export async function removeSession(id: string): Promise<void> {
  await api(`/api/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

export async function chooseModel(id: string, provider: string, model: string | null): Promise<ModelChoice> {
  const payload = await api<{ v: number; model: ModelChoice }>(`/api/sessions/${encodeURIComponent(id)}/model`, {
    method: 'POST',
    body: { v: 1, provider, model },
  });
  return payload.model;
}

export async function fetchArtifact(locator: string): Promise<Blob> {
  const headers = new Headers();
  if (TOKEN) headers.set('Authorization', `Bearer ${TOKEN}`);
  const response = await fetch(`/api/artifacts/${encodeURIComponent(locator)}`, { headers });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new ApiError((body as { message?: string }).message || `HTTP ${response.status}`, 'artifact_error', response.status);
  }
  return response.blob();
}

export async function forkSession(id: string, reason = 'user fork'): Promise<{ forked: boolean; session_id?: string }> {
  return api(`/api/sessions/${encodeURIComponent(id)}/fork`, {
    method: 'POST',
    body: { v: 1, reason },
  });
}

export async function exportSession(id: string, format: 'md' | 'json'): Promise<Blob> {
  const headers = new Headers();
  if (TOKEN) headers.set('Authorization', `Bearer ${TOKEN}`);
  const response = await fetch(`/api/sessions/${encodeURIComponent(id)}/export?format=${format}`, { headers });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new ApiError((body as { message?: string }).message || `导出失败 HTTP ${response.status}`, 'export_error', response.status);
  }
  return response.blob();
}

export interface ScheduleItem {
  item_id: string;
  due_at: string;
  overdue?: boolean;
  text: string;
  error?: string;
}

export interface ScheduleList {
  v: number;
  session: string;
  items: ScheduleItem[];
}

export async function loadSchedules(id: string): Promise<ScheduleList> {
  return api(`/api/sessions/${encodeURIComponent(id)}/schedules`);
}

export async function createSchedule(id: string, when: string, text: string): Promise<void> {
  await api(`/api/sessions/${encodeURIComponent(id)}/schedules`, {
    method: 'POST',
    body: { v: 1, when, text },
  });
}

export async function cancelSchedule(id: string, itemId: string): Promise<void> {
  await api(`/api/sessions/${encodeURIComponent(id)}/schedules/${encodeURIComponent(itemId)}`, {
    method: 'DELETE',
  });
}

export interface StreamHandlers {
  onHello(head: number): void;
  onItem(item: Item): void;
  onLive(live: { step?: string; kind?: string; text?: string }): void;
  onHeartbeat(head: number): void;
  onClose(error?: Error): void;
}

/** Fetch-streaming SSE; EventSource cannot carry the bearer header. */
export async function streamSession(
  id: string,
  from: number,
  handlers: StreamHandlers,
  signal: AbortSignal,
): Promise<void> {
  const headers = new Headers();
  if (TOKEN) headers.set('Authorization', `Bearer ${TOKEN}`);
  let response: Response;
  try {
    response = await fetch(`/api/sessions/${encodeURIComponent(id)}/events?from=${from}`, { headers, signal });
  } catch (error) {
    if (!signal.aborted) handlers.onClose(error as Error);
    return;
  }
  if (!response.ok || !response.body) {
    handlers.onClose(new Error(`events HTTP ${response.status}`));
    return;
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let split = buffer.indexOf('\n\n');
      while (split >= 0) {
        const block = buffer.slice(0, split);
        buffer = buffer.slice(split + 2);
        handleBlock(block, handlers);
        split = buffer.indexOf('\n\n');
      }
    }
    if (!signal.aborted) handlers.onClose();
  } catch (error) {
    if (!signal.aborted) handlers.onClose(error as Error);
  }
}

function handleBlock(block: string, handlers: StreamHandlers): void {
  let event = 'message';
  const data: string[] = [];
  for (const line of block.split(/\r?\n/)) {
    if (line.startsWith('event:')) event = line.slice(6).trim();
    else if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
  }
  if (!data.length) return;
  let payload: Record<string, unknown>;
  try {
    payload = JSON.parse(data.join('\n')) as Record<string, unknown>;
  } catch {
    return;
  }
  if (event === 'hello') {
    handlers.onHello(Number(payload.head ?? 0));
  } else if (event === 'item') {
    const item = payload.item as Item | undefined;
    if (item) handlers.onItem(item);
  } else if (event === 'live') {
    handlers.onLive({ step: payload.step as string, kind: payload.kind as string, text: payload.text as string });
  } else if (event === 'heartbeat') {
    handlers.onHeartbeat(Number(payload.head ?? 0));
  }
}
