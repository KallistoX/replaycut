# replaycut

**Clip manager for the OBS replay buffer.**

OBS keeps a replay buffer while you play. You press a hotkey, OBS writes the
last few minutes to disk, replaycut notices the file, and you trim the good
part in your browser: set in and out points, pick an audio mix, give it a
title, hit Share. The clip is encoded on the gaming PC. Optional integrations
upload the result (Nextcloud, OneDrive, any S3 bucket or WebDAV server, as
a video on YouTube, a vertical cut as a Short, or as a post on X) and post
the link into a Discord channel, a Telegram chat or any webhook; without
them, replaycut is a local clip manager and the finished file lands in a
folder.

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
   copies replaycut to `%LOCALAPPDATA%\replaycut\app`, adds a start menu and
   a desktop shortcut, asks whether replaycut should start when you sign in
   (default: no), then starts the service and opens the page in your
   browser. No admin rights, and no firewall rule: replaycut listens on this
   PC only until you say otherwise.
2. The browser opens the setup: it suggests the recording folder from your
   OBS profile, waits for the first replay and reports codec and audio
   tracks, lets you switch on Nextcloud and Discord with a test each, and
   ends with access from other devices - a switch that asks for a password
   and for the firewall rule (see [Security](#security)). Skip what you do
   not need; without integrations replaycut is a local clip manager.
   Everything can be changed later under Settings (`replaycut setup` on the
   console still works; see [`docs/settings.md`](docs/settings.md)).
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

- **Clips** (`/`): the list with thumbnails, the player with in/out marks,
  audio and mode choice, Share (the quick-share storage) with a menu for the
  other storages and "file only", live progress and a queue, the result with
  links or "Open folder" and "Copy file", "Download" for the device the
  page is open on, "Publish to ..." for another
  storage, and the share history.
- **Settings** (`/settings`): everything in `settings.json`, integrations
  with their tests, theme, autostart, access from other devices with the
  password and the signed-in devices; changes apply at once, the port after
  "Restart now".
- **OBS** (`/obs`): with the WebSocket server switched on in OBS (Tools ›
  WebSocket Server Settings), replaycut connects on its own, saves replays
  through OBS instead of a simulated key press, warns while the replay
  buffer is stopped and can start it, and compares folder, format, encoder
  and audio tracks with what it expects - every difference with the OBS
  menu path. It never changes OBS settings.
- **Diagnostics** (`/diagnostics`): ffmpeg, encoder, folder, scan,
  integrations, network, firewall and the device login as one list with a
  fix per problem, and "Copy diagnostics" for a support message.
  `replaycut test` prints the same.
- **Setup** (`/setup`): the wizard, any time again.

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
old service started at sign-in. The old service was reachable in the
network, so that stays: the old firewall rule and URL reservation are
removed in the same administrator step that adds the new rule, and the
switch under Settings › Access closes it again when you want it closed.

## Security

replaycut serves your recordings and can upload them and post links, so it
starts closed and opens only where you say so.

**This PC only, until you change it.** A new installation listens on
`127.0.0.1` and the installer adds no firewall rule. Access from other
devices is one switch, in the setup wizard and under Settings › Access: it
asks for a password first, then for the one administrator prompt that opens
the port in your *private* network profile, and restarts the service. The
switch turns it all off again. An installation from before 2.8 that is open
to the network without a password keeps running, but says so - a red
banner, a notification once per start, and a failing diagnostics line.

**Signing in on a phone, without typing the password.** The login page
leads with "Ask &lt;your PC&gt;". The PC shows a notification, a card in
every open page and an entry in the tray, all with the same four-character
code and with the asking device's name and address; one click on Allow and
the phone is in for 30 days. The QR code in the wizard and in the settings
does the same in one scan: its address carries a token that is good once
and for two minutes, and only this PC and already signed-in devices get to
see such a code. Sign-in requests run out after two minutes, at most five
are open at a time, and a burst from several addresses pauses the device
login for ten minutes - the password keeps working.

**The password** is the way back in and 8 to 128 characters long; there are
no rules about digits or symbols, because length is what counts. "Generate
one for me" offers four words from a list built into the executable, shown
once. Ten wrong tries from one address lock that address for a minute, more
than 30 wrong tries from anywhere within five minutes pause the password
login for ten. The password is stored as an argon2id hash; sessions are
stored as SHA-256 of their cookie.

**Settings › Signed-in devices** lists every browser that may use replaycut
with its name, address and when it was last seen. "Sign out" ends one at
once, "Sign out everywhere" all but your own.

**What a web page you visit cannot do.** Every write with a foreign
`Origin` is refused, and every request whose `Host` is not `localhost`, an
IP address, this computer's name or a name in `allowedHosts` is answered
with `421` - that closes DNS rebinding, where a page uses its own name to
reach a service on your machine.

**Not in 2.8**: HTTPS. The traffic in your network is plain HTTP, so the
password crosses it once when you use it - which is why the device login
exists and is the everyday way in. Do not put replaycut on the open
internet; a VPN or a mesh network (Tailscale and friends) is the way to
reach it from outside.

`requireLoginOnLoopback` in the settings asks for the password on this PC
as well, for a Windows account other people use.

The generated passphrases use the [EFF Short Wordlist
#1](https://www.eff.org/dice) by the Electronic Frontier Foundation,
licensed [CC-BY 3.0
US](https://creativecommons.org/licenses/by/3.0/us/); the list is embedded
in `crates/replaycut/src/wordlist.rs`.

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

### Tests

Three layers, all in Rust and all run by CI:

| Layer          | Where                                     | Needs                          |
|----------------|-------------------------------------------|--------------------------------|
| Unit tests     | `#[cfg(test)]` in `crates/replaycut/src`  | nothing                        |
| UI invariants  | `crates/replaycut/tests/ui_invariants.rs` | nothing                        |
| API contract   | `tests/api`                               | a running service and ffmpeg   |

```bash
cargo test -p replaycut
```

runs the first two. The UI invariants read `ui/index.html` as text and check
what the rest of the program relies on: the viewport meta tag, that every
`data-f` binding is a path in `Settings::default()` and that numeric fields
are read as numbers, that every id the script looks up exists in the markup,
and that every icon reference has a `<symbol>`. The UI has no build step and
no tests of its own; 2.7.0 shipped a broken viewport tag and text-instead-of-number
limits because nothing looked.

`.github/workflows/ci.yml` runs `fmt`, `clippy`, the build and those tests in
the `check` job. A second job, `contract`, installs ffmpeg, starts the service
with `--dry-run` on port 8423 and runs the contract suite against it; the
service log is uploaded as an artifact when the job fails.

#### API contract tests

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
