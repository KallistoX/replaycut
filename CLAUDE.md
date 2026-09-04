# CLAUDE.md

Guidance for Claude Code (and humans) working in this repository.

## What this is

replaycut is a clip manager for the OBS replay buffer: a small self-hosted
service that watches the folder OBS writes replays to, serves a browser UI for
trimming, encodes the selected range with ffmpeg and, through optional
integrations, uploads the result and posts a link. Target platform for 2.0 is
Windows; the browser is the remote control and may be a phone or laptop in the
same network.

2.0 is a Rust rewrite of a PowerShell service (1.x) that is in use but was
never published. The rewrite is API-identical to 1.4; the UI (`ui/index.html`,
one static file, vanilla JS) is the 1.4 UI with English strings, logic
unchanged. Features come after 2.0.

## Layout

```
Cargo.toml              workspace
crates/replaycut/       the service binary
  src/main.rs            CLI (clap), startup, logging
  src/settings.rs        settings.json (see docs/settings.md)
  src/state.rs           clips, jobs, history, titles, seen list; 1.x file formats
  src/scanner.rs         folder watcher and scan rules
  src/media.rs           ffmpeg/ffprobe, encoder detection, resource limits
  src/share.rs           the share pipeline
  src/integrations.rs    storage (Nextcloud) and notify (Discord) plus dry-run stand-ins
  src/credentials.rs     Windows Credential Manager
  src/setup.rs           `replaycut setup` and `replaycut test`
  src/http.rs            axum router, one handler per endpoint
  src/platform.rs        recycle bin, replay hotkey, clipboard
ui/index.html           the browser UI (vanilla JS, served by the service)
tests/api/              black-box HTTP contract tests, run against BASE_URL
docs/api.md             the HTTP API contract - binding for every implementation
docs/settings.md        settings.json, credentials, command line
.github/workflows/      CI (fmt, clippy, build, compile tests)
```

## Conventions

- **English everywhere**: code, comments, commits, docs, log lines, UI strings.
- **The API contract in `docs/api.md` is binding.** Behaviour the UI relies on
  is part of the contract. Change the contract deliberately, in its own commit,
  with the tests updated in the same change.
- **Tests run against a live service.** `tests/api` reads `BASE_URL` and
  `CLIP_DIR` from the environment, generates its fixture clip with ffmpeg,
  and must pass against 1.4.1 in dry-run mode and against the Rust service. A
  test that only passes against one of them is a contract question, not a
  test to skip.
- **No secrets, no private infrastructure details in the repo**: no hostnames,
  IP addresses, key fingerprints, server names or personal cloud URLs, not
  even as defaults or in examples. Use `example.com`, `localhost` and
  placeholders. Credentials belong in the Windows Credential Manager at
  runtime, never in files.
- **Small, single-purpose commits** with a short imperative subject.
  `CHANGELOG.md` follows Keep a Changelog; add an entry under Unreleased with
  user-visible changes.
- `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings`
  must be clean; CI enforces both.
- Keep the service frugal: it runs next to a game. Idle CPU and RAM matter,
  and ffmpeg must not take all cores at normal priority.
- Roadmap and design notes live outside this repository. Ask before assuming
  a decision that is not recorded in `docs/` or `CHANGELOG.md`.

## Working with the service under test

- Start the service in a mode that encodes for real but does not upload or
  post (1.4.1: `-DryRun`; 2.0: integrations disabled), pointed at a scratch
  clip folder, on a port of its own.
- Then `BASE_URL=http://localhost:<port> CLIP_DIR=<that folder> cargo test`.
- The suite is serial by design (one share slot); do not parallelise it.

## Release process

1. Set the workspace version in `Cargo.toml`, turn the `Unreleased` section
   of `CHANGELOG.md` into `## [<version>] - <date>`, commit.
2. `git tag v<version>` and `git push origin v<version>`: `release.yml` builds
   the EXE, packs `replaycut-<version>-windows-x64.zip` plus `SHA256SUMS`
   and publishes the GitHub release with that version's CHANGELOG section.
3. Install from the published ZIP on a real machine before announcing it.
