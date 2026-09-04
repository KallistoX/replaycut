# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow
[Semantic Versioning](https://semver.org/).

replaycut 2.0 is a rewrite of a PowerShell service (1.x) that was never
published. The 2.0 line keeps that service's HTTP API; `docs/api.md` is the
contract.

## [Unreleased]

### Added

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
