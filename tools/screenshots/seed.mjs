// Builds the state the screenshots show: eight recordings cut out of one demo
// clip, titles on most of them, a clip with two cuts and a rendered output,
// and two clips already done. Everything goes through the HTTP API, against a
// service started with --dry-run, so nothing is uploaded and nothing posted.
//
//   node seed.mjs --base http://localhost:8425 --clips <folder> [--demo demo.mkv]
//
// The order matters and is the reason this is a script and not a handful of
// curl calls: files first, then titles, then the cuts, and "done" last - a
// render puts a clip that is already done back into the list.

import { execFileSync } from 'node:child_process';
import { mkdirSync, utimesSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i > -1 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const base = arg('base', 'http://localhost:8425').replace(/\/$/, '');
const clipDir = arg('clips');
const demo = arg('demo', join(here, 'demo.mkv'));
if (!clipDir) {
  console.error('usage: node seed.mjs --base <url> --clips <folder> [--demo demo.mkv]');
  process.exit(2);
}

// One evening and the one before it. Every entry is a different second of the
// demo clip, so the thumbnails differ; `minutesAgo` keeps the list looking
// like it was recorded tonight, whenever the pipeline runs.
const PLAN = [
  // The newest one opens by default and is the clip the pictures show, so it
  // is the whole demo clip - the shot lands 5.2 s in.
  { from: 0.0, seconds: 8.6, minutesAgo: 14, title: 'Headshot at 243 m' },
  { from: 6.4, seconds: 2.2, minutesAgo: 37, title: 'Last man standing' },
  { from: 2.0, seconds: 4.0, minutesAgo: 58, title: 'Squad wipe at the bridge', cuts: true },
  { from: 1.0, seconds: 5.0, minutesAgo: 82 },
  { from: 3.0, seconds: 3.6, minutesAgo: 111, title: 'Bolt action, 240 m' },
  { from: 5.0, seconds: 1.5, minutesAgo: 1381, done: true },
  { from: 0.8, seconds: 5.2, minutesAgo: 1426, title: 'Tank rush into the spawn' },
  { from: 4.0, seconds: 2.4, minutesAgo: 1471, done: true },
];

const pad = (n) => String(n).padStart(2, '0');

function baseName(when) {
  return `Replay_${when.getFullYear()}-${pad(when.getMonth() + 1)}-${pad(when.getDate())}`
    + `_${pad(when.getHours())}-${pad(when.getMinutes())}-${pad(when.getSeconds())}`;
}

async function api(path, options = {}) {
  const res = await fetch(`${base}${path}`, {
    ...options,
    headers: options.body ? { 'Content-Type': 'application/json' } : undefined,
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(`${options.method || 'GET'} ${path}: ${res.status} ${JSON.stringify(body)}`);
  return body;
}

async function waitFor(what, test, seconds = 120) {
  const deadline = Date.now() + seconds * 1000;
  while (Date.now() < deadline) {
    try {
      if (await test()) return;
    } catch { /* the service may still be starting */ }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`gave up waiting for ${what}`);
}

// 1. The recordings. OBS writes MKV with a keyframe every couple of seconds;
// the cut step copies from a keyframe, so the demo clip carries them too and
// these segments keep them (-g 60 at 30 fps).
mkdirSync(clipDir, { recursive: true });
const clips = PLAN.map((entry) => {
  const when = new Date(Date.now() - entry.minutesAgo * 60_000);
  const name = baseName(when);
  const file = join(clipDir, `${name}.mkv`);
  execFileSync('ffmpeg', [
    '-v', 'error', '-y',
    '-ss', String(entry.from), '-i', demo, '-t', String(entry.seconds),
    '-map', '0',
    '-c:v', 'libx264', '-crf', '30', '-preset', 'veryfast', '-pix_fmt', 'yuv420p',
    '-g', '60', '-keyint_min', '60', '-sc_threshold', '0',
    '-c:a', 'copy',
    file,
  ]);
  const stamp = when.getTime() / 1000;
  utimesSync(file, stamp, stamp);
  return { ...entry, base: name };
});
console.log(`wrote ${clips.length} recordings to ${clipDir}`);

// 2. Wait until the scanner has probed every one of them.
await waitFor('the scanner', async () => {
  const { clips: seen } = await api('/api/clips?done=1');
  return seen.length >= clips.length && seen.every((c) => c.status === 'ready');
});
console.log('the service knows them all');

// 3. Titles.
for (const clip of clips.filter((c) => c.title)) {
  await api(`/api/clips/${clip.base}/name`, {
    method: 'PUT',
    body: JSON.stringify({ name: clip.title }),
  });
}
console.log('titles set');

// 4. Two cuts on one clip, one of them rendered, so the clip shows what came
// out of it. `after: keep` because a render says nothing about the clip.
const withCuts = clips.find((c) => c.cuts);
const cuts = [];
for (const range of [{ start: 0.4, end: 2.6, audio: 'mix' }, { start: 1.8, end: 3.8, audio: 'game' }]) {
  const made = await api('/api/cuts', {
    method: 'POST',
    body: JSON.stringify({ base: withCuts.base, ...range }),
  });
  // `cut` is the id here; `GET /api/cuts/<id>` answers with the cut itself.
  const id = typeof made.cut === 'string' ? made.cut : made.cut?.id;
  if (!id) throw new Error(`POST /api/cuts answered without an id: ${JSON.stringify(made)}`);
  await waitFor(`cut ${id}`, async () => (await api(`/api/cuts/${id}`)).state === 'ready');
  cuts.push(id);
}
await api(`/api/cuts/${cuts[0]}/render`, {
  method: 'POST',
  body: JSON.stringify({ target: 'file', mode: 'h264', after: 'keep' }),
});
await waitFor('the render', async () => (await api(`/api/cuts/${cuts[0]}`)).outputs.length > 0);
console.log(`cuts ${cuts.join(', ')} on ${withCuts.base}, one rendered`);

// 5. Done last.
for (const clip of clips.filter((c) => c.done)) {
  await api(`/api/clips/${clip.base}/state`, {
    method: 'PUT',
    body: JSON.stringify({ state: 'done' }),
  });
}

const active = (await api('/api/clips')).clips;
const all = (await api('/api/clips?done=1')).clips;
console.log(`ready: ${active.length} active, ${all.length - active.length} done, `
  + `${all.filter((c) => c.title).length} with a title`);
