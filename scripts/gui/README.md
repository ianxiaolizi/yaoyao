# GUI smoke

This is the browser half of `agent web`'s acceptance: it starts the real binary,
seeds a throwaway data directory with a session that has thinking, a `git.diff`
observation, and a pending approval, then drives the page in Edge.

```powershell
cargo build -p agent
npm ci --prefix scripts/gui
node scripts/gui/smoke.mjs
```

Screenshots land in `target/gui-smoke-*.png`.

- `AGENT_BIN` overrides `target/debug/agent.exe`.
- `EDGE_PATH` overrides the default Edge path.
- The script owns the server and its temporary workspace; it never touches your
  `.agent-data`.
- CI runs the same command in the `gui-smoke` job.

## Timeline performance (local only)

`perf.mjs` seeds sessions of 2,000 / 5,000 / 10,000 events and measures what a
reader feels: time to open the session, scroll frames per second, the worst
frame while a real reply streams, and how many finished turns were rebuilt
during that reply. It is not part of CI — the numbers are only comparable on the
machine that recorded `perf-baseline.json`, and a runner is a different machine.

```powershell
node scripts/gui/perf.mjs            # compare against the baseline
node scripts/gui/perf.mjs --record   # rewrite it
```

It fails when a time is more than twice the baseline, the scroll rate drops
below half of it, or a finished turn is rebuilt while the next one streams.
