//! The candidate's document library and retrieval over it.
//!
//! Everything the agents may cite is stored as documents split into chunks: profile
//! documents (CV, notes, extra write-ups the user adds), answers they saved from the
//! Q&A chat, and each application's job description, tailored resume and cover letter.
//! Search is hybrid: SQLite FTS5 keyword ranking always, plus embedding similarity when
//! the AI provider offers embeddings, fused with reciprocal rank fusion.
use crate::ai::Ai;
use crate::db::{self, now, Db, Job, Profile, Store};
use anyhow::Result;
use rusqlite::params;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const CHUNK_CHARS: usize = 900;
const CANDIDATES: usize = 40;
const RRF_K: f32 = 60.0;

#[derive(Clone, Debug)]
pub struct Doc {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub job_id: Option<i64>,
    pub updated_at: String,
    pub chunks: i64,
    pub embedded: i64,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub kind: String,
    pub title: String,
    pub job_id: Option<i64>,
    pub text: String,
    pub score: f32,
}

/// Which documents a search may return.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub profile_id: i64,
    /// Leave out this application's own documents (they are given to the model in full).
    pub exclude_job: Option<i64>,
    /// Leave out these kinds (e.g. the CV, which the model already has).
    pub exclude_kinds: Vec<&'static str>,
}

fn hash(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Paragraph-packed chunks, each prefixed with its nearest markdown heading for context.
pub fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut heading = String::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>, heading: &str| {
        let body = cur.trim();
        if !body.is_empty() {
            out.push(if heading.is_empty() { body.to_string() } else { format!("{heading}\n{body}") });
        }
        cur.clear();
    };
    for mut para in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        if para.starts_with('#') {
            flush(&mut cur, &mut out, &heading);
            let (head, rest) = para.split_once('\n').unwrap_or((para, ""));
            heading = head.trim_start_matches('#').trim().to_string();
            para = rest.trim();
            if para.is_empty() {
                continue;
            }
        }
        if cur.len() + para.len() > CHUNK_CHARS && !cur.is_empty() {
            flush(&mut cur, &mut out, &heading);
        }
        if para.len() > CHUNK_CHARS {
            // One very long paragraph: split on line or sentence boundaries.
            let mut piece = String::new();
            for part in para.split_inclusive(['\n', '.']) {
                if piece.len() + part.len() > CHUNK_CHARS && !piece.is_empty() {
                    flush(&mut piece, &mut out, &heading);
                }
                piece.push_str(part);
            }
            flush(&mut piece, &mut out, &heading);
        } else {
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(para);
        }
    }
    flush(&mut cur, &mut out, &heading);
    out
}

/// Insert or replace the document identified by (kind, profile, job, title). Returns its id.
pub fn upsert(db: &Db, profile_id: Option<i64>, job_id: Option<i64>, kind: &str, title: &str, content: &str) -> Result<Option<i64>> {
    let existing: Option<(i64, String)> = db.conn.query_row(
        "SELECT id, hash FROM documents WHERE kind = ?1 AND profile_id IS ?2 AND job_id IS ?3 AND title = ?4",
        params![kind, profile_id, job_id, title], |r| Ok((r.get(0)?, r.get(1)?)),
    ).ok();
    let content = content.trim();
    if content.is_empty() {
        if let Some((id, _)) = existing {
            db.conn.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
        }
        return Ok(None);
    }
    let h = hash(content);
    let id = match existing {
        Some((id, old)) if old == h => return Ok(Some(id)),
        Some((id, _)) => {
            db.conn.execute("UPDATE documents SET content = ?1, hash = ?2, updated_at = ?3 WHERE id = ?4", params![content, h, now(), id])?;
            db.conn.execute("DELETE FROM chunks WHERE document_id = ?1", params![id])?;
            id
        }
        None => {
            db.conn.execute(
                "INSERT INTO documents (profile_id, job_id, kind, title, content, hash, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![profile_id, job_id, kind, title, content, h, now()],
            )?;
            db.conn.last_insert_rowid()
        }
    };
    for (i, c) in chunk(content).iter().enumerate() {
        db.conn.execute("INSERT INTO chunks (document_id, ord, text) VALUES (?1, ?2, ?3)", params![id, i as i64, c])?;
    }
    Ok(Some(id))
}

pub fn delete_document(db: &Db, id: i64) -> Result<()> {
    db.conn.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
    Ok(())
}

/// The documents a profile owns that the user manages (not per-application artefacts).
pub fn library(db: &Db, profile_id: i64) -> Result<Vec<Doc>> {
    let mut st = db.conn.prepare(
        "SELECT d.id, d.kind, d.title, d.content, d.job_id, d.updated_at,
                (SELECT COUNT(*) FROM chunks c WHERE c.document_id = d.id),
                (SELECT COUNT(*) FROM chunks c WHERE c.document_id = d.id AND c.embedding IS NOT NULL)
         FROM documents d WHERE d.profile_id = ?1 AND d.kind IN ('doc', 'answer')
         ORDER BY d.kind, d.updated_at DESC")?;
    let rows = st.query_map(params![profile_id], |r| Ok(Doc {
        id: r.get(0)?, kind: r.get(1)?, title: r.get(2)?, content: r.get(3)?, job_id: r.get(4)?,
        updated_at: r.get(5)?, chunks: r.get(6)?, embedded: r.get(7)?,
    }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// (chunks, chunks with embeddings from `model`) across the library, for the status line.
pub fn index_status(db: &Db, model: &str) -> Result<(i64, i64)> {
    Ok(db.conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(embedding IS NOT NULL AND embed_model = ?1), 0) FROM chunks", params![model],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

// ---------- keeping documents in sync with profiles and applications ----------

/// The profile's CV, structured facts and notes as searchable documents.
pub fn sync_profile(db: &Db, p: &Profile) -> Result<()> {
    upsert(db, Some(p.id), None, "cv", "Base CV", &p.cv_md)?;
    upsert(db, Some(p.id), None, "notes", "Notes for the AI", &p.notes_md)?;
    let facts = if p.data.as_object().is_some_and(|o| !o.is_empty()) { yaml_like(&p.data, 0) } else { String::new() };
    upsert(db, Some(p.id), None, "profile", "Profile facts", &facts)?;
    Ok(())
}

/// An application's job description, current resume and cover letter.
/// The searchable text of a job description: title line, summary, then the requirement lists.
pub fn jd_text(title: &str, company: &str, jd: &Value) -> String {
    let mut text = String::new();
    if !jd.is_null() {
        text.push_str(&format!("# {title} at {company}\n\n{}\n", jd["summary"].as_str().unwrap_or("")));
        for (k, head) in [("responsibilities", "Responsibilities"), ("requirements_must", "Must have"), ("requirements_nice", "Nice to have")] {
            let items = crate::resume::strs(&jd[k]);
            if !items.is_empty() {
                text.push_str(&format!("\n## {head}\n\n- {}\n", items.join("\n- ")));
            }
        }
    }
    text
}

/// The earlier application whose job description reads most like this one, with its similarity
/// (cosine of the built-in embeddings of the opening passage). Catches the same role re-posted
/// under a new link or a slightly different title.
pub async fn similar_job(store: &Store, job_id: i64, title: &str, company: &str, jd: &Value) -> Option<(i64, f32)> {
    let text = chunk(&jd_text(title, company, jd)).into_iter().next()?;
    let v = crate::ai::builtin_embed(&[text]).await.ok()?.pop()?;
    let db = db::lock(store);
    let mut st = db.conn.prepare(
        "SELECT d.job_id, c.embedding FROM documents d JOIN chunks c ON c.document_id = d.id
         WHERE d.kind = 'jd' AND c.ord = 0 AND d.job_id IS NOT NULL AND d.job_id != ?1
           AND c.embedding IS NOT NULL AND c.embed_model = ?2").ok()?;
    let rows = st.query_map(params![job_id, crate::embed::MODEL_ID], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))).ok()?;
    rows.filter_map(|r| r.ok())
        .map(|(id, b)| (id, cosine(&v, &from_blob(&b))))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

pub fn sync_job(db: &Db, j: &Job) -> Result<()> {
    let label = format!("{} at {}", j.title, j.company);
    let jd_text = jd_text(&j.title, &j.company, &j.jd);
    upsert(db, j.profile_id, Some(j.id), "jd", &format!("Job: {label}"), &jd_text)?;
    let resume_text = serde_json::from_value::<crate::resume::Resume>(
        db.job_resume(j.id)?.unwrap_or(Value::Null)).map(|r| resume_text(&r)).unwrap_or_default();
    upsert(db, j.profile_id, Some(j.id), "resume", &format!("Resume for {label}"), &resume_text)?;
    upsert(db, j.profile_id, Some(j.id), "cover", &format!("Cover letter for {label}"), &j.cover_letter)?;
    Ok(())
}

/// Save a Q&A answer to the library so later applications can reuse it.
pub fn save_answer(db: &Db, profile_id: i64, job: &Job, question: &str, answer: &str, msg_id: i64) -> Result<()> {
    let title = format!("Answer #{msg_id}: {}", question.chars().take(80).collect::<String>());
    let body = format!("Question ({} at {}): {question}\n\nAnswer: {answer}", job.title, job.company);
    upsert(db, Some(profile_id), Some(job.id), "answer", &title, &body)?;
    Ok(())
}

pub fn unsave_answer(db: &Db, msg_id: i64) -> Result<()> {
    db.conn.execute("DELETE FROM documents WHERE kind = 'answer' AND title LIKE ?1", params![format!("Answer #{msg_id}:%")])?;
    Ok(())
}

pub fn resume_text(r: &crate::resume::Resume) -> String {
    let mut s = format!("## Summary\n\n{}\n", r.summary);
    if !r.competencies.is_empty() {
        s.push_str(&format!("\n## Core competencies\n\n{}\n", r.competencies.join(", ")));
    }
    for e in &r.experience {
        s.push_str(&format!("\n## {} at {} ({})\n\n", e.role, e.company, e.dates));
        for b in &e.bullets {
            s.push_str(&format!("- {b}\n"));
        }
    }
    for p in &r.projects {
        s.push_str(&format!("\n## Project: {}\n\n{} ({})\n", p.name, p.description, p.tech));
    }
    for g in &r.skills {
        s.push_str(&format!("\n{}: {}", g.category, g.items.join(", ")));
    }
    s
}

/// Readable nested key: value text for the profile facts.
fn yaml_like(v: &Value, depth: usize) -> String {
    let pad = "  ".repeat(depth);
    match v {
        Value::Object(o) => o.iter()
            .filter(|(_, x)| !matches!(x, Value::Null) && x.as_str() != Some("") && !x.as_array().is_some_and(|a| a.is_empty()))
            .map(|(k, x)| match x {
                Value::Object(_) | Value::Array(_) => format!("{pad}{k}:\n{}", yaml_like(x, depth + 1)),
                _ => format!("{pad}{k}: {}", scalar(x)),
            })
            .collect::<Vec<_>>().join("\n"),
        Value::Array(a) => a.iter().map(|x| match x {
            Value::Object(_) => format!("{pad}-\n{}", yaml_like(x, depth + 1)),
            _ => format!("{pad}- {}", scalar(x)),
        }).collect::<Vec<_>>().join("\n"),
        other => format!("{pad}{}", scalar(other)),
    }
}

fn scalar(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
}

pub fn profile_facts(p: &Profile) -> String {
    yaml_like(&p.data, 0)
}

// ---------- embeddings ----------

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

/// Embeddings from the user's AI settings, or the built-in model when no AI is connected yet.
/// Returns the vectors and the model id they belong to; None when semantic search is off.
async fn embed_texts(ai: Option<&Ai>, texts: &[String]) -> Result<Option<(Vec<Vec<f32>>, String)>> {
    match ai {
        Some(ai) => Ok(ai.embed(texts).await?.map(|v| (v, ai.settings.embed_model.clone()))),
        None => Ok(Some((crate::ai::builtin_embed(texts).await?, crate::embed::MODEL_ID.to_string()))),
    }
}

fn embed_model_of(ai: Option<&Ai>) -> String {
    ai.map(|a| a.settings.embed_model.clone()).unwrap_or_else(|| crate::embed::MODEL_ID.to_string())
}

/// The AI client from saved settings, if one is configured.
pub fn saved_ai(store: &Store) -> Option<Ai> {
    let settings: crate::ai::AiSettings = serde_json::from_value(db::lock(store).setting("ai").ok()?).ok()?;
    Ai::new(settings, None).ok()
}

/// Embed pending passages with whatever the saved settings say (built-in by default).
pub async fn embed_builtin(store: &Store) -> Result<usize> {
    let ai = saved_ai(store);
    embed_pending(store, ai.as_ref()).await
}

/// Embed every chunk that lacks a vector from the current embedding model. Returns how many.
pub async fn embed_pending(store: &Store, ai: Option<&Ai>) -> Result<usize> {
    embed_pending_progress(store, ai, &|_, _| {}).await
}

/// Passages still waiting for a vector from the current embedding model.
pub fn pending_count(store: &Store, ai: Option<&Ai>) -> usize {
    let model = embed_model_of(ai);
    db::lock(store).conn.query_row(
        "SELECT COUNT(*) FROM chunks WHERE embedding IS NULL OR embed_model IS NOT ?1", params![model], |r| r.get::<_, i64>(0),
    ).unwrap_or(0) as usize
}

/// As `embed_pending`, reporting (done, total) after each batch.
pub async fn embed_pending_progress(store: &Store, ai: Option<&Ai>, progress: &(dyn Fn(usize, usize) + Sync)) -> Result<usize> {
    let model = embed_model_of(ai);
    if model.is_empty() {
        return Ok(0);
    }
    let total = pending_count(store, ai);
    let mut done = 0;
    loop {
        let batch: Vec<(i64, String)> = {
            let db = db::lock(store);
            let mut st = db.conn.prepare(
                "SELECT id, text FROM chunks WHERE embedding IS NULL OR embed_model IS NOT ?1 ORDER BY id LIMIT 32")?;
            let rows = st.query_map(params![model], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        if batch.is_empty() {
            return Ok(done);
        }
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        let Some((vecs, _)) = embed_texts(ai, &texts).await? else { return Ok(done) };
        let db = db::lock(store);
        for ((id, _), v) in batch.iter().zip(vecs) {
            db.conn.execute("UPDATE chunks SET embedding = ?1, embed_model = ?2 WHERE id = ?3", params![to_blob(&v), model, id])?;
        }
        drop(db);
        done += batch.len();
        progress(done.min(total.max(done)), total.max(done));
    }
}

// ---------- search ----------

const STOP: [&str; 30] = ["the", "and", "for", "you", "your", "with", "what", "why", "how", "are", "this", "that", "have", "has",
    "will", "would", "can", "our", "about", "from", "into", "they", "them", "who", "was", "were", "describe", "tell", "please", "any"];

/// An FTS5 query that ORs the meaningful words (quoted, so user text can't inject syntax).
fn fts_query(q: &str) -> String {
    let mut seen = Vec::new();
    for w in q.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '+' && c != '#') {
        let w = w.trim_matches(|c: char| c == '+' || c == '#');
        if w.chars().count() >= 2 && !STOP.contains(&w) && !seen.iter().any(|s: &String| s == w) {
            seen.push(w.to_string());
        }
    }
    seen.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(" OR ")
}

fn scope_sql(scope: &Scope) -> String {
    let mut sql = format!("d.profile_id = {}", scope.profile_id);
    if let Some(j) = scope.exclude_job {
        sql.push_str(&format!(" AND (d.job_id IS NULL OR d.job_id != {j})"));
    }
    for k in &scope.exclude_kinds {
        sql.push_str(&format!(" AND d.kind != '{k}'"));
    }
    sql
}

/// Top `k` chunks for the query: keyword and (when given) vector rankings fused with RRF.
pub fn search(db: &Db, query: &str, query_vec: Option<&[f32]>, embed_model: &str, scope: &Scope, k: usize) -> Result<Vec<Hit>> {
    let filter = scope_sql(scope);
    let mut fused: HashMap<i64, f32> = HashMap::new();

    let fq = fts_query(query);
    if !fq.is_empty() {
        let mut st = db.conn.prepare(&format!(
            "SELECT c.id FROM chunks_fts f JOIN chunks c ON c.id = f.rowid JOIN documents d ON d.id = c.document_id
             WHERE chunks_fts MATCH ?1 AND {filter} ORDER BY bm25(chunks_fts) LIMIT {CANDIDATES}"))?;
        let ids: Vec<i64> = st.query_map(params![fq], |r| r.get(0))?.collect::<Result<_, _>>()?;
        for (rank, id) in ids.iter().enumerate() {
            *fused.entry(*id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
    }

    if let Some(qv) = query_vec {
        let mut st = db.conn.prepare(&format!(
            "SELECT c.id, c.embedding FROM chunks c JOIN documents d ON d.id = c.document_id
             WHERE c.embedding IS NOT NULL AND c.embed_model = ?1 AND {filter}"))?;
        let mut scored: Vec<(i64, f32)> = st
            .query_map(params![embed_model], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .filter_map(|r| r.ok())
            .map(|(id, b)| (id, cosine(qv, &from_blob(&b))))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (rank, (id, sim)) in scored.iter().take(CANDIDATES).enumerate() {
            if *sim > 0.2 {
                *fused.entry(*id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
            }
        }
    }

    let mut ranked: Vec<(i64, f32)> = fused.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut hits = Vec::new();
    for (id, score) in ranked.into_iter().take(k) {
        let hit = db.conn.query_row(
            "SELECT d.kind, d.title, d.job_id, c.text FROM chunks c JOIN documents d ON d.id = c.document_id WHERE c.id = ?1",
            params![id], |r| Ok(Hit { kind: r.get(0)?, title: r.get(1)?, job_id: r.get(2)?, text: r.get(3)?, score }),
        )?;
        hits.push(hit);
    }
    Ok(hits)
}

/// Retrieve for a question: embeds the query when possible (outside the DB lock), then searches.
pub async fn retrieve(store: &Store, ai: Option<&Ai>, query: &str, scope: &Scope, k: usize) -> Vec<Hit> {
    let mut qv = None;
    let mut model = String::new();
    if let Ok(Some((mut v, m))) = embed_texts(ai, &[query.to_string()]).await {
        qv = v.pop();
        model = m;
    }
    let db = db::lock(store);
    search(&db, query, qv.as_deref(), &model, scope, k).unwrap_or_default()
}

/// Retrieved passages formatted for a prompt.
pub fn format_hits(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "(nothing relevant found)".into();
    }
    hits.iter().map(|h| format!("--- [{}] {} ---\n{}", h.kind, h.title, h.text)).collect::<Vec<_>>().join("\n\n")
}

impl Db {
    /// The current tailored resume JSON of a job, if any.
    pub fn job_resume(&self, id: i64) -> Result<Option<Value>> {
        let s: Option<String> = self.conn.query_row("SELECT resume FROM jobs WHERE id = ?1", params![id], |r| r.get(0))?;
        Ok(s.and_then(|s| serde_json::from_str(&s).ok()).filter(|v: &Value| v.is_object()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn hybrid_search_finds_library_text_and_respects_scope() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let p = db.active_profile().unwrap();
        upsert(&db, Some(p.id), None, "doc", "Projects", "## RAG platform\n\nBuilt a retrieval-augmented generation platform at Acme serving 4,000 users.\n\n## Hobbies\n\nClimbing and chess.").unwrap();
        let job = db.insert_job("", "", "Acme", "ML Engineer", "", Some(p.id), "manual").unwrap();
        upsert(&db, Some(p.id), Some(job), "answer", "Answer #1: Why us", "Question: why Acme? Answer: retrieval work.").unwrap();
        let scope = Scope { profile_id: p.id, exclude_job: Some(job), exclude_kinds: vec!["cv"] };
        let hits = search(&db, "Describe your retrieval platform experience", None, "", &scope, 5).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].text.contains("retrieval-augmented"));
        assert!(hits.iter().all(|h| h.job_id != Some(job)));
        // Unchanged content is not re-chunked; changed content replaces old chunks.
        let id = upsert(&db, Some(p.id), None, "doc", "Projects", "Only chess now.").unwrap().unwrap();
        let n: i64 = db.conn.query_row("SELECT COUNT(*) FROM chunks WHERE document_id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert!(search(&db, "retrieval platform", None, "", &scope, 5).unwrap().is_empty());
    }

    #[test]
    fn fts_query_is_quoted() {
        assert_eq!(fts_query("Why C++ and \"NEAR\"(x)?"), "\"near\"");
        assert_eq!(fts_query("node.js kubernetes"), "\"node\" OR \"js\" OR \"kubernetes\"");
    }

    #[test]
    fn chunks_carry_headings() {
        let c = chunk("# Name\n\n## Experience\n\nDid things.\n\nMore things.");
        assert!(c.iter().any(|x| x.starts_with("Experience\nDid things.")));
    }
}
