//! Local storage: one SQLite file in the user's app-data folder.
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

pub const STATUSES: [&str; 9] = ["New", "Evaluated", "Ready", "Applied", "Interview", "Offer", "Rejected", "Discarded", "Closed"];

/// The application is finished (closed by the user, rejected, or the posting is gone). Kept for history.
pub fn is_closed(status: &str) -> bool {
    matches!(status, "Rejected" | "Discarded" | "Closed")
}
/// Pipeline stages while agents work on a job.
pub const RUNNING: [&str; 7] = ["queued", "fetching", "extracting", "evaluating", "tailoring", "revising", "writing"];

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    url TEXT, company TEXT, title TEXT, location TEXT,
    raw_text TEXT, text_source TEXT,
    jd TEXT, evaluation TEXT, score REAL, resume TEXT, ats TEXT, ats_score INTEGER,
    pdf_path TEXT, cover_letter TEXT,
    stage TEXT NOT NULL DEFAULT 'queued', status TEXT NOT NULL DEFAULT 'New',
    notes TEXT DEFAULT '', error TEXT, logs TEXT NOT NULL DEFAULT '[]',
    source TEXT DEFAULT 'web', applied_at TEXT, next_step TEXT, next_step_date TEXT
);
CREATE TABLE IF NOT EXISTS job_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    ts TEXT NOT NULL, kind TEXT NOT NULL, status TEXT, text TEXT
);
CREATE TABLE IF NOT EXISTS job_chat (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    ts TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, model TEXT
);
CREATE TABLE IF NOT EXISTS companies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL, careers_url TEXT NOT NULL, api_url TEXT, notes TEXT DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1, favorite INTEGER NOT NULL DEFAULT 0,
    last_scan_at TEXT, last_status TEXT, last_found INTEGER, last_postings TEXT
);
CREATE TABLE IF NOT EXISTS discovered (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL, url_key TEXT NOT NULL UNIQUE,
    company TEXT, title TEXT, location TEXT, first_seen TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'new', reason TEXT,
    job_id INTEGER REFERENCES jobs(id) ON DELETE SET NULL, source TEXT DEFAULT 'scan'
);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- Legacy single profile (server exports); adopted into `profiles` on startup.
CREATE TABLE IF NOT EXISTS profile (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    cv_md TEXT NOT NULL DEFAULT '', data TEXT NOT NULL DEFAULT '{}', notes_md TEXT NOT NULL DEFAULT ''
);
"#;

/// Schema version 1: profiles, resume versions, agent log, chat memory, document library.
const V1: &str = r#"
CREATE TABLE profiles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    cv_md TEXT NOT NULL DEFAULT '', data TEXT NOT NULL DEFAULT '{}', notes_md TEXT NOT NULL DEFAULT '',
    template TEXT NOT NULL DEFAULT 'classic',
    active INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
ALTER TABLE jobs ADD COLUMN profile_id INTEGER REFERENCES profiles(id) ON DELETE SET NULL;
ALTER TABLE jobs ADD COLUMN current_version INTEGER;
ALTER TABLE jobs ADD COLUMN chat_memory TEXT NOT NULL DEFAULT '';
ALTER TABLE jobs ADD COLUMN chat_memory_upto INTEGER NOT NULL DEFAULT 0;
ALTER TABLE job_chat ADD COLUMN saved INTEGER NOT NULL DEFAULT 0;
CREATE TABLE resume_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    kind TEXT NOT NULL,              -- tailored | revision | template | imported
    note TEXT NOT NULL DEFAULT '',   -- the instruction or what produced it
    resume TEXT NOT NULL, ats TEXT, ats_score INTEGER,
    template TEXT NOT NULL DEFAULT 'classic', pdf_path TEXT, model TEXT
);
CREATE INDEX resume_versions_job ON resume_versions(job_id);
CREATE TABLE job_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    ts TEXT NOT NULL, agent TEXT NOT NULL, level TEXT NOT NULL DEFAULT 'info',
    message TEXT NOT NULL, data TEXT
);
CREATE INDEX job_log_job ON job_log(job_id);
CREATE TABLE documents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id INTEGER REFERENCES profiles(id) ON DELETE CASCADE,
    job_id INTEGER REFERENCES jobs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,              -- cv | profile | notes | doc | answer | resume | jd | cover
    title TEXT NOT NULL, content TEXT NOT NULL, hash TEXT NOT NULL,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    ord INTEGER NOT NULL, text TEXT NOT NULL, embedding BLOB, embed_model TEXT
);
CREATE INDEX chunks_doc ON chunks(document_id);
CREATE VIRTUAL TABLE chunks_fts USING fts5(text, content='chunks', content_rowid='id', tokenize='porter unicode61');
CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
"#;

pub type Store = Arc<Mutex<Db>>;

pub fn lock(store: &Store) -> MutexGuard<'_, Db> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Db {
    pub conn: Connection,
}

pub fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn json_col(r: &Row, i: usize) -> Value {
    r.get::<_, Option<String>>(i).ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)
}

fn text_col(r: &Row, i: usize) -> String {
    r.get::<_, Option<String>>(i).ok().flatten().unwrap_or_default()
}

#[derive(Clone, Debug, Default)]
pub struct Job {
    pub id: i64,
    pub created_at: String,
    pub url: String,
    pub company: String,
    pub title: String,
    pub location: String,
    pub raw_text: String,
    pub status: String,
    pub stage: String,
    pub score: Option<f64>,
    pub ats_score: Option<i64>,
    pub evaluation: Value,
    pub jd: Value,
    pub pdf_path: String,
    pub cover_letter: String,
    pub notes: String,
    pub error: String,
    pub applied_at: String,
    pub next_step: String,
    pub next_step_date: String,
    pub profile_id: Option<i64>,
    pub current_version: Option<i64>,
    pub chat_memory: String,
    pub chat_memory_upto: i64,
    pub source: String,
}

const JOB_COLS: &str = "id, created_at, url, company, title, location, raw_text, status, stage, score, ats_score, evaluation, jd,
    pdf_path, cover_letter, notes, error, applied_at, next_step, next_step_date, profile_id, current_version,
    chat_memory, chat_memory_upto, source";

fn job_row(r: &Row) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        created_at: text_col(r, 1),
        url: text_col(r, 2),
        company: text_col(r, 3),
        title: text_col(r, 4),
        location: text_col(r, 5),
        raw_text: text_col(r, 6),
        status: text_col(r, 7),
        stage: text_col(r, 8),
        score: r.get(9)?,
        ats_score: r.get(10)?,
        evaluation: json_col(r, 11),
        jd: json_col(r, 12),
        pdf_path: text_col(r, 13),
        cover_letter: text_col(r, 14),
        notes: text_col(r, 15),
        error: text_col(r, 16),
        applied_at: text_col(r, 17),
        next_step: text_col(r, 18),
        next_step_date: text_col(r, 19),
        profile_id: r.get(20)?,
        current_version: r.get(21)?,
        chat_memory: text_col(r, 22),
        chat_memory_upto: r.get::<_, Option<i64>>(23)?.unwrap_or(0),
        source: text_col(r, 24),
    })
}

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub cv_md: String,
    pub data: Value,
    pub notes_md: String,
    pub template: String,
    pub active: bool,
}

impl Profile {
    pub fn full_name(&self) -> String {
        self.data["candidate"]["full_name"].as_str().unwrap_or("").trim().to_string()
    }
    pub fn email(&self) -> String {
        self.data["candidate"]["email"].as_str().unwrap_or("").trim().to_string()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Version {
    pub id: i64,
    pub job_id: i64,
    pub created_at: String,
    pub kind: String,
    pub note: String,
    pub resume: Value,
    pub ats: Value,
    pub ats_score: Option<i64>,
    pub template: String,
    pub pdf_path: String,
    pub model: String,
}

#[derive(Clone, Debug, Default)]
pub struct LogLine {
    pub id: i64,
    pub ts: String,
    pub agent: String,
    pub level: String,
    pub message: String,
    pub data: Value,
}

#[derive(Clone, Debug, Default)]
pub struct ChatMsg {
    pub id: i64,
    pub ts: String,
    pub role: String,
    pub content: String,
    pub model: String,
    pub saved: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Event {
    pub ts: String,
    pub kind: String,
    pub status: String,
    pub text: String,
}

/// Columns callers may change with `update_job`.
const UPDATABLE: [&str; 22] = [
    "url", "company", "title", "location", "raw_text", "text_source", "jd", "evaluation", "score", "resume", "ats",
    "ats_score", "pdf_path", "cover_letter", "stage", "status", "notes", "error", "applied_at", "next_step",
    "next_step_date", "current_version",
];

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        // journal_size_limit keeps the write-ahead log from growing past 4 MB between checkpoints.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;
                            PRAGMA journal_size_limit = 4194304;")?;
        conn.execute_batch(SCHEMA)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(&format!("BEGIN; {V1} PRAGMA user_version = 1; COMMIT;"))?;
        }
        let db = Db { conn };
        db.adopt_legacy()?;
        Ok(db)
    }

    /// On quit: fold the write-ahead log back into the database and shrink it to nothing,
    /// so the data folder is one compact file.
    pub fn tidy(&self) {
        let _ = self.conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);");
    }

    pub fn store(self) -> Store {
        Arc::new(Mutex::new(self))
    }

    /// Bring server-era data into the current shape: the single profile, resumes stored
    /// on the job row, and the JSON log column. Idempotent; runs at every start.
    pub fn adopt_legacy(&self) -> Result<()> {
        let c = &self.conn;
        let has_profiles: i64 = c.query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get(0))?;
        if has_profiles == 0 {
            c.execute(
                "INSERT INTO profiles (name, cv_md, data, notes_md, active, created_at, updated_at)
                 SELECT 'Main profile', cv_md, data, notes_md, 1, ?1, ?1 FROM profile WHERE id = 1",
                params![now()],
            )?;
        }
        c.execute("UPDATE jobs SET profile_id = (SELECT id FROM profiles WHERE active = 1 LIMIT 1) WHERE profile_id IS NULL", [])?;
        c.execute(
            "INSERT INTO resume_versions (job_id, created_at, kind, note, resume, ats, ats_score, template, pdf_path)
             SELECT id, updated_at, 'imported', 'Tailored before this app', resume, ats, ats_score, 'classic', pdf_path
             FROM jobs WHERE resume IS NOT NULL AND resume != 'null'
               AND NOT EXISTS (SELECT 1 FROM resume_versions v WHERE v.job_id = jobs.id)",
            [],
        )?;
        c.execute(
            "UPDATE jobs SET current_version = (SELECT MAX(id) FROM resume_versions v WHERE v.job_id = jobs.id)
             WHERE current_version IS NULL",
            [],
        )?;
        let legacy: Vec<(i64, String)> = c
            .prepare("SELECT id, logs FROM jobs WHERE logs IS NOT NULL AND logs != '[]'")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (id, logs) in legacy {
            let lines: Vec<Value> = serde_json::from_str(&logs).unwrap_or_default();
            for l in lines {
                c.execute(
                    "INSERT INTO job_log (job_id, ts, agent, level, message) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, l["ts"].as_str().unwrap_or(""), l["agent"].as_str().unwrap_or(""),
                            l["level"].as_str().unwrap_or("info"), l["message"].as_str().unwrap_or("")],
                )?;
            }
            c.execute("UPDATE jobs SET logs = '[]' WHERE id = ?1", params![id])?;
        }
        Ok(())
    }

    // ---------- jobs ----------

    pub fn job_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))?)
    }

    pub fn jobs(&self) -> Result<Vec<Job>> {
        let mut st = self.conn.prepare(&format!("SELECT {JOB_COLS} FROM jobs ORDER BY created_at DESC, id DESC"))?;
        let rows = st.query_map([], job_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn job(&self, id: i64) -> Result<Job> {
        self.conn
            .query_row(&format!("SELECT {JOB_COLS} FROM jobs WHERE id = ?1"), params![id], job_row)
            .optional()?
            .ok_or_else(|| anyhow!("application {id} no longer exists"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_job(&self, url: &str, raw_text: &str, company: &str, title: &str, location: &str,
                      profile_id: Option<i64>, source: &str) -> Result<i64> {
        let t = now();
        self.conn.execute(
            "INSERT INTO jobs (created_at, updated_at, url, raw_text, text_source, company, title, location, profile_id, source, stage, status)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', 'New')",
            params![t, url, raw_text, if raw_text.is_empty() { None } else { Some("pasted") }, company, title, location, profile_id, source],
        )?;
        let id = self.conn.last_insert_rowid();
        self.add_event(id, "created", "", if source == "scan" { "Found by the company scanner" } else { "Added" })?;
        Ok(id)
    }

    /// Set columns on a job. Strings and numbers are stored as-is, objects/arrays as JSON text, null clears.
    pub fn update_job(&self, id: i64, fields: &[(&str, Value)]) -> Result<()> {
        for (col, val) in fields {
            if !UPDATABLE.contains(col) {
                return Err(anyhow!("column {col} can't be updated"));
            }
            let sql = format!("UPDATE jobs SET {col} = ?1, updated_at = ?2 WHERE id = ?3");
            match val {
                Value::Null => self.conn.execute(&sql, params![Option::<String>::None, now(), id])?,
                Value::String(s) => self.conn.execute(&sql, params![s, now(), id])?,
                Value::Number(n) if n.is_i64() => self.conn.execute(&sql, params![n.as_i64(), now(), id])?,
                Value::Number(n) => self.conn.execute(&sql, params![n.as_f64(), now(), id])?,
                Value::Bool(b) => self.conn.execute(&sql, params![*b as i64, now(), id])?,
                other => self.conn.execute(&sql, params![other.to_string(), now(), id])?,
            };
        }
        Ok(())
    }

    pub fn delete_job(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM jobs WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_chat_memory(&self, id: i64, memory: &str, upto: i64) -> Result<()> {
        self.conn.execute("UPDATE jobs SET chat_memory = ?1, chat_memory_upto = ?2 WHERE id = ?3", params![memory, upto, id])?;
        Ok(())
    }

    /// Jobs left mid-run when the app last closed.
    pub fn unfinished(&self) -> Result<Vec<(i64, String)>> {
        let marks = RUNNING.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
        let mut st = self.conn.prepare(&format!("SELECT id, stage FROM jobs WHERE stage IN ({marks})"))?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Same company + title already tracked (normalised), for duplicate warnings.
    pub fn twin(&self, id: i64, company: &str, title: &str) -> Result<Option<Job>> {
        let key = |c: &str, t: &str| format!("{}|{}", norm_key(c), norm_key(t));
        let want = key(company, title);
        Ok(self.jobs()?.into_iter().find(|j| j.id != id && key(&j.company, &j.title) == want))
    }

    // ---------- events, log, chat ----------

    pub fn add_event(&self, job_id: i64, kind: &str, status: &str, text: &str) -> Result<()> {
        self.conn.execute("INSERT INTO job_events (job_id, ts, kind, status, text) VALUES (?1, ?2, ?3, ?4, ?5)",
                          params![job_id, now(), kind, status, text])?;
        Ok(())
    }

    pub fn events(&self, job_id: i64) -> Result<Vec<Event>> {
        let mut st = self.conn.prepare("SELECT ts, kind, status, text FROM job_events WHERE job_id = ?1 ORDER BY id")?;
        let rows = st.query_map(params![job_id], |r| Ok(Event { ts: text_col(r, 0), kind: text_col(r, 1), status: text_col(r, 2), text: text_col(r, 3) }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn log(&self, job_id: i64, agent: &str, level: &str, message: &str, data: Option<&Value>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO job_log (job_id, ts, agent, level, message, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![job_id, now(), agent, level, message, data.map(|d| d.to_string())],
        )?;
        Ok(())
    }

    pub fn logs(&self, job_id: i64) -> Result<Vec<LogLine>> {
        let mut st = self.conn.prepare("SELECT id, ts, agent, level, message, data FROM job_log WHERE job_id = ?1 ORDER BY id")?;
        let rows = st.query_map(params![job_id], |r| Ok(LogLine {
            id: r.get(0)?, ts: text_col(r, 1), agent: text_col(r, 2), level: text_col(r, 3), message: text_col(r, 4), data: json_col(r, 5),
        }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn add_chat(&self, job_id: i64, role: &str, content: &str, model: &str) -> Result<i64> {
        self.conn.execute("INSERT INTO job_chat (job_id, ts, role, content, model) VALUES (?1, ?2, ?3, ?4, ?5)",
                          params![job_id, now(), role, content, model])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn chat(&self, job_id: i64) -> Result<Vec<ChatMsg>> {
        let mut st = self.conn.prepare("SELECT id, ts, role, content, model, saved FROM job_chat WHERE job_id = ?1 ORDER BY id")?;
        let rows = st.query_map(params![job_id], chat_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// (job id, message) for one chat message.
    pub fn chat_msg(&self, id: i64) -> Result<(i64, ChatMsg)> {
        let job_id: i64 = self.conn.query_row("SELECT job_id FROM job_chat WHERE id = ?1", params![id], |r| r.get(0))?;
        let msg = self.conn.query_row("SELECT id, ts, role, content, model, saved FROM job_chat WHERE id = ?1", params![id], chat_row)?;
        Ok((job_id, msg))
    }

    pub fn set_chat_saved(&self, id: i64, saved: bool) -> Result<()> {
        self.conn.execute("UPDATE job_chat SET saved = ?1 WHERE id = ?2", params![saved as i64, id])?;
        Ok(())
    }

    // ---------- resume versions ----------

    #[allow(clippy::too_many_arguments)]
    pub fn add_version(&self, job_id: i64, kind: &str, note: &str, resume: &Value, ats: &Value, template: &str,
                       pdf_path: &str, model: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO resume_versions (job_id, created_at, kind, note, resume, ats, ats_score, template, pdf_path, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![job_id, now(), kind, note, resume.to_string(), ats.to_string(), ats["score"].as_i64(), template, pdf_path, model],
        )?;
        let vid = self.conn.last_insert_rowid();
        self.make_current(job_id, vid)?;
        Ok(vid)
    }

    /// Point the job (and its ATS score / PDF shortcut columns) at a version.
    pub fn make_current(&self, job_id: i64, version_id: i64) -> Result<()> {
        let v = self.version(version_id)?;
        if v.job_id != job_id {
            return Err(anyhow!("that version belongs to another application"));
        }
        self.update_job(job_id, &[
            ("current_version", json!(v.id)),
            ("resume", v.resume.clone()),
            ("ats", v.ats.clone()),
            ("ats_score", v.ats_score.map(Value::from).unwrap_or(Value::Null)),
            ("pdf_path", json!(v.pdf_path)),
        ])
    }

    pub fn versions(&self, job_id: i64) -> Result<Vec<Version>> {
        let mut st = self.conn.prepare(
            "SELECT id, job_id, created_at, kind, note, resume, ats, ats_score, template, pdf_path, model
             FROM resume_versions WHERE job_id = ?1 ORDER BY id DESC")?;
        let rows = st.query_map(params![job_id], version_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn version(&self, id: i64) -> Result<Version> {
        self.conn.query_row(
            "SELECT id, job_id, created_at, kind, note, resume, ats, ats_score, template, pdf_path, model
             FROM resume_versions WHERE id = ?1", params![id], version_row,
        ).optional()?.ok_or_else(|| anyhow!("resume version {id} no longer exists"))
    }

    // ---------- profiles ----------

    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let mut st = self.conn.prepare("SELECT id, name, cv_md, data, notes_md, template, active FROM profiles ORDER BY id")?;
        let rows = st.query_map([], profile_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn profile(&self, id: i64) -> Result<Profile> {
        self.conn.query_row("SELECT id, name, cv_md, data, notes_md, template, active FROM profiles WHERE id = ?1", params![id], profile_row)
            .optional()?.ok_or_else(|| anyhow!("profile {id} no longer exists"))
    }

    /// The profile new applications use. Creates an empty one on a fresh install.
    pub fn active_profile(&self) -> Result<Profile> {
        let id: Option<i64> = self.conn.query_row("SELECT id FROM profiles ORDER BY active DESC, id LIMIT 1", [], |r| r.get(0)).optional()?;
        match id {
            Some(id) => self.profile(id),
            None => {
                let id = self.create_profile("My profile")?;
                self.set_active_profile(id)?;
                self.profile(id)
            }
        }
    }

    pub fn create_profile(&self, name: &str) -> Result<i64> {
        self.conn.execute("INSERT INTO profiles (name, created_at, updated_at) VALUES (?1, ?2, ?2)", params![name, now()])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn duplicate_profile(&self, id: i64, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO profiles (name, cv_md, data, notes_md, template, created_at, updated_at)
             SELECT ?1, cv_md, data, notes_md, template, ?2, ?2 FROM profiles WHERE id = ?3",
            params![name, now(), id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn save_profile(&self, p: &Profile) -> Result<()> {
        self.conn.execute(
            "UPDATE profiles SET name = ?1, cv_md = ?2, data = ?3, notes_md = ?4, template = ?5, updated_at = ?6 WHERE id = ?7",
            params![p.name, p.cv_md, p.data.to_string(), p.notes_md, p.template, now(), p.id],
        )?;
        Ok(())
    }

    pub fn set_active_profile(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE profiles SET active = (id = ?1)", params![id])?;
        Ok(())
    }

    pub fn delete_profile(&self, id: i64) -> Result<()> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get(0))?;
        if n <= 1 {
            return Err(anyhow!("Keep at least one profile"));
        }
        self.conn.execute("DELETE FROM profiles WHERE id = ?1", params![id])?;
        self.conn.execute(
            "UPDATE profiles SET active = 1 WHERE id = (SELECT MIN(id) FROM profiles)
             AND NOT EXISTS (SELECT 1 FROM profiles WHERE active = 1)", [])?;
        Ok(())
    }

    // ---------- settings ----------

    pub fn setting(&self, key: &str) -> Result<Value> {
        let v: Option<String> = self.conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0)).optional()?;
        Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null))
    }

    pub fn set_setting(&self, key: &str, value: &Value) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
    }
}

fn chat_row(r: &Row) -> rusqlite::Result<ChatMsg> {
    Ok(ChatMsg {
        id: r.get(0)?, ts: text_col(r, 1), role: text_col(r, 2), content: text_col(r, 3), model: text_col(r, 4),
        saved: r.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
    })
}

fn version_row(r: &Row) -> rusqlite::Result<Version> {
    Ok(Version {
        id: r.get(0)?, job_id: r.get(1)?, created_at: text_col(r, 2), kind: text_col(r, 3), note: text_col(r, 4),
        resume: json_col(r, 5), ats: json_col(r, 6), ats_score: r.get(7)?, template: text_col(r, 8),
        pdf_path: text_col(r, 9), model: text_col(r, 10),
    })
}

fn profile_row(r: &Row) -> rusqlite::Result<Profile> {
    let data = json_col(r, 3);
    Ok(Profile {
        id: r.get(0)?, name: text_col(r, 1), cv_md: text_col(r, 2),
        data: if data.is_object() { data } else { json!({}) },
        notes_md: text_col(r, 4), template: text_col(r, 5), active: r.get::<_, i64>(6)? != 0,
    })
}

fn norm_key(s: &str) -> String {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_has_profile_and_versions() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let p = db.active_profile().unwrap();
        let id = db.insert_job("https://x", "", "Acme", "Engineer", "", Some(p.id), "manual").unwrap();
        let v = db.add_version(id, "tailored", "", &json!({"summary": "s"}), &json!({"score": 88}), "classic", "/tmp/a.pdf", "m").unwrap();
        let j = db.job(id).unwrap();
        assert_eq!(j.current_version, Some(v));
        assert_eq!(j.ats_score, Some(88));
        assert_eq!(db.versions(id).unwrap().len(), 1);
        db.log(id, "Tailor", "info", "hello", Some(&json!({"round": 1}))).unwrap();
        assert_eq!(db.logs(id).unwrap()[0].data["round"], 1);
        db.delete_job(id).unwrap();
        assert!(db.versions(id).unwrap().is_empty());
    }
}
