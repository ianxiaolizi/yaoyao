// The GUI smoke test.
//
// It starts the real `agent web` binary against a throwaway data directory,
// seeds two sessions with the event shapes the projection understands, then
// drives the page in Edge. Nothing here talks to a mock HTTP server: the test
// would pass even if the page and the binary disagreed about the API.
//
// Run from the repository root:
//
//   pnpm --dir scripts/gui install
//   node scripts/gui/smoke.mjs
//
// Edge is expected at the default Windows path; override with EDGE_PATH.
// `AGENT_BIN` may point at a built binary other than target/debug/agent.exe.

import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import {
  closeSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import assert from 'node:assert/strict';
import { chromium } from 'playwright-core';

const root = resolve(import.meta.dirname, '../..');
const binary = process.env.AGENT_BIN || join(root, 'target', 'debug', 'agent.exe');
const edge =
  process.env.EDGE_PATH ||
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe';
const screenshots = join(root, 'target');
mkdirSync(screenshots, { recursive: true });

if (!existsSync(binary)) {
  throw new Error(`agent binary not found at ${binary}; run cargo build -p agent first`);
}

const workspace = mkdtempSync(join(tmpdir(), 'yaoyao-gui-smoke-'));
const dataDir = join(workspace, '.agent-data');
const logPath = join(workspace, 'server.log');
let server;
let browser;

function event(session, seq, type, { turn = null, step = null, actor = 'agent', payload = {} } = {}) {
  const at = new Date().toISOString();
  return {
    seq,
    recorded_at: at,
    event: {
      event_id: randomUUID(),
      event_type: type,
      schema_version: '0.1.0',
      session_id: session,
      run_id: null,
      turn_id: turn,
      step_id: step,
      actor,
      created_at: at,
      causation_id: null,
      correlation_id: null,
      payload,
      metadata: {},
      ignorable: false,
    },
  };
}

function inbox(text, session) {
  return {
    created_at: new Date().toISOString(),
    item_id: 'i1',
    kind: 'next_turn',
    payload: { text },
    priority: 0,
    session_id: session,
    status: 'pending',
  };
}

function reason(turn, step, kind, claim, id) {
  return {
    node: {
      id,
      turn_id: turn,
      step,
      kind,
      claim,
      basis: [{ fact: 'the smoke fixture' }],
      rule: 'smoke/rule',
      source: 'kernel',
    },
  };
}

function seedWork() {
  const session = 'work';
  const turn = 't1';
  const step = 's1';
  const item = inbox('show the diff and think about it', session);
  const diff = {
    files: [
      {
        file: 'src/lib.rs',
        patch: '@@ -1,1 +1,2 @@\n-old\n+new\n+more',
        additions: 2,
        deletions: 1,
        status: 'modified',
      },
    ],
    file_count: 1,
    additions: 2,
    deletions: 1,
  };
  const events = [
    event(session, 0, 'inbox/appended', { actor: 'user', payload: item }),
    event(session, 1, 'turn/started', {
      turn,
      actor: 'scheduler',
      payload: { inbox_item: item.item_id },
    }),
    event(session, 2, 'step/started', { turn, step }),
    event(session, 3, 'reason/recorded', {
      turn,
      step,
      payload: reason(turn, 0, 'situation', 'the person asked to see the diff', 'r/1'),
    }),
    event(session, 4, 'reason/recorded', {
      turn,
      step,
      payload: reason(turn, 1, 'reading', 'read the change, then explain it', 'r/2'),
    }),
    event(session, 5, 'tool/started', {
      turn,
      step,
      payload: {
        tool_name: 'git.diff',
        provider_tool_call_id: 'c1',
        arguments: { staged: false },
      },
    }),
    event(session, 6, 'tool/completed', {
      turn,
      step,
      payload: {
        tool_name: 'git.diff',
        provider_tool_call_id: 'c1',
        content: JSON.stringify(diff),
      },
    }),
    event(session, 7, 'model/request.prepared', {
      turn,
      step,
      payload: {
        estimated_context_tokens: 100,
        estimated_messages_tokens: 150,
        estimated_tool_tokens: 20,
        estimated_request_tokens: 170,
        max_input_tokens: 8000,
        reserved_output_tokens: 2000,
        message_count: 10,
        context_message_count: 8,
        dropped_message_count: 2,
        tool_count: 7,
        tool_manifest_id: 'smoke-manifest',
        context_layers: {
          state: { message_uid: 'msg/state/t1', text: 'the state layer' },
          task: { message_uid: 'msg/task/t1', text: 'the task layer' },
        },
        provider_usage: {
          input_tokens: 170,
          output_tokens: 20,
          total_tokens: 190,
          cache_hit_tokens: 80,
          reasoning_tokens: 5,
        },
      },
    }),
    event(session, 8, 'subagent/spawned', {
      turn,
      step,
      payload: {
        child_session_id: 'child-1',
        provider: 'mock',
        parent_turn_id: 't1',
        fork_at_turn_id: 't1',
        request: { objective: 'summarize the diff' },
      },
    }),
    event(session, 9, 'subagent/completed', {
      turn,
      step,
      actor: 'system',
      payload: {
        child_session_id: 'child-1',
        provider: 'mock',
        parent_turn_id: 't1',
        outcome: { status: 'completed', answer: 'done' },
      },
    }),
    event(session, 10, 'plan/updated', {
      turn,
      step,
      payload: {
        provider_tool_call_id: 'p1',
        plan: {
          goal: 'show the diff clearly',
          items: [
            { text: 'read the diff', status: 'done' },
            { text: 'explain it', status: 'doing' },
          ],
          facts: ['the log is the source'],
        },
      },
    }),
    event(session, 11, 'turn/completed', {
      turn,
      actor: 'scheduler',
      payload: { final_content: 'The diff is one hunk in src/lib.rs.' },
    }),
  ];
  writeSession(session, events);
}

function seedArtifact() {
  const session = 'artifact-smoke';
  const turn = 't3';
  const step = 's3';
  const item = inbox('show the artifact', session);
  const bytes = Buffer.from('R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==', 'base64');
  const hash = createHash('sha256').update(bytes).digest('hex');
  mkdirSync(join(dataDir, 'artifacts'), { recursive: true });
  writeFileSync(join(dataDir, 'artifacts', `${hash}.blob`), bytes);
  const events = [
    event(session, 0, 'inbox/appended', { actor: 'user', payload: item }),
    event(session, 1, 'turn/started', {
      turn,
      actor: 'scheduler',
      payload: { inbox_item: item.item_id },
    }),
    event(session, 2, 'step/started', { turn, step }),
    event(session, 3, 'tool/started', {
      turn,
      step,
      payload: {
        tool_name: 'tool.output',
        provider_tool_call_id: 'a1',
        arguments: {},
      },
    }),
    event(session, 4, 'tool/completed', {
      turn,
      step,
      payload: {
        tool_name: 'tool.output',
        provider_tool_call_id: 'a1',
        content: `the image is at artifact://sha256/${hash}`,
      },
    }),
    event(session, 5, 'turn/completed', {
      turn,
      actor: 'scheduler',
      payload: { final_content: 'The artifact is available.' },
    }),
  ];
  writeSession(session, events);
}

function seedApproval() {
  seedApprovalTo('ui-approval', 'ap-smoke', new Date().toISOString());
  seedApprovalTo('ui-expired', 'ap-old', new Date(Date.now() - 25 * 60 * 60_000).toISOString());
}

function seedApprovalTo(session, approvalId, createdAt) {
  const turn = 't2';
  const step = 's2';
  const item = inbox('run the acceptance tests', session);
  const request = {
    approval_id: approvalId,
    session_id: session,
    capability: 'process.spawn',
    tool_name: 'shell.run',
    args_summary: 'cargo test -p agent --test m4_acceptance',
    status: 'pending',
    created_at: createdAt,
  };
  const events = [
    event(session, 0, 'inbox/appended', { actor: 'user', payload: item }),
    event(session, 1, 'turn/started', {
      turn,
      actor: 'scheduler',
      payload: { inbox_item: item.item_id },
    }),
    event(session, 2, 'step/started', { turn, step }),
    event(session, 3, 'approval/requested', { payload: request }),
    event(session, 4, 'turn/yielded', {
      turn,
      actor: 'scheduler',
      payload: { approval_id: request.approval_id },
    }),
  ];
  writeSession(session, events);
}

function writeSession(session, events) {
  const dir = join(dataDir, session);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'events.jsonl'), events.map((row) => JSON.stringify(row)).join('\n') + '\n');
}

function waitForUrl(timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(logPath)) {
      const text = readFileSync(logPath, 'utf8');
      const match = text.match(/http:\/\/127\.0\.0\.1:\d+\/#t=[0-9a-f-]+/);
      if (match) return match[0];
      if (text.includes('failed:') || text.includes('error')) {
        throw new Error(`server did not start:\n${text}`);
      }
    }
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
  }
  throw new Error(`server did not print a URL within ${timeoutMs}ms:\n${readFileSync(logPath, 'utf8')}`);
}

async function main() {
  seedWork();
  seedArtifact();
  seedApproval();

  const logFd = openSync(logPath, 'a');
  server = spawn(binary, ['web', '--port', '0'], {
    cwd: workspace,
    env: {
      ...process.env,
      AGENT_WORKSPACE_DIR: workspace,
      AGENT_DATA_DIR: dataDir,
      AGENT_MODEL_PROVIDER: 'echo',
    },
    stdio: ['ignore', logFd, logFd],
  });
  closeSync(logFd);
  const url = waitForUrl();
  const origin = url.split('/#t=')[0];
  const token = url.split('#t=')[1];

  browser = await chromium.launch({ executablePath: edge, headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  const errors = [];
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`console: ${message.text()}`);
  });
  page.on('pageerror', (error) => errors.push(`pageerror: ${error.message}`));
  page.on('response', (response) => {
    if (response.status() >= 400) errors.push(`http ${response.status()}: ${response.url()}`);
  });

  await page.goto(`${origin}/?session=work#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.t', { timeout: 15_000 });
  await page.waitForFunction(
    () => [...document.querySelectorAll('.ans')].some((node) => node.textContent.includes('one hunk')),
    null,
    { timeout: 15_000 },
  );
  assert.equal(await page.textContent('#sessionTitle'), 'show the diff and think about it');
  assert.ok((await page.$$('.act')).length >= 1, 'the git.diff action is rendered');
  assert.ok((await page.$$('.think')).length >= 1, 'the thinking fold is rendered');

  await page.click('#panelBtn');
  await page.click('.p-tab[data-pv="diff"]');
  await page.waitForSelector('.diff-file', { timeout: 10_000 });
  assert.equal(await page.textContent('.diff-file .fp'), 'src/lib.rs');
  assert.ok((await page.textContent('.diff-file .patch')).includes('+new'));
  await page.screenshot({ path: join(screenshots, 'gui-smoke-work.png') });

  await page.click('.p-tab[data-pv="why"]');
  await page.waitForSelector('.node', { timeout: 10_000 });
  assert.ok((await page.$$('.node')).length >= 2, 'the /why chain is rendered');

  await page.click('.p-tab[data-pv="ctx"]');
  await page.waitForSelector('.ctxrow', { timeout: 10_000 });
  assert.ok((await page.textContent('#ctxBody')).includes('历史'), 'the context breakdown is rendered');
  await page.screenshot({ path: join(screenshots, 'gui-smoke-context.png') });

  await page.click('.p-tab[data-pv="plan"]');
  await page.waitForSelector('.plan-item', { timeout: 10_000 });
  assert.ok((await page.textContent('#planBody')).includes('show the diff clearly'));
  await page.screenshot({ path: join(screenshots, 'gui-smoke-plan.png') });

  await page.click('.p-tab[data-pv="sub"]');
  await page.waitForSelector('.sub-row', { timeout: 10_000 });
  assert.ok((await page.textContent('#subBody')).includes('summarize the diff'));
  await page.screenshot({ path: join(screenshots, 'gui-smoke-subagents.png') });

  await page.click('.p-tab[data-pv="sched"]');
  await page.waitForSelector('#schedAdd', { timeout: 10_000 });
  await page.fill('#schedWhen', '+10m');
  await page.fill('#schedText', 'later smoke');
  await page.click('#schedAdd');
  await page.waitForFunction(
    () => document.querySelector('#schedBody')?.textContent.includes('later smoke'),
    null,
    { timeout: 10_000 },
  );
  await page.screenshot({ path: join(screenshots, 'gui-smoke-schedule.png') });
  await page.click('[data-cancel-schedule]');

  page.once('dialog', (dialog) => dialog.accept('smoke fork'));
  await page.click('#forkBtn');
  await page.waitForFunction(
    () => document.querySelector('#toast')?.textContent.includes('已分叉出'),
    null,
    { timeout: 20_000 },
  );
  await page.waitForFunction(
    () => document.querySelector('#sessionTag')?.textContent !== 'work',
    null,
    { timeout: 20_000 },
  );
  assert.ok((await page.textContent('#threads')).includes('分叉自'), 'the child lineage is in the sidebar');

  await page.click('#exportMenu > summary');
  const [download] = await Promise.all([
    page.waitForEvent('download'),
    page.click('[data-export="md"]'),
  ]);
  assert.match(download.suggestedFilename(), /\.md$/);

  await page.goto(`${origin}/?session=artifact-smoke#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.act > summary', { timeout: 15_000 });
  await page.click('.act > summary');
  await page.waitForSelector('[data-artifact]', { timeout: 10_000 });
  await page.click('[data-artifact]');
  await page.waitForSelector('#artifactView:not([hidden]) img', { timeout: 10_000 });
  await page.waitForFunction(
    () => document.querySelector('#artifactImage')?.naturalWidth > 0,
    null,
    { timeout: 10_000 },
  );
  await page.screenshot({ path: join(screenshots, 'gui-smoke-artifact.png') });
  await page.click('#artifactClose');

  const before = await page.$eval('.side', (node) => Math.round(node.getBoundingClientRect().width));
  const handle = await page.$('#rzSide');
  const box = await handle.boundingBox();
  await page.mouse.move(box.x + 4, box.y + 200);
  await page.mouse.down();
  await page.mouse.move(box.x + 44, box.y + 200, { steps: 4 });
  await page.mouse.up();
  await page.waitForTimeout(300);
  const after = await page.$eval('.side', (node) => Math.round(node.getBoundingClientRect().width));
  assert.ok(after > before, 'the sidebar follows the drag');

  await page.click('#themeBtn');
  assert.equal(await page.$eval('html', (node) => node.dataset.theme), 'dark');
  await page.click('#themeBtn');

  await page.goto(`${origin}/?session=ui-approval#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-approval="ap-smoke"]', { timeout: 15_000 });
  assert.equal(await page.$$eval('[data-decision]', (nodes) => nodes.length), 4);
  assert.match(await page.textContent('.ask .ttl'), /后过期/);
  await page.screenshot({ path: join(screenshots, 'gui-smoke-approval.png') });

  await page.goto(`${origin}/?session=ui-expired#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-expired-resume]', { timeout: 15_000 });
  assert.match(await page.textContent('.ask-state'), /已过期/);
  await page.screenshot({ path: join(screenshots, 'gui-smoke-expired.png') });

  // The timeline updates per turn, not wholesale: a streamed reply must not
  // re-create the turns that were already closed. Three real turns give the
  // check finished turns to protect while a fourth one streams, and the page
  // keeps one wrapper per block — so a finished turn that is rebuilt loses its
  // node, which is exactly what the observer below counts.
  await page.goto(`${origin}/#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#newTask', { timeout: 15_000 });
  await page.click('#newTask');
  const settled = (count) =>
    page.waitForFunction(
      (want) => {
        const blocks = document.querySelectorAll('#timeline > .tblock[data-key^="turn:"]');
        return (
          blocks.length >= want &&
          Boolean(document.querySelector('#timeline > .tblock:last-child .end [data-why]'))
        );
      },
      count,
      { timeout: 20_000 },
    );
  for (const [index, prompt] of ['one', 'two', 'three'].entries()) {
    await page.fill('#input', `turn ${prompt}`);
    await page.click('#sendBtn');
    await page.waitForFunction(
      (want) => [...document.querySelectorAll('.ans')].some((node) => node.textContent.includes(want)),
      `echo: turn ${prompt}`,
      { timeout: 20_000 },
    );
    await settled(index + 1);
  }
  await page.evaluate(() => {
    const timeline = document.getElementById('timeline');
    window.__smoke = {
      keys: [...timeline.children].filter((node) => node.dataset.key?.startsWith('turn:')).map((node) => node.dataset.key),
      rebuilt: [],
    };
    new MutationObserver((rows) => {
      for (const row of rows) {
        for (const node of row.removedNodes) {
          if (node.nodeType === 1 && node.classList.contains('tblock')) {
            window.__smoke.rebuilt.push(node.dataset.key);
          }
        }
      }
    }).observe(timeline, { childList: true });
  });
  await page.fill('#input', 'turn four');
  await page.click('#sendBtn');
  await page.waitForFunction(
    () => [...document.querySelectorAll('.ans')].some((node) => node.textContent.includes('echo: turn four')),
    null,
    { timeout: 20_000 },
  );
  const streamed = await page.evaluate(() => {
    const { keys, rebuilt } = window.__smoke;
    const closing = keys[keys.length - 1];
    return {
      // A turn folds into a summary when the next one opens: that turn is
      // allowed to be rebuilt once, while it changes shape.
      closingRebuilds: rebuilt.filter((key) => key === closing).length,
      untouched: rebuilt.filter((key) => key !== closing && keys.includes(key)).length,
      turns: document.querySelectorAll('#timeline > .tblock[data-key^="turn:"]').length,
    };
  });
  assert.equal(streamed.untouched, 0, 'streaming a reply left the closed turns alone');
  assert.ok(streamed.closingRebuilds <= 1, `the closing turn changed shape at most once (${streamed.closingRebuilds})`);
  assert.ok(streamed.turns >= 4, 'the new turn was appended');

  await page.goto(`${origin}/#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(700);
  await page.click('#newTask');
  await page.fill('#input', 'hello smoke');
  await page.click('#sendBtn');
  await page.waitForFunction(
    () => [...document.querySelectorAll('.ans')].some((node) => node.textContent.includes('echo: hello smoke')),
    null,
    { timeout: 20_000 },
  );
  await page.waitForFunction(
    () => document.querySelector('#sessionTitle')?.textContent === 'hello smoke',
    null,
    { timeout: 10_000 },
  );

  const narrow = await browser.newPage({ viewport: { width: 800, height: 900 } });
  await narrow.goto(`${origin}/?session=work#t=${token}`, { waitUntil: 'domcontentloaded' });
  await narrow.waitForSelector('.ans', { state: 'attached', timeout: 15_000 });
  await narrow.click('#sideToggle');
  assert.ok(await narrow.$eval('body', (node) => node.classList.contains('side-open')));
  assert.ok(await narrow.isVisible('.side'), 'the narrow-screen drawer opens');
  await narrow.screenshot({ path: join(screenshots, 'gui-smoke-narrow.png') });
  await narrow.click('#sideBackdrop');
  await narrow.close();

  if (errors.length) throw new Error(`browser errors:\n${errors.join('\n')}`);
  console.log(JSON.stringify({
    ok: true,
    screenshots: [
      'gui-smoke-work.png',
      'gui-smoke-context.png',
      'gui-smoke-plan.png',
      'gui-smoke-subagents.png',
      'gui-smoke-schedule.png',
      'gui-smoke-approval.png',
      'gui-smoke-expired.png',
      'gui-smoke-artifact.png',
      'gui-smoke-narrow.png',
    ],
  }));
}

try {
  await main();
} finally {
  if (browser) await browser.close().catch(() => {});
  if (server && server.exitCode === null) {
    const exited = new Promise((resolve) => server.once('exit', resolve));
    server.kill();
    // Windows keeps the log file open until the child has actually exited; a
    // recursive remove before that point fails with EPERM.
    await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 2000))]);
  }
  try {
    rmSync(workspace, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  } catch {
    // A leftover temp directory is not a test failure.
  }
}
