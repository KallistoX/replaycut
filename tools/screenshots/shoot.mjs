// Takes the pictures the README and the website use, from the real page of a
// running service - not from a mockup, so they cannot drift from the UI.
//
//   node shoot.mjs --base http://localhost:8425 --out ../../docs/images [--themes]
//
// Two things need a trick. The page holds an open Server-Sent-Events stream,
// so a browser waiting for the network to fall quiet waits forever: Playwright
// shoots when we say so instead. And a <video> is composited by the GPU and
// comes out black in a screenshot, so its poster - the thumbnail the service
// made from the recording - is laid over it before the picture is taken.

import { execFileSync } from 'node:child_process';
import { mkdirSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const here = dirname(fileURLToPath(import.meta.url));

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i > -1 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const base = arg('base', 'http://localhost:8425').replace(/\/$/, '');
const outDir = arg('out', join(here, '..', '..', 'docs', 'images'));
const withThemes = process.argv.includes('--themes');
const tmp = join(here, '.shots');

// Six of the twelve themes, enough for the colour cycle on the website: the
// default, two dark classics, one warm, one cold, one light.
const THEMES = ['wardogs', 'nord', 'catppuccin-mocha', 'dracula', 'gruvbox-dark', 'catppuccin-latte'];

const IN_AT = 1.4;
const OUT_AT = 4.6;

mkdirSync(tmp, { recursive: true });
mkdirSync(join(outDir, 'themes'), { recursive: true });

function ffmpeg(args) {
  execFileSync('ffmpeg', ['-v', 'error', '-y', ...args]);
}

/** Wait for the clips page, put marks on the timeline, hide the caret. */
async function prepare(page) {
  await page.goto(base, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#cliplist', { state: 'visible' });

  // The recordings are H.264, which the browser must be able to decode for
  // the marks to mean anything. A browser that cannot still gets a picture:
  // the poster below carries the frame.
  let plays = true;
  try {
    await page.waitForFunction(() => {
      const v = document.querySelector('video');
      return v && v.readyState >= 2;
    }, null, { timeout: 15_000 });
  } catch {
    plays = false;
    console.warn('the browser did not decode the recording - no marks on this one');
  }

  if (plays) {
    for (const [time, button] of [[IN_AT, '#bIn'], [OUT_AT, '#bOut']]) {
      await page.evaluate((t) => new Promise((done) => {
        const v = document.querySelector('video');
        if (Math.abs(v.currentTime - t) < 0.05) return done();
        v.addEventListener('seeked', () => done(), { once: true });
        v.currentTime = t;
      }), time);
      await page.click(button);
    }
  }

  // The poster over the video, and no blinking caret in any field.
  await page.evaluate(() => {
    const video = document.querySelector('video');
    if (video && video.getAttribute('poster')) {
      const frame = document.createElement('img');
      frame.src = video.getAttribute('poster');
      frame.style.cssText = 'position:absolute;inset:0;width:100%;height:100%;'
        + 'object-fit:contain;background:#000;z-index:1';
      video.parentElement.style.position = 'relative';
      video.parentElement.appendChild(frame);
    }
    document.activeElement?.blur();
    const style = document.createElement('style');
    style.textContent = '*{caret-color:transparent!important}';
    document.head.appendChild(style);
  });
  await page.waitForTimeout(250);
}

async function shoot(browser, { width, height, file, theme }) {
  const context = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: 2,
    hasTouch: width < 768,
    isMobile: width < 768,
    colorScheme: 'dark',
  });
  if (theme) {
    await context.addInitScript((name) => {
      try { localStorage.setItem('rc-theme', name); } catch { /* private mode */ }
    }, theme);
  }
  const page = await context.newPage();
  await prepare(page);
  await page.screenshot({ path: file });
  await context.close();
}

// Playwright's own Chromium is built without the proprietary codecs, so it
// cannot decode an H.264 recording. Edge and Chrome can, and both sit on a
// Windows runner; the bundled browser stays as the fallback.
async function launch() {
  for (const channel of ['msedge', 'chrome']) {
    try {
      return await chromium.launch({ channel });
    } catch {
      console.warn(`no ${channel} on this machine`);
    }
  }
  return chromium.launch();
}

const browser = await launch();

// The clips page as it looks on a desktop and on a phone.
await shoot(browser, { width: 1440, height: 900, file: join(tmp, 'desktop.png') });
await shoot(browser, { width: 390, height: 844, file: join(tmp, 'mobile.png') });

ffmpeg(['-i', join(tmp, 'desktop.png'), '-vf', 'scale=1920:-2', '-q:v', '3', join(outDir, 'clips.jpg')]);
ffmpeg(['-i', join(tmp, 'mobile.png'), '-vf', 'scale=750:-2', join(outDir, 'clips_mobile.png')]);
console.log('clips.jpg and clips_mobile.png written');

// The same picture in six themes; the website fades them into each other.
if (withThemes) {
  for (const theme of THEMES) {
    const shotFile = join(tmp, `theme-${theme}.png`);
    await shoot(browser, { width: 1440, height: 900, file: shotFile, theme });
    ffmpeg(['-i', shotFile, '-vf', 'scale=960:-2', '-c:v', 'libwebp', '-quality', '80',
      join(outDir, 'themes', `${theme}.webp`)]);
  }
  console.log(`${THEMES.length} theme pictures written`);
}

await browser.close();
rmSync(tmp, { recursive: true, force: true });
