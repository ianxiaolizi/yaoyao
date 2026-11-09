export interface Usage {
  input_tokens?: number;
  output_tokens?: number;
  total_tokens?: number;
  cache_hit_tokens?: number;
}

/** A session's model, as the log records it: the host knobs, not a route. */
export interface ModelChoice {
  provider: string;
  model?: string | null;
}

/** What this process could actually build, offered by `/api/meta`. */
export interface ModelOption extends ModelChoice {
  label: string;
}

export interface Meta {
  v: number;
  app: string;
  workspace: string;
  model: string;
  sandbox: string;
  budget: number | null;
  capabilities: string[];
  models?: ModelOption[];
}

/** The kernel's status axes for one session, as `/api/sessions/{id}/status` folds them. */
export interface SessionStatus {
  v: number;
  session: string;
  run_status: 'in_progress' | 'completed' | 'interrupted' | 'failed' | null;
  delivery_status: 'delivered' | 'unanswered' | null;
  verification_status: 'verified' | 'unverified' | 'stale' | 'not_applicable' | 'blocked' | null;
  completion_status: 'verified_complete' | 'blocked_or_parked' | null;
  /** Why the latest settled turn stopped, when the log recorded a reason. */
  stop_reason?: string | null;
}

export interface SessionSummary {
  id: string;
  title: string;
  snippet: string;
  last_activity_at: string;
  open_turn: string | null;
  pending_approvals: number;
  steps: number;
  usage?: Usage;
  archived?: boolean;
  forked_from?: string | null;
  model?: ModelChoice | null;
  yielded?: boolean;
}

export interface Item {
  id: string;
  kind: 'user' | 'note' | 'thinking' | 'action' | 'approval' | 'answer' | 'turn_end';
  seq: number;
  updated_seq: number;
  turn?: string;
  step?: string;
  text?: string;
  tool?: string;
  arguments?: unknown;
  status?: string;
  content?: string;
  duration_ms?: number;
  approval_id?: string;
  summary?: string;
  state?: string;
  created_at?: string;
  expires_at?: string;
  ending?: string;
  stop_reason?: string;
  usage?: Usage;
  resumed?: boolean;
  steps?: number;
  /** When a prompt was queued; the turn rail uses it as the turn's time. */
  at?: string;
}

export interface Transcript {
  v: number;
  session: string;
  head: number;
  items: Item[];
}

export interface Live {
  step?: string;
  text?: string;
  reasoning?: string;
}

export type PanelKey = 'plan' | 'diff' | 'context' | 'subagents' | 'schedules' | 'why';

export interface WhyBasis {
  fact?: string;
  event?: string;
  node?: string;
}

/** One node of a turn's reasoning chain, as `/api/sessions/{id}/why` returns it. */
export interface WhyNode {
  id: string;
  turn_id: string;
  step: number;
  kind: string;
  claim: string;
  basis?: WhyBasis[];
  rule: string;
  source?: string;
}

export interface WhyReport {
  v: number;
  ok: boolean;
  session: string;
  turn: string;
  nodes: WhyNode[];
  verdict?: { rule: string; claim: string; step: number } | null;
  stop?: { event: string; reason: string } | null;
  check?: string;
}

export interface ArtifactView {
  kind: 'image' | 'html' | 'markdown' | 'text' | 'pdf' | 'docx' | 'xlsx';
  name: string;
  url?: string;
  text?: string;
  contentType?: string;
  /** docx: one entry per paragraph. */
  paragraphs?: string[];
  /** docx: the same paragraphs with Word heading levels kept. */
  blocks?: { text: string; heading?: number }[];
  /** xlsx: cells of the first sheet, gaps padded so columns line up. */
  rows?: string[][];
  /** xlsx: merged ranges, 0-based and inclusive. */
  merges?: { r1: number; c1: number; r2: number; c2: number }[];
  sheet?: string;
}
