# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow
[Semantic Versioning](https://semver.org/).

replaycut 2.0 is a rewrite of a PowerShell service (1.x) that was never
published. The 2.0 line keeps that service's HTTP API; `docs/api.md` is the
contract.

## [Unreleased]

### Added

- Share targets: every configured storage is a target, `POST /api/share`
  takes `target` (a storage id or `file`), the default is the storage marked
  "quick share" in the settings. `config.targets` lists the integrations
  with their state.
- Publish again: `POST /api/jobs/<id>/publish` sends the finished file of a
  share to another storage without cutting it again.
- OneDrive as a storage: connect with a code at Microsoft (device flow, works
  from a phone), uploads go to `Apps/replaycut/<month>/` with a link anyone
  can open. `GET /api/oauth/<provider>`, `POST .../start`, `POST
  .../disconnect`. Needs a build with a client id.
- S3-compatible storage (AWS S3, Cloudflare R2, Backblaze B2, MinIO, Wasabi):
  SigV4-signed uploads to `<prefix>/<month>/`, links from a public URL or
  presigned with an expiry; `POST /api/test/s3` checks bucket and keys.
- Generic WebDAV storage: any DAV server plus a public URL that serves the
  folder; `POST /api/test/webdav` checks server and login.
- Deleting a clip with "also remove from storage" now removes the remote
  copies from every storage its shares went to.

### Changed

- The post stage of a job is called `notify` (was `discord`); the status
  text stays in `discord`. Settings gain `integrations.nextcloud.quickShare`
  and `integrations.discord.autoPost`, both default on.
- Integrations tab in two groups, Storage and Notify, with "Quick share
  target" and "Post automatically" switches; the Share button gets a menu
  with the other storages and "file only"; result card and history show the
  target and offer "Publish to ..." for every other storage.

## [2.4.0] - 2026-09-05

Cutting gets comfortable: shares queue up and can be cancelled, every clip
has a picture, the Nextcloud quota sits in the header, a fast copy mode
skips the re-encode, the GPU decodes where it can, and the page hears about
changes the moment they happen.

### Added

- Shares queue up instead of answering "a share is already running": the
  Share button stays usable, the card shows the place in the queue, and the
  next job starts as soon as the running one ends (`position` in the
  answer and the job, `queue` in `GET /api/clips`).
- Cancel a share: `POST /api/jobs/<id>/cancel` and the Cancel button on the
  progress card. Waiting jobs leave the queue at once; a running encode or
  upload is stopped and its partial output removed.
- Thumbnails: every clip shows a picture from 10 s before its end in the
  list and as the player poster (`thumb` on the clip, `GET /media/<base>.jpg`).
- The Nextcloud quota in the header ("Nextcloud 63 %", yellow from 80 %,
  red from 95 %), refreshed in the background (`config.quota`).
- Fast copy: a share mode that keeps the OBS video stream instead of
  re-encoding (keyframe-accurate, `mode: copy`, `actualStart` in the job).
  The choice is remembered in the browser; the default stays H.264.
- GPU decoding: the encoder detection now tries the full GPU path of each
  vendor with a real clip (AMD `d3d11va`, NVIDIA `cuda` with `scale_cuda`,
  Intel `qsv`) and falls back to software decoding per share when it fails.
  On an AMD card the AV1 decode moves off the CPU (13 s instead of 42 s CPU
  time for 30 s of 1440p60). `hwaccel` gains `auto` (the default) and `none`.
- `replaycut bench`: encodes part of the newest clip with every profile and
  prints wall time, CPU time and speed.
- The page listens to `GET /api/events` (Server-Sent Events) instead of
  asking every 3 s: changes show up at once, an idle page costs nothing,
  and a restart no longer waits for open connections. Polling stays as the
  fallback.
- The title field suggests the recording day and time as a placeholder;
  Enter on the empty field takes it.
- A keyboard shortcut list behind the "?" button and the "?" key.

### Changed

- Contract: a second `POST /api/share` while a job runs answers 202 with a
  queue position (409 only for the same cut twice).

### Fixed

- "Update now" on a release that has no signature yet ends in an error
  ("not signed yet") instead of waiting forever.

## [2.3.1] - 2026-09-05

A small release to prove the one-click update from 2.3.0.

### Fixed

- README and CHANGELOG: the install path `%LOCALAPPDATA%\replaycut\app` had
  lost its backslashes.

## [2.3.0] - 2026-09-05

The one-click update and the complete tray menu. From this release on,
every release is signed: the updater installs only what the maintainer's
minisign key vouches for.

### Added

- One-click update: `GET /api/update` and `POST /api/update/{check,download,install,seen}`.
  The service downloads the release ZIP, verifies the minisign signature of
  `SHA256SUMS` and the hash, unpacks, checks the new executable and restarts
  into it. Releases without a valid signature are never installed.
- The update banner: "Update now" runs the whole update with a progress bar
  and reloads the page on the new version; "What's new" shows the release
  notes; after an update the page says so once. Settings › General has
  "Check for updates now" with the time of the last check.
- The tray menu is complete: Open, Copy address, Show QR code, Pause
  scanning, Check for updates (with a notification for the outcome), Open
  log folder, Quit. The tooltip says "paused" and "update available".
- `POST /api/scanning { paused }` and `config.scanning`: pause the folder
  scan from the tray or the API; the UI shows a banner with "Resume".

### Changed

- The address dialog shows no QR code while replaycut listens on this PC
  only; it says how to change that instead.

## [2.2.0] - 2026-09-04

The OBS integration: replaycut talks to OBS through obs-websocket, saves
replays without simulated key presses, shows when the replay buffer is
stopped and compares the OBS profile with what it expects. Read-only; the
only actions are saving a replay and starting the buffer.

### Added

- OBS integration, part 1: the service connects to obs-websocket 5 on this PC
  (`obs` in settings.json, default `localhost:4455`, password as the
  credential `replaycut/obs-websocket`), keeps reconnecting with a backoff,
  and answers F9 through `SaveReplayBuffer` when connected - with a clear 409
  while the replay buffer is stopped - instead of a simulated key press; the
  key press stays as the fallback. `config.obs` reports the connection.
  A saved replay wakes the scanner at once; a stopped buffer raises a
  desktop notification. Contract: docs/api.md "Since 2.2".
- OBS integration, part 2: the OBS page reads profile, recording folder,
  format, encoder, video settings and the audio-track layout through the
  connection and compares them with what replaycut expects - every
  difference with the OBS menu path, plus the buttons "Start replay buffer"
  and "Use this folder in replaycut"; the top bar shows OBS, the clips page
  warns while the buffer is stopped, the wizard uses the connection, the
  diagnostics row is real. `GET /api/obs`, `POST /api/obs/replay-buffer/start`,
  `/reconnect`, `/refresh`, `/adopt-folder`. Nothing in OBS is written.

## [2.1.0] - 2026-09-04

Setup in the browser, settings at runtime, an optional password, a
diagnostics page and the new design. Rollout to the group still waits for
the one-click update.

### Added

- Settings change at runtime: `GET/PUT /api/settings` (everything but port,
  bind and the UI file takes effect at once), `POST /api/test/nextcloud` and
  `/api/test/discord`, `GET /api/addresses` with a QR code, `GET /themes/<name>.css`
  from the data directory, `POST /api/restart`. Contract: docs/api.md "Since 2.1".
- Optional password for other devices: argon2id hash in settings.json, 30-day
  session cookie, login throttle; this PC (loopback) never needs it. Every
  cross-site write is refused by an Origin check, password or not.
- `setupDone` and `theme` in settings.json; the page routes `/setup`,
  `/settings`, `/diagnostics`, `/login` serve the UI file.
- Local mode without integrations has a way out of the browser: "Open folder"
  and "Copy file" on the result (`POST /api/jobs/<id>/open-folder` and
  `/copy-file`; the file lands in the clipboard as a file object, Ctrl+V in
  Discord attaches it).
- The UI moves to the design system (docs/design): top bar with the pages,
  banners instead of the status block, the clip list as a collapsed panel
  beside the game, a login page, themes from the data directory.
- Setup wizard at `/setup` (OBS folder from the profile, live check for the
  first replay with codec and browser playability, integrations with tests,
  password, addresses and QR code) and the settings page at `/settings`
  (General and Integrations, changes apply at once, restart button for port
  and bind). `GET /api/setup/obs` reads the OBS profiles; clips carry codec,
  size and frame rate.
- Diagnostics page at `/diagnostics` and `GET /api/diagnostics`: eleven
  checks with a fix per problem and a text copy without secrets;
  `replaycut test` prints that text when the service runs.
- A fresh installation opens the browser setup; a migrated one counts as set
  up. `replaycut setup` points at the wizard.
- `docs/design`: the design system for the web UI - tokens, component sheet,
  page mockups and icon sources - and `docs/themes.md`, the theme format.

### Changed

- New application and tray icons (play mark with cut marks, amber on a dark
  tile) in six sizes up to 256 px; the tray states "job running" and "last
  job failed" carry an amber or a red dot. Rendered from the SVG sources in
  `docs/design/icons` by `mkico`.

## [2.0.0] - 2026-09-04

First public release: the Rust service, the installer and the migration
from the 1.x PowerShell service.

### Added

- Update hint: a minute after start and then daily the service asks GitHub
  for the latest release; a newer one appears as `config.update` in
  `/api/clips` (documented in `docs/api.md`, covered by the contract suite)
  and as a dismissable banner above the clip list. `checkUpdates: false`
  switches the check off. Nothing is downloaded.
- Windows integration, part 2: `replaycut install` (idempotent, per user, no
  admin except the optional firewall rule): copies the program to
  `%LOCALAPPDATA%\replaycut\app`, writes start menu and desktop shortcuts
  with the AppUserModelID, registers the notification app id, asks about
  autostart (HKCU Run entry, default off) and the firewall rule (one UAC
  prompt, private profile), starts the service and opens the page;
  `replaycut uninstall` (`--purge` also removes settings, state, logs and
  credentials); `replaycut autostart on|off|status`; migration from the 1.x
  PowerShell service (task arguments, state files, credentials, task and
  firewall cleanup); `install.cmd` and `uninstall.cmd` for the release ZIP.
- Windows integration, part 1: the executable runs without a console window
  and attaches to the terminal it was started from for `--help`, `setup`,
  `test`, `stop` and log output; only one instance runs at a time (a second
  start opens the browser); a tray icon with Open, Copy address and Quit,
  a tooltip with the clip count or share progress and a badge while a share
  runs or after a failed one; `replaycut stop` ends the running service
  through a named event; desktop notifications for saved clips and share
  results (WinRT toasts, shown once the app is registered by the installer);
  `--no-browser`; the log records the shutdown reason and panics with a
  backtrace; fatal start-up errors show a dialog when there is no console.
- Service core, part 4: resource limits for ffmpeg (`ffmpegPriority`,
  default below normal; `ffmpegThreads`, default half the cores) so a share
  does not stall the game; `docs/settings.md`; idle footprint of the release
  build measured and recorded in the README.
- Service core, part 3: real integrations. Nextcloud storage (WebDAV upload
  into `<folder>/<YYYY-MM>/`, public link created or reused, remote delete)
  and Discord notify (webhook post with the display name as user name).
  Credentials live in the Windows Credential Manager under
  `replaycut/nextcloud` and `replaycut/discord-webhook`; `replaycut setup`
  configures both on the console, `replaycut test` checks them.
- Service core, part 2: the share pipeline. `POST /api/share` validates and
  registers a job (202, 409 while a job runs, 404 unknown clip, 400 invalid
  selection or audio mode), encodes with ffmpeg and live progress, then runs
  the storage and notify integrations when enabled, records history (200
  entries) and keeps the last 30 jobs. `--dry-run` uses simulated
  integrations with `dry-run.invalid` links. `DELETE ...?nextcloud=1`
  removes remote copies through the storage integration. The contract suite
  passes 11/11 against the Rust service in dry-run mode.
- Service core, part 1: `replaycut` binary with settings.json, rolling log,
  folder scanner (change notifications plus polling, 2-second age and
  exclusive-open rule), preview remux, and the read side of the API:
  `GET /`, `/api/clips`, `/api/history`, `/api/jobs/<id>`, `/media/<base>.mp4`
  with range requests, clip titles, delete to the recycle bin, `/api/save`
  (F9 to OBS), 404 handling. `--dry-run` simulates hotkey and integrations.
- `ui/index.html`: the browser UI, translated to English, logic unchanged.
- Repository skeleton: Cargo workspace with the `replaycut` binary crate
  (placeholder) and the `replaycut-api-tests` crate.
- `docs/api.md`: the HTTP API contract, transcribed from the 1.4 service.
- Black-box API test suite (`cargo test`, configured via `BASE_URL` and
  `CLIP_DIR`) covering clip discovery, preview range requests, titles, the
  share pipeline, the single-job rule (409), delete, `/api/save` and 404s.
- CI skeleton (fmt, clippy, build, compile tests).

[Unreleased]: https://github.com/KallistoX/replaycut/compare/v2.2.0...HEAD
[2.2.0]: https://github.com/KallistoX/replaycut/releases/tag/v2.2.0
[2.1.0]: https://github.com/KallistoX/replaycut/releases/tag/v2.1.0
[2.0.0]: https://github.com/KallistoX/replaycut/releases/tag/v2.0.0
