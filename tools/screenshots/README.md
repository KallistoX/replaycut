# Screenshots of the UI

The pictures in `docs/images` are taken from the real page of a running
service, not drawn and not from a mockup, so they cannot quietly drift away
from the UI. `.github/workflows/screenshots.yml` runs this whenever
`ui/index.html` or `docs/design/**` changes and commits what comes out.

Not a workspace member and not part of the build: this is a tool beside the
project, like `docs/design/icons/mkico`. The UI itself still has no build step.

| File | What it is |
|---|---|
| `demo.mkv` | Six seconds of gameplay, the material every screenshot is made of |
| `seed.mjs` | Puts the state into a service: recordings, titles, cuts, an output |
| `shoot.mjs` | Playwright: the clips page on a desktop and on a phone, optionally in six themes |

## Running it by hand

Needs Node 20+, ffmpeg on the `PATH` and a service that encodes but uploads
nowhere. Use a port and folders of your own so an installed replaycut is not
touched:

```bash
cargo build --release -p replaycut
target/release/replaycut --dry-run --port 8425 --no-browser \
  --clip-dir /tmp/rc-shots/clips --data-dir /tmp/rc-shots/data --ui ui/index.html

cd tools/screenshots
npm install
npx playwright install --with-deps chromium
node seed.mjs --base http://localhost:8425 --clips /tmp/rc-shots/clips
node shoot.mjs --base http://localhost:8425 --out ../../docs/images --themes
```

`--themes` adds `docs/images/themes/<name>.webp`, the six pictures the website
fades into each other.

## About the demo clip

It is a recording of the maintainer's own, cut to six seconds, scaled to 720p
and encoded with a keyframe every two seconds - the way OBS writes a replay,
which the cut step relies on. The nameplates of other players are blurred and
the segment ends before the team chat appears, so no one else's name is in the
repository.

The four audio tracks are the same track four times. A real replay carries a
mix, a microphone, the game and the voice chat; the screenshots only need the
audio row to have something true to show.

## When a picture looks wrong

- **The player is black.** A `<video>` is composited by the GPU and comes out
  black in a screenshot. `shoot.mjs` lays the poster - the thumbnail the
  service made - over it before taking the picture. If that stops working,
  check that the clip still has a `thumb`.
- **The run never finishes.** The page holds an open Server-Sent-Events
  connection (`GET /api/events`), so anything that waits for the network to
  fall quiet waits forever. That is why this is Playwright and not
  `chrome --headless --screenshot`.
- **The cut fails with an EBML error.** The recording has no keyframe near the
  start of the range. `seed.mjs` encodes the segments with `-g 60`; keep it.
