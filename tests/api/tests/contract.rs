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
        &json!({ "base": "no such clip", "start": 0, "end": 5 }),
    );
    assert_eq!(status, 404, "unknown clip: {v}");
    assert_eq!(v["ok"], false);
    assert!(v["error"].is_string());

    let (status, v) = post_json(
        "/api/share",
        &json!({ "base": f.base, "start": 5, "end": 5.5, "audio": "mix" }),
    );
    assert!(
        status == 400 || status == 500,
        "selection under 1 s: {status} {v}"
    );
    assert_eq!(v["ok"], false);
    assert!(v["error"].is_string());

    let (status, v) = post_json(
        "/api/share",
        &json!({ "base": f.base, "start": 0, "end": 5, "audio": "nope" }),
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
        &json!({ "base": f.base, "start": 0, "end": 3, "audio": "mix" }),
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
        &json!({ "base": base, "start": 5, "end": 9, "audio": "mix", "mode": "bogus" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = post_json(
        "/api/share",
        &json!({ "base": base, "start": 5, "end": 9, "audio": "mix", "mode": "copy" }),
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
        &json!({ "base": base, "start": 0, "end": 2, "audio": "mix", "target": "bogus" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = post_json(
        "/api/share",
        &json!({ "base": base, "start": 0, "end": 2, "audio": "mix", "target": "file" }),
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
        &json!({ "base": base, "start": 0, "end": 2, "audio": "mix", "mode": "copy", "vertical": true, "target": "file" }),
    );
    assert_eq!(status, 400, "{v}");
    let (status, v) = post_json(
        "/api/share",
        &json!({ "base": base, "start": 0, "end": 2, "audio": "mix", "vertical": true, "verticalPos": 0.25, "target": "file" }),
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
        &json!({ "base": base, "start": 0, "end": 2, "audio": "mix", "target": "file" }),
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
        &json!({ "base": base, "start": 0, "end": 2, "audio": "mix", "target": "file" }),
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
        &json!({ "base": base, "start": 0, "end": 2, "audio": "mix", "target": "file" }),
    );
    assert_eq!(status, 202, "{v}");
    let (_, job) = wait_job(v["job"].as_str().unwrap_or(""), JOB_TIMEOUT);
    assert_eq!(job["stage"], "done", "{}", job["error"]);
    assert_eq!(job["kbps"], 0, "{job}");
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
