# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow
[Semantic Versioning](https://semver.org/).

replaycut 2.0 is a rewrite of a PowerShell service (1.x) that was never
published. The 2.0 line keeps that service's HTTP API; `docs/api.md` is the
contract.

## [Unreleased]

### Fixed

- **The size the share row promises is the size that comes out.** "about
  N MB" always planned a best-quality render at the recording's resolution,
  because the limits a target may put on a share (`maxHeight`, `maxKbps`,
  there since 2.7) never reached the page at all. With Nextcloud set to
  1080p and 8000 kbit/s the line promised 218 MB for a file of 11.76 MB. The
  storage targets carry their limits now, the estimate uses them, and it
  takes the 9:16 crop into account as well, which it also ignored; the line
  says what will really happen ("H.264, 1080p at 8000 kbit/s"). Measured
  against real renderings of a 2560x1440 recording: 15 MB promised for
  15.14 MB without limits, 9 MB for 8.69 MB at 1080p, 9 MB for 8.89 MB as a
  vertical cut. ([#25](https://github.com/KallistoX/replaycut/issues/25))
- **Every output has its own file again.** An output was named after the
  clip, the range and the title and nothing else, so rendering a range a
  second time - another quality, another audio track, another target -
  overwrote the first rendering, on this PC and on the storage. The older
  output stayed in the list with the size of a file that was no longer its
  own, and "Download", "Copy the file" and "Open the folder" handed over the
  newer one. A name that is taken now gets a counter: `..._2.mp4`.
  ([#26](https://github.com/KallistoX/replaycut/issues/26))
- **A clip whose recording went while replaycut was not running is listed
  again.** Since 3.0 a clip stays on the page for its cuts once its
  recording is gone - but only when the service watched it go. A recording
  deleted while replaycut was closed left a clip the next start never
  listed at all: its cuts and everything rendered from them were still on
  disk and in the store, and still on the Activity page, but there was no
  way to reach them from the clips page. The scan reconciles the store with
  the folder now, whoever emptied it.
- **A failed post no longer writes the credential into the log.** When a
  post could not reach its server at all - no network, a wrong host, a
  timeout - the error carried the URL it had tried, and for these
  integrations the URL is the credential: the bot token is part of
  Telegram's, a Discord webhook is nothing but its URL. That text went into
  the log, into the job's status, into the history and from there into the
  text behind "Copy diagnostics". The URL is stripped from such an error
  now.
- **The page comes back from a one-click update about three seconds
  sooner.** It waited a fixed three seconds before it even started asking
  whether the new service was up. It now waits for the old one to go quiet
  and then asks four times a second.
- **"Open the folder" and "Copy the file" work again after a restart.** Both
  looked the job up among the jobs of the running service only, so every
  output from an earlier run answered "unknown job" - which is every output
  on the clips and activity pages after the first restart, while the
  download button beside them worked. They ask the store as well now, as
  publishing and posting have since 2.6.1.
- **`presignDays` is validated as the documentation describes it.** S3
  presigned links live 1 to 7 days, and that is what `docs/settings.md`
  says, but `PUT /api/settings` took any number and stored it; only the
  signing clamped it back. Out of range is a 400 now, and a file that
  already holds such a number is repaired when it is read.
- **The cuts line of the diagnostics says megabytes below a gigabyte.** It
  read "0.0 GB" for anything smaller, which is most of the time.
- **A publish carries the `maxHeight` of the file it sends.** It copied
  every other field of its source job, so a rendering that had been scaled
  down read as a full-resolution one in the history.

### Changed

- **The Telegram integration says that it has never met a real bot.** It was
  built and tested against a fake of the Bot API; the card, `docs/api.md` and
  `docs/settings.md` say so now and ask for an issue either way, because an
  integration nobody can test may be dropped again.
- **The YouTube card no longer warns about a quota that is long gone.** Since
  2.6 the card and `docs/youtube.md` said an upload costs 1 600 of the
  project's 10 000 units a day, so the built-in client was good for about six
  uploads a day for everyone together. YouTube has since given `videos.insert`
  a bucket of its own: **100 uploads a day per project**, with the 10 000
  units left for the other calls (reading the channel name costs 1, deleting
  a video 50). The texts say that now - which also takes the sharpest edge off
  the reason to bring your own Google client, though the reason itself stays.

## [3.7.1] - 2026-09-11

A fix release for a hole that was there from the start: on a Windows that
had never seen a Visual C++ Redistributable, replaycut did not start at all.
Nobody noticed because every game brings that redistributable along - it
took a clean machine, and the one that found it was the winget validation
sandbox.

### Changed

- **Google verified replaycut's built-in YouTube client.** Connecting a
  channel is the plain Google login now: the "Google hasn't verified this
  app" screen is gone, and so is the limit of 100 accounts that may connect
  at all. The quota is unchanged - 10 000 units a day for everyone who uses
  the built-in client together, about six uploads - so `docs/youtube.md` and
  its "bring your own Google client" route stay as they are.

### Fixed

- **replaycut starts on a Windows that has no Visual C++ Redistributable.**
  The executable imported `VCRUNTIME140.dll`, which is not part of Windows -
  it arrives with that redistributable, which every game installs and a fresh
  installation does not have. On such a machine replaycut died before its
  first line with `STATUS_DLL_NOT_FOUND` and no message. The Windows build now
  links the C runtime into the executable, so the ZIP carries everything it
  needs; only the UCRT is left, and that has been part of Windows since 10.
  The executable grew by 135 KB. Found by the winget validation pipeline,
  which installs into a clean Windows sandbox.

## [3.7.0] - 2026-09-11

A small release about the settings page and about getting replaycut onto a
PC in the first place. The save bar no longer floats in the middle of a
short page, `Ctrl` + `S` does what it does everywhere else, the Limits
section says what Discord's inline player needs, and there is a winget
package waiting for its pull request.

### Added

- **The Limits section says what Discord needs.** Since 2.7 a share keeps the
  recording's resolution and quality, and Discord only shows its inline player
  for a linked file below the size its proxy accepts - above that the post is
  a bare link. `Limits (optional)` on Nextcloud, OneDrive, S3 and WebDAV now
  says so, and the Discord card points at the storage that feeds it. The line
  appears once a notify integration is on. Nothing changed about the encoding:
  best quality stays the default, the limit stays the choice per target.

- **`Ctrl` + `S` saves the settings page.** It works from inside a field too,
  which is where the hand usually is, and it no longer offers to save the page
  as an HTML file. The shortcut list behind `?` names it.

- **A winget package is prepared.** `dist/winget/` carries the manifests for
  `KallistoX.replaycut` and the note that goes with them: a portable package
  that puts `replaycut` on the `PATH`, with `replaycut install` still the step
  that adds the shortcuts and the autostart. `winget install replaycut` works
  once the pull request against `microsoft/winget-pkgs` is merged.

### Changed

- **The save bar sits at the bottom edge of the window.** It used to be
  `sticky`, which only pinned it while the settings page really did scroll:
  on a short tab in a large window it floated in the middle of the page with
  the bottom half of the window empty below it. Now it holds that edge the
  way the top bar holds its own, on every tab and at every window size, and
  the page keeps the room below its last card free.

## [3.6.0] - 2026-09-10

The release for the settings page. Settings › General had grown to seven
groups and a wall of grey help text, and the four things anyone actually
comes back for were buried in it. It is a short page again: what you come
back for stands open at the top, what you set once sits behind five lines
that already say what is set, and the explanations wait behind a "?" until
you ask for them.

### Changed

- **Settings › General has three levels instead of seven groups.** The theme,
  the display name and everything about access - the network switch with the
  address and its QR code, the password, the signed-in devices - stand open
  at the top. What you set once sits in five closed cards whose heads say
  what is set: `HTTPS off`, `auto (h264_amf) · below normal`, `Port 8420 ·
  starts when you sign in · daily update check`. A card that waits for a
  restart opens itself. `Advanced` at the bottom holds hardware decoding, the
  thread count, the log level and the password prompt for this PC.

- **The help of a setting sits behind a "?" next to its label.** Clicking it
  opens the explanation under the field; Escape or a click elsewhere closes
  it again. Lines that report state - "In use: h264_amf", what the network
  switch just did, when the certificate expires - keep standing on the page.

No setting moved out of `settings.json`, changed its name or changed its
meaning, and the HTTP API is untouched.

## [3.5.0] - 2026-09-10

The release for what the browser got in the way of, most of it on a phone: a
menu near the bottom of the screen opened into nothing, a banner that came up
while you worked further down the page sat a thousand pixels above you, and
the same phone took a new row in the device list every time it signed in
another way.

### Fixed

- **`replaycut uninstall --purge` removes every stored credential.** On
  Windows it deleted the two entries of 1.x, Nextcloud and the Discord
  webhook, and left the seven that came with 2.5 and 2.6 behind - the
  YouTube refresh token among them. Both platforms now walk the same list,
  so a purge leaves nothing of yours in the credential store.

- **A menu opens upwards when there is no room below it.** The "..." of a cut
  or an output row, the render menu, the share menu and the menu in the top
  bar all measured nothing and opened downwards, so a row in the lower half
  of the screen - on a phone, almost every row - put its entries out of
  sight. They are measured on open now, against the window and against a
  list that scrolls on its own, and flip up when that is where the room is;
  a menu that fits neither way is clamped and scrolls inside.

- **The banner strip stays under the top bar while the page scrolls.** A
  banner that came up while you were working further down the settings page
  was a thousand pixels above you and looked like nothing had happened.
  The first banner is pinned under the bar now; further ones scroll with the
  page, and on a phone the pinned one takes at most a third of the screen.

- **One phone is one row in the device list.** Signing in a second way -
  the password on one day, a QR code on the next - added a second session
  instead of renewing the first, and over a few weeks one phone filled the
  list. A browser now carries a device id (a second cookie, no secret, a
  year); a sign-in from a device that has a session renews that session, so
  the row keeps its place and says the newest way in. Rows from before this
  release have no id: those that share a browser and an address are shown as
  one device, and signing that one out ends all of its sessions.

### Changed

- The privacy policy says what Google user data replaycut accesses, how it
  uses it, whom it shares it with (nobody), how it protects it and how long
  it keeps it, each under its own heading (`docs/privacy.md`, and the same
  text on replaycut.de).

## [3.4.3] - 2026-09-10

The last of the three small things the 3.4 line needed, this one about the
updater itself: it announced a release the moment it was published, which
is a moment before it can be installed.

### Changed

- **A release is offered once it can be installed, not before.** Publishing
  and signing are two separate steps, and between them the release exists on
  GitHub without its `SHA256SUMS.minisig`. Until now the banner appeared the
  moment the release was published, and clicking it answered "the release is
  not signed yet - try again later"; the same answer then stuck around,
  because the check that found it only runs once a day. Such a release is no
  longer announced at all, and while one is waiting for its missing piece
  replaycut looks again every 15 minutes instead of every 24 hours, so the
  update turns up shortly after it is signed rather than the next day. The
  refusal on install stays as a second line of defence, for a release that
  loses a piece between the check and the click.

## [3.4.2] - 2026-09-09

3.4.1 made the HTTPS switch saveable; this one makes it usable. Restarting
into HTTPS left the page waiting on the address it came from, so the switch
ended in "the service did not come back" while replaycut was already
answering - one scheme over.

### Fixed

- **Turning HTTPS on gets you back to the page.** "Restart now" built the
  address to return to from the scheme the page was already using, so
  switching HTTPS on left the browser waiting on `http://` for a service
  that had just started answering `https://` - until it gave up with "the
  service did not come back", while replaycut was running fine. The address
  now comes from the settings that were saved, so it follows the switch the
  way it already followed a changed port. Across a scheme change the page
  cannot check the new address for you - it is a different origin, and a
  certificate of replaycut's own stops the browser at a warning first - so
  it says which way it is going and goes there rather than blaming the PC.
- **Saving a setting that needs a restart says so where you are looking.**
  The banner appears at the top of a long settings page; a switch near the
  bottom looked as if nothing had happened. Saving now brings the banner
  into view.

## [3.4.1] - 2026-09-09

3.4.0's one new feature could not be switched on. Everything that carries
HTTPS was in it - the certificate authority, the listener, the settings
page - except the one line that lets the settings API accept the field, so
saving the switch answered "unknown field: https". Editing `settings.json`
by hand worked, which is how it got through development and the release
without anyone noticing.

### Fixed

- **The HTTPS switch can be saved.** 3.4.0 carried `https` in the settings
  file and bound it in the settings page, but `PUT /api/settings` refuses
  every field name it does not know and that list was never told about the
  new one: saving answered "unknown field: https" and the one feature of
  the release could not be turned on from the UI at all. Editing
  `settings.json` by hand worked, which is why it went unnoticed. The field
  is accepted now, `https.cert` and `https.key` with it, and a typo inside
  the block is still a `400` rather than a field that quietly does nothing.
  A new check over `ui/index.html` fails the build if a field the page
  binds cannot be saved, so this cannot happen again to the next one.

## [3.4.0] - 2026-09-09

This release is about the two things replaycut had left open at the edges:
the connection, and the copies that a package manager keeps.

HTTPS is now a switch. It is off by default and it stays off for anyone who
does not want it, because in a home network plain HTTP is usually fine and
a certificate nobody trusts is a step backwards. Switch it on and replaycut
becomes its own certificate authority - one that is made once and never
replaced, so a device that trusts it keeps trusting it while the
certificate under it is renewed as your addresses change. Bring a
certificate of your own instead, from Tailscale or a reverse proxy, and
there is nothing to trust at all. The README says plainly what each way
costs you.

The same release teaches replaycut to be a good guest in a distribution
package, and gives a client that is not a browser a way in: it signs in
like a phone does and keeps a token instead of a cookie.

### Added

- **HTTPS, if you want it.** `https.enabled` in the settings makes the port
  speak TLS; it is off by default and a restart away, like the port itself.
  The service is then its own certificate authority: `<data-dir>/tls` holds a
  CA that is made once and never replaced - it is the identity of this
  replaycut - and a server certificate signed by it that carries the
  machine's names and addresses. That certificate is re-issued whenever the
  addresses change or it comes within 30 days of running out, and the CA
  stays put through all of it, so a device that trusts it once keeps
  trusting it. `https.cert` and `https.key` point at your own PEM pair
  instead, which is the comfortable way: a certificate from Tailscale, a
  reverse proxy or a real domain needs no trusting anywhere. Browsers do not
  know a certificate authority of ours, so they warn once until `ca.crt` is
  imported by hand - the log line at startup names the file and the
  fingerprint. A plaintext request to the TLS port gets a readable page with
  the `https://` link rather than a reset connection, and a certificate that
  cannot be read leaves the service running on plain HTTP with the reason in
  the log: on a gaming PC without a console, unreachable is worse than
  unencrypted.

  Settings › Access has the switch and, once it runs, the path of the
  certificate to import and its fingerprint to compare. Every address the
  service hands out follows the switch - the toasts, the tray, the QR code -
  and the session cookie gains `Secure` while TLS runs, and only then. The
  QR code carries the fingerprint as a fragment, which browsers never send
  and logs never see, so a client can pin the authority before its first
  request. The OAuth login keeps its plain-HTTP callback on a port of its
  own, because Google's rules for a loopback redirect say `http`.

  The diagnostics gain an `https` line: off, or the certificate in use with
  what is left of it, or - the one worth shouting about - switched on but
  unreadable, which leaves the service running unencrypted rather than not
  running at all.
- **A client that is not a browser can sign in.** `POST /api/login` and
  `POST /api/pair/request` take `client: "native"`, and then the token
  comes back in the body instead of a cookie; `Authorization: Bearer`
  counts wherever the cookie counts. Browsers keep getting a cookie they
  cannot read, which is the point of keeping the two apart. Settings ›
  Signed-in devices marks such a session *app*, and revoking it works the
  same. Nothing else changes for it: it asks, this PC allows, and the rate
  limits are what they were.
- **The UI is found next to a system-wide executable.** A distribution
  package puts the executable into `/usr/bin` and the UI into
  `/usr/share/replaycut/ui/index.html`; a relative `uiFile` is now also
  looked up under `<prefix>/share/replaycut` for an executable in
  `<prefix>/bin` and, on Linux, in every `$XDG_DATA_DIRS` entry, after the
  executable's folder and the working directory as before. Packages no
  longer need `--ui` in their desktop entry and unit. (#15)
- **The Linux ZIP carries the files a distribution package needs.**
  `replaycut.desktop` and `replaycut.service` for `/usr/bin/replaycut`
  (the installer's own output for that path, kept in `dist/linux/` and
  checked by a test) and the icon `replaycut.svg`, so a package installs
  them instead of writing its own; the README says where they go. (#15)
- **A copy from a distribution package knows it.** On Linux an executable
  under `/usr` was put there by a package (AUR, deb, rpm): `replaycut
  install` and `uninstall` say so and leave the package's files alone
  (`uninstall --purge` still removes this user's settings, state and
  credentials), `autostart on` and the switch in Settings enable the unit
  the package installed instead of writing a second one, `autostart status`
  names the unit that is enabled, and the update banner says the copy
  updates with the package manager instead of "not installed with
  install.cmd" - which, on Linux, now reads `install.sh` where it still
  applies. `GET /api/update` carries `fromPackage`. (#15)

### Fixed

- **"Start replay buffer" in the banner works on every page.** The banner
  saying the replay buffer is not running shows wherever you are, but its
  button was only wired up on the clips page - on Settings, OBS, Activity or
  Diagnostics, clicking it did nothing at all: no request, no message.
  It is now wired once with the other banners, and takes the same route as
  the button on the OBS page and the one in the diagnostics. (#20)

## [3.3.0] - 2026-09-09

This release is about the integrations: the one that was hardest to set up
now needs no setup at all, the page that lists them got its length back,
and the one that could never work is gone.

YouTube used to ask every user for a Google project of their own before the
card did anything. replaycut brings its own client now - switch on, connect,
done - and the old way stays for anyone who outgrows the shared quota.
Settings › Integrations shows eight closed cards instead of eight open
forms, each with the mark of the service behind it, and one sentence at the
top saying where a share actually goes. X leaves: its API has had no free
tier since February 2026, so there is no honest way to offer it.

- **YouTube works without a Google project of your own.** Until now every
  user had to create an OAuth client in the Google console before the
  YouTube card did anything - five clicks of console work that stopped most
  people right there. A release now carries replaycut's own client, so the
  card is switch on, *Connect YouTube*, done. The old way stays as "Own
  Google client" on the same card, because YouTube's quota belongs to the
  project and not to the user: 1 600 units per upload out of 10 000 a day
  means the built-in client is good for about six uploads a day across
  everyone who uses it, until Google grants the extension. `docs/youtube.md`
  is now the guide for that case and says plainly when it is worth it.
  Changing the client, or the client type, disconnects the channel: a
  refresh token belongs to the client that issued it.
  Two things to know while Google reviews the client, which takes weeks:
  connecting shows "Google hasn't verified this app" once - open *Advanced*
  and continue - and at most 100 Google accounts can connect it until the
  review is through. Both are gone once it is, and a client of your own has
  neither.

### Changed

- **Settings › Integrations is a page again, not a scroll.** Eight
  integrations with every field of every one of them open at once made the
  page a screen and a half on a laptop and endless on a phone. The cards now
  start closed: a head with the service's own mark, the badge with its live
  state and the switch, and one click opens the fields it always had. An
  integration that is off says in one line what it would do instead of
  showing an empty form. Above them, one sentence says what the Share button
  actually does right now - and the quick-share target is chosen there, once,
  instead of hunting for the right switch among eight cards. Nextcloud,
  OneDrive, YouTube, Discord and Telegram carry their own marks; S3, WebDAV
  and the webhook keep drawn icons, because those are a protocol or a family
  of providers rather than one product. A card opens when its head is
  clicked, not only its arrow.
- **The code of a device login is a code, not a word in a sentence.** When
  OneDrive or YouTube ask you to type a short code at the provider, that
  code is what the eye is looking for: it now stands in a display of its
  own, monospace and spaced out, with the link under it saying that any
  device will do - a phone is the point of that way in.

### Fixed

- **YouTube errors say what is wrong.** Google answers a failed upload with
  the message "Unauthorized" and puts the reason in a field replaycut threw
  away, so a share failed with "HTTP 401 Unauthorized Unauthorized" and left
  the user guessing. Errors now carry Google's reason and, for the ones that
  mean something, the remedy: a Google account without a YouTube channel
  ("create one at youtube.com, then connect again") and an exhausted daily
  quota ("resets at midnight Pacific time"). The diagnostics row for a
  missing channel used to suggest reconnecting, which never helped.
- **A device code no longer slips away while you read it.** The card polls
  the login every two seconds and used to rewrite the line with the code
  each time, which cancelled any selection of it and flickered. It now
  writes only what has changed, and one click selects the whole code.

### Removed

- **X is no longer a share target.** X's API has had no free tier since
  6 February 2026: every post is billed per request to whoever registered
  the app, so a client shipped with replaycut would put all its users'
  posts on the maintainer's bill, and asking each user for their own paid
  developer account is worse. The card, the settings block
  (`integrations.x`), the credential `replaycut/x`, the OAuth provider and
  the diagnostics row are gone. No release ever carried an X client id, so
  the target could never be used; `integrations.x` in an existing
  `settings.json` is ignored like any unknown field.

## [3.2.0] - 2026-09-08

3.1 taught the service a second platform; this release finishes the job.
Secrets, notifications, the clipboard, the installer, the autostart, the
tray icon and a package of its own in every release: a Linux machine sets
replaycut up the way a Windows one does, and the one-click update works
there too. The browser side gets twelve themes to pick from and a top bar
that fits on a phone.

- **Twelve themes on board.** A fresh installation no longer offers the
  dark default alone: Material Design dark and light, the four Catppuccin
  flavours, Nord, Dracula, Gruvbox dark, Solarized dark, Tokyo Night and
  the light `plain` are built into the executable and appear in Settings ›
  General › Theme, switching without a restart. Every one of them meets
  the contrast targets of `docs/themes.md`, and the light ones carry the
  selected range on the timeline at the same strength as the dark ones
  (`--accent-soft` at 0.22), where a weaker tint went under. A theme file
  of the same name in the `themes` folder of the data directory still
  wins, so a shipped theme can be copied and changed. Contributed by
  almighty-atlas, issue #11.
- **Linux, second stage: the platform services.** Secrets go to the
  keyring behind the freedesktop Secret Service (gnome-keyring, KWallet,
  KeePassXC), so every integration and the obs-websocket password can be
  set up; notifications appear on the desktop and a click opens the page;
  "Copy link" serves the Wayland clipboard (and stays there after the
  service moved on), "Copy file" offers the file to paste in a file
  manager, "Open folder" asks the file manager to show it; ffmpeg runs at
  the nice level `ffmpegPriority` asks for; the diagnostics show memory
  and free space; and `h264_vaapi` joins the encoder detection for AMD and
  Intel GPUs, one profile pair per render node, with `hwaccel: vaapi` as a
  manual choice. The replay hotkey stays unavailable on Wayland; the page
  says so and points at obs-websocket.
- **Linux, third stage: install, autostart and the page's wording.**
  `replaycut install` (or `install.sh`) puts the files under
  `~/.local/share/replaycut/app`, links `~/.local/bin/replaycut`, adds a
  desktop entry and an icon and asks whether replaycut should start with
  the desktop session, which a systemd user unit then does; the switch on
  the settings page and `replaycut autostart` drive the same unit.
  `uninstall` takes it all back again. The status document says which
  platform the service runs on (`config.platform`), and the page words
  things for it: keyring instead of Credential Manager, trash instead of
  recycle bin, the app menu instead of the Start menu.
- **Linux, fourth stage: the release package.** Every release now carries
  `replaycut-<version>-linux-x64.zip` next to the Windows ZIP - one static
  executable that runs on any x64 distribution - and one `SHA256SUMS` for
  both, so the maintainer's signature covers both. The one-click update
  works on Linux: it fetches the package for its platform, keeps the
  executable runnable and installs into what `install.sh` set up.
- **Linux, fifth stage: the tray icon.** A StatusNotifierItem on the
  session bus - what KDE, Waybar and GNOME with the AppIndicator extension
  show - with the menu of the Windows tray: Open, Copy address, Show QR
  code, the sign-in requests, Pause scanning, Check for updates, Open log
  folder, Quit; the tooltip and the busy and error badges follow the state
  as on Windows. Without a tray host the service runs without the icon.

### Changed

- replaycut has a website: <https://replaycut.de>, one page with the
  screenshots, the four steps and the download. The OAuth integrations point
  at it now: `docs/youtube.md` names `https://replaycut.de/` as the
  application home page and `https://replaycut.de/privacy/` as the privacy
  policy instead of two GitHub links. The site's source is its own
  repository, `KallistoX/replaycut.de`.
- **One top bar for every width.** It carries the wordmark, the three pages
  a session uses - Clips, Activity, Settings - as an icon with a label, the
  status dot, and a "..." menu that holds OBS, Diagnostics, the keyboard
  shortcuts, the encoder and storage badges and the version. Below 600 px
  the three labels fall away and the icons grow to touch size, so the bar
  stops running over the screen edge on a phone - it needed 424 px of the
  375 a phone has. The page you are on is marked by the accent on its label
  and its icon instead of a filled box with a line under it; when that page
  is one from the menu, the "..." button carries the mark. Storage above
  80 % puts a dot on that button, so the warning is visible without opening
  the menu.

### Fixed

- The note in "Open it on your phone" ("Only this PC can reach replaycut
  ... set **Listen on** to all interfaces ...") broke into three columns
  while the service listened on loopback only: the note is a flex row, and
  the bold words became a flex item of their own. It reads as one paragraph
  again, with the info icon the same note carries in the wizard. (issue #9)
- Linux: "Open log folder" in the tray opens the folder in the file manager
  again. It went through `xdg-open`, whose handler for folders may be a
  terminal program that shows nothing when started without a terminal;
  folders now go to the file manager over D-Bus (`FileManager1.ShowFolders`)
  first, `xdg-open` is the fallback, and an `xdg-open` that exits with an
  error is logged instead of vanishing quietly. (#7)
- Linux: the address for other devices ("Copy address" in the tray, the QR
  code, the diagnostics) used the bare host name, which only resolves on
  the PC itself. It is now `<host>.local` when the machine announces itself
  over mDNS (Avahi, systemd-resolved), else the IPv4 address. (#8)
- The update banner no longer offers "Update now" for a release that has no
  package for this platform (a Linux build looking at a release from before
  the Linux package): it says so, offers the release page, and `download`
  explains it instead of "missing the ZIP". `latest.packaged` in
  `GET /api/update` carries the fact. (#10)

## [3.1.0] - 2026-09-08

The clips page of 3.0 put the same thing on the screen three times. This is
that page after a tidy-up: a cut is a row, an output looks the same wherever
it shows up, and the button per action became a menu per row. Underneath,
the service learns a second platform.

### Added

- **Linux, first stage.** The service builds, passes its tests and runs on
  Linux, and CI checks that next to Windows. The single-instance guard and
  `replaycut stop` work there (a lock file under `$XDG_RUNTIME_DIR` and
  SIGTERM), the address for other devices carries the real host name, a
  start without a terminal opens the browser like the Windows shortcut,
  the OBS profiles are read from `~/.config/obs-studio` (or the Flatpak's
  copy) and the default clip folder is the XDG videos directory. Secrets,
  notifications, the clipboard under Wayland, GPU encoding through VAAPI,
  autostart, the installer and the tray follow in the next stages; until
  then those report that they are not available on this platform.

### Changed

- **The clips page carries less.** A cut is one row now - its range, how
  long, which audio, and where its outputs went - and it opens to show them.
  An output is the same row wherever it appears: under its cut, in the
  result of the last share, on Activity. Each row has the one action you
  want (copy the link, or open the folder for a local file); everything
  rarer - the page link, download, post, publish, render to another target,
  delete - moved into a "…" menu. The four settings of the share row read as
  a sentence ("Mix (all) · H.264 · 16:9 · mark done afterwards") and open on
  "Change", and the Share button says where it is going.
- The clip list says what it knows in words instead of a wall of badges, the
  key hint on the Share button no longer fights the accent colour, and a
  section heading no longer competes with the buttons beside it.

## [3.0.1] - 2026-09-08

Two things 3.0.0 got wrong, found by putting a real 2.x state next to real
recordings. 3.0.0 was published but never signed, so this is the first 3.0
anyone can install.

### Fixed

- Rendering a cut no longer marks its clip done. "Afterwards" belongs to the
  share row; a render often happens days later and says nothing about the
  clip. (The endpoint still takes `after`, the page no longer sends it.)
- The migration lists the clips whose recording is long gone. Their old
  shares hung under a clip the page never showed, so they were only
  reachable on Activity; they now appear under "Done" with their links,
  their title and the day they were recorded (read out of the file name).
  A cut from before 3.0 has no cut file, says so and offers "Marks" instead
  of "Render": the range goes back on the timeline and one Share makes it.

## [3.0.0] - 2026-09-08

Cut, then decide. A clip used to be one dialog on a five-minute recording:
pick a range, pick the audio, upload, and whatever you did not decide then
was gone with the recording. 3.0 puts a **cut** in between - the range as its
own file, the picture untouched and every audio track along - and renders
everything from that. The same cut goes to Nextcloud today and to YouTube as
a Short tomorrow, with another audio mix, long after the recording is in the
recycle bin. **Save cut** saves a range while you play and renders nothing;
the clips page shows what came out of every cut.

The list keeps itself: a shared clip is done and out of the way, grouped by
the evening it belongs to, one click back. **Activity** is the new page for
"what did I send where".

**Your state moves.** Titles, the seen list and the share history leave their
three JSON files for `replaycut.db` beside the settings. The first start
imports them and moves the old files to `backup-2.x\`, so an installation of
2.x that is put back finds its state where it left it. The history is no
longer capped at 200 entries.

### Added

- **Every share keeps its cut.** The range you pick becomes a file of its
  own first - `.cuts\<id>.mkv` in the clip folder, the recording's picture
  and *all* its audio tracks, copied, not re-encoded - and the upload is
  rendered from that. So audio mode, 9:16 and quality are decisions you can
  take again later: the cut is enough, the five-minute recording is not
  needed any more.
- **Save cut**: save a range while you play and render it after the game -
  to any target, with another audio mix or as a Short, as often as you like.
- **Shared clips leave the list.** A clip you have shared is done and is out
  of the way; `?done=1` lists the done ones, and one call brings a clip back.
  "Afterwards" per share decides between keeping it, marking it done and
  moving the recording to the recycle bin - the new setting
  `cleanup.afterShare` (default: done) is what the share row starts with.
  `cleanup.recycleDoneAfterDays` does the same on a timer.
- Deleting a clip has a reach: `?scope=clip` recycles only the recording and
  leaves the cuts, so the clip can still be rendered and published;
  `?scope=all` (the default, as before) takes everything with it. A single
  cut can go on its own with `DELETE /api/cuts/<id>`.
- A recording that disappears from the folder no longer takes its cuts with
  it: the clip stays in the list, marked done, with everything it produced.
- Diagnostics: a line for the cuts - how many files, how much space, how
  many the service knows about - with a warning from 10 GB on.
- The history keeps every entry now instead of the newest 200, and
  `GET /api/history` takes `limit` and `before` to walk back through it.
- **The clips page was rebuilt.** One list with a filter (Active / Done) and
  a heading per day, badges for the cuts and the targets a clip went to; the
  clip itself shows its cuts underneath, each with what came out of it and a
  "Render" of its own. "Save cut" and "Afterwards" sit next to Share, and a
  clip whose recording is gone shows its thumbnail and keeps its cuts.
- **New page: Activity.** What is running with a way to cancel it, and every
  output ever made - newest first, filtered per target, one click to the clip
  it came from. It replaces the "Shared" list on the clips page.

### Changed

- **replaycut keeps its state in one file.** Clip titles, which clips have
  already been announced and the share history moved out of three JSON files
  and into `replaycut.db` next to the settings. The first start of 3.0
  imports the old files and moves them to `backup-2.x\`, so an installation
  of 2.x put back later finds its state where it left it.
- The share progress has one more step, "Cut", before "Encode". It takes
  about a second and needs no graphics card. What comes out is frame for
  frame what 2.8 produced.

### Removed

- The "Shared" section of the clips page: its entries are on Activity now,
  and under the cut they came from on the clip itself.

### Fixed

- Settings: changing how long presigned S3 links stay valid can be saved
  again - the value went out as text and the service refused it.

## [2.8.0] - 2026-09-07

Secure by default. replaycut now listens on your PC only until you open it
up, and opening it up is one switch that asks for a password first. The
everyday way onto a phone is no longer typing that password: the phone
asks, your PC shows the request with a code and the device's name, and one
click lets it in - or the phone scans the QR code, which signs it in on the
spot. Settings lists every signed-in device and lets you sign one out.
Nothing about your clips or the sharing changed.

### Added

- **replaycut now listens on this PC only.** A new installation is not
  reachable from the network until you turn it on - in the setup wizard or
  under Settings › Access - and turning it on sets a password first and
  then asks Windows for the firewall rule. The installer no longer creates
  that rule on its own.
- Settings › Signed-in devices: every browser that may use replaycut, with
  its name, address and when it was last seen, one "Sign out" per device
  and "Sign out everywhere".
- Diagnostics: three new lines - where the service listens (a failure when
  it is open to the network without a password), whether the firewall rule
  exists, and how the device login is doing.
- **Sign in on your phone without typing the password.** The login page
  offers "Ask" plus the name of your PC: the PC shows a notification, the
  open UI a card and the tray an entry, all with the same four-character
  code and with the device's name and address. One click on Allow and the
  phone is in. A request runs out after two minutes.
- The QR code in the wizard and in the settings signs a phone in when it
  is scanned: its address carries a token that is good once and for two
  minutes. Only this PC and signed-in devices get such a code.
- `requireLoginOnLoopback` in the settings: ask for the password on this PC
  as well, for a Windows account other people use.
- "Generate one for me" next to the password fields in the wizard and in
  the settings: four words from a built-in list, shown once in clear text.
- `allowedHosts` in the settings: names this replaycut answers to besides
  `localhost`, its own name and any address - for an own DNS name or a
  reverse proxy.

### Changed

- An installation that is reachable from the network without a password
  keeps working, but says so: a red banner with "Set a password" and "This
  PC only", one notification per start, and a failure in the diagnostics.
- A request that names a host this replaycut does not answer to is refused
  with 421. That closes DNS rebinding, where a page in your browser uses
  its own name to reach the service on your PC.
- A password is 8 to 128 characters now, and there are no rules about
  digits or symbols: length is what counts.
- Signed-in devices are recorded with a name ("iPhone, Safari"), their
  browser, their address and when they were last seen. Sessions from
  earlier versions keep working.
- More than 30 failed logins from all addresses together within five
  minutes pause the password login for ten minutes.
- The size estimate in the share row learns from your own shares: the job
  records the recording's codec and bitrate (`codec`, `sourceKbps`), and
  the row uses the median ratio of the last plain H.264 shares of that
  codec instead of a fixed factor per codec.

### Fixed

- The page is mobile-friendly again: 2.7.0 shipped with a broken viewport
  meta tag, so phones rendered the desktop layout scaled down.
- The "Limits" fields on the storage cards save from the settings page
  again; 2.7.0 sent the height as text and the service answered 400.

- A restart (update, settings, `replaycut stop`) no longer waits up to 5 s
  while a browser has the player open on a long clip: the shutdown now
  gives open connections one second, which is all the restarting request
  needs.
- "Post to ..." shows its result on the result card as well: the status
  reaches `last` (what the card shows after a reload), the card re-renders
  after the click, and a status such as "Discord: Link posted" or "Posted
  (HTTP 200)" is no longer drawn as a failure.

## [2.7.0] - 2026-09-06

Quality first: a share now looks like the recording, on every target,
and space limits belong to the target that needs them. Posting to Discord
and friends happens for the quick share only; everything else asks.

### Changed

- Shares keep the recording's resolution and frame rate and are encoded in
  the encoder's quality mode: no more 1080p and 6000 kbit/s by default,
  the size follows the picture. The global "Video bitrate" setting
  (`shareKbps`) is gone; every storage card has an optional "Limits"
  section (max height, max bitrate) for the places where space matters.
  A "Publish to" onto a target with limits cuts the clip again within
  them.
- Only the quick share (the Share button's target) posts to Discord,
  Telegram and webhooks automatically; shares from the menu and "Publish
  to" stay quiet, and the result card and the history offer "Post to ..."
  instead (`POST /api/jobs/<id>/post`).
- The share mode "Fast copy" is now called "As recorded (no re-encode)".
- The Share menu says which target posts automatically, the progress card
  shows "no auto-post" for the others, and the size estimate accounts for
  the recording's codec (an AV1 recording grows about 2.5x as H.264).

### Fixed

- The stage list under the progress bar broke its layout as soon as a
  stage was done: the wizard's "done" page style leaked into it.

## [2.6.1] - 2026-09-06

### Fixed

- "Publish to ..." on a history entry from before the last restart answered
  "unknown job"; the source is now read from the history as well.

### Changed

- `docs/privacy.md` states what replaycut stores and sends, and
  `docs/youtube.md` names the home page and privacy URLs Google wants
  before an app can be published.

## [2.6.0] - 2026-09-06

Beyond the cloud folder: a share can become a YouTube video (a vertical cut
a Short) or a post on X, the link can go to Telegram or any webhook next
to Discord, the finished file downloads straight to the phone, and a
browser that cannot play the recording gets a playable copy on demand.

### Added

- YouTube as a share target: every share is uploaded as its own video
  (unlisted by default, private or public by choice), title from the clip,
  description from a template, link `youtu.be/<id>`. Uses your own Google
  client because of YouTube's upload quota; connected with a code at Google
  like OneDrive, or, with a Desktop client, in the browser on this PC
  (loopback login with PKCE: `POST /api/oauth/<provider>/loopback`,
  `GET /oauth/<provider>/callback`). `docs/youtube.md` walks through the
  five-minute setup.
- X as a share target: every share becomes a post with the video attached
  (chunked media upload, text from a template), link `x.com/<user>/status/
  <id>`; connected in the browser on this PC. Needs a build with the
  replaycut app's client id.
- Telegram as a notify integration: a bot posts the link into a chat,
  group or channel (`integrations.telegram`, token as `telegramToken`,
  `POST /api/test/telegram`).
- Generic webhook as a notify integration: a JSON `POST` per share to any
  URL, signed with `X-Replaycut-Signature` when a secret is stored, for
  n8n, Home Assistant, Zapier or a Matrix bridge (`integrations.webhook`,
  `POST /api/test/webhook`).
- Download: the result card and the history offer the finished MP4 as a
  download (`GET /api/jobs/<id>/file`); on a phone it lands in the gallery,
  ready for TikTok, Instagram or WhatsApp.
- Playable preview: when the browser cannot decode the recording (AV1 on
  an iPhone), the player offers "Make a playable preview", a 720p H.264
  copy made on the PC and kept next to the preview (`clip.previewH264`,
  `POST /api/clips/<base>/preview`). `previewH264: always` in the settings
  makes it right after every recording with idle priority.
- Vertical cut: "Vertical 9:16 (Short)" in the share row crops a full-height
  window (position by slider, shown over the player) and scales it to
  1080x1920 - a Short on YouTube, or a file for TikTok, Reels and WhatsApp
  (`vertical` and `verticalPos` in `POST /api/share` and the job).

### Changed

- The Integrations tab lists YouTube and X under Storage, Telegram and
  Webhook under Notify; the OneDrive and YouTube cards share one connect
  flow. The delete dialog's "also remove" now names every storage, not
  only Nextcloud.
- Notify integrations receive the share as structured data (title, clip,
  target, link, time); the Discord post itself is unchanged.

## [2.5.0] - 2026-09-05

Share where you like: every configured storage is a target with its own
entry in the Share menu, a finished clip can be published to another one,
and three new storages join Nextcloud - OneDrive (connected with a code at
Microsoft), any S3-compatible bucket and any WebDAV server.

### Added

- Share targets: every configured storage is a target, `POST /api/share`
  takes `target` (a storage id or `file`), the default is the storage marked
  "quick share" in the settings. `config.targets` lists the integrations
  with their state.
- Publish again: `POST /api/jobs/<id>/publish` sends the finished file of a
  share to another storage without cutting it again.
- OneDrive as a storage: connect with a code at Microsoft (device flow, works
  from a phone), uploads go to `Apps/replaycut/<month>/` with a link anyone
  can open. `GET /api/oauth/<provider>`, `POST .../start`, `POST
  .../disconnect`. Needs a build with a client id.
- S3-compatible storage (AWS S3, Cloudflare R2, Backblaze B2, MinIO, Wasabi):
  SigV4-signed uploads to `<prefix>/<month>/`, links from a public URL or
  presigned with an expiry; `POST /api/test/s3` checks bucket and keys.
- Generic WebDAV storage: any DAV server plus a public URL that serves the
  folder; `POST /api/test/webdav` checks server and login.
- Deleting a clip with "also remove from storage" now removes the remote
  copies from every storage its shares went to.

### Changed

- The post stage of a job is called `notify` (was `discord`); the status
  text stays in `discord`. Settings gain `integrations.nextcloud.quickShare`
  and `integrations.discord.autoPost`, both default on.
- Integrations tab in two groups, Storage and Notify, with "Quick share
  target" and "Post automatically" switches; the Share button gets a menu
  with the other storages and "file only"; result card and history show the
  target and offer "Publish to ..." for every other storage.

## [2.4.0] - 2026-09-05

Cutting gets comfortable: shares queue up and can be cancelled, every clip
has a picture, the Nextcloud quota sits in the header, a fast copy mode
skips the re-encode, the GPU decodes where it can, and the page hears about
changes the moment they happen.

### Added

- Shares queue up instead of answering "a share is already running": the
  Share button stays usable, the card shows the place in the queue, and the
  next job starts as soon as the running one ends (`position` in the
  answer and the job, `queue` in `GET /api/clips`).
- Cancel a share: `POST /api/jobs/<id>/cancel` and the Cancel button on the
  progress card. Waiting jobs leave the queue at once; a running encode or
  upload is stopped and its partial output removed.
- Thumbnails: every clip shows a picture from 10 s before its end in the
  list and as the player poster (`thumb` on the clip, `GET /media/<base>.jpg`).
- The Nextcloud quota in the header ("Nextcloud 63 %", yellow from 80 %,
  red from 95 %), refreshed in the background (`config.quota`).
- Fast copy: a share mode that keeps the OBS video stream instead of
  re-encoding (keyframe-accurate, `mode: copy`, `actualStart` in the job).
  The choice is remembered in the browser; the default stays H.264.
- GPU decoding: the encoder detection now tries the full GPU path of each
  vendor with a real clip (AMD `d3d11va`, NVIDIA `cuda` with `scale_cuda`,
  Intel `qsv`) and falls back to software decoding per share when it fails.
  On an AMD card the AV1 decode moves off the CPU (13 s instead of 42 s CPU
  time for 30 s of 1440p60). `hwaccel` gains `auto` (the default) and `none`.
- `replaycut bench`: encodes part of the newest clip with every profile and
  prints wall time, CPU time and speed.
- The page listens to `GET /api/events` (Server-Sent Events) instead of
  asking every 3 s: changes show up at once, an idle page costs nothing,
  and a restart no longer waits for open connections. Polling stays as the
  fallback.
- The title field suggests the recording day and time as a placeholder;
  Enter on the empty field takes it.
- A keyboard shortcut list behind the "?" button and the "?" key.

### Changed

- Contract: a second `POST /api/share` while a job runs answers 202 with a
  queue position (409 only for the same cut twice).

### Fixed

- "Update now" on a release that has no signature yet ends in an error
  ("not signed yet") instead of waiting forever.

## [2.3.1] - 2026-09-05

A small release to prove the one-click update from 2.3.0.

### Fixed

- README and CHANGELOG: the install path `%LOCALAPPDATA%\replaycut\app` had
  lost its backslashes.

## [2.3.0] - 2026-09-05

The one-click update and the complete tray menu. From this release on,
every release is signed: the updater installs only what the maintainer's
minisign key vouches for.

### Added

- One-click update: `GET /api/update` and `POST /api/update/{check,download,install,seen}`.
  The service downloads the release ZIP, verifies the minisign signature of
  `SHA256SUMS` and the hash, unpacks, checks the new executable and restarts
  into it. Releases without a valid signature are never installed.
- The update banner: "Update now" runs the whole update with a progress bar
  and reloads the page on the new version; "What's new" shows the release
  notes; after an update the page says so once. Settings › General has
  "Check for updates now" with the time of the last check.
- The tray menu is complete: Open, Copy address, Show QR code, Pause
  scanning, Check for updates (with a notification for the outcome), Open
  log folder, Quit. The tooltip says "paused" and "update available".
- `POST /api/scanning { paused }` and `config.scanning`: pause the folder
  scan from the tray or the API; the UI shows a banner with "Resume".

### Changed

- The address dialog shows no QR code while replaycut listens on this PC
  only; it says how to change that instead.

## [2.2.0] - 2026-09-04

The OBS integration: replaycut talks to OBS through obs-websocket, saves
replays without simulated key presses, shows when the replay buffer is
stopped and compares the OBS profile with what it expects. Read-only; the
only actions are saving a replay and starting the buffer.

### Added

- OBS integration, part 1: the service connects to obs-websocket 5 on this PC
  (`obs` in settings.json, default `localhost:4455`, password as the
  credential `replaycut/obs-websocket`), keeps reconnecting with a backoff,
  and answers F9 through `SaveReplayBuffer` when connected - with a clear 409
  while the replay buffer is stopped - instead of a simulated key press; the
  key press stays as the fallback. `config.obs` reports the connection.
  A saved replay wakes the scanner at once; a stopped buffer raises a
  desktop notification. Contract: docs/api.md "Since 2.2".
- OBS integration, part 2: the OBS page reads profile, recording folder,
  format, encoder, video settings and the audio-track layout through the
  connection and compares them with what replaycut expects - every
  difference with the OBS menu path, plus the buttons "Start replay buffer"
  and "Use this folder in replaycut"; the top bar shows OBS, the clips page
  warns while the buffer is stopped, the wizard uses the connection, the
  diagnostics row is real. `GET /api/obs`, `POST /api/obs/replay-buffer/start`,
  `/reconnect`, `/refresh`, `/adopt-folder`. Nothing in OBS is written.

## [2.1.0] - 2026-09-04

Setup in the browser, settings at runtime, an optional password, a
diagnostics page and the new design. Rollout to the group still waits for
the one-click update.

### Added

- Settings change at runtime: `GET/PUT /api/settings` (everything but port,
  bind and the UI file takes effect at once), `POST /api/test/nextcloud` and
  `/api/test/discord`, `GET /api/addresses` with a QR code, `GET /themes/<name>.css`
  from the data directory, `POST /api/restart`. Contract: docs/api.md "Since 2.1".
- Optional password for other devices: argon2id hash in settings.json, 30-day
  session cookie, login throttle; this PC (loopback) never needs it. Every
  cross-site write is refused by an Origin check, password or not.
- `setupDone` and `theme` in settings.json; the page routes `/setup`,
  `/settings`, `/diagnostics`, `/login` serve the UI file.
- Local mode without integrations has a way out of the browser: "Open folder"
  and "Copy file" on the result (`POST /api/jobs/<id>/open-folder` and
  `/copy-file`; the file lands in the clipboard as a file object, Ctrl+V in
  Discord attaches it).
- The UI moves to the design system (docs/design): top bar with the pages,
  banners instead of the status block, the clip list as a collapsed panel
  beside the game, a login page, themes from the data directory.
- Setup wizard at `/setup` (OBS folder from the profile, live check for the
  first replay with codec and browser playability, integrations with tests,
  password, addresses and QR code) and the settings page at `/settings`
  (General and Integrations, changes apply at once, restart button for port
  and bind). `GET /api/setup/obs` reads the OBS profiles; clips carry codec,
  size and frame rate.
- Diagnostics page at `/diagnostics` and `GET /api/diagnostics`: eleven
  checks with a fix per problem and a text copy without secrets;
  `replaycut test` prints that text when the service runs.
- A fresh installation opens the browser setup; a migrated one counts as set
  up. `replaycut setup` points at the wizard.
- `docs/design`: the design system for the web UI - tokens, component sheet,
  page mockups and icon sources - and `docs/themes.md`, the theme format.

### Changed

- New application and tray icons (play mark with cut marks, amber on a dark
  tile) in six sizes up to 256 px; the tray states "job running" and "last
  job failed" carry an amber or a red dot. Rendered from the SVG sources in
  `docs/design/icons` by `mkico`.

## [2.0.0] - 2026-09-04

First public release: the Rust service, the installer and the migration
from the 1.x PowerShell service.

### Added

- Update hint: a minute after start and then daily the service asks GitHub
  for the latest release; a newer one appears as `config.update` in
  `/api/clips` (documented in `docs/api.md`, covered by the contract suite)
  and as a dismissable banner above the clip list. `checkUpdates: false`
  switches the check off. Nothing is downloaded.
- Windows integration, part 2: `replaycut install` (idempotent, per user, no
  admin except the optional firewall rule): copies the program to
  `%LOCALAPPDATA%\replaycut\app`, writes start menu and desktop shortcuts
  with the AppUserModelID, registers the notification app id, asks about
  autostart (HKCU Run entry, default off) and the firewall rule (one UAC
  prompt, private profile), starts the service and opens the page;
  `replaycut uninstall` (`--purge` also removes settings, state, logs and
  credentials); `replaycut autostart on|off|status`; migration from the 1.x
  PowerShell service (task arguments, state files, credentials, task and
  firewall cleanup); `install.cmd` and `uninstall.cmd` for the release ZIP.
- Windows integration, part 1: the executable runs without a console window
  and attaches to the terminal it was started from for `--help`, `setup`,
  `test`, `stop` and log output; only one instance runs at a time (a second
  start opens the browser); a tray icon with Open, Copy address and Quit,
  a tooltip with the clip count or share progress and a badge while a share
  runs or after a failed one; `replaycut stop` ends the running service
  through a named event; desktop notifications for saved clips and share
  results (WinRT toasts, shown once the app is registered by the installer);
  `--no-browser`; the log records the shutdown reason and panics with a
  backtrace; fatal start-up errors show a dialog when there is no console.
- Service core, part 4: resource limits for ffmpeg (`ffmpegPriority`,
  default below normal; `ffmpegThreads`, default half the cores) so a share
  does not stall the game; `docs/settings.md`; idle footprint of the release
  build measured and recorded in the README.
- Service core, part 3: real integrations. Nextcloud storage (WebDAV upload
  into `<folder>/<YYYY-MM>/`, public link created or reused, remote delete)
  and Discord notify (webhook post with the display name as user name).
  Credentials live in the Windows Credential Manager under
  `replaycut/nextcloud` and `replaycut/discord-webhook`; `replaycut setup`
  configures both on the console, `replaycut test` checks them.
- Service core, part 2: the share pipeline. `POST /api/share` validates and
  registers a job (202, 409 while a job runs, 404 unknown clip, 400 invalid
  selection or audio mode), encodes with ffmpeg and live progress, then runs
  the storage and notify integrations when enabled, records history (200
  entries) and keeps the last 30 jobs. `--dry-run` uses simulated
  integrations with `dry-run.invalid` links. `DELETE ...?nextcloud=1`
  removes remote copies through the storage integration. The contract suite
  passes 11/11 against the Rust service in dry-run mode.
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

[Unreleased]: https://github.com/KallistoX/replaycut/compare/v2.2.0...HEAD
[2.2.0]: https://github.com/KallistoX/replaycut/releases/tag/v2.2.0
[2.1.0]: https://github.com/KallistoX/replaycut/releases/tag/v2.1.0
[2.0.0]: https://github.com/KallistoX/replaycut/releases/tag/v2.0.0
