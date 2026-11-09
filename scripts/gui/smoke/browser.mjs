// The real binary and browser half of the GUI smoke test.
import { spawn } from 'node:child_process';
import { closeSync, existsSync, openSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { chromium } from 'playwright-core';

const edge =
  process.env.EDGE_PATH ||
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe';

function waitForUrl(logPath, timeoutMs = 30_000) {
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

export async function startBrowserHarness({ root, workspace, dataDir, logPath }) {
  const binary = process.env.AGENT_BIN || join(root, 'target', 'debug', 'agent.exe');
  if (!existsSync(binary)) {
    throw new Error(`agent binary not found at ${binary}; run cargo build -p agent first`);
  }
  const logFd = openSync(logPath, 'a');
  const server = spawn(binary, ['web', '--port', '0'], {
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
  const url = waitForUrl(logPath);
  const origin = url.split('/#t=')[0];
  const token = url.split('#t=')[1];
  if (!origin || !token) throw new Error('agent web did not print a usable token URL');
  const browser = await chromium.launch({ executablePath: edge, headless: true });
  const context = await browser.newContext({
    viewport: { width: 1440, height: 1000 },
    acceptDownloads: true,
  });
  return {
    browser,
    context,
    origin,
    token,
    async close() {
      await browser.close().catch(() => {});
      if (server.exitCode === null) {
        const exited = new Promise((resolve) => server.once('exit', resolve));
        server.kill();
        // Windows keeps the log file open until the child has actually exited.
        await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 2000))]);
      }
    },
  };
}

export async function openPage(context, origin, path) {
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

export async function text(page, selector) {
  try {
    return (await page.textContent(selector, { timeout: 8_000 })) ?? '';
  } catch {
    return '';
  }
}

export async function waitText(page, selector, needle, timeout = 15_000) {
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
