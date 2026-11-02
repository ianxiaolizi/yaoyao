import type { Item } from './types';

const REF = /artifact:\/\/sha256\/[0-9a-f]{64}/g;

export interface ArtifactRef {
  locator: string;
  turn?: string;
  label?: string;
}

/** Artifact refs are embedded in tool output; the log has no separate list. */
export function artifactRefs(items: Iterable<Item>): ArtifactRef[] {
  const seen = new Set<string>();
  const out: ArtifactRef[] = [];
  for (const item of items) {
    if (item.kind !== 'action' || typeof item.content !== 'string') continue;
    for (const match of item.content.match(REF) ?? []) {
      if (seen.has(match)) continue;
      seen.add(match);
      const args = item.arguments && typeof item.arguments === 'object' ? (item.arguments as Record<string, unknown>) : {};
      const path = typeof args.path === 'string' ? args.path : typeof args.name === 'string' ? args.name : '';
      const label = path.split(/[\\/]/).filter(Boolean).at(-1);
      out.push({ locator: match, turn: item.turn, ...(label ? { label } : {}) });
    }
  }
  return out;
}

export function artifactLabel(locator: string, label?: string): string {
  if (label) return label;
  const hash = locator.split('/').at(-1) ?? locator;
  return `artifact-${hash.slice(0, 6)}`;
}
