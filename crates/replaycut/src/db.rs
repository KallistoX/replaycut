//! The state store since 3.0: one SQLite file in the data directory
//! (`replaycut.db`) in place of the three JSON files of 1.4 and 2.x.
//!
//! Three tables plus `meta` (the schema version and what has been imported):
//! `clips` holds what we know about a recording beyond its file (its title,
//! that it was announced, from R11c its state), `cuts` the trimmed ranges and
//! `jobs` every finished job - the share history.
//!
//! Since 3.11 a cut can carry a transcript, in one nullable column. The clip
//! list is built on every poll and every push, so it never reads that column
//! as a whole: `subtitle_summaries` asks SQLite for the few fields it shows
//! and leaves the text on disk.
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

const SCHEMA: i64 = 5;

/// Schema 2 (R11c): a clip keeps what the scanner knew about it, so a clip
/// whose recording is gone can still be listed for its cuts.
const UPGRADE_2: &str = "ALTER TABLE clips ADD COLUMN doc TEXT";

/// Schema 3 (3.10.1, issue #40): a clip knows which folder its recording is
/// in, and whether the service gave that recording up on its own.
const UPGRADE_3: [&str; 2] = [
    "ALTER TABLE clips ADD COLUMN dir TEXT",
    "ALTER TABLE clips ADD COLUMN lost INTEGER NOT NULL DEFAULT 0",
];

/// Schema 4 (R15, since 3.11): a cut can carry a transcript. One nullable
/// column and nothing else, so the way back is free: a 3.10.x opening this
/// store finds every table and every column it knows, unchanged, and simply
/// never looks at `cuts.subtitles`. Its `INSERT ... ON CONFLICT DO UPDATE`
/// names the columns it writes, so an older build editing a cut leaves the
/// transcript where it is instead of dropping it.
const UPGRADE_4: [&str; 1] = ["ALTER TABLE cuts ADD COLUMN subtitles TEXT"];

/// Schema 5 (since 3.12): a cut has a title of its own and a look for its
/// subtitles. Two nullable columns, for the same way back as schema 4: a
/// 3.11.x lists its columns one by one, never sees these two, and its
/// `ON CONFLICT DO UPDATE` leaves them where they are. The look lives here
/// and not in the subtitle document, because a 3.11.x writes that document
/// back after every rendering and would lose a field it does not know.
const UPGRADE_5: [&str; 2] = [
    "ALTER TABLE cuts ADD COLUMN title TEXT",
    "ALTER TABLE cuts ADD COLUMN look TEXT",
];

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
    has_file   INTEGER NOT NULL DEFAULT 1,
    -- the clip document as the scanner last saw it, so a recording that is
    -- gone can still be listed for its cuts
    doc        TEXT,
    -- the folder the recording is in (since 3.10.1); NULL means the folder
    -- the service watches right now
    dir        TEXT,
    -- the service gave the recording up on its own, so finding it again
    -- brings the clip back (since 3.10.1)
    lost       INTEGER NOT NULL DEFAULT 0
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
    state        TEXT NOT NULL DEFAULT 'pending',
    -- the transcript as one JSON document (schema 4, since 3.11);
    -- NULL means this cut has none
    subtitles    TEXT,
    -- the cut's own title (schema 5, since 3.12); NULL: the recording's
    title        TEXT,
    -- the look of its subtitles per frame as JSON (schema 5); NULL: the
    -- settings decide
    look         TEXT
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
    /// This start is the one that brought the store to schema 3: what 3.10
    /// put away when the clip folder changed is healed once (issue #40).
    upgraded_to_3: std::sync::atomic::AtomicBool,
}

/// What `import_2x` took over, for the log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Import {
    pub titles: usize,
    pub seen: usize,
    pub jobs: usize,
    pub cuts: usize,
    /// Clips whose recording is no longer in the folder: they are listed
    /// as done, for the outputs that came out of them.
    pub gone: usize,
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
            upgraded_to_3: std::sync::atomic::AtomicBool::new(false),
        };
        db.upgrade()?;
        db.set_meta("schema", &SCHEMA.to_string())?;
        Ok(db)
    }

    /// Bring a store written by an older 3.0 build up to the current schema.
    /// A fresh store already has everything; the statements are run only for
    /// the versions in between.
    fn upgrade(&self) -> Result<()> {
        let from: i64 = self
            .meta("schema")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(SCHEMA);
        if from < 2 {
            // a store made by the first 3.0 build has no `doc` column
            if let Err(e) = self.conn.lock().execute(UPGRADE_2, []) {
                tracing::debug!("schema 2: {e}");
            }
        }
        if from < 3 {
            self.upgraded_to_3
                .store(true, std::sync::atomic::Ordering::Relaxed);
            for sql in UPGRADE_3 {
                if let Err(e) = self.conn.lock().execute(sql, []) {
                    tracing::debug!("schema 3: {e}");
                }
            }
            // Until 3.10 the folder was only in the clip document; taking it
            // from there is what keeps the clips of a folder that was
            // switched away from where they belong.
            match self.fill_dirs_from_doc() {
                Ok(n) if n > 0 => tracing::info!("{n} clip(s) now know their folder"),
                Ok(_) => {}
                Err(e) => tracing::warn!("cannot tell where the recordings are: {e:#}"),
            }
        }
        if from < 4 {
            for sql in UPGRADE_4 {
                if let Err(e) = self.conn.lock().execute(sql, []) {
                    tracing::debug!("schema 4: {e}");
                }
            }
        }
        if from < 5 {
            for sql in UPGRADE_5 {
                if let Err(e) = self.conn.lock().execute(sql, []) {
                    tracing::debug!("schema 5: {e}");
                }
            }
        }
        Ok(())
    }

    /// Whether this start brought the store to schema 3 - the one start that
    /// looks for what a folder change did before 3.10.1 (issue #40).
    pub fn upgraded_to_3(&self) -> bool {
        self.upgraded_to_3
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Schema 3: fill `dir` from `doc.path` for every row that has one.
    fn fill_dirs_from_doc(&self) -> Result<usize> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let rows: Vec<(String, String)> = {
            let mut stmt =
                tx.prepare("SELECT base, doc FROM clips WHERE doc IS NOT NULL AND dir IS NULL")?;
            let found =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            found.collect::<rusqlite::Result<_>>()?
        };
        let mut filled = 0;
        for (base, doc) in rows {
            let Some(dir) = serde_json::from_str::<Value>(&doc)
                .ok()
                .and_then(|d| text(&d, "path"))
                .filter(|p| !p.is_empty())
                .and_then(|p| {
                    Path::new(&p)
                        .parent()
                        .map(|d| d.to_string_lossy().into_owned())
                })
                .filter(|d| !d.is_empty())
            else {
                continue;
            };
            tx.execute(
                "UPDATE clips SET dir = ?2 WHERE base = ?1",
                params![base, dir],
            )?;
            filled += 1;
        }
        tx.commit()?;
        Ok(filled)
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

    // --- clips (since 3.0: state, and what is known about a clip whose
    //     recording is gone) ---

    /// Every clip row, by base.
    pub fn clips(&self) -> Result<BTreeMap<String, ClipRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(CLIP_COLUMNS)?;
        let rows = stmt.query_map([], clip_from_row)?;
        let mut out = BTreeMap::new();
        for row in rows {
            let row = row?;
            out.insert(row.base.clone(), row);
        }
        Ok(out)
    }

    pub fn clip(&self, base: &str) -> Result<Option<ClipRow>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                &format!("{CLIP_COLUMNS} WHERE base = ?1"),
                params![base],
                clip_from_row,
            )
            .optional()?)
    }

    /// What the scanner sees: the clip document as it is now, the recording
    /// is there in `dir`, and when it was first noticed. Since 3.10.1 the
    /// folder is stored, so a clip survives a change of the clip folder.
    pub fn remember_clip(
        &self,
        base: &str,
        doc: &Value,
        first_seen: &str,
        dir: &Path,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO clips (base, doc, has_file, first_seen, dir, lost)
             VALUES (?1, ?2, 1, ?3, ?4, 0)
             ON CONFLICT(base) DO UPDATE SET
                doc = excluded.doc, has_file = 1, dir = excluded.dir, lost = 0,
                first_seen = COALESCE(clips.first_seen, excluded.first_seen)",
            params![base, doc.to_string(), first_seen, dir.to_string_lossy()],
        )?;
        Ok(())
    }

    /// `new`, `active` or `done`; `done_at` goes with `done`.
    pub fn set_clip_state(&self, base: &str, state: &str, done_at: Option<&str>) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO clips (base, state, done_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(base) DO UPDATE SET state = excluded.state, done_at = excluded.done_at",
            params![base, state, done_at],
        )?;
        Ok(())
    }

    /// Whether the recording is still there. `lost` says whether the service
    /// found that out by itself (since 3.10.1): only then does finding the
    /// recording again bring the clip back.
    pub fn set_clip_file(&self, base: &str, has_file: bool, lost: bool) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE clips SET has_file = ?2, lost = ?3 WHERE base = ?1",
            params![base, has_file, lost],
        )?;
        Ok(())
    }

    /// Forget a clip completely (its cuts and outputs are removed by the
    /// caller, which knows the files).
    pub fn delete_clip(&self, base: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM clips WHERE base = ?1", params![base])?;
        Ok(())
    }

    /// Clips marked done before `before` whose recording is still there -
    /// the candidates of `cleanup.recycleDoneAfterDays`.
    pub fn done_before(&self, before: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT base FROM clips
              WHERE state = ?1 AND has_file = 1 AND done_at IS NOT NULL AND done_at < ?2",
        )?;
        let rows = stmt.query_map(params![CLIP_DONE, before], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
        self.jobs_before(None, limit)
    }

    /// A page of the history: the newest `limit` entries that started before
    /// `before` (a local timestamp), newest first. Since 3.0 the store keeps
    /// every entry, so the page is how the UI walks back through them.
    pub fn jobs_before(&self, before: Option<&str>, limit: usize) -> Result<Vec<Value>> {
        let conn = self.conn.lock();
        let limit = limit as i64;
        // an empty `before` means "from the newest", so one statement does both
        let before = before.unwrap_or_default();
        let mut stmt = conn.prepare(
            "SELECT entry FROM jobs WHERE ?2 = '' OR at < ?2
              ORDER BY at DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit, before], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            match serde_json::from_str(&row?) {
                Ok(v) => out.push(v),
                Err(e) => tracing::warn!("history entry unreadable: {e}"),
            }
        }
        Ok(out)
    }

    /// Every stored job of one clip, newest first (the delete paths ask for
    /// the remote copies they have to remove).
    pub fn jobs_of_base(&self, base: &str) -> Result<Vec<Value>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT entry FROM jobs WHERE base = ?1 ORDER BY at DESC, rowid DESC")?;
        let rows = stmt.query_map(params![base], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(v) = serde_json::from_str(&row?) {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// Every stored job of one cut, newest first.
    pub fn jobs_of_cut(&self, cut: &str) -> Result<Vec<Value>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT entry FROM jobs WHERE cut = ?1 ORDER BY at DESC, rowid DESC")?;
        let rows = stmt.query_map(params![cut], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(v) = serde_json::from_str(&row?) {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// Does a job other than `id` carry this file name (since 3.8)? Every
    /// output has its own file, so a name that is taken is not written again.
    pub fn file_taken_by_other(&self, file: &str, id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT 1 FROM jobs WHERE file = ?1 AND id <> ?2 LIMIT 1")?;
        Ok(stmt.exists(params![file, id])?)
    }

    /// Drop the jobs of one cut (a cut that is deleted takes its outputs).
    pub fn delete_jobs_of_cut(&self, cut: &str) -> Result<usize> {
        Ok(self
            .conn
            .lock()
            .execute("DELETE FROM jobs WHERE cut = ?1", params![cut])?)
    }

    /// How many entries the store holds (the diagnostics say so).
    pub fn job_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .lock()
            .query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))?)
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

    /// Forget every cut that never got a file (since 3.10). No job survives a
    /// restart, so on start each one is left over from a share that waited
    /// or was cutting when the service stopped. Returns their ids.
    pub fn delete_pending_cuts(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let ids = {
            let mut stmt = conn.prepare("SELECT id FROM cuts WHERE file IS NULL AND state = ?1")?;
            let rows = stmt.query_map(params![CUT_PENDING], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()?
        };
        conn.execute(
            "DELETE FROM cuts WHERE file IS NULL AND state = ?1",
            params![CUT_PENDING],
        )?;
        Ok(ids)
    }

    /// The cuts of one clip, oldest first.
    pub fn cuts_of(&self, base: &str) -> Result<Vec<Cut>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "{CUT_COLUMNS} WHERE base = ?1 ORDER BY created, rowid"
        ))?;
        let rows = stmt.query_map(params![base], cut_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Give a cut a title of its own, or take it away with an empty one -
    /// then the recording's applies again (since 3.12).
    pub fn set_cut_title(&self, id: &str, title: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE cuts SET title = NULLIF(?2, '') WHERE id = ?1",
            params![id, title],
        )?;
        Ok(())
    }

    /// Give a cut a look of its own, or take it away (since 3.12). A look
    /// with neither frame set is stored as none.
    pub fn set_cut_look(&self, id: &str, look: &CutLook) -> Result<()> {
        let value = if look.is_empty() {
            None
        } else {
            Some(serde_json::to_string(look)?)
        };
        self.conn.lock().execute(
            "UPDATE cuts SET look = ?2 WHERE id = ?1",
            params![id, value],
        )?;
        Ok(())
    }

    /// Where the 9:16 window of this cut sits, 0 (left) to 1 (right), or
    /// `None` for the middle (since 3.12, from the workshop).
    pub fn set_cut_vertical_pos(&self, id: &str, pos: Option<f64>) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE cuts SET vertical_pos = ?2 WHERE id = ?1",
            params![id, pos],
        )?;
        Ok(())
    }

    /// Forget one cut. The caller removes its file and its outputs.
    pub fn delete_cut(&self, id: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM cuts WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Forget every cut of a clip.
    pub fn delete_cuts_of(&self, base: &str) -> Result<usize> {
        Ok(self
            .conn
            .lock()
            .execute("DELETE FROM cuts WHERE base = ?1", params![base])?)
    }

    /// Mark a cut whose file is no longer there.
    /// The transcript of one cut, segments and all (since 3.11).
    pub fn subtitles(&self, cut: &str) -> Result<Option<Subtitles>> {
        let conn = self.conn.lock();
        let text: Option<String> = conn
            .query_row("SELECT subtitles FROM cuts WHERE id = ?1", [cut], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        match text {
            Some(t) => Ok(serde_json::from_str(&t).ok()),
            None => Ok(None),
        }
    }

    /// What every cut says about its subtitles, for the clip list: the few
    /// fields it shows, asked of SQLite so that the text stays on disk.
    pub fn subtitle_summaries(&self) -> Result<BTreeMap<String, SubtitleSummary>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id,
                    json_extract(subtitles, '$.language'),
                    json_extract(subtitles, '$.model'),
                    json_extract(subtitles, '$.source'),
                    json_extract(subtitles, '$.at'),
                    json_extract(subtitles, '$.mode'),
                    json_extract(subtitles, '$.edited'),
                    json_array_length(subtitles, '$.segments')
             FROM cuts WHERE subtitles IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                SubtitleSummary {
                    language: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    model: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    source: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    at: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    mode: r
                        .get::<_, Option<String>>(5)?
                        .unwrap_or_else(|| SUBS_NONE.to_string()),
                    edited: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
                    count: r.get::<_, Option<i64>>(7)?.unwrap_or(0).max(0) as usize,
                },
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (id, summary) = row?;
            out.insert(id, summary);
        }
        Ok(out)
    }

    /// Write the transcript of a cut, or take it away with `None`.
    pub fn put_subtitles(&self, cut: &str, subs: Option<&Subtitles>) -> Result<()> {
        let text = match subs {
            Some(s) => Some(serde_json::to_string(s)?),
            None => None,
        };
        self.conn.lock().execute(
            "UPDATE cuts SET subtitles = ?2 WHERE id = ?1",
            params![cut, text],
        )?;
        Ok(())
    }

    pub fn set_cut_state(&self, id: &str, state: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE cuts SET state = ?2, file = NULL WHERE id = ?1",
            params![id, state],
        )?;
        Ok(())
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
    pub fn import_2x(&self, data_dir: &Path, clip_dir: &Path) -> Result<Option<Import>> {
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
                    title: String::new(),
                    look: CutLook::default(),
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
        // Which of these clips still have their recording? A clip whose file
        // is gone would otherwise never be listed, and its old shares with
        // it: the import looks once, and the scanner keeps it right after.
        let mut rows = tx.prepare(CLIP_COLUMNS)?;
        let bases: Vec<String> = rows
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(rows);
        for base in bases {
            if clip_dir.join(format!("{base}.mkv")).is_file() {
                continue;
            }
            // it is done: nothing left to cut from, only what it produced.
            // `lost = 1` since 3.10.1: a recording that turns up again after
            // all brings its clip back.
            tx.execute(
                "UPDATE clips SET has_file = 0, lost = 1, state = ?2,
                    done_at = COALESCE(done_at, ?3), doc = ?4
                  WHERE base = ?1",
                params![
                    base,
                    CLIP_DONE,
                    crate::util::now_local(),
                    gone_clip_doc(&base).to_string()
                ],
            )?;
            import.gone += 1;
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

/// What the store knows about a clip beyond its file (since 3.0).
/// The title and the seen flag have queries of their own; this is the rest.
#[derive(Debug, Clone, Default)]
pub struct ClipRow {
    pub base: String,
    /// `new` (no cuts), `active` (cuts) or `done` (out of the list).
    pub state: String,
    pub done_at: Option<String>,
    /// When the scanner first saw the recording.
    pub first_seen: Option<String>,
    /// The recording is still in the clip folder.
    pub has_file: bool,
    /// The clip document as the scanner last saw it.
    pub doc: Option<Value>,
    /// The folder the recording is in (since 3.10.1). `None` means the
    /// folder the service watches right now.
    pub dir: Option<String>,
    /// The service gave the recording up on its own (since 3.10.1): finding
    /// it again brings the clip back into the list. A clip somebody marked
    /// done, or whose recording was recycled on purpose, stays done.
    pub lost: bool,
}

pub const CLIP_NEW: &str = "new";
pub const CLIP_ACTIVE: &str = "active";
pub const CLIP_DONE: &str = "done";

const CLIP_COLUMNS: &str =
    "SELECT base, state, done_at, first_seen, has_file, doc, dir, lost FROM clips";

fn clip_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ClipRow> {
    Ok(ClipRow {
        base: r.get(0)?,
        state: r.get(1)?,
        done_at: r.get(2)?,
        first_seen: r.get(3)?,
        has_file: r.get::<_, i64>(4)? != 0,
        doc: r
            .get::<_, Option<String>>(5)?
            .and_then(|t| serde_json::from_str(&t).ok()),
        dir: r.get::<_, Option<String>>(6)?.filter(|d| !d.is_empty()),
        lost: r.get::<_, i64>(7)? != 0,
    })
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
    /// The cut's own title (since 3.12), taken from the recording when the
    /// cut was made; empty means the recording's title applies.
    #[serde(default)]
    pub title: String,
    /// How its burned-in subtitles look (since 3.12), per frame; a frame
    /// without one takes the look of the settings.
    #[serde(default)]
    pub look: CutLook,
}

/// A cut's own look (since 3.12). Kept in a column of its own rather than
/// in the subtitle document: a 3.11 writes that document back on every
/// rendering and would drop a field it does not know.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutLook {
    pub wide: Option<crate::settings::Look>,
    pub vertical: Option<crate::settings::Look>,
}

impl CutLook {
    /// The look of one frame: the cut's own, or the one of the settings.
    pub fn of<'a>(
        &'a self,
        vertical: bool,
        defaults: &'a crate::settings::Looks,
    ) -> &'a crate::settings::Look {
        let own = if vertical { &self.vertical } else { &self.wide };
        own.as_ref().unwrap_or_else(|| defaults.of(vertical))
    }

    fn is_empty(&self) -> bool {
        self.wide.is_none() && self.vertical.is_none()
    }
}

pub const CUT_PENDING: &str = "pending";
pub const CUT_READY: &str = "ready";
pub const CUT_MISSING: &str = "missing";

/// What a rendering does with the subtitles of its cut (since 3.11).
pub const SUBS_NONE: &str = "none";
/// Drawn into the picture - the only way that works on a phone, in a
/// Discord preview and in a Short.
pub const SUBS_BURN: &str = "burn";
/// Carried along as a track the player can switch on.
pub const SUBS_TRACK: &str = "track";
pub const SUBS_MODES: [&str; 3] = [SUBS_NONE, SUBS_BURN, SUBS_TRACK];

/// Which track a transcript was read from (since 3.11).
pub const SOURCE_MIC: &str = "mic";
pub const SOURCE_MIX: &str = "mix";

/// One subtitle: a range of the **recording** - the same time base as
/// `cut.start`/`cut.end` and as the player in the browser - and its text.
/// A `\n` in the text is a line break in the subtitle; there is no other
/// markup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// The transcript of a cut (since 3.11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subtitles {
    /// The language whisper was told to use, or the one it detected.
    pub language: String,
    /// The model that produced them, empty when they were only ever typed.
    pub model: String,
    /// `mic` or `mix`.
    pub source: String,
    pub at: String,
    /// What the next rendering of this cut does with them.
    pub mode: String,
    /// Somebody corrected them, so a second transcription asks first.
    pub edited: bool,
    pub segments: Vec<Segment>,
}

/// What the clip list and `GET /api/cuts/<id>` say about a cut's subtitles:
/// everything but the text. The segments come from
/// `GET /api/cuts/<id>/subtitles`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleSummary {
    pub language: String,
    pub model: String,
    pub source: String,
    pub at: String,
    pub mode: String,
    pub edited: bool,
    pub count: usize,
}

impl Subtitles {
    pub fn summary(&self) -> SubtitleSummary {
        SubtitleSummary {
            language: self.language.clone(),
            model: self.model.clone(),
            source: self.source.clone(),
            at: self.at.clone(),
            mode: self.mode.clone(),
            edited: self.edited,
            count: self.segments.len(),
        }
    }
}

/// Two ranges are the same cut when they agree to within a frame or two.
const TOLERANCE: f64 = 0.005;

const CUT_COLUMNS: &str = "SELECT id, base, start, \"end\", audio, vertical, vertical_pos,
                                  file, actual_start, created, state, title, look FROM cuts";

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
        title: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
        // a look that no longer reads is no look: the settings' applies
        look: r
            .get::<_, Option<String>>(12)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
    })
}

/// A new cut row, or what the cut stage learned about an existing one. The
/// title is written with a new row only: once there, it changes through
/// [`Db::set_cut_title`] and nothing else (since 3.12).
fn insert_cut(tx: &rusqlite::Transaction<'_>, cut: &Cut) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO cuts (id, base, start, \"end\", audio, vertical, vertical_pos,
                           file, actual_start, created, state, title)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULLIF(?12, ''))
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
            cut.state,
            cut.title
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

/// The clip document for a recording that is already gone at import time:
/// what the page needs to list it for its cuts. The time comes out of the
/// base name (`... 2026-09-05 23-17-55`), which is how OBS names a replay;
/// everything the file would have told us stays empty.
fn gone_clip_doc(base: &str) -> Value {
    serde_json::json!({
        "base": base,
        "name": format!("{base}.mkv"),
        "path": "",
        "size": 0,
        "duration": 0.0,
        "tracks": 0,
        "created": created_from_base(base),
        "preview": "",
        "status": "gone",
        "codec": "",
        "width": 0,
        "height": 0,
        "fps": 0.0,
        "thumb": Value::Null,
        "previewH264": Value::Null,
    })
}

/// `<anything> YYYY-MM-DD HH-MM-SS` -> `YYYY-MM-DDTHH:MM:SS`, else empty.
fn created_from_base(base: &str) -> String {
    let b = base.as_bytes();
    if b.len() < 19 {
        return String::new();
    }
    for i in 0..=b.len() - 19 {
        let w = &b[i..i + 19];
        let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .iter()
            .all(|&k| w[k].is_ascii_digit());
        if digits && w[4] == b'-' && w[7] == b'-' && w[10] == b' ' && w[13] == b'-' && w[16] == b'-'
        {
            let s = String::from_utf8_lossy(w);
            return format!("{}T{}:{}:{}", &s[..10], &s[11..13], &s[14..16], &s[17..19]);
        }
    }
    String::new()
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

    /// A folder for the tests; nothing is written there.
    const VIDEOS: &str = if cfg!(windows) {
        r"C:\Users\me\Videos"
    } else {
        "/home/me/videos"
    };

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rc-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_store_knows_nothing_yet() {
        let db = Db::memory().unwrap();
        assert_eq!(
            db.meta("schema").unwrap().as_deref(),
            Some(SCHEMA.to_string().as_str())
        );
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
        let import = db
            .import_2x(&dir, &dir)
            .unwrap()
            .expect("something to import");
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
        assert!(db.import_2x(&dir, &dir).unwrap().is_none());
        drop(db);
        let again = Db::open(&dir.join(FILE)).unwrap();
        assert!(again.import_2x(&dir, &dir).unwrap().is_none());
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
        let import = db
            .import_2x(&dir, &dir)
            .unwrap()
            .expect("something to import");
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
        assert!(db.import_2x(&dir, &dir).unwrap().is_none());
        // nothing was marked: state files that arrive later are still taken over
        assert!(db.meta("import").unwrap().is_none());
        assert!(!dir.join(BACKUP_DIR).exists());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clip_walks_from_new_over_active_to_done_and_back() {
        let db = Db::memory().unwrap();
        let doc = json!({ "base": "Replay A", "name": "Replay A.mkv", "duration": 20.0 });
        db.remember_clip("Replay A", &doc, "2026-09-08T20:00:00", Path::new(VIDEOS))
            .unwrap();
        let row = db.clip("Replay A").unwrap().expect("the clip is known");
        assert_eq!(row.state, CLIP_NEW);
        assert!(row.has_file);
        assert_eq!(row.first_seen.as_deref(), Some("2026-09-08T20:00:00"));
        assert_eq!(row.doc.unwrap()["name"], "Replay A.mkv");

        // a cut makes it active (a pending one, reserved by a share that
        // never ran, is gone on the next start; a ready one stays)
        db.put_cut(&Cut {
            id: "bbbb2222".into(),
            base: "Replay A".into(),
            start: 7.0,
            end: 9.0,
            audio: "mix".into(),
            vertical: false,
            vertical_pos: None,
            file: None,
            actual_start: None,
            created: "2026-09-08T20:00:30".into(),
            state: CUT_PENDING.into(),
            title: String::new(),
            look: CutLook::default(),
        })
        .unwrap();
        db.put_cut(&Cut {
            id: "aaaa1111".into(),
            base: "Replay A".into(),
            start: 1.0,
            end: 5.0,
            audio: "mix".into(),
            vertical: false,
            vertical_pos: None,
            file: Some("aaaa1111.mkv".into()),
            actual_start: Some(0.5),
            created: "2026-09-08T20:01:00".into(),
            state: CUT_READY.into(),
            title: String::new(),
            look: CutLook::default(),
        })
        .unwrap();
        assert_eq!(db.clip("Replay A").unwrap().unwrap().state, CLIP_ACTIVE);
        assert_eq!(db.cuts_of("Replay A").unwrap().len(), 2);
        assert_eq!(db.delete_pending_cuts().unwrap(), ["bbbb2222"]);
        assert!(db.delete_pending_cuts().unwrap().is_empty());
        assert_eq!(db.cuts_of("Replay A").unwrap().len(), 1);

        // done and back, and a second scan does not undo the state
        db.set_clip_state("Replay A", CLIP_DONE, Some("2026-09-08T21:00:00"))
            .unwrap();
        db.remember_clip("Replay A", &doc, "2026-09-08T22:00:00", Path::new(VIDEOS))
            .unwrap();
        let row = db.clip("Replay A").unwrap().unwrap();
        assert_eq!(row.state, CLIP_DONE);
        assert_eq!(row.done_at.as_deref(), Some("2026-09-08T21:00:00"));
        assert_eq!(
            row.first_seen.as_deref(),
            Some("2026-09-08T20:00:00"),
            "the first sighting is not overwritten"
        );

        // the tidy-up finds it once it is old enough, and not before
        assert!(db.done_before("2026-09-08T20:30:00").unwrap().is_empty());
        assert_eq!(db.done_before("2026-09-09T21:00:00").unwrap(), ["Replay A"]);
        db.set_clip_file("Replay A", false, false).unwrap();
        assert!(
            db.done_before("2026-09-09T21:00:00").unwrap().is_empty(),
            "a recording that is already gone is nothing to tidy up"
        );

        db.set_clip_state("Replay A", CLIP_ACTIVE, None).unwrap();
        assert_eq!(db.clip("Replay A").unwrap().unwrap().state, CLIP_ACTIVE);
        db.delete_cuts_of("Replay A").unwrap();
        db.delete_clip("Replay A").unwrap();
        assert!(db.clip("Replay A").unwrap().is_none());
    }

    /// Since 3.10.1 (issue #40): the folder of a recording is part of what
    /// the store knows, and a scan says whether the file is simply gone or
    /// whether the service lost sight of it.
    #[test]
    fn a_clip_knows_its_folder_and_whether_it_was_lost() {
        let db = Db::memory().unwrap();
        let doc = json!({ "base": "Replay A", "name": "Replay A.mkv" });
        db.remember_clip("Replay A", &doc, "2026-09-08T20:00:00", Path::new(VIDEOS))
            .unwrap();
        let row = db.clip("Replay A").unwrap().unwrap();
        assert_eq!(row.dir.as_deref(), Some(VIDEOS));
        assert!(!row.lost);

        // the scan misses the file: lost, and the mark survives a restart
        db.set_clip_file("Replay A", false, true).unwrap();
        assert!(db.clip("Replay A").unwrap().unwrap().lost);
        // it is there again: `remember_clip` takes the mark off
        db.remember_clip("Replay A", &doc, "2026-09-08T21:00:00", Path::new(VIDEOS))
            .unwrap();
        let row = db.clip("Replay A").unwrap().unwrap();
        assert!(!row.lost);
        assert!(row.has_file);

        // recycled on purpose: no mark, so nothing brings it back by itself
        db.set_clip_file("Replay A", false, false).unwrap();
        assert!(!db.clip("Replay A").unwrap().unwrap().lost);
    }

    /// A store written before 3.10.1 has no folder; it comes out of the clip
    /// document, which has carried the absolute path since 3.0.
    #[test]
    fn the_upgrade_takes_the_folder_out_of_the_document() {
        let db = Db::memory().unwrap();
        let path = Path::new(VIDEOS).join("Replay A.mkv");
        let doc = json!({ "base": "Replay A", "path": path.to_string_lossy() });
        db.conn
            .lock()
            .execute(
                "INSERT INTO clips (base, doc) VALUES ('Replay A', ?1)",
                params![doc.to_string()],
            )
            .unwrap();
        // a row of 2.x that never had a document keeps no folder: it belongs
        // to whatever the service watches
        db.conn
            .lock()
            .execute("INSERT INTO clips (base) VALUES ('Replay B')", [])
            .unwrap();

        assert_eq!(db.fill_dirs_from_doc().unwrap(), 1);
        assert_eq!(
            db.clip("Replay A").unwrap().unwrap().dir.as_deref(),
            Some(VIDEOS)
        );
        assert!(db.clip("Replay B").unwrap().unwrap().dir.is_none());
        // running it twice changes nothing
        assert_eq!(db.fill_dirs_from_doc().unwrap(), 0);
    }

    #[test]
    fn the_history_has_no_cap_and_is_read_in_pages() {
        let db = Db::memory().unwrap();
        for i in 0..250 {
            db.put_job(
                &json!({
                    "id": format!("j{i:03}"), "base": "Replay A",
                    "at": format!("2026-09-08T{:02}:{:02}:00", i / 60, i % 60),
                    "file": "a.mp4",
                }),
                None,
            )
            .unwrap();
        }
        assert_eq!(db.job_count().unwrap(), 250);
        // the default page is the newest 200, and the rest is still there
        assert_eq!(db.recent_jobs(200).unwrap().len(), 200);
        let all = db.jobs_before(None, 1000).unwrap();
        assert_eq!(all.len(), 250);
        assert_eq!(all[0]["id"], "j249");
        assert_eq!(all[249]["id"], "j000");
        // walking back with `before`
        let page = db.jobs_before(None, 100).unwrap();
        let oldest = page.last().unwrap()["at"].as_str().unwrap().to_string();
        let next = db.jobs_before(Some(&oldest), 100).unwrap();
        assert_eq!(next.len(), 100);
        assert_eq!(next[0]["id"], "j149");
    }

    /// The clips of an old history whose recordings are long gone: they are
    /// listed as done, with what the base name still tells us, so their
    /// outputs stay reachable from the clip they came from.
    #[test]
    fn the_import_marks_clips_without_a_recording() {
        let dir = scratch("gone");
        let clips = dir.join("clips");
        std::fs::create_dir_all(&clips).unwrap();
        // one recording is still there, the other one is not
        std::fs::write(clips.join("WARDOGS 2026-09-05 23-17-55.mkv"), b"x").unwrap();
        let history = json!([
            { "id": "j2", "base": "WARDOGS 2026-09-05 23-17-55", "start": 1.0, "end": 6.0,
              "audio": "mix", "at": "2026-09-05T23:20:00", "file": "a.mp4",
              "link": "https://example.com/a", "target": "nextcloud" },
            { "id": "j1", "base": "WARDOGS 2026-09-01 20-15-00", "start": 2.0, "end": 9.0,
              "audio": "mix", "at": "2026-09-01T20:16:00", "file": "b.mp4",
              "link": "https://example.com/b", "target": "nextcloud" }
        ]);
        std::fs::write(dir.join(STATE_FILES[2]), history.to_string()).unwrap();

        let db = Db::open(&dir.join(FILE)).unwrap();
        let import = db.import_2x(&dir, &clips).unwrap().expect("an import");
        assert_eq!(import.gone, 1, "one recording is still in the folder");

        let here = db.clip("WARDOGS 2026-09-05 23-17-55").unwrap().unwrap();
        assert!(here.has_file, "this recording is there: {here:?}");
        assert_eq!(here.state, CLIP_ACTIVE, "{here:?}");

        let gone = db.clip("WARDOGS 2026-09-01 20-15-00").unwrap().unwrap();
        assert!(!gone.has_file, "{gone:?}");
        assert_eq!(
            gone.state, CLIP_DONE,
            "a clip without its recording is done"
        );
        assert!(gone.done_at.is_some(), "{gone:?}");
        let doc = gone.doc.expect("a document, or the page cannot list it");
        assert_eq!(
            doc["created"], "2026-09-01T20:15:00",
            "the time out of the name"
        );
        assert_eq!(doc["name"], "WARDOGS 2026-09-01 20-15-00.mkv");
        assert_eq!(doc["duration"], 0.0, "nothing is invented about the file");
        // and its share still hangs under a cut of that clip
        assert_eq!(db.cuts_of("WARDOGS 2026-09-01 20-15-00").unwrap().len(), 1);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_time_is_read_out_of_the_base_name() {
        assert_eq!(
            created_from_base("WARDOGS 2026-09-05 23-17-55"),
            "2026-09-05T23:17:55"
        );
        assert_eq!(
            created_from_base("Replay 2026-09-05 23-17-55 take 2"),
            "2026-09-05T23:17:55"
        );
        assert_eq!(created_from_base("no time here"), "");
        assert_eq!(created_from_base("2026-09-05"), "");
    }

    fn a_cut(id: &str) -> Cut {
        Cut {
            id: id.into(),
            base: "Replay S".into(),
            start: 6.0,
            end: 12.0,
            audio: "mix".into(),
            vertical: false,
            vertical_pos: None,
            file: Some(format!("{id}.mkv")),
            actual_start: Some(5.0),
            created: "2026-09-20T13:00:00".into(),
            state: CUT_READY.into(),
            title: String::new(),
            look: CutLook::default(),
        }
    }

    fn some_subtitles() -> Subtitles {
        Subtitles {
            language: "de".into(),
            model: "base".into(),
            source: SOURCE_MIC.into(),
            at: "2026-09-20T13:02:00".into(),
            mode: "burn".into(),
            edited: true,
            segments: vec![
                Segment {
                    start: 6.42,
                    end: 8.10,
                    text: "der kommt von links".into(),
                },
                Segment {
                    start: 8.40,
                    end: 10.95,
                    text: "nimm den Rauch,\nich geh rum".into(),
                },
            ],
        }
    }

    #[test]
    fn a_cut_carries_its_transcript_and_gives_a_summary_without_the_text() {
        let db = Db::memory().unwrap();
        db.put_cut(&a_cut("5ub7174e")).unwrap();
        assert!(db.subtitles("5ub7174e").unwrap().is_none());
        assert!(db.subtitle_summaries().unwrap().is_empty());

        let subs = some_subtitles();
        db.put_subtitles("5ub7174e", Some(&subs)).unwrap();
        assert_eq!(db.subtitles("5ub7174e").unwrap().unwrap(), subs);

        // the clip list reads this, and it never touches the segments
        let summaries = db.subtitle_summaries().unwrap();
        let s = summaries
            .get("5ub7174e")
            .expect("the cut is in the summary");
        assert_eq!(s, &subs.summary());
        assert_eq!(s.count, 2);
        assert_eq!(s.language, "de");
        assert!(s.edited);

        // the mode travels with the document and reaches the summary
        let mut as_track = subs.clone();
        as_track.mode = "track".into();
        db.put_subtitles("5ub7174e", Some(&as_track)).unwrap();
        assert_eq!(db.subtitles("5ub7174e").unwrap().unwrap().mode, "track");
        assert_eq!(
            db.subtitle_summaries().unwrap()["5ub7174e"].mode,
            "track",
            "the list shows what the next rendering will do"
        );

        db.put_subtitles("5ub7174e", None).unwrap();
        assert!(db.subtitles("5ub7174e").unwrap().is_none());
        assert!(db.subtitle_summaries().unwrap().is_empty());
    }

    /// The way back to 3.10.x: that build writes a cut with the column list
    /// it knows, which leaves `subtitles` alone. `put_cut` is that same
    /// statement, so editing a cut must not drop its transcript.
    #[test]
    fn writing_a_cut_again_leaves_its_transcript_alone() {
        let db = Db::memory().unwrap();
        db.put_cut(&a_cut("5ub7174f")).unwrap();
        db.put_subtitles("5ub7174f", Some(&some_subtitles()))
            .unwrap();

        let mut again = a_cut("5ub7174f");
        again.audio = "game".into();
        again.state = CUT_MISSING.into();
        db.put_cut(&again).unwrap();

        let back = db.subtitles("5ub7174f").unwrap().expect("still there");
        assert_eq!(back.segments.len(), 2);
        assert_eq!(db.cut("5ub7174f").unwrap().unwrap().audio, "game");
    }

    /// A cut starts with the title it is given and keeps it: writing the cut
    /// again - what the cut stage does, and what a 3.11.x does with its own
    /// column list - leaves the title and the look alone (since 3.12).
    #[test]
    fn writing_a_cut_again_leaves_title_and_look_alone() {
        let db = Db::memory().unwrap();
        let mut cut = a_cut("717e0001");
        cut.title = "Drei mit einem Schuss".into();
        db.put_cut(&cut).unwrap();
        assert_eq!(
            db.cut("717e0001").unwrap().unwrap().title,
            "Drei mit einem Schuss"
        );
        let short = CutLook {
            wide: None,
            vertical: Some(crate::settings::Look {
                position: "middle".into(),
                size: "l".into(),
                color: "box".into(),
            }),
        };
        db.set_cut_look("717e0001", &short).unwrap();

        let mut again = a_cut("717e0001");
        again.state = CUT_MISSING.into();
        db.put_cut(&again).unwrap();
        let read = db.cut("717e0001").unwrap().unwrap();
        assert_eq!(read.title, "Drei mit einem Schuss");
        assert_eq!(read.look, short);

        // the settings fill in the frame the cut leaves open
        let defaults = crate::settings::Looks::default();
        assert_eq!(read.look.of(false, &defaults), &defaults.wide);
        assert_eq!(read.look.of(true, &defaults).size, "l");

        // no frame of its own is no look at all, not an empty one
        db.set_cut_look("717e0001", &CutLook::default()).unwrap();
        let look: Option<String> = db
            .conn
            .lock()
            .query_row("SELECT look FROM cuts WHERE id = '717e0001'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(look, None);

        // an empty title gives the cut back to the recording's
        db.set_cut_title("717e0001", "").unwrap();
        assert_eq!(db.cut("717e0001").unwrap().unwrap().title, "");
        db.set_cut_title("717e0001", "Second angle").unwrap();
        assert_eq!(db.cut("717e0001").unwrap().unwrap().title, "Second angle");
    }

    /// A store written by 3.11 (schema 4) gains the two columns of schema 5
    /// on the first start and loses nothing: its cuts read with an empty
    /// title, and the transcript is where it was.
    #[test]
    fn a_store_of_schema_4_gains_title_and_look() {
        let dir = scratch("schema4");
        let file = dir.join(FILE);
        {
            let conn = Connection::open(&file).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES ('schema', '4');
                 CREATE TABLE cuts (
                     id TEXT PRIMARY KEY, base TEXT NOT NULL, start REAL NOT NULL DEFAULT 0,
                     \"end\" REAL NOT NULL DEFAULT 0, audio TEXT NOT NULL DEFAULT '',
                     vertical INTEGER NOT NULL DEFAULT 0, vertical_pos REAL, file TEXT,
                     actual_start REAL, created TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'pending', subtitles TEXT);
                 INSERT INTO cuts (id, base, start, \"end\", audio, file, state, subtitles)
                   VALUES ('717e0002', 'Replay S', 6, 12, 'mix', '717e0002.mkv', 'ready', '{}');",
            )
            .unwrap();
        }
        let db = Db::open(&file).unwrap();
        assert_eq!(db.meta("schema").unwrap().as_deref(), Some("5"));
        let cut = db.cut("717e0002").unwrap().expect("the cut is still there");
        assert_eq!(cut.title, "");
        assert_eq!(cut.end, 12.0);
        db.set_cut_title("717e0002", "Clutch 1v3").unwrap();
        assert_eq!(db.cut("717e0002").unwrap().unwrap().title, "Clutch 1v3");
        let subtitles: Option<String> = db
            .conn
            .lock()
            .query_row(
                "SELECT subtitles FROM cuts WHERE id = '717e0002'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(subtitles.as_deref(), Some("{}"));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A transcript belongs to its cut and goes with it.
    #[test]
    fn deleting_a_cut_takes_its_transcript() {
        let db = Db::memory().unwrap();
        db.put_cut(&a_cut("5ub71750")).unwrap();
        db.put_subtitles("5ub71750", Some(&some_subtitles()))
            .unwrap();
        db.delete_cut("5ub71750").unwrap();
        assert!(db.subtitles("5ub71750").unwrap().is_none());
        assert!(db.subtitle_summaries().unwrap().is_empty());
    }
}
