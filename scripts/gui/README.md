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
