//! The state store since 3.0: one SQLite file in the data directory
//! (`replaycut.db`) in place of the three JSON files of 1.4 and 2.x.
//!
//! Three tables plus `meta` (the schema version and what has been imported):
//! `clips` holds what we know about a recording beyond its file (its title,
//! that it was announced, from R11c its state), `cuts` the trimmed ranges and
//! `jobs` every finished job - the share history.
//!
//! A job is stored as the JSON document the API serves, with the fields we
//! query on (base, cut, kind, target, time) as columns beside it. That way a
//! new field in the contract needs no schema change and an entry written by
//! 1.4 survives the round trip unchanged.
//!
//! The maps in `state.rs` stay the cache the status document is built from;
//! this file is the truth that survives a restart.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The store in the data directory.
pub const FILE: &str = "replaycut.db";
/// Where the state files of 2.x go once they have been imported.
pub const BACKUP_DIR: &str = "backup-2.x";
/// The state files of 1.4 and 2.x, in the order they are imported.
pub const STATE_FILES: [&str; 3] = ["clip-names.json", "clip-seen.json", "clip-history.json"];

const SCHEMA: i64 = 1;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- One row per recording we know something about. `state`, `done_at` and
-- `has_file` are filled from R11c on; until then every clip is `new`.
CREATE TABLE IF NOT EXISTS clips (
    base       TEXT PRIMARY KEY,
    title      TEXT NOT NULL DEFAULT '',
    seen       INTEGER NOT NULL DEFAULT 0,
    state      TEXT NOT NULL DEFAULT 'new',
    done_at    TEXT,
    first_seen TEXT,
    has_file   INTEGER NOT NULL DEFAULT 1
);
-- The trimmed ranges with their own file (from R11b); the import puts one row
-- here per range shared with 2.x so that its outputs hang under a cut.
CREATE TABLE IF NOT EXISTS cuts (
    id           TEXT PRIMARY KEY,
    base         TEXT NOT NULL,
    start        REAL NOT NULL DEFAULT 0,
    "end"        REAL NOT NULL DEFAULT 0,
    audio        TEXT NOT NULL DEFAULT '',
    vertical     INTEGER NOT NULL DEFAULT 0,
    vertical_pos REAL,
    file         TEXT,
    actual_start REAL,
    created      TEXT NOT NULL DEFAULT '',
    state        TEXT NOT NULL DEFAULT 'pending'
);
CREATE INDEX IF NOT EXISTS cuts_by_base ON cuts (base);
-- Every finished job: `entry` is the history document of the contract.
CREATE TABLE IF NOT EXISTS jobs (
    id       TEXT PRIMARY KEY,
    base     TEXT NOT NULL DEFAULT '',
    cut      TEXT,
    kind     TEXT NOT NULL DEFAULT 'share',
    target   TEXT NOT NULL DEFAULT '',
    at       TEXT NOT NULL DEFAULT '',
    finished TEXT,
    file     TEXT,
    link     TEXT,
    nc_path  TEXT,
    entry    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_by_time ON jobs (at);
CREATE INDEX IF NOT EXISTS jobs_by_base ON jobs (base);
CREATE INDEX IF NOT EXISTS jobs_by_cut  ON jobs (cut);
"#;

pub struct Db {
    conn: Mutex<Connection>,
}

/// What `import_2x` took over, for the log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Import {
    pub titles: usize,
    pub seen: usize,
    pub jobs: usize,
    pub cuts: usize,
    pub backup: PathBuf,
}

impl Db {
    /// Open (and create) the store. A store that cannot be used is a reason
    /// not to start, not to run on and lose everything that is written next.
    pub fn open(path: &Path) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        Self::prepare(conn).with_context(|| format!("cannot use {}", path.display()))
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(conn: Connection) -> Result<Self> {
        // WAL survives a crash without an fsync per write; the store is local
        // and never opened by a second process.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(SCHEMA_SQL)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.set_meta("schema", &SCHEMA.to_string())?;
        Ok(db)
    }

    // --- meta ---

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // --- titles and the seen list ---

    pub fn titles(&self) -> Result<BTreeMap<String, String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT base, title FROM clips WHERE title <> ''")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn seen(&self) -> Result<BTreeSet<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT base FROM clips WHERE seen = 1")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether a seen list was ever written. False on the very first run: the
    /// clips already in the folder are then recorded without a notification.
    pub fn seen_ready(&self) -> bool {
        matches!(self.meta("seen"), Ok(Some(_)))
    }

    /// Write the titles as they are. A base that lost its title keeps its row
    /// only as long as something else needs it.
    pub fn save_titles(&self, titles: &BTreeMap<String, String>) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("UPDATE clips SET title = '' WHERE title <> ''", [])?;
        for (base, title) in titles {
            upsert_clip(&tx, base, "title", title)?;
        }
        prune_clips(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Write the seen list as it is (the scanner drops bases whose file is gone).
    pub fn save_seen(&self, seen: &BTreeSet<String>) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("UPDATE clips SET seen = 0 WHERE seen = 1", [])?;
        for base in seen {
            upsert_clip(&tx, base, "seen", &1)?;
        }
        prune_clips(&tx)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('seen', '1') ON CONFLICT(key) DO NOTHING",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    // --- jobs (the history) ---

    /// Store a finished job. `cut` is the cut it was rendered from.
    pub fn put_job(&self, entry: &Value, cut: Option<&str>) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO jobs (id, base, cut, kind, target, at, finished, file, link, nc_path, entry)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                base = excluded.base, cut = excluded.cut, kind = excluded.kind,
                target = excluded.target, at = excluded.at, finished = excluded.finished,
                file = excluded.file, link = excluded.link, nc_path = excluded.nc_path,
                entry = excluded.entry",
            params![
                text(entry, "id").unwrap_or_default(),
                text(entry, "base").unwrap_or_default(),
                cut,
                text(entry, "kind").unwrap_or_else(|| "share".into()),
                text(entry, "target").unwrap_or_default(),
                text(entry, "at").unwrap_or_default(),
                text(entry, "finished"),
                text(entry, "file"),
                text(entry, "link"),
                text(entry, "ncPath"),
                entry.to_string(),
            ],
        )?;
        Ok(())
    }

    /// The newest `limit` entries, newest first.
    pub fn recent_jobs(&self, limit: usize) -> Result<Vec<Value>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT entry FROM jobs ORDER BY at DESC, rowid DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            match serde_json::from_str(&row?) {
                Ok(v) => out.push(v),
                Err(e) => tracing::warn!("history entry unreadable: {e}"),
            }
        }
        Ok(out)
    }

    /// One stored entry, for a job that is no longer in the cache.
    pub fn entry(&self, id: &str) -> Result<Option<Value>> {
        let text: Option<String> = self
            .conn
            .lock()
            .query_row("SELECT entry FROM jobs WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
    }

    /// Replace a job's entry (a manual post appends to `discord`, since 2.7).
    pub fn update_entry(&self, id: &str, entry: &Value) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE jobs SET entry = ?2 WHERE id = ?1",
            params![id, entry.to_string()],
        )?;
        Ok(())
    }

    /// Drop a clip's jobs (after its remote copies were deleted).
    pub fn delete_jobs_for_base(&self, base: &str) -> Result<usize> {
        Ok(self
            .conn
            .lock()
            .execute("DELETE FROM jobs WHERE base = ?1", params![base])?)
    }

    /// The outputs of every cut, newest first: the finished jobs that left a
    /// file or a link behind. A cancelled job is in the store but is no
    /// output, and neither is a job of a clip that was shared before 3.0.
    pub fn outputs_by_cut(&self) -> Result<BTreeMap<String, Vec<Value>>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT cut, entry FROM jobs
              WHERE cut IS NOT NULL AND (file IS NOT NULL OR link IS NOT NULL)
              ORDER BY at DESC, rowid DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for row in rows {
            let (cut, entry) = row?;
            match serde_json::from_str(&entry) {
                Ok(v) => out.entry(cut).or_default().push(v),
                Err(e) => tracing::warn!("output entry unreadable: {e}"),
            }
        }
        Ok(out)
    }

    // --- cuts ---

    /// Every cut, oldest first per clip.
    pub fn cuts(&self) -> Result<Vec<Cut>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{CUT_COLUMNS} ORDER BY base, created, rowid"))?;
        let rows = stmt.query_map([], cut_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn cut(&self, id: &str) -> Result<Option<Cut>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                &format!("{CUT_COLUMNS} WHERE id = ?1"),
                params![id],
                cut_from_row,
            )
            .optional()?)
    }

    /// The cut of a range, whatever audio mode it was made with: the file
    /// holds the picture and every track, so the range is what identifies it.
    pub fn cut_of_range(&self, base: &str, start: f64, end: f64) -> Result<Option<Cut>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                &format!(
                    "{CUT_COLUMNS} WHERE base = ?1 AND abs(start - ?2) < {TOLERANCE}
                       AND abs(\"end\" - ?3) < {TOLERANCE} ORDER BY created, rowid"
                ),
                params![base, start, end],
                cut_from_row,
            )
            .optional()?)
    }

    pub fn put_cut(&self, cut: &Cut) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        insert_cut(&tx, cut)?;
        tx.commit()?;
        Ok(())
    }

    /// Forget a cut that never got a file: the job that was to make it
    /// failed or was cancelled. A cut with a file is never dropped this way.
    pub fn delete_pending_cut(&self, id: &str) -> Result<bool> {
        let gone = self.conn.lock().execute(
            "DELETE FROM cuts WHERE id = ?1 AND file IS NULL AND state = ?2",
            params![id, CUT_PENDING],
        )?;
        Ok(gone > 0)
    }

    /// What the cut stage produced: the file, where it really begins, and the
    /// state that goes with it.
    pub fn set_cut_file(
        &self,
        id: &str,
        file: Option<&str>,
        actual_start: Option<f64>,
        state: &str,
    ) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE cuts SET file = ?2, actual_start = ?3, state = ?4 WHERE id = ?1",
            params![id, file, actual_start, state],
        )?;
        Ok(())
    }

    // --- the import of the 2.x state files ---

    /// Take over `clip-names.json`, `clip-seen.json` and `clip-history.json`
    /// and move them to `backup-2.x\`. Runs once: afterwards `meta.import`
    /// says when, and the files are no longer in the data directory.
    pub fn import_2x(&self, data_dir: &Path) -> Result<Option<Import>> {
        if self.meta("import")?.is_some() {
            return Ok(None);
        }
        let files = STATE_FILES.map(|f| data_dir.join(f));
        if !files.iter().any(|f| f.is_file()) {
            return Ok(None);
        }
        let mut import = Import {
            backup: data_dir.join(BACKUP_DIR),
            ..Import::default()
        };
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        if let Some(Value::Object(map)) = read_json_file(&files[0]) {
            for (base, title) in map {
                if let Some(title) = title.as_str().filter(|t| !t.is_empty()) {
                    upsert_clip(&tx, &base, "title", &title)?;
                    import.titles += 1;
                }
            }
        }
        if let Some(Value::Array(items)) = read_json_file(&files[1]) {
            for base in items.iter().filter_map(Value::as_str) {
                upsert_clip(&tx, base, "seen", &1)?;
                import.seen += 1;
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('seen', '1') ON CONFLICT(key) DO NOTHING",
                [],
            )?;
        }
        if let Some(Value::Array(items)) = read_json_file(&files[2]) {
            // The file is newest first; inserting the other way round leaves
            // the rowids in the order the shares happened.
            // Every share of 2.x was a range of its clip: one cut per range,
            // without a file, so that its outputs hang under it. A cancelled
            // share produced nothing and makes no cut of its own; it joins the
            // one of its range when another share used it.
            let mut cuts: BTreeMap<String, String> = BTreeMap::new();
            for entry in items.iter().rev() {
                let base = text(entry, "base").unwrap_or_default();
                let cancelled = entry.get("cancelled").and_then(Value::as_bool) == Some(true);
                if base.is_empty() || cancelled {
                    continue;
                }
                let key = range_key(entry, &base);
                if cuts.contains_key(&key) {
                    continue;
                }
                let row = Cut {
                    id: crate::auth::random_hex(4),
                    base,
                    start: number(entry, "start"),
                    end: number(entry, "end"),
                    audio: text(entry, "audio").unwrap_or_default(),
                    vertical: entry.get("vertical").and_then(Value::as_bool) == Some(true),
                    vertical_pos: entry.get("verticalPos").and_then(Value::as_f64),
                    file: None,
                    actual_start: None,
                    state: CUT_MISSING.into(),
                    created: text(entry, "at").unwrap_or_default(),
                };
                insert_cut(&tx, &row)?;
                import.cuts += 1;
                cuts.insert(key, row.id);
            }
            for entry in items.iter().rev() {
                let Some(id) = text(entry, "id") else {
                    continue;
                };
                let base = text(entry, "base").unwrap_or_default();
                let cut = (!base.is_empty())
                    .then(|| cuts.get(&range_key(entry, &base)))
                    .flatten();
                tx.execute(
                    "INSERT INTO jobs (id, base, cut, kind, target, at, finished, file, link, nc_path, entry)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                     ON CONFLICT(id) DO NOTHING",
                    params![
                        id,
                        base,
                        cut,
                        text(entry, "kind").unwrap_or_else(|| "share".into()),
                        text(entry, "target").unwrap_or_default(),
                        text(entry, "at").unwrap_or_default(),
                        text(entry, "finished"),
                        text(entry, "file"),
                        text(entry, "link"),
                        text(entry, "ncPath"),
                        entry.to_string(),
                    ],
                )?;
                import.jobs += 1;
            }
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('import', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![crate::util::now_local()],
        )?;
        tx.commit()?;
        drop(conn);

        // The files are kept: a downgrade to 2.x finds its state in the backup.
        std::fs::create_dir_all(&import.backup)
            .with_context(|| format!("cannot create {}", import.backup.display()))?;
        for file in &files {
            let Some(name) = file.file_name().filter(|_| file.is_file()) else {
                continue;
            };
            if let Err(e) = std::fs::rename(file, import.backup.join(name)) {
                tracing::warn!("cannot move {} to the backup: {e}", file.display());
            }
        }
        Ok(Some(import))
    }
}

/// A range of a clip with its own file: the lossless keyframe cut every
/// rendering is made from (since 3.0). `audio`, `vertical` and `vertical_pos`
/// are what the next rendering uses unless it says otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cut {
    pub id: String,
    pub base: String,
    pub start: f64,
    pub end: f64,
    pub audio: String,
    #[serde(default)]
    pub vertical: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vertical_pos: Option<f64>,
    /// The file in `.cuts\`, null while the cut is being made or after it
    /// was imported from 2.x.
    pub file: Option<String>,
    /// Where the file really begins: the keyframe at or before `start`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_start: Option<f64>,
    pub created: String,
    /// `pending`, `ready` or `missing`.
    pub state: String,
}

pub const CUT_PENDING: &str = "pending";
pub const CUT_READY: &str = "ready";
pub const CUT_MISSING: &str = "missing";

/// Two ranges are the same cut when they agree to within a frame or two.
const TOLERANCE: f64 = 0.005;

const CUT_COLUMNS: &str = "SELECT id, base, start, \"end\", audio, vertical, vertical_pos,
                                  file, actual_start, created, state FROM cuts";

fn cut_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Cut> {
    Ok(Cut {
        id: r.get(0)?,
        base: r.get(1)?,
        start: r.get(2)?,
        end: r.get(3)?,
        audio: r.get(4)?,
        vertical: r.get::<_, i64>(5)? != 0,
        vertical_pos: r.get(6)?,
        file: r.get(7)?,
        actual_start: r.get(8)?,
        created: r.get(9)?,
        state: r.get(10)?,
    })
}

fn insert_cut(tx: &rusqlite::Transaction<'_>, cut: &Cut) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO cuts (id, base, start, \"end\", audio, vertical, vertical_pos,
                           file, actual_start, created, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET
            start = excluded.start, \"end\" = excluded.\"end\", audio = excluded.audio,
            vertical = excluded.vertical, vertical_pos = excluded.vertical_pos,
            file = excluded.file, actual_start = excluded.actual_start,
            state = excluded.state",
        params![
            cut.id,
            cut.base,
            cut.start,
            cut.end,
            cut.audio,
            cut.vertical,
            cut.vertical_pos,
            cut.file,
            cut.actual_start,
            cut.created,
            cut.state
        ],
    )?;
    // A clip with at least one cut is `active` (the state is served from R11c).
    upsert_clip(tx, &cut.base, "state", &"active")
}

/// Create the clip row if it is new and set one column on it. The other
/// columns keep their value, so writing a title never resets a clip's state.
fn upsert_clip(
    tx: &rusqlite::Transaction<'_>,
    base: &str,
    column: &str,
    value: &dyn rusqlite::ToSql,
) -> rusqlite::Result<()> {
    let sql = format!(
        "INSERT INTO clips (base, {column}) VALUES (?1, ?2)
         ON CONFLICT(base) DO UPDATE SET {column} = excluded.{column}"
    );
    tx.execute(&sql, params![base, value])?;
    Ok(())
}

/// Rows that carry nothing worth keeping: no title, never announced, no cuts
/// and nothing R11c records. Deleting a clip has to forget its title, and a
/// row that only ever held one would otherwise stay behind.
fn prune_clips(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM clips
          WHERE title = '' AND seen = 0 AND state = 'new' AND has_file = 1
            AND base NOT IN (SELECT base FROM cuts)",
        [],
    )?;
    Ok(())
}

/// What makes a range of a clip one cut: where it starts and ends and which
/// audio mode it was shared with.
fn range_key(entry: &Value, base: &str) -> String {
    format!(
        "{base}|{}|{}|{}",
        number(entry, "start"),
        number(entry, "end"),
        text(entry, "audio").unwrap_or_default()
    )
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn number(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn read_json_file(path: &Path) -> Option<Value> {
    if !path.is_file() {
        return None;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("{} unreadable: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("{} unreadable: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rc-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_store_knows_nothing_yet() {
        let db = Db::memory().unwrap();
        assert_eq!(db.meta("schema").unwrap().as_deref(), Some("1"));
        assert!(db.titles().unwrap().is_empty());
        assert!(db.seen().unwrap().is_empty());
        assert!(db.recent_jobs(50).unwrap().is_empty());
        // the very first run announces nothing
        assert!(!db.seen_ready());
    }

    #[test]
    fn titles_and_the_seen_list_are_written_as_they_are() {
        let db = Db::memory().unwrap();
        let mut titles = BTreeMap::new();
        titles.insert("Replay A".to_string(), "Ace".to_string());
        titles.insert("Replay B".to_string(), "Clutch".to_string());
        db.save_titles(&titles).unwrap();
        assert_eq!(db.titles().unwrap(), titles);

        let seen: BTreeSet<String> = ["Replay A", "Replay B"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        db.save_seen(&seen).unwrap();
        assert_eq!(db.seen().unwrap(), seen);
        assert!(db.seen_ready());

        // a title that goes away leaves the clip in the seen list ...
        titles.remove("Replay B");
        db.save_titles(&titles).unwrap();
        assert_eq!(db.titles().unwrap().len(), 1);
        assert_eq!(db.seen().unwrap().len(), 2);
        // ... and a clip that is deleted leaves no row behind
        let seen: BTreeSet<String> = ["Replay A"].iter().map(|s| s.to_string()).collect();
        db.save_seen(&seen).unwrap();
        assert_eq!(db.seen().unwrap(), seen);
        assert_eq!(db.titles().unwrap().len(), 1);
    }

    #[test]
    fn jobs_come_back_newest_first_and_can_be_amended() {
        let db = Db::memory().unwrap();
        for (id, at) in [("j1", "2026-09-01T20:00:00"), ("j2", "2026-09-01T21:00:00")] {
            db.put_job(
                &json!({ "id": id, "base": "Replay A", "at": at, "file": "a.mp4" }),
                None,
            )
            .unwrap();
        }
        db.put_job(
            &json!({ "id": "j3", "base": "Replay B", "at": "2026-09-02T10:00:00" }),
            None,
        )
        .unwrap();
        let ids: Vec<String> = db
            .recent_jobs(10)
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(ids, ["j3", "j2", "j1"]);
        assert_eq!(db.recent_jobs(1).unwrap().len(), 1);

        // a manual post is appended to an entry that is no longer in the cache
        let mut entry = db.entry("j1").unwrap().unwrap();
        entry["discord"] = json!("posted");
        db.update_entry("j1", &entry).unwrap();
        assert_eq!(db.entry("j1").unwrap().unwrap()["discord"], "posted");

        assert_eq!(db.delete_jobs_for_base("Replay A").unwrap(), 2);
        assert_eq!(db.recent_jobs(10).unwrap().len(), 1);
    }

    #[test]
    fn the_state_files_of_2_x_are_imported_once() {
        let dir = scratch("import");
        let write = |name: &str, v: Value| {
            std::fs::write(dir.join(name), v.to_string()).unwrap();
        };
        write(
            STATE_FILES[0],
            json!({ "Replay A": "Ace", "Replay B": "Clutch", "Replay C": "" }),
        );
        write(STATE_FILES[1], json!(["Replay A", "Replay B", "Replay C"]));
        // newest first, as 2.x wrote it; two shares of the same range of
        // Replay A, one of another range, one entry from 1.4 without target
        write(
            STATE_FILES[2],
            json!([
                { "id": "j4", "base": "Replay B", "start": 0.0, "end": 5.0, "audio": "mix",
                  "at": "2026-09-02T10:00:00", "file": "b.mp4", "target": "nextcloud",
                  "link": "https://example.com/b", "ncPath": "clips/b.mp4" },
                { "id": "j3", "base": "Replay A", "start": 10.0, "end": 20.0, "audio": "mix",
                  "at": "2026-09-01T22:00:00", "file": "a3.mp4", "target": "onedrive" },
                { "id": "j2", "base": "Replay A", "start": 0.0, "end": 5.0, "audio": "game",
                  "at": "2026-09-01T21:00:00", "file": "a2.mp4", "target": "nextcloud" },
                { "id": "j1", "base": "Replay A", "start": 0.0, "end": 5.0, "audio": "game",
                  "at": "2026-09-01T20:00:00", "file": "a1.mp4", "shareKbps": 8000 }
            ]),
        );

        let db = Db::open(&dir.join(FILE)).unwrap();
        let import = db.import_2x(&dir).unwrap().expect("something to import");
        assert_eq!((import.titles, import.seen, import.jobs), (2, 3, 4));
        // one cut per range: Replay A was shared twice from the same range
        assert_eq!(import.cuts, 3);

        assert_eq!(db.titles().unwrap().len(), 2);
        assert_eq!(db.seen().unwrap().len(), 3);
        assert!(db.seen_ready());
        let ids: Vec<String> = db
            .recent_jobs(50)
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(ids, ["j4", "j3", "j2", "j1"]);
        // a field 3.0 does not know survives the round trip
        assert_eq!(db.entry("j1").unwrap().unwrap()["shareKbps"], 8000);

        // the files are in the backup, so a downgrade finds its state
        for name in STATE_FILES {
            assert!(!dir.join(name).is_file(), "{name} still in the data folder");
            assert!(
                dir.join(BACKUP_DIR).join(name).is_file(),
                "{name} not in the backup"
            );
        }
        // and a second start does nothing at all
        assert!(db.import_2x(&dir).unwrap().is_none());
        drop(db);
        let again = Db::open(&dir.join(FILE)).unwrap();
        assert!(again.import_2x(&dir).unwrap().is_none());
        assert_eq!(again.recent_jobs(50).unwrap().len(), 4);
        assert_eq!(again.titles().unwrap().len(), 2);
        drop(again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shapes a real 2.x history holds, with placeholders for the URLs:
    /// entries from before 2.5 without `target` and `mode`, a 9:16 cut, a
    /// publish that re-used the file of the share above it, a render to
    /// `file` without a link, and cancelled jobs without a file at all.
    #[test]
    fn a_history_as_2_x_wrote_it_lands_under_its_clips() {
        let dir = scratch("shapes");
        let history = json!([
            { "at": "2026-09-07T16:06:44", "audio": "mix", "base": "Replay B",
              "direct": "https://example.com/v/2", "end": 147.315528,
              "file": "Replay_B_131-147_Title_9x16.mp4", "finished": "2026-09-07T16:07:03",
              "id": "27740515", "kbps": 0, "link": "https://example.com/v/2", "mode": "h264",
              "ncPath": "v2", "seconds": 16.47, "sizeMB": 223.49, "start": 130.848216,
              "target": "youtube", "title": "Title", "vertical": true, "verticalPos": 0.5 },
            { "at": "2026-09-07T16:02:09", "audio": "mix", "base": "Replay B",
              "end": 33.29179270462634, "file": "Replay_B_0-33_Title.mp4",
              "finished": "2026-09-07T16:02:24", "id": "e859d2a0", "kbps": 0, "mode": "h264",
              "seconds": 33.29, "sizeMB": 450.33, "start": 0.0, "target": "file",
              "title": "Title" },
            { "at": "2026-09-07T16:01:46", "audio": "mix", "base": "Replay B",
              "cancelled": true, "end": 33.29179270462634, "finished": "2026-09-07T16:01:50",
              "id": "b6396571", "kbps": 0, "mode": "h264", "seconds": 33.29, "start": 0.0,
              "target": "nextcloud", "title": "Title" },
            { "at": "2026-09-06T15:26:23", "audio": "mix", "base": "Replay B",
              "direct": "https://example.com/v/1", "discord": "Link posted", "end": 147.885318,
              "file": "Replay_B_130-148_Title.mp4", "finished": "2026-09-06T15:26:26",
              "id": "76023924", "kbps": 6000, "link": "https://example.com/v/1", "mode": "h264",
              "ncPath": "v1", "seconds": 17.65, "sizeMB": 13.79, "source": "22c9e63e",
              "start": 130.231572, "target": "youtube", "title": "Title" },
            { "at": "2026-09-06T15:24:02", "audio": "mix", "base": "Replay B",
              "direct": "https://example.com/s/1/download", "discord": "Link posted",
              "end": 147.885318, "file": "Replay_B_130-148_Title.mp4",
              "finished": "2026-09-06T15:24:41", "id": "22c9e63e", "kbps": 6000,
              "link": "https://example.com/s/1", "mode": "h264",
              "ncPath": "/Clips/2026-09/Replay_B_130-148_Title.mp4", "seconds": 17.65,
              "sizeMB": 13.79, "start": 130.231572, "target": "nextcloud", "title": "Title" },
            // before 2.5: no target, no mode - it went to Nextcloud
            { "at": "2026-09-04T18:00:00", "audio": "game", "base": "Replay A", "end": 12.5,
              "file": "Replay_A_5-12.mp4", "finished": "2026-09-04T18:00:31", "id": "0f1e2d3c",
              "kbps": 6000, "link": "https://example.com/s/0", "ncPath": "/Clips/Replay_A_5-12.mp4",
              "seconds": 7.5, "sizeMB": 9.2, "start": 5.0, "title": "" }
        ]);
        std::fs::write(dir.join(STATE_FILES[2]), history.to_string()).unwrap();

        let db = Db::open(&dir.join(FILE)).unwrap();
        let import = db.import_2x(&dir).unwrap().expect("something to import");
        assert_eq!(import.jobs, 6);
        // Replay A one range; Replay B three (9:16, the full clip, the range
        // that was shared and published). The cancelled job joins the range it
        // belongs to instead of making a cut of its own.
        assert_eq!(import.cuts, 4);
        // the publish and its source hang under the same cut
        assert_eq!(cut_of(&db, "76023924"), cut_of(&db, "22c9e63e"));
        assert_ne!(cut_of(&db, "76023924"), cut_of(&db, "27740515"));
        assert_eq!(cut_of(&db, "b6396571"), cut_of(&db, "e859d2a0"));
        // the 9:16 window and a pre-2.5 entry come back as they were
        let vertical = db.entry("27740515").unwrap().unwrap();
        assert_eq!(vertical["verticalPos"], 0.5);
        assert_eq!(vertical["vertical"], true);
        let old = db.entry("0f1e2d3c").unwrap().unwrap();
        assert!(
            old.get("target").is_none(),
            "an entry of 1.4 keeps its shape"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cut a job hangs under, for the tests above.
    fn cut_of(db: &Db, id: &str) -> Option<String> {
        db.conn
            .lock()
            .query_row("SELECT cut FROM jobs WHERE id = ?1", params![id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .unwrap()
    }

    #[test]
    fn an_import_that_finds_nothing_leaves_the_store_alone() {
        let dir = scratch("empty");
        let db = Db::open(&dir.join(FILE)).unwrap();
        assert!(db.import_2x(&dir).unwrap().is_none());
        // nothing was marked: state files that arrive later are still taken over
        assert!(db.meta("import").unwrap().is_none());
        assert!(!dir.join(BACKUP_DIR).exists());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
