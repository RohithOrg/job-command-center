//! One-time import of an export from the server version (`import.json` in the data folder).
use crate::db::Db;
use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::Value;
use std::path::Path;

fn s(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(x) => Some(x.clone()),
        other => Some(other.to_string()),
    }
}

fn js(v: &Value) -> Option<String> {
    if v.is_null() { None } else { Some(v.to_string()) }
}

/// Imports `<data>/import.json` if present and the database is empty. Returns jobs imported.
pub fn run(db: &mut Db, data_dir: &Path) -> Result<usize> {
    let file = data_dir.join("import.json");
    if !file.exists() || db.job_count()? > 0 {
        return Ok(0);
    }
    let export: Value = serde_json::from_str(&std::fs::read_to_string(&file)?).context("import.json is not valid JSON")?;
    let tx = db.conn.transaction()?;

    for j in export["jobs"].as_array().into_iter().flatten() {
        let pdf = j["pdf_file"].as_str().map(|f| data_dir.join("output").join(f).to_string_lossy().to_string());
        tx.execute(
            "INSERT INTO jobs (id, created_at, updated_at, url, company, title, location, raw_text, text_source, jd,
             evaluation, score, resume, ats, ats_score, pdf_path, cover_letter, stage, status, notes, error, logs, source,
             applied_at, next_step, next_step_date)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26)",
            params![
                j["id"].as_i64(), s(&j["created_at"]), s(&j["updated_at"]), s(&j["url"]), s(&j["company"]), s(&j["title"]),
                s(&j["location"]), s(&j["raw_text"]), s(&j["text_source"]), js(&j["jd"]), js(&j["evaluation"]),
                j["score"].as_f64(), js(&j["resume"]), js(&j["ats"]), j["ats_score"].as_i64(), pdf, s(&j["cover_letter"]),
                // anything mid-run on the server is shown as ready here
                if matches!(j["stage"].as_str(), Some("ready" | "failed" | "imported")) { s(&j["stage"]) } else { Some("ready".into()) },
                s(&j["status"]), s(&j["notes"]), s(&j["error"]),
                js(&j["logs"]).unwrap_or_else(|| "[]".into()), s(&j["source"]), s(&j["applied_at"]),
                s(&j["next_step"]), s(&j["next_step_date"]),
            ],
        )?;
    }
    for e in export["events"].as_array().into_iter().flatten() {
        tx.execute("INSERT INTO job_events (job_id, ts, kind, status, text) VALUES (?1,?2,?3,?4,?5)",
            params![e["job_id"].as_i64(), s(&e["ts"]), s(&e["kind"]), s(&e["status"]), s(&e["text"])])?;
    }
    for m in export["chat"].as_array().into_iter().flatten() {
        tx.execute("INSERT INTO job_chat (job_id, ts, role, content, model) VALUES (?1,?2,?3,?4,?5)",
            params![m["job_id"].as_i64(), s(&m["ts"]), s(&m["role"]), s(&m["content"]), s(&m["model"])])?;
    }
    for c in export["companies"].as_array().into_iter().flatten() {
        tx.execute(
            "INSERT INTO companies (name, careers_url, api_url, notes, enabled, favorite, last_scan_at, last_status, last_found, last_postings)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![s(&c["name"]), s(&c["careers_url"]), s(&c["api_url"]), s(&c["notes"]), c["enabled"].as_i64().unwrap_or(1),
                    c["favorite"].as_i64().unwrap_or(0), s(&c["last_scan_at"]), s(&c["last_status"]), c["last_found"].as_i64(),
                    js(&c["last_postings"])],
        )?;
    }
    for d in export["discovered"].as_array().into_iter().flatten() {
        tx.execute(
            "INSERT OR IGNORE INTO discovered (url, url_key, company, title, location, first_seen, state, reason, job_id, source)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![s(&d["url"]), s(&d["url_key"]), s(&d["company"]), s(&d["title"]), s(&d["location"]), s(&d["first_seen"]),
                    s(&d["state"]), s(&d["reason"]), d["job_id"].as_i64(), s(&d["source"])],
        )?;
    }
    if let Some(p) = export.get("profile") {
        tx.execute("INSERT OR REPLACE INTO profile (id, cv_md, data, notes_md) VALUES (1, ?1, ?2, ?3)",
            params![p["cv_md"].as_str().unwrap_or(""), p["data"].to_string(), p["notes_md"].as_str().unwrap_or("")])?;
    }
    if let Some(obj) = export["settings"].as_object() {
        for (k, v) in obj {
            if k != "ai" { // AI keys never travel in an export; connect the provider again in Settings
                tx.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", params![k, v.to_string()])?;
            }
        }
    }
    tx.commit()?;
    let n = export["jobs"].as_array().map(|a| a.len()).unwrap_or(0);
    std::fs::rename(&file, data_dir.join("import.json.done"))?;
    Ok(n)
}
