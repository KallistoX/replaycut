# Settings and command line

replaycut keeps its configuration in one JSON file and its secrets in the
Windows Credential Manager. Nothing sensitive is ever written to disk in
plain text.

## Where things live

| Item | Location |
|---|---|
| Data directory | `%LOCALAPPDATA%\replaycut` (override with `--data-dir`) |
| Settings | `<data-dir>\settings.json` (override with `--settings`) |
| State files | `<data-dir>\clip-names.json`, `clip-seen.json`, `clip-history.json` |
| Browser sessions | `<data-dir>\sessions.json` (hashes of the login cookies, 30 days) |
| Themes | `<data-dir>\themes\<name>.css` (see `docs/themes.md`) |
| Logs | `<data-dir>\logs\replaycut.<date>.log`, daily rotation, 7 files kept |
| Previews | `<clipDir>\.preview\` |
| Shared clips | `<clipDir>\shared\` |
| Credentials | Credential Manager, generic credentials `replaycut/nextcloud`, `replaycut/discord-webhook`, `replaycut/obs-websocket`, `replaycut/onedrive`, `replaycut/s3`, `replaycut/webdav`, `replaycut/youtube`, `replaycut/youtube-client` |

The settings file is created with defaults on the first start. Unknown
fields are ignored, missing fields take their defaults, so a partial file is
fine.

## settings.json

```json
{
  "clipDir": "C:\\Users\\you\\Videos",
  "port": 8420,
  "bind": "0.0.0.0",
  "uiFile": "ui/index.html",
  "displayName": "replaycut",
  "shareKbps": 6000,
  "encoder": "auto",
  "hwaccel": "",
  "ffmpegPriority": "belowNormal",
  "ffmpegThreads": 0,
  "logLevel": "info",
  "integrations": {
    "nextcloud": {
      "enabled": false,
      "url": "https://cloud.example.com",
      "folder": "Clips",
      "expireDays": 0,
      "quickShare": true
    },
    "discord": {
      "enabled": false,
      "autoPost": true
    },
    "onedrive": {
      "enabled": false,
      "quickShare": false
    },
    "youtube": {
      "enabled": false,
      "quickShare": false,
      "privacy": "unlisted",
      "description": "{title}\n\nClip from {date}, shared with replaycut."
    }
  }
}
```

| Field | Meaning |
|---|---|
| `clipDir` | Folder OBS writes replays to. Scanned for `*.mkv`, not recursively. Default: `Videos` in the user profile. |
| `port` | HTTP port of the UI and API. |
| `bind` | Address to listen on. `0.0.0.0` makes the UI reachable from other devices in the network, `127.0.0.1` restricts it to this PC. |
| `uiFile` | The UI file. A relative path is looked up next to the executable first, then in the working directory. |
| `displayName` | Prefix of the Discord post (`**<displayName>** ...`) and the webhook user name. Clip names that start with this word are shortened in the post. |
| `shareKbps` | Video bitrate of the shared H.264 file in kbit/s, constant bit rate. |
| `encoder` | `auto` tries `h264_amf`, `h264_nvenc`, `h264_qsv`, `libx264` in that order with a real test encode and uses the first that works. An encoder name forces that encoder. |
| `hwaccel` | `auto` (or empty, the default since 2.4): the encoder profile decides, GPU decoding where the test encode proves it works; `none`: software decoding; `cuda`, `d3d11va` or `qsv`: passed to ffmpeg as `-hwaccel` with CPU scaling. |
| `ffmpegPriority` | Windows priority class of every ffmpeg process: `normal`, `belowNormal` (default) or `idle`. Keeps the game responsive while a clip is encoded. |
| `ffmpegThreads` | `-threads` for decoder and encoder. `0` (default) means half of the logical cores, at least 2. Set it to the core count and `ffmpegPriority` to `normal` for maximum speed when nothing else is running. |
| `logLevel` | `error`, `warn`, `info`, `debug` or `trace`. The `RUST_LOG` environment variable overrides it. |
| `checkUpdates` | `true` asks GitHub once a day (a minute after start, then every 24 h) whether a newer release exists and shows a banner in the UI; nothing is downloaded. Set to `false` if the service must not contact GitHub. |
| `setupDone` | `false` until the browser setup (`/setup`) finished. A file without the field counts as set up, so an installation from 2.0 is not asked again. |
| `theme` | Name of the UI theme: `wardogs` (built in) or a file `themes\<name>.css` in the data directory. See `docs/themes.md`. |
| `passwordHash` | Set through the settings page or `PUT /api/settings` with `password`; an argon2id hash, never the password. Absent means no password: every device in the network may use the UI. This PC (loopback) never needs the password. |
| `obs` | Since 2.2: `{ "enabled": true, "host": "localhost", "port": 4455 }` - where obs-websocket listens (OBS: Tools › WebSocket Server Settings). With `enabled` the service connects on its own and retries quietly while OBS is closed. The password is a credential, see below. |

Since 2.1 the settings page and `PUT /api/settings` change the file at
runtime; everything but `port`, `bind` and `uiFile` takes effect without a
restart. Command-line overrides win over the file for as long as the
process runs but are never written back.
| `integrations.nextcloud` | `enabled` switches the upload on. `url` is the server, `folder` the target folder (clips land in `<folder>/<YYYY-MM>/`), `expireDays` sets an expiry on the public link (`0` = never; an expired link also kills the Discord post). `quickShare` (default true, since 2.5) makes it the target of the Share button; off keeps the button local and leaves Nextcloud in the button's menu. |
| `integrations.discord` | `enabled` switches the webhook post on. The webhook URL itself is a credential. `autoPost` (default true, since 2.5) posts every share that produced a link. |
| `integrations.onedrive` | `enabled` switches the OneDrive upload on (since 2.5); `quickShare` makes it the Share button's target. The account is connected under Settings › Integrations with a code at Microsoft; the refresh token is the credential `replaycut/onedrive`. Uploads land in `Apps/replaycut/<YYYY-MM>/`. |
| `integrations.s3` | S3-compatible storage (since 2.5): `endpoint` (`https://<account>.r2.cloudflarestorage.com`, `https://s3.<region>.amazonaws.com`, `http://minio:9000`), `region` (`auto` for R2), `bucket`, `prefix` (folder inside the bucket), `publicBase` (public URL serving the keys; empty = presigned links), `presignDays` (1-7). Keys are the credential `replaycut/s3`. |
| `integrations.webdav` | Generic WebDAV (since 2.5): `url` (the DAV root), `folder` below it, `publicBase` (public URL that serves the folder; required, the link is `<publicBase>/<month>/<file>`). Login is the credential `replaycut/webdav`. |
| `integrations.youtube` | YouTube (since 2.6): every share is uploaded as its own video. `privacy` is `unlisted` (default), `private` or `public`; `description` is a template with `{title}`, `{clip}` and `{date}`. The video title is the clip's title (or its name), plus `#Shorts` for a vertical cut. Needs your own Google client (credential `replaycut/youtube-client`, see [`docs/youtube.md`](youtube.md)) and a connected channel (credential `replaycut/youtube`, connected with a code at Google under Settings › Integrations). |

An enabled integration without stored credentials is skipped with a warning
in the log; the service still starts.

## Credentials

| Target | User name | Secret |
|---|---|---|
| `replaycut/nextcloud` | Nextcloud user | App password (Nextcloud: Settings, Security, Devices & sessions) |
| `replaycut/discord-webhook` | `webhook` | The webhook URL |
| `replaycut/obs-websocket` | `obs-websocket` | The obs-websocket server password (OBS: Tools › WebSocket Server Settings › Show Connect Info); since 2.2, written by the OBS page or `PUT /api/settings` with `obsPassword` |
| `replaycut/onedrive` | Account name | The OAuth refresh token (since 2.5); written by the Connect flow, removed by Disconnect |
| `replaycut/s3` | Access key ID | Secret access key (since 2.5) |
| `replaycut/webdav` | DAV user | DAV password (since 2.5) |
| `replaycut/youtube-client` | Google client ID | Client secret of your own Google project (since 2.6, see [`docs/youtube.md`](youtube.md)) |
| `replaycut/youtube` | Channel title | The OAuth refresh token of the connected channel (since 2.6) |

`replaycut setup` writes them; `cmdkey /list` shows them; `cmdkey /delete:replaycut/nextcloud` removes one by hand.

## Command line

```
replaycut [OPTIONS] [COMMAND]

Commands:
  run        Run the service (default)
  setup      Configure display name, Nextcloud and Discord interactively
  test       Check the enabled integrations and their credentials
  stop       Stop the running service
  install    Install or update replaycut for this user
  uninstall  Remove the installation (--purge also removes settings and credentials)
  autostart  on | off | status: start replaycut at sign-in

Options:
  --data-dir <DIR>     Data directory (settings, state, logs)
  --settings <FILE>    Settings file
  --clip-dir <DIR>     Override clipDir
  --port <PORT>        Override port
  --bind <ADDRESS>     Override bind
  --ui <FILE>          Override uiFile
  --log-level <LEVEL>  Override logLevel
  --dry-run            Encode for real, simulate uploads, posts, hotkey, clipboard and toasts
  --no-browser         Do not open the browser when the service starts
```

Options override the settings file for that run only; they are not written
back. `--dry-run` is what the API test suite uses: the pipeline runs every
stage, but the storage and notify integrations are replaced by simulations
that return links on `dry-run.invalid`, and neither the replay hotkey nor
the clipboard is touched.

## Encoder profiles and `replaycut bench`

Since 2.4 the encoder detection tries, per GPU vendor, the full GPU path
first and the same encoder with software decoding second, each with a real
two-second encode of the newest preview (without a clip only the software
profiles are tried; the next settings change or restart with a clip picks the
GPU path up). A share whose GPU path fails at run time is retried once with
software decoding; the diagnostics count such fallbacks.

| Profile | Decode | Scale | Encode |
|---|---|---|---|
| `amf-d3d11` | `-hwaccel d3d11va` (frames come back to RAM) | CPU | `h264_amf` |
| `amf` | software | CPU | `h264_amf` |
| `nvenc-cuda` | `-hwaccel cuda -hwaccel_output_format cuda` | `scale_cuda` | `h264_nvenc` |
| `nvenc` | software | CPU | `h264_nvenc` |
| `qsv-full` | `-hwaccel qsv -hwaccel_output_format qsv` | `scale_qsv` | `h264_qsv` |
| `qsv` | software | CPU | `h264_qsv` |
| `libx264` | software | CPU | `libx264 -preset veryfast` |

`replaycut bench [--seconds N]` encodes N seconds (default 30) of the newest
clip with every profile the ffmpeg build knows and prints wall time, CPU
time, speed and size. Measured so far (AV1 2560x1440 @ 60 fps from OBS,
ffmpeg 9.0.1, 30 s):

| GPU | Profile | Wall | CPU | Speed |
|---|---|---|---|---|
| AMD Radeon (RDNA) | `amf-d3d11` | 13.0 s | 13.5 s | 2.3x real time |
| AMD Radeon (RDNA) | `amf` | 21.2 s | 42.6 s | 1.4x |
| AMD Radeon (RDNA) | `libx264` | 22.3 s | 90.2 s | 1.3x |
| NVIDIA | `nvenc-cuda` | pending | | |
| Intel | `qsv-full` | pending | | |

The AV1 decode is the CPU cost; the GPU decoder takes it away. Send your
table to the maintainer when your vendor is still pending.

## Resource limits while sharing

Encoding runs on the gaming PC, next to the game. Two settings keep it
polite:

- `ffmpegPriority: belowNormal` hands the CPU to the game whenever both
  want it. This is a Windows priority class, set when the process starts.
- `ffmpegThreads` caps how many cores ffmpeg uses. Software AV1 decoding
  (dav1d) otherwise spreads across every core at full load, which is what
  makes a game stutter during a share.

Hardware encoders (`h264_amf`, `h264_nvenc`, `h264_qsv`) do the encoding on
the GPU; the thread cap then mostly limits decoding and scaling.

## Starting and stopping

The executable has no console window. Started by double-click or from a
shortcut it runs silently with a tray icon and opens the UI in the browser;
if the service is already running, the double-click only opens the browser.
Started from a terminal it attaches to that terminal, so `--help`, `setup`,
`test`, `stop` and the log lines appear in that window. At an interactive
`cmd.exe` or PowerShell prompt the shell does not wait for a windowless
program; the output still appears, but `setup`, which asks questions, needs
the shell to wait: use `start /wait replaycut setup` in `cmd.exe` and
`Start-Process -Wait -NoNewWindow replaycut setup` in PowerShell. Batch
files wait on their own.

The tray icon offers **Open** (the UI in the browser), **Copy address**
(`http://<computer name>:<port>/` for a phone or laptop in the same network),
**Show QR code** (the settings page with the address dialog open), **Pause
scanning** (new replays wait in the folder until unticked; forgotten at the
next start), **Check for updates** (asks GitHub now and answers with a
notification), **Open log folder** and **Quit**. Its tooltip shows the
number of clips, the progress of the running share, "paused" or "update
available"; the icon carries a badge while a share runs and a red badge
after a failed one.

`replaycut stop` asks the running service to shut down (through a named
event, not through HTTP, so a web page cannot stop the service) and waits
for it to exit. Only one instance runs at a time; a second start opens the
browser and exits.

Desktop notifications ("Clip saved", "Clip shared, link copied", "Share
failed") need the application registered with Windows, which
`replaycut install` does; without it they are skipped and a hint is logged
once. `--dry-run` only logs them.

The log records why the service stopped (Ctrl+C, the console closing, the
stop event, Quit in the tray menu, sign-out) and any panic with a backtrace.

## Installation layout

`replaycut install` (what `install.cmd` runs) is idempotent and needs no
admin rights except for the optional firewall rule:

| Item | Where |
|---|---|
| Program files | `%LOCALAPPDATA%\replaycut\app\` (`replaycut.exe`, `ui\index.html`, `replaycut.ico`, docs) |
| Settings, state, logs | `%LOCALAPPDATA%\replaycut\` |
| Shortcuts | Start menu and desktop, `replaycut.lnk`, carrying the AppUserModelID `replaycut` |
| Notification registration | `HKCU\Software\Classes\AppUserModelId\replaycut` |
| Autostart (optional) | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\replaycut` = `"<app>\replaycut.exe" --no-browser` |
| Firewall rule (optional) | `replaycut`, inbound TCP on the configured port, private profile, bound to the executable |

Migration from the 1.x service happens inside `install` when the scheduled
task `WARDOGS Clip-Service` or its state files exist: the task's arguments
become `settings.json` (only when none exists yet), `clip-*.json` and the
credentials `wardogs/*` are copied where ours are missing, the task is
stopped and removed, autostart is switched on, and the old firewall rule and
URL reservation are removed in the elevated step.

## One-click update

Since 2.3 the service can update itself: it downloads the release ZIP and
`SHA256SUMS` from GitHub, checks the minisign signature
(`SHA256SUMS.minisig`) against the public key built into the running
executable, compares the hash, unpacks into the update folder, runs the new
executable with `--version`, then moves the running `replaycut.exe` aside
as `replaycut.old.exe`, copies the package into `app\` and restarts. The
first start after that removes `replaycut.old.exe` and the update folder.
Settings, state and credentials are not touched. When anything fails before
the copy, nothing has changed; when the new executable does not start, the
previous one is still there as `replaycut.old.exe`.

The public key is `dist/minisign.pub` in the repository (minisign key
`48259F89A10BFB0C`). To check a download by hand:

```
minisign -Vm SHA256SUMS -p minisign.pub
sha256sum -c SHA256SUMS
```

Two environment variables exist for testing the flow against a fake release
served locally and are not meant for normal use: `REPLAYCUT_RELEASES_URL`
replaces the GitHub releases URL, `REPLAYCUT_UPDATE_PUBKEY` replaces the
built-in signing key (base64, as `minisign -G` prints it). For the OneDrive
flow, `REPLAYCUT_ONEDRIVE_CLIENT_ID` replaces the built-in client id and
`REPLAYCUT_MS_LOGIN_BASE` / `REPLAYCUT_GRAPH_BASE` point at a fake Microsoft.
