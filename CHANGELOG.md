# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow
[Semantic Versioning](https://semver.org/).

replaycut 2.0 is a rewrite of a PowerShell service (1.x) that was never
published. The 2.0 line keeps that service's HTTP API; `docs/api.md` is the
contract.

## [Unreleased]

### Added

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
