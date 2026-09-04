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
| Logs | `<data-dir>\logs\replaycut.<date>.log`, daily rotation, 7 files kept |
| Previews | `<clipDir>\.preview\` |
| Shared clips | `<clipDir>\shared\` |
| Credentials | Credential Manager, generic credentials `replaycut/nextcloud` and `replaycut/discord-webhook` |

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
      "expireDays": 0
    },
    "discord": {
      "enabled": false
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
| `hwaccel` | ffmpeg `-hwaccel` value for decoding, for example `d3d11va` or `cuda`. Empty means software decoding. |
| `ffmpegPriority` | Windows priority class of every ffmpeg process: `normal`, `belowNormal` (default) or `idle`. Keeps the game responsive while a clip is encoded. |
| `ffmpegThreads` | `-threads` for decoder and encoder. `0` (default) means half of the logical cores, at least 2. Set it to the core count and `ffmpegPriority` to `normal` for maximum speed when nothing else is running. |
| `logLevel` | `error`, `warn`, `info`, `debug` or `trace`. The `RUST_LOG` environment variable overrides it. |
| `integrations.nextcloud` | `enabled` switches the upload on. `url` is the server, `folder` the target folder (clips land in `<folder>/<YYYY-MM>/`), `expireDays` sets an expiry on the public link (`0` = never; an expired link also kills the Discord post). |
| `integrations.discord` | `enabled` switches the webhook post on. The webhook URL itself is a credential. |

An enabled integration without stored credentials is skipped with a warning
in the log; the service still starts.

## Credentials

| Target | User name | Secret |
|---|---|---|
| `replaycut/nextcloud` | Nextcloud user | App password (Nextcloud: Settings, Security, Devices & sessions) |
| `replaycut/discord-webhook` | `webhook` | The webhook URL |

`replaycut setup` writes them; `cmdkey /list` shows them; `cmdkey /delete:replaycut/nextcloud` removes one by hand.

## Command line

```
replaycut [OPTIONS] [COMMAND]

Commands:
  run    Run the service (default)
  setup  Configure display name, Nextcloud and Discord interactively
  test   Check the enabled integrations and their credentials

Options:
  --data-dir <DIR>     Data directory (settings, state, logs)
  --settings <FILE>    Settings file
  --clip-dir <DIR>     Override clipDir
  --port <PORT>        Override port
  --bind <ADDRESS>     Override bind
  --ui <FILE>          Override uiFile
  --log-level <LEVEL>  Override logLevel
  --dry-run            Encode for real, simulate uploads, posts, hotkey and clipboard
```

Options override the settings file for that run only; they are not written
back. `--dry-run` is what the API test suite uses: the pipeline runs every
stage, but the storage and notify integrations are replaced by simulations
that return links on `dry-run.invalid`, and neither the replay hotkey nor
the clipboard is touched.

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
