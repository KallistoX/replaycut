# The winget package

The three manifests in `manifests/` are the `KallistoX.replaycut` package for
the [Windows Package Manager](https://github.com/microsoft/winget-pkgs). They
live here so the version, the download URL and the checksum are updated in the
same repository that produces them; the pull request that carries them into
`microsoft/winget-pkgs` is filed by hand (see *Submitting* below).

Until that pull request is merged, `winget install replaycut` finds nothing.
The ZIP from the [releases page](https://github.com/KallistoX/replaycut/releases/latest)
stays the way to install replaycut.

## What the package does

`winget install KallistoX.replaycut` downloads the Windows release ZIP,
unpacks it to `%LOCALAPPDATA%\Microsoft\WinGet\Packages\…` and puts that
folder on the user's `PATH`, so `replaycut` works in any new terminal.
`winget uninstall KallistoX.replaycut` removes both again.

That is a *portable* install, and it is deliberately not the whole story:

- `replaycut` starts the service and opens the page - enough to try it.
- `replaycut install` is still what makes it an installation: it copies the
  files to `%LOCALAPPDATA%\replaycut\app`, adds the start menu and desktop
  shortcuts, registers the toast notifications, asks about autostart and puts
  the built-in one-click updater in charge of that copy.

**Why not `install.cmd` as a nested installer.** winget runs an installer
unattended. `install.cmd` asks whether replaycut should start at sign-in and
waits for a key at the end, so it would hang forever behind winget's progress
bar. A nested-installer package needs `replaycut install` to grow a
non-interactive mode first (something like `replaycut install --yes
--autostart off`, answering every question from the command line and skipping
the `pause`); with that in place the installer manifest becomes
`NestedInstallerType: portable` → `nested` with `NestedInstallerFiles:
install.cmd`, and the package would give the full installation in one step.
Worth doing, but it is a change to the installer, not to the manifest.

**Why `ArchiveBinariesDependOnPath: true`.** `replaycut.exe` reads its UI from
`ui\index.html` next to itself. The flag tells winget to put the unpacked
folder on the `PATH` instead of linking the executable into its own `Links`
folder, where a resolved symlink could leave the UI behind.

**Why the ffmpeg dependency.** replaycut encodes with `ffmpeg` and reads clips
with `ffprobe`; without them on the `PATH` it can list clips and nothing else.
`Gyan.FFmpeg` is the package the README already points at. If a winget-pkgs
reviewer objects to the dependency, drop the `Dependencies` block and say so
in the description instead - nothing else in the manifest depends on it.

## Updating it for a new release

1. `PackageVersion` in all three files, `ReleaseDate` and `ReleaseNotesUrl`.
2. `InstallerUrl`: the ZIP of the new tag.
3. `InstallerSha256`: the line for `replaycut-<version>-windows-x64.zip` in
   that release's `SHA256SUMS`, in upper case.

```powershell
gh release download v<version> -p SHA256SUMS -D .
```

`wingetcreate update KallistoX.replaycut --version <version> --urls <zip-url>`
does the same from the published release and can submit the pull request in
one step; it needs a GitHub token with `public_repo` on the fork.

## Checking it before submitting

```powershell
winget validate --manifest dist\winget\manifests
```

That is the check this repository can run on its own, and it passes. Trying
the install locally needs one machine-wide setting, because winget refuses
manifests from disk otherwise. It asks for administrator rights:

```powershell
winget settings --enable LocalManifestFiles
winget install --manifest dist\winget\manifests --skip-dependencies
replaycut --version
winget uninstall --id "ARP\User\X64\KallistoX.replaycut__DefaultSource"
winget settings --disable LocalManifestFiles
```

`--skip-dependencies` keeps the test from installing `Gyan.FFmpeg` over the
ffmpeg you already have. The install writes to
`%LOCALAPPDATA%\Microsoft\WinGet\`, never to `%LOCALAPPDATA%\replaycut`, so
the installation on port 8420 is not touched - but do not start the portable
`replaycut` without `--port` and `--data-dir`, or it will fight the running
one for the port.

What the manifests describe was checked without winget as well: the release
ZIP of 3.6.0 carries `replaycut.exe` and `ui\index.html` at its root (so
`RelativeFilePath: replaycut.exe` is right), its SHA256 matches
`InstallerSha256`, and the executable unpacked from it serves the page from
the folder it was unpacked into.

### What the install actually did

Run on Windows 11 with winget 1.29.290, against 3.6.0:

- The archive was downloaded from GitHub, the hash verified, and everything
  unpacked to
  `%LOCALAPPDATA%\Microsoft\WinGet\Packages\KallistoX.replaycut__DefaultSource`
  - the executable with `ui\index.html` beside it, as the manifest assumes.
- That folder went onto the user `PATH`; no symlink was made. Note that this
  is what happens **with or without** `ArchiveBinariesDependOnPath`: winget
  falls back to the `PATH` when it cannot create a symlink, and creating one
  needs Developer Mode or administrator rights. The flag is what makes the
  outcome the same on a machine that *could* make the symlink, instead of
  leaving the UI behind a link.
- `replaycut --version` answered `replaycut 3.6.0`, and the copy from that
  folder served the page (HTTP 200, the full UI) on a scratch port.
- `winget uninstall --id "ARP\User\X64\KallistoX.replaycut__DefaultSource"`
  removed the folder and the registration. Note the id: a package installed
  from a local manifest is not found under `KallistoX.replaycut`.
- **It left the `PATH` entry behind**, pointing at the folder it had just
  deleted. Also with and without the flag, so it is winget's behaviour on a
  machine without symlinks, not something the manifest asks for. Harmless -
  a missing directory on the `PATH` is ignored - but worth knowing, and worth
  cleaning up by hand after a test install.
- `%LOCALAPPDATA%\replaycut` was not touched at any point.

## Submitting

The pull request goes to `microsoft/winget-pkgs` from a personal account, with
the three files under
`manifests/k/KallistoX/replaycut/<version>/`. Either

- `wingetcreate submit --token <pat> dist\winget\manifests`, or
- fork `microsoft/winget-pkgs`, copy the folder into that path, and open the
  pull request from a branch named after the package and version.

A bot validates the manifest, installs the package in a sandbox and comments;
a moderator merges. Expect a day or two, and expect SmartScreen to be a topic
until the executable is signed (see the SignPath note in the wardogs repo).

Once the package is in, the README's Install section gets its sentence:

> On Windows, `winget install KallistoX.replaycut` puts `replaycut` on your
> `PATH`; run `replaycut install` once for the shortcuts, the notifications
> and the optional autostart.
