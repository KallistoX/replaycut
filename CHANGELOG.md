# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow
[Semantic Versioning](https://semver.org/).

replaycut 2.0 is a rewrite of a PowerShell service (1.x) that was never
published. The 2.0 line keeps that service's HTTP API; `docs/api.md` is the
contract.

## [Unreleased]

### Added

- Repository skeleton: Cargo workspace with the `replaycut` binary crate
  (placeholder) and the `replaycut-api-tests` crate.
- `docs/api.md`: the HTTP API contract, transcribed from the 1.4 service.
- Black-box API test suite (`cargo test`, configured via `BASE_URL` and
  `CLIP_DIR`) covering clip discovery, preview range requests, titles, the
  share pipeline, the single-job rule (409), delete, `/api/save` and 404s.
- CI skeleton (fmt, clippy, build, compile tests).
