# Themes

The web UI is drawn entirely from a small set of CSS custom properties, the
design tokens. A theme is a CSS file that sets those tokens to other values.
It never contains selectors or markup, so a theme cannot break a page; it
can only recolour it.

The default theme is `wardogs`: dark, amber accent. It is built into the UI.
A second theme, `plain` (light, blue accent), ships as an example in
`docs/design/themes/plain.css`.

## Where themes live and how they are chosen

| Item | Location |
|---|---|
| Built-in default | `docs/design/tokens.css` in the repository, embedded in `ui/index.html` |
| Your themes | `%LOCALAPPDATA%\replaycut\themes\<name>.css` (`<data-dir>\themes\`) |
| Selection | Settings › General › Theme, or `"theme": "<name>"` in `settings.json` |

The service serves `GET /themes/<name>.css` from that folder and the UI
loads it after the built-in tokens. Loading order is the contract: `wardogs`
is always loaded first, the chosen theme second, so a theme may set only the
tokens it wants to change and inherits the rest. A missing or unreadable
theme file falls back to `wardogs` with a warning in the log.

Theme selection and the `/themes/` route arrive with the settings page; until
then the built-in theme is used.

## Writing a theme

Copy `docs/design/themes/plain.css`, rename it, change values. Rules:

- Only `:root { --token: value; }`. Nothing else is read.
- Colours can be any CSS colour. The `-soft` tokens are translucent tints
  and are usually `rgba(<accent or state colour>, 0.12 - 0.22)`.
- Keep the contrast targets below; the component sheet
  (`docs/design/components.html`, theme switcher top right) computes them
  live for your file, so open it with your theme and look at the table.
- Spacing, radius, typography and layout tokens exist so a theme *can*
  change them (a wider clip list, a larger base font), but most themes
  should leave them alone.
- No `@import`, no URLs: the UI runs offline in a LAN and loads nothing
  external.

## The tokens

### Surfaces

| Token | Meaning |
|---|---|
| `--bg` | page background |
| `--surface` | panels, cards, list rows, dialogs, the top bar |
| `--surface-2` | inputs, wells, code blocks, the timeline track; one step deeper than `--surface` |
| `--line` | borders and separators |
| `--line-strong` | borders that must stand out: hover, active rows, dialogs |
| `--scrim` | the overlay behind dialogs |

### Text

| Token | Meaning |
|---|---|
| `--fg` | primary text |
| `--fg-muted` | secondary text, meta data, labels |
| `--fg-faint` | placeholders, keyboard hints, disabled text |

### Accent and state colours

| Token | Meaning |
|---|---|
| `--accent`, `--accent-hover` | the one primary action per page, the active selection, links |
| `--accent-fg` | text on the accent colour |
| `--accent-soft` | translucent accent: the selected range on the timeline, active tints |
| `--ok`, `--ok-soft` | success |
| `--warn`, `--warn-soft` | warnings. Independent of the accent so a blue theme keeps amber warnings |
| `--err`, `--err-soft` | errors, destructive actions |
| `--info`, `--info-soft` | neutral notices (update available, new clip) |
| `--focus` | the keyboard focus ring. Chosen to differ from the accent so it stays visible on the primary button |

### Typography

| Token | Default |
|---|---|
| `--font` | `system-ui, -apple-system, "Segoe UI", Roboto, sans-serif` |
| `--font-mono` | `ui-monospace, Consolas, "Cascadia Mono", "SF Mono", monospace` - addresses, paths, diagnostics |
| `--text-xs` / `-sm` / `-base` / `-lg` / `-xl` | 11 / 12 / 14 / 16 / 20 px |
| `--leading` | 1.4 |
| `--weight-strong` | 600 |

### Spacing, radii, motion, layout

| Token | Default |
|---|---|
| `--space-1` … `--space-6` | 4 / 8 / 12 / 16 / 24 / 32 px |
| `--radius-sm` / `-md` / `-lg` / `-pill` | 4 / 6 / 10 / 999 px |
| `--duration`, `--duration-slow` | 150 ms, 400 ms (progress bar) |
| `--sidebar-w` | 280 px, the clip list on wide screens |
| `--touch` | 44 px, minimum height of anything a finger hits |
| `--max-w` | 720 px, width of single-column pages |
| `--topbar-h` | 44 px |

## Contrast targets

Measured as WCAG contrast ratios. Text needs 4.5:1, secondary text and UI
parts 3:1. The table shows the two shipped themes; the component sheet
recomputes it for any theme.

| Pair | Needs | wardogs | plain |
|---|---|---|---|
| `--fg` on `--bg` | 4.5 | 15.2 | 15.2 |
| `--fg` on `--surface` | 4.5 | 13.6 | 16.5 |
| `--fg` on `--surface-2` | 4.5 | 15.9 | 14.0 |
| `--fg-muted` on `--surface` | 3 | 5.4 | 6.0 |
| `--fg-muted` on `--bg` | 3 | 6.0 | 5.5 |
| `--fg-faint` on `--surface-2` | 3 | 3.4 | 3.2 |
| `--accent-fg` on `--accent` | 4.5 | 10.1 | 4.8 |
| `--accent` on `--surface` | 3 | 9.1 | 4.8 |
| `--ok` on `--surface` | 3 | 7.5 | 4.1 |
| `--warn` on `--surface` | 3 | 9.1 | 3.6 |
| `--err` on `--surface` | 3 | 5.2 | 5.2 |
| `--info` on `--surface` | 3 | 6.5 | 4.8 |
| `--focus` on `--bg` | 3 | 9.9 | 15.2 |
| `--focus` on `--surface` | 3 | 8.8 | 16.5 |
| `--line` on `--surface` | 1.3 | 1.4 | 1.4 |

## What a theme cannot change

Icons: the app icon, the tray icon and the UI icon sprite keep the wardogs
colours. The sprite is monochrome and follows `currentColor`, so it takes
the theme's text colours automatically; only the app and tray icons are
fixed.
