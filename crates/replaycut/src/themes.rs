//! The themes that ship inside the executable.
//!
//! A theme is a CSS file that sets design tokens and nothing else; the format
//! and the contrast targets are in docs/themes.md. `wardogs` is the default
//! and lives in `ui/index.html`, so it has no file here. A file of the same
//! name in `<data-dir>/themes/` wins over a built-in one, which is how a user
//! copies a shipped theme and changes it.
//!
//! The eleven themes next to `plain` were contributed by almighty-atlas
//! (issue #11); each file names the palette it follows.

/// The theme built into the UI. It is always offered and has no file.
pub const BUILT_IN_THEME: &str = "wardogs";

/// Name and CSS of every theme in the executable, sorted by name.
pub const BUILT_IN_THEMES: [(&str, &str); 12] = [
    (
        "catppuccin-frappe",
        include_str!("../assets/themes/catppuccin-frappe.css"),
    ),
    (
        "catppuccin-latte",
        include_str!("../assets/themes/catppuccin-latte.css"),
    ),
    (
        "catppuccin-macchiato",
        include_str!("../assets/themes/catppuccin-macchiato.css"),
    ),
    (
        "catppuccin-mocha",
        include_str!("../assets/themes/catppuccin-mocha.css"),
    ),
    ("dracula", include_str!("../assets/themes/dracula.css")),
    (
        "gruvbox-dark",
        include_str!("../assets/themes/gruvbox-dark.css"),
    ),
    (
        "material-dark",
        include_str!("../assets/themes/material-dark.css"),
    ),
    (
        "material-light",
        include_str!("../assets/themes/material-light.css"),
    ),
    ("nord", include_str!("../assets/themes/nord.css")),
    ("plain", include_str!("../assets/themes/plain.css")),
    (
        "solarized-dark",
        include_str!("../assets/themes/solarized-dark.css"),
    ),
    (
        "tokyo-night",
        include_str!("../assets/themes/tokyo-night.css"),
    ),
];

/// The CSS of the built-in theme with that name, if there is one.
pub fn built_in(name: &str) -> Option<&'static str> {
    BUILT_IN_THEMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, css)| *css)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::is_theme_name;

    /// The contrast targets of docs/themes.md: foreground, background and the
    /// WCAG ratio the pair needs.
    const PAIRS: [(&str, &str, f64); 15] = [
        ("fg", "bg", 4.5),
        ("fg", "surface", 4.5),
        ("fg", "surface-2", 4.5),
        ("fg-muted", "surface", 3.0),
        ("fg-muted", "bg", 3.0),
        ("fg-faint", "surface-2", 3.0),
        ("accent-fg", "accent", 4.5),
        ("accent", "surface", 3.0),
        ("ok", "surface", 3.0),
        ("warn", "surface", 3.0),
        ("err", "surface", 3.0),
        ("info", "surface", 3.0),
        ("focus", "bg", 3.0),
        ("focus", "surface", 3.0),
        ("line", "surface", 1.3),
    ];

    /// The value of `--<token>` in a theme file. Themes are plain
    /// `--token: value;` lines, so a line scan is enough.
    fn token<'a>(css: &'a str, name: &str) -> Option<&'a str> {
        let needle = format!("--{name}:");
        css.lines().find_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix(&needle)?;
            Some(rest.trim_end_matches(';').trim())
        })
    }

    fn rgb(value: &str) -> Option<[f64; 3]> {
        let hex = value.strip_prefix('#')?;
        let byte = |s: &str| u8::from_str_radix(s, 16).ok().map(f64::from);
        match hex.len() {
            3 => {
                let d: Vec<_> = hex.chars().map(|c| format!("{c}{c}")).collect();
                Some([byte(&d[0])?, byte(&d[1])?, byte(&d[2])?])
            }
            6 => Some([byte(&hex[0..2])?, byte(&hex[2..4])?, byte(&hex[4..6])?]),
            _ => None,
        }
    }

    /// WCAG relative luminance.
    fn luminance(c: [f64; 3]) -> f64 {
        let lin = |v: f64| {
            let v = v / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
    }

    fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn the_shipped_themes_are_named_like_their_files() {
        let mut names: Vec<_> = BUILT_IN_THEMES.iter().map(|(n, _)| *n).collect();
        let sorted = {
            let mut s = names.clone();
            s.sort_unstable();
            s
        };
        assert_eq!(names, sorted, "BUILT_IN_THEMES is sorted by name");
        names.dedup();
        assert_eq!(names.len(), BUILT_IN_THEMES.len(), "no name twice");
        for name in names {
            assert!(is_theme_name(name), "{name} is a usable theme name");
            assert!(built_in(name).is_some(), "{name} is found by name");
        }
        assert!(built_in(BUILT_IN_THEME).is_none(), "wardogs has no file");
        assert!(built_in("nope").is_none());
    }

    /// docs/themes.md: only `:root { --token: value; }`, nothing external.
    #[test]
    fn the_shipped_themes_only_set_tokens() {
        for (name, css) in BUILT_IN_THEMES {
            assert!(!css.contains("@import"), "{name} imports");
            assert!(!css.contains("url("), "{name} loads a URL");
            let body = css.split_once('{').expect("{name} has a rule").1;
            for line in body.lines().map(str::trim) {
                let code = line.split("/*").next().unwrap_or("").trim();
                if code.is_empty() || code == "}" {
                    continue;
                }
                assert!(code.starts_with("--"), "{name}: {line}");
            }
        }
    }

    /// The contrast targets of docs/themes.md, for every shipped theme.
    #[test]
    fn the_shipped_themes_meet_the_contrast_targets() {
        let mut failed = Vec::new();
        for (name, css) in BUILT_IN_THEMES {
            for (fg, bg, needs) in PAIRS {
                let colour = |t: &str| {
                    let v = token(css, t).unwrap_or_else(|| panic!("{name} sets --{t}"));
                    rgb(v).unwrap_or_else(|| panic!("{name}: --{t} is {v}, not a hex colour"))
                };
                let ratio = contrast(colour(fg), colour(bg));
                if ratio < needs {
                    failed.push(format!(
                        "{name}: --{fg} on --{bg} is {ratio:.2}, needs {needs}"
                    ));
                }
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }
}
