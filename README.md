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

replaycut 2.0 is a rewrite in Rust of a PowerShell service (1.x) that was in
daily use but never published. It keeps that service's HTTP API
([`docs/api.md`](docs/api.md) is the contract, checked by a black-box test
suite) and its file formats, so a 1.x installation migrates in place.
Releases are published on GitHub as a ZIP for Windows x64; see
[`CHANGELOG.md`](CHANGELOG.md) for what each version brings.

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

## Install

1. Download the release ZIP, unpack it anywhere and run `install.cmd`. It
   copies replaycut to `%LOCALAPPDATA%eplaycutpp`, adds a start menu and
   a desktop shortcut, asks whether replaycut should start when you sign in
   (default: no) and whether other devices in your private network may reach
   it (one administrator prompt for the firewall rule), then starts the
   service and opens the page in your browser. No admin rights otherwise.
2. The browser opens the setup: it suggests the recording folder from your
   OBS profile, waits for the first replay and reports codec and audio
   tracks, lets you switch on Nextcloud and Discord with a test each, and
   offers a password for other devices. Skip what you do not need; without
   integrations replaycut is a local clip manager. Everything can be changed
   later under Settings (`replaycut setup` on the console still works; see
   [`docs/settings.md`](docs/settings.md)).
3. Play. Press the replay hotkey when something happens, open the page, trim,
   Share.

replaycut runs as a tray icon: **Open** shows the page, **Copy address** puts
the address for your phone into the clipboard, **Show QR code** shows it for
scanning, **Pause scanning** keeps new replays out of the list for a while,
**Check for updates** asks GitHub now, **Open log folder** and **Quit** do
what they say. Double-click
the shortcut to start it again; if it is already running, that opens the page.

Windows SmartScreen may warn about an unsigned download the first time: click
"More info", then "Run anyway".

### Pages

- **Clips** (`/`): the list, the player with in/out marks, audio choice,
  Share with live progress, the result with links or - in local mode - "Open
  folder" and "Copy file", and the share history.
- **Settings** (`/settings`): everything in `settings.json`, integrations
  with their tests, theme, autostart and the password; changes apply at
  once, port and bind after "Restart now".
- **OBS** (`/obs`): with the WebSocket server switched on in OBS (Tools ›
  WebSocket Server Settings), replaycut connects on its own, saves replays
  through OBS instead of a simulated key press, warns while the replay
  buffer is stopped and can start it, and compares folder, format, encoder
  and audio tracks with what it expects - every difference with the OBS
  menu path. It never changes OBS settings.
- **Diagnostics** (`/diagnostics`): ffmpeg, encoder, folder, scan,
  integrations, network as one list with a fix per problem, and "Copy
  diagnostics" for a support message. `replaycut test` prints the same.
- **Setup** (`/setup`): the wizard, any time again.

With a password set, phones and other computers see a login page; the PC
that runs replaycut never needs it. Every cross-site write is refused, so a
web page you visit cannot change your settings.

### Update

replaycut checks GitHub once a day and shows a banner when a newer release
exists. "Update now" downloads the ZIP, verifies its signature and hash and
restarts on the new version; settings, titles, history and credentials are
kept. By hand: unpack the new ZIP and run its `install.cmd`. Every release's
`SHA256SUMS` is signed with the maintainer's minisign key (the public key is
built into replaycut); an unsigned or foreign release is never installed.

### Uninstall

Run `uninstall.cmd` from the unpacked ZIP (or `replaycut uninstall` from a
terminal). It stops the service and removes the files, shortcuts, autostart
entry and, after asking, the firewall rule. Settings, titles, history and
credentials stay unless you use `replaycut uninstall --purge`. Your clips
are never touched.

### Coming from the 1.x PowerShell service

`install.cmd` detects the old scheduled task and takes over its clip folder,
port, Nextcloud settings, titles, history and credentials, then stops and
removes the task so the port is free. Autostart is switched on, because the
old service started at sign-in. The old firewall rule and URL reservation are
removed in the same administrator step as the new firewall rule.

## Development

```bash
cargo build --workspace
```

Run the service from the repository during development, on its own port and
with its own folders so it does not interfere with an installed instance:

```bash
cargo run -p replaycut -- --dry-run --port 8422 --bind 127.0.0.1 --clip-dir <scratch folder> --data-dir <scratch data dir> --ui ui/index.html
```

`--dry-run` encodes for real but simulates uploads, posts, the replay hotkey,
the clipboard and desktop notifications. The executable has no console
window of its own; from a terminal it attaches to that terminal (see
"Starting and stopping" in `docs/settings.md`). A running instance is
stopped with `replaycut stop` or Quit in the tray menu. Settings live in `<data-dir>/settings.json` and are
created with defaults on first start; command-line flags override them.
All settings, the credential targets and the command line are documented in
[`docs/settings.md`](docs/settings.md).

### Resource usage

The service is meant to sit next to a game. Measured on the release build
(idle, no clients connected, folder watcher active):

| Metric (10 minutes idle, 16-core desktop) | Value |
|---|---|
| Working set | 28.6 MB average, 28.7 MB peak |
| Private memory | 7.7 MB |
| CPU time | 0.11 s in 10 minutes (0.02 % of one core) |
| Threads / handles | 11 / 333 |
| Executable | 5.0 MB |

With the tray icon (Windows integration, part 1) the release build sits at
16.5 MB working set, 8 threads and 0.05 s CPU after one minute idle,
started without a console.


While a clip is shared, ffmpeg runs at below-normal priority with a thread
cap (see `ffmpegPriority` and `ffmpegThreads`), so the game keeps the CPU.

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
