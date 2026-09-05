# replaycut design system

The design for every page of the web UI, the tray icon and the toasts, as
one system. Everything here is static HTML and CSS that the UI lifts as it
grows; nothing is generated at build time.

| File | What it is |
|---|---|
| `tokens.css` | The design tokens and, at the same time, the default theme `wardogs`. See [docs/themes.md](../themes.md). |
| `themes/plain.css` | A light theme that proves the token contract: it overrides values only. |
| `base.css` | Every component. No colour literal in here; each one is a token. |
| `components.html` | The component sheet: every component in every state, both themes, a live contrast table, the UI icon sprite. |
| `mockups/*.html` | One mockup per page with a state switcher (bottom left) and a theme switcher. Widths come from the window: 1000 px and up is two columns on the clips page, 700 px is "the window beside the game", 375 px is a phone. |
| `mockups/mock.js`, `mock.css` | The switchers. Not part of the UI, except the three `matchMedia` lines that open the clip list on wide screens. |
| `icons/` | SVG sources of the app icon and the two tray states, `mkico`, the tool that renders them to `.ico`, and `social.svg`, the 1280x640 social preview for the GitHub repository (rendered to `social.png`). |

Open any of the HTML files straight from disk; there is no build step and
no external asset.

## Navigation and layout

- A top bar on every regular page: wordmark, the four pages (Clips,
  Settings, OBS, Diagnostics), a status dot (ok / job running / warning /
  failed / unreachable) and the version. The setup wizard and the login page
  carry only the wordmark.
- Banners sit directly under the top bar, stacked in this order: service
  unreachable, last share failed, scan stuck, update available. They replace
  the old status block.
- **Clips page**: two columns from 1000 px (list 280 px, editor next to
  it). Between 700 and 1000 px the list becomes a collapsed panel above the
  editor showing "3 clips · newest 21:14" and the F9 button; the newest clip
  is already loaded, so the usual flow needs no click on the list. Below
  600 px everything stacks and the timeline handles grow to touch size.
- Every other page is one column, `--max-w` wide (720 px), centred.
- Exactly one primary button per page: Share, Next, Save, Sign in.
- Nothing scrolls horizontally at any width; long paths and links wrap or
  get a copy button.

## Components

Listed in `components.html`. The ones worth knowing by name:

- **Check row** - the one component for live checks in the wizard, for the
  diagnostics list and for the OBS page. States: idle, waiting, ok, warn,
  problem (with the fix and, where it applies, the exact OBS menu path),
  skipped.
- **Card** - integrations are cards: the switch in the head, the body
  hidden while the integration is off. Another integration is one more card.
- **Result box** - four variants: links (storage on), local (no storage:
  Open folder / Copy file), partial (storage ok, notify failed), error.
- **Progress** - stages only for the integrations that are on, as the API
  contract says: one stage in local mode, three with Nextcloud and Discord.

## Icons

### App and tray icon

`icons/icon.svg` is the app icon (motif A: play triangle with two cut
marks, amber on a dark rounded tile so it reads on both task bar colours).
`icon-busy.svg` and `icon-error.svg` add an amber or a red dot for the tray
states "job running" and "last job failed". The `-small.svg` variants have
thicker marks and are used for the 16 and 20 px entries, where the normal
marks would vanish.

The `.ico` files are rendered, never edited by hand:

```
cd docs/design/icons/mkico
cargo run --release -- ../../../../crates/replaycut/assets
```

Without an argument the output lands in `icons/out/` together with a PNG
per size and `sheet.png`, the small sizes on a dark and a light bar for a
quick look. Each `.ico` has 16, 20, 24, 32, 48 and 256 px (Windows uses 20
and 24 for the task bar at 125 % and 150 %). `mkico` is its own tiny cargo
project, not a workspace member, so the service keeps its dependency list. The 256 px entry is PNG-compressed as Windows has read it since
Vista; the sizes below are plain BMP. GDI+ readers such as `System.Drawing.Icon`
ignore the PNG entry and fall back to 48 px, which is fine.

The icon SVGs carry the wardogs colours as literals on purpose: they are the
brand, not themed.

### Favicon

Inline in the HTML head, no extra file:

```html
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 64 64'%3E%3Crect width='64' height='64' rx='14' fill='%230b0e12'/%3E%3Cpath d='M22 16v32l26-16z' fill='%23f2b632'/%3E%3Cpath d='M12 24h7M12 40h7' stroke='%23f2b632' stroke-width='5' stroke-linecap='round'/%3E%3C/svg%3E">
```

### UI icons

An inline SVG sprite at the top of the body (`components.html` has the
reference copy): 20 px grid, stroke 1.75, `currentColor`, 25 symbols. No
icon font, no external file.

## Toasts

Two kinds, never the same message in both.

**In-page toasts** (bottom right, bottom centre on a phone, 3 s, at most
three stacked) confirm something the page itself did: link copied, title
saved, deleted, F9 sent.

**Windows toasts** reach the user while the game is in front. Only events
that matter there:

| Event | Title | Text | Click opens |
|---|---|---|---|
| OBS wrote a replay | Clip saved | `<clip name> · <m:ss>` | the UI with the clip loaded |
| Share done, storage on | Clip shared, link copied | `<size> MB, <s> s · Discord: posted` or `· Discord: failed` | the UI, result box |
| Share done, no storage | Clip ready | `<size> MB, <s> s · in shared\` | the UI, result box (Open folder / Copy file) |
| Share failed | Share failed | the error, one line | the UI, result box |
| Replay buffer stopped (OBS integration) | Replay buffer stopped | `F9 will do nothing until it runs again` | the OBS page |
| Update installed (one-click update) | Update installed | `replaycut <version> is running` | What's new |

The share result deliberately appears in both places: in the browser for
whoever is watching, on the desktop for whoever is already back in the game.

Differences to `crates/replaycut/src/toast.rs` today, for a small follow-up
commit:

- Clip saved: the text currently ends with `Trim it at <url>`. The click
  already opens that URL; drop the sentence and show the duration as `m:ss`.
- Clip ready: the text currently starts with the file name, which overflows
  the toast. Show size and seconds, and `in shared\`.
- Discord status: keep "posted" / "post failed" as short words after the
  size and duration.

## Tray menu (full tray, R8)

Order and wording, top to bottom:

1. **Open** - the UI in the default browser
2. **Copy address** - `http://<hostname>:<port>/` to the clipboard
3. **Show QR code** - opens the Access section of Settings in the browser; no native window
4. **Pause scanning** - check mark while paused
5. **Check for updates**
6. **Open log folder**
7. **Quit**

No icons in the menu. Tooltip stays "replaycut - <n> clips" or
"replaycut - sharing ... <percent> %". Icon state: normal, busy (amber dot)
while a share runs, error (red dot) after a failed share until the next
successful one.

## Acceptance checks

- No colour literal outside `tokens.css`, `themes/*.css` and `icons/`:
  `grep -rEn '#[0-9a-fA-F]{3,8}\b|rgba?\([0-9]' docs/design/mockups docs/design/base.css docs/design/components.html`
  finds only anchors (`href="#..."`).
- The contrast table in `components.html` shows "pass" in both themes.
- Every mockup renders at 375, 700 and 1400 px without a horizontal scroll bar.
- The `.ico` files come out of `mkico`, not an editor.
