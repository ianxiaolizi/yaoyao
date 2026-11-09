// Check a video artifact by decoding it, not by looking at its name.
//
// A `.webm` that exists and has a plausible size is not a video: the export can
// be truncated, the codec can be unsupported, or the frame can be blank. This
// opens the file in a real browser, waits for the media metadata, seeks to a
// frame, and draws that frame to a canvas. The check passes only when the
// browser reports a positive duration, real dimensions, and a frame that
// contains more than one colour.
//
// Usage:
//   node scripts/verify/video.mjs --target <path-or-url> [--screenshot <png>]
//        [--timeout-ms <n>] [--min-duration-ms <n>] [--max-duration-ms <n>]
//        [--width <n>] [--height <n>]
//
// It prints one `CHECK_OK …` line on success and `CHECK_FAILED …` on stderr
// with every reason it found, then exits 1. Bad arguments exit 2.
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
const minDurationMs = optionalNumber(args, 'min-duration-ms');
const maxDurationMs = optionalNumber(args, 'max-duration-ms');
const wantWidth = optionalNumber(args, 'width');
const wantHeight = optionalNumber(args, 'height');
const screenshot = args.screenshot;

let url;
try {
  url = targetUrl(target);
} catch (error) {
  usage(error.message);
}

let browser;
try {
  // A file:// page is its own opaque origin, and Chrome taints a canvas that
  // drew a frame from a media document unless file access is opened. This is a
  // local check of a local artifact, so the relaxation is scoped to the browser
  // this script starts and nothing else.
  const launchArgs = url.startsWith('file:') ? ['--allow-file-access-from-files'] : [];
  browser = await chromium.launch({ executablePath: edge, headless: true, args: launchArgs });
  const page = await browser.newPage();
  page.setDefaultTimeout(timeoutMs);

  // Navigating to the media file itself gives the page the file's own origin,
  // so drawing a frame to a canvas is not blocked as cross-origin.
  await page.goto(url, { waitUntil: 'load', timeout: timeoutMs });
  await page.locator('video').first().waitFor({ state: 'attached', timeout: timeoutMs });

  const decoded = await page.evaluate(async (limit) => {
    const video = document.querySelector('video');
    if (!video) {
      throw new Error('the browser rendered no video element');
    }
    const waitFor = (event, ms) =>
      new Promise((resolve, reject) => {
        const timer = setTimeout(
          () => reject(new Error(`timed out after ${ms}ms waiting for ${event}`)),
          ms,
        );
        video.addEventListener(
          event,
          () => {
            clearTimeout(timer);
            resolve();
          },
          { once: true },
        );
      });

    const failed = new Promise((_, reject) => {
      video.addEventListener(
        'error',
        () =>
          reject(
            new Error(
              `the browser could not decode the file: ${
                video.error?.message || `media error code ${video.error?.code ?? 'unknown'}`
              }`,
            ),
          ),
        { once: true },
      );
    });
    const metadata =
      video.readyState >= 1
        ? Promise.resolve()
        : waitFor('loadedmetadata', limit);
    await Promise.race([metadata, failed]);

    if (!Number.isFinite(video.duration)) {
      // A WebM written by MediaRecorder carries no duration until the browser
      // is pushed past its end. Seeking far ahead makes it compute one; this is
      // the standard trick, not a special case for our own fixtures.
      const settled = waitFor('durationchange', 5_000).catch(() => {});
      video.currentTime = 1e101;
      await settled;
      video.currentTime = 0;
    }

    const duration = video.duration;
    if (!Number.isFinite(duration) || duration <= 0) {
      throw new Error(`the file reports no usable duration (${duration})`);
    }
    if (video.videoWidth <= 0 || video.videoHeight <= 0) {
      throw new Error(`the file reports no frame size (${video.videoWidth}x${video.videoHeight})`);
    }

    const canvas = document.createElement('canvas');
    canvas.width = video.videoWidth;
    canvas.height = video.videoHeight;
    const context = canvas.getContext('2d');

    // Two frames, not one: a title card or an opening fade is legitimately
    // uniform, and a single sample would call it blank. The question is
    // whether the decoder produced *anything visible* across the file.
    const sample = async (fraction) => {
      const seeked = waitFor('seeked', limit);
      video.currentTime = Math.min(duration * fraction, Math.max(0, duration - 0.001));
      await Promise.race([seeked, failed]);
      context.drawImage(video, 0, 0, canvas.width, canvas.height);
      let pixels;
      try {
        pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
      } catch (error) {
        throw new Error(`the decoded frame could not be read back: ${error.message}`);
      }
      let lit = false;
      const colours = new Set();
      for (let index = 0; index < pixels.length; index += 4 * 97) {
        const red = pixels[index];
        const green = pixels[index + 1];
        const blue = pixels[index + 2];
        const alpha = pixels[index + 3];
        if (alpha > 0 && red + green + blue > 24) {
          lit = true;
        }
        if (colours.size < 256) {
          colours.add((red << 16) | (green << 8) | blue);
        }
      }
      return { lit, colours: colours.size };
    };

    const early = await sample(0.3);
    const late = await sample(0.6);

    return {
      duration,
      width: video.videoWidth,
      height: video.videoHeight,
      lit: early.lit || late.lit,
      colours: Math.max(early.colours, late.colours),
    };
  }, timeoutMs);

  const reasons = [];
  const durationMs = Math.round(decoded.duration * 1000);
  if (!decoded.lit) {
    reasons.push('the decoded frames are blank: no sampled pixel is visible');
  }
  if (minDurationMs !== undefined && durationMs < minDurationMs) {
    reasons.push(`duration ${durationMs}ms is under the ${minDurationMs}ms minimum`);
  }
  if (maxDurationMs !== undefined && durationMs > maxDurationMs) {
    reasons.push(`duration ${durationMs}ms is over the ${maxDurationMs}ms maximum`);
  }
  if (wantWidth !== undefined && decoded.width !== wantWidth) {
    reasons.push(`width is ${decoded.width}, not ${wantWidth}`);
  }
  if (wantHeight !== undefined && decoded.height !== wantHeight) {
    reasons.push(`height is ${decoded.height}, not ${wantHeight}`);
  }
  if (screenshot) {
    mkdirSync(dirname(resolve(screenshot)), { recursive: true });
    await page.locator('video').first().screenshot({ path: screenshot });
  }

  if (reasons.length > 0) {
    throw checkFailure(reasons);
  }
  console.log(
    `CHECK_OK video target=${url} duration_ms=${durationMs} width=${decoded.width}` +
      ` height=${decoded.height} colours=${decoded.colours}` +
      (screenshot ? ` screenshot=${resolve(screenshot)}` : ''),
  );
} catch (error) {
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
  const value = optionalNumber(parsed, name);
  return value === undefined ? fallback : value;
}

function optionalNumber(parsed, name) {
  const raw = parsed[name];
  if (raw === undefined) {
    return undefined;
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
    console.error(`CHECK_FAILED video target=${url}: ${reason}`);
  }
}

function usage(message) {
  console.error(`video check: ${message}`);
  console.error('usage: node video.mjs --target <path-or-url> [--screenshot <png>] [--timeout-ms <n>]');
  process.exit(2);
}
