// Check a web artifact the way a person would: open it, let it run, and look
// at what actually rendered.
//
// A file that parses is not a page that works. This opens the page in a real
// browser, waits for it to load, and fails when:
//
//   - the browser reported a script error (pageerror) or a console error;
//   - an http(s) subresource answered 4xx/5xx;
//   - the body has no visible text and no canvas, svg, video, img or iframe —
//     an empty render is not a page.
//
// It prints one `CHECK_OK …` line on success and `CHECK_FAILED …` on stderr
// with every reason it found, then exits 1. Bad arguments exit 2: the check did
// not run, which is a different answer from "it ran and failed".
//
// Usage:
//   node scripts/verify/web.mjs --target <path-or-url> [--screenshot <png>]
//                               [--timeout-ms <n>]
//
// Edge is expected at the default Windows path; override with EDGE_PATH.

import { existsSync, mkdirSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { chromium } from 'playwright-core';

const edge =
  process.env.EDGE_PATH ||
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe';

const args = parseArgs(process.argv.slice(2));
const target = args.target;
if (!target) {
  usage('--target is required');
}
const timeoutMs = numberArg(args, 'timeout-ms', 30_000);
const screenshot = args.screenshot;

let url;
try {
  url = targetUrl(target);
} catch (error) {
  usage(error.message);
}

const reasons = [];
let browser;

try {
  browser = await chromium.launch({ executablePath: edge, headless: true });
  const page = await browser.newPage();

  page.on('pageerror', (error) => {
    reasons.push(`page error: ${error.message}`);
  });
  page.on('console', (message) => {
    if (message.type() === 'error') {
      reasons.push(`console error: ${message.text()}`);
    }
  });
  page.on('response', (response) => {
    const remote = /^https?:/i.test(response.url());
    if (remote && response.status() >= 400) {
      reasons.push(`HTTP ${response.status()}: ${response.url()}`);
    }
  });

  const response = await page.goto(url, { waitUntil: 'load', timeout: timeoutMs });
  if (response && /^https?:/i.test(url) && response.status() >= 400) {
    reasons.push(`HTTP ${response.status()}: ${url}`);
  }

  const rendered = await page.evaluate(() => {
    const body = document.body;
    const text = (body?.innerText ?? document.documentElement?.innerText ?? '').trim();
    const media = body?.querySelector('canvas, svg, video, img, iframe, object') ?? null;
    return { textLength: text.length, media: media !== null };
  });
  if (rendered.textLength === 0 && !rendered.media) {
    reasons.push('nothing rendered: no visible text and no canvas/svg/video/img/iframe');
  }

  if (screenshot) {
    mkdirSync(dirname(resolve(screenshot)), { recursive: true });
    await page.screenshot({ path: screenshot, fullPage: true });
  }

  if (reasons.length > 0) {
    throw checkFailure(reasons);
  }
  console.log(
    `CHECK_OK web target=${url} text=${rendered.textLength} media=${rendered.media}` +
      (screenshot ? ` screenshot=${resolve(screenshot)}` : ''),
  );
} catch (error) {
  // A navigation timeout is a failure of the page, not of the harness: the
  // check ran and the answer is "it did not load". Anything else is also a
  // failed check — this script never claims a pass it did not see.
  report(error.failed ? error.reasons : [error.message]);
  process.exitCode = 1;
} finally {
  // Reached on the failure path too: a check that fails must not leave a
  // browser behind, because a model may run it many times in a row.
  await browser?.close();
}

// ------------------------------------------------------------------ helpers

function parseArgs(argv) {
  const parsed = {};
  for (let index = 0; index < argv.length; index += 1) {
    const word = argv[index];
    const [name, inline] = word.startsWith('--') && word.includes('=')
      ? [word.slice(0, word.indexOf('=')), word.slice(word.indexOf('=') + 1)]
      : [word, undefined];
    if (!name.startsWith('--')) {
      usage(`unexpected argument '${word}'`);
    }
    const key = name.slice(2);
    const value = inline ?? argv[index + 1];
    if (value === undefined || value.startsWith('--')) {
      usage(`${name} needs a value`);
    }
    parsed[key] = value;
    if (inline === undefined) {
      index += 1;
    }
  }
  return parsed;
}

function numberArg(parsed, name, fallback) {
  const raw = parsed[name];
  if (raw === undefined) {
    return fallback;
  }
  const value = Number(raw);
  if (!Number.isFinite(value) || value <= 0) {
    usage(`--${name} must be a positive number, not '${raw}'`);
  }
  return value;
}

function targetUrl(value) {
  if (/^(https?|file):\/\//i.test(value)) {
    return value;
  }
  let path = resolve(value);
  if (existsSync(path) && statSync(path).isDirectory()) {
    path = join(path, 'index.html');
  }
  if (!existsSync(path)) {
    throw new Error(`target not found: ${path}`);
  }
  return pathToFileURL(path).href;
}

// A failure the check itself found, carried to the one place that prints.
function checkFailure(reasons) {
  const error = new Error(reasons.join('; '));
  error.failed = true;
  error.reasons = reasons;
  return error;
}

function report(reasons) {
  for (const reason of reasons) {
    console.error(`CHECK_FAILED web target=${url}: ${reason}`);
  }
}

function usage(message) {
  console.error(`web check: ${message}`);
  console.error('usage: node web.mjs --target <path-or-url> [--screenshot <png>] [--timeout-ms <n>]');
  process.exit(2);
}
