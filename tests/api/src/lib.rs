//! Helpers for the replaycut API contract tests.
//!
//! The tests are black-box: they talk to a running service at `BASE_URL`,
//! drop a generated test clip into `CLIP_DIR` (the folder that service scans)
//! and drive the HTTP API described in `docs/api.md`.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Client, Response};
use serde_json::{json, Value};

/// Test configuration from the environment.
pub struct Env {
    pub base_url: String,
    pub clip_dir: PathBuf,
    pub ffmpeg: String,
}

pub fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        let base_url = std::env::var("BASE_URL").unwrap_or_else(|_| {
            panic!("BASE_URL is not set. Point it at the service under test, e.g. http://localhost:8420")
        });
        let clip_dir = std::env::var("CLIP_DIR").unwrap_or_else(|_| {
            panic!("CLIP_DIR is not set. It must be the clip folder the service scans; the test clip is written there")
        });
        let clip_dir = PathBuf::from(clip_dir);
        assert!(
            clip_dir.is_dir(),
            "CLIP_DIR {} does not exist or is not a directory",
            clip_dir.display()
        );
        Env {
            base_url: base_url.trim_end_matches('/').to_string(),
            clip_dir,
            ffmpeg: std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string()),
        }
    })
}

/// The generated test clip: 20 s of `testsrc` video plus four AAC tracks.
pub struct Fixture {
    /// File name without `.mkv`; the key for every API call.
    pub base: String,
    pub name: String,
    pub path: PathBuf,
    /// When ffmpeg finished writing the file.
    pub ready_at: Instant,
}

pub const FIXTURE_PREFIX: &str = "replaycut-test ";
pub const FIXTURE_SECONDS: f64 = 20.0;

pub fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        remove_stale_fixtures();
        let base = format!("{FIXTURE_PREFIX}{}", utc_stamp());
        let name = format!("{base}.mkv");
        let path = make_clip(&base);
        Fixture {
            base,
            name,
            path,
            ready_at: Instant::now(),
        }
    })
}

/// Write `<base>.mkv` into the clip folder: 20 s of `testsrc` video plus
/// four AAC tracks. Used for the fixture and for extra clips in tests.
pub fn make_clip(base: &str) -> PathBuf {
    let env = env();
    let path = env.clip_dir.join(format!("{base}.mkv"));
    let mut cmd = Command::new(&env.ffmpeg);
    cmd.args([
        "-y",
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=1280x720:rate=30",
    ]);
    for hz in [440, 554, 660, 880] {
        cmd.args(["-f", "lavfi", "-i", &format!("sine=frequency={hz}")]);
    }
    cmd.args([
        "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:a", "-map", "4:a",
    ]);
    cmd.args(["-t", &FIXTURE_SECONDS.to_string()]);
    cmd.args([
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
    ]);
    cmd.args(["-c:a", "aac", "-b:a", "64k"]);
    cmd.arg(&path);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", env.ffmpeg));
    assert!(
        out.status.success(),
        "ffmpeg failed to create the clip: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

/// Leftovers from an aborted earlier run would show up as extra clips.
fn remove_stale_fixtures() {
    let Ok(entries) = std::fs::read_dir(&env().clip_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(FIXTURE_PREFIX) && name.ends_with(".mkv") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `YYYY-MM-DD HH-MM-SS` in UTC, without pulling in a date crate. The clip
/// name must contain a `YYYY-MM-DD` date because the share pipeline derives
/// the month folder from it.
fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}-{:02}-{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---------------------------------------------------------------------------
// HTTP

pub fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap()
    })
}

pub fn url(path: &str) -> String {
    format!("{}{path}", env().base_url)
}

/// Percent-encode like JavaScript's `encodeURIComponent`, which is what the UI uses.
pub fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(b as char),
            _ => write!(out, "%{b:02X}").unwrap(),
        }
    }
    out
}

/// Body as JSON, or as a JSON string when the body is not JSON (plain-text 404s).
fn to_json(resp: Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, value)
}

pub fn get(path: &str) -> Response {
    client()
        .get(url(path))
        .send()
        .unwrap_or_else(|e| panic!("GET {path}: {e}"))
}

pub fn get_json(path: &str) -> (u16, Value) {
    to_json(get(path))
}

pub fn post_json(path: &str, body: &Value) -> (u16, Value) {
    to_json(
        client()
            .post(url(path))
            .json(body)
            .send()
            .unwrap_or_else(|e| panic!("POST {path}: {e}")),
    )
}

pub fn put_json(path: &str, body: &Value) -> (u16, Value) {
    to_json(
        client()
            .put(url(path))
            .json(body)
            .send()
            .unwrap_or_else(|e| panic!("PUT {path}: {e}")),
    )
}

pub fn delete(path: &str) -> (u16, Value) {
    to_json(
        client()
            .delete(url(path))
            .send()
            .unwrap_or_else(|e| panic!("DELETE {path}: {e}")),
    )
}

// ---------------------------------------------------------------------------
// Service state helpers

pub fn state() -> Value {
    let (status, v) = get_json("/api/clips");
    assert_eq!(status, 200, "GET /api/clips: {v}");
    v
}

pub fn find_clip(base: &str) -> Option<Value> {
    state()["clips"]
        .as_array()?
        .iter()
        .find(|c| c["base"] == base)
        .cloned()
}

/// Poll `/api/clips` until the clip is listed.
pub fn wait_for_clip(base: &str, timeout: Duration) -> Value {
    let start = Instant::now();
    loop {
        if let Some(c) = find_clip(base) {
            return c;
        }
        assert!(
            start.elapsed() < timeout,
            "clip {base} not listed after {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Poll `/api/clips` until the clip is gone.
pub fn wait_for_clip_gone(base: &str, timeout: Duration) {
    let start = Instant::now();
    while find_clip(base).is_some() {
        assert!(
            start.elapsed() < timeout,
            "clip {base} still listed after {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Poll a job until it reaches `done` or `error`. Returns the distinct stages
/// seen, in order, and the final job object.
pub fn wait_job(id: &str, timeout: Duration) -> (Vec<String>, Value) {
    let start = Instant::now();
    let mut stages: Vec<String> = Vec::new();
    loop {
        let (status, job) = get_json(&format!("/api/jobs/{id}"));
        assert_eq!(status, 200, "GET /api/jobs/{id}: {job}");
        let stage = job["stage"]
            .as_str()
            .unwrap_or_else(|| panic!("job without stage: {job}"))
            .to_string();
        if stages.last() != Some(&stage) {
            stages.push(stage.clone());
        }
        if stage == "done" || stage == "error" {
            return (stages, job);
        }
        assert!(
            start.elapsed() < timeout,
            "job {id} still in stage {stage} after {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub const STAGE_ORDER: [&str; 6] = ["queued", "encode", "upload", "discord", "done", "error"];

/// Stages must appear in contract order and never repeat or go back.
pub fn assert_stages_monotonic(stages: &[String]) {
    let mut last = -1i32;
    for s in stages {
        let idx = STAGE_ORDER
            .iter()
            .position(|k| k == s)
            .unwrap_or_else(|| panic!("unknown stage {s}")) as i32;
        assert!(idx > last, "stage order violated: {stages:?}");
        last = idx;
    }
}

/// Start a share and return the job id.
pub fn share(base: &str, start: f64, end: f64, audio: &str) -> String {
    let (status, v) = post_json(
        "/api/share",
        &json!({ "base": base, "start": start, "end": end, "audio": audio }),
    );
    assert_eq!(status, 202, "POST /api/share: {v}");
    assert_eq!(v["ok"], true, "POST /api/share: {v}");
    v["job"]
        .as_str()
        .unwrap_or_else(|| panic!("share response without job id: {v}"))
        .to_string()
}

pub fn is_local_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && s.chars()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16) || c.is_ascii_digit())
}

pub fn share_file_name(base: &str, start: i64, end: i64, slug: &str) -> String {
    let mut name = base.split_whitespace().collect::<Vec<_>>().join("_");
    name.push_str(&format!("_{start}-{end}"));
    if !slug.is_empty() {
        name.push('_');
        name.push_str(slug);
    }
    name.push_str(".mp4");
    name
}

// ---------------------------------------------------------------------------
// Serialisation of tests

static LOCK: Mutex<()> = Mutex::new(());
static DONE: AtomicUsize = AtomicUsize::new(0);

pub struct Serial(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Drop for Serial {
    fn drop(&mut self) {
        DONE.fetch_add(1, Ordering::SeqCst);
    }
}

/// Every test holds this guard: the service has one share slot and one
/// fixture, so tests must not overlap. Dropping the guard counts the test as
/// finished (see [`wait_for_tests`]).
pub fn serial() -> Serial {
    Serial(LOCK.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Block until `n` other tests have finished. Used by the delete test, which
/// must run last even when the harness runs tests in parallel.
pub fn wait_for_tests(n: usize) {
    let start = Instant::now();
    while DONE.load(Ordering::SeqCst) < n {
        assert!(
            start.elapsed() < Duration::from_secs(600),
            "other tests did not finish in time"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
