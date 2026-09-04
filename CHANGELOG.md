# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow
[Semantic Versioning](https://semver.org/).

replaycut 2.0 is a rewrite of a PowerShell service (1.x) that was never
published. The 2.0 line keeps that service's HTTP API; `docs/api.md` is the
contract.

## [Unreleased]

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
