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

// The marks bracket the shot, which the demo clip carries 5.2 s in, and that
// frame is what the player shows.
const IN_AT = 3.8;
const OUT_AT = 6.4;
const FRAME_AT = 5.2;

mkdirSync(tmp, { recursive: true });
mkdirSync(join(outDir, 'themes'), { recursive: true });

function ffmpeg(args) {
  execFileSync('ffmpeg', ['-v', 'error', '-y', ...args]);
}

/** Wait for the clips page, put marks on the timeline, hide the caret. */
async function prepare(page, theme) {
  await page.goto(base, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#cliplist', { state: 'visible' });

  // The page starts in the default and swaps the stylesheet when the settings
  // arrive. Shooting in between catches a half-painted picture: the accent
  // already the new one, the surfaces still the old.
  if (theme && theme !== 'wardogs') {
    await page.waitForFunction((name) => {
      const link = document.getElementById('theme');
      return link && link.getAttribute('href') === `/themes/${name}.css`
        && link.sheet && link.sheet.cssRules.length > 0;
    }, theme, { timeout: 15_000 });
  }

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

  // A still over the video: the frame with the shot when the browser can
  // decode it, the poster the service made otherwise. Either way the picture
  // shows something instead of the black a composited video leaves behind.
  await page.evaluate(async ({ at, plays }) => {
    const video = document.querySelector('video');
    if (!video) return;
    let source = video.getAttribute('poster');
    if (plays) {
      await new Promise((done) => {
        if (Math.abs(video.currentTime - at) < 0.05) return done();
        video.addEventListener('seeked', () => done(), { once: true });
        video.currentTime = at;
      });
      const canvas = document.createElement('canvas');
      canvas.width = video.videoWidth;
      canvas.height = video.videoHeight;
      canvas.getContext('2d').drawImage(video, 0, 0);
      source = canvas.toDataURL('image/png');
    }
    if (source) {
      const frame = document.createElement('img');
      frame.src = source;
      frame.style.cssText = 'position:absolute;inset:0;width:100%;height:100%;'
        + 'object-fit:contain;background:#000;z-index:1';
      video.parentElement.style.position = 'relative';
      // right behind the video, so what the page lays over it stays on top
      video.after(frame);
    }
  }, { at: FRAME_AT, plays });

  await page.evaluate(() => {
    document.activeElement?.blur();
    const style = document.createElement('style');
    style.textContent = '*{caret-color:transparent!important}';
    document.head.appendChild(style);
  });
  await page.waitForTimeout(250);
}

/** The theme belongs to the service: the page takes the browser's guess only
 * until the settings arrive, then the configured one wins. */
async function setTheme(name) {
  await putSettings({ theme: name });
}
async function putSettings(patch) {
  const res = await fetch(`${base}/api/settings`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(patch),
  });
  if (!res.ok) throw new Error(`could not change the settings ${JSON.stringify(patch)}: ${res.status}`);
}

/** The workshop (since 3.12): the cut the seed gave subtitles, loaded by its
 * address, the playhead on the shot and its line over the picture. The
 * subtitles are a beta and are switched on for this one picture only. */
async function shootWorkshop(browser, { width, height, file }) {
  await putSettings({ subtitles: { enabled: true } });
  const { clips } = await (await fetch(`${base}/api/clips`)).json();
  const clip = clips.find((c) => (c.cuts || []).some((k) => k.subtitles && k.subtitles.count));
  if (!clip) throw new Error('the seed left no cut with subtitles');
  const cut = clip.cuts.find((k) => k.subtitles && k.subtitles.count);
  const context = await browser.newContext({ viewport: { width, height }, deviceScaleFactor: 2, colorScheme: 'dark' });
  const page = await context.newPage();
  await page.goto(`${base}/#${encodeURIComponent(clip.base)}/${cut.id}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#workshop.has-subs #lane .block', { timeout: 20_000 });
  let plays = true;
  try {
    await page.waitForFunction(() => {
      const v = document.getElementById('wv');
      return v && v.readyState >= 2;
    }, null, { timeout: 15_000 });
  } catch {
    plays = false;
    console.warn('the browser did not decode the cut - the workshop shows the thumbnail');
  }
  await page.evaluate(async ({ at, start, plays, thumb }) => {
    const video = document.getElementById('wv');
    if (plays) {
      await new Promise((done) => {
        video.addEventListener('seeked', () => done(), { once: true });
        wSeek(at - start); // the page's own seek, so the lane and the line follow
      });
    }
    let source = thumb;
    if (plays) {
      const canvas = document.createElement('canvas');
      canvas.width = video.videoWidth;
      canvas.height = video.videoHeight;
      canvas.getContext('2d').drawImage(video, 0, 0);
      source = canvas.toDataURL('image/png');
    }
    if (source) {
      const frame = document.createElement('img');
      frame.src = source;
      // exactly over the video: the controls sit under it in the workshop
      frame.style.cssText = `position:absolute;left:${video.offsetLeft}px;top:${video.offsetTop}px;`
        + `width:${video.offsetWidth}px;height:${video.offsetHeight}px;`
        + 'object-fit:contain;background:#000;z-index:1;border-radius:var(--radius-lg)';
      video.after(frame);
    }
    await document.fonts.ready;
    document.activeElement?.blur();
    const style = document.createElement('style');
    style.textContent = '*{caret-color:transparent!important}';
    document.head.appendChild(style);
  }, { at: FRAME_AT + 0.05, start: cut.start, plays, thumb: clip.thumb });
  await page.waitForTimeout(400);
  await page.screenshot({ path: file });
  await context.close();
  await putSettings({ subtitles: { enabled: false } });
}

async function shoot(browser, { width, height, file, theme }) {
  const context = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: 2,
    hasTouch: width < 768,
    isMobile: width < 768,
    colorScheme: 'dark',
  });
  const page = await context.newPage();
  await prepare(page, theme);
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

// A cut in the workshop, with its subtitles (since 3.12).
await shootWorkshop(browser, { width: 1440, height: 900, file: join(tmp, 'workshop.png') });
ffmpeg(['-i', join(tmp, 'workshop.png'), '-vf', 'scale=1920:-2', '-q:v', '3', join(outDir, 'workshop.jpg')]);
console.log('workshop.jpg written');

// The same picture in six themes; the website fades them into each other.
if (withThemes) {
  for (const theme of THEMES) {
    await setTheme(theme);
    const shotFile = join(tmp, `theme-${theme}.png`);
    await shoot(browser, { width: 1440, height: 900, file: shotFile, theme });
    ffmpeg(['-i', shotFile, '-vf', 'scale=960:-2', '-c:v', 'libwebp', '-quality', '80',
      join(outDir, 'themes', `${theme}.webp`)]);
  }
  await setTheme('wardogs');
  console.log(`${THEMES.length} theme pictures written`);
}

await browser.close();
rmSync(tmp, { recursive: true, force: true });
