# GUI smoke

The browser half of `agent web`'s acceptance. Since M7 the page is compiled into
the binary (`web/dist` → `crates/agent-web/build.rs`), so the test starts the
real `agent.exe web`, not a Vite server, and drives exactly the bytes a user
gets.

```powershell
cargo build -p agent
npm ci --prefix scripts/gui
node scripts/gui/smoke.mjs
```

It seeds a throwaway data directory with eight sessions — a four-turn session with
thinking, a `git.diff` observation, a plan, context layers and a two-level
subagent tree; an artifact session with an image / HTML / Markdown / PDF / docx /
xlsx; a session replayed from a real captured run (`fixtures/real-chain`); pending
and expired approvals; a scratch session for archive/delete; a 1,000-item
session — then checks:

- the session list (6 recent rows + 「全部会话」), title, current turn, thinking fold, action rows;
- the turn rail: hover / focus summary, `Alt ↑/↓`, click to jump;
- the evidence sheet's plan / diff / why / context / subagents / schedules tabs；
- 排定 and 取消 against the real Inbox;
- export downloads `${id}.md` with the session's own text;
- fork opens a real child session, rename/archive/delete go through the menu;
- ⌘K / Ctrl K filtering and Enter;
- image / HTML / Markdown / PDF / docx / xlsx artifact previews, the result dock
  and full screen;
- Markdown tables, ordered lists, links, embedded data images and the external-image
  fallback; docx headings; xlsx merged cells and date formats;
- the real chain's plan, diff, answer and reasoning nodes;
- the real-subagent fixture's delegated + settled shapes;
- approval decisions, a ticking TTL and the settled record;
- model switch writes a choice and says when it applies; the approval-policy chip
  states that auto-approval is not offered; `+` is disabled;
- a new turn against the echo provider;
- one deliberately dropped SSE connection, after which the next turn still lands
  and no item is duplicated;
- 1,000 items: first render, unfolding every past turn, scrolling;
- `prefers-reduced-motion: reduce` leaves no transitions on the moving surfaces;
- 1024 / 900 drawer and 390px single column;
- no console error, page error or 4xx along the way.

Screenshots land in `target/gui-shots/m7-*.png`, `m8-*.png`, `m9-*.png` and `m10-*.png` by default. Set `YAOYAO_SCREENSHOT_DIR` to choose another directory, including `web/shots` when you deliberately want to refresh the checked-in acceptance images.
`AGENT_BIN` overrides `target/debug/agent.exe`; `EDGE_PATH` overrides the default Edge path.
The script owns the server and its temporary workspace; it never touches your
`.agent-data`. CI runs the same command in the `gui-smoke` job.

## Capturing a real chain

`capture-real-chain.mjs` runs the real binary — scheduler, tool gateway,
filesystem, `git diff` — against a scripted loopback model, because a machine
without `AGENT_MODEL_API_KEY` cannot call a vendor. Add `--live` on a machine
that has one and the same flow is captured from the real endpoint.

```powershell
# one-time: the gitignored local config used by both the launcher and this shell
Copy-Item scripts\gui\start.local.ps1.example scripts\gui\start.local.ps1
notepad scripts\gui\start.local.ps1

# the capture script inherits the current process environment, so load it first
. .\scripts\gui\start.local.ps1

node scripts/gui/capture-real-chain.mjs         # writes fixtures/real-chain/ (scripted stub)
node scripts/gui/capture-real-chain.mjs --live  # same flow, vendor model
```

The fixture keeps the run's bytes, timestamps and ids; only the throwaway
workspace path is rewritten to `<workspace>`.

## Capturing a real subagent

`capture-real-subagent.mjs` runs the real binary, policy gate and in-process
subagent provider: a parent turn closes, then `subagent.run` delegates
`check the notes`. Only the model is the echo provider. The fixture keeps the
parent log's real `subagent/spawned` + `subagent/completed`.

```powershell
node scripts/gui/capture-real-subagent.mjs    # writes fixtures/real-subagent/
```

## Timeline performance (local only)

`perf.mjs` seeds sessions of 2,000 / 5,000 / 10,000 events and measures what a
reader feels: time to open the session, time to unfold every earlier turn, scroll
frames per second, and the worst frame while a real reply streams. It is not part
of CI — the numbers are only comparable on the machine that recorded
`perf-baseline.json`, and a runner is a different machine.

```powershell
node scripts/gui/perf.mjs            # compare against the baseline
node scripts/gui/perf.mjs --record   # rewrite it
```
