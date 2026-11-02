// The GUI smoke test, rewritten for M7.
//
// The page is compiled into the binary, so this drives the binary a user gets:
// `target/debug/agent.exe web`, no Vite, no `web/dist` on the served path. The
// fixtures are ordinary event logs under a throwaway data directory, so a pass
// proves the page, the projection and the binary agree rather than that a mock
// did.
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
import { deflateRawSync } from 'node:zlib';
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
const shots = join(root, 'web', 'shots');
mkdirSync(shots, { recursive: true });

if (!existsSync(binary)) {
  throw new Error(`agent binary not found at ${binary}; run cargo build -p agent first`);
}

const temp = mkdtempSync(join(tmpdir(), 'yaoyao-m7-smoke-'));
const workspace = join(temp, 'workspace');
const dataDir = join(temp, 'data');
const logPath = join(temp, 'server.log');
mkdirSync(workspace, { recursive: true });
mkdirSync(dataDir, { recursive: true });

let server;
let browser;

// ---------------------------------------------------------------- fixtures

const nowIso = () => new Date().toISOString();

function event(session, seq, type, { turn = null, step = null, actor = 'agent', payload = {} } = {}) {
  const at = nowIso();
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

function inbox(session, itemId, text) {
  return {
    created_at: nowIso(),
    item_id: itemId,
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
      rule: 'situation/derive',
      source: 'kernel',
    },
  };
}

function writeSession(session, events) {
  const dir = join(dataDir, session);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'events.jsonl'), events.map((row) => JSON.stringify(row)).join('\n') + '\n');
}

/** A small log builder: one sequence, one turn at a time. */
function builder(session) {
  const events = [];
  let seq = 0;
  const add = (type, { turn = null, step = null, actor = 'agent', payload = {} } = {}) => {
    events.push(event(session, seq++, type, { turn, step, actor, payload }));
  };
  return {
    add,
    events,
    ask(turn, text) {
      const item = inbox(session, `inbox-${turn}`, text);
      add('inbox/appended', { actor: 'user', payload: item });
      add('turn/started', { turn, actor: 'scheduler', payload: { inbox_item: item.item_id } });
      add('step/started', { turn, step: `step-${turn}` });
    },
    answer(turn, text, extra = {}) {
      add('turn/completed', { turn, actor: 'scheduler', payload: { final_content: text, ...extra } });
    },
    save() {
      writeSession(session, events);
    },
  };
}

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

function seedWork() {
  const log = builder('work');
  log.ask('t1', 'show the diff and think about it');
  log.answer('t1', 'One hunk in src/lib.rs.');
  log.ask('t2', 'turn two');
  log.answer('t2', 'Two.');
  log.ask('t3', 'turn three');
  log.answer('t3', 'Three.');

  // The current turn carries everything the evidence sheet reads.
  log.ask('t4', 'what changed, and is it safe?');
  log.add('reason/recorded', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      node: {
        id: 'r/1',
        turn_id: 't4',
        step: 0,
        kind: 'situation',
        claim: 'the person asked what changed',
        basis: [{ fact: 'the turn context' }],
        rule: 'situation/derive',
        source: 'kernel',
      },
    },
  });
  log.add('reason/recorded', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      node: {
        id: 'r/2',
        turn_id: 't4',
        step: 0,
        kind: 'reading',
        claim: 'the input asks a question, so the turn answers it',
        basis: [{ node: 'r/1' }],
        rule: 'reading/question-cue',
        source: 'kernel',
      },
    },
  });
  log.add('reason/recorded', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      node: {
        id: 'r/3',
        turn_id: 't4',
        step: 0,
        kind: 'stance',
        claim: 'answering; no call is needed',
        basis: [{ node: 'r/2' }],
        rule: 'stance/answer-question',
        source: 'kernel',
      },
    },
  });
  log.add('reason/recorded', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      node: {
        id: 'r/4',
        turn_id: 't4',
        step: 0,
        kind: 'verdict',
        claim: 'answered from the diff',
        basis: [{ node: 'r/3' }],
        rule: 'verdict/answer',
        source: 'kernel',
      },
    },
  });
  log.add('tool/started', {
    turn: 't4',
    step: 'step-t4',
    payload: { tool_name: 'git.diff', provider_tool_call_id: 'c1', arguments: { staged: false } },
  });
  log.add('tool/completed', {
    turn: 't4',
    step: 'step-t4',
    actor: 'tool',
    payload: {
      tool_name: 'git.diff',
      provider_tool_call_id: 'c1',
      status: 'completed',
      content: JSON.stringify(diff),
    },
  });
  log.add('model/request.prepared', {
    turn: 't4',
    step: 'step-t4',
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
        state: { message_uid: 'msg/state/t4', text: 'the state layer' },
        task: { message_uid: 'msg/task/t4', text: 'the task layer' },
      },
      provider_usage: {
        input_tokens: 170,
        output_tokens: 20,
        total_tokens: 190,
        cache_hit_tokens: 80,
        reasoning_tokens: 5,
      },
    },
  });
  log.add('subagent/spawned', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      child_session_id: 'work-child',
      provider: 'mock',
      parent_turn_id: 't4',
      fork_at_turn_id: 't4',
      request: { objective: 'summarize the diff' },
    },
  });
  log.add('subagent/completed', {
    turn: 't4',
    step: 'step-t4',
    actor: 'system',
    payload: {
      child_session_id: 'work-child',
      provider: 'mock',
      parent_turn_id: 't4',
      outcome: { status: 'completed', answer: 'done' },
    },
  });
  log.add('plan/updated', {
    turn: 't4',
    step: 'step-t4',
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
  });
  log.answer('t4', 'The diff is one hunk in src/lib.rs.');
  log.save();

  // The child's own log exists and has one grandchild, so the tree can read two
  // levels without probing a session that is not there (a 404 the browser would
  // log as a console error).
  const child = builder('work-child');
  child.ask('c1', 'review the diff');
  child.add('subagent/spawned', {
    turn: 'c1',
    step: 'step-c1',
    payload: {
      child_session_id: 'work-grandchild',
      provider: 'mock',
      parent_turn_id: 'c1',
      fork_at_turn_id: 'c1',
      request: { objective: 'check the short path' },
    },
  });
  child.answer('c1', 'looks fine');
  child.save();
}

/** A minimal one-page PDF with a real xref, so the viewer has something to open. */
function pdfBytes() {
  const stream = 'BT /F1 12 Tf 20 100 Td (M8 smoke) Tj ET';
  const bodies = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>',
    `<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`,
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
  ];
  let out = '%PDF-1.4\n';
  const offsets = [];
  for (const [index, body] of bodies.entries()) {
    offsets.push(out.length);
    out += `${index + 1} 0 obj\n${body}\nendobj\n`;
  }
  const xref = out.length;
  out += `xref\n0 ${bodies.length + 1}\n0000000000 65535 f \n`;
  out += offsets.map((offset) => `${String(offset).padStart(10, '0')} 00000 n \n`).join('');
  out += `trailer\n<< /Size ${bodies.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return Buffer.from(out, 'latin1');
}

function putArtifact(bytes) {
  const hash = createHash('sha256').update(bytes).digest('hex');
  const dir = join(dataDir, 'artifacts');
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, `${hash}.blob`), bytes);
  return { locator: `artifact://sha256/${hash}`, hash };
}

/** A ZIP with deflated entries, the way a real OOXML file is packed. */
function zipBytes(entries) {
  const local = [];
  const central = [];
  let offset = 0;
  for (const [name, content] of entries) {
    const data = Buffer.from(content, 'utf8');
    const packed = deflateRawSync(data);
    const nameBytes = Buffer.from(name, 'utf8');
    const header = Buffer.alloc(30);
    header.writeUInt32LE(0x04034b50, 0);
    header.writeUInt16LE(20, 4);
    header.writeUInt16LE(0x0800, 6);
    header.writeUInt16LE(8, 8);
    header.writeUInt32LE(0, 14);
    header.writeUInt32LE(packed.length, 18);
    header.writeUInt32LE(data.length, 22);
    header.writeUInt16LE(nameBytes.length, 26);
    local.push(header, nameBytes, packed);
    const entry = Buffer.alloc(46);
    entry.writeUInt32LE(0x02014b50, 0);
    entry.writeUInt16LE(20, 4);
    entry.writeUInt16LE(20, 6);
    entry.writeUInt16LE(0x0800, 8);
    entry.writeUInt16LE(8, 10);
    entry.writeUInt32LE(packed.length, 20);
    entry.writeUInt32LE(data.length, 24);
    entry.writeUInt16LE(nameBytes.length, 28);
    entry.writeUInt32LE(offset, 42);
    central.push(entry, nameBytes);
    offset += 30 + nameBytes.length + packed.length;
  }
  const directory = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...local, directory, end]);
}

function docxBytes() {
  return zipBytes([
    [
      '[Content_Types].xml',
      '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>',
    ],
    [
      '_rels/.rels',
      '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>',
    ],
    [
      'word/document.xml',
      '<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>M9 文档预览</w:t></w:r></w:p><w:p><w:r><w:t>第二段：docx 由页面解压读取。</w:t></w:r></w:p><w:p><w:r><w:t>第三个段落带 &amp; 一个转义符。</w:t></w:r></w:p></w:body></w:document>',
    ],
  ]);
}

function xlsxBytes() {
  return zipBytes([
    [
      'xl/sharedStrings.xml',
      '<?xml version="1.0" encoding="UTF-8"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>月份</t></si><si><t>收入</t></si><si><t>一月</t></si></sst>',
    ],
    [
      'xl/worksheets/sheet1.xml',
      '<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>42</v></c></row></sheetData></worksheet>',
    ],
  ]);
}

function seedArtifacts() {
  const log = builder('artifact-smoke');
  log.ask('t1', 'show the artifacts');
  const png = Buffer.from('R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==', 'base64');
  const html = Buffer.from(
    '<!doctype html><html><body style="font:14px system-ui;padding:24px"><h1>junction 修复验证</h1><p>36 / 36 通过</p></body></html>',
    'utf8',
  );
  const md = Buffer.from(
    '# junction 修复验证\n\n- 复现：repro_junction.mjs\n- 修复：lexical + realpath\n- 回归：36 / 36\n',
    'utf8',
  );
  for (const [label, bytes] of [
    ['shot.gif', png],
    ['report.html', html],
    ['junction-fix.md', md],
    ['coverage.pdf', pdfBytes()],
    ['notes.docx', docxBytes()],
    ['numbers.xlsx', xlsxBytes()],
  ]) {
    const { locator } = putArtifact(bytes);
    const call = `art-${label}`;
    log.add('tool/started', {
      turn: 't1',
      step: 'step-t1',
      payload: { tool_name: 'fs.write', provider_tool_call_id: call, arguments: { path: `reports/${label}` } },
    });
    log.add('tool/completed', {
      turn: 't1',
      step: 'step-t1',
      actor: 'tool',
      payload: {
        tool_name: 'fs.write',
        provider_tool_call_id: call,
        status: 'completed',
        content: `wrote reports/${label}\n${locator}`,
        result_locator: locator,
        artifact_refs: [],
      },
    });
  }
  log.answer('t1', 'Three artifacts are ready.');
  log.save();
}

function seedApproval(session, approvalId, createdAt) {
  const log = builder(session);
  log.ask('t2', 'run the acceptance tests');
  log.add('approval/requested', {
    turn: 't2',
    step: 'step-t2',
    actor: 'scheduler',
    payload: {
      approval_id: approvalId,
      session_id: session,
      capability: 'process.spawn',
      tool_name: 'shell.run',
      args_summary: 'cargo test -p agent --test m4_acceptance',
      status: 'pending',
      created_at: createdAt,
    },
  });
  log.add('turn/yielded', { turn: 't2', actor: 'scheduler', payload: { approval_id: approvalId } });
  log.save();
}

function seedScratch() {
  const log = builder('scratch');
  log.ask('t1', 'scratch session');
  log.answer('t1', 'scratch answer');
  log.save();
}

/**
 * A log captured from a real turn (`capture-real-chain.mjs`): the scheduler,
 * tool gateway, filesystem and `git diff` all ran. Replaying it keeps the
 * panels honest about real event shapes instead of this file's hand-written
 * fixtures.
 */
function seedRealChain() {
  const source = join(import.meta.dirname, 'fixtures', 'real-chain', 'events.jsonl');
  const dir = join(dataDir, 'real-chain');
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'events.jsonl'), readFileSync(source));
}

/** 100 turns × 10 items: the fold, not the API, is what gets measured. */function seedLong() {
  const log = builder('perf-1000');
  for (let turn = 1; turn <= 100; turn += 1) {
    const id = `t${turn}`;
    log.ask(id, `perf turn ${turn}`);
    log.add('reason/recorded', {
      turn: id,
      step: `step-${id}`,
      payload: reason(id, 0, 'situation', `turn ${turn} was asked`, `r/${turn}`),
    });
    for (let action = 1; action <= 6; action += 1) {
      const call = `c-${turn}-${action}`;
      log.add('tool/started', {
        turn: id,
        step: `step-${id}`,
        payload: { tool_name: 'fs.read', provider_tool_call_id: call, arguments: { path: `src/file-${turn}.rs` } },
      });
      log.add('tool/completed', {
        turn: id,
        step: `step-${id}`,
        actor: 'tool',
        payload: {
          tool_name: 'fs.read',
          provider_tool_call_id: call,
          status: 'completed',
          content: `line ${action} of file ${turn}`,
        },
      });
    }
    log.answer(id, `turn ${turn} done`);
  }
  log.save();
}

// ---------------------------------------------------------------- the run

function waitForUrl(timeoutMs = 30_000) {
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

const failures = [];
let checks = 0;
function check(name, ok, detail = '') {
  checks += 1;
  console.log(ok ? 'PASS' : 'FAIL', name);
  if (!ok) failures.push(detail ? `${name}: ${detail}` : name);
}

function must(name, value) {
  if (!value) throw new Error(`prerequisite missing: ${name}`);
  return value;
}

async function openPage(context, path) {
  const page = await context.newPage();
  const errors = [];
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`console: ${message.text()}`);
  });
  page.on('pageerror', (error) => errors.push(`pageerror: ${error.message}`));
  page.on('response', (response) => {
    if (response.status() >= 400) errors.push(`http ${response.status()}: ${response.url()}`);
  });
  await page.goto(`${origin}/${path}`, { waitUntil: 'domcontentloaded' });
  return { page, errors };
}

async function text(page, selector) {
  try {
    return (await page.textContent(selector, { timeout: 8_000 })) ?? '';
  } catch {
    return '';
  }
}

async function waitText(page, selector, needle, timeout = 15_000) {
  try {
    await page.waitForFunction(
      ([sel, want]) => (document.querySelector(sel)?.textContent ?? '').includes(want),
      [selector, needle],
      { timeout },
    );
    return true;
  } catch {
    return false;
  }
}

let origin = '';
let token = '';

async function main() {
  seedWork();
  seedArtifacts();
  seedApproval('ui-approval', 'ap-smoke', nowIso());
  seedApproval('ui-expired', 'ap-old', new Date(Date.now() - 25 * 60 * 60_000).toISOString());
  seedScratch();
  seedRealChain();
  seedLong();

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
  origin = url.split('/#t=')[0];
  token = url.split('#t=')[1];
  must('url', origin && token);

  browser = await chromium.launch({ executablePath: edge, headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 }, acceptDownloads: true });
  const { page, errors } = await openPage(context, `?session=work#t=${token}`);

  // The page is the compiled bundle, not a dev server's response.
  const scripts = await page.$$eval('script[src]', (nodes) => nodes.map((node) => node.getAttribute('src')));
  check('页面来自打包产物 /assets/', scripts.some((src) => /^\.\/assets\/index-.*\.js$/.test(src)), scripts.join(','));

  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  check('会话标题取第一条用户消息', (await text(page, '#title')) === 'show the diff and think about it');
  check('轮数与日志一致', (await text(page, '#round')) === '第 4 轮');
  check('侧栏列出全部会话', (await page.$$eval('.y-session', (nodes) => nodes.length)) === 8);
  check('当前轮目标', (await text(page, '.y-goal')).includes('what changed, and is it safe?'));
  check('当前轮回答', (await text(page, '.y-answer')).includes('one hunk'));
  check('思考折叠存在', (await page.$$eval('.y-thinking', (nodes) => nodes.length)) >= 1);

  await page.click('.y-action > summary');
  check('行动行展开后含参数', (await text(page, '.y-action')).includes('git.diff'));
  check('行动行展开后含 diff 输出', (await text(page, '.y-action')).includes('src/lib.rs'));

  check(
    '旧轮折叠，只留最近两轮',
    (await page.$$eval('.y-past-row', (nodes) => nodes.length)) === 2,
    String(await page.$$eval('.y-past-row', (nodes) => nodes.length)),
  );
  check('折叠入口写明更早轮数', (await text(page, '[data-past-toggle]')).includes('更早的 1 轮'));
  await page.click('[data-past-toggle]');
  check(
    '展开后所有旧轮都在',
    (await page.$$eval('.y-past-row', (nodes) => nodes.length)) === 3,
  );
  await page.screenshot({ path: join(shots, 'm7-work.png') });

  // Evidence sheet: plan / diff / context / subagents.
  await page.click('#details');
  await page.waitForSelector('#sheet.open', { timeout: 10_000 });
  check('详情默认打开计划', await waitText(page, '#sheetBody', 'show the diff clearly'));
  await page.click('[data-panel-tab="diff"]');
  check('改动页签读到文件名', await waitText(page, '#sheetBody', 'src/lib.rs'));
  check('改动页签读到 patch', await waitText(page, '#sheetBody', '+new'));
  check('改动页签读到行数', await waitText(page, '#sheetBody', '−1'));
  await page.screenshot({ path: join(shots, 'm7-diff.png') });

  await page.click('[data-panel-tab="why"]');
  check('推理链页签读到判定', await waitText(page, '#sheetBody', 'answered from the diff'));
  check('推理链页签按轮标标题', (await text(page, '#sheetTitle')).includes('第 4 轮'));
  check('推理链节点带依据', await waitText(page, '#sheetBody', 'the turn context'));
  await page.screenshot({ path: join(shots, 'm8-why.png') });

  await page.click('[data-panel-tab="context"]');
  check('上下文页签读到用量', await waitText(page, '#sheetBody', '已用'));
  check('上下文页签读到分层', await waitText(page, '#sheetBody', 'state'));
  await page.click('[data-panel-tab="subagents"]');
  check('子代理页签读到目标', await waitText(page, '#sheetBody', 'summarize the diff'));
  check('子代理树读到第二层', await waitText(page, '#sheetBody', 'check the short path'));
  await page.screenshot({ path: join(shots, 'm7-sheet.png') });

  // Schedules: create and cancel against the real Inbox.
  await page.click('[data-panel-tab="schedules"]');
  await page.waitForSelector('#schedWhen', { timeout: 10_000 });
  await page.fill('#schedWhen', '+10m');
  await page.fill('#schedText', 'later smoke');
  await page.click('[data-schedule-add]');
  check('排定一条定时', await waitText(page, '#sheetBody', 'later smoke'));
  await page.click('[data-schedule-cancel]');
  check('取消定时后列表清空', await waitText(page, '#sheetBody', '没有待运行的定时'));
  await page.click('#sheetClose');
  check('侧板关闭', !(await page.$eval('#sheet', (node) => node.classList.contains('open'))));

  // The per-turn link opens the same chain for that turn.
  await page.click('.y-turn .y-why');
  check('轮末推理链链接打开该轮', await waitText(page, '#sheetBody', 'answered from the diff'));
  check('轮末链接沿用轮标题', (await text(page, '#sheetTitle')).includes('第 4 轮'));
  await page.click('#sheetClose');

  // Widths: drag, keyboard, double-click reset.
  const sideBefore = await page.$eval('#side', (node) => node.getBoundingClientRect().width);
  const sideBox = await (await page.$('#rzSide')).boundingBox();
  await page.mouse.move(sideBox.x + 3, sideBox.y + 200);
  await page.mouse.down();
  await page.mouse.move(sideBox.x + 43, sideBox.y + 200, { steps: 5 });
  await page.mouse.up();
  const sideDragged = await page.$eval('#side', (node) => node.getBoundingClientRect().width);
  check('拖动会话栏变宽', sideDragged > sideBefore + 20, `${Math.round(sideBefore)} -> ${Math.round(sideDragged)}`);
  await page.focus('#rzSide');
  await page.keyboard.press('ArrowRight');
  const sideKeyed = await page.$eval('#side', (node) => node.getBoundingClientRect().width);
  check('ArrowRight 也能调宽 8px', Math.round(sideKeyed) === Math.round(sideDragged) + 8, `${Math.round(sideDragged)} -> ${Math.round(sideKeyed)}`);
  await page.dblclick('#rzSide');
  const sideReset = await page.$eval('#side', (node) => node.getBoundingClientRect().width);
  check('双击复位会话栏宽度', Math.round(sideReset) === 236, String(Math.round(sideReset)));

  await page.click('#details');
  await page.waitForSelector('#sheet.open', { timeout: 10_000 });
  const sheetBefore = await page.$eval('#sheet', (node) => node.getBoundingClientRect().width);
  const sheetBox = await (await page.$('#rzSheet')).boundingBox();
  await page.mouse.move(sheetBox.x + 3, sheetBox.y + 200);
  await page.mouse.down();
  await page.mouse.move(sheetBox.x - 37, sheetBox.y + 200, { steps: 5 });
  await page.mouse.up();
  const sheetDragged = await page.$eval('#sheet', (node) => node.getBoundingClientRect().width);
  check('拖动详情面板变宽', sheetDragged > sheetBefore + 20, `${Math.round(sheetBefore)} -> ${Math.round(sheetDragged)}`);
  await page.dblclick('#rzSheet');
  const sheetReset = await page.$eval('#sheet', (node) => node.getBoundingClientRect().width);
  check('双击复位详情宽度', Math.round(sheetReset) === 420, String(Math.round(sheetReset)));
  await page.click('#sheetClose');

  // Export really downloads the rendered file.
  await page.click('#more');
  const [download] = await Promise.all([
    page.waitForEvent('download', { timeout: 15_000 }),
    page.click('[data-menu="export-md"]'),
  ]);
  const downloadPath = await download.path();
  const exported = downloadPath ? readFileSync(downloadPath, 'utf8') : '';
  check('导出 Markdown 文件名', download.suggestedFilename() === 'work.md', download.suggestedFilename());
  check('导出文件含会话内容', exported.includes('one hunk in src/lib.rs'));

  // Fork: a real child session, opened in place.
  await page.click('#more');
  await page.click('[data-menu="fork"]');
  check('分叉提示出现', await waitText(page, '#toast', '已从这一轮分叉出'));
  await page.waitForFunction(
    () => {
      const on = document.querySelector('.y-session.on');
      return Boolean(on) && on.dataset.session !== 'work';
    },
    null,
    { timeout: 20_000 },
  );
  check('分叉后打开子会话', (await page.$eval('.y-session.on', (node) => node.dataset.session)) !== 'work');
  check('子会话沿用标题', (await text(page, '#title')).includes('show the diff'));

  // Command palette.
  await page.keyboard.press('Control+K');
  await page.waitForSelector('#palette:not([hidden])', { timeout: 10_000 });
  await page.fill('#paletteInput', '计划');
  check('命令面板按输入过滤', (await page.$$eval('.y-cmd', (nodes) => nodes.length)) === 1);
  await page.keyboard.press('Enter');
  check('Enter 打开计划面板', await waitText(page, '#sheetBody', 'show the diff clearly'));
  await page.keyboard.press('Escape');
  check('Esc 关闭面板', !(await page.$eval('#sheet', (node) => node.classList.contains('open'))));

  // Artifacts: image, HTML, Markdown and PDF previews from the embedded page.
  await page.goto(`${origin}/?session=artifact-smoke#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#result:not([hidden])', { timeout: 15_000 });
  check('成果条数', (await text(page, '#resultCount')).includes('6 个产出'));
  await page.click('#resultMore');
  await page.waitForSelector('.y-result.open', { timeout: 10_000 });
  await page.waitForSelector('#artifactPreview img', { timeout: 10_000 });
  check(
    '图片 artifact 预览',
    await page.$eval('#artifactPreview img', (node) => node.complete && node.naturalWidth > 0),
  );
  await page.screenshot({ path: join(shots, 'm7-artifacts.png') });

  await page.click('#artifactList button:has-text("junction-fix.md")');
  check('Markdown artifact 渲染', await waitText(page, '.y-art-md', 'junction 修复验证'));
  await page.click('#artifactList button:has-text("report.html")');
  const frame = await page.waitForSelector('.y-art-frame', { timeout: 10_000 }).catch(() => null);
  check(
    'HTML artifact 走无脚本 iframe',
    Boolean(frame) && ((await frame.getAttribute('srcdoc')) ?? '').includes('36 / 36'),
  );
  await page.click('#artifactList button:has-text("coverage.pdf")');
  const pdf = await page.waitForSelector('.y-art-pdf', { timeout: 10_000 }).catch(() => null);
  check(
    'PDF artifact 走内嵌查看器',
    Boolean(pdf) &&
      (await pdf.getAttribute('type')) === 'application/pdf' &&
      ((await pdf.getAttribute('src')) ?? '').startsWith('blob:'),
  );
  await page.click('#artifactList button:has-text("notes.docx")');
  check('docx 预览读到第一段', await waitText(page, '.y-art-docx', 'M9 文档预览'));
  check('docx 预览读到第二段', await waitText(page, '.y-art-docx', '页面解压'));
  check('docx 转义符还原', await waitText(page, '.y-art-docx', '带 & 一个转义符'));
  await page.click('#artifactList button:has-text("numbers.xlsx")');
  check('xlsx 预览读到单元格', await waitText(page, '.y-art-sheet', '42'));
  check('xlsx 预览带工作表名', await waitText(page, '.y-art-sheet-name', 'sheet1.xml'));

  await page.click('#resultFull');
  await page.waitForSelector('.y-result.full', { timeout: 10_000 });
  check('成果台全屏', await page.$eval('#resultFull', (node) => node.getAttribute('aria-pressed') === 'true'));
  // Evidence in the full-screen state: the preview is unambiguously the thing
  // on screen, with no absolute-positioned siblings to clip it.
  await page.waitForTimeout(300);
  await page.screenshot({ path: join(shots, 'm9-docx-xlsx.png') });
  await page.keyboard.press('Escape');
  check('Esc 退出全屏但保留成果台', await page.$eval('#result', (node) => node.classList.contains('open') && !node.classList.contains('full')));
  await page.keyboard.press('Escape');
  check('再按 Esc 收起成果台', !(await page.$eval('#result', (node) => node.classList.contains('open'))));

  // The real chain: captured from an actual turn (scheduler, gateway, fs,
  // git diff), so the panels are checked against real event shapes too.
  await page.goto(`${origin}/?session=real-chain#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  check('真实执行链：标题取真实输入', (await text(page, '#title')).includes('parseRange'));
  check('真实执行链：行动行有 fs.read', await waitText(page, '#transcript', 'fs.read'));
  check('真实执行链：行动行有 git.diff', await waitText(page, '#transcript', 'git.diff'));
  check('真实执行链：回答是模型原话', await waitText(page, '.y-answer', '闭区间'));
  await page.click('#details');
  check('真实执行链：计划面板读到真实计划', await waitText(page, '#sheetBody', 'off-by-one'));
  await page.click('[data-panel-tab="diff"]');
  check('真实执行链：改动面板读到真实文件', await waitText(page, '#sheetBody', 'src/parser.mjs'));
  check('真实执行链：patch 是 git 的输出', await waitText(page, '#sheetBody', 'diff --git'));
  await page.click('[data-panel-tab="why"]');
  await page.waitForSelector('.y-why-verdict', { timeout: 10_000 }).catch(() => null);
  check('真实执行链：推理链节点成链', (await page.$$eval('.y-node', (nodes) => nodes.length)) >= 10);
  check('真实执行链：判定来自 verdict 节点', await waitText(page, '#sheetBody', 'verdict/answer'));
  check('真实执行链：链自检通过', !(await text(page, '#sheetBody')).includes('自检没有全过'));
  await page.screenshot({ path: join(shots, 'm9-real-chain.png') });
  await page.click('#sheetClose');

  // Approval gate: four decisions, a TTL, and a settled record.
  await page.goto(`${origin}/?session=ui-approval#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#gate [data-decision="once"]', { timeout: 15_000 });
  check('审批卡四个决定', (await page.$$eval('#gate [data-decision]', (nodes) => nodes.length)) === 4);
  check('审批卡有 TTL 文案', /失效/.test(await text(page, '#gate .ttl')));
  const ttlFirst = await text(page, '#gate .ttl');
  await page.waitForTimeout(1_300);
  const ttlSecond = await text(page, '#gate .ttl');
  check('审批 TTL 逐秒走', ttlFirst !== ttlSecond && /后失效/.test(ttlSecond), `${ttlFirst} -> ${ttlSecond}`);
  await page.screenshot({ path: join(shots, 'm7-approval.png') });
  await page.click('#gate [data-decision="once"]');
  // The seeded approval has no live run behind it. What the page must do is
  // record the decision once, stop offering it, and take the card down — a
  // second click on a settled approval would be the interface lying.
  check('允许后记录为已允许', await waitText(page, '#transcript', '已允许', 20_000));
  check(
    '允许后不再可重复点',
    await page.$$eval('#gate [data-decision]', (nodes) => nodes.every((node) => node.disabled)),
  );
  check(
    '决定后审批卡撤下',
    await page
      .waitForFunction(() => document.querySelector('#gate')?.hidden === true, null, { timeout: 10_000 })
      .then(() => true)
      .catch(() => false),
  );

  await page.goto(`${origin}/?session=ui-expired#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#gate [data-resume]', { timeout: 15_000 });
  check('过期审批显示已过期', /已过期/.test(await text(page, '#gate')));
  check('过期审批可继续这一轮', Boolean(await page.$('#gate [data-resume]')));
  await page.screenshot({ path: join(shots, 'm7-expired.png') });

  // A new turn against the echo provider, then reconnect after one dropped SSE.
  await page.goto(`${origin}/?session=work#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  await page.click('#newTask');
  await page.fill('#input', 'hello smoke');
  await page.click('#send');
  check('新任务收到回答', await waitText(page, '.y-answer', 'echo: hello smoke', 25_000));
  check('新任务标题随第一轮更新', await waitText(page, '#title', 'hello smoke', 15_000));
  await page.screenshot({ path: join(shots, 'm7-sent.png') });

  const reconnect = await context.newPage();
  const streamPattern = '**/api/sessions/work/events?**';
  let dropped = 0;
  const handler = async (route) => {
    dropped += 1;
    await route.abort('failed');
    await reconnect.unroute(streamPattern, handler);
  };
  await reconnect.route(streamPattern, handler);
  await reconnect.goto(`${origin}/?session=work#t=${token}`, { waitUntil: 'domcontentloaded' });
  await reconnect.waitForSelector('.y-turn', { timeout: 15_000 });
  await reconnect.fill('#input', 'reconnect check');
  await reconnect.click('#send');
  check('断流一次后仍能收到回答', await waitText(reconnect, '.y-answer', 'echo: reconnect check', 25_000));
  await reconnect.waitForFunction(() => !document.querySelector('.y-live'), null, { timeout: 15_000 });
  const duplicates = await reconnect.$$eval('.y-answer', (nodes) =>
    nodes.filter((node) => (node.textContent ?? '').includes('echo: reconnect check')).length,
  );
  check('重连没有重复 item', dropped === 1 && duplicates === 1, `dropped=${dropped} copies=${duplicates}`);
  await reconnect.close();

  // 1000 items: load, unfold every past turn, and scroll.
  const long = await context.newPage();
  const longStart = Date.now();
  await long.goto(`${origin}/?session=perf-1000#t=${token}`, { waitUntil: 'domcontentloaded' });
  await long.waitForSelector('.y-turn', { timeout: 25_000 });
  const loadMs = Date.now() - longStart;
  check('1000 条 item 首次渲染 < 5s', loadMs < 5_000, `${loadMs}ms`);
  check('长会话保留最近两轮', (await long.$$eval('.y-past-row', (nodes) => nodes.length)) === 2);
  const expandMs = await long.evaluate(async () => {
    const started = performance.now();
    document.querySelector('[data-past-toggle]').click();
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    return performance.now() - started;
  });
  check('展开 97 个旧轮 < 2s', expandMs < 2_000, `${expandMs.toFixed(0)}ms`);
  const scrollMs = await long.evaluate(() => {
    const scroller = document.querySelector('#scroll');
    const started = performance.now();
    scroller.scrollTop = scroller.scrollHeight;
    void scroller.offsetHeight;
    return performance.now() - started;
  });
  check('滚到底部 < 500ms', scrollMs < 500, `${scrollMs.toFixed(0)}ms`);
  check('长会话仍是单列', (await long.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)));
  await long.screenshot({ path: join(shots, 'm7-long.png') });
  await long.close();

  // Reduced motion: the two moving surfaces arrive without travelling.
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.goto(`${origin}/?session=work#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  const motion = await page.evaluate(() => {
    const sheet = getComputedStyle(document.querySelector('#sheet'));
    const result = getComputedStyle(document.querySelector('#result'));
    return {
      sheet: sheet.transitionDuration,
      sheetAnimation: sheet.animationName,
      result: result.transitionDuration,
    };
  });
  check('减少动效：侧板无过渡', motion.sheet === '0s', motion.sheet);
  check('减少动效：成果台无过渡', motion.result === '0s', motion.result);
  await page.emulateMedia({ reducedMotion: 'no-preference' });

  // Narrow screens.
  const narrow = await context.newPage();
  await narrow.setViewportSize({ width: 900, height: 900 });
  await narrow.goto(`${origin}/?session=work#t=${token}`, { waitUntil: 'domcontentloaded' });
  await narrow.waitForSelector('.y-turn', { timeout: 15_000 });
  check('900px 默认收起侧栏', await narrow.$eval('.y-app', (node) => node.classList.contains('y-side-off')));
  await narrow.click('#toggleSide');
  check('☰ 打开抽屉', await narrow.$eval('.y-app', (node) => node.classList.contains('y-side-open')));
  check('抽屉有遮罩', await narrow.isVisible('#sideMask'));
  await narrow.screenshot({ path: join(shots, 'm7-narrow.png') });
  await narrow.click('#sideMask');
  check('点遮罩关抽屉', await narrow.$eval('.y-app', (node) => node.classList.contains('y-side-off')));
  await narrow.close();

  const phone = await context.newPage();
  await phone.setViewportSize({ width: 390, height: 844 });
  await phone.goto(`${origin}/?session=work#t=${token}`, { waitUntil: 'domcontentloaded' });
  await phone.waitForSelector('.y-turn', { timeout: 15_000 });
  check('390 无横向溢出', await phone.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1));
  check('390 输入框仍在', await phone.isVisible('#input'));
  await phone.click('#details');
  check('390 点详情打开侧板（触屏等价）', await phone.$eval('#sheet', (node) => node.classList.contains('open')));
  await phone.screenshot({ path: join(shots, 'm7-390.png') });
  await phone.close();

  // Model switch: the menu comes from /api/meta, the choice is written to the
  // session's log, and the page must not promise it applies to a running turn.
  await page.click('#modelChip');
  await page.waitForSelector('#modelMenu:not([hidden])', { timeout: 10_000 });
  const modelOptions = await page.$$eval('#modelMenu [data-provider]', (nodes) =>
    nodes.map((node) => (node.textContent ?? '').trim()),
  );
  check('模型菜单列出本进程可用的模型', modelOptions.some((label) => label.includes('echo')), modelOptions.join(','));
  await page.click('#modelMenu [data-provider="echo"]');
  check('切换模型给出下一轮生效提示', await waitText(page, '#toast', '下一轮生效', 15_000));
  await page.click('#modelChip');
  await page.screenshot({ path: join(shots, 'm8-model.png') });
  check(
    '菜单把当前模型标成已选',
    await page
      .waitForFunction(() => document.querySelector('#modelMenu [data-provider="echo"]')?.textContent?.startsWith('✓'))
      .then(() => true)
      .catch(() => false),
  );
  await page.keyboard.press('Escape');

  // Archive, restore, delete — last, because deleting moves the page to another
  // session. The fixture is its own session so the rest of the run is unaffected.
  await page.goto(`${origin}/?session=scratch#t=${token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  await page.click('#more');
  await page.click('[data-menu="archive"]');
  check('归档提示', await waitText(page, '#toast', '已归档', 15_000));
  check(
    '归档会话进已归档区',
    await page
      .waitForFunction(() => [...document.querySelectorAll('.y-session.arch')].some((node) => node.dataset.session === 'scratch'))
      .then(() => true)
      .catch(() => false),
  );
  check('侧栏出现已归档分区', (await text(page, '#sessionList')).includes('已归档'));
  await page.screenshot({ path: join(shots, 'm8-archive.png') });
  await page.waitForFunction(
    () => document.querySelector('[data-menu="archive"]')?.textContent?.trim() === '恢复此会话',
    null,
    { timeout: 10_000 },
  );
  await page.click('#more');
  await page.click('[data-menu="archive"]');
  check('恢复提示', await waitText(page, '#toast', '已恢复', 15_000));
  check(
    '恢复后离开已归档区',
    await page
      .waitForFunction(() => ![...document.querySelectorAll('.y-session.arch')].some((node) => node.dataset.session === 'scratch'))
      .then(() => true)
      .catch(() => false),
  );

  await page.click('#more');
  await page.click('[data-menu="remove"]');
  check('删除先要确认', (await text(page, '#menu [data-menu="remove"]')).trim() === '再点一次删除');
  await page.click('#menu [data-menu="remove"]');
  check('删除提示', await waitText(page, '#toast', '已删除这个会话', 20_000));
  check(
    '删除后侧栏不再有它',
    await page
      .waitForFunction(() => !document.querySelector('[data-session="scratch"]'), null, { timeout: 10_000 })
      .then(() => true)
      .catch(() => false),
  );

  check('全程无 console error / pageerror / 4xx', errors.length === 0, errors.join(' | '));

  if (failures.length) {
    throw new Error(`${failures.length} check(s) failed:\n- ${failures.join('\n- ')}`);
  }
  console.log(
    JSON.stringify(
      {
        ok: true,
        checks,
        perf: { loadMs, expandMs: Math.round(expandMs), scrollMs: Math.round(scrollMs) },
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
        ],
      },
      null,
      2,
    ),
  );
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
    rmSync(temp, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  } catch {
    // A leftover temp directory is not a test failure.
  }
}
