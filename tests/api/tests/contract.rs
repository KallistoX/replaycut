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
    assert!(
        job["kbps"].as_f64().unwrap_or(0.0) > 0.0,
        "kbps: {}",
        job["kbps"]
    );
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
    assert_eq!(status, 409, "second share while busy: {v}");
    assert_eq!(v["ok"], false);
    assert_eq!(v["job"], first.as_str(), "409 names the running job");
    assert!(v["error"].is_string());

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
