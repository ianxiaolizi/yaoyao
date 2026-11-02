import type { ArtifactView, Item, Live, Meta, ModelChoice, PanelKey, SessionSummary, Transcript } from './types';

export interface PanelState {
  key: PanelKey;
  /** The turn a panel is about; only 推理链 uses it today. */
  turn?: string;
  data: unknown;
  error: string | null;
  loading: boolean;
}

export interface State {
  meta: Meta | null;
  sessions: SessionSummary[];
  active: string | null;
  summary: SessionSummary | null;
  /** A model chosen for a session that has no summary yet (created by the click). */
  chosenModel: ModelChoice | null;
  items: Map<string, Item>;
  head: number;
  running: boolean;
  waiting: boolean;
  live: Live | null;
  toast: string | null;
  error: string | null;
  expanded: Set<string>;
  sidebar: boolean;
  showAllPast: boolean;
  panel: PanelState | null;
  result: boolean;
  resultFull: boolean;
  artifact: string | null;
  artifactView: ArtifactView | null;
}

export type Action =
  | { type: 'meta/loaded'; meta: Meta }
  | { type: 'sessions/loaded'; sessions: SessionSummary[] }
  | { type: 'session/select'; id: string; summary: SessionSummary | null }
  | { type: 'session/new'; id: string }
  | { type: 'summary/loaded'; summary: SessionSummary }
  | { type: 'model/select'; model: ModelChoice }
  | { type: 'transcript/loaded'; transcript: Transcript }
  | { type: 'item/upsert'; item: Item }
  | { type: 'head'; head: number }
  | { type: 'live'; live: Live | null }
  | { type: 'running'; running: boolean }
  | { type: 'waiting'; waiting: boolean }
  | { type: 'toast'; toast: string | null }
  | { type: 'error'; error: string | null }
  | { type: 'turn/toggle'; turn: string }
  | { type: 'past/toggle' }
  | { type: 'sidebar/toggle' }
  | { type: 'panel/open'; key: PanelKey; turn?: string }
  | { type: 'panel/loaded'; data: unknown }
  | { type: 'panel/failed'; error: string }
  | { type: 'panel/close' }
  | { type: 'result/toggle' }
  | { type: 'result/close' }
  | { type: 'result/full' }
  | { type: 'artifact/select'; artifact: string | null }
  | { type: 'artifact/loaded'; view: ArtifactView | null };

export function initialState(): State {
  return {
    meta: null,
    sessions: [],
    active: null,
    summary: null,
    chosenModel: null,
    items: new Map(),
    head: 0,
    running: false,
    waiting: false,
    live: null,
    toast: null,
    error: null,
    expanded: new Set(),
    sidebar: typeof window === 'undefined' ? true : window.innerWidth > 1080,
    showAllPast: false,
    panel: null,
    result: false,
    resultFull: false,
    artifact: null,
    artifactView: null,
  };
}

export function reduce(state: State, action: Action): State {
  switch (action.type) {
    case 'meta/loaded':
      return { ...state, meta: action.meta };
    case 'sessions/loaded':
      return { ...state, sessions: action.sessions };
    case 'session/select':
      return {
        ...state,
        active: action.id,
        summary: action.summary,
        chosenModel: null,
        items: new Map(),
        head: 0,
        live: null,
        running: Boolean(action.summary?.open_turn),
        waiting: Boolean(action.summary?.pending_approvals),
        expanded: new Set(),
        showAllPast: false,
        panel: null,
        result: false,
        resultFull: false,
        artifact: null,
        artifactView: null,
        error: null,
      };
    case 'session/new':
      return {
        ...state,
        active: action.id,
        summary: null,
        chosenModel: null,
        items: new Map(),
        head: 0,
        live: null,
        running: false,
        waiting: false,
        expanded: new Set(),
        showAllPast: false,
        panel: null,
        result: false,
        resultFull: false,
        artifact: null,
        artifactView: null,
        error: null,
      };
    case 'summary/loaded':
      return {
        ...state,
        summary: action.summary,
        chosenModel: null,
        running: Boolean(action.summary.open_turn),
        waiting: Boolean(action.summary.pending_approvals),
      };
    case 'model/select':
      return { ...state, chosenModel: action.model };
    case 'transcript/loaded':
      return {
        ...state,
        items: new Map(action.transcript.items.map((item) => [item.id, item])),
        head: action.transcript.head,
      };
    case 'item/upsert': {
      const items = new Map(state.items);
      items.set(action.item.id, { ...items.get(action.item.id), ...action.item });
      return {
        ...state,
        items,
        head: Math.max(state.head, action.item.updated_seq ?? action.item.seq),
      };
    }
    case 'head':
      return { ...state, head: Math.max(state.head, action.head) };
    case 'live':
      return { ...state, live: action.live };
    case 'running':
      return { ...state, running: action.running, live: action.running ? state.live : null };
    case 'waiting':
      return { ...state, waiting: action.waiting };
    case 'toast':
      return { ...state, toast: action.toast };
    case 'error':
      return { ...state, error: action.error };
    case 'turn/toggle': {
      const expanded = new Set(state.expanded);
      if (expanded.has(action.turn)) expanded.delete(action.turn);
      else expanded.add(action.turn);
      return { ...state, expanded };
    }
    case 'past/toggle':
      return { ...state, showAllPast: !state.showAllPast };
    case 'sidebar/toggle':
      return { ...state, sidebar: !state.sidebar };
    case 'panel/open':
      return {
        ...state,
        panel: { key: action.key, turn: action.turn, data: null, error: null, loading: true },
      };
    case 'panel/loaded':
      return state.panel
        ? { ...state, panel: { ...state.panel, data: action.data, error: null, loading: false } }
        : state;
    case 'panel/failed':
      return state.panel
        ? { ...state, panel: { ...state.panel, data: null, error: action.error, loading: false } }
        : state;
    case 'panel/close':
      return { ...state, panel: null };
    case 'result/toggle':
      return { ...state, result: !state.result, resultFull: state.result ? false : state.resultFull };
    case 'result/close':
      return { ...state, result: false, resultFull: false };
    case 'result/full':
      return { ...state, result: true, resultFull: !state.resultFull };
    case 'artifact/select':
      return { ...state, artifact: action.artifact, artifactView: null };
    case 'artifact/loaded':
      return { ...state, artifactView: action.view };
  }
}

export class Store {
  private state: State;
  private listeners = new Set<(state: State, previous: State) => void>();

  constructor(state: State) {
    this.state = state;
  }

  get(): State {
    return this.state;
  }

  dispatch(action: Action): void {
    const previous = this.state;
    const next = reduce(previous, action);
    if (next === previous) return;
    this.state = next;
    for (const listener of this.listeners) listener(next, previous);
  }

  subscribe(listener: (state: State, previous: State) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }
}
