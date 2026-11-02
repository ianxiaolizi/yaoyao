// Capture one real `subagent.run` for the GUI fixture.
//
// The binary, kernel, policy and in-process subagent provider all run for real;
// only the model is the echo provider, because this machine has no vendor key.
// The parent log therefore contains a genuine `subagent/spawned` plus the
// child's settlement, which is what the GUI's 子代理 tab reads.
//
//   node scripts/gui/capture-real-subagent.mjs

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../..');
const binary = process.env.AGENT_BIN || join(root, 'target', 'debug', 'agent.exe');
const fixtureDir = join(here, 'fixtures', 'real-subagent');

if (!existsSync(binary)) {
  throw new Error(`agent binary not found at ${binary}; run cargo build -p agent first`);
}

const temp = mkdtempSync(join(tmpdir(), 'yaoyao-real-subagent-'));
const workspace = join(temp, 'workspace');
const dataDir = join(temp, 'data');
const config = join(temp, 'user.toml');
mkdirSync(workspace, { recursive: true });
mkdirSync(dataDir, { recursive: true });
writeFileSync(join(workspace, 'notes.txt'), 'remember the milk\n');
writeFileSync(
  config,
  [
    'capabilities = ["subagent.spawn"]',
    'plugins = ["subagent.inner@1.0.0"]',
    '',
    '[subagents.inner]',
    'kind = "in-process"',
    'requires = ["subagent.spawn"]',
    '',
    '[profiles.p]',
    'tools = { allow = ["subagent.spawn"] }',
    'plugins = { enable = ["subagent.inner"] }',
    '',
  ].join('\n'),
);

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

async function main() {
  const env = {
    ...process.env,
    AGENT_WORKSPACE_DIR: workspace,
    AGENT_DATA_DIR: dataDir,
    AGENT_MODEL_PROVIDER: 'echo',
    AGENT_USER_CONFIG: config,
  };
  delete env.AGENT_MANAGED_POLICY;
  delete env.AGENT_SANDBOX;

  // A child forks the parent's last closed turn, so the parent has to have one.
  const parent = await run(binary, ['--profile', 'p', 'run', 'hello'], { cwd: workspace, env });
  if (parent.code !== 0) {
    throw new Error(`parent run failed (exit ${parent.code}):\n${parent.stdout}\n${parent.stderr}`);
  }

  const args = [
    '--profile',
    'p',
    'tool',
    'subagent.run',
    JSON.stringify({ provider: 'inner', objective: 'check the notes' }),
  ];
  const result = await run(binary, args, { cwd: workspace, env });
  if (result.code !== 0) {
    throw new Error(`subagent.run failed (exit ${result.code}):\n${result.stdout}\n${result.stderr}`);
  }
  if (!result.stdout.includes('completed')) {
    throw new Error(`no completed child in the output:\n${result.stdout}\n${result.stderr}`);
  }

  const log = readFileSync(join(dataDir, 'default', 'events.jsonl'), 'utf8');
  const records = log
    .trim()
    .split('\n')
    .map((line) => JSON.parse(line));
  const scrubbed = JSON.stringify(records).split(workspace.replace(/\\/g, '\\\\')).join('<workspace>');
  const events = JSON.parse(scrubbed);

  const spawned = events.filter((row) => row.event.event_type === 'subagent/spawned').length;
  const settled = events.filter((row) =>
    ['subagent/completed', 'subagent/failed', 'subagent/cancelled'].includes(row.event.event_type),
  ).length;
  if (spawned !== 1 || settled !== 1) {
    throw new Error(`expected one spawned + one settled child, got ${spawned} / ${settled}`);
  }

  mkdirSync(fixtureDir, { recursive: true });
  writeFileSync(
    join(fixtureDir, 'events.jsonl'),
    events.map((row) => JSON.stringify(row)).join('\n') + '\n',
  );
  writeFileSync(
    join(fixtureDir, 'README.md'),
    `# real-subagent fixture\n\nCaptured by \`node scripts/gui/capture-real-subagent.mjs\` on ${new Date().toISOString()}.\n\n` +
      'A real binary, kernel, policy gate and in-process subagent provider; the model was the echo provider ' +
      '(no vendor credential on this machine). The only rewritten text is the throwaway workspace path.\n',
  );
  console.log(JSON.stringify({ ok: true, events: events.length, spawned, settled, fixture: fixtureDir }, null, 2));
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
