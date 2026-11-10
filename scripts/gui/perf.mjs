// A local performance harness for the new shell (M7).
//
// It is deliberately not part of CI: the numbers are only comparable on the
// machine that recorded the baseline, so a GitHub runner would be measuring a
// different computer. Run it after changing the page and compare:
//
//   node scripts/gui/perf.mjs            # compare against perf-baseline.json
//   node scripts/gui/perf.mjs --record   # rewrite the baseline
//
// Each size is a seeded session of that many events, six events per turn. Four
// numbers per size, all end-user wall clock:
//
//   open      navigation → the current turn and its two folded neighbours (ms)
//   unfold    click "更早的 N 轮" → every earlier turn is a row again (ms)
//   scroll    frames per second while scrolling the unfolded list (fps)
//   live max  worst frame gap while a real reply streams (ms)
//   rebuilds  times the transcript was replaced during that reply (reported,
//             not gated: this shell redraws the column per durable item, and
//             the fold — not incremental patching — is what bounds the cost)
//
// The comparison is relative to the recorded baseline, with wide margins: this
// catches "the page got several times slower", not "the page got 8% slower".

import { spawn } from 'node:child_process';
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
import { chromium } from 'playwright-core';

const root = resolve(import.meta.dirname, '../..');
const binary = process.env.AGENT_BIN || join(root, 'target', 'debug', 'agent.exe');
const edge =
  process.env.EDGE_PATH ||
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe';
const baselinePath = join(import.meta.dirname, 'perf-baseline.json');
const record = process.argv.includes('--record');
const SIZES = [2000, 5000, 10000];
const EVENTS_PER_TURN = 6;

if (!existsSync(binary)) {
  throw new Error(`agent binary not found at ${binary}; run cargo build -p agent first`);
}

const temp = mkdtempSync(join(tmpdir(), 'yaoyao-gui-perf-'));
const workspace = join(temp, 'workspace');
const dataDir = join(temp, 'data');
const logPath = join(temp, 'server.log');
mkdirSync(workspace, { recursive: true });
mkdirSync(dataDir, { recursive: true });
let server;
let browser;

function event(session, seq, type, at, { turn = null, step = null, actor = 'agent', payload = {} } = {}) {
  return {
    seq,
    recorded_at: at,
    event: {
      event_id: `e-${session}-${seq}`,
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

// The inbox items are `discarded` on purpose: `pending` ones are claims on the
// scheduler, and a live server would start running them while the page is
// being measured.
function seed(session, size) {
  const turns = Math.round(size / EVENTS_PER_TURN);
  const base = Date.parse('2026-10-30T00:00:00Z');
  const at = (seq) => new Date(base + seq * 1000).toISOString();
  const events = [];
  let seq = 0;
  for (let index = 0; index < turns; index += 1) {
    const turn = `p${index + 1}`;
    const step = `ps${index + 1}`;
    const call = `c${index + 1}`;
    const item = {
      created_at: at(seq),
      item_id: `pi${index + 1}`,
      kind: 'next_turn',
      payload: { text: `perf question ${index + 1}: read the log and explain what changed` },
      priority: 0,
      session_id: session,
      status: 'discarded',
    };
    events.push(event(session, seq++, 'inbox/appended', at(seq), { actor: 'user', payload: item }));
    events.push(event(session, seq++, 'turn/started', at(seq), { turn, actor: 'scheduler', payload: { inbox_item: item.item_id } }));
    events.push(event(session, seq++, 'step/started', at(seq), { turn, step }));
    events.push(event(session, seq++, 'tool/started', at(seq), {
      turn,
      step,
      payload: { tool_name: 'fs.read', provider_tool_call_id: call, arguments: { path: `src/file${index + 1}.rs` } },
    }));
    events.push(event(session, seq++, 'tool/completed', at(seq), {
      turn,
      step,
      payload: {
        tool_name: 'fs.read',
        provider_tool_call_id: call,
        content: `file ${index + 1}: ${'the log is the only source of truth; '.repeat(6)}`,
      },
    }));
    events.push(event(session, seq++, 'turn/completed', at(seq), { turn, actor: 'scheduler', payload: { final_content: `answer ${index + 1}` } }));
  }
  const dir = join(dataDir, session);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'events.jsonl'), events.map((row) => JSON.stringify(row)).join('\n') + '\n');
  return turns;
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

async function measure(page, origin, token, session, turns) {
  const started = Date.now();
  await page.goto(`${origin}/?session=${session}#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(
    () => Boolean(document.querySelector('.y-turn')) && document.querySelectorAll('.y-past-row').length === 2,
    null,
    { timeout: 180_000 },
  );
  // Two frames, so the current turn has been painted before the clock stops.
  await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  const openMs = Date.now() - started;

  // The reply is measured first, in the state the page opens in: a reader who
  // sends a message is looking at the folded column. (Measuring it after the
  // unfolded view would bill the reply for the browser collecting that page.)
  await page.evaluate(() => {
    const transcript = document.getElementById('transcript');
    window.__perf = { gaps: [], rebuilds: 0, raf: 0 };
    let last = performance.now();
    const tick = () => {
      const now = performance.now();
      window.__perf.gaps.push(now - last);
      last = now;
      window.__perf.raf = requestAnimationFrame(tick);
    };
    window.__perf.raf = requestAnimationFrame(tick);
    new MutationObserver((rows) => {
      for (const row of rows) {
        if (row.removedNodes.length || row.addedNodes.length) window.__perf.rebuilds += 1;
      }
    }).observe(transcript, { childList: true });
  });
  await page.fill('#input', 'live update');
  await page.click('#send');
  await page.waitForFunction(
    () => [...document.querySelectorAll('.y-answer')].some((node) => node.textContent.includes('echo: live update')),
    null,
    { timeout: 60_000 },
  );
  const live = await page.evaluate(() => {
    cancelAnimationFrame(window.__perf.raf);
    const gaps = window.__perf.gaps.slice(1);
    return {
      frames: gaps.length,
      maxGapMs: Math.round(Math.max(0, ...gaps)),
      rebuilds: window.__perf.rebuilds,
    };
  });

  // A fresh page for unfolding: the reply added a turn, and the fold is what
  // decides how many rows the click has to build.
  await page.goto(`${origin}/?session=${session}#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(() => Boolean(document.querySelector('.y-turn')), null, { timeout: 120_000 });
  const unfoldMs = await page.evaluate(async (want) => {
    const began = performance.now();
    document.querySelector('[data-past-toggle]').click();
    await new Promise((done) => {
      const check = () => {
        if (document.querySelectorAll('.y-past-row').length === want) done();
        else requestAnimationFrame(check);
      };
      requestAnimationFrame(check);
    });
    return performance.now() - began;
  }, turns);

  const scroll = await page.evaluate(async () => {
    const box = document.getElementById('scroll');
    const total = Math.max(0, box.scrollHeight - box.clientHeight);
    box.scrollTop = 0;
    const gaps = [];
    let last = performance.now();
    const began = last;
    await new Promise((done) => {
      const step = () => {
        const now = performance.now();
        gaps.push(now - last);
        last = now;
        box.scrollTop = Math.min(total, ((now - began) / 1200) * total);
        if (now - began >= 1200) {
          done();
          return;
        }
        requestAnimationFrame(step);
      };
      requestAnimationFrame(step);
    });
    const samples = gaps.slice(1);
    return {
      frames: samples.length,
      fps: Math.round((samples.length / 1.2) * 10) / 10,
      maxGapMs: Math.round(Math.max(0, ...samples)),
    };
  });

  return {
    openMs,
    unfoldMs: Math.round(unfoldMs),
    scrollFps: scroll.fps,
    scrollMaxGapMs: scroll.maxGapMs,
    liveMaxGapMs: live.maxGapMs,
    liveRebuilds: live.rebuilds,
  };
}

function verdict(now, base) {
  const claims = [];
  const times = (value, was) => (was > 0 ? value / was : 1);
  claims.push({
    what: 'open',
    text: `${now.openMs}ms (${times(now.openMs, base.openMs).toFixed(2)}× baseline)`,
    ok: times(now.openMs, base.openMs) <= 2,
  });
  claims.push({
    what: 'unfold',
    text: `${now.unfoldMs}ms (${times(now.unfoldMs, base.unfoldMs).toFixed(2)}× baseline)`,
    ok: times(now.unfoldMs, base.unfoldMs) <= 2,
  });
  claims.push({
    what: 'scroll',
    text: `${now.scrollFps}fps (${times(now.scrollFps, base.scrollFps).toFixed(2)}× baseline)`,
    ok: now.scrollFps >= base.scrollFps * 0.5,
  });
  claims.push({
    what: 'scroll max',
    text: `${now.scrollMaxGapMs}ms (${times(now.scrollMaxGapMs, base.scrollMaxGapMs).toFixed(2)}× baseline)`,
    ok: times(now.scrollMaxGapMs, base.scrollMaxGapMs) <= 2,
  });
  claims.push({
    what: 'live max',
    text: `${now.liveMaxGapMs}ms (${times(now.liveMaxGapMs, base.liveMaxGapMs).toFixed(2)}× baseline)`,
    ok: times(now.liveMaxGapMs, base.liveMaxGapMs) <= 2,
  });
  return claims;
}

async function main() {
  const seeded = SIZES.map((size) => ({ size, session: `perf-${size}`, turns: seed(`perf-${size}`, size) }));

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
  page.on('pageerror', (error) => errors.push(`pageerror: ${error.message}`));

  // The first navigation of a fresh Edge pays for the browser's own cold start.
  // Warm it up on a throwaway session, so "open" measures the page instead.
  const warm = seed('perf-warm', 600);
  await page.goto(`${origin}/?session=perf-warm#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(
    () => Boolean(document.querySelector('.y-turn')),
    null,
    { timeout: 120_000 },
  );
  void warm;

  const measured = {};
  for (const { size, session, turns } of seeded) {
    measured[size] = await measure(page, origin, token, session, turns);
  }

  const baseline = record || !existsSync(baselinePath) ? null : JSON.parse(readFileSync(baselinePath, 'utf8'));
  const failed = [];
  for (const { size } of seeded) {
    const now = measured[size];
    const line =
      `${String(size).padStart(6)} events  open ${String(now.openMs).padStart(6)}ms  ` +
      `unfold ${String(now.unfoldMs).padStart(5)}ms  scroll ${String(now.scrollFps).padStart(5)}fps ` +
      `(max ${String(now.scrollMaxGapMs).padStart(4)}ms)  live max ${String(now.liveMaxGapMs).padStart(5)}ms  ` +
      `rebuilds ${now.liveRebuilds}`;
    console.log(line);
    if (!baseline) continue;
    for (const claim of verdict(now, baseline.runs?.[size] || {})) {
      console.log(`        ${claim.ok ? 'ok  ' : 'FAIL'} ${claim.what}: ${claim.text}`);
      if (!claim.ok) failed.push(`${size} events: ${claim.what} ${claim.text}`);
    }
  }

  if (errors.length) throw new Error(`browser errors:\n${errors.join('\n')}`);

  if (record || !baseline) {
    writeFileSync(
      baselinePath,
      JSON.stringify(
        {
          recorded_at: new Date().toISOString(),
          on: 'Windows 10 IoT LTSC 2021, Edge headless, debug build of agent web (M7 shell)',
          how: 'node scripts/gui/perf.mjs --record',
          runs: measured,
        },
        null,
        2,
      ) + '\n',
    );
    console.log(`recorded ${baselinePath}`);
    return;
  }
  if (baseline.runs && JSON.stringify(Object.keys(baseline.runs)) !== JSON.stringify(SIZES.map(String))) {
    console.log(`note: the baseline covers ${Object.keys(baseline.runs).join(', ')} events, this run measured ${SIZES.join(', ')}`);
  }
  if (failed.length) {
    console.log(`\nslower than the baseline:\n${failed.map((row) => `  ${row}`).join('\n')}`);
    process.exitCode = 1;
  }
}

try {
  await main();
} finally {
  if (browser) await browser.close().catch(() => {});
  if (server && server.exitCode === null) {
    const exited = new Promise((done) => server.once('exit', done));
    server.kill();
    await Promise.race([exited, new Promise((done) => setTimeout(done, 2000))]);
  }
  try {
    rmSync(temp, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  } catch {
    // A leftover temp directory is not a test failure.
  }
}
