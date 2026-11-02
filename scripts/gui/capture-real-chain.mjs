// Capture one real execution chain for the GUI fixtures.
//
// "Real" here means the real binary: the real scheduler, the real tool gateway,
// real files on disk and a real `git diff`. The model is a scripted
// OpenAI-compatible endpoint on loopback, because this machine has no
// AGENT_MODEL_API_KEY; the log it writes has exactly the shape a vendor model
// would produce — `model/request.prepared`, `plan/updated`, `tool/started`,
// `tool/completed`, `reason/recorded`, `turn/completed` — which is what the
// page reads and what the smoke replays.
//
//   node scripts/gui/capture-real-chain.mjs
//
// Writes `scripts/gui/fixtures/real-chain/events.jsonl` (scrubbed of the
// throwaway directory) and prints the event tally. Point `AGENT_MODEL_BASE_URL`
// and `AGENT_MODEL_API_KEY` at a real endpoint and pass `--live` to capture the
// same flow from a vendor model instead of the stub.

import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../..');
const binary = process.env.AGENT_BIN || join(root, 'target', 'debug', 'agent.exe');
const live = process.argv.includes('--live');
const fixtureDir = join(here, 'fixtures', 'real-chain');

if (!existsSync(binary)) {
  throw new Error(`agent binary not found at ${binary}; run cargo build -p agent first`);
}

const temp = mkdtempSync(join(tmpdir(), 'yaoyao-real-chain-'));
const workspace = join(temp, 'workspace');
const dataDir = join(temp, 'data');
mkdirSync(workspace, { recursive: true });
mkdirSync(dataDir, { recursive: true });

const BUGGY = `export function parseRange(text) {
  const [start, end] = text.split('-').map(Number);
  // end is exclusive here, which is not what callers expect
  return { start, end: end + 1 };
}
`;

const FIXED = `export function parseRange(text) {
  const [start, end] = text.split('-').map(Number);
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) {
    throw new Error(\`bad range: \${text}\`);
  }
  return { start, end };
}
`;

function run(command, args, options = {}) {
  return new Promise((done) => {
    const child = spawn(command, args, { ...options, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => (stdout += chunk));
    child.stderr.on('data', (chunk) => (stderr += chunk));
    child.on('close', (code) => done({ code, stdout, stderr }));
  });
}

function toolCall(name, args, id) {
  return {
    id,
    name,
    arguments: JSON.stringify(args),
  };
}

/** What the stub model says next, keyed by how many tool rounds have happened. */
function script(toolRounds) {
  if (toolRounds === 0) {
    return {
      call: toolCall(
        'plan.update',
        {
          goal: '修掉 parseRange 的 off-by-one，并用 git diff 证明改动',
          items: [
            { id: 'read', text: '读 src/parser.mjs，确认 end 的语义', status: 'doing' },
            { id: 'fix', text: '改成闭区间并补参数校验', status: 'todo' },
            { id: 'diff', text: '用 git.diff 展示改动', status: 'todo' },
          ],
          facts: ['工作区是一个 git 仓库'],
        },
        'call-plan-1',
      ),
    };
  }
  if (toolRounds === 1) return { call: toolCall('fs.read', { path: 'src/parser.mjs' }, 'call-read-1') };
  if (toolRounds === 2) {
    return { call: toolCall('fs.write', { path: 'src/parser.mjs', content: FIXED }, 'call-write-1') };
  }
  if (toolRounds === 3) return { call: toolCall('git.diff', { staged: false }, 'call-diff-1') };
  return {
    text:
      'parseRange 原来是 [start, end)，调用方按闭区间理解，于是 1-3 会变成 1..4。' +
      '改成闭区间并拒绝 end < start；git diff 里能看到这一处改动。',
  };
}

/** One OpenAI-compatible chat completion, streamed as SSE. */
function answer(res, body) {
  const messages = Array.isArray(body.messages) ? body.messages : [];
  const rounds = messages.filter(
    (message) => message.role === 'assistant' && Array.isArray(message.tool_calls) && message.tool_calls.length,
  ).length;
  const step = script(rounds);
  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  });
  const send = (payload) => res.write(`data: ${JSON.stringify(payload)}\n\n`);
  const base = { id: 'stub-1', object: 'chat.completion.chunk', created: Math.floor(Date.now() / 1000) };
  if (step.call) {
    send({
      ...base,
      choices: [
        {
          index: 0,
          delta: {
            role: 'assistant',
            tool_calls: [
              {
                index: 0,
                id: step.call.id,
                type: 'function',
                function: { name: step.call.name, arguments: step.call.arguments },
              },
            ],
          },
          finish_reason: null,
        },
      ],
    });
    send({ ...base, choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] });
  } else {
    send({ ...base, choices: [{ index: 0, delta: { role: 'assistant', content: step.text }, finish_reason: null }] });
    send({ ...base, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] });
  }
  send({ ...base, choices: [], usage: { prompt_tokens: 900, completion_tokens: 120, total_tokens: 1020 } });
  res.write('data: [DONE]\n\n');
  res.end();
}

async function main() {
  // A workspace with a real git history, so `git.diff` has something to say.
  mkdirSync(join(workspace, 'src'), { recursive: true });
  writeFileSync(join(workspace, 'src', 'parser.mjs'), BUGGY);
  writeFileSync(join(workspace, 'README.md'), '# real-chain fixture\n\nA tiny parser used to capture one real run.\n');
  await run('git', ['init', '--quiet'], { cwd: workspace });
  await run('git', ['config', 'user.email', 'smoke@example.invalid'], { cwd: workspace });
  await run('git', ['config', 'user.name', 'smoke'], { cwd: workspace });
  await run('git', ['add', '.'], { cwd: workspace });
  await run('git', ['commit', '--quiet', '-m', 'initial'], { cwd: workspace });

  const stub = createServer((req, res) => {
    let raw = '';
    req.on('data', (chunk) => (raw += chunk));
    req.on('end', () => {
      if (req.method !== 'POST') {
        res.writeHead(404).end();
        return;
      }
      let body = {};
      try {
        body = JSON.parse(raw || '{}');
      } catch {
        // An unreadable body still gets a well-formed stream; the run will fail
        // on its own terms and say so in the log.
      }
      answer(res, body);
    });
  });
  await new Promise((done) => stub.listen(0, '127.0.0.1', done));
  const port = stub.address().port;

  const env = {
    ...process.env,
    AGENT_WORKSPACE_DIR: workspace,
    AGENT_DATA_DIR: dataDir,
    AGENT_MODEL_PROVIDER: 'openai',
    AGENT_MODEL_BASE_URL: live ? process.env.AGENT_MODEL_BASE_URL : `http://127.0.0.1:${port}/v1`,
    AGENT_MODEL_NAME: live ? process.env.AGENT_MODEL_NAME : 'scripted-stub',
    AGENT_MODEL_API_KEY: process.env.AGENT_MODEL_API_KEY || 'stub-key',
  };
  const result = await run(
    binary,
    ['ask', '--session', 'real-chain', '修一下 src/parser.mjs 里 parseRange 的边界问题，并用 git.diff 展示改动'],
    { cwd: workspace, env },
  );
  stub.close();
  if (result.code !== 0) {
    throw new Error(`the run failed (exit ${result.code}):\n${result.stdout}\n${result.stderr}`);
  }

  const log = readFileSync(join(dataDir, 'real-chain', 'events.jsonl'), 'utf8');
  const records = log
    .trim()
    .split('\n')
    .map((line) => JSON.parse(line));
  // The throwaway directory is noise in a fixture that ships; nothing else is
  // rewritten, so the timestamps, ids and bytes are the run's own.
  const scrubbed = JSON.stringify(records).split(workspace.replace(/\\/g, '\\\\')).join('<workspace>');
  const events = JSON.parse(scrubbed);

  mkdirSync(fixtureDir, { recursive: true });
  writeFileSync(join(fixtureDir, 'events.jsonl'), events.map((row) => JSON.stringify(row)).join('\n') + '\n');
  writeFileSync(
    join(fixtureDir, 'README.md'),
    `# real-chain fixture\n\nCaptured by \`node scripts/gui/capture-real-chain.mjs\` on ${new Date().toISOString()}.\n\n` +
      'One turn driven by the real binary: scheduler, tool gateway, filesystem and `git diff`.\n' +
      (live ? 'The model was the configured vendor endpoint.\n' : 'The model was the scripted loopback stub (no vendor credential on this machine).\n') +
      'The only rewritten text is the throwaway workspace path (`<workspace>`).\n',
  );

  const tally = {};
  for (const record of events) {
    const type = record.event.event_type;
    tally[type] = (tally[type] ?? 0) + 1;
  }
  console.log(JSON.stringify({ ok: true, events: events.length, tally, fixture: join(fixtureDir, 'events.jsonl') }, null, 2));
}

try {
  await main();
} finally {
  try {
    rmSync(temp, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  } catch {
    // A leftover temp directory is not a failure.
  }
}
