//! Invariants of the single-page UI (`ui/index.html`).
//!
//! The UI is one static file with no build step and no tests of its own, so a
//! careless edit can break it silently: 2.7.0 shipped with a mangled viewport
//! meta tag and with limit fields that were sent as text instead of numbers.
//! These checks are string searches over the file - no HTML or JS parser - and
//! cover what the service and the settings API rely on.
//!
//! The settings side pulls `Settings::default()` in from `../src/settings.rs`,
//! so the field paths are checked against the real struct, not against a copy
//! that can drift.

use std::collections::{BTreeMap, BTreeSet};

#[path = "../src/media.rs"]
#[allow(dead_code)]
mod media;
#[path = "../src/settings.rs"]
#[allow(dead_code)]
mod settings;

/// Stand-in for the one helper `settings.rs` borrows from `notify.rs`.
/// Pulling the real module in would drag half the crate along; nothing here
/// validates a webhook URL, so the body only has to compile.
mod notify {
    pub fn is_http_url(url: &str) -> bool {
        url.starts_with("https://") || url.starts_with("http://")
    }
}

use settings::Settings;

/// `data-f` paths the settings document does not carry: secrets are write-only
/// (the service reports them as `secrets.*` flags and never sends them back)
/// and `password` is sent on save but never read. None of them is bound in the
/// UI today - the list is here so a future write-only field has a home.
const NOT_IN_SETTINGS: &[&str] = &["secrets.", "password", "nextcloudUser", "nextcloudPassword"];

fn ui() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/index.html");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every `<needle>value"` in the file, with the offset the value starts at.
fn attr_values<'a>(html: &'a str, needle: &str) -> Vec<(usize, &'a str)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = html[at..].find(needle) {
        let start = at + i + needle.len();
        let end = match html[start..].find('"') {
            Some(e) => start + e,
            None => break,
        };
        out.push((start, &html[start..end]));
        at = end;
    }
    out
}

fn line_of(html: &str, at: usize) -> usize {
    html[..at].matches('\n').count() + 1
}

/// The tag that carries the attribute at `at`, e.g. `<input class=.. data-f=..>`.
fn tag_around(html: &str, at: usize) -> &str {
    let start = html[..at].rfind('<').unwrap_or(0);
    let end = html[start..]
        .find('>')
        .map(|e| start + e + 1)
        .unwrap_or(html.len());
    &html[start..end]
}

#[test]
fn viewport_meta_is_intact() {
    let html = ui();
    let hits: Vec<&str> = attr_values(&html, "<meta name=\"viewport\" content=\"")
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one viewport meta tag, found {}: {hits:?}",
        hits.len()
    );
    assert_eq!(
        hits[0], "width=device-width, initial-scale=1",
        "the viewport content changed - phones scale the page from this"
    );
}

/// Leaf paths of a JSON document, in dotted form (`integrations.s3.bucket`).
fn leaf_paths(
    value: &serde_json::Value,
    prefix: &str,
    out: &mut BTreeMap<String, serde_json::Value>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                leaf_paths(v, &path, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), other.clone());
        }
    }
}

fn default_paths() -> BTreeMap<String, serde_json::Value> {
    let value = serde_json::to_value(Settings::default()).expect("serialise the defaults");
    let mut out = BTreeMap::new();
    leaf_paths(&value, "", &mut out);
    out
}

/// Every `data-f` the markup binds statically; the one template literal
/// (`data-f="${f}"`) is built at run time from paths that appear elsewhere.
fn bound_fields(html: &str) -> Vec<(usize, String)> {
    attr_values(html, "data-f=\"")
        .into_iter()
        .filter(|(_, f)| !f.contains("${"))
        .map(|(at, f)| (at, f.to_string()))
        .collect()
}

#[test]
fn every_bound_field_is_a_settings_path() {
    let html = ui();
    let defaults = default_paths();
    let mut missing = Vec::new();
    for (at, field) in bound_fields(&html) {
        if NOT_IN_SETTINGS.iter().any(|p| field.starts_with(p)) {
            continue;
        }
        if !defaults.contains_key(&field) {
            missing.push(format!("line {}: data-f=\"{field}\"", line_of(&html, at)));
        }
    }
    assert!(
        missing.is_empty(),
        "these fields have no path in Settings::default() (a typo, a renamed \
         field, or a write-only field that belongs in NOT_IN_SETTINGS):\n  {}",
        missing.join("\n  ")
    );
}

/// Existing in `Settings` is not enough: the page also has to be able to save
/// the field, and `PUT /api/settings` refuses every name that is not in
/// `PATCH_KEYS`. 3.4.0 shipped `https` in the struct and in the page but not
/// in that list, so the HTTPS switch answered "unknown field: https" and the
/// one feature of the release could not be turned on from the UI at all.
#[test]
fn every_bound_field_can_be_saved() {
    let html = ui();
    let mut missing = Vec::new();
    for (at, field) in bound_fields(&html) {
        if NOT_IN_SETTINGS.iter().any(|p| field.starts_with(p)) {
            continue;
        }
        if !settings::patch_accepts(&field) {
            missing.push(format!("line {}: data-f=\"{field}\"", line_of(&html, at)));
        }
    }
    assert!(
        missing.is_empty(),
        "these fields are bound in the page but PUT /api/settings would refuse \
         them - add the name to PATCH_KEYS, or to the field list of its group:\n  {}",
        missing.join("\n  ")
    );
}

/// The alternatives of the regex `readField` uses to decide "this is a number".
/// Reading it out of the file keeps the test honest when the list grows.
fn read_field_number_hints(html: &str) -> Vec<String> {
    const MARK: &str = ".test(el.dataset.f)";
    let at = html
        .find(MARK)
        .expect("readField no longer tests el.dataset.f");
    let before = &html[..at];
    let close = before.rfind('/').expect("no regex literal before the test");
    let open = before[..close]
        .rfind('/')
        .expect("no regex literal before the test");
    let pattern = &before[open + 1..close];
    assert!(
        !pattern.is_empty()
            && pattern
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '|'),
        "readField's regex is no longer a plain alternation of words ({pattern:?}) - \
         teach this test the new shape"
    );
    pattern.split('|').map(str::to_string).collect()
}

#[test]
fn numeric_fields_are_read_as_numbers() {
    let html = ui();
    let defaults = default_paths();
    let hints = read_field_number_hints(&html);
    // `changes()` sends what `readField` returns, and the service rejects a
    // string where a number belongs. A numeric field therefore has to be an
    // <input type="number"> or match one of the hints above.
    let mut bad = Vec::new();
    for (at, field) in bound_fields(&html) {
        if !matches!(defaults.get(&field), Some(serde_json::Value::Number(_))) {
            continue;
        }
        let tag = tag_around(&html, at);
        let numeric = tag.contains("type=\"number\"") || hints.iter().any(|h| field.contains(h));
        if !numeric {
            bad.push(format!("line {}: data-f=\"{field}\"", line_of(&html, at)));
        }
    }
    assert!(
        bad.is_empty(),
        "these fields have a numeric default but readField sends them as text \
         (give the element type=\"number\" or add a word to readField's regex):\n  {}",
        bad.join("\n  ")
    );
}

/// Ids the script looks up by hand: `$('x')` and `getElementById('x')`.
fn referenced_ids(html: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for needle in ["$('", "getElementById('"] {
        let mut at = 0;
        while let Some(i) = html[at..].find(needle) {
            let start = at + i + needle.len();
            let end = match html[start..].find('\'') {
                Some(e) => start + e,
                None => break,
            };
            let id = &html[start..end];
            // Only a complete literal counts: `$('page-' + p)` builds its id
            // at run time, and `$('#x .y')` is a selector, not an id.
            let closed = html[end + 1..].trim_start().starts_with(')');
            if closed
                && !id.is_empty()
                && !id.contains(|c: char| c.is_whitespace() || "#.[]<>,:*".contains(c))
            {
                out.insert(id.to_string());
            }
            at = end;
        }
    }
    out
}

#[test]
fn every_id_the_script_looks_up_exists() {
    let html = ui();
    // Ids inside template literals count as defined: the script renders them
    // into the page before it looks them up. Only `${...}` ids are skipped,
    // and no lookup depends on one today.
    let defined: BTreeSet<&str> = attr_values(&html, " id=\"")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| !v.contains("${"))
        .collect();
    let ids = referenced_ids(&html);
    let missing: Vec<&String> = ids
        .iter()
        .filter(|id| !defined.contains(id.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "the script looks up ids the markup never defines (a rename, or an \
         element built at run time - then note it here): {missing:?}"
    );
}

#[test]
fn every_icon_reference_has_a_symbol() {
    let html = ui();
    let symbols: BTreeSet<&str> = attr_values(&html, "<symbol id=\"")
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    let mut missing = Vec::new();
    for (at, href) in attr_values(&html, "<use href=\"#") {
        if href.contains("${") {
            continue;
        }
        if !symbols.contains(href) {
            missing.push(format!(
                "line {}: <use href=\"#{href}\">",
                line_of(&html, at)
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "these icons have no <symbol> and render as nothing:\n  {}",
        missing.join("\n  ")
    );
}

/// Banner buttons that only one page wires, with the reason. The banner they
/// sit in is raised by that page alone, so the button cannot be reached from
/// anywhere else; anything not listed here has to work on every page.
const BANNER_BUTTONS_OF_ONE_PAGE: &[(&str, &str)] = &[(
    "b-restart-now",
    "the \"Restart needed\" banner is raised by the settings page alone, and \
     the address to come back to is built from the settings it just saved",
)];

/// Every button of the banner strip is wired.
///
/// The strip is part of every page, so a button in it is on screen wherever
/// the user is. Issue 20 was exactly this: a banner whose button did nothing
/// because its handler sat in one page's init. `data-dismiss` buttons are
/// wired by a single loop over the whole document and need no handler of
/// their own.
#[test]
fn every_banner_button_is_wired() {
    let html = ui();
    let mut unwired = Vec::new();
    for (at, id) in attr_values(&html, " id=\"") {
        if !id.starts_with("b-") {
            continue;
        }
        let tag = tag_around(&html, at);
        if !tag.starts_with("<button") || tag.contains("data-dismiss") {
            continue;
        }
        if BANNER_BUTTONS_OF_ONE_PAGE.iter().any(|(b, _)| *b == id) {
            continue;
        }
        if !html.contains(&format!("$('{id}').onclick")) {
            unwired.push(format!("line {}: id=\"{id}\"", line_of(&html, at)));
        }
    }
    assert!(
        unwired.is_empty(),
        "these banner buttons have no onclick, so they do nothing wherever \
         their banner shows (wire them next to the other banner handlers, or \
         list them in BANNER_BUTTONS_OF_ONE_PAGE with the reason):\n  {}",
        unwired.join("\n  ")
    );
}
