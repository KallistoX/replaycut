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

`{ "history": [HistoryEntry, ...] }`, newest first, capped at 200 entries
(since 3.0 the cap is a page size, see [below](#get-apihistorylimitbefore)).

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
  "platform":   "windows",
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
- `platform` (since 3.2): `windows` or `linux`, the operating system the
  service runs on (`std::env::consts::OS`). The page words things for it:
  "Credential Manager" or "keyring", "recycle bin" or "trash", where a
  stopped service is started from. Absent before 3.2; the page then assumes
  Windows.
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

Since 2.8 this section has three additions: every request is checked
against its `Host` header, the password has a length rule and a generator,
and a device can be let in without the password at all (the device login).
`requireLoginOnLoopback` makes this PC sign in as well. See "Since 2.8".

### `GET /setup`, `/settings`, `/diagnostics`, `/login`, `/obs`

The UI file, exactly as `GET /`; the page's script shows the page named by
the path. Since 2.8 `/approve` and `/approve/<id>` join them, since 3.0
`/activity`.

### `GET /api/settings`

The settings file without secrets plus what the settings page needs:

```json
{
  "clipDir": "C:\\Users\\you\\Videos", "port": 8420, "bind": "127.0.0.1",
  "allowedHosts": [], "requireLoginOnLoopback": false,
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
| `nextcloudUser`, `nextcloudPassword` | together: stored in the credential store (the Credential Manager; the Secret Service on Linux) |
| `discordWebhook` | stored in the credential store; must look like a Discord webhook URL |
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
`qrSvg` is an SVG document encoding `urls[0]`. `<host>` is the computer name
on Windows; on Linux it is `<computer name>.local` when the machine
announces itself over mDNS, else the IPv4 address (since 3.2). `hostname`
stays the bare computer name. With `bind` set to loopback
only the localhost address is listed, `local` is true and `qrSvg` is empty
(since 2.3): a code for localhost would only lead a phone to itself.
Since 2.8 the code signs the scanning phone in and `qrSignsIn` says so -
see "QR pairing" below; the `urls` never carry a token.

### `GET /themes/<name>.css`

`<data-dir>\themes\<name>.css` as `text/css`; `<name>` is lower-case
letters, digits and dashes. Anything else is `404`, and before 3.2 a
missing file was too. The built-in theme `wardogs` has no file.

Since 3.2 twelve themes are built into the executable: `catppuccin-frappe`,
`catppuccin-latte`, `catppuccin-macchiato`, `catppuccin-mocha`, `dracula`,
`gruvbox-dark`, `material-dark`, `material-light`, `nord`, `plain`,
`solarized-dark`, `tokyo-night`. The folder is read first, so a file of the
same name overrides the built-in theme and a user can copy one and change
it; with neither a file nor a built-in the answer stays `404`.
`settings.themes` lists `wardogs`, the built-in names and the folder
together, sorted and without duplicates.

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

`profiles` comes from `%APPDATA%\obs-studio` (`~/.config/obs-studio` or the
Flatpak's copy on Linux; `basic\profiles\*\basic.ini`, current one from
`user.ini` or `global.ini`), empty when OBS is not installed for this user. `newest` is the youngest clip the scanner knows
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
    { "id": "network",   "label": "Network",         "status": "ok",   "detail": "listening on 0.0.0.0:8420 · http://<host>:8420/ ... · password set" },
    { "id": "firewall",  "label": "Firewall",        "status": "ok",   "detail": "rule \"replaycut\" allows the port in private networks" },
    { "id": "pairing",   "label": "Device login",    "status": "ok",   "detail": "ready" }
  ],
  "text": "replaycut 2.1.0 - 2026-09-04T21:25:10\nservice   OK    ...\n..."
}
```

`status` is `ok`, `warn`, `fail` or `skip`; `fix` accompanies warnings and
failures with what to do. `text` is the same list as plain text plus a
settings line and the last 20 log lines, without any secret - meant for
"Copy diagnostics". The ids are stable and only grow (2.5 added the
storage and notify integrations, 2.8 `firewall` and `pairing`);
`replaycut test` prints `text` when the service runs.

### `POST /api/jobs/<id>/open-folder`, `POST /api/jobs/<id>/copy-file`

The local mode's way out of the browser: for a finished job with a file in
`shared\`, `open-folder` opens Explorer with that file selected and
`copy-file` puts the file itself (a file object, `CF_HDROP`) into the
clipboard, so Ctrl+V in Discord attaches it. On Linux `open-folder` asks the
file manager over D-Bus (`org.freedesktop.FileManager1.ShowItems`) and falls
back to opening the folder, and `copy-file` offers the file as
`text/uri-list` on the Wayland clipboard. Response `{ ok: true }`
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
    "assetName": "replaycut-2.3.1-windows-x64.zip", "assetSize": 4200000,
    "packaged": true
  },
  "error": null
}
```

`phase` is one of `idle` (no newer release known), `checking`, `available`,
`downloading` (`percent` 0-99), `ready` (downloaded and verified),
`installing`, `error` (`error` says why; `latest` stays). `latest` is absent
when nothing newer is known; `notes` is the release body as Markdown, cut at
16 KB. `latest.packaged` (since 3.2) says whether the release carries the ZIP for
this platform; without it the release is shown as available, `download`
ends in `error` and the page offers the release page instead of "Update
now". `installed` is false when this executable does not run from the app
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
The source may be a job of this run or a history entry (since 2.6.1; before
that, entries from before the last restart answered 404). 400 for `file`,
an unconfigured target or a source without a finished file; 404 for an
unknown job; 409 when the same publish is already running or waiting; 429
when the queue is full.

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

### Telegram and the generic webhook

Two more notify integrations next to Discord (`telegram` and `webhook` in
`config.targets`, kind `notify`). Like Discord they post every share that
produced a link when `autoPost` is on, in the `notify` stage, and their
status text joins the job's `discord` field.

- `integrations.telegram { enabled, autoPost, chatId }` with the bot token
  as the write-only `telegramToken` in `PUT /api/settings` (credential
  `replaycut/telegram`; empty removes it, a value without `:` is a 400).
  The post is `sendMessage` with HTML: `<b><displayName></b> <label> (<n>
  s)` and the direct link on the next line. `POST /api/test/telegram {
  token?, chatId? }` runs `getMe` and sends a test message: `{ ok, bot,
  posted, ms }` or `{ ok: false, error }`.
- `integrations.webhook { enabled, autoPost, url }` with the optional
  secret as the write-only `webhookSecret` (credential
  `replaycut/webhook-secret`). `url` must be http(s) (400 otherwise). Every
  share is a `POST` with `Content-Type: application/json`, header
  `X-Replaycut-Event: shared` and the body `{ event: "shared", title, clip,
  seconds, target, link, direct, at, job, displayName }`; with a secret the
  header `X-Replaycut-Signature: sha256=<hex HMAC-SHA256 of the exact
  body>` is added. Any 2xx counts as posted. `POST /api/test/webhook { url?,
  secret? }` sends `{ event: "test", at, version }` the same way and
  answers `{ ok, status, signed, ms }`.
- Diagnostics rows `telegram` (`getMe`) and `generic-webhook` (configuration
  only; the receiver is not called).

### `GET /api/jobs/<id>/file`

The finished MP4 of a job as a download: `Content-Type: video/mp4`,
`Content-Disposition: attachment; filename="<file>"` (plus the RFC 5987
form), range requests as for `/media/`. The job may come from the jobs or
the history. 404 for an unknown job, a job without a finished file or a
file that is gone. The result card and the history offer it as "Download":
on a phone the file lands in the gallery, on a PC in the downloads folder.

### Playable preview

Browsers that cannot decode the recording's codec (an iPhone with AV1, for
example) get a 720p H.264 copy of the clip: `.preview/<base>.h264.mp4`,
served as `GET /media/<base>.h264.mp4` and listed as `clip.previewH264`
(`null` until it exists). Cutting stays on the original; the copy shares
its time axis.

- `POST /api/clips/<base>/preview` queues the copy as a job of `kind:
  "preview"` (`202 { ok, job, position }`; stages `queued -> encode ->
  done`, `percent` from ffmpeg, cancel as for a share). Its bitrate is
  2000 kbit/s at the profile's encoder and priority. A preview job never
  enters the history and is not the `last` job. 404 for an unknown clip,
  409 when the copy exists or is being made (`job` names it), 429 when the
  queue is full.
- Settings: `previewH264` is `onDemand` (default: the page shows "Make a
  playable preview" when `browserPlays()` says no) or `always` (the copy is
  made right after the scan, behind the running jobs, with ffmpeg at idle
  priority). Other values are a 400.
- The copy is removed with the clip and by the orphan cleanup.

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

## Since 2.7

### Quality by default, limits per target

Shares keep the recording's resolution and frame rate and are encoded with
the encoder's quality mode (a fixed step per encoder, no bitrate): the
size follows the picture. The global `shareKbps` is gone: `PUT
/api/settings` with it is a 400, `config.shareKbps` is `0`, and the job's
`kbps` is `0` for a quality-driven encode.

Every storage block gains `maxHeight` and `maxKbps` (0 = none, the
default). A share to a target with limits is scaled to at most `maxHeight`
(the profile's scale filter) and capped at `maxKbps` (constant bitrate as
before 2.7); the job carries `kbps` and `maxHeight` as used. `maxHeight`
must be 0 or 240..4320, `maxKbps` 0 or 500..200000 (400 otherwise). A
`publish` to a target whose limits the finished file exceeds (a
quality-driven or copy-mode file against a bitrate cap, a file without a
height cap against a height cap) cuts the clip again within them instead:
the answer is a plain share job for that target, 404 when the clip is
gone. Limits that a service imposes itself (X: 512 MB, 140 s) are checked
by that integration and reported as the share's error.

The share mode `copy` is unchanged and is labelled "As recorded (no
re-encode)" in the UI.

### Posting on request

Notify integrations with `autoPost` post only the quick share: a share
whose `target` is the default storage and that is not a `publish`. Shares
to other targets and publishes skip the `notify` stage.

`POST /api/jobs/<id>/post { "target": "<notify id>" }` posts the link of
a finished job (from the jobs or the history) to that notify integration
now and answers `200 { ok, status }` with the integration's status text,
which is also appended to the job's `discord` field (in the jobs, the
history and `config.last`). 400 for a job without
a link or an unknown or unconfigured notify target, 404 for an unknown
job. The result card and the history offer it as "Post to ...".

## Since 2.8

### The recording's codec and bitrate on the job

A share job and its history entry carry `codec` (the recording's video
codec as in the clip, e.g. `av1`) and `sourceKbps` (the recording's total
bitrate in kbit/s, from its size and duration, at the time of the share);
a publish inherits both from its source. The UI learns the size estimate
of the share row from them: the median ratio of output to recording
bitrate over the last plain H.264 shares of the same codec (no copy mode,
no vertical cut, no limits) replaces the fixed per-codec factor once one
such share exists.

### Host check

Every request is checked against its `Host` header (the name, without the
port). Allowed are `localhost` (and `*.localhost`), any IP address, this
machine's name (also with `.local`) and every name in `allowedHosts`.
Anything else is refused with `421 { ok: false, error: "unknown host" }`,
before any handler runs - that is the answer for pages, `/api/*`,
`/media/*` and `/themes/*` alike. A request without a `Host` header
passes: the check is against DNS rebinding, which needs a name; an IP
address in the header cannot be rebound.

`allowedHosts` (settings, default `[]`) takes up to 20 host names without
scheme or port, for an own DNS name or a reverse proxy; letters, digits,
`-`, `.` and `_`, at most 253 characters each (400 otherwise).

### Password rules and the passphrase generator

A password is 8 to 128 characters; `PUT /api/settings` with a shorter or
longer one answers `400 { ok: false, error }` and changes nothing (the
empty string still removes the password). No rules about character
classes.

`GET /api/password/suggest` -> `200 { password: "orbit-velvet-cactus-lantern" }`:
four words from a built-in list (EFF Short Wordlist #1, 1296 words),
joined with `-`, drawn fresh for every call. Nothing is stored - the
password exists once the caller sends it back with `PUT /api/settings`.

### The login throttle over all addresses

Beside the ten failures per address (60 s, since 2.1), more than 30 failed
logins from all addresses together within five minutes pause the password
login for ten minutes: `POST /api/login` answers `429 { ok: false, error }`
until then, for every address.

### Sessions know their device

A session in `sessions.json` carries `id` (a random handle, not a secret),
`name` ("iPhone, Safari", guessed from the `User-Agent`), `agent`, `ip`,
`created`, `lastSeen` (written at most once a minute) and `via`
(`password`, `approve` or `qr`). Sessions written before 2.8 keep working
and are filled in on the first start: an id, `Unknown device`, `lastSeen`
= `created`, `via: password`.

### Additions to `GET /api/session`

Besides `authenticated`, `loopback` and `passwordSet`:

| Field | Meaning |
| --- | --- |
| `host` | The name of the PC this service runs on. Pages use it instead of "this PC", which is wrong on a phone. |
| `network` | `loopback` (bind `127.0.0.1` or `::1`), `lan` (`0.0.0.0` or `::`) or `custom` (any other bind address). |
| `pairing` | Whether the device login accepts requests. |

### The device login

A device that has no session asks, and the PC answers. Everything about a
request lives in memory: at most five open at a time, two minutes each.

- `POST /api/pair/request { name }` -> `202 { ok: true, id, code, expires }`.
  `id` is 16 random bytes as hex, `code` four characters out of
  `ABCDEFGHJKMNPQRSTUVWXYZ23456789` (shown on both sides, never typed),
  `expires` the seconds the request lives. An empty or missing `name` is
  filled in from the `User-Agent`. The service records name, agent, address
  and time, shows a toast whose button opens `/approve/<id>` over loopback,
  counts the request in the tray and puts it into `pending` (below).
  `429 { ok: false, error }` when the device login is paused or when there
  are too many requests: at most five open, five a minute; requests from
  more than three addresses within a minute pause the device login for ten
  minutes (the password login keeps working, and `GET /api/session` reports
  `pairing: false`).
- `GET /api/pair/<id>` -> `200 { ok: true, status }` with `status` one of
  `pending`, `approved`, `denied`, `expired`. The device polls it every two
  seconds. The first answer after an approval carries the `Set-Cookie` for
  `rc_session`; every later one does not. A poll from another address than
  the one that asked answers `denied`, and an unknown or forgotten `id`
  answers `expired`.
- `POST /api/pair/<id>/approve`, `POST /api/pair/<id>/deny` ->
  `200 { ok: true, name }`, only for loopback and signed-in clients
  (`401` otherwise). `404` for an unknown request, `409` for one that is
  already answered or has run out. An approval creates the session with the
  device's name and `via: approve`.
- `GET /api/pair/pending` -> `200 { ok: true, pending: [...] }`, the same
  list as below, for the approve page.
- `GET /approve` and `GET /approve/<id>` serve the UI file like the other
  pages (since 2.1): the requests as cards with Allow and Deny, the one
  named in the path first.
- `GET /api/clips` and the `state` event of `GET /api/events` carry
  `pending: [ { id, name, agent, ip, code, asked, expires } ]` **for
  loopback and signed-in clients only** - the code belongs to the person
  who decides. Other clients see no such field.

### QR pairing

`GET /api/addresses` answers loopback and signed-in clients with a QR code
whose URL carries `?pair=<token>` (32 random bytes, base64url, good once
and for two minutes) and sets `qrSignsIn: true`; every other client gets
the plain address and `qrSignsIn: false`. The listed `urls` never carry a
token. A page that shows the code asks for a new one every 90 s.

`GET /?pair=<token>` redeems it: `303` to `/` with the `Set-Cookie` for a
session (`via: qr`), or `303` to `/login?pair=expired` when the token is
used up or too old. Tokens are stored as SHA-256 and never logged.

### `requireLoginOnLoopback`

Settings flag, default `false`. With it, this PC needs the login as well -
for a Windows account other people use. It applies to `/api/*` and
`/media/*` as for any other client, and therefore to answering sign-in
requests: the approve page asks for the password first, and the waiting
device keeps waiting.

### Network access

A new installation binds to `127.0.0.1`: only this PC. `bind` in the
settings still takes any address (and `--bind` overrides it), but the
switch in the wizard and in the settings knows two states and calls:

- `POST /api/network/enable` -> `200 { ok, network: "lan", firewall, restartNeeded }`.
  It sets `bind` to `0.0.0.0`, then adds the firewall rule for the port in
  private networks through the Windows administrator prompt. `409` while
  no password is set: without one there is nothing to protect the service
  with. `firewall` is `added`, `declined` (the prompt was refused),
  `failed`, `dryRun` or `unavailable` - the switch flips either way, and
  the diagnostics report a missing rule.
- `POST /api/network/disable` -> `200 { ok, network: "loopback", firewall, restartNeeded }`,
  with the rule removed the same way.

The listener follows `bind` only after a restart, so `restartNeeded` is
true and the caller sends `POST /api/restart` and waits for the service to
answer again.

`GET /api/clips` carries `config.network` (`loopback`, `lan` or `custom`).
`lan` without a password is what installations from before 2.8 look like:
the page shows a red banner ("Reachable from the network without a
password") with "Set a password" and "This PC only", the service logs a
warning and shows one toast per start, and the diagnostics fail.

### Signed-in devices

- `GET /api/sessions` -> `200 { ok, sessions: [ { id, name, agent, ip, created, lastSeen, via, current } ] }`,
  newest first; `current` marks the caller's own session.
- `DELETE /api/sessions/<id>` -> `200 { ok, name }`, `404` for an unknown
  id. That device needs to sign in again at once.
- `POST /api/sessions/clear` -> `200 { ok, removed }`: every session but
  the caller's own.

### Diagnostics of 2.8

Three lines beside the existing ones:

| Check | What it says |
| --- | --- |
| `network` | Where the service listens. `fail` when it is on the network without a password. |
| `firewall` | Whether the rule for the port exists; `skip` while network access is off. |
| `pairing` | The device login: ready, how many devices wait, or paused after a flood. |

## Since 3.0

### The cut between the recording and every rendering

Everything a rendering is made from is now a **cut**: the range of a
recording as its own file, `.cuts\<id>.mkv` in the clip folder, taken with a
stream copy from the keyframe at or before `start` to `end`, with the
picture untouched and **every** audio track of the recording. Nothing is
re-encoded, so a cut costs about as long as copying its bytes and no GPU.

Audio mode, the 9:16 window, the bitrate and the height cap are therefore
parameters of the *rendering*, not of the cut: the same cut can be rendered
again with another audio mode, to another target, later - the recording is
no longer needed for that.

A range is one cut per clip: the same range asked for twice answers with the
cut that is already there instead of making a second file.

### Cut

```json
{
  "id": "24af8830",
  "base": "Replay 2026-09-08 13-40-00",
  "start": 6.0,
  "end": 12.0,
  "audio": "mix",
  "vertical": false,
  "verticalPos": 0.5,
  "file": "24af8830.mkv",
  "actualStart": 0.0,
  "created": "2026-09-08T13:41:48",
  "state": "ready",
  "outputs": [HistoryEntry, ...]
}
```

- `id`: eight hex characters.
- `start`, `end`: the exact range in the recording, in seconds.
- `audio`, `vertical`, `verticalPos`: what the next rendering of this cut
  uses unless it says otherwise.
- `file`: the name in `.cuts\`, `null` while the cut is being made and for
  cuts the migration derived from the history of 2.x.
- `actualStart`: where the file really begins - the keyframe at or before
  `start`. A rendering seeks `start - actualStart` into the file.
- `state`: `pending` (the file is being made), `ready`, `missing` (there is
  no file).
- `outputs`: the finished jobs of this cut that left a file or a link
  behind, newest first, in the shape of a [HistoryEntry](#historyentry).

### `POST /api/cuts`

Body as `POST /api/share` minus `target` and `mode`: `base`, `start`, `end`,
`audio`, `vertical`, `verticalPos`. Saves the range and renders nothing.

`202 { ok: true, job, position, cut }` - the job has the stages
`queued -> cut -> done` and `kind: "cut"`. It is no output: it never appears
in the history.

- `404` unknown clip, `400` a range under a second or an unknown audio mode.
- `409 { ok: false, error, cut }` when the range is already a cut with its
  file; `409 { ok: false, error, job }` when a job for it is already running.

### `GET /api/cuts/<id>`

`200` with the [Cut](#cut) and its `outputs`, `404` when the id is unknown.

### `POST /api/cuts/<id>/render`

```json
{ "target": "nextcloud", "mode": "h264", "audio": "game",
  "vertical": false, "verticalPos": 0.5 }
```

Encodes a cut that exists and sends it on. Everything the body leaves out
comes from the cut; `target` may be `file` (render without an upload).
Stages `queued -> encode -> upload -> notify -> done`, `kind: "render"`.

`202 { ok: true, job, position, cut }`.

- `404` unknown cut, `400` unknown target, unknown audio mode or `mode`
  other than `h264`/`copy`, `409` the same render is already running.
- `400` when the cut has no file any more: cut the range again.

The clip may be gone by then; a cut is rendered from its own file.

### Additions to `POST /api/share`

The answer carries `cut` (the cut of this range, made or reused), and the
pipeline gains the stage `cut` before `encode`.

### Additions to `GET /api/clips`

Every clip carries `cuts: [Cut]`, oldest first, each with its `outputs`, plus
the state fields below. The document gains `counts: { active, done }`.

### Additions to the job

- `kind`: `share` (the default, absent in the document), `cut`, `render`,
  `publish` or `preview`. Until 2.8 a publish was a share with a `source`;
  it now says `publish` as well and keeps `source`.
- `cut`: the cut this job cut, rendered or published from. Absent for a
  preview and for history entries written before 3.0.

### The state of a clip

A clip is `new` (nothing cut yet), `active` (it has cuts) or `done` (it is
out of the list). `GET /api/clips` leaves the done ones out; `?done=1` lists
them all. `counts` in the [State](#state) says how many there are of each,
whichever way the list was asked for.

Every clip carries `state`, `doneAt` (when it was marked, else `null`),
`firstSeen` (when the scanner first saw the recording) and `file` - the name
of the recording, or `null` once it has gone to the recycle bin. A clip
without its recording stays in the list as long as it has cuts: its
thumbnail, its cuts and their outputs are still there, only the player has
nothing to play.

### `PUT /api/clips/<base>/state`

`{ "state": "done" }` or `{ "state": "active" }` ->
`200 { ok: true, base, state, doneAt }`.

- `done` takes the clip out of the list. It does not touch a running job,
  and it deletes nothing.
- `active` brings it back; a clip without cuts becomes `new` again, which is
  the same thing to the list.
- `400` for any other value, `404` for a clip nobody knows.

### "Afterwards": `after`

`POST /api/share` and `POST /api/cuts/<id>/render` take `after` in the body:

| Value | What happens when the job is done |
| --- | --- |
| `keep` | Nothing. The clip stays in the list. |
| `done` | The clip is marked done and leaves the list. |
| `recycle` | Also moves the recording and its playable copies to the recycle bin. The cut and everything that came out of it stay. |

Left out, the setting `cleanup.afterShare` decides (default `done`). An
unknown value is a `400`. The job carries what it was told in `after`, and a
failed job changes nothing.

`cleanup.recycleDoneAfterDays` does the same as `recycle` later on: the
recordings of clips that have been done for that many days go to the recycle
bin. `0` (the default) never does. Cut files are never removed automatically.

### `DELETE /api/clips/<base>[?scope=clip|all][&remote=1]`

`scope` says how far the delete reaches:

| `scope` | What goes |
| --- | --- |
| `all` (the default, and what 1.4 did) | The recording, its previews and thumbnail, every file in `shared\` that came from this clip, every cut file - and the clip itself, with its cuts. |
| `clip` | Only the recording and its playable copies. The clip stays in the list as `done` with `file: null`, its cuts stay renderable. A clip without cuts falls back to `all`. |

`remote=1` (`nextcloud=1` remains an alias) deletes the remote copies as
well and drops the clip's history entries; with `scope=clip` nothing remote
is touched, because no output is removed either.

Response: `{ ok: true, recycled, nextcloud, scope, cuts }` - `cuts` is how
many cuts were kept. `400` for another `scope`, `409` while a job of this
clip runs, `404` for an unknown clip.

### `DELETE /api/cuts/<id>[?remote=1]`

One cut with the outputs that came from it: the cut file, the finished files
of its jobs in `shared\`, their history entries and, with `remote=1`, the
remote copies. A clip that loses its last cut is `new` again.

`200 { ok: true, recycled, nextcloud, base }`, `404` for an unknown cut,
`409` while a job of this cut runs.

### `GET /api/history[?limit=&before=]`

Since 3.0 the store keeps every entry - there is no 200 cap any more.

- `limit`: how many entries at most (default 200, at most 5000).
- `before`: only entries whose `at` is older than this local timestamp. The
  `at` of the last entry of a page is what asks for the next one.

`history` in the status document stays at the 50 newest entries.

### Additions to the diagnostics

One more line: `cuts` says how many cut files there are, how much space they
take, how many cuts and outputs the store knows and how many cuts have lost
their file. It warns from 10 GB on.

### Where the state lives

Titles, the seen list and the history are rows in `<data-dir>\replaycut.db`
(SQLite) instead of three JSON files. The first start of 3.0 imports
`clip-names.json`, `clip-seen.json` and `clip-history.json` and moves them to
`<data-dir>\backup-2.x\`; every share of 2.x becomes a cut without a file, so
its outputs still hang under their clip. None of this is visible in the API.

## Since 3.3

### The YouTube client comes with the build

Until 3.2 every user had to create a Google OAuth client of their own
before YouTube worked at all. A release now carries one, compiled in from
the repository variables of the workflows, and the settings say which one
to use.

- `integrations.youtube.client` is `builtin` (default) or `own`; anything
  else is a 400. `own` keeps the behaviour of 2.6: the client is the
  write-only pair `youtubeClientId` / `youtubeClientSecret` in
  `PUT /api/settings` (credential `replaycut/youtube-client`).
- `integrations.youtube.clientType` (`tv` or `desktop`) chooses the flow for
  both, and the build carries a client of each type.
- `GET /api/oauth/youtube` gains `builtIn`: this build was compiled with a
  client for the chosen `clientType`. A fork, or a local `cargo build`
  without the variables, has none, so `builtIn` is false, `configured` is
  false while `client` is `builtin`, and `POST /api/oauth/youtube/start`
  answers 409 as it does without a stored client. Every provider document
  carries the field; for OneDrive it is the build's client id.
- A refresh token belongs to the client that issued it, so changing
  `client` or `clientType` disconnects the channel, exactly as storing a
  new client of one's own already did.
- The quota is per Google project, not per user: 1600 units per upload out
  of 10 000 a day, so the built-in client is good for about six uploads a
  day across everyone who uses it. `own` is the way out and stays.

### X is gone

The X target of 2.6 is removed. X's API has had no free tier since
6 February 2026: posting is billed per request, so a client shipped with
replaycut would put every user's posts on the maintainer's bill. No release
ever carried a client id, so no installation can have used the target.

- `x` is no longer in `config.targets`, `integrations.x` is no longer a
  settings group (a `PUT /api/settings` that names it is a 400, like any
  unknown integration) and `secrets.x` is gone.
- `GET /api/oauth/x` is a 404 like any unknown provider, and the
  diagnostics have no `x` row.
- `integrations.x` in an existing `settings.json` is ignored, as any unknown
  field is. A share whose target was `x` cannot exist, so the history and
  the delete dialog need no migration.

## Since 3.4

### A copy from a distribution package

On Linux an executable under `/usr` (`/usr/bin/replaycut`) was put there by
a package (AUR, deb, rpm), and the package manager updates it.

- `GET /api/update` gains `fromPackage`: true for such a copy. `installed`
  is false for it as before, so the page offers no "Update now"; the hint
  says the copy updates with the package manager, and the link goes to the
  release page. `POST /api/update/install` answers 409 with that reason.
  The daily check itself stays on: the banner is how the user learns that
  the package is behind.
- `replaycut install` and `replaycut uninstall` do nothing for such a copy
  and say so (`uninstall --purge` still removes this user's settings, state
  and credentials); `autostart on`, and the switch in Settings, enable the
  unit the package installed instead of writing a second one, and
  `autostart status` names the unit that is enabled.

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
   Since 3.0 a share first passes the stage `cut`: the range becomes
   `.cuts\<id>.mkv` with a stream copy (or the cut that is already there is
   used), and step 2 reads that file instead of the recording, seeking
   `start - actualStart` into it. Everything below is unchanged.
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
`libx264` are tried in that order (Linux adds `h264_vaapi` between NVENC and
Quick Sync) with a real two-frame encode; the first that works wins and is
reported as `config.encoder`.

### Delete

See `DELETE /api/clips/<base>`. Files go to the recycle bin, never
`unlink`. The title and, with `?nextcloud=1`, the history entries are removed.
Since 3.0 the delete has a `scope`, and a recording that disappears from the
folder on its own follows the same rule: with cuts the clip stays as `done`
with `file: null`, without them it is forgotten. A cut file that belongs to
no cut goes to the recycle bin on the next scan, and a cut whose file is gone
becomes `missing`.

### Limits

- One share job at a time; 30 jobs kept in memory; 200 history entries until
  2.8, since 3.0 all of them (`history` in the status stays at 50). Titles,
  seen list and history are persisted as JSON until 2.8 and in
  `replaycut.db` since 3.0.
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
