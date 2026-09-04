# replaycut

**Clip manager for the OBS replay buffer.**

OBS keeps a replay buffer while you play. You press a hotkey, OBS writes the
last few minutes to disk, replaycut notices the file, and you trim the good
part in your browser: set in and out points, pick an audio mix, give it a
title, hit Share. The clip is encoded on the gaming PC. Optional integrations
upload the result and post a link into a Discord channel; without them,
replaycut is a local clip manager and the finished file lands in a folder.

Everything runs on the PC that runs the game. The browser, on the same PC, a
laptop or a phone in the same network, is only the remote control.

## Status

Pre-release. replaycut 2.0 is a rewrite in Rust of a PowerShell service that
is in daily use but was never published. This repository currently holds:

- the HTTP API contract in [`docs/api.md`](docs/api.md), transcribed from the
  running 1.4 service;
- a black-box test suite that runs against any implementation of that
  contract (see below);
- a placeholder binary crate.

The service itself follows. Until then there is nothing to install.

## Requirements

- Windows 10 or 11 (the service uses the recycle bin, toast notifications and
  the Credential Manager; other platforms are not a goal for 2.0).
- [OBS Studio](https://obsproject.com/) with the replay buffer enabled,
  recording to MKV. Multiple audio tracks are optional; the recommended
  layout is track 1 = mix, 2 = microphone, 3 = game, 4 = voice chat.
- [ffmpeg](https://ffmpeg.org/) and ffprobe on the `PATH` (on Windows, for
  example `winget install Gyan.FFmpeg`).
- A hardware H.264 encoder is used when available (AMD AMF, NVIDIA NVENC,
  Intel Quick Sync), otherwise libx264.

## Three steps

Once the 2.0 service is released:

1. Download the release ZIP, unpack it and run `install.cmd`. The service
   registers itself to start at logon and opens the setup page in your browser.
2. In the setup page, point replaycut at the folder OBS writes replays to,
   press your replay hotkey once so it can verify the file, and optionally
   enable integrations (upload target, Discord webhook).
3. Play. Press the replay hotkey when something happens, open the page, trim,
   Share.

## Development

```bash
cargo build --workspace
```

Run the service from the repository during development, on its own port and
with its own folders so it does not interfere with an installed instance:

```bash
cargo run -p replaycut -- --dry-run --port 8422 --bind 127.0.0.1 --clip-dir <scratch folder> --data-dir <scratch data dir> --ui ui/index.html
```

`--dry-run` encodes for real but simulates uploads, posts, the replay hotkey
and the clipboard. Settings live in `<data-dir>/settings.json` and are
created with defaults on first start; command-line flags override them.

### API contract tests

The tests in `tests/api` are black-box tests against a running service. They
place a generated test clip into the folder the service scans, drive the API,
and clean up after themselves. They need ffmpeg on the `PATH` and two
environment variables:

| Variable   | Meaning                                                             |
|------------|---------------------------------------------------------------------|
| `BASE_URL` | Base address of the service under test, e.g. `http://localhost:8420` |
| `CLIP_DIR` | The clip folder that service scans (the fixture is written there)    |

```bash
BASE_URL=http://localhost:8420 CLIP_DIR=/path/to/clips cargo test -p replaycut-api-tests
```

The suite runs single-threaded (configured in `.cargo/config.toml`) because
the service has one share slot. Sharing must be run in a mode that does not
upload or post anywhere; the 1.4 service offers `-DryRun` for this, the 2.0
service will run the suite with integrations disabled.

## License

AGPL-3.0-only. See [`LICENSE`](LICENSE).

"OBS" is a trademark of the OBS Project. replaycut is an independent tool that
works with OBS Studio's replay buffer and is not affiliated with the OBS
Project.
