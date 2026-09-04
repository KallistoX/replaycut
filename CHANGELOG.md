# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow
[Semantic Versioning](https://semver.org/).

replaycut 2.0 is a rewrite of a PowerShell service (1.x) that was never
published. The 2.0 line keeps that service's HTTP API; `docs/api.md` is the
contract.

## [Unreleased]

### Added

- OBS integration, part 1: the service connects to obs-websocket 5 on this PC
  (`obs` in settings.json, default `localhost:4455`, password as the
  credential `replaycut/obs-websocket`), keeps reconnecting with a backoff,
  and answers F9 through `SaveReplayBuffer` when connected - with a clear 409
  while the replay buffer is stopped - instead of a simulated key press; the
  key press stays as the fallback. `config.obs` reports the connection.
  A saved replay wakes the scanner at once; a stopped buffer raises a
  desktop notification. Contract: docs/api.md "Since 2.2".

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
  `%LOCALAPPDATA%eplaycutpp`, writes start menu and desktop shortcuts
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

[Unreleased]: https://github.com/KallistoX/replaycut/compare/v2.1.0...HEAD
[2.1.0]: https://github.com/KallistoX/replaycut/releases/tag/v2.1.0
[2.0.0]: https://github.com/KallistoX/replaycut/releases/tag/v2.0.0
