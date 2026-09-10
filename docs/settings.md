# Settings and command line

replaycut keeps its configuration in one JSON file and its secrets in the
platform's credential store: the Windows Credential Manager, or on Linux the
freedesktop Secret Service (gnome-keyring, KWallet, KeePassXC). Nothing
sensitive is ever written to disk in plain text.

## Where things live

| Item | Location |
|---|---|
| Data directory | `%LOCALAPPDATA%\replaycut` (override with `--data-dir`) |
| Settings | `<data-dir>\settings.json` (override with `--settings`) |
| State | `<data-dir>\replaycut.db` (clips, cuts and jobs; since 3.0) |
| State of 2.x | `<data-dir>\backup-2.x\clip-names.json`, `clip-seen.json`, `clip-history.json` - the first start of 3.0 imports these three files and moves them here |
| Browser sessions | `<data-dir>\sessions.json` (hashes of the login cookies, 30 days; since 2.8 with the device's name, browser, address, when it was made and last seen, and how it got in; since 3.4 also whether a browser or a client of its own holds it; since 3.5 the device id from the `rc_device` cookie, so one device is one row however it signs in) |
| TLS certificates | `<data-dir>\tls\` (since 3.4, only with `https.enabled` and no own PEM pair): `ca.crt` and `ca.key`, the authority this installation is known by, made once and never replaced; `server.crt` and `server.key`, re-issued whenever the machine's names or addresses change or less than 30 days are left. On Linux the private keys are `0600`; on Windows the folder is under `%LOCALAPPDATA%` and therefore already bound to your account, as `settings.json` and `sessions.json` are. |
| Themes | `<data-dir>\themes\<name>.css` (see `docs/themes.md`) |
| Logs | `<data-dir>\logs\replaycut.<date>.log`, daily rotation, 7 files kept |
| Previews | `<clipDir>\.preview\` |
| Cuts | `<clipDir>\.cuts\<id>.mkv` (since 3.0) |
| Shared clips | `<clipDir>\shared\` |
| Credentials | Credential Manager, generic credentials `replaycut/nextcloud`, `replaycut/discord-webhook`, `replaycut/obs-websocket`, `replaycut/onedrive`, `replaycut/s3`, `replaycut/webdav`, `replaycut/youtube`, `replaycut/youtube-client` |

On Linux the data directory is `$XDG_DATA_HOME/replaycut`
(`~/.local/share/replaycut`), the paths below it are the same with forward
slashes, and the credentials live in the keyring behind the Secret Service
(see [Credentials](#credentials)).

The settings file is created with defaults on the first start. Unknown
fields are ignored, missing fields take their defaults, so a partial file is
fine.

## settings.json

```json
{
  "clipDir": "C:\\Users\\you\\Videos",
  "port": 8420,
  "bind": "127.0.0.1",
  "allowedHosts": [],
  "requireLoginOnLoopback": false,
  "https": { "enabled": false, "cert": "", "key": "" },
  "uiFile": "ui/index.html",
  "displayName": "replaycut",
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
      "client": "builtin",
      "clientType": "tv",
      "privacy": "unlisted",
      "description": "{title}\n\nClip from {date}, shared with replaycut."
    },
    "telegram": {
      "enabled": false,
      "autoPost": true,
      "chatId": ""
    },
    "webhook": {
      "enabled": false,
      "autoPost": true,
      "url": ""
    }
  }
}
```

| Field | Meaning |
|---|---|
| `clipDir` | Folder OBS writes replays to. Scanned for `*.mkv`, not recursively. Default: `Videos` in the user profile. |
| `port` | HTTP port of the UI and API. |
| `bind` | Address to listen on. Since 2.8 the default is `127.0.0.1`: this PC only. `0.0.0.0` makes the UI reachable from other devices in the network - the switch under Settings › Access sets it, together with the password and the firewall rule (see "Security" in the README). Any other address is left alone by that switch and shown as "custom". |
| `allowedHosts` | Since 2.8: host names this service answers to besides `localhost`, its own computer name and any IP address - for an own DNS name or a reverse proxy. Up to 20 names without scheme or port; everything else gets a `421`. Default empty. |
| `requireLoginOnLoopback` | Since 2.8: `true` asks for the password on this PC as well, for a Windows account other people use. It applies to answering sign-in requests too. Default `false`. |
| `https.enabled` | Since 3.4: the port speaks TLS. Default `false`; the switch is under Settings › Access and takes effect after a restart, like `port` and `bind`. With `cert` and `key` empty, replaycut is its own certificate authority in `<data-dir>/tls` - see "Security" in the README for what that costs you in browser warnings. |
| `https.cert`, `https.key` | Since 3.4: paths to your own PEM pair (certificate chain, leaf first; and its private key). Both or neither - one alone is a `400`. A pair every device already trusts, from Tailscale, a reverse proxy or a domain of yours, is the comfortable way to run HTTPS. If the files cannot be read at startup the service runs on plain HTTP and says so in the log and in the diagnostics. |
| `uiFile` | The UI file. A relative path is looked up next to the executable first, then in the working directory; since 3.4 also under `<prefix>/share/replaycut` for an executable in `<prefix>/bin` and, on Linux, in every `$XDG_DATA_DIRS` entry (`/usr/share/replaycut/ui/index.html` for a distribution package). |
| `displayName` | Prefix of the Discord post (`**<displayName>** ...`) and the webhook user name. Clip names that start with this word are shortened in the post. |
| `encoder` | `auto` tries `h264_amf`, `h264_nvenc`, `h264_qsv`, `libx264` in that order (on Linux `h264_vaapi` comes between NVENC and Quick Sync) with a real test encode and uses the first that works. An encoder name forces that encoder. |
| `hwaccel` | `auto` (or empty, the default since 2.4): the encoder profile decides, GPU decoding where the test encode proves it works; `none`: software decoding; `cuda`, `d3d11va`, `qsv` or `vaapi`: passed to ffmpeg as `-hwaccel` with CPU scaling. |
| `ffmpegPriority` | Priority of every ffmpeg process: `normal`, `belowNormal` (default) or `idle`. Keeps the game responsive while a clip is encoded. On Windows the priority class of the process, on Linux its nice level (0, 10 or 19). |
| `ffmpegThreads` | `-threads` for decoder and encoder. `0` (default) means half of the logical cores, at least 2. Set it to the core count and `ffmpegPriority` to `normal` for maximum speed when nothing else is running. |
| `logLevel` | `error`, `warn`, `info`, `debug` or `trace`. The `RUST_LOG` environment variable overrides it. |
| `checkUpdates` | `true` asks GitHub once a day (a minute after start, then every 24 h) whether a newer release exists and shows a banner in the UI; nothing is downloaded. Set to `false` if the service must not contact GitHub. |
| `setupDone` | `false` until the browser setup (`/setup`) finished. A file without the field counts as set up, so an installation from 2.0 is not asked again. |
| `theme` | Name of the UI theme: `wardogs` (built in) or a file `themes\<name>.css` in the data directory. See `docs/themes.md`. |
| `previewH264` | Since 2.6: `onDemand` (default) shows "Make a playable preview" in the player when the browser cannot decode the recording (AV1 on an iPhone, say) and makes a 720p H.264 copy on click; `always` makes the copy right after every recording, behind the running jobs, with ffmpeg at idle priority (about a minute of GPU per 5-minute buffer). The copy lives next to the preview as `<base>.h264.mp4`. Recording in H.264 in OBS avoids the need. |
| `passwordHash` | Set through the settings page or `PUT /api/settings` with `password`; an argon2id hash, never the password. Since 2.8 a password is 8 to 128 characters, and "Generate one for me" offers four words from a built-in list. Absent means no password - which is why network access cannot be turned on without setting one first. This PC (loopback) never needs the password unless `requireLoginOnLoopback` is set. |
| `obs` | Since 2.2: `{ "enabled": true, "host": "localhost", "port": 4455 }` - where obs-websocket listens (OBS: Tools › WebSocket Server Settings). With `enabled` the service connects on its own and retries quietly while OBS is closed. The password is a credential, see below. |
| `cleanup` | Since 3.0: `{ "afterShare": "done", "recycleDoneAfterDays": 0 }` - what happens to a clip once it has been shared. `afterShare` is the default of the "Afterwards" menu in the share row: `keep` leaves the clip in the list, `done` (the default) takes it out (one click brings it back), `recycle` also moves the recording to the recycle bin. The share may say otherwise per job (`after` in `POST /api/share`). `recycleDoneAfterDays` moves the recordings of clips that have been done that long to the recycle bin; `0` never does. **Cuts and shared files are never removed automatically** - the cut is what every later rendering is made from. |

Since 2.1 the settings page and `PUT /api/settings` change the file at
runtime; everything but `port`, `bind` and `uiFile` takes effect without a
restart. The network switch of 2.8 changes `bind` and restarts the service
for you. Command-line overrides win over the file for as long as the
process runs but are never written back.
| `integrations.nextcloud` | `enabled` switches the upload on. `url` is the server, `folder` the target folder (clips land in `<folder>/<YYYY-MM>/`), `expireDays` sets an expiry on the public link (`0` = never; an expired link also kills the Discord post). `quickShare` (default true, since 2.5) makes it the target of the Share button; off keeps the button local and leaves Nextcloud in the button's menu. |
| `integrations.discord` | `enabled` switches the webhook post on. The webhook URL itself is a credential. `autoPost` (default true, since 2.5) posts every share that produced a link; since 2.7 only the quick share, the others on request ("Post to ..."). |
| `integrations.<storage>.maxHeight`, `maxKbps` | Since 2.7, on every storage block (`nextcloud`, `onedrive`, `s3`, `webdav`, `youtube`): `0` (default) keeps the recording's resolution and encodes in the encoder's quality mode; a height (240-4320) scales the share down to it, a bitrate (500-200000 kbit/s) caps it at constant bitrate. Limits belong to the target, so only shares to that target shrink. Discord shows its inline player only for a small file: 1080p at 6000-8000 kbit/s keeps a one-minute clip under the size its proxy accepts, and above that the post is a bare link. The global `shareKbps` of 2.0-2.6 is gone. |
| `integrations.onedrive` | `enabled` switches the OneDrive upload on (since 2.5); `quickShare` makes it the Share button's target. The account is connected under Settings › Integrations with a code at Microsoft; the refresh token is the credential `replaycut/onedrive`. Uploads land in `Apps/replaycut/<YYYY-MM>/`. |
| `integrations.s3` | S3-compatible storage (since 2.5): `endpoint` (`https://<account>.r2.cloudflarestorage.com`, `https://s3.<region>.amazonaws.com`, `http://minio:9000`), `region` (`auto` for R2), `bucket`, `prefix` (folder inside the bucket), `publicBase` (public URL serving the keys; empty = presigned links), `presignDays` (1-7). Keys are the credential `replaycut/s3`. |
| `integrations.webdav` | Generic WebDAV (since 2.5): `url` (the DAV root), `folder` below it, `publicBase` (public URL that serves the folder; required, the link is `<publicBase>/<month>/<file>`). Login is the credential `replaycut/webdav`. |
| `integrations.youtube` | YouTube (since 2.6): every share is uploaded as its own video. `privacy` is `unlisted` (default), `private` or `public`; `description` is a template with `{title}`, `{clip}` and `{date}`. The video title is the clip's title (or its name), plus `#Shorts` for a vertical cut. `client` (since 3.3) is `builtin` (default: the Google client this release was built with, nothing to set up) or `own` (a client from your own Google project, credential `replaycut/youtube-client`, see [`docs/youtube.md`](youtube.md)); either way the channel is the credential `replaycut/youtube`. `clientType` is `tv` (a "TVs and Limited Input devices" client, connected with a code from any device, the default) or `desktop` (a "Desktop app" client, connected in the browser on this PC). Changing `client` or `clientType` disconnects the channel, because a refresh token belongs to the client that issued it. |
| `integrations.telegram` | Telegram bot (since 2.6): `chatId` is the chat, group or channel (`-1001234567890` or `@channelname`), the bot token the credential `replaycut/telegram`. `autoPost` (default true) posts every share that produced a link. |
| `integrations.webhook` | Generic webhook (since 2.6): `url` receives a JSON `POST` per share (`{ event: "shared", title, clip, seconds, target, link, direct, at, job, displayName }`); with a secret (credential `replaycut/webhook-secret`) the header `X-Replaycut-Signature: sha256=<HMAC>` is added. `autoPost` (default true). |

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
| `replaycut/telegram` | `bot` | The bot token from @BotFather (since 2.6) |
| `replaycut/webhook-secret` | `secret` | The HMAC secret of the generic webhook (since 2.6, optional) |

`replaycut setup` writes them. On Windows `cmdkey /list` shows them and
`cmdkey /delete:replaycut/nextcloud` removes one by hand. On Linux each is an
item in the default keyring with the attributes `application=replaycut` and
`target=<target>` (the user name as `user`): `secret-tool lookup application
replaycut target replaycut/nextcloud` shows one, `secret-tool clear` with the
same attributes removes it. A keyring daemon has to run in the session;
without one the settings page reports that the secret cannot be stored.

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
  autostart  on | off | status: start replaycut at sign-in (Windows) or with the desktop session (Linux)

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
| `vaapi-full/<node>` (Linux) | `-hwaccel vaapi -hwaccel_output_format vaapi` | `scale_vaapi` | `h264_vaapi` |
| `vaapi/<node>` (Linux) | software, `format=nv12,hwupload` before the encoder | CPU | `h264_vaapi` |
| `qsv-full` | `-hwaccel qsv -hwaccel_output_format qsv` | `scale_qsv` | `h264_qsv` |
| `qsv` | software | CPU | `h264_qsv` |
| `libx264` | software | CPU | `libx264 -preset veryfast` |

On Linux the VAAPI pair exists once per render node (`/dev/dri/renderD128`,
`renderD129`, ...), tried in that order and between the NVIDIA and the Intel
profiles: VAAPI is what AMD and Intel GPUs offer there, and a machine with
two GPUs (a processor's own and a card) has two nodes.

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
  want it. On Windows this is the process's priority class, on Linux its
  nice level (`belowNormal` = 10, `idle` = 19), set when the process starts.
- `ffmpegThreads` caps how many cores ffmpeg uses. Software AV1 decoding
  (dav1d) otherwise spreads across every core at full load, which is what
  makes a game stutter during a share.

Hardware encoders (`h264_amf`, `h264_nvenc`, `h264_qsv`, `h264_vaapi`) do
the encoding on the GPU; the thread cap then mostly limits decoding and
scaling.

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
(`http://<computer name>:<port>/` for a phone or laptop in the same network;
on Linux `<computer name>.local` when the machine announces itself over
mDNS, else its IPv4 address, because a bare Linux host name does not
resolve from other devices),
**Show QR code** (the settings page with the address dialog open), **Pause
scanning** (new replays wait in the folder until unticked; forgotten at the
next start), **Check for updates** (asks GitHub now and answers with a
notification), **Open log folder** and **Quit**. Its tooltip shows the
number of clips, the progress of the running share, "paused" or "update
available"; the icon carries a badge while a share runs and a red badge
after a failed one.

`replaycut stop` asks the running service to shut down (through a named
event on Windows and SIGTERM to the process behind the lock file on Linux,
not through HTTP, so a web page cannot stop the service) and waits for it
to exit. Only one instance runs at a time; a second start opens the browser
and exits.

Desktop notifications ("Clip saved", "Clip shared, link copied", "Share
failed") need the application registered with Windows, which
`replaycut install` does; on Linux they go to the session's notification
service. Without either they are skipped and a hint is logged once.
`--dry-run` only logs them.

The log records why the service stopped (Ctrl+C, the console closing, the
stop event, Quit in the tray menu, sign-out) and any panic with a backtrace.

On Linux the tray icon is a StatusNotifierItem on the session bus with the
same menu; it appears wherever a tray host runs (KDE, Waybar's `tray`
module, GNOME with the AppIndicator extension) and the service runs without
it otherwise. A start without a terminal (the desktop entry, the systemd
unit) opens the browser like the Windows shortcut unless `--no-browser` is
given, and the lock file `$XDG_RUNTIME_DIR/replaycut-<port>.lock` is what
keeps a second instance out.

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

On Linux `replaycut install` (what `install.sh` runs) lays the same
installation out with the XDG base directories, without root and without a
firewall step:

| Item | Where |
|---|---|
| Program files | `$XDG_DATA_HOME/replaycut/app/` (`replaycut`, `ui/index.html`, docs) |
| Settings, state, logs | `$XDG_DATA_HOME/replaycut/` |
| Command line | `~/.local/bin/replaycut`, a link to the installed executable |
| App menu | `$XDG_DATA_HOME/applications/replaycut.desktop` and the icon `$XDG_DATA_HOME/icons/hicolor/scalable/apps/replaycut.svg` |
| Autostart (optional) | the systemd user unit `$XDG_CONFIG_HOME/systemd/user/replaycut.service`, enabled for `graphical-session.target`, `ExecStart=<app>/replaycut --no-browser` |

The unit is part of the graphical session, so it starts once the desktop is
up (the clipboard and the notifications need the session's environment) and
stops with it. Desktops that run under systemd (GNOME, KDE, sway and
Hyprland with uwsm) reach `graphical-session.target` on their own; a
compositor started otherwise has to start the unit itself (Hyprland:
`exec-once = systemctl --user start replaycut`).

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

On Linux the same happens with `replaycut-<version>-linux-x64.zip`,
`replaycut` and `replaycut.old`; the ZIP's file modes are kept and the
executable gets its bit either way. Both ZIPs share one `SHA256SUMS`, so one
signature covers both platforms. The updater only installs into a copy that
`install.sh` put in place; a build run from elsewhere is updated by hand, and
a copy a distribution package put under `/usr` (`fromPackage` in
`GET /api/update`) is updated by the package manager - the banner says so,
and `replaycut install`, `uninstall` and `autostart` know the package's files
are not theirs (since 3.4).

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
