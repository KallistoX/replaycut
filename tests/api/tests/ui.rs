//! Contract test for the page the service serves at `/`.
//!
//! `contract.rs` already checks that `/` answers with HTML; this file checks
//! that the page the service actually ships is the UI - with the viewport meta
//! tag phones need and the handful of ids the rest of the UI hangs off. It
//! caught nothing back then because it did not exist: 2.7.0 went out with a
//! mangled viewport tag.
//!
//! Run against a live service:
//!   BASE_URL=http://localhost:8424 CLIP_DIR=<folder the service scans> cargo test -p replaycut-api-tests
//!
//! Only a GET, so this needs no turn in the serial queue of `contract.rs`.

use replaycut_api_tests::*;

/// The 1.x UI is a different page with different ids, so this case needs a 2.1
/// service. Against 1.4.1 it prints "skipped" and passes, like the gates in
/// `contract.rs` - deliberately a copy, so the two files stay independent.
fn since_21() -> bool {
    let (status, v) = get_json("/api/clips");
    assert_eq!(status, 200, "GET /api/clips: {v}");
    let version = v["config"]["version"].as_str().unwrap_or("0").to_string();
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let ok = (major, minor) >= (2, 1);
    if !ok {
        eprintln!("skipped: needs replaycut 2.1, service is {version}");
    }
    ok
}

#[test]
fn u01_ui_page_has_viewport_and_hooks() {
    if !since_21() {
        return;
    }
    let resp = get("/");
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("text/html"), "content-type {ct}");
    let body = resp.text().unwrap();

    let viewports = body.matches("name=\"viewport\"").count();
    assert_eq!(
        viewports, 1,
        "expected one viewport meta tag, found {viewports}"
    );
    assert!(
        body.contains("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"),
        "the viewport meta tag is not the one phones need"
    );

    // The player, the share row with its selects, the quota badge and, since
    // 3.0, the cut list and the filter above the clips: without these the
    // page loads and does nothing. (`hist`, the "Shared" list until 2.8, is
    // gone - its entries live on the Activity page and under their cut.)
    for id in [
        "v",
        "bShare",
        "bCut",
        "mode",
        "frame",
        "after",
        "quota",
        "cuts",
        "clipFilter",
    ] {
        assert!(
            body.contains(&format!("id=\"{id}\"")),
            "the page has no element with id=\"{id}\""
        );
    }
}
