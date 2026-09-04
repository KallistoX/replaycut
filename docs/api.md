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

- Only one job runs at a time. While a job runs, every share request answers
  `409 { ok: false, error, job: "<running id>" }`. The UI attaches itself to
  that job id.
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
  `done` or `error`. Stages are never revisited. A client polling at
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
only the localhost address is listed.

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
- `adopt-folder`: makes the OBS recording folder the `clipDir` of replaycut
  (the same as `PUT /api/settings`); `200 { ok, clipDir }`, `409` without a
  connection or while a share runs.

### Additions to `GET /api/clips` and `/api/settings`

`config.obs` = `{ enabled, connected, replayActive }`. The settings
document carries `obs: { enabled, host, port }` and `secrets.obs`;
`PUT /api/settings` accepts `obs.enabled`, `obs.host`, `obs.port` and the
write-only `obsPassword` (`""` removes it). Changing any of them reconnects.

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
