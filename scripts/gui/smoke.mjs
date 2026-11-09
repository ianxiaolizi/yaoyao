// The GUI smoke test, rewritten for M7 and split for M11.
//
// The page is compiled into the binary, so this drives the binary a user gets:
// `target/debug/agent.exe web`, no Vite, no `web/dist` on the served path. The
// fixtures are ordinary event logs under a throwaway data directory, so a pass
// proves the page, the projection and the binary agree rather than that a mock
// did.
//
// Run from the repository root:
//
//   npm ci --prefix scripts/gui
//   node scripts/gui/smoke.mjs
//
// Edge is expected at the default Windows path; override with EDGE_PATH.
// `AGENT_BIN` may point at a built binary other than target/debug/agent.exe.

import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { startBrowserHarness } from './smoke/browser.mjs';
import { seedAll } from './smoke/seed.mjs';
import * as approvals from './smoke/checks/approvals.mjs';
import * as artifacts from './smoke/checks/artifacts.mjs';
import * as core from './smoke/checks/core.mjs';
import * as responsive from './smoke/checks/responsive.mjs';

const root = resolve(import.meta.dirname, '../..');
const shots = process.env.YAOYAO_SCREENSHOT_DIR
  ? resolve(process.env.YAOYAO_SCREENSHOT_DIR)
  : join(root, 'target', 'gui-shots');
mkdirSync(shots, { recursive: true });

const temp = mkdtempSync(join(tmpdir(), 'yaoyao-m7-smoke-'));
const workspace = join(temp, 'workspace');
const dataDir = join(temp, 'data');
const logPath = join(temp, 'server.log');
mkdirSync(workspace, { recursive: true });
mkdirSync(dataDir, { recursive: true });

const failures = [];
let checks = 0;
function check(name, ok, detail = '') {
  checks += 1;
  console.log(ok ? 'PASS' : 'FAIL', name);
  if (!ok) failures.push(detail ? `${name}: ${detail}` : name);
}

const ctx = {
  context: null,
  origin: '',
  token: '',
  shots,
  errors: [],
  perf: { loadMs: 0, expandMs: 0, scrollMs: 0 },
  check,
};

let harness;
try {
  seedAll(dataDir);
  harness = await startBrowserHarness({ root, workspace, dataDir, logPath });
  ctx.context = harness.context;
  ctx.origin = harness.origin;
  ctx.token = harness.token;

  await core.run(ctx);
  await artifacts.run(ctx);
  await approvals.run(ctx);
  await responsive.run(ctx);

  // A live model need not emit `plan/updated`; asking for the missing plan is a
  // legitimate 404 and the panel renders the API's explanation. Exclude exactly
  // that pair and keep every other console/page/HTTP failure fatal.
  const expectedPlan404 = ctx.errors.some((entry) => entry.includes('/api/sessions/real-chain/plan'));
  const expectedConsole = 'console: Failed to load resource: the server responded with a status of 404 (Not Found)';
  const unexpectedErrors = ctx.errors.filter(
    (entry) =>
      !entry.includes('/api/sessions/real-chain/plan') &&
      !(expectedPlan404 && entry === expectedConsole),
  );
  check('真实执行链：缺计划 404 是预期降级', expectedPlan404);
  check('全程无 console error / pageerror / 4xx', unexpectedErrors.length === 0, unexpectedErrors.join(' | '));

  if (failures.length) {
    throw new Error(`${failures.length} check(s) failed:\n- ${failures.join('\n- ')}`);
  }
  console.log(
    JSON.stringify(
      {
        ok: true,
        checks,
        perf: ctx.perf,
        screenshots: [
          'm7-work.png',
          'm7-diff.png',
          'm7-sheet.png',
          'm7-artifacts.png',
          'm7-approval.png',
          'm7-expired.png',
          'm7-sent.png',
          'm7-long.png',
          'm7-narrow.png',
          'm7-390.png',
          'm8-why.png',
          'm8-model.png',
          'm8-archive.png',
          'm9-real-chain.png',
          'm11-office.png',
          'm10-unverified.png',
          'm10-verified.png',
          'm10-capability-blocked.png',
        ],
      },
      null,
      2,
    ),
  );
} finally {
  if (harness) await harness.close().catch(() => {});
  if (temp.startsWith(tmpdir()) && temp.includes('yaoyao-m7-smoke-')) {
    try {
      rmSync(temp, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } catch {
      // A leftover temp directory is not a test failure.
    }
  }
}
