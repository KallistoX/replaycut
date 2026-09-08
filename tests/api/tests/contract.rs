//! Black-box contract tests for the replaycut HTTP API (`docs/api.md`).
//!
//! Run against a live service:
//!   BASE_URL=http://localhost:8421 CLIP_DIR=<folder the service scans> cargo test -p replaycut-api-tests
//!
//! The service must encode for real but not upload or post anywhere
//! (1.4.1: `-DryRun`; 2.0: integrations disabled). Tests are numbered so
//! that single-threaded runs execute them in this order; the delete test
//! additionally waits for the others when the harness runs in parallel.

use std::time::Duration;

use replaycut_api_tests::*;
use serde_json::json;

const TESTS_BEFORE_DELETE: usize = 10;
const JOB_TIMEOUT: Duration = Duration::from_secs(180);

#[test]
fn t01_ui_root_served() {
    let _g = serial();
    let resp = get("/");
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("text/html"), "content-type {ct}");
    let cc = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(cc.contains("no-store"), "cache-control {cc:?}");
    let body = resp.text().unwrap();
    assert!(
        body.contains("<html") || body.contains("<!doctype") || body.contains("<!DOCTYPE"),
        "not HTML"
    );
}

#[test]
fn t02_clip_appears_within_5s() {
    let _g = serial();
    let f = fixture();
    let clip = wait_for_clip(&f.base, Duration::from_secs(5));
    eprintln!(
        "clip listed {} ms after the file was complete",
        f.ready_at.elapsed().as_millis()
    );

    assert_eq!(clip["name"], f.name.as_str());
    assert_eq!(clip["base"], f.base.as_str());
    assert!(
        clip["size"].as_u64().unwrap_or(0) > 0,
        "size: {}",
        clip["size"]
    );
    let duration = clip["duration"]
        .as_f64()
        .unwrap_or_else(|| panic!("duration: {}", clip["duration"]));
    assert!(
        (duration - FIXTURE_SECONDS).abs() <= 0.5,
        "duration {duration}"
    );
    assert_eq!(clip["tracks"], 4, "tracks: {}", clip["tracks"]);
    assert_eq!(clip["status"], "ready");
    assert_eq!(clip["preview"], format!("/media/{}.mp4", encode(&f.base)));
    assert_eq!(clip["title"], "", "a new clip has no title");
    let created = clip["created"].as_str().unwrap_or("");
    assert!(is_local_timestamp(created), "created: {created:?}");
    let path = clip["path"].as_str().unwrap_or("");
    assert!(path.ends_with(&f.name), "path: {path:?}");

    let st = state();
    let scan_at = st["scanAt"].as_str().unwrap_or("");
    assert!(is_local_timestamp(scan_at), "scanAt: {scan_at:?}");
    assert_eq!(st["busy"], false);
    assert!(st["job"].is_null(), "job: {}", st["job"]);
    assert!(st["config"]["version"].is_string(), "config.version");
    assert!(st["config"]["encoder"].is_string(), "config.encoder");
    let audio = st["config"]["audio"].as_array().expect("config.audio");
    let ids: Vec<&str> = audio.iter().filter_map(|a| a["id"].as_str()).collect();
    assert_eq!(ids, ["mix", "gamemic", "game", "gamediscord"]);
    for a in audio {
        assert!(
            a["label"].is_string() && a["need"].is_u64(),
            "audio entry: {a}"
        );
    }
}

#[test]
fn t03_preview_supports_range() {
    let _g = serial();
    let f = fixture();
    wait_for_clip(&f.base, Duration::from_secs(5));
    let path = format!("/media/{}.mp4", encode(&f.base));

    let full = get(&path);
    assert_eq!(full.status().as_u16(), 200);
    let header = |r: &reqwest::blocking::Response, k: &str| {
        r.headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    assert!(
        header(&full, "content-type").starts_with("video/mp4"),
        "content-type"
    );
    assert_eq!(header(&full, "accept-ranges"), "bytes");
    let len: usize = header(&full, "content-length")
        .parse()
        .expect("content-length");
    let body = full.bytes().unwrap();
    assert_eq!(body.len(), len);
    assert_eq!(
        &body[4..8],
        b"ftyp",
        "preview does not start with an ftyp box"
    );
    let find = |needle: &[u8]| body.windows(needle.len()).position(|w| w == needle);
    let (moov, mdat) = (find(b"moov"), find(b"mdat"));
    assert!(moov.is_some() && mdat.is_some(), "moov/mdat missing");
    assert!(moov < mdat, "faststart: moov must precede mdat");

    let part = client()
        .get(url(&path))
        .header("Range", "bytes=0-1023")
        .send()
        .unwrap();
    assert_eq!(part.status().as_u16(), 206);
    assert_eq!(
        header(&part, "content-range"),
        format!("bytes 0-1023/{len}")
    );
    assert_eq!(header(&part, "accept-ranges"), "bytes");
    let slice = part.bytes().unwrap();
    assert_eq!(slice.len(), 1024);
    assert_eq!(&slice[..], &body[..1024]);

    let tail = client()
        .get(url(&path))
        .header("Range", "bytes=-100")
        .send()
        .unwrap();
    assert_eq!(tail.status().as_u16(), 206);
    assert_eq!(
        header(&tail, "content-range"),
        format!("bytes {}-{}/{len}", len - 100, len - 1)
    );
    let slice = tail.bytes().unwrap();
    assert_eq!(&slice[..], &body[len - 100..]);

    let open = client()
        .get(url(&path))
        .header("Range", &format!("bytes={}-", len - 10))
        .send()
        .unwrap();
    assert_eq!(open.status().as_u16(), 206);
    assert_eq!(open.bytes().unwrap().len(), 10);
}

#[test]
fn t04_title_set_and_remove() {
    let _g = serial();
    let f = fixture();
    wait_for_clip(&f.base, Duration::from_secs(5));
    let path = format!("/api/clips/{}/name", encode(&f.base));

    let (status, v) = put_json(&path, &json!({ "name": "  Test\ntitle\t " }));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["base"], f.base.as_str());
    assert_eq!(
        v["title"], "Test title",
        "CR/LF/TAB become spaces, result is trimmed"
    );
    assert_eq!(find_clip(&f.base).unwrap()["title"], "Test title");

    let long = "x".repeat(81);
    let (status, v) = put_json(&path, &json!({ "name": long }));
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["title"].as_str().map(str::len),
        Some(80),
        "titles are cut to 80 characters"
    );

    let (status, v) = put_json(&path, &json!({ "name": "" }));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["title"], "");
    assert_eq!(
        find_clip(&f.base).unwrap()["title"],
        "",
        "empty name removes the title"
    );
}

#[test]
fn t05_share_dry_run_completes() {
    let _g = serial();
    let f = fixture();
    wait_for_clip(&f.base, Duration::from_secs(5));
    let name_path = format!("/api/clips/{}/name", encode(&f.base));
    let (status, _) = put_json(&name_path, &json!({ "name": "Dry run test" }));
    assert_eq!(status, 200);

    let id = share(&f.base, 2.0, 8.0, "mix");
    let (stages, job) = wait_job(&id, JOB_TIMEOUT);
    eprintln!("stages: {stages:?}");
    assert_stages_monotonic(&stages);
    assert_eq!(job["stage"], "done", "job failed: {}", job["error"]);
    assert_eq!(job["ok"], true);
    assert_eq!(
        job["error"].as_str().unwrap_or(""),
        "",
        "error is empty on success"
    );
    assert_eq!(job["id"], id.as_str());
    assert_eq!(job["base"], f.base.as_str());
    assert_eq!(job["percent"], 100);
    assert_eq!(job["start"].as_f64(), Some(2.0));
    assert_eq!(job["end"].as_f64(), Some(8.0));
    assert_eq!(job["seconds"].as_f64(), Some(6.0));
    assert_eq!(job["audio"], "mix");
    // the bitrate cap of the target; 0 since 2.7 means quality-driven
    assert!(job["kbps"].is_number(), "kbps: {}", job["kbps"]);
    assert_eq!(job["title"], "Dry run test");
    assert_eq!(job["file"], share_file_name(&f.base, 2, 8, "Dry-run-test"));
    assert!(
        job["sizeMB"].as_f64().unwrap_or(0.0) > 0.0,
        "sizeMB: {}",
        job["sizeMB"]
    );
    let link = job["link"].as_str().unwrap_or("");
    let direct = job["direct"].as_str().unwrap_or("");
    assert!(link.starts_with("http"), "link: {link:?}");
    assert_eq!(direct, format!("{link}/download"));
    let nc = job["ncPath"].as_str().unwrap_or("");
    assert!(
        nc.ends_with(job["file"].as_str().unwrap()),
        "ncPath: {nc:?}"
    );
    assert!(job["discord"].is_string(), "discord: {}", job["discord"]);
    for k in ["at", "finished"] {
        assert!(
            is_local_timestamp(job[k].as_str().unwrap_or("")),
            "{k}: {}",
            job[k]
        );
    }
    let shared = env()
        .clip_dir
        .join("shared")
        .join(job["file"].as_str().unwrap());
    assert!(
        shared.is_file(),
        "shared file missing: {}",
        shared.display()
    );

    let st = state();
    assert_eq!(st["busy"], false);
    assert!(st["job"].is_null());
    assert_eq!(st["last"]["id"], id.as_str());
    assert_eq!(st["last"]["stage"], "done");
    let hist = st["history"].as_array().expect("history");
    assert_eq!(
        hist[0]["id"],
        id.as_str(),
        "newest history entry is the job"
    );
    for k in ["percent", "stage", "ok", "error"] {
        assert!(hist[0].get(k).is_none(), "history entry must not carry {k}");
    }
    for k in [
        "base", "title", "seconds", "sizeMB", "audio", "link", "direct", "file", "finished", "at",
    ] {
        assert!(!hist[0][k].is_null(), "history entry lacks {k}");
    }

    let (status, all) = get_json("/api/history");
    assert_eq!(status, 200);
    let all = all["history"].as_array().expect("history");
    assert!(
        all.iter().any(|e| e["id"] == id.as_str()),
        "/api/history lacks the job"
    );
}

#[test]
fn t06_share_audio_modes() {
    let _g = serial();
    let f = fixture();
    wait_for_clip(&f.base, Duration::from_secs(5));
    let name_path = format!("/api/clips/{}/name", encode(&f.base));
    let (status, _) = put_json(&name_path, &json!({ "name": "" }));
    assert_eq!(status, 200);

    for (audio, start, end) in [
        ("gamemic", 10.0, 12.0),
        ("game", 12.0, 14.0),
        ("gamediscord", 14.0, 16.0),
    ] {
        let id = share(&f.base, start, end, audio);
        let (stages, job) = wait_job(&id, JOB_TIMEOUT);
        assert_stages_monotonic(&stages);
        assert_eq!(job["stage"], "done", "{audio} failed: {}", job["error"]);
        assert_eq!(job["audio"], audio);
        assert_eq!(job["seconds"].as_f64(), Some(2.0));
        assert_eq!(job["title"], "", "no title, no slug");
        assert_eq!(
            job["file"],
            share_file_name(&f.base, start as i64, end as i64, "")
        );
    }
}

#[test]
fn t07_share_rejects_bad_requests() {
    let _g = serial();
    let f = fixture();
    wait_for_clip(&f.base, Duration::from_secs(5));

    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": "no such clip", "start": 0, "end": 5 }),
    );
    assert_eq!(status, 404, "unknown clip: {v}");
    assert_eq!(v["ok"], false);
    assert!(v["error"].is_string());

    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": f.base, "start": 5, "end": 5.5, "audio": "mix" }),
    );
    assert!(
        status == 400 || status == 500,
        "selection under 1 s: {status} {v}"
    );
    assert_eq!(v["ok"], false);
    assert!(v["error"].is_string());

    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": f.base, "start": 0, "end": 5, "audio": "nope" }),
    );
    assert!(
        status == 400 || status == 500,
        "unknown audio mode: {status} {v}"
    );
    assert_eq!(v["ok"], false);

    let st = state();
    assert_eq!(
        st["busy"], false,
        "rejected requests must not occupy the job slot"
    );
    assert!(st["job"].is_null());
}

#[test]
fn t08_second_share_gets_409() {
    let _g = serial();
    let f = fixture();
    wait_for_clip(&f.base, Duration::from_secs(5));

    let first = share(&f.base, 0.0, FIXTURE_SECONDS + 5.0, "gamemic");
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": f.base, "start": 0, "end": 3, "audio": "mix" }),
    );
    // since 2.4 a second share waits in the queue instead of a 409
    let queued = if since_24() {
        assert_eq!(status, 202, "second share while busy: {v}");
        assert_eq!(v["ok"], true);
        assert_eq!(v["position"], 1, "{v}");
        Some(v["job"].as_str().unwrap_or("").to_string())
    } else {
        assert_eq!(status, 409, "second share while busy: {v}");
        assert_eq!(v["ok"], false);
        assert_eq!(v["job"], first.as_str(), "409 names the running job");
        assert!(v["error"].is_string());
        None
    };

    let st = state();
    assert_eq!(st["busy"], true);
    assert_eq!(st["job"], first.as_str());

    let (stages, job) = wait_job(&first, JOB_TIMEOUT);
    assert_stages_monotonic(&stages);
    assert_eq!(job["stage"], "done", "{}", job["error"]);
    assert_eq!(job["start"].as_f64(), Some(0.0));
    let end = job["end"].as_f64().unwrap();
    assert!(
        (end - FIXTURE_SECONDS).abs() <= 0.5,
        "end is clamped to the clip duration, got {end}"
    );
    if let Some(second) = queued {
        let (_, job) = wait_job(&second, JOB_TIMEOUT);
        assert_eq!(job["stage"], "done", "queued job: {}", job["error"]);
        assert!(
            job.get("position").is_none(),
            "position is dropped once done: {job}"
        );
    }
}

#[test]
fn t09_unknown_ids_and_paths_404() {
    let _g = serial();
    let (status, v) = get_json("/api/jobs/nope");
    assert_eq!(status, 404, "{v}");
    assert_eq!(v["ok"], false);
    assert!(v["error"].is_string());

    for path in [
        "/nope",
        "/api/nope",
        "/media/nope.mp4",
        "/api/clips/nope/other",
    ] {
        let (status, v) = get_json(path);
        assert_eq!(status, 404, "GET {path}: {v}");
    }

    let (status, _) = put_json("/api/clips/nope/name", &json!({ "name": "x" }));
    assert!(
        status == 404 || status == 500,
        "title on unknown clip: {status}"
    );
    let (status, _) = delete("/api/clips/nope");
    assert!(
        status == 404 || status == 500,
        "delete unknown clip: {status}"
    );
}

#[test]
fn t10_save_accepts_empty_body() {
    let _g = serial();
    let resp = client().post(url("/api/save")).body("").send().unwrap();
    let (status, v) = (
        resp.status().as_u16(),
        resp.json::<serde_json::Value>().unwrap(),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], true);
}

#[test]
fn t11_delete_moves_to_recycle_bin() {
    wait_for_tests(TESTS_BEFORE_DELETE);
    let _g = serial();
    let f = fixture();
    wait_for_clip(&f.base, Duration::from_secs(5));
    assert!(f.path.is_file(), "fixture vanished before delete");

    let (status, v) = delete(&format!("/api/clips/{}", encode(&f.base)));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], true);
    let recycled = v["recycled"].as_u64().unwrap_or(0);
    assert!(recycled >= 1, "recycled: {}", v["recycled"]);
    eprintln!("recycled {recycled} file(s)");
    assert_eq!(v["nextcloud"], 0, "no remote deletion without ?nextcloud=1");

    assert!(!f.path.exists(), "MKV still in the clip folder");
    let prefix = f.base.split_whitespace().collect::<Vec<_>>().join("_") + "_";
    if let Ok(entries) = std::fs::read_dir(env().clip_dir.join("shared")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(&prefix),
                "shared file left behind: {name}"
            );
        }
    }
    wait_for_clip_gone(&f.base, Duration::from_secs(5));
    let (status, _) = get_json(&format!("/media/{}.mp4", encode(&f.base)));
    assert_eq!(status, 404, "preview must be gone");
    let st = state();
    assert!(
        st["last"].is_null() || st["last"]["base"] != f.base.as_str(),
        "last still points at the clip"
    );
}

/// `config.update` (2.0): absent in 1.x, otherwise `null` or `{ version, url }`.
#[test]
fn t12_config_update_is_null_or_release() {
    let _g = serial();
    let (status, v) = get_json("/api/clips");
    assert_eq!(status, 200);
    let config = v["config"].as_object().expect("config object");
    match config.get("update") {
        None => eprintln!("config.update absent (pre-2.0 service)"),
        Some(serde_json::Value::Null) => {}
        Some(u) => {
            assert!(u["version"].is_string(), "update.version: {u}");
            assert!(u["url"].is_string(), "update.url: {u}");
        }
    }
}

// ---------------------------------------------------------------- since 2.1
//
// These cases need a 2.1 service; against 1.4.1 they print "skipped" and
// pass, so the suite stays green for both.

fn since_21() -> bool {
    let v = state()["config"]["version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let ok = (major, minor) >= (2, 1);
    if !ok {
        eprintln!("skipped: needs replaycut 2.1, service is {v}");
    }
    ok
}

#[test]
fn t13_settings_document_hides_secrets() {
    let _g = serial();
    if !since_21() {
        return;
    }
    let (status, doc) = get_json("/api/settings");
    assert_eq!(status, 200);
    if !since_27() {
        assert!(doc["shareKbps"].is_number(), "shareKbps: {doc}");
    }
    assert!(
        doc.get("passwordHash").is_none(),
        "passwordHash must never be sent"
    );
    assert!(doc["secrets"]["nextcloud"].is_boolean(), "secrets: {doc}");
    assert!(doc["passwordSet"].is_boolean());
    assert!(
        doc["themes"]
            .as_array()
            .is_some_and(|t| t.iter().any(|n| n == "wardogs")),
        "themes: {}",
        doc["themes"]
    );
    assert!(doc["restartNeeded"].is_array());
    assert_eq!(doc["version"], state()["config"]["version"]);
}

#[test]
fn t14_settings_put_validates() {
    let _g = serial();
    if !since_21() {
        return;
    }
    let (status, body) = put_json("/api/settings", &json!({ "port": 0 }));
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap_or("").contains("port"),
        "{body}"
    );
    let (status, body) = put_json("/api/settings", &json!({ "bogus": 1 }));
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown field"),
        "{body}"
    );
    let (status, body) = put_json("/api/settings", &json!({ "passwordHash": "x" }));
    assert_eq!(status, 400, "{body}");
    // a text body is not JSON
    let resp = client()
        .put(url("/api/settings"))
        .header("content-type", "text/plain")
        .body("port=1")
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 415);
}

#[test]
fn t15_settings_put_applies_bitrate() {
    let _g = serial();
    if !since_21() {
        return;
    }
    if since_27() {
        eprintln!("skipped: the global bitrate is gone since 2.7 (t42 covers the limits)");
        return;
    }
    let before = state()["config"]["shareKbps"].as_u64().unwrap_or(6000);
    let target = if before == 4000 { 4500 } else { 4000 };
    let (status, body) = put_json("/api/settings", &json!({ "shareKbps": target }));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert!(
        body["restartNeeded"]
            .as_array()
            .is_some_and(|a| a.is_empty()),
        "{body}"
    );
    assert_eq!(body["settings"]["shareKbps"], target);
    assert_eq!(
        state()["config"]["shareKbps"],
        target,
        "config reflects the change"
    );
    let (status, _) = put_json("/api/settings", &json!({ "shareKbps": before }));
    assert_eq!(status, 200);
    assert_eq!(state()["config"]["shareKbps"], before);
}

#[test]
fn t16_origin_check_refuses_cross_site_writes() {
    let _g = serial();
    if !since_21() {
        return;
    }
    let resp = client()
        .post(url("/api/save"))
        .header("origin", "http://evil.example")
        .body("")
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let body: serde_json::Value = resp.json().unwrap_or_default();
    assert_eq!(body["ok"], false);
    // same-origin and no origin both pass (the endpoint itself answers 200 in dry run)
    let host = url("")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let resp = client()
        .post(url("/api/save"))
        .header("origin", format!("http://{host}"))
        .body("")
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "same-origin");
    let (status, _) = post_json("/api/save", &json!({}));
    assert_eq!(status, 200, "no origin header");
}

#[test]
fn t17_session_on_loopback_is_authenticated() {
    let _g = serial();
    if !since_21() {
        return;
    }
    let (status, s) = get_json("/api/session");
    assert_eq!(status, 200);
    assert_eq!(s["authenticated"], true, "{s}");
    assert_eq!(s["loopback"], true, "the suite runs on this machine: {s}");
    assert!(s["passwordSet"].is_boolean());
}

#[test]
fn t18_theme_route_is_strict() {
    let _g = serial();
    if !since_21() {
        return;
    }
    assert_eq!(get("/themes/nope.css").status().as_u16(), 404);
    assert_eq!(get("/themes/..%2Fsettings.json").status().as_u16(), 404);
    assert_eq!(get("/themes/Bad%20Name.css").status().as_u16(), 404);
    assert_eq!(
        get("/themes/wardogs.css").status().as_u16(),
        404,
        "built in, no file"
    );
}

#[test]
fn t19_addresses_carry_a_qr_code() {
    let _g = serial();
    if !since_21() {
        return;
    }
    let (status, a) = get_json("/api/addresses");
    assert_eq!(status, 200);
    assert!(a["port"].is_number());
    let urls = a["urls"].as_array().cloned().unwrap_or_default();
    assert!(!urls.is_empty(), "{a}");
    assert!(urls
        .iter()
        .all(|u| u.as_str().is_some_and(|s| s.starts_with("http://"))));
    // since 2.3 a loopback-only service sends no code (it would lead a phone
    // to itself) and says so with `local`
    if a["local"] == true {
        assert_eq!(a["qrSvg"], "", "{a}");
        assert_eq!(urls.len(), 1, "{a}");
    } else {
        assert!(
            a["qrSvg"].as_str().is_some_and(|s| s.contains("<svg")),
            "qrSvg"
        );
    }
}

#[test]
fn t20_pages_serve_the_ui() {
    let _g = serial();
    if !since_21() {
        return;
    }
    for page in ["/setup", "/settings", "/diagnostics", "/login"] {
        let resp = get(page);
        assert_eq!(resp.status().as_u16(), 200, "{page}");
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(ct.starts_with("text/html"), "{page}: {ct}");
    }
}

#[test]
fn t21_local_mode_actions_on_the_last_job() {
    let _g = serial();
    if !since_21() {
        return;
    }
    let (status, _) = post_json("/api/jobs/nope/open-folder", &json!({}));
    assert_eq!(status, 404);
    let (status, _) = post_json("/api/jobs/nope/copy-file", &json!({}));
    assert_eq!(status, 404);
    // t05 left a finished job behind; in dry run the actions only log
    let last = state()["last"].clone();
    if let Some(id) = last["id"].as_str() {
        if last["ok"] == true {
            let (status, body) = post_json(&format!("/api/jobs/{id}/copy-file"), &json!({}));
            assert!(status == 200 || status == 404, "{status} {body}");
            if status == 200 {
                assert_eq!(body["ok"], true);
                assert!(
                    body["file"].as_str().is_some_and(|f| f.ends_with(".mp4")),
                    "{body}"
                );
            }
        }
    }
}

#[test]
fn t22_setup_obs_document() {
    let _g = serial();
    if !since_21() {
        return;
    }
    let (status, d) = get_json("/api/setup/obs");
    assert_eq!(status, 200);
    assert!(d["profiles"].is_array(), "{d}");
    assert!(d["watching"].as_str().is_some_and(|w| !w.is_empty()), "{d}");
    assert!(
        d["newest"].is_null() || d["newest"]["codec"].is_string(),
        "{d}"
    );
    assert!(d["otherFiles"].is_array());
    // the fixture clip (from t02) carries the video facts
    let f = fixture();
    if let Some(c) = find_clip(&f.base) {
        assert_eq!(c["codec"], "h264", "{c}");
        assert_eq!(c["width"], 1280);
        assert_eq!(c["height"], 720);
        assert_eq!(c["fps"], 30);
    }
}

#[test]
fn t23_diagnostics_list_every_check() {
    let _g = serial();
    if !since_21() {
        return;
    }
    let (status, d) = get_json("/api/diagnostics");
    assert_eq!(status, 200);
    let checks = d["checks"].as_array().cloned().unwrap_or_default();
    let ids: Vec<&str> = checks.iter().filter_map(|c| c["id"].as_str()).collect();
    for id in [
        "service",
        "update",
        "ffmpeg",
        "encoder",
        "folder",
        "scan",
        "nextcloud",
        "quota",
        "webhook",
        "obs",
        "network",
    ] {
        assert!(ids.contains(&id), "missing check {id}: {ids:?}");
    }
    for c in &checks {
        assert!(
            ["ok", "warn", "fail", "skip"].contains(&c["status"].as_str().unwrap_or("")),
            "{c}"
        );
        assert!(c["detail"].is_string(), "{c}");
    }
    let ffmpeg = checks.iter().find(|c| c["id"] == "ffmpeg").unwrap();
    assert_eq!(ffmpeg["status"], "ok", "{ffmpeg}");
    let text = d["text"].as_str().unwrap_or("");
    assert!(text.starts_with("replaycut "), "{text}");
    assert!(text.contains("settings  clipDir="), "{text}");
    assert!(
        !text.to_ascii_lowercase().contains("webhooks/"),
        "no webhook URL in the copy"
    );
}

// ---------------------------------------------------------------- since 2.2

fn since_22() -> bool {
    let v = state()["config"]["version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let ok = (major, minor) >= (2, 2);
    if !ok {
        eprintln!("skipped: needs replaycut 2.2, service is {v}");
    }
    ok
}

#[test]
fn t24_save_reports_how_it_was_sent_and_obs_config() {
    let _g = serial();
    if !since_22() {
        return;
    }
    let cfg = state()["config"].clone();
    assert!(cfg["obs"]["connected"].is_boolean(), "config.obs: {cfg}");
    assert!(cfg["obs"]["replayActive"].is_boolean());
    let (status, body) = post_json("/api/save", &json!({}));
    // without OBS: the key press path answers 200 with via=hotkey; with OBS
    // connected either 200 via obs-websocket or 409 when the buffer is off
    match status {
        200 => assert!(
            ["hotkey", "obs-websocket"].contains(&body["via"].as_str().unwrap_or("")),
            "{body}"
        ),
        409 => assert!(
            body["error"]
                .as_str()
                .unwrap_or("")
                .contains("replay buffer"),
            "{body}"
        ),
        other => panic!("unexpected status {other}: {body}"),
    }
    let (status, body) = put_json("/api/settings", &json!({ "obs": { "port": 0 } }));
    assert_eq!(status, 400, "{body}");
    let (status, body) = put_json("/api/settings", &json!({ "obs": { "bogus": 1 } }));
    assert_eq!(status, 400, "{body}");
    let (_, doc) = get_json("/api/settings");
    assert_eq!(doc["obs"]["port"], 4455);
    assert!(doc["secrets"]["obs"].is_boolean());
}

#[test]
fn t25_obs_document_and_actions_without_obs() {
    let _g = serial();
    if !since_22() {
        return;
    }
    let (status, d) = get_json("/api/obs");
    assert_eq!(status, 200);
    assert!(d["enabled"].is_boolean(), "{d}");
    assert!(d["connected"].is_boolean(), "{d}");
    assert!(d["checks"].is_array(), "{d}");
    assert_eq!(d["settings"]["port"], 4455);
    if d["connected"] == false {
        let (status, body) = post_json("/api/obs/replay-buffer/start", &json!({}));
        assert_eq!(status, 409, "{body}");
        let (status, body) = post_json("/api/obs/adopt-folder", &json!({}));
        assert_eq!(status, 409, "{body}");
    }
    let (status, body) = post_json("/api/obs/reconnect", &json!({}));
    assert_eq!(status, 200, "{body}");
}

// Since 2.3: the one-click update.

fn since_23() -> bool {
    let v = state()["config"]["version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let ok = (major, minor) >= (2, 3);
    if !ok {
        eprintln!("skipped: needs replaycut 2.3, service is {v}");
    }
    ok
}

#[test]
fn t26_update_document_and_actions_without_an_update() {
    let _g = serial();
    if !since_23() {
        return;
    }
    let (status, d) = get_json("/api/update");
    assert_eq!(status, 200);
    let phase = d["phase"].as_str().unwrap_or("");
    assert!(
        [
            "idle",
            "checking",
            "available",
            "downloading",
            "ready",
            "installing",
            "error"
        ]
        .contains(&phase),
        "{d}"
    );
    assert!(d["current"].is_string(), "{d}");
    assert!(d["installed"].is_boolean(), "{d}");
    assert!(d["checkUpdates"].is_boolean(), "{d}");
    assert!(d["percent"].is_number(), "{d}");
    assert!(d["justUpdated"].is_boolean(), "{d}");
    if d["latest"].is_object() {
        assert!(d["latest"]["version"].is_string(), "{d}");
        assert!(d["latest"]["notes"].is_string(), "{d}");
    } else {
        // nothing newer known: download and install refuse
        let (status, body) = post_json("/api/update/download", &json!({}));
        assert_eq!(status, 409, "{body}");
        let (status, body) = post_json("/api/update/install", &json!({}));
        assert_eq!(status, 409, "{body}");
    }
    let (status, body) = post_json("/api/update/seen", &json!({}));
    assert_eq!(status, 200, "{body}");
    let (_, d) = get_json("/api/update");
    assert_eq!(d["justUpdated"], false);
    // the check asks the releases API; without network it answers 502
    let (status, body) = post_json("/api/update/check", &json!({}));
    assert!(status == 200 || status == 502, "{status} {body}");
    if status == 200 {
        assert!(body["checkedAt"].is_string(), "{body}");
    }
}

#[test]
fn t27_pause_scanning_holds_a_new_clip_back() {
    let _g = serial();
    if !since_23() {
        return;
    }
    let f = fixture();
    assert_eq!(state()["config"]["scanning"]["paused"], false);
    let (status, body) = post_json("/api/scanning", &json!({ "paused": "yes" }));
    assert_eq!(status, 400, "{body}");
    let (status, body) = post_json("/api/scanning", &json!({ "paused": true }));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["paused"], true);
    assert_eq!(state()["config"]["scanning"]["paused"], true);
    // a new clip must not show up while paused
    let base = format!("{} paused", f.base);
    make_clip(&base);
    std::thread::sleep(Duration::from_secs(8));
    assert!(find_clip(&base).is_none(), "the clip appeared while paused");
    let (status, body) = post_json("/api/scanning", &json!({ "paused": false }));
    assert_eq!(status, 200, "{body}");
    assert_eq!(state()["config"]["scanning"]["paused"], false);
    let clip = wait_for_clip(&base, Duration::from_secs(20));
    assert_eq!(clip["base"], base);
    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
}

// Since 2.4: the share queue and cancelling.

fn since_24() -> bool {
    let v = state()["config"]["version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let ok = (major, minor) >= (2, 4);
    if !ok {
        eprintln!("skipped: needs replaycut 2.4, service is {v}");
    }
    ok
}

#[test]
fn t28_queue_runs_shares_in_order_and_cancel_ends_them() {
    let _g = serial();
    if !since_24() {
        return;
    }
    // the fixture is gone after t11: this test brings its own clip
    let base = format!("{} queue", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));
    let body =
        |start: f64, end: f64| json!({ "base": base, "start": start, "end": end, "audio": "mix" });

    // three shares: the first runs, the others wait with a position
    let a = share(&base, 0.0, 3.0, "mix");
    let (status, v) = post_json("/api/share", &body(3.0, 6.0));
    assert_eq!(status, 202, "{v}");
    assert_eq!(v["position"], 1, "{v}");
    let b = v["job"].as_str().unwrap_or("").to_string();
    let (status, v) = post_json("/api/share", &body(6.0, 9.0));
    assert_eq!(status, 202, "{v}");
    assert_eq!(v["position"], 2, "{v}");
    let c = v["job"].as_str().unwrap_or("").to_string();
    let st = state();
    assert_eq!(st["job"], a.as_str());
    assert_eq!(st["queue"], json!([b, c]), "{}", st["queue"]);
    let (_, jb) = get_json(&format!("/api/jobs/{b}"));
    assert_eq!(jb["stage"], "queued");
    assert_eq!(jb["position"], 1);

    // the same cut again attaches to the waiting one
    let (status, v) = post_json("/api/share", &body(3.0, 6.0));
    assert_eq!(status, 409, "{v}");
    assert_eq!(v["job"], b.as_str());

    // a waiting job leaves the queue at once
    let (status, v) = post_json(&format!("/api/jobs/{c}/cancel"), &json!({}));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["stopped"], true);
    let (_, jc) = get_json(&format!("/api/jobs/{c}"));
    assert_eq!(jc["stage"], "cancelled");
    assert_eq!(jc["cancelled"], true);
    assert_eq!(jc["ok"], false);
    assert_eq!(state()["queue"], json!([b]));

    // the first finishes, the second takes over and gets cancelled while it runs
    let (stages, ja) = wait_job(&a, JOB_TIMEOUT);
    assert_stages_monotonic(&stages);
    assert_eq!(ja["stage"], "done", "{}", ja["error"]);
    let (status, v) = post_json(&format!("/api/jobs/{b}/cancel"), &json!({}));
    assert!(
        status == 200 || status == 409,
        "cancel running: {status} {v}"
    );
    let (stages, jb) = wait_job(&b, JOB_TIMEOUT);
    assert_stages_monotonic(&stages);
    assert!(
        jb["stage"] == "cancelled" || jb["stage"] == "done",
        "second job: {jb}"
    );
    if jb["stage"] == "cancelled" {
        assert_eq!(jb["cancelled"], true);
        let prefix = base.replace(char::is_whitespace, "_");
        let leftover = std::fs::read_dir(env().clip_dir.join("shared"))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .any(|n| n.starts_with(&prefix) && n.contains("_3-6"))
            })
            .unwrap_or(false);
        assert!(
            !leftover,
            "the partial file of a cancelled encode must be removed"
        );
    }

    // finished and unknown jobs cannot be cancelled
    let (status, _) = post_json(&format!("/api/jobs/{a}/cancel"), &json!({}));
    assert_eq!(status, 409);
    let (status, _) = post_json("/api/jobs/nope/cancel", &json!({}));
    assert_eq!(status, 404);

    // the queue is empty again: a new share runs at once
    assert_eq!(state()["busy"], false);
    let (status, v) = post_json("/api/share", &body(0.0, 2.0));
    assert_eq!(status, 202, "{v}");
    assert_eq!(v["position"], 0);
    let (_, jd) = wait_job(v["job"].as_str().unwrap_or(""), JOB_TIMEOUT);
    assert_eq!(jd["stage"], "done", "{}", jd["error"]);
    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
}

#[test]
fn t29_thumbnail_is_listed_and_served_cacheable() {
    let _g = serial();
    if !since_24() {
        return;
    }
    let base = format!("{} thumb", fixture().base);
    make_clip(&base);
    let clip = wait_for_clip(&base, Duration::from_secs(20));
    let thumb = clip["thumb"].as_str().unwrap_or("").to_string();
    assert_eq!(thumb, format!("/media/{}.jpg", encode(&base)), "{clip}");
    let resp = get(&thumb);
    assert_eq!(resp.status().as_u16(), 200);
    let header = |k: &str| {
        resp.headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    assert!(
        header("content-type").starts_with("image/jpeg"),
        "content-type"
    );
    assert!(header("cache-control").contains("max-age"), "cache-control");
    let body = resp.bytes().unwrap();
    assert!(
        body.len() > 1000 && body[0] == 0xFF && body[1] == 0xD8,
        "JPEG magic"
    );
    let (status, _) = get_json("/media/nope.jpg");
    assert_eq!(status, 404);
    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
}

#[test]
fn t30_quota_is_null_without_a_storage_account() {
    let _g = serial();
    if !since_24() {
        return;
    }
    // the service under test runs without Nextcloud (dry run or integrations off)
    let cfg = state()["config"].clone();
    assert!(cfg.get("quota").is_some(), "config.quota missing: {cfg}");
    if cfg["quota"].is_object() {
        assert!(cfg["quota"]["usedPercent"].is_number(), "{cfg}");
        assert!(
            cfg["quota"]["free"].is_number() && cfg["quota"]["total"].is_number(),
            "{cfg}"
        );
    } else {
        assert!(cfg["quota"].is_null(), "{cfg}");
    }
}

#[test]
fn t31_copy_share_keeps_the_stream_and_reports_the_real_start() {
    let _g = serial();
    if !since_24() {
        return;
    }
    let base = format!("{} copy", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 5, "end": 9, "audio": "mix", "mode": "bogus" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 5, "end": 9, "audio": "mix", "mode": "copy" }),
    );
    assert_eq!(status, 202, "{v}");
    let id = v["job"].as_str().unwrap_or("").to_string();
    let (stages, job) = wait_job(&id, JOB_TIMEOUT);
    assert_stages_monotonic(&stages);
    assert_eq!(job["stage"], "done", "{}", job["error"]);
    assert_eq!(job["mode"], "copy");
    let actual = job["actualStart"].as_f64().expect("actualStart");
    assert!((0.0..=5.0).contains(&actual), "actualStart {actual}");
    assert_eq!(job["file"], share_file_name(&base, 5, 9, ""));
    assert!(
        env()
            .clip_dir
            .join("shared")
            .join(job["file"].as_str().unwrap_or(""))
            .is_file(),
        "shared file exists"
    );
    // the default mode is h264 and says so
    let id = share(&base, 0.0, 2.0, "mix");
    let (_, job) = wait_job(&id, JOB_TIMEOUT);
    assert_eq!(job["mode"], "h264", "{job}");
    assert!(job.get("actualStart").is_none(), "{job}");
    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
}

#[test]
fn t32_event_stream_pushes_the_state_after_a_change() {
    let _g = serial();
    if !since_24() {
        return;
    }
    use std::io::Read;
    let base = format!("{} sse", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));
    let mut resp = client()
        .get(url("/api/events"))
        .timeout(Duration::from_secs(10))
        .send()
        .expect("GET /api/events");
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("text/event-stream"), "content-type {ct}");
    // the first event comes at once, the second after the change below
    let (status, _) = put_json(
        &format!("/api/clips/{}/name", encode(&base)),
        &json!({ "name": "SSE title" }),
    );
    assert_eq!(status, 200);
    let started = std::time::Instant::now();
    let mut text = String::new();
    let mut buf = [0u8; 4096];
    // an event ends with a blank line; read until the one with the new title is complete
    while text.matches("event: state").count() < 2
        || !text.contains("SSE title")
        || !text.ends_with(
            "

",
        )
    {
        let n = resp.read(&mut buf).expect("read event stream");
        assert!(n > 0, "event stream ended early");
        text.push_str(&String::from_utf8_lossy(&buf[..n]));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no state event with the new title within 5 s: {text}"
        );
    }
    let data_line = text
        .lines()
        .rev()
        .find(|l| l.starts_with("data: "))
        .expect("data line");
    let doc: serde_json::Value = serde_json::from_str(&data_line[6..]).expect("state JSON");
    assert!(
        doc["clips"].is_array() && doc["config"]["version"].is_string(),
        "{doc}"
    );
    drop(resp);
    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
}

// Since 2.5: share targets and publishing a finished job again.

fn since_25() -> bool {
    let v = state()["config"]["version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let ok = (major, minor) >= (2, 5);
    if !ok {
        eprintln!("skipped: needs replaycut 2.5, service is {v}");
    }
    ok
}

#[test]
fn t33_share_targets_and_publish_again() {
    let _g = serial();
    if !since_25() {
        return;
    }
    let base = format!("{} target", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));

    // the known integrations with their state
    let targets = state()["config"]["targets"].clone();
    let list = targets.as_array().expect("config.targets is an array");
    assert!(
        list.iter()
            .any(|t| t["id"] == "nextcloud" && t["kind"] == "storage"),
        "{targets}"
    );
    assert!(
        list.iter()
            .any(|t| t["id"] == "discord" && t["kind"] == "notify"),
        "{targets}"
    );
    for t in list {
        assert!(
            t["label"].is_string() && t["enabled"].is_boolean() && t["connected"].is_boolean(),
            "{t}"
        );
    }

    // an unknown target is a 400, `file` skips the upload
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 0, "end": 2, "audio": "mix", "target": "bogus" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 0, "end": 2, "audio": "mix", "target": "file" }),
    );
    assert_eq!(status, 202, "{v}");
    let (stages, job) = wait_job(v["job"].as_str().unwrap_or(""), JOB_TIMEOUT);
    assert_eq!(job["stage"], "done", "{}", job["error"]);
    assert_eq!(job["target"], "file");
    assert!(
        !stages.iter().any(|s| s == "upload"),
        "file target must not upload: {stages:?}"
    );
    assert!(job.get("link").is_none() || job["link"].is_null(), "{job}");

    // the default target is the quick-share storage (the dry run stands in for Nextcloud)
    let id = share(&base, 2.0, 4.0, "mix");
    let (stages, job) = wait_job(&id, JOB_TIMEOUT);
    assert_stages_monotonic(&stages);
    assert_eq!(job["stage"], "done", "{}", job["error"]);
    assert_eq!(job["target"], "nextcloud", "{job}");
    // the dry-run upload is too quick to be seen by a poll; the link proves it ran
    assert!(job["link"].is_string(), "{job}");

    // publish the finished file again, without cutting
    let (status, v) = post_json(
        &format!("/api/jobs/{id}/publish"),
        &json!({ "target": "nextcloud" }),
    );
    assert_eq!(status, 202, "{v}");
    assert_eq!(v["source"], id.as_str());
    let (stages, again) = wait_job(v["job"].as_str().unwrap_or(""), JOB_TIMEOUT);
    assert_eq!(again["stage"], "done", "{}", again["error"]);
    assert_eq!(again["source"], id.as_str());
    assert_eq!(again["file"], job["file"], "the same file is published");
    assert!(
        !stages.iter().any(|s| s == "encode"),
        "publish must not encode: {stages:?}"
    );
    assert!(again["link"].is_string(), "{again}");

    // publish to `file` makes no sense, unknown jobs are 404
    let (status, v) = post_json(
        &format!("/api/jobs/{id}/publish"),
        &json!({ "target": "file" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, _) = post_json("/api/jobs/nope/publish", &json!({ "target": "nextcloud" }));
    assert_eq!(status, 404);

    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
}

#[test]
fn t34_oauth_status_and_start_without_a_client() {
    let _g = serial();
    if !since_25() {
        return;
    }
    let (status, d) = get_json("/api/oauth/onedrive");
    assert_eq!(status, 200, "{d}");
    assert_eq!(d["provider"], "onedrive");
    assert!(
        d["configured"].is_boolean() && d["connected"].is_boolean(),
        "{d}"
    );
    let (status, _) = get_json("/api/oauth/bogus");
    assert_eq!(status, 404);
    let (status, v) = post_json("/api/oauth/bogus/start", &json!({}));
    assert_eq!(status, 404, "{v}");
    if d["configured"] == false {
        // a build without a client id says so instead of starting a flow
        let (status, v) = post_json("/api/oauth/onedrive/start", &json!({}));
        assert_eq!(status, 409, "{v}");
        assert!(
            v["error"].as_str().unwrap_or("").contains("client id"),
            "{v}"
        );
    }
    // the settings document knows the OneDrive block and its credential flag
    let (_, s) = get_json("/api/settings");
    assert!(s["integrations"]["onedrive"]["enabled"].is_boolean(), "{s}");
    assert!(
        s["integrations"]["onedrive"]["quickShare"].is_boolean(),
        "{s}"
    );
    assert!(s["secrets"]["onedrive"].is_boolean(), "{s}");
    // switching quick share to another storage takes it from the first
    let (status, r) = put_json(
        "/api/settings",
        &json!({ "integrations": { "onedrive": { "quickShare": true } } }),
    );
    assert_eq!(status, 200, "{r}");
    assert_eq!(
        r["settings"]["integrations"]["onedrive"]["quickShare"],
        true
    );
    assert_eq!(
        r["settings"]["integrations"]["nextcloud"]["quickShare"],
        false
    );
    let (status, r) = put_json(
        "/api/settings",
        &json!({ "integrations": { "nextcloud": { "quickShare": true } } }),
    );
    assert_eq!(status, 200, "{r}");
    assert_eq!(
        r["settings"]["integrations"]["onedrive"]["quickShare"],
        false
    );
    assert_eq!(
        r["settings"]["integrations"]["nextcloud"]["quickShare"],
        true
    );
}

#[test]
fn t35_s3_and_webdav_are_targets_with_tests() {
    let _g = serial();
    if !since_25() {
        return;
    }
    let list = state()["config"]["targets"].clone();
    let list = list.as_array().expect("targets");
    for id in ["s3", "webdav"] {
        assert!(
            list.iter().any(|t| t["id"] == id && t["kind"] == "storage"),
            "{id} missing: {list:?}"
        );
    }
    // the settings know both blocks and their credential flags
    let (_, s) = get_json("/api/settings");
    assert!(s["integrations"]["s3"]["endpoint"].is_string(), "{s}");
    assert!(s["integrations"]["s3"]["presignDays"].is_number(), "{s}");
    assert!(s["integrations"]["webdav"]["url"].is_string(), "{s}");
    assert!(
        s["secrets"]["s3"].is_boolean() && s["secrets"]["webdav"].is_boolean(),
        "{s}"
    );
    // half a credential pair is a 400
    let (status, v) = put_json("/api/settings", &json!({ "s3AccessKey": "AK" }));
    assert_eq!(status, 400, "{v}");
    let (status, v) = put_json("/api/settings", &json!({ "webdavPassword": "x" }));
    assert_eq!(status, 400, "{v}");
    // the connection tests validate before they talk to anyone
    let (status, v) = post_json(
        "/api/test/s3",
        &json!({ "endpoint": "ftp://nope", "bucket": "b", "accessKey": "a", "secretKey": "b" }),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap_or("").contains("http"), "{v}");
    let (status, v) = post_json(
        "/api/test/webdav",
        &json!({ "url": "https://dav.example.com", "publicBase": "", "user": "u", "password": "p" }),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().unwrap_or("").contains("public base"),
        "{v}"
    );
    // without stored keys the test says so instead of failing on the network
    let (status, v) = post_json("/api/test/s3", &json!({}));
    assert_eq!(status, 200, "{v}");
    if s["secrets"]["s3"] == false {
        assert!(
            v["error"].as_str().unwrap_or("").contains("no S3 keys"),
            "{v}"
        );
    }
}

// ---------------------------------------------------------------- since 2.6

fn since_26() -> bool {
    let v = state()["config"]["version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let ok = (major, minor) >= (2, 6);
    if !ok {
        eprintln!("skipped: needs replaycut 2.6, service is {v}");
    }
    ok
}

/// `width x height` of the first video stream through ffprobe (next to the
/// ffmpeg the fixture uses); `None` when ffprobe is not there.
fn video_size(path: &std::path::Path) -> Option<(u32, u32)> {
    let ffmpeg = env().ffmpeg.to_string();
    let ffprobe = ffmpeg.replace("ffmpeg", "ffprobe");
    let out = std::process::Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.trim().split(',').map(|n| n.trim().parse::<u32>().ok());
    Some((it.next()??, it.next()??))
}

#[test]
fn t36_youtube_target_and_vertical_cut() {
    let _g = serial();
    if !since_26() {
        return;
    }
    // youtube is a known storage
    let list = state()["config"]["targets"].clone();
    assert!(
        list.as_array()
            .expect("targets")
            .iter()
            .any(|t| t["id"] == "youtube" && t["kind"] == "storage"),
        "{list}"
    );
    // the settings know the block and both credential flags
    let (_, s) = get_json("/api/settings");
    assert!(s["integrations"]["youtube"]["enabled"].is_boolean(), "{s}");
    assert!(
        s["integrations"]["youtube"]["quickShare"].is_boolean(),
        "{s}"
    );
    assert!(s["integrations"]["youtube"]["privacy"].is_string(), "{s}");
    assert!(
        s["integrations"]["youtube"]["description"].is_string(),
        "{s}"
    );
    assert!(
        s["secrets"]["youtube"].is_boolean() && s["secrets"]["youtubeClient"].is_boolean(),
        "{s}"
    );
    // privacy is validated, half a client pair is a 400
    let (status, v) = put_json(
        "/api/settings",
        &json!({ "integrations": { "youtube": { "privacy": "secret" } } }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = put_json("/api/settings", &json!({ "youtubeClientId": "x" }));
    assert_eq!(status, 400, "{v}");
    // the oauth document; without a stored client nothing can start
    let (status, d) = get_json("/api/oauth/youtube");
    assert_eq!(status, 200, "{d}");
    assert_eq!(d["provider"], "youtube");
    assert!(
        d["configured"].is_boolean() && d["connected"].is_boolean(),
        "{d}"
    );
    if d["configured"] == false {
        let (status, v) = post_json("/api/oauth/youtube/start", &json!({}));
        assert_eq!(status, 409, "{v}");
        assert!(v["error"].as_str().unwrap_or("").contains("client"), "{v}");
    }

    // a vertical cut: copy mode cannot crop, h264 makes a 9:16 file of its own
    let base = format!("{} vertical", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 0, "end": 2, "audio": "mix", "mode": "copy", "vertical": true, "target": "file" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 0, "end": 2, "audio": "mix", "vertical": true, "verticalPos": 0.25, "target": "file" }),
    );
    assert_eq!(status, 202, "{v}");
    let (_, job) = wait_job(v["job"].as_str().unwrap_or(""), JOB_TIMEOUT);
    assert_eq!(job["stage"], "done", "{}", job["error"]);
    assert_eq!(job["vertical"], true, "{job}");
    assert_eq!(job["verticalPos"], 0.25, "{job}");
    let file = job["file"].as_str().unwrap_or("");
    assert!(file.ends_with("_9x16.mp4"), "{file}");
    let path = env().clip_dir.join("shared").join(file);
    if path.is_file() {
        if let Some(size) = video_size(&path) {
            assert_eq!(size, (1080, 1920), "vertical cut is 1080x1920");
        }
    }
    // the same range without the crop is a different share, not a duplicate
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 0, "end": 2, "audio": "mix", "target": "file" }),
    );
    assert_eq!(status, 202, "{v}");
    let (_, wide) = wait_job(v["job"].as_str().unwrap_or(""), JOB_TIMEOUT);
    assert_eq!(wide["stage"], "done", "{}", wide["error"]);
    assert!(
        wide.get("vertical").is_none() || wide["vertical"] == false,
        "{wide}"
    );
    assert_ne!(wide["file"], job["file"]);

    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
}

#[test]
fn t37_loopback_login_is_a_second_way_in() {
    let _g = serial();
    if !since_26() {
        return;
    }
    // the provider document says which way it connects
    let (status, d) = get_json("/api/oauth/youtube");
    assert_eq!(status, 200, "{d}");
    assert!(d["loopback"].is_boolean(), "{d}");
    let (_, od) = get_json("/api/oauth/onedrive");
    assert_eq!(od["loopback"], false, "{od}");
    // the client type is a validated setting
    let (status, v) = put_json(
        "/api/settings",
        &json!({ "integrations": { "youtube": { "clientType": "phone" } } }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, r) = put_json(
        "/api/settings",
        &json!({ "integrations": { "youtube": { "clientType": "desktop" } } }),
    );
    assert_eq!(status, 200, "{r}");
    let (_, d) = get_json("/api/oauth/youtube");
    assert_eq!(d["loopback"], true, "{d}");
    // a desktop client cannot start a device flow and vice versa; without a client nothing starts
    if d["configured"] == false {
        let (status, v) = post_json("/api/oauth/youtube/loopback", &json!({}));
        assert_eq!(status, 409, "{v}");
    }
    let (status, v) = post_json("/api/oauth/onedrive/loopback", &json!({}));
    assert!(status == 400 || status == 409, "{status} {v}");
    let (status, _) = post_json("/api/oauth/bogus/loopback", &json!({}));
    assert_eq!(status, 404);
    // the callback refuses answers that belong to no waiting login, as a page
    let res = get("/oauth/youtube/callback?code=x&state=nope");
    assert_eq!(res.status().as_u16(), 400);
    assert!(
        res.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .starts_with("text/html"),
        "the callback answers a page for the browser tab"
    );
    let res = get("/oauth/youtube/callback?error=access_denied");
    assert_eq!(res.status().as_u16(), 400);
    let res = get("/oauth/bogus/callback?code=x&state=y");
    assert_eq!(res.status().as_u16(), 404);
    let (status, r) = put_json(
        "/api/settings",
        &json!({ "integrations": { "youtube": { "clientType": "tv" } } }),
    );
    assert_eq!(status, 200, "{r}");
}

#[test]
fn t38_x_is_a_loopback_target() {
    let _g = serial();
    if !since_26() {
        return;
    }
    let list = state()["config"]["targets"].clone();
    assert!(
        list.as_array()
            .expect("targets")
            .iter()
            .any(|t| t["id"] == "x" && t["kind"] == "storage"),
        "{list}"
    );
    let (_, s) = get_json("/api/settings");
    assert!(s["integrations"]["x"]["enabled"].is_boolean(), "{s}");
    assert!(s["integrations"]["x"]["quickShare"].is_boolean(), "{s}");
    assert!(s["integrations"]["x"]["text"].is_string(), "{s}");
    assert!(s["secrets"]["x"].is_boolean(), "{s}");
    // the post text is limited
    let (status, v) = put_json(
        "/api/settings",
        &json!({ "integrations": { "x": { "text": "a".repeat(281) } } }),
    );
    assert_eq!(status, 400, "{v}");
    // X always connects through the browser on this PC
    let (status, d) = get_json("/api/oauth/x");
    assert_eq!(status, 200, "{d}");
    assert_eq!(d["provider"], "x");
    assert_eq!(d["loopback"], true, "{d}");
    let (status, v) = post_json("/api/oauth/x/start", &json!({}));
    assert!(status == 400 || status == 409, "{status} {v}");
    if d["configured"] == false {
        let (status, v) = post_json("/api/oauth/x/loopback", &json!({}));
        assert_eq!(status, 409, "{v}");
        assert!(v["error"].as_str().unwrap_or("").contains("client"), "{v}");
    }
    let res = get("/oauth/x/callback?code=x&state=nope");
    assert_eq!(res.status().as_u16(), 400);
}

#[test]
fn t39_telegram_and_webhook_are_notify_targets_with_tests() {
    let _g = serial();
    if !since_26() {
        return;
    }
    let list = state()["config"]["targets"].clone();
    let list = list.as_array().expect("targets");
    for id in ["telegram", "webhook"] {
        assert!(
            list.iter().any(|t| t["id"] == id && t["kind"] == "notify"),
            "{id} missing: {list:?}"
        );
    }
    let (_, s) = get_json("/api/settings");
    assert!(s["integrations"]["telegram"]["chatId"].is_string(), "{s}");
    assert!(
        s["integrations"]["telegram"]["autoPost"].is_boolean(),
        "{s}"
    );
    assert!(s["integrations"]["webhook"]["url"].is_string(), "{s}");
    assert!(s["integrations"]["webhook"]["autoPost"].is_boolean(), "{s}");
    assert!(
        s["secrets"]["telegram"].is_boolean() && s["secrets"]["webhookSecret"].is_boolean(),
        "{s}"
    );
    // a webhook URL must be http(s), a token must look like one
    let (status, v) = put_json(
        "/api/settings",
        &json!({ "integrations": { "webhook": { "url": "ftp://nope" } } }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = put_json("/api/settings", &json!({ "telegramToken": "nope" }));
    assert_eq!(status, 400, "{v}");
    // the tests validate before they talk to anyone
    let (status, v) = post_json(
        "/api/test/webhook",
        &json!({ "url": "ftp://nope", "secret": "" }),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap_or("").contains("http"), "{v}");
    let (status, v) = post_json(
        "/api/test/telegram",
        &json!({ "token": "nope", "chatId": "-1001" }),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap_or("").contains("token"), "{v}");
    // without a stored token the test says so instead of failing on the network
    let (status, v) = post_json("/api/test/telegram", &json!({}));
    assert_eq!(status, 200, "{v}");
    if s["secrets"]["telegram"] == false {
        assert!(
            v["error"].as_str().unwrap_or("").contains("no bot token"),
            "{v}"
        );
    }
    // the diagnostics carry both rows
    let (_, d) = get_json("/api/diagnostics");
    let ids: Vec<&str> = d["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"telegram") && ids.contains(&"generic-webhook"),
        "{ids:?}"
    );
}

#[test]
fn t40_finished_file_downloads_as_attachment() {
    let _g = serial();
    if !since_26() {
        return;
    }
    let base = format!("{} download", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 0, "end": 2, "audio": "mix", "target": "file" }),
    );
    assert_eq!(status, 202, "{v}");
    let id = v["job"].as_str().unwrap_or("").to_string();
    let (_, job) = wait_job(&id, JOB_TIMEOUT);
    assert_eq!(job["stage"], "done", "{}", job["error"]);
    let file = job["file"].as_str().unwrap_or("").to_string();

    let res = get(&format!("/api/jobs/{id}/file"));
    assert_eq!(res.status().as_u16(), 200);
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("video/mp4"), "{ct}");
    let cd = res
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(cd.starts_with("attachment"), "{cd}");
    assert!(cd.contains(&file), "{cd} should name {file}");
    let bytes = res.bytes().expect("body");
    assert!(bytes.len() > 10_000, "{} bytes", bytes.len());
    assert_eq!(&bytes[4..8], b"ftyp", "an MP4 starts with the ftyp box");

    let (status, _) = get_json("/api/jobs/nope/file");
    assert_eq!(status, 404);

    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
}

#[test]
fn t41_playable_preview_on_demand() {
    let _g = serial();
    if !since_26() {
        return;
    }
    // the setting is validated
    let (status, v) = put_json("/api/settings", &json!({ "previewH264": "sometimes" }));
    assert_eq!(status, 400, "{v}");
    let (_, s) = get_json("/api/settings");
    assert!(
        s["previewH264"] == "onDemand" || s["previewH264"] == "always",
        "{s}"
    );

    let base = format!("{} h264", fixture().base);
    make_clip(&base);
    let clip = wait_for_clip(&base, Duration::from_secs(20));
    assert!(clip["previewH264"].is_null(), "{clip}");

    let (status, v) = post_json(&format!("/api/clips/{}/preview", encode(&base)), &json!({}));
    assert_eq!(status, 202, "{v}");
    let id = v["job"].as_str().unwrap_or("").to_string();
    assert!(v["position"].is_number(), "{v}");
    let (stages, job) = wait_job(&id, JOB_TIMEOUT);
    assert_eq!(job["stage"], "done", "{}", job["error"]);
    assert_eq!(job["kind"], "preview", "{job}");
    assert_eq!(job["base"], base.as_str());
    assert!(
        !stages.iter().any(|s| s == "upload" || s == "notify"),
        "a preview only encodes: {stages:?}"
    );
    // the clip now carries the copy, served like the preview
    let clip = find_clip(&base).expect("clip");
    let url = clip["previewH264"].as_str().unwrap_or("").to_string();
    assert!(url.ends_with(".h264.mp4"), "{clip}");
    let res = get(&url);
    assert_eq!(res.status().as_u16(), 200);
    assert!(res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .starts_with("video/mp4"));
    let path = env()
        .clip_dir
        .join(".preview")
        .join(format!("{base}.h264.mp4"));
    if let Some(size) = video_size(&path) {
        assert_eq!(size.1, 720, "the copy is 720p: {size:?}");
    }
    // it is not a share: not in the history, and a second request is a 409
    let (_, h) = get_json("/api/history");
    assert!(
        !h["history"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|e| e["id"] == id),
        "preview jobs stay out of the history"
    );
    let (status, v) = post_json(&format!("/api/clips/{}/preview", encode(&base)), &json!({}));
    assert_eq!(status, 409, "{v}");
    let (status, _) = post_json("/api/clips/nope/preview", &json!({}));
    assert_eq!(status, 404);

    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
    assert!(!path.is_file(), "the copy goes with the clip");
}

// ---------------------------------------------------------------- since 2.7

fn since_27() -> bool {
    let v = state()["config"]["version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (2, 7)
}

fn since_28() -> bool {
    let v = state()["config"]["version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (2, 8)
}

#[test]
fn t42_quality_by_default_limits_per_target_and_posting_on_request() {
    let _g = serial();
    if !since_27() {
        eprintln!("skipped: needs replaycut 2.7");
        return;
    }
    // the global bitrate is gone
    assert_eq!(state()["config"]["shareKbps"], 0);
    let (status, v) = put_json("/api/settings", &json!({ "shareKbps": 6000 }));
    assert_eq!(status, 400, "{v}");
    let (_, s) = get_json("/api/settings");
    assert!(s.get("shareKbps").is_none(), "{s}");
    for id in ["nextcloud", "onedrive", "s3", "webdav", "youtube", "x"] {
        assert!(s["integrations"][id]["maxHeight"].is_number(), "{id}: {s}");
        assert!(s["integrations"][id]["maxKbps"].is_number(), "{id}: {s}");
    }
    let (status, v) = put_json(
        "/api/settings",
        &json!({ "integrations": { "nextcloud": { "maxHeight": 100 } } }),
    );
    assert_eq!(status, 400, "{v}");

    let base = format!("{} quality", fixture().base);
    make_clip(&base);
    let clip = wait_for_clip(&base, Duration::from_secs(20));
    let height = clip["height"].as_u64().unwrap_or(720);

    // a share without limits keeps the recording's resolution and reports no bitrate cap
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 0, "end": 2, "audio": "mix", "target": "file" }),
    );
    assert_eq!(status, 202, "{v}");
    let (_, job) = wait_job(v["job"].as_str().unwrap_or(""), JOB_TIMEOUT);
    assert_eq!(job["stage"], "done", "{}", job["error"]);
    assert_eq!(job["kbps"], 0, "{job}");
    if since_28() {
        // the recording's codec and bitrate travel with the job (the UI learns
        // its size estimate from them)
        assert!(job["codec"].is_string(), "{job}");
        assert!(job["sourceKbps"].as_u64().unwrap_or(0) > 0, "{job}");
    }
    assert!(job.get("maxHeight").is_none(), "{job}");
    let file = env()
        .clip_dir
        .join("shared")
        .join(job["file"].as_str().unwrap_or(""));
    if let Some(size) = video_size(&file) {
        assert_eq!(u64::from(size.1), height, "recording resolution kept");
    }

    // limits on the default storage: the share is capped and scaled
    let (status, r) = put_json(
        "/api/settings",
        &json!({ "integrations": { "nextcloud": { "maxHeight": 360, "maxKbps": 1500 } } }),
    );
    assert_eq!(status, 200, "{r}");
    let id = share(&base, 2.0, 4.0, "mix");
    let (stages, capped) = wait_job(&id, JOB_TIMEOUT);
    assert_eq!(capped["stage"], "done", "{}", capped["error"]);
    assert_eq!(capped["kbps"], 1500, "{capped}");
    assert_eq!(capped["maxHeight"], 360, "{capped}");
    let file = env()
        .clip_dir
        .join("shared")
        .join(capped["file"].as_str().unwrap_or(""));
    if let Some(size) = video_size(&file) {
        assert_eq!(size.1, 360, "scaled to the cap");
    }
    // the quick share posts automatically (the dry run stands in for Discord)
    assert!(
        stages.iter().any(|s| s == "notify") || capped["discord"].is_string(),
        "quick share posts: {stages:?} {capped}"
    );
    let (status, r) = put_json(
        "/api/settings",
        &json!({ "integrations": { "nextcloud": { "maxHeight": 0, "maxKbps": 0 } } }),
    );
    assert_eq!(status, 200, "{r}");

    // a publish does not post on its own; "post" does it on request
    let (status, v) = post_json(
        &format!("/api/jobs/{id}/publish"),
        &json!({ "target": "nextcloud" }),
    );
    assert_eq!(status, 202, "{v}");
    let pid = v["job"].as_str().unwrap_or("").to_string();
    let (stages, again) = wait_job(&pid, JOB_TIMEOUT);
    assert_eq!(again["stage"], "done", "{}", again["error"]);
    assert!(
        !stages.iter().any(|s| s == "notify") && again.get("discord").is_none(),
        "a publish stays quiet: {stages:?} {again}"
    );
    let (status, v) = post_json(
        &format!("/api/jobs/{pid}/post"),
        &json!({ "target": "discord" }),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], true);
    assert!(v["status"].is_string(), "{v}");
    let (_, posted) = get_json(&format!("/api/jobs/{pid}"));
    assert!(
        posted["discord"].as_str().unwrap_or("").contains("Discord"),
        "{posted}"
    );
    // `last` (the result card after a reload) carries the note as well
    let (_, doc) = get_json("/api/clips");
    if doc["last"]["id"] == json!(pid) {
        assert!(
            doc["last"]["discord"]
                .as_str()
                .unwrap_or("")
                .contains("Discord"),
            "last: {}",
            doc["last"]
        );
    }
    // a job without a link, an unknown notify, an unknown job
    let (status, v) = post_json(
        &format!("/api/jobs/{}/post", job["id"].as_str().unwrap_or("")),
        &json!({ "target": "discord" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = post_json(
        &format!("/api/jobs/{pid}/post"),
        &json!({ "target": "nope" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, _) = post_json("/api/jobs/nope/post", &json!({ "target": "discord" }));
    assert_eq!(status, 404);

    let (status, _) = delete(&format!("/api/clips/{}", encode(&base)));
    assert_eq!(status, 200);
    wait_for_clip_gone(&base, Duration::from_secs(10));
}

#[test]
fn t43_a_host_this_service_does_not_answer_to_is_refused() {
    let _g = serial();
    if !since_28() {
        eprintln!("skipped: needs replaycut 2.8");
        return;
    }
    // the name the suite uses keeps working
    let (status, _) = get_json("/api/clips");
    assert_eq!(status, 200);
    // a name that is not this machine's is a 421, on the API and on the pages
    for host in ["evil.example", "evil.example:8420"] {
        let (status, v) = get_with_host("/api/clips", host);
        assert_eq!(status, 421, "{host}: {v}");
        assert_eq!(v["ok"], false, "{v}");
        assert!(
            v["error"].as_str().unwrap_or("").contains("host"),
            "{host}: {v}"
        );
    }
    let (status, _) = get_with_host("/", "evil.example");
    assert_eq!(status, 421);
    // an address cannot be rebound, so it stays allowed
    let (status, _) = get_with_host("/api/clips", "127.0.0.1");
    assert_eq!(status, 200);
    let (status, _) = get_with_host("/api/clips", "[::1]");
    assert_eq!(status, 200);
}

#[test]
fn t44_password_rules_the_generator_and_the_network_switch() {
    let _g = serial();
    if !since_28() {
        eprintln!("skipped: needs replaycut 2.8");
        return;
    }
    let (_, before) = get_json("/api/settings");
    assert!(before["allowedHosts"].is_array(), "{before}");

    // 8 to 128 characters, and a rejected password changes nothing
    for pw in ["short12".to_string(), "x".repeat(129)] {
        let (status, v) = put_json("/api/settings", &json!({ "password": pw }));
        assert_eq!(status, 400, "{pw}: {v}");
        assert!(v["error"].as_str().unwrap_or("").contains("128"), "{v}");
    }
    let (_, after) = get_json("/api/settings");
    assert_eq!(after["passwordSet"], before["passwordSet"], "{after}");

    // "Generate one for me": four words, a new one every time
    let (status, v) = get_json("/api/password/suggest");
    assert_eq!(status, 200, "{v}");
    let suggestion = v["password"].as_str().unwrap_or("").to_string();
    let words: Vec<&str> = suggestion.split('-').collect();
    assert_eq!(words.len(), 4, "{suggestion}");
    assert!(
        words
            .iter()
            .all(|w| w.len() >= 3 && w.chars().all(|c| c.is_ascii_lowercase())),
        "{suggestion}"
    );
    assert!(suggestion.chars().count() >= 8, "{suggestion}");
    let (_, again) = get_json("/api/password/suggest");
    assert_ne!(again["password"], v["password"], "always the same words");

    // the session document says which PC this is and how it listens
    let (status, session) = get_json("/api/session");
    assert_eq!(status, 200, "{session}");
    assert!(
        !session["host"].as_str().unwrap_or("").is_empty(),
        "{session}"
    );
    assert!(
        ["loopback", "lan", "custom"].contains(&session["network"].as_str().unwrap_or("")),
        "{session}"
    );
    assert!(session["pairing"].is_boolean(), "{session}");

    // without a password there is nothing to protect the network access with
    if before["passwordSet"] == json!(false) {
        let (status, v) = post_json("/api/network/enable", &json!({}));
        assert_eq!(status, 409, "{v}");
        assert_eq!(v["ok"], false, "{v}");
    }
}

/// The alphabet of the pairing code: no letters that can be misread.
const CODE_ALPHABET: &str = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

fn ask_for_access(name: &str) -> (String, String) {
    let (status, v) = post_json("/api/pair/request", &json!({ "name": name }));
    assert_eq!(status, 202, "POST /api/pair/request: {v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["expires"], 120, "{v}");
    let id = v["id"].as_str().unwrap_or("").to_string();
    let code = v["code"].as_str().unwrap_or("").to_string();
    assert_eq!(id.len(), 32, "{v}");
    assert_eq!(code.chars().count(), 4, "{v}");
    assert!(
        code.chars().all(|c| CODE_ALPHABET.contains(c)),
        "code {code} has a character that can be misread"
    );
    (id, code)
}

#[test]
fn t45_the_device_login_hands_the_cookie_over_once() {
    let _g = serial();
    if !since_28() {
        eprintln!("skipped: needs replaycut 2.8");
        return;
    }
    let (id, code) = ask_for_access("Contract test device");

    // this PC sees the request, with the same code
    let (status, p) = get_json("/api/pair/pending");
    assert_eq!(status, 200, "{p}");
    let mine = p["pending"]
        .as_array()
        .expect("pending")
        .iter()
        .find(|r| r["id"] == id.as_str())
        .unwrap_or_else(|| panic!("the request is not listed: {p}"))
        .clone();
    assert_eq!(mine["name"], "Contract test device", "{mine}");
    assert_eq!(mine["code"], code.as_str(), "{mine}");
    assert!(!mine["ip"].as_str().unwrap_or("").is_empty(), "{mine}");
    assert!(mine["expires"].as_u64().unwrap_or(0) > 60, "{mine}");
    // and the open UI learns about it through the state document
    let st = state();
    assert!(
        st["pending"]
            .as_array()
            .expect("pending in the state")
            .iter()
            .any(|r| r["id"] == id.as_str()),
        "{st}"
    );

    // the device waits
    let (status, s) = get_json(&format!("/api/pair/{id}"));
    assert_eq!(status, 200, "{s}");
    assert_eq!(s["status"], "pending", "{s}");

    // this PC allows it
    let (status, a) = post_json(&format!("/api/pair/{id}/approve"), &json!({}));
    assert_eq!(status, 200, "{a}");
    assert_eq!(a["ok"], true, "{a}");
    let (_, p) = get_json("/api/pair/pending");
    assert!(
        !p["pending"]
            .as_array()
            .expect("pending")
            .iter()
            .any(|r| r["id"] == id.as_str()),
        "an answered request is not waiting any more: {p}"
    );

    // the next poll of that device carries the cookie - once
    let res = get(&format!("/api/pair/{id}"));
    assert_eq!(res.status().as_u16(), 200);
    let cookie = res
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(cookie.contains("rc_session="), "set-cookie: {cookie:?}");
    assert!(cookie.contains("HttpOnly"), "set-cookie: {cookie:?}");
    assert!(cookie.contains("SameSite=Strict"), "set-cookie: {cookie:?}");
    let body: serde_json::Value = res.json().expect("poll body");
    assert_eq!(body["status"], "approved", "{body}");
    let again = get(&format!("/api/pair/{id}"));
    assert!(
        again.headers().get("set-cookie").is_none(),
        "the token is handed over once"
    );
    assert_eq!(
        to_value(again)["status"],
        "approved",
        "the device may still ask how it went"
    );

    // a decision that came too late, and one for a request nobody made
    let (status, v) = post_json(&format!("/api/pair/{id}/approve"), &json!({}));
    assert_eq!(status, 409, "{v}");
    let (status, v) = post_json(
        "/api/pair/0123456789abcdef0123456789abcdef/deny",
        &json!({}),
    );
    assert_eq!(status, 404, "{v}");

    // a QR code that was used up or is too old lands on the login page
    let res = client()
        .get(url("/?pair=nonsense"))
        .send()
        .expect("GET /?pair=");
    assert!(
        res.url().path() == "/login" || res.status().as_u16() == 303,
        "a stale QR token goes to the login page, got {} {}",
        res.status(),
        res.url()
    );
}

#[test]
fn t46_a_denied_request_and_one_that_is_gone() {
    let _g = serial();
    if !since_28() {
        eprintln!("skipped: needs replaycut 2.8");
        return;
    }
    let (id, _) = ask_for_access("Contract test deny");
    let (status, v) = post_json(&format!("/api/pair/{id}/deny"), &json!({}));
    assert_eq!(status, 200, "{v}");
    let (status, s) = get_json(&format!("/api/pair/{id}"));
    assert_eq!(status, 200, "{s}");
    assert_eq!(s["status"], "denied", "{s}");
    let res = get(&format!("/api/pair/{id}"));
    assert!(
        res.headers().get("set-cookie").is_none(),
        "a denied device gets no cookie"
    );

    // A request that ran out answers the same way as one that never existed:
    // the device starts over and learns nothing about other requests. (The
    // two minutes themselves are covered by the unit tests.)
    let (status, s) = get_json("/api/pair/ffffffffffffffffffffffffffffffff");
    assert_eq!(status, 200, "{s}");
    assert_eq!(s["status"], "expired", "{s}");
}

#[test]
fn t47_signed_in_devices_are_listed_and_can_be_revoked() {
    let _g = serial();
    if !since_28() {
        eprintln!("skipped: needs replaycut 2.8");
        return;
    }
    // the diagnostics of 2.8 answer for the network, the firewall and the
    // device login
    let (_, d) = get_json("/api/diagnostics");
    let ids: Vec<&str> = d["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    for id in ["network", "firewall", "pairing"] {
        assert!(ids.contains(&id), "missing check {id}: {ids:?}");
    }

    // a device that was let in shows up with what is known about it
    let (id, _) = ask_for_access("Contract test session");
    let (status, v) = post_json(&format!("/api/pair/{id}/approve"), &json!({}));
    assert_eq!(status, 200, "{v}");
    let _ = get(&format!("/api/pair/{id}")); // the device picks up its cookie
    let (status, s) = get_json("/api/sessions");
    assert_eq!(status, 200, "{s}");
    let device = s["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|d| d["name"] == "Contract test session")
        .unwrap_or_else(|| panic!("the new session is not listed: {s}"))
        .clone();
    for field in ["id", "name", "agent", "ip", "created", "lastSeen", "via"] {
        assert!(device[field].is_string(), "{field}: {device}");
    }
    assert_eq!(device["via"], "approve", "{device}");
    assert!(device["current"].is_boolean(), "{device}");

    // revoking it takes it off the list
    let sid = device["id"].as_str().unwrap_or("").to_string();
    let (status, v) = delete(&format!("/api/sessions/{sid}"));
    assert_eq!(status, 200, "{v}");
    let (_, s) = get_json("/api/sessions");
    assert!(
        !s["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .any(|d| d["id"] == sid.as_str()),
        "the revoked session is gone: {s}"
    );
    let (status, v) = delete("/api/sessions/0123456789abcdef");
    assert_eq!(status, 404, "{v}");

    // and "sign out everywhere" clears the rest
    let (status, v) = post_json("/api/sessions/clear", &json!({}));
    assert_eq!(status, 200, "{v}");
    assert!(v["removed"].is_number(), "{v}");
    let (_, s) = get_json("/api/sessions");
    assert_eq!(
        s["sessions"].as_array().map(Vec::len),
        Some(0),
        "the suite holds no cookie, so nothing is left: {s}"
    );
}

// ---------------------------------------------------------------- since 3.0

fn since_30() -> bool {
    let v = state()["config"]["version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    let major = v
        .split('.')
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    let ok = major >= 3;
    if !ok {
        eprintln!("skipped: needs replaycut 3.0, service is {v}");
    }
    ok
}

/// ffprobe on a file in `shared\`, through the ffprobe next to the ffmpeg
/// the fixture uses.
fn probe_shared(file: &str, args: &[&str]) -> String {
    let ffprobe = env().ffmpeg.to_string().replace("ffmpeg", "ffprobe");
    let out = std::process::Command::new(&ffprobe)
        .args(["-v", "error"])
        .args(args)
        .args(["-of", "csv=p=0"])
        .arg(env().clip_dir.join("shared").join(file))
        .output()
        .unwrap_or_else(|e| panic!("cannot run {ffprobe}: {e}"));
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        !text.is_empty(),
        "ffprobe said nothing about {file}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    text
}

fn shared_duration(file: &str) -> f64 {
    let text = probe_shared(file, &["-show_entries", "format=duration"]);
    text.parse()
        .unwrap_or_else(|_| panic!("no duration for {file}: {text:?}"))
}

/// The number of video frames - the check that catches an off-by-one at the
/// end that a duration in seconds still calls equal.
fn shared_frames(file: &str) -> u64 {
    let text = probe_shared(
        file,
        &[
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames",
        ],
    );
    text.parse()
        .unwrap_or_else(|_| panic!("no frame count for {file}: {text:?}"))
}

/// Every share is made from a cut now: the answer names it, the stage `cut`
/// comes before `encode`, and the cut stays on the clip with its outputs.
#[test]
fn t48_a_share_is_made_from_a_cut_that_stays() {
    let _g = serial();
    if !since_30() {
        return;
    }
    let base = format!("{} cut", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));
    // `after: keep` so the clip stays listed; t50 is about the default
    let body = json!({ "base": base, "start": 6.0, "end": 12.0, "audio": "mix",
                       "target": "file", "after": "keep" });

    let (status, v) = post_json("/api/share", &body);
    assert_eq!(status, 202, "{v}");
    let cut = v["cut"]
        .as_str()
        .unwrap_or_else(|| panic!("the share does not name its cut: {v}"))
        .to_string();
    let job = v["job"].as_str().expect("job").to_string();
    let (stages, done) = wait_job(&job, JOB_TIMEOUT);
    assert_eq!(done["ok"], true, "{done}");
    // The cut is a stream copy of a few dozen milliseconds, so polling may
    // step right over the stage; what it must never do is show it after the
    // encode. That the cut ran at all is proven by the cut object below.
    assert_stages_monotonic(&stages);
    eprintln!("stages of the share: {stages:?}");
    assert_eq!(done["cut"], cut.as_str(), "{done}");

    // the cut hangs on the clip, with its file and this share as its output
    let clip = find_clip(&base).unwrap_or_else(|| panic!("clip {base} is gone"));
    let cuts = clip["cuts"].as_array().expect("the clip lists its cuts");
    let entry = cuts
        .iter()
        .find(|c| c["id"] == cut.as_str())
        .unwrap_or_else(|| panic!("cut {cut} is not on the clip: {cuts:?}"));
    assert_eq!(entry["state"], "ready", "{entry}");
    assert_eq!(entry["file"], format!("{cut}.mkv"), "{entry}");
    assert_eq!(entry["start"], 6.0, "{entry}");
    assert_eq!(entry["end"], 12.0, "{entry}");
    let actual = entry["actualStart"].as_f64().expect("actualStart");
    assert!(
        (0.0..=6.0).contains(&actual),
        "the cut starts at a keyframe at or before the range: {entry}"
    );
    assert!(
        entry["outputs"]
            .as_array()
            .expect("outputs")
            .iter()
            .any(|o| o["id"] == job.as_str()),
        "the share is not an output of its cut: {entry}"
    );
    assert!(
        env()
            .clip_dir
            .join(".cuts")
            .join(format!("{cut}.mkv"))
            .is_file(),
        "the cut file is not in .cuts"
    );

    // and one cut answers for itself
    let (status, one) = get_json(&format!("/api/cuts/{cut}"));
    assert_eq!(status, 200, "{one}");
    assert_eq!(one["id"], cut.as_str(), "{one}");
    assert_eq!(one["base"], base.as_str(), "{one}");
    assert!(
        one["outputs"].as_array().is_some_and(|o| !o.is_empty()),
        "{one}"
    );
    let (status, v) = get_json("/api/cuts/0badc0de");
    assert_eq!(status, 404, "{v}");

    // the same range again is the same cut, not a second file
    let (status, again) = post_json("/api/share", &body);
    assert_eq!(status, 202, "{again}");
    assert_eq!(
        again["cut"],
        cut.as_str(),
        "the range was cut twice: {again}"
    );
    let (_, done) = wait_job(again["job"].as_str().expect("job"), JOB_TIMEOUT);
    assert_eq!(done["ok"], true, "{done}");
}

/// Save a cut while playing, render it later: the rendering is the same
/// length as a share of that range straight from the recording.
#[test]
fn t49_a_cut_is_saved_now_and_rendered_later() {
    let _g = serial();
    if !since_30() {
        return;
    }
    let base = format!("{} render", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));
    let body = json!({ "base": base, "start": 2.0, "end": 9.0, "audio": "mix" });

    let (status, v) = post_json("/api/cuts", &body);
    assert_eq!(status, 202, "{v}");
    let (cut, job) = (
        v["cut"].as_str().expect("cut").to_string(),
        v["job"].as_str().expect("job").to_string(),
    );
    let (stages, done) = wait_job(&job, JOB_TIMEOUT);
    assert_eq!(done["ok"], true, "{done}");
    // `queued` and `cut` can both be over before the first poll; what this
    // job must never do is encode or upload anything
    assert_stages_monotonic(&stages);
    assert_eq!(
        stages.last().map(String::as_str),
        Some("done"),
        "{stages:?}"
    );
    assert!(
        !stages.iter().any(|s| s == "encode" || s == "upload"),
        "a cut renders and sends nothing: {stages:?}"
    );
    assert_eq!(done["kind"], "cut", "{done}");
    // a cut renders nothing, so it is no output and no history entry
    assert!(done["file"].is_null(), "{done}");
    let (_, h) = get_json("/api/history");
    assert!(
        !h["history"]
            .as_array()
            .expect("history")
            .iter()
            .any(|e| e["id"] == job.as_str()),
        "the cut job is in the history"
    );

    // the same range once more answers with the cut that is there
    let (status, dup) = post_json("/api/cuts", &body);
    assert_eq!(status, 409, "{dup}");
    assert_eq!(dup["cut"], cut.as_str(), "{dup}");

    // render it later, without an upload
    let (status, r) = post_json(
        &format!("/api/cuts/{cut}/render"),
        &json!({ "target": "file" }),
    );
    assert_eq!(status, 202, "{r}");
    assert_eq!(r["cut"], cut.as_str(), "{r}");
    let (stages, rendered) = wait_job(r["job"].as_str().expect("job"), JOB_TIMEOUT);
    assert_eq!(rendered["ok"], true, "{rendered}");
    assert_eq!(rendered["kind"], "render", "{rendered}");
    assert_eq!(rendered["cut"], cut.as_str(), "{rendered}");
    assert!(
        !stages.contains(&"cut".to_string()),
        "the cut was there already: {stages:?}"
    );
    let from_cut = rendered["file"].as_str().expect("file").to_string();

    // a title makes the direct share of the same range a different file
    let (status, v) = put_json(
        &format!("/api/clips/{}/name", encode(&base)),
        &json!({ "name": "Same range" }),
    );
    assert_eq!(status, 200, "{v}");
    let (status, v) = post_json(
        "/api/share",
        &json!({ "after": "keep", "base": base, "start": 2.0, "end": 9.0, "audio": "mix", "target": "file" }),
    );
    assert_eq!(status, 202, "{v}");
    let (_, direct) = wait_job(v["job"].as_str().expect("job"), JOB_TIMEOUT);
    assert_eq!(direct["ok"], true, "{direct}");
    let straight = direct["file"].as_str().expect("file").to_string();
    assert_ne!(straight, from_cut);

    let (a, b) = (shared_duration(&from_cut), shared_duration(&straight));
    let frame = 1.0 / 30.0;
    assert!(
        (a - b).abs() <= frame,
        "rendered from the cut: {a} s, straight from the recording: {b} s"
    );
    assert!(
        (a - 7.0).abs() <= frame,
        "the rendering is not the length of the range: {a} s"
    );
    assert_eq!(
        shared_frames(&from_cut),
        shared_frames(&straight),
        "the rendering from the cut has a different number of frames than the one \
         straight from the recording"
    );

    // 404 and 400 where they belong
    let (status, v) = post_json("/api/cuts/0badc0de/render", &json!({ "target": "file" }));
    assert_eq!(status, 404, "{v}");
    let (status, v) = post_json(
        &format!("/api/cuts/{cut}/render"),
        &json!({ "target": "nowhere" }),
    );
    assert_eq!(status, 400, "{v}");
}

/// A clip leaves the list when it is done and comes back on request.
#[test]
fn t50_afterwards_takes_the_clip_out_of_the_list() {
    let _g = serial();
    if !since_30() {
        return;
    }
    let base = format!("{} done", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));

    // the default is in the settings and can be overruled per share
    let (_, s) = get_json("/api/settings");
    assert!(
        ["keep", "done", "recycle"].contains(&s["cleanup"]["afterShare"].as_str().unwrap_or("")),
        "cleanup.afterShare: {}",
        s["cleanup"]
    );
    assert!(
        s["cleanup"]["recycleDoneAfterDays"].is_number(),
        "{}",
        s["cleanup"]
    );
    let (status, v) = put_json(
        "/api/settings",
        &json!({ "cleanup": { "afterShare": "nope" } }),
    );
    assert_eq!(status, 400, "{v}");

    let (status, v) = post_json(
        "/api/share",
        &json!({ "base": base, "start": 1.0, "end": 4.0, "audio": "mix",
                 "target": "file", "after": "keep" }),
    );
    assert_eq!(status, 202, "{v}");
    let (_, kept) = wait_job(v["job"].as_str().expect("job"), JOB_TIMEOUT);
    assert_eq!(kept["ok"], true, "{kept}");
    assert_eq!(kept["after"], "keep", "{kept}");
    assert!(
        find_clip(&base).is_some(),
        "'keep' leaves the clip where it was"
    );

    // `done` takes it out of the list, `?done=1` still shows it
    let (status, v) = post_json(
        "/api/share",
        &json!({ "base": base, "start": 5.0, "end": 8.0, "audio": "mix",
                 "target": "file", "after": "done" }),
    );
    assert_eq!(status, 202, "{v}");
    let (_, done) = wait_job(v["job"].as_str().expect("job"), JOB_TIMEOUT);
    assert_eq!(done["ok"], true, "{done}");
    assert_eq!(done["after"], "done", "{done}");
    assert!(
        find_clip(&base).is_none(),
        "the clip is still in the list after 'done'"
    );
    let (_, all) = get_json("/api/clips?done=1");
    let clip = all["clips"]
        .as_array()
        .expect("clips")
        .iter()
        .find(|c| c["base"] == base.as_str())
        .unwrap_or_else(|| panic!("?done=1 does not list the clip: {}", all["clips"]));
    assert_eq!(clip["state"], "done", "{clip}");
    assert!(
        is_local_timestamp(clip["doneAt"].as_str().unwrap_or("")),
        "{clip}"
    );
    assert!(
        clip["file"].is_string(),
        "the recording is still there: {clip}"
    );
    let counts = &all["counts"];
    assert!(
        counts["done"].as_u64().unwrap_or(0) >= 1 && counts["active"].is_number(),
        "counts: {counts}"
    );

    // and one call brings it back
    let (status, v) = put_json(
        &format!("/api/clips/{}/state", encode(&base)),
        &json!({ "state": "active" }),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["state"], "active", "{v}");
    assert!(find_clip(&base).is_some(), "'active' brings the clip back");
    let (status, v) = put_json(
        &format!("/api/clips/{}/state", encode(&base)),
        &json!({ "state": "sideways" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = put_json("/api/clips/nothing-here/state", &json!({ "state": "done" }));
    assert_eq!(status, 404, "{v}");
}

/// Deleting has a reach: only the recording, or everything.
#[test]
fn t51_delete_takes_the_recording_or_everything() {
    let _g = serial();
    if !since_30() {
        return;
    }
    let base = format!("{} scope", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));
    let (status, v) = post_json(
        "/api/cuts",
        &json!({ "base": base, "start": 2.0, "end": 8.0, "audio": "mix" }),
    );
    assert_eq!(status, 202, "{v}");
    let cut = v["cut"].as_str().expect("cut").to_string();
    let (_, made) = wait_job(v["job"].as_str().expect("job"), JOB_TIMEOUT);
    assert_eq!(made["ok"], true, "{made}");

    // scope=clip: the recording goes, the cut stays and still renders
    let (status, v) = delete(&format!("/api/clips/{}?scope=clip", encode(&base)));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["scope"], "clip", "{v}");
    assert!(v["recycled"].as_u64().unwrap_or(0) >= 1, "{v}");
    let (_, all) = get_json("/api/clips?done=1");
    let clip = all["clips"]
        .as_array()
        .expect("clips")
        .iter()
        .find(|c| c["base"] == base.as_str())
        .unwrap_or_else(|| panic!("the clip is gone although its cut is not: {}", all["clips"]));
    assert!(clip["file"].is_null(), "the recording is gone: {clip}");
    assert_eq!(clip["state"], "done", "{clip}");
    assert_eq!(clip["cuts"].as_array().map(Vec::len), Some(1), "{clip}");
    let (status, one) = get_json(&format!("/api/cuts/{cut}"));
    assert_eq!(status, 200, "{one}");
    assert_eq!(one["state"], "ready", "{one}");

    let (status, r) = post_json(
        &format!("/api/cuts/{cut}/render"),
        &json!({ "target": "file" }),
    );
    assert_eq!(status, 202, "{r}");
    let (_, rendered) = wait_job(r["job"].as_str().expect("job"), JOB_TIMEOUT);
    assert_eq!(
        rendered["ok"], true,
        "a cut renders without its recording: {rendered}"
    );

    // scope=all: the cut and its outputs go too
    let (status, v) = delete(&format!("/api/clips/{}?scope=all", encode(&base)));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["scope"], "all", "{v}");
    assert!(v["recycled"].as_u64().unwrap_or(0) >= 1, "{v}");
    let (status, one) = get_json(&format!("/api/cuts/{cut}"));
    assert_eq!(status, 404, "{one}");
    let (_, all) = get_json("/api/clips?done=1");
    assert!(
        !all["clips"]
            .as_array()
            .expect("clips")
            .iter()
            .any(|c| c["base"] == base.as_str()),
        "the clip survived scope=all"
    );
    let (status, v) = delete(&format!("/api/clips/{}?scope=sideways", encode(&base)));
    assert_eq!(status, 400, "{v}");

    // and one cut can go on its own
    let base = format!("{} cutdel", fixture().base);
    make_clip(&base);
    wait_for_clip(&base, Duration::from_secs(20));
    let (_, v) = post_json(
        "/api/cuts",
        &json!({ "base": base, "start": 1.0, "end": 6.0, "audio": "mix" }),
    );
    let cut = v["cut"].as_str().expect("cut").to_string();
    let (_, made) = wait_job(v["job"].as_str().expect("job"), JOB_TIMEOUT);
    assert_eq!(made["ok"], true, "{made}");
    let clip = find_clip(&base).expect("clip");
    assert_eq!(
        clip["state"], "active",
        "a clip with a cut is active: {clip}"
    );

    let (status, v) = delete(&format!("/api/cuts/{cut}"));
    assert_eq!(status, 200, "{v}");
    assert!(v["recycled"].as_u64().unwrap_or(0) >= 1, "{v}");
    let (status, _) = get_json(&format!("/api/cuts/{cut}"));
    assert_eq!(status, 404);
    let clip = find_clip(&base).expect("the clip stays, it still has its recording");
    assert_eq!(
        clip["state"], "new",
        "a clip without cuts is new again: {clip}"
    );
    assert_eq!(clip["cuts"].as_array().map(Vec::len), Some(0), "{clip}");
    let (status, v) = delete("/api/cuts/0badc0de");
    assert_eq!(status, 404, "{v}");
}

/// The history keeps everything and is read in pages.
#[test]
fn t52_the_history_has_no_cap() {
    let _g = serial();
    if !since_30() {
        return;
    }
    let (_, all) = get_json("/api/history");
    let entries = all["history"].as_array().expect("history").clone();
    assert!(!entries.is_empty(), "the tests before left entries behind");
    // newest first, and every entry has what a client needs
    for e in &entries {
        assert!(e["id"].is_string() && e["base"].is_string(), "{e}");
        assert!(is_local_timestamp(e["at"].as_str().unwrap_or("")), "{e}");
    }
    let times: Vec<&str> = entries.iter().filter_map(|e| e["at"].as_str()).collect();
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(times, sorted, "the history is not newest first");

    // a page, and the page after it
    let (status, first) = get_json("/api/history?limit=2");
    assert_eq!(status, 200, "{first}");
    let page = first["history"].as_array().expect("history");
    assert_eq!(page.len(), 2.min(entries.len()), "{first}");
    assert_eq!(page[0]["id"], entries[0]["id"], "{first}");
    let before = page.last().unwrap()["at"].as_str().unwrap().to_string();
    let (status, next) = get_json(&format!("/api/history?limit=2&before={}", encode(&before)));
    assert_eq!(status, 200, "{next}");
    for e in next["history"].as_array().expect("history") {
        assert!(
            e["at"].as_str().unwrap_or("") < before.as_str(),
            "a page after {before} contains {e}"
        );
    }
    // the status document keeps its 50 for the clients of 1.4
    let st = state();
    assert!(
        st["history"].as_array().map(Vec::len).unwrap_or(0) <= 50,
        "the status document must stay small"
    );
    // the store is not capped: what the status shows is a window on it
    assert!(
        entries.len() >= st["history"].as_array().map(Vec::len).unwrap_or(0),
        "the history endpoint returns at least what the status shows"
    );
}
