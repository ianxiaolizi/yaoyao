// The verifier scripts' own test: every check is run against a page or a file
// that must pass, and against one that must fail.
//
// A check that cannot fail is decoration. Each case below spawns the same
// script `agent verify` spawns, as a child process, so the exit status and the
// `CHECK_OK` / `CHECK_FAILED` lines are exercised exactly as a recipe sees
// them.
//
// Run from the repository root:
//
//   npm ci --prefix scripts/verify
//   node scripts/verify/selftest.mjs

import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, writeFileSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { chromium } from 'playwright-core';

const here = import.meta.dirname;
const edge =
  process.env.EDGE_PATH ||
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe';

if (!existsSync(edge)) {
  console.error(`selftest: no browser at ${edge}; set EDGE_PATH to a Chromium binary`);
  process.exit(2);
}

const temp = mkdtempSync(join(tmpdir(), 'yaoyao-verify-selftest-'));
const failures = [];

function write(name, text) {
  const path = join(temp, name);
  writeFileSync(path, text);
  return path;
}

function run(script, args) {
  const result = spawnSync(process.execPath, [join(here, script), ...args], {
    encoding: 'utf8',
  });
  return { code: result.status, out: result.stdout ?? '', err: result.stderr ?? '' };
}

function expect(name, condition, detail) {
  if (condition) {
    console.log(`PASS ${name}`);
  } else {
    failures.push(`${name}: ${detail}`);
    console.error(`FAIL ${name}: ${detail}`);
  }
}

// ---------------------------------------------------------------- web checks

const okPage = write(
  'ok.html',
  '<!doctype html><title>ok</title><body><h1>rendered</h1><canvas width="8" height="8"></canvas></body>',
);
const shot = join(temp, 'ok.png');
const ok = run('web.mjs', ['--target', okPage, '--screenshot', shot]);
expect('web renders and passes', ok.code === 0, `exit ${ok.code}: ${ok.err}`);
expect('web prints CHECK_OK', ok.out.includes('CHECK_OK web'), ok.out);
expect(
  'web writes a screenshot',
  existsSync(shot) && statSync(shot).size > 0,
  'the screenshot is missing or empty',
);

const throwing = write(
  'throws.html',
  '<!doctype html><title>throws</title><body><p>text is here</p><script>throw new Error("boom")</script></body>',
);
const thrown = run('web.mjs', ['--target', throwing]);
expect('a page error fails the check', thrown.code === 1, `exit ${thrown.code}`);
expect('the failure names the page error', thrown.err.includes('page error'), thrown.err);

const empty = write('empty.html', '<!doctype html><title>empty</title><body></body>');
const blank = run('web.mjs', ['--target', empty]);
expect('an empty render fails the check', blank.code === 1, `exit ${blank.code}`);
expect('the failure says nothing rendered', blank.err.includes('nothing rendered'), blank.err);

const missing = run('web.mjs', ['--target', join(temp, 'not-there.html')]);
expect('a missing target is a check that did not run', missing.code === 2, `exit ${missing.code}`);

// -------------------------------------------------------------- video checks

const text = write('not-a-video.webm', 'this is text, not a container');
const notVideo = run('video.mjs', ['--target', text]);
expect('a text file is not a video', notVideo.code === 1, `exit ${notVideo.code}`);
expect(
  'the failure says the file could not be decoded',
  notVideo.err.includes('CHECK_FAILED video'),
  notVideo.err,
);

// A real WebM, recorded in the browser the check will use. Skipped loudly when
// this browser cannot record — never passed silently.
let generated = null;
try {
  generated = await recordWebm(join(temp, 'recorded.webm'));
} catch (error) {
  console.error(`SKIP video generation: ${error.message}`);
}

if (generated) {
  const videoShot = join(temp, 'recorded.png');
  const decoded = run('video.mjs', [
    '--target',
    generated,
    '--width',
    '64',
    '--height',
    '64',
    '--min-duration-ms',
    '100',
    '--screenshot',
    videoShot,
  ]);
  expect('a recorded webm decodes and passes', decoded.code === 0, `exit ${decoded.code}: ${decoded.err}`);
  expect('video prints duration and size', decoded.out.includes('duration_ms=') && decoded.out.includes('width=64'), decoded.out);
  expect(
    'video writes a screenshot',
    existsSync(videoShot) && statSync(videoShot).size > 0,
    'the screenshot is missing or empty',
  );
}

// ---------------------------------------------------------------- the result

if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed`);
  process.exit(1);
}
console.log(`\nselftest ok (${generated ? 'web + video' : 'web; video generation skipped'})`);

// ------------------------------------------------------------------ helpers

async function recordWebm(path) {
  const browser = await chromium.launch({ executablePath: edge, headless: true });
  try {
    const page = await browser.newPage();
    await page.goto('about:blank');
    const bytes = await page.evaluate(async () => {
      const canvas = document.createElement('canvas');
      canvas.width = 64;
      canvas.height = 64;
      const context = canvas.getContext('2d');
      let frame = 0;
      const paint = setInterval(() => {
        context.fillStyle = frame % 2 === 0 ? '#ff0000' : '#0000ff';
        context.fillRect(0, 0, canvas.width, canvas.height);
        frame += 1;
      }, 33);

      const stream = canvas.captureStream(30);
      const type = MediaRecorder.isTypeSupported('video/webm;codecs=vp8')
        ? 'video/webm;codecs=vp8'
        : 'video/webm';
      if (typeof MediaRecorder === 'undefined' || !MediaRecorder.isTypeSupported(type)) {
        throw new Error('this browser cannot record webm');
      }
      const chunks = [];
      const recorder = new MediaRecorder(stream, { mimeType: type });
      recorder.ondataavailable = (event) => {
        if (event.data.size > 0) {
          chunks.push(event.data);
        }
      };
      const stopped = new Promise((resolve) => {
        recorder.onstop = resolve;
      });
      recorder.start();
      await new Promise((resolve) => setTimeout(resolve, 800));
      recorder.stop();
      await stopped;
      clearInterval(paint);
      if (chunks.length === 0) {
        throw new Error('the recorder produced no data');
      }
      const blob = new Blob(chunks, { type: 'video/webm' });
      const buffer = new Uint8Array(await blob.arrayBuffer());
      return Array.from(buffer);
    });
    writeFileSync(path, Buffer.from(bytes));
    return path;
  } finally {
    await browser.close();
  }
}
