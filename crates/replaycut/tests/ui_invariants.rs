//! Invariants of the single-page UI (`ui/index.html`).
//!
//! The UI is one static file with no build step and no tests of its own, so a
//! careless edit can break it silently: 2.7.0 shipped with a mangled viewport
//! meta tag and with limit fields that were sent as text instead of numbers.
//! These checks are string searches over the file and cover what the service
//! and the settings API rely on; only the script itself goes through a
//! JavaScript parser (`oxc`, a dev-dependency), so it loads at all.
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

/// The other direction for the limits: every target the settings give
/// limits to has both fields in the page. Until 3.8 "File only" had none in
/// the settings either, and a share to it was always a best-quality render -
/// a field that exists in the settings but not in the page is the same gap.
#[test]
fn every_target_limit_is_in_the_page() {
    let html = ui();
    let bound: BTreeSet<String> = bound_fields(&html).into_iter().map(|(_, f)| f).collect();
    let limits: Vec<String> = default_paths()
        .into_keys()
        .filter(|p| {
            p.starts_with("integrations.") && (p.ends_with(".maxHeight") || p.ends_with(".maxKbps"))
        })
        .collect();
    assert!(
        limits.contains(&"integrations.file.maxHeight".to_string())
            && limits.contains(&"integrations.file.maxKbps".to_string()),
        "Settings::default() lost the limits of File only: {limits:?}"
    );
    let missing: Vec<&String> = limits.iter().filter(|p| !bound.contains(*p)).collect();
    assert!(
        missing.is_empty(),
        "the settings take these limits but the page offers no field for them: {missing:?}"
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

/// The script of the page, the text between `<script>` and `</script>`.
fn scripts(html: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = html[at..].find("<script>") {
        let start = at + i + "<script>".len();
        let end = start
            + html[start..]
                .find("</script>")
                .expect("a <script> without </script>");
        out.push((start, &html[start..end]));
        at = end;
    }
    out
}

/// The script parses, and the early errors a browser raises before running
/// a single line are not there either - a name declared twice, a `return`
/// outside a function, an `else` without its `if`. Any of them leaves the
/// page blank without a test noticing; in the session of 2026-09-13 both a
/// second `const seen` and an `else` after a line put between it and its `if`
/// only showed up in the browser console.
#[test]
fn the_script_parses_without_early_errors() {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    let html = ui();
    let found = scripts(&html);
    assert!(!found.is_empty(), "the page has no inline script");
    let mut problems = Vec::new();
    for (offset, source) in found {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, SourceType::cjs()).parse();
        let mut errors: Vec<_> = parsed.errors;
        if !parsed.panicked {
            let semantic = SemanticBuilder::new()
                .with_check_syntax_error(true)
                .build(&parsed.program);
            errors.extend(semantic.errors);
        }
        for e in errors {
            let at = e
                .labels
                .as_ref()
                .and_then(|l| l.first())
                .map(|l| offset + l.offset())
                .unwrap_or(offset);
            problems.push(format!("line {}: {e}", line_of(&html, at)));
        }
    }
    assert!(
        problems.is_empty(),
        "the page's script does not load:\n  {}",
        problems.join("\n  ")
    );
}

/// The job the page watches is the job the progress bar shows, so only a job
/// that runs may be watched. Issue 29 was every place that starts a job
/// calling `watchJob` with the job it had just queued: the bar fell to 0 %
/// behind "Queued" while the running job went on out of sight, and its
/// result card never came. A place that starts a job hands the answer to
/// `followJob`, which watches a job that runs at once and leaves a waiting
/// one to the queue line; the poll attaches to the job the state document
/// names as running. Nothing else calls `watchJob`.
#[test]
fn only_a_running_job_takes_the_progress_bar() {
    let html = ui();
    let follow = html
        .find("function followJob(")
        .expect("followJob is gone - where do started jobs go now?");
    let follow_end = follow
        + html[follow..]
            .find("\n}")
            .expect("followJob has no closing brace at the start of a line");
    let mut stray = Vec::new();
    let mut at = 0;
    while let Some(i) = html[at..].find("watchJob(") {
        let pos = at + i;
        at = pos + 1;
        let defined = html[..pos].ends_with("function ");
        let from_poll = html[pos..].starts_with("watchJob(d.job)");
        let from_follow = pos > follow && pos < follow_end;
        if !(defined || from_poll || from_follow) {
            let line = html[pos..].lines().next().unwrap_or_default();
            stray.push(format!(
                "line {}: {}",
                line_of(&html, pos),
                line.chars().take(80).collect::<String>()
            ));
        }
    }
    assert!(
        stray.is_empty(),
        "these calls watch a job directly - one that waits would take the progress \
         bar from the running job (hand the answer to followJob instead):\n  {}",
        stray.join("\n  ")
    );
}

/// The keys of one table of the clips page, `const NAME = { ... };` with one
/// entry per line: what stands before the first colon, unquoted. `None`
/// when the page has no such table.
fn table_keys(html: &str, name: &str) -> Option<Vec<String>> {
    let start = html.find(&format!("const {name} = {{"))?;
    let body = &html[start..];
    let body = &body[..body.find("\n};")?];
    Some(
        body.lines()
            .skip(1)
            .filter_map(|line| {
                let (key, _) = line.trim_start().split_once(':')?;
                let key = key.trim();
                let key = key
                    .strip_prefix('\'')
                    .and_then(|k| k.strip_suffix('\''))
                    .unwrap_or(key);
                Some(key.to_string())
            })
            .collect(),
    )
}

/// Every letter the clips page answers to is in the list behind `?`, and no
/// letter does two things in one mode. Since 3.12 the page has two modes -
/// cutting a recording, and the workshop for a cut - and each keeps its keys
/// in a table of its own (`CUT_KEYS`, `WORK_KEYS`): `I` sets a mark in one
/// and the start of a line in the other, which is fine, but a letter twice
/// in one table is legal JavaScript that silently keeps only the last entry.
/// `D` (Mark done / Bring back) came in 3.9.1 next to `I`, `O` and `P`.
#[test]
fn every_letter_shortcut_is_listed_once_per_mode() {
    let html = ui();
    let help_at = html
        .find("id=\"helpModal\"")
        .expect("the keyboard shortcuts dialog is gone");
    let help = &html[help_at..help_at + html[help_at..].find("</table>").unwrap_or(0)];
    // no letter is handled beside the tables, where a second meaning could hide
    const NEEDLE: &str = "k === '";
    let mut at = 0;
    while let Some(i) = html[at..].find(NEEDLE) {
        let start = at + i + NEEDLE.len();
        at = start;
        let mut chars = html[start..].chars();
        if let (Some(c), Some('\'')) = (chars.next(), chars.next()) {
            assert!(
                !c.is_ascii_alphabetic(),
                "the letter {c:?} is handled outside the key tables - put it into CUT_KEYS or WORK_KEYS"
            );
        }
    }
    let modes: [(&str, &[char]); 2] = [
        ("CUT_KEYS", &['i', 'o', 'p', 'd', 'c', 'w']),
        ("WORK_KEYS", &['i', 'o', 'p', 's', 'm', 'n', 'w']),
    ];
    for (table, must) in modes {
        let Some(keys) = table_keys(&html, table) else {
            assert!(
                must.is_empty(),
                "the clips page has no {table} any more - where do its keys live now?"
            );
            continue;
        };
        let mut letters: BTreeMap<char, usize> = BTreeMap::new();
        for key in &keys {
            let mut chars = key.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                if c.is_ascii_alphabetic() {
                    assert!(
                        c.is_ascii_lowercase(),
                        "{table} writes {c:?} in upper case - the handler looks letters up in lower case"
                    );
                    *letters.entry(c).or_default() += 1;
                }
            }
        }
        for m in must {
            assert!(
                letters.contains_key(m),
                "{table} no longer answers to {m:?} - update this test and the list behind ?"
            );
        }
        let twice: Vec<char> = letters
            .iter()
            .filter(|(_, n)| **n > 1)
            .map(|(c, _)| *c)
            .collect();
        assert!(
            twice.is_empty(),
            "{table} has these letters twice - only the last entry would ever run: {twice:?}"
        );
        let unlisted: Vec<char> = letters
            .keys()
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| !help.contains(&format!("<kbd>{c}</kbd>")))
            .collect();
        assert!(
            unlisted.is_empty(),
            "these shortcuts of {table} work but the list behind ? does not name them: {unlisted:?}"
        );
    }
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
/// The body of `function NAME(` up to its closing brace at the start of a line.
fn function_body<'a>(html: &'a str, name: &str) -> &'a str {
    let start = html
        .find(&format!("function {name}("))
        .unwrap_or_else(|| panic!("function {name} is gone"));
    let end = start
        + html[start..]
            .find("\n}")
            .unwrap_or_else(|| panic!("{name} has no closing brace at the start of a line"));
    &html[start..end]
}

/// Where the function that encloses `at` starts, and its name.
fn enclosing_function(html: &str, at: usize) -> &str {
    let start = html[..at].rfind("\nfunction ").map_or(0, |i| i + 10);
    let name_end = html[start..].find('(').map_or(start, |i| start + i);
    &html[start..name_end]
}

/// Only one player holds a source (3.14.1). Both players of the clips page
/// play the same preview when a cut is in the workshop, and in Firefox two
/// `<video>` elements on one address share a media cache: the paused player
/// of cutting mode kept its read-ahead from the start of a 2 GB file in it,
/// and the cut near the end was throttled until its picture stood. So the
/// workshop takes the source from `#v` when it opens and gives it back when
/// it closes - after `#wv` let go of it - and nothing else hands `#v` a
/// source while the workshop is open.
#[test]
fn only_one_player_holds_a_source() {
    let html = ui();
    let enter = function_body(&html, "enterWorkshop");
    assert!(
        enter.contains(" v.removeAttribute('src')"),
        "enterWorkshop must take the source from #v - the two players would share it"
    );
    let leave = function_body(&html, "leaveWorkshop");
    let (wv_off, v_on) = (
        leave.find("wv.removeAttribute('src')"),
        leave.find(" v.src = "),
    );
    assert!(
        matches!((wv_off, v_on), (Some(off), Some(on)) if off < on),
        "leaveWorkshop gives #v the recording back only after #wv let go of it"
    );
    // Every other place that points #v somewhere: `load` (it leaves the
    // workshop first), and a switch of a source #v already holds.
    let mut stray = Vec::new();
    let mut at = 0;
    while let Some(i) = html[at..].find("v.src = ") {
        let pos = at + i;
        at = pos + 1;
        if html[..pos].ends_with('w') {
            continue;
        }
        let line_start = html[..pos].rfind('\n').map_or(0, |i| i + 1);
        let line = html[line_start..].lines().next().unwrap_or_default();
        let ok = match enclosing_function(&html, pos) {
            "load" | "leaveWorkshop" => true,
            _ => line.contains("if (v.getAttribute('src') && "),
        };
        if !ok {
            stray.push(format!(
                "line {}: {}",
                line_of(&html, pos),
                line.trim().chars().take(80).collect::<String>()
            ));
        }
    }
    assert!(
        stray.is_empty(),
        "these give #v a source without asking whether it holds one - with the \
         workshop open, both players would hold the recording:\n  {}",
        stray.join("\n  ")
    );
}

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

/// The fast path keeps its elements where they were (since 3.12). The
/// workshop for cuts is a container of its own beside `#editorBody`, so
/// every element of cutting mode stays inside `#editorBody`, and the
/// progress bar and the result card - the two things both modes share -
/// sit in the job strip under the top bar (since 3.13). Moving one of
/// these into the workshop, or the workshop into the tools of cutting
/// mode, is what this is here to catch.
#[test]
fn the_fast_path_keeps_its_ids() {
    let html = ui();
    let at = |id: &str| {
        html.find(&format!(" id=\"{id}\""))
            .unwrap_or_else(|| panic!("#{id} is gone from the page"))
    };
    let (body, workshop) = (at("editorBody"), at("workshop"));
    assert!(body < workshop, "#workshop comes after #editorBody");
    for id in [
        "title",
        "bDone",
        "bDel",
        "v",
        "tl",
        "sel",
        "hin",
        "hout",
        "bIn",
        "bOut",
        "bAll",
        "bPrev",
        "bShare",
        "bShareMenu",
        "bCut",
        "selInfo",
        "planInfo",
    ] {
        let p = at(id);
        assert!(
            p > body && p < workshop,
            "#{id} belongs to the tools of cutting mode, inside #editorBody"
        );
    }
    for id in ["wv", "wtl", "cutTitle", "bRender", "outputs"] {
        assert!(at(id) > workshop, "#{id} belongs to the workshop");
    }
    let (jobs, prog, result, clips) = (at("jobs"), at("prog"), at("result"), at("page-clips"));
    assert!(
        jobs < prog && prog < result && result < clips,
        "#prog and #result sit in the job strip before the clips page, \
         outside both modes, the progress bar first"
    );
}
