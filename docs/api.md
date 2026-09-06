# replaycut HTTP API - contract

This document describes the HTTP API of the replaycut service as implemented by
the 1.4 PowerShell service and required of every later implementation. It was
transcribed from the 1.4 code, not from memory. What the browser UI relies on
is the contract; the UI stays unchanged in 2.0.

Where 1.4 behaviour is an accident rather than a design, the section says so
and states what a later implementation may do instead. Everything else is
binding. The test suite in `tests/api` checks the parts that can be observed
over HTTP.

## Conventions

- Plain HTTP, no authentication (2.0 adds an optional password later; the
  contract below is unaffected).
- JSON bodies are UTF-8 with `Content-Type: application/json; charset=utf-8`.
  Requests with a JSON body send `Content-Type: application/json`.
- Errors carry `{ "ok": false, "error": "<message>" }`. Messages are free
  text for humans and not part of the contract.
- Status codes in use: `200`, `202`, `206`, `404`, `409`, `500`. Unknown
  routes answer `404` with the plain-text body `not found`.
- Any unhandled exception inside a handler answers `500` with the JSON error
  form. 1.4 uses this for validation errors too (see each endpoint).
- Path segments that carry a clip `base` are percent-encoded by the client
  (`encodeURIComponent`); the server decodes them.
- Timestamps are local time in the form `yyyy-MM-ddTHH:mm:ss` without an
  offset (the .NET `s` format).
- Numbers are JSON numbers. A duration of exactly 20 seconds is serialised as
  `20`, not `20.0`; clients must not depend on a decimal point.

## Endpoints

| Method and path | Purpose | Success |
|---|---|---|
| `GET /`, `GET /index.html` | The UI (one static HTML file) | `200`, `text/html; charset=utf-8`, `Cache-Control: no-store` |
| `GET /api/clips` | Clip list and service state | `200` [State](#state) |
| `GET /media/<base>.mp4` | Preview video, range-capable | `200` or `206`, `video/mp4` |
| `POST /api/share` | Start a share job | `202 { ok: true, job: "<id>" }` |
| `GET /api/jobs/<id>` | Progress of a share job | `200` [Job](#job) |
| `PUT /api/clips/<base>/name` | Set or remove the clip title | `200 { ok: true, base, title }` |
| `DELETE /api/clips/<base>[?nextcloud=1]` | Delete a clip | `200 { ok: true, recycled, nextcloud }` |
| `GET /api/history` | All share history entries | `200 { history: [...] }` |
| `POST /api/save` | Ask OBS to save the replay buffer | `200 { ok: true }` |

### `GET /`

Serves the UI file with `Cache-Control: no-store` so a service update is
picked up on the next reload. `/index.html` is an alias.

### `GET /api/clips`

Returns the [State](#state) object. Called by the UI every 3 seconds; it must
be cheap. Clips are sorted by `created` descending (newest first). `history`
here is capped at the 50 newest entries; `GET /api/history` returns all.

### `GET /media/<base>.mp4`

Streams the preview for clip `<base>` (URL-decoded, `.mp4` suffix stripped).
The preview is a remux of the original video stream and audio track 1 into an
MP4 with `faststart`, so the file starts with the `ftyp` box followed by
`moov`.

- Always sends `Accept-Ranges: bytes` and `Content-Length`.
- Without `Range`: `200` with the whole file.
- With `Range: bytes=a-b`, `bytes=a-` or `bytes=-n`: `206` with
  `Content-Range: bytes <start>-<end>/<total>` and exactly that slice. `b` is
  clamped to the last byte.
- Multiple ranges and `If-Range` are not supported. 1.4 answers an
  unsatisfiable range with a truncated `206`; 2.0 answers `416`.
- Unknown clip: `404` plain text. A `base` containing `/` or `\` is rejected
  (`500` in 1.4, `400` or `404` acceptable later).

### `POST /api/share`

Body: `{ "base": "<clip base>", "start": <seconds>, "end": <seconds>, "audio": "<mode>" }`.

- `start` is clamped to `>= 0`, `end` to `<= clip.duration`. The resulting
  length `end - start`, rounded to two decimals, must be at least 1 second.
- `audio` defaults to `mix` when missing or empty. Valid modes and the number
  of audio tracks the clip needs (see [Config.audio](#config)):

  | id | ffmpeg mapping (tracks are 0-based) | need |
  |---|---|---|
  | `mix` | `-map 0:a:0` | 1 |
  | `gamemic` | amix of `0:a:2` and `0:a:1`, `normalize=0` | 4 |
  | `game` | `-map 0:a:2` | 3 |
  | `gamediscord` | amix of `0:a:2` and `0:a:3`, `normalize=0` | 4 |

- Only one job runs at a time. Before 2.4, every share request while a job
  runs answers `409 { ok: false, error, job: "<running id>" }` and the UI
  attaches itself to that job id. Since 2.4 the request joins a queue
  instead; see [Since 2.4](#since-24).
- Unknown `base`: `404 { ok: false, error }`.
- Validation failures (too short, unknown audio mode, clip has fewer tracks
  than the mode needs): `500 { ok: false, error }` in 1.4, `400` in 2.0; the
  tests accept both.
- Success: `202 { ok: true, job: "<id>" }` is sent immediately; the work
  continues in the background. The id is 8 lowercase hex characters in 1.4;
  treat it as an opaque string.
- The job takes the clip's title at the moment the job starts. The UI saves
  the title (`PUT .../name`) and awaits that response before posting the
  share.

### `GET /api/jobs/<id>`

Returns the [Job](#job) object for a running or recent job. The service keeps
the 30 most recent jobs; older ids answer `404 { ok: false, error }`.
The UI polls this once per second until `stage` is `done` or `error`.

### `PUT /api/clips/<base>/name`

Body: `{ "name": "<title>" }`. `POST` is accepted as an alias for `PUT`.

- CR, LF and TAB are replaced by spaces, the result is trimmed and cut to 80
  characters.
- An empty result removes the title. Titles persist across restarts and are
  keyed by `base`; the MKV keeps its OBS file name.
- Response: `{ ok: true, base, title }` with the title as stored.
- Unknown `base`: `500 { ok: false, error }` in 1.4 (the handler throws),
  `404` in 2.0; the tests accept either.

### `DELETE /api/clips/<base>[?nextcloud=1]`

Moves the MKV and every shared file derived from it (`shared/<base with
whitespace as _>_*.mp4`) to the recycle bin, removes the preview and forgets
the clip and its title. The clip disappears from `/api/clips` immediately.

- Response: `{ ok: true, recycled: <files moved to the recycle bin>, nextcloud: <remote files deleted> }`.
- With `?nextcloud=1` the remote copies are deleted as well (paths from the
  shared files' names and from history entries of this clip) and the clip's
  history entries are removed. Without it, `nextcloud` is `0` and history is
  kept.
- Deleting the clip of a running job is refused: `500 { ok: false, error }`
  in 1.4, `409` in 2.0.
- Unknown `base`: `500` in 1.4, `404` in 2.0.
- `?nextcloud=1` without an active storage integration: `400` in 2.0 (1.4
  fails with `500` when credentials are missing).

### `GET /api/history`

`{ "history": [HistoryEntry, ...] }`, newest first, capped at 200 entries.

### `POST /api/save`

Triggers "save replay buffer" in OBS (1.4 sends the F9 key; 2.0 may use
obs-websocket with the key press as fallback). The request must carry a
`Content-Length` header in 1.4 (http.sys), an empty body is fine; the UI
sends `body: ''`. 2.0 accepts any body. Response: `{ ok: true }`. Failure to
reach OBS is not detectable and still answers `ok: true`.

## Objects

### State

```json
{
  "clips":   [Clip, ...],
  "last":    Job | null,
  "busy":    false,
  "job":     "<id>" | null,
  "scanAt":  "2026-09-04T11:38:35" | null,
  "history": [HistoryEntry, ...],
  "config":  Config
}
```

- `busy` is `true` and `job` is the id while a share runs; afterwards `busy`
  is `false`, `job` is `null` and `last` is a copy of the finished job
  (`done` or `error`). `last` is cleared when its clip is deleted.
- `scanAt` is the time of the last completed folder scan. The UI warns when it
  is older than 30 seconds.
- `config.update` (since 2.0) is `null` or `{ "version": "2.1.0", "url": "<release page>" }`
  when a newer release exists on GitHub; the service checks a minute after
  start and then daily unless `checkUpdates` is `false`. 1.x does not send
  the field; clients treat absent and `null` alike. The UI shows a banner.

### Clip

```json
{
  "name":     "Replay 2026-09-04 11-40-00.mkv",
  "base":     "Replay 2026-09-04 11-40-00",
  "path":     "C:\\Users\\me\\Videos\\Replay 2026-09-04 11-40-00.mkv",
  "size":     1519131,
  "duration": 20,
  "tracks":   4,
  "created":  "2026-09-04T11:38:55",
  "preview":  "/media/Replay%202026-09-04%2011-40-00.mp4",
  "status":   "ready",
  "title":    ""
}
```

- `base` is the file name without `.mkv`; it is the key for every other call.
- `size` is the MKV size in bytes. `duration` is measured on the preview with
  ffprobe, rounded to two decimals. `tracks` is the number of audio streams in
  the MKV (1 when probing fails).
- `created` is the MKV's last-write time. `preview` is the URL-encoded path
  under `/media/`. `status` is always `ready` in 1.4 (clips only appear once
  the preview exists). `title` is the stored title or `""`.
- `path` is the absolute path on the service host. The UI does not use it; a
  later implementation may keep or drop it.

### Job

```json
{
  "id": "412b2e96", "base": "Replay 2026-09-04 11-40-00",
  "start": 2, "end": 8, "seconds": 6, "audio": "gamemic", "kbps": 6000,
  "stage": "done", "percent": 100, "ok": true, "error": "",
  "at": "2026-09-04T11:38:59", "finished": "2026-09-04T11:39:00",
  "title": "", "file": "Replay_2026-09-04_11-40-00_2-8.mp4", "sizeMB": 0.3,
  "link": "https://cloud.example.com/s/abc123", "direct": "https://cloud.example.com/s/abc123/download",
  "ncPath": "/Clips/2026-09/Replay_2026-09-04_11-40-00_2-8.mp4",
  "discord": "Link posted"
}
```

- `stage` moves through `queued`, `encode`, `upload`, `discord`, then ends in
  `done` or `error` (since 2.4 also `cancelled`). Stages are never revisited. A client polling at
  intervals may miss intermediate stages.
- `percent` is only meaningful during `encode`: it follows ffmpeg's progress
  and stays at most `99` until ffmpeg exits, then `100`. The UI shows `100`
  during `upload` and `discord` regardless of the field.
- `ok` is `null` while running, then `true` or `false`. `error` is `null`
  while running, `""` on success and the message on failure.
- `start` and `end` are the clamped values; `seconds` is `end - start`
  rounded to two decimals; `kbps` is the configured video bitrate.
- Fields appear as the stages set them: `title` at start of `encode`; `file`
  and `sizeMB` after `encode`; `link`, `direct` and `ncPath` after `upload`;
  `discord` after `discord`. `discord` is a human-readable status string, not
  an enum (1.4 uses German text such as "Link gepostet").
- `finished` is set when the job ends.
- Stages that belong to a disabled integration may be skipped by a later
  implementation (for example no `upload` when no storage integration is
  active); the fields they would fill are then absent. The order of the
  remaining stages is unchanged.

### HistoryEntry

A copy of a successfully finished Job without `percent`, `stage`, `ok` and
`error`. Newest first; the service keeps 200 entries across restarts.

### Config

```json
{
  "shareKbps":  6000,
  "expireDays": 0,
  "version":    "1.4.1",
  "encoder":    "h264_amf",
  "audio": [
    { "id": "mix",         "label": "Mix (all)",                       "need": 1 },
    { "id": "gamemic",     "label": "Game + microphone (no voice chat)", "need": 4 },
    { "id": "game",        "label": "Game only",                       "need": 3 },
    { "id": "gamediscord", "label": "Game + voice chat (no microphone)", "need": 4 }
  ],
  "webhook":   true,
  "nextcloud": true
}
```

- `audio` lists the share modes in display order with the number of audio
  tracks a clip needs for the mode to be offered. Labels are for display and
  may be translated; ids are the contract.
- `version` and `encoder` are shown in the UI header.
- `webhook` and `nextcloud` say whether the respective integration is
  usable: 1.4 checks that credentials exist (Credential Manager read on
  every call), 2.0 reports whether the integration is enabled and has
  credentials, evaluated at startup.

## Since 2.1

Everything in this section exists from replaycut 2.1 on. A 1.x service does
not have it; the test suite checks `config.version` and skips these cases
below 2.1. The 1.4 contract above is unchanged.

### Access control

- **Origin check**, always on: a request with any method other than `GET`,
  `HEAD` or `OPTIONS` is refused with `403 { ok: false, error }` when it
  carries an `Origin` header that does not name the same host and port as
  the `Host` header (scheme ignored). Requests without `Origin` (scripts,
  the test suite) pass; a browser always sends it for cross-site writes.
- **Password**, optional. Set through `PUT /api/settings` (`password`),
  stored as an argon2id hash in `settings.json`, never returned. While a
  password is set, requests to `/api/*` and `/media/*` from any address but
  loopback need the session cookie `rc_session` (HttpOnly, SameSite=Strict,
  30 days); without it they get `401 { ok: false, error: "login required" }`.
  Loopback (`127.0.0.0/8`, `::1`) never needs a login. Pages, `/themes/*`,
  `/api/session`, `/api/login` and `/api/logout` are always open.
- JSON endpoints of 2.1 (`PUT /api/settings`, `POST /api/test/*`,
  `POST /api/login`) require `Content-Type: application/json` and answer
  `415` otherwise. `POST /api/save` keeps accepting an empty body.

### `GET /setup`, `/settings`, `/diagnostics`, `/login`, `/obs`

The UI file, exactly as `GET /`; the page's script shows the page named by
the path.

### `GET /api/settings`

The settings file without secrets plus what the settings page needs:

```json
{
  "clipDir": "C:\\Users\\you\\Videos", "port": 8420, "bind": "0.0.0.0",
  "displayName": "replaycut", "shareKbps": 6000, "encoder": "auto",
  "hwaccel": "", "ffmpegPriority": "belowNormal", "ffmpegThreads": 0,
  "logLevel": "info", "checkUpdates": true, "setupDone": true,
  "theme": "wardogs", "uiFile": "ui/index.html",
  "integrations": {
    "nextcloud": { "enabled": false, "url": "https://cloud.example.com", "folder": "Clips", "expireDays": 0 },
    "discord": { "enabled": false }
  },
  "secrets": { "nextcloud": false, "discord": false },
  "passwordSet": false,
  "autostart": false,
  "themes": ["plain", "wardogs"],
  "restartNeeded": [],
  "overrides": { "clipDir": false, "port": false, "bind": false },
  "version": "2.1.0"
}
```

`secrets` says whether a credential is stored, never what it is.
`restartNeeded` lists restart-only fields changed since the service started.
`overrides` marks fields a command-line flag overrides; saving them changes
the file but not the running service.

### `PUT /api/settings`

Body: a partial object with any of the fields above except `secrets`,
`passwordSet`, `autostart`, `themes`, `restartNeeded`, `overrides`,
`version`, plus these write-only keys:

| Key | Effect |
|---|---|
| `password` | sets the password (at least 6 characters); `""` removes it and ends every session |
| `nextcloudUser`, `nextcloudPassword` | together: stored in the Credential Manager |
| `discordWebhook` | stored in the Credential Manager; must look like a Discord webhook URL |
| `autostart` | `true`/`false`: the sign-in entry (Windows) |

Response `200 { ok: true, restartNeeded: ["port"], settings: <GET document> }`.
Everything but `port`, `bind` and `uiFile` takes effect at once: a new
clip folder is scanned, a new encoder is detected, integrations are
rebuilt, the next share uses the new bitrate. Errors: `400` with the field
name in `error` (`unknown field: x`, `invalid value: ...`, `port must not be
0`), `409` while a share runs and `clipDir` or `encoder` is in the body,
`415` without the JSON content type.

### `POST /api/test/nextcloud`

Body `{ url, folder, user?, password? }`; without `user`/`password` the
stored credential is used. Response `{ ok: true, user, displayName,
freeBytes, totalBytes, ms }` (quota fields `null` when unlimited) or
`{ ok: false, error }` with status `200` in both cases: the test ran, the
result is the payload.

### `POST /api/test/discord`

Body `{ webhook?, displayName? }`; without `webhook` the stored one. Sends
one test message. Response `{ ok: true }` or `{ ok: false, error }`; in dry
run `{ ok: true, dryRun: true }` without sending.

### `GET /api/addresses`

`{ hostname, port, bind, urls: ["http://<host>:<port>/", "http://<ip>:<port>/", "http://localhost:<port>/"], qrSvg }`.
`qrSvg` is an SVG document encoding `urls[0]`. With `bind` set to loopback
only the localhost address is listed, `local` is true and `qrSvg` is empty
(since 2.3): a code for localhost would only lead a phone to itself.

### `GET /themes/<name>.css`

`<data-dir>\themes\<name>.css` as `text/css`; `<name>` is lower-case
letters, digits and dashes. Anything else, including a missing file, is
`404`. The built-in theme `wardogs` has no file.

### `GET /api/session`, `POST /api/login`, `POST /api/logout`

- `GET /api/session` -> `{ authenticated, loopback, passwordSet }`.
  `authenticated` is true without a password, on loopback, or with a valid
  session cookie.
- `POST /api/login { password }` -> `200 { ok: true }` with a `Set-Cookie`
  for `rc_session`, `401` on a wrong password (after a 1 s delay), `429`
  after ten failures from the same address for 60 s.
- `POST /api/logout` -> `200 { ok: true }` and clears the cookie.

### `POST /api/restart`

`202 { ok: true }`: the service starts a new copy of itself that waits for
this one to exit, then shuts down. `409` while a share runs, `503` when the
process cannot restart itself.

### `GET /api/setup/obs`

What the setup wizard's OBS step shows. Read only; nothing in OBS changes.

```json
{
  "profiles": [
    { "name": "Gaming", "current": true, "mode": "Advanced", "recPath": "C:\\Users\\you\\Videos\\Clips", "format": "mkv" }
  ],
  "watching": "C:\\Users\\you\\Videos\\Clips",
  "newest": { "name": "Replay ... .mkv", "base": "Replay ...", "created": "2026-09-04T21:14:02", "duration": 300, "tracks": 4, "codec": "hevc", "width": 1920, "height": 1080, "fps": 60, "container": "mkv" },
  "otherFiles": ["Replay ... .mp4"],
  "encoder": "h264_nvenc"
}
```

`profiles` comes from `%APPDATA%\obs-studio` (`basic\profiles\*\basic.ini`,
current one from `user.ini` or `global.ini`), empty when OBS is not
installed for this user. `newest` is the youngest clip the scanner knows
or `null`; `otherFiles` lists up to three non-MKV recordings in the folder
(an MP4 there means OBS records in the wrong container).

Clips in `GET /api/clips` carry the same `codec`, `width`, `height` and
`fps` (empty or 0 when probing failed).

### `GET /api/diagnostics`

Runs every check (each with a 5 s timeout, in parallel) and answers

```json
{
  "checks": [
    { "id": "service",   "label": "replaycut",       "status": "ok",   "detail": "2.1.0 · running since ..." },
    { "id": "update",    "label": "Update",          "status": "ok",   "detail": "..." },
    { "id": "ffmpeg",    "label": "ffmpeg",          "status": "ok",   "detail": "7.1 at ..." },
    { "id": "encoder",   "label": "Encoder",         "status": "ok",   "detail": "h264_nvenc · priority BelowNormal · 8 threads" },
    { "id": "folder",    "label": "Recording folder","status": "ok",   "detail": "... · 212.0 GB free · 12 clips ..." },
    { "id": "scan",      "label": "Folder scan",     "status": "ok",   "detail": "last scan 2 s ago · watcher active" },
    { "id": "nextcloud", "label": "Nextcloud",       "status": "skip", "detail": "integration is off" },
    { "id": "quota",     "label": "Nextcloud quota", "status": "skip", "detail": "integration is off" },
    { "id": "webhook",   "label": "Discord webhook", "status": "fail", "detail": "HTTP 404 - ...", "fix": "Discord: Server settings › ..." },
    { "id": "obs",       "label": "OBS",             "status": "skip", "detail": "not connected - ..." },
    { "id": "network",   "label": "Network",         "status": "ok",   "detail": "listening on 0.0.0.0:8420 · http://<host>:8420/ ..." }
  ],
  "text": "replaycut 2.1.0 - 2026-09-04T21:25:10\nservice   OK    ...\n..."
}
```

`status` is `ok`, `warn`, `fail` or `skip`; `fix` accompanies warnings and
failures with what to do. `text` is the same list as plain text plus a
settings line and the last 20 log lines, without any secret - meant for
"Copy diagnostics". The eleven ids are stable; `replaycut test` prints
`text` when the service runs.

### `POST /api/jobs/<id>/open-folder`, `POST /api/jobs/<id>/copy-file`

The local mode's way out of the browser: for a finished job with a file in
`shared\`, `open-folder` opens Explorer with that file selected and
`copy-file` puts the file itself (a file object, `CF_HDROP`) into the
clipboard, so Ctrl+V in Discord attaches it. Response `{ ok: true }`
(`copy-file` adds `file`), `404` for an unknown job or a file that is gone,
`409` for a job without a finished file. In dry run both only log.

### Additions to `GET /api/clips`

`config` gains `setupDone`, `theme`, `passwordSet`, `localMode` (no storage
integration active) and `displayName`.

## Since 2.2

The OBS integration through obs-websocket 5 (built into OBS 28 and newer).
Read-only apart from two harmless actions. The service connects on its own
to `obs.host:obs.port` from `settings.json` (default `localhost:4455`,
`obs.enabled: true`) with the password stored as the credential
`replaycut/obs-websocket`, and keeps reconnecting with a backoff of 2 to
30 s while nothing answers.

### `POST /api/save` with OBS connected

- Connected and the replay buffer running: `SaveReplayBuffer` is sent;
  `200 { ok: true, via: "obs-websocket" }`.
- Connected but the buffer stopped: `409 { ok: false, error: "the replay
  buffer is not running - ..." }`. Nothing is pressed.
- Not connected: the key press of 1.x; `200 { ok: true, via: "hotkey" }`.

### `GET /api/obs`

The connection, the facts read from OBS and the checks the OBS page shows:

```json
{
  "enabled": true, "connected": true, "version": "30.2.3", "wsVersion": "5.5.2",
  "replayActive": true, "obsClosing": false,
  "lastSaved": { "path": "C:\\Users\\you\\Videos\\Clips\\Replay ....mkv", "at": "2026-09-04T21:14:02" },
  "facts": {
    "profile": { "name": "Gaming", "mode": "Advanced", "recPath": "...", "format": "mkv", "encoder": "jim_nvenc", "replaySeconds": 300, "recTracks": 15 },
    "video": { "width": 1920, "height": 1080, "fps": 60 },
    "inputs": [ { "name": "Mic", "kind": "wasapi_input_capture", "tracks": [1, 2] } ],
    "checkedAt": "2026-09-04T21:20:00"
  },
  "checks": [ { "id": "replay", "label": "Replay buffer", "status": "ok", "detail": "running (300 s)" } ],
  "settings": { "host": "localhost", "port": 4455, "enabled": true, "passwordSet": true }
}
```

Without a connection `connected` is false, `reason` says why in plain
words, `facts` is absent and `checks` is empty. Check ids: `replay`,
`folder`, `format`, `codec`, `tracks`; status `ok`, `warn`, `problem`;
`fix` names the OBS menu path; `action` is `start-replay-buffer` or
`adopt-folder` when a button applies. Facts are read on connect, on a
profile change and every 30 s.

### `POST /api/obs/replay-buffer/start`, `/api/obs/reconnect`, `/api/obs/adopt-folder`

- `replay-buffer/start`: `StartReplayBuffer`; `200 { ok }` (also when it
  already runs), `409` without a connection.
- `reconnect`: connect now instead of waiting out the backoff; always `200`.
- `refresh`: read the facts again now; `200 { ok }`, `409` without a connection.
- `adopt-folder`: makes the OBS recording folder the `clipDir` of replaycut
  (the same as `PUT /api/settings`); `200 { ok, clipDir }`, `409` without a
  connection or while a share runs.

### Additions to `GET /api/clips` and `/api/settings`

`config.obs` = `{ enabled, connected, replayActive }`. The settings
document carries `obs: { enabled, host, port }` and `secrets.obs`;
`PUT /api/settings` accepts `obs.enabled`, `obs.host`, `obs.port` and the
write-only `obsPassword` (`""` removes it). Changing any of them reconnects.

## Since 2.3

The one-click update. The daily check (since 2.0) now also remembers the
release notes and the assets; the service can download the release ZIP,
verify it and replace itself.

Trust: a release is installed only when its `SHA256SUMS` carries a valid
minisign signature by one of the public keys built into the running
replaycut, and the ZIP matches the sums. An unsigned or foreign release is
shown as available but the download ends in `error`.

### `GET /api/update`

```json
{
  "phase": "available",
  "current": "2.3.0",
  "installed": true,
  "checkUpdates": true,
  "percent": 0,
  "checkedAt": "2026-09-04 12:00:00",
  "justUpdated": false,
  "latest": {
    "version": "2.3.1", "url": "https://github.com/.../releases/tag/v2.3.1",
    "notes": "## Fixed
- ...", "publishedAt": "2026-09-04T11:00:00Z",
    "assetName": "replaycut-2.3.1-windows-x64.zip", "assetSize": 4200000
  },
  "error": null
}
```

`phase` is one of `idle` (no newer release known), `checking`, `available`,
`downloading` (`percent` 0-99), `ready` (downloaded and verified),
`installing`, `error` (`error` says why; `latest` stays). `latest` is absent
when nothing newer is known; `notes` is the release body as Markdown, cut at
16 KB. `installed` is false when this executable does not run from the app
folder (a development build or a copy run from the ZIP): then `install`
refuses and the UI offers the download link instead. `justUpdated` is true
on the first start after a one-click update until `POST /api/update/seen`;
while it is, `updatedNotes` and `updatedUrl` carry the release notes and
page of the version just installed (the UI shows "What's new").

### `POST /api/update/check`, `/download`, `/install`, `/seen`

`check` asks the releases API now and answers with the document above
(`ok: true`); 502 when GitHub cannot be reached. `download` starts the
download in the background and answers `{ "ok": true }` at once; 409 when no
update is known or one is already downloading; progress and the outcome show
in `GET /api/update`. `install` replaces the program files with the verified
package and restarts the service (like `/api/restart`); 409 when nothing is
ready, when this copy is not installed, or while a share is running. `seen`
clears `justUpdated` and the notes. The service restarts with the same
command line, so overrides such as `--port` stay in force. All four need a
session from the LAN, like every non-GET request.

`config.update` in `GET /api/clips` keeps its 2.0 shape (`null` or
`{ version, url }`).

### `POST /api/scanning`

`{ "paused": true | false }` pauses or resumes the folder scan: while paused,
new replays stay in the folder unseen (no clip, no toast) until scanning
resumes, when they appear as usual. Answers `{ "ok": true, "paused": ... }`;
400 without a boolean. The state lives in memory only: a restart scans
again. `config.scanning` in `GET /api/clips` is `{ "paused": bool }`; the
UI shows a banner with "Resume" while paused and the tray menu carries the
same switch ("Pause scanning").

## Since 2.4

### The share queue

`POST /api/share` no longer answers 409 while a job runs: the new job waits
and the answer is `202 { ok: true, job, position }` with `position` 0 when
it runs at once, else its place in the queue (1 = next). At most 10 jobs
wait; then `429 { ok: false, error }` with `Retry-After`. The same cut (same
`base`, `start`, `end` and `audio`) while it runs or waits still answers
`409 { ok: false, error, job }` naming that job, so a double click attaches
to the first request. One job runs at a time; the next one starts as soon as
the running one ends, in order.

The [Job](#job) carries `position` while `queued` (dropped once it runs) and
`cancelled: true` after a cancel. `GET /api/clips` lists the waiting ids as
`queue` (an array, oldest first) next to `busy` and `job`.

### `POST /api/jobs/<id>/cancel`

Ends a job: a waiting one leaves the queue at once (`{ ok: true, stopped:
true }`, its stage is `cancelled` immediately); a job in `encode` or `upload`
is stopped (`{ ok: true, stopped: false }`): ffmpeg is killed and the partial
file removed, an upload in flight is dropped and the started remote file
deleted, then the job ends with stage `cancelled`, `ok: false`, `error:
"cancelled"`, `cancelled: true`. A job in `discord`, `done`, `error` or
already `cancelled` answers 409; an unknown id 404. Cancelled jobs appear in
the history with `cancelled: true` and no link.

### Thumbnails

Every clip gets one JPEG, 320 px wide, taken 10 s before the end (the moment
that made someone press F9), or in the middle of clips under 15 s. The clip
carries `thumb: "/media/<base>.jpg"`, or `null` until the picture exists
(older clips get theirs one per second after the update). `GET
/media/<base>.jpg` answers `image/jpeg` with `Cache-Control: max-age=86400`;
the picture never changes. Thumbnails live next to the previews and are
removed with them.

### Storage quota

`config.quota` in `GET /api/clips` is `null` without a Nextcloud account, an
unlimited account or a failed check, else `{ usedPercent, free, total }`
(bytes). The service asks the account five seconds after start, every 15
minutes, after every upload and after a settings change. The UI shows
"Nextcloud NN %" in the header, yellow from 80 %, red from 95 %.

### Copy mode

`POST /api/share` accepts `mode`: `h264` (default: cut, scale to 1080p and
re-encode as today) or `copy` (the OBS video stream as it is, in an MP4
with `+faststart`; audio is copied for `mix` and mixed to AAC for the other
modes). Anything else is a 400. Copy is keyframe-accurate: the file starts at
the keyframe before `start`, so the job reports `actualStart` (seconds,
`<= start`) once the encode finished; `mode` is always in the job. A copy of
an AV1 or HEVC recording plays in Chrome, Edge and Firefox; Discord embeds and
iPhones need `h264`.

### `GET /api/events`

A Server-Sent Events stream (`text/event-stream`). On connect and after
every change the service sends `event: state` with the `GET /api/clips`
document as `data`; bursts of changes (job progress, a scan) become one
event. A `ping` comment goes out every 25 s. At most 8 streams are open at a
time, a ninth answers 503; the UI then polls `/api/clips` every 3 s as before
2.4 and tries the stream again later. The stream ends when the service shuts
down, so a restart does not wait for open connections.

## Since 2.5

### Share targets

Every configured storage integration is a target; `POST /api/share` takes
`target`: a storage id (`nextcloud`, `onedrive`, `s3`, `webdav`; `youtube`
since 2.6) or `file` for no upload. Missing or empty means the default: the storage marked
`quickShare` in the settings, or `file` when none is. An unknown or
unconfigured id is a 400. The job carries `target`. Notify integrations with
`autoPost` post every share that produced a link; their stage is `notify`
(2.4 and earlier called it `discord`; clients accept both). The status text
of the post stays in `discord` and names the integration when more than one
posted.

`config.targets` in `GET /api/clips` lists every integration replaycut knows:
`[{ id, label, kind: "storage" | "notify", enabled, connected, quickShare |
autoPost }]`. `connected` is true when the integration is enabled and has
its credentials. `config.nextcloud` keeps meaning "a default storage is
configured" and `config.webhook` "a notify integration posts automatically".

Settings: `integrations.nextcloud.quickShare` (default true) and
`integrations.discord.autoPost` (default true).

### `POST /api/jobs/<id>/publish`

`{ "target": "<storage id>" }` sends the finished file of a `done` job to
that storage without cutting again: a new job with `source: "<id>"`, the
same base, range, audio, mode, title and file, stages `queued -> upload ->
notify -> done`, queued like any share (`202 { ok, job, position, source }`).
400 for `file`, an unconfigured target or a source without a finished file;
404 for an unknown job; 409 when the same publish is already running or
waiting; 429 when the queue is full.

### OneDrive and `GET /api/oauth/<provider>`

OneDrive is the first storage that is an account rather than a server:
`integrations.onedrive { enabled, quickShare }` in the settings, the account
itself a refresh token in the Credential Manager (`replaycut/onedrive`),
connected through the OAuth device-code flow so it works from a phone as
well. Uploads land in `Apps/replaycut/<month>/` and get an anonymous view
link; `page` and `direct` are the same link.

- `GET /api/oauth/<provider>` (`onedrive`): `{ provider, label, configured,
  connected, account, flow }`. `configured` is false when the build carries
  no client id (then nothing can be connected); `flow` is `null` or the
  device flow in progress: `{ status: "pending" | "done" | "failed",
  userCode, verificationUri, expiresIn, account?, error? }`.
- `POST /api/oauth/<provider>/start`: begins a device flow and answers with
  the document above including `userCode` and `verificationUri` (`ok: true`);
  a running flow is returned as is. 409 without a client id, 502 when the
  provider cannot be reached, 404 for an unknown provider. The page polls
  `GET` every few seconds; on `done` the runtime is rebuilt and the storage
  is a target.
- `POST /api/oauth/<provider>/disconnect`: forgets the account.

Quick share is exclusive: a `PUT /api/settings` that sets one storage's
`quickShare` to true clears it on the others.

### S3 and WebDAV

Two more storages, both server-style like Nextcloud:

- `integrations.s3 { enabled, quickShare, endpoint, region, bucket, prefix,
  publicBase, presignDays }` with the keys as the write-only pair
  `s3AccessKey` / `s3SecretKey` in `PUT /api/settings` (credential
  `replaycut/s3`; both empty removes them). Objects land at
  `<prefix>/<month>/<file>`; the link is `<publicBase>/<key>` or, with an
  empty `publicBase`, a presigned GET that expires after `presignDays`
  (1-7). Requests use Signature Version 4 with an unsigned payload and
  path-style addressing, which AWS S3, Cloudflare R2, Backblaze B2, MinIO
  and Wasabi accept.
- `integrations.webdav { enabled, quickShare, url, folder, publicBase }` with
  the login as `webdavUser` / `webdavPassword` (credential
  `replaycut/webdav`). Files go to `<url>/<folder>/<month>/<file>`, the link
  is `<publicBase>/<month>/<file>`: a plain DAV server has no public links,
  so the same folder must be served publicly and `publicBase` is required.
- `POST /api/test/s3` and `POST /api/test/webdav` take the same fields as the
  settings block (plus the credential pair) and answer `{ ok, ... }` after a
  reachability check and a probe file that is written and removed; `{ ok:
  false, error }` names what to fix. Validation errors are reported the same
  way, not as 400.
- `DELETE /api/clips/<base>?nextcloud=1` removes the remote copies from every
  configured storage a job of the clip went to (paths from the history);
  `nextcloud` in the answer counts them all.

## Since 2.6

### YouTube

YouTube is a storage target (`youtube` in `config.targets`) whose "file" is
a video of its own: `videos.insert` through a resumable upload, title from
the clip's title (or its name without the display-name prefix), description
from a template, `privacyStatus` from the settings (`unlisted` by default),
not made for kids, category Gaming. `page` and `direct` are both
`https://youtu.be/<id>`, `ncPath` is the video id; deleting a clip with
`?nextcloud=1` deletes the videos its jobs uploaded.

- Settings: `integrations.youtube { enabled, quickShare, privacy: "unlisted"
  | "private" | "public", description }`. `description` may use `{title}`,
  `{clip}` (the base name) and `{date}` (`YYYY-MM-DD` from the clip name,
  else from the job). `privacy` outside the three values is a 400.
- The Google client is the user's own (quota: 1600 units per upload out of
  10 000 a day per project): the write-only pair `youtubeClientId` /
  `youtubeClientSecret` in `PUT /api/settings` (credential
  `replaycut/youtube-client`; both empty removes it, half a pair is a 400).
  Storing a client disconnects the channel connected with the previous one.
  `secrets.youtubeClient` says whether one is stored, `secrets.youtube`
  whether a channel is connected.
- `GET /api/oauth/youtube` works like OneDrive's document; `configured` is
  false until a client is stored, and `POST /api/oauth/youtube/start`
  answers 409 then. The device flow runs against Google's limited-input
  endpoint (`https://www.google.com/device`), which wants a client of the
  type "TVs and Limited Input devices" and the scope
  `https://www.googleapis.com/auth/youtube`; `account` is the channel title.
- A vertical share (below) gets ` #Shorts` appended to its title; YouTube
  lists it as a Short by its format and length.

### Loopback login

A second way to connect an account, for providers or client types without
a device flow: `integrations.youtube.clientType` is `tv` (default, the
device flow above) or `desktop` (a Google "Desktop app" client). The
provider document carries `loopback: true` then, and:

- `POST /api/oauth/<provider>/loopback` starts the login: it answers the
  document plus `url`, the provider's authorization page with PKCE (S256),
  `state`, `access_type=offline` and `prompt=consent`, and the redirect
  `http://127.0.0.1:<port>/oauth/<provider>/callback`. The page opens the
  URL in a new tab; that only works in a browser on the PC that runs
  replaycut, so the card says so when the page was loaded from elsewhere.
  409 without a client, 400 when the provider connects with a code (and
  `/start` answers 400 for a loopback provider), 404 for an unknown one.
  The login expires after ten minutes.
- `GET /oauth/<provider>/callback?code&state` is where the provider sends
  the browser back. It checks `state`, exchanges the code with the
  verifier, stores the refresh token and answers a small HTML page for that
  tab (200 connected, 400 refused or unknown state, 404 unknown provider);
  `error` and `error_description` from the provider end the flow as
  `failed`. The settings card keeps polling `GET /api/oauth/<provider>` and
  sees `done` or `failed` like with the device flow. The route sits outside
  `/api/` and needs no session: the state token is the proof.

### Vertical cut

`POST /api/share` takes `vertical: true` and `verticalPos` (0..1, default
0.5): the video is cropped to a 9:16 window of full height whose left edge
sits at `(iw - ih*9/16) * verticalPos`, scaled to 1080x1920, at the usual
bitrate. It needs the `h264` mode (`copy` with `vertical` is a 400). The job
and the history entry carry `vertical: true` and `verticalPos`; the file
name gets `_9x16` before `.mp4`, so the same range with and without the crop
are two shares, not a duplicate. A publish job inherits both fields.

## Behaviour

### Folder scan

- The service scans the clip folder for `*.mkv` (top level only) every 2
  seconds, oldest file first.
- A file becomes a clip when it is at least 2 seconds old (last-write time)
  and can be opened exclusively (OBS still writing means "not yet").
- On first sight the preview is created (`.preview/<base>.mp4`, remux of
  `0:v:0` and `0:a:0`, `-c copy`, `+faststart`), the duration is probed and
  the clip is added. Preview creation for large files takes well under a
  second; a new clip is expected to appear in `/api/clips` within 5 seconds
  of the file being complete.
- Clips whose MKV disappeared are removed from the list, their previews and
  seen entries are cleaned up. Previews without an MKV are deleted.
- The scan is independent of HTTP traffic; a stalled scan is visible through
  `scanAt`.

### Toast on new clips ("seen" logic)

- Every clip is announced exactly once with a desktop notification, tracked
  by `base` in a persisted seen-list.
- On the very first start (no seen-list yet) the existing files are recorded
  silently. From then on every unknown clip is announced, however old the
  file is or however long the service was down.
- Seen entries whose MKV no longer exists are dropped.

### Share pipeline

1. `queued`: job created and registered; `busy` becomes `true`.
2. `encode`: ffmpeg cuts `[start, start + seconds]` from the MKV with input
   seeking (`-ss` before `-i`, frame-accurate), scales to 1080p height
   (`scale=-2:1080`), encodes H.264 with the detected encoder at `shareKbps`
   (CBR, `maxrate = kbps`, `bufsize = 2 * kbps`), audio per mode as AAC 128k,
   `+faststart`. Progress comes from `-progress pipe:1` (`out_time_us`).
   Output file: `shared/<base with whitespace as _>_<start>-<end>[_<slug>].mp4` with
   `start` and `end` rounded to whole seconds,
   where `slug` is the title with every run of characters other than
   `[A-Za-z0-9_-]` replaced by `-`, trimmed of `-`, cut to 40 characters.
3. `upload`: the file is uploaded to `<folder>/<YYYY-MM>/` where the month
   comes from the first `YYYY-MM-DD` in `base` (fallback `unsortiert` in 1.4,
   `unsorted` in 2.0). A public read-only link is created; if one already exists for that
   path it is reused. `link` is the share page, `direct` is `<link>/download`,
   `ncPath` is `/<folder>/<month>/<file>`. The direct link is put into the
   clipboard.
4. `discord`: one message is posted through the webhook:
   `**<prefix>** [<title> - ]<base without a leading "<prefix> " part> (<int seconds> s) - <direct>`.
   In 1.4 the prefix is the hard-coded string `WARDOGS`; 2.0 makes it the configured display name.
   Only the bare direct link, never an attachment; 2.0 sends the display
   name as the webhook user name. Missing webhook credentials are not an
   error (`discord` says so; in 2.0 the stage is skipped).
5. `done`: the job is appended to history, `busy` is `false`, `last` is the
   job. Any failure in steps 2 to 4 ends the job with `error` instead; the
   partially produced file (if any) stays in `shared/`.

Encoder detection happens at startup: `h264_amf`, `h264_nvenc`, `h264_qsv`,
`libx264` are tried in that order with a real two-frame encode; the first
that works wins and is reported as `config.encoder`.

### Delete

See `DELETE /api/clips/<base>`. Files go to the recycle bin, never
`unlink`. The title and, with `?nextcloud=1`, the history entries are removed.

### Limits

- One share job at a time; 30 jobs kept in memory; 200 history entries and
  the titles and seen-list persisted as JSON.
- Log lines and toast texts are not part of the contract.

## Notes for the 2.0 implementation

- Keep every status code and field above. Where this document says a
  different code is acceptable later, the tests already accept both.
- The UI reads: `clips[]` (`base`, `title`, `duration`, `size`, `tracks`,
  `preview`), `config` (`version`, `encoder`, `expireDays`, `audio[]`),
  `history[]` (`id`, `title`, `base`, `finished`/`at`, `seconds`, `sizeMB`,
  `audio`, `link`, `direct`), `job`, `last`, `scanAt`, and on jobs `stage`,
  `percent`, `seconds`, `kbps`, `audio`, `sizeMB`, `ok`, `error`, `direct`,
  `link`, `discord`, `finished`, `at`, `title`, `base`.
- Paths with spaces and non-ASCII characters (OBS file names) must work
  end to end: folder scan, `/media/` URL encoding, ffmpeg arguments, share
  file names.
