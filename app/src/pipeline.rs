//! The agent pipeline: Scout -> Extractor -> Evaluator -> Skill Gap -> (Tailor <-> ATS Critic) -> Writer.
//!
//! Each job runs as a background task. Every agent writes to the job's log (with structured
//! data for the ATS rounds) so the app shows the work live. The Tailor/ATS loop is the agentic
//! core: the critic renders the real PDF, reads it back the way an ATS does, and sends concrete
//! feedback (missing keywords, unsupported claims and metrics, dropped roles) to the tailor until
//! the target score is reached or nothing fixable remains. Every result is kept as a resume version.
use crate::ai::{Ai, AiSettings};
use crate::ats::{self, Keyword};
use crate::db::{self, Job, Profile, Store};
use crate::fetch::{self, FetchError};
use crate::pdf::{self, PageSize};
use crate::prompts;
use crate::rag::{self, Scope};
use crate::resume::{strs, Contact, Header, Resume};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;

pub const ATS_TARGET: i64 = 85;
/// Below this fit score the agents stop after evaluating (career-ops: under 3.5 "only with a reason").
const MIN_FIT_TO_TAILOR: f64 = 3.0;
const MAX_ROUNDS: usize = 3;
const MIN_FETCHED_CHARS: usize = 700;

/// Shared by every background task: storage, where PDFs go, and how to tell the UI something changed.
#[derive(Clone)]
pub struct Ctx {
    pub store: Store,
    pub out_dir: PathBuf,
    pub notify: Arc<dyn Fn(i64) + Send + Sync>,
    sem: Arc<Semaphore>,
}

impl Ctx {
    pub fn new(store: Store, out_dir: PathBuf, notify: Arc<dyn Fn(i64) + Send + Sync>) -> Ctx {
        Ctx { store, out_dir, notify, sem: Arc::new(Semaphore::new(2)) }
    }

    pub fn ai(&self) -> Result<Ai> {
        let settings: AiSettings = serde_json::from_value(db::lock(&self.store).setting("ai")?).unwrap_or_default();
        if settings.provider.is_empty() {
            bail!("No AI connected yet. Open Settings and connect one.");
        }
        Ai::new(settings, None)
    }

    fn note(&self, job: i64, agent: &str, level: &str, msg: &str, data: Option<&Value>) {
        let _ = db::lock(&self.store).log(job, agent, level, msg, data);
        (self.notify)(job);
    }

    fn update(&self, job: i64, fields: &[(&str, Value)]) -> Result<()> {
        db::lock(&self.store).update_job(job, fields)?;
        (self.notify)(job);
        Ok(())
    }

    fn job(&self, id: i64) -> Result<Job> {
        db::lock(&self.store).job(id)
    }

    fn profile_for(&self, job: &Job) -> Result<Profile> {
        let db = db::lock(&self.store);
        match job.profile_id {
            Some(id) => db.profile(id).or_else(|_| db.active_profile()),
            None => db.active_profile(),
        }
    }
}

// ---------- what the agents know about the candidate ----------

/// Everything that counts as fact about the candidate: CV, profile facts, notes and library documents.
fn evidence(ctx: &Ctx, p: &Profile) -> String {
    let db = db::lock(&ctx.store);
    let docs: Vec<String> = rag::library(&db, p.id).unwrap_or_default().into_iter()
        .filter(|d| d.kind == "doc").map(|d| d.content).collect();
    format!("{}\n\n{}\n\n{}\n\n{}", p.cv_md, rag::profile_facts(p), p.notes_md, docs.join("\n\n"))
}

/// The candidate block every agent gets, plus library passages relevant to `focus`.
async fn context_block(ctx: &Ctx, ai: &Ai, p: &Profile, focus: &str) -> String {
    let scope = Scope { profile_id: p.id, exclude_job: None, exclude_kinds: vec!["cv", "notes", "profile", "jd", "resume", "cover", "answer"] };
    let hits = rag::retrieve(&ctx.store, Some(ai), focus, &scope, 6).await;
    let facts = rag::profile_facts(p);
    format!(
        "=== BASE CV — the main source of facts about the candidate ===\n{}\n\n\
         === PROFILE — identity, targets, location, work authorization ===\n{}\n\n\
         === CANDIDATE NOTES — framing, priorities, location policy ===\n{}\n\n\
         === FROM THE CANDIDATE'S DOCUMENT LIBRARY — also facts about them ===\n{}\n",
        p.cv_md,
        if facts.is_empty() { "(not filled in)".into() } else { facts },
        if p.notes_md.trim().is_empty() { "(none)" } else { p.notes_md.trim() },
        rag::format_hits(&hits),
    )
}

pub fn header(p: &Profile) -> Header {
    let c = &p.data["candidate"];
    let get = |k: &str| c[k].as_str().unwrap_or("").trim().to_string();
    let mut contacts = Vec::new();
    if !get("phone").is_empty() {
        contacts.push(Contact { text: get("phone"), url: format!("tel:{}", get("phone").replace(' ', "")) });
    }
    if !get("email").is_empty() {
        contacts.push(Contact { text: get("email"), url: format!("mailto:{}", get("email")) });
    }
    for k in ["linkedin", "github", "portfolio_url"] {
        let v = get(k);
        if !v.is_empty() {
            let url = if v.starts_with("http") { v.clone() } else { format!("https://{v}") };
            let display = v.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.").trim_end_matches('/').to_string();
            contacts.push(Contact { text: display, url });
        }
    }
    if !get("location").is_empty() {
        contacts.push(Contact { text: get("location"), url: String::new() });
    }
    Header { name: get("full_name"), headline: p.data["narrative"]["headline"].as_str().unwrap_or("").to_string(), contacts }
}

/// Roles from CV headings like "### Data Scientist — Acme, Germany".
fn base_roles(cv: &str) -> Vec<(String, String)> {
    cv.lines().filter_map(|l| {
        let l = l.trim().strip_prefix("### ")?;
        let (role, company) = l.split_once(" — ").or_else(|| l.split_once(" – ")).or_else(|| l.split_once(" - "))?;
        Some((role.trim().to_string(), company.trim().to_string()))
    }).collect()
}

fn missing_roles(resume: &Resume, cv: &str) -> Vec<String> {
    let have: Vec<String> = resume.experience.iter().map(|e| format!("{} {}", e.role, e.company).to_lowercase()).collect();
    base_roles(cv).into_iter().filter_map(|(role, company)| {
        let company_word = company.to_lowercase().split(|c: char| c.is_whitespace() || ",/(".contains(c)).next().unwrap_or("").to_string();
        let role_word = role.to_lowercase().split_whitespace().next().unwrap_or("").to_string();
        (!company_word.is_empty() && !have.iter().any(|h| h.contains(&company_word) && h.contains(&role_word)))
            .then(|| format!("{role} — {company}"))
    }).collect()
}

pub fn page_size(jd: &Value) -> PageSize {
    let place = format!("{} {}", jd["country"].as_str().unwrap_or(""), jd["location"].as_str().unwrap_or("")).to_lowercase();
    if ["united states", "usa", "canada", ", us"].iter().any(|c| place.contains(c)) { PageSize::Letter } else { PageSize::A4 }
}

fn trimmed(v: &Value, keys: &[&str]) -> Value {
    Value::Object(keys.iter().filter_map(|k| v.get(*k).map(|x| (k.to_string(), x.clone()))).collect())
}

// ---------- rendering + ATS critic ----------

pub struct Checked {
    pub pdf_path: String,
    pub report: Value,
}

/// Render the resume with a template, read the PDF back like an ATS, and score it.
pub async fn check_resume(ctx: &Ctx, job: &Job, p: &Profile, resume: &Resume, template: &str) -> Result<Checked> {
    // Readable names: Resumes/TomTom – Senior Applied Scientist/Ana Lima – TomTom – Senior Applied Scientist (v2, Modern).pdf
    let folder = if job.company.is_empty() && job.title.is_empty() {
        format!("Application {}", job.id)
    } else {
        file_safe(&format!("{} – {}", job.company, job.title))
    };
    let dir = ctx.out_dir.join(folder);
    std::fs::create_dir_all(&dir)?;
    let n = db::lock(&ctx.store).versions(job.id)?.len() + 1;
    let design = pdf::TEMPLATES.iter().find(|t| t.id == template).map(|t| t.name).unwrap_or("Classic");
    let who = header(p).name;
    let path = dir.join(file_safe(&format!("{}{} – {} (v{n}, {design}).pdf",
        if who.is_empty() { String::new() } else { format!("{who} – ") }, job.company, job.title)));
    let (resume_c, header_c, template_c, size) = (resume.clone(), header(p), template.to_string(), page_size(&job.jd));
    let (bytes, text, pages) = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String, usize)> {
        let bytes = pdf::render(&resume_c, &header_c, &template_c, size)?;
        let (text, pages) = pdf::extract_text(&bytes)?;
        Ok((bytes, text, pages))
    }).await??;
    std::fs::write(&path, bytes)?;
    let support: Option<HashMap<String, bool>> = serde_json::from_value(job.jd["keyword_support"].clone()).ok();
    let mut report = ats::check(&text, pages, &Keyword::from_jd(&job.jd), &evidence(ctx, p), &p.email(),
                                &strs(&job.evaluation["skill_gaps"]), support.as_ref());
    report["missing_roles"] = json!(missing_roles(resume, &p.cv_md));
    Ok(Checked { pdf_path: path.to_string_lossy().to_string(), report })
}

/// A file or folder name that's valid on every OS, at most 120 characters.
fn file_safe(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if r#"/\:*?"<>|"#.contains(c) || c.is_control() { ' ' } else { c }).collect();
    let squeezed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = squeezed.trim_matches(|c: char| c == '.' || c == ' ');
    trimmed.chars().take(120).collect::<String>().trim_end().to_string()
}

fn report_line(r: &Value) -> String {
    let mut s = format!("ATS {}/100, keywords {}%, {} page(s)", r["score"], r["coverage"], r["pages"]);
    let missing = strs(&r["missing"]);
    if !missing.is_empty() {
        s.push_str(&format!(". Missing: {}", missing.iter().take(8).cloned().collect::<Vec<_>>().join(", ")));
    }
    s
}

fn is_clean(r: &Value) -> bool {
    strs(&r["unsupported_numbers"]).is_empty() && strs(&r["unsupported_keywords"]).is_empty() && strs(&r["missing_roles"]).is_empty()
}

// ---------- agents ----------

async fn scout(ctx: &Ctx, job: &Job) -> Result<String> {
    if !job.raw_text.trim().is_empty() && (job.url.is_empty() || job.raw_text.len() >= MIN_FETCHED_CHARS) {
        ctx.note(job.id, "Scout", "info", &format!("Using the job description text ({} characters)", job.raw_text.len()), None);
        return Ok(job.raw_text.clone());
    }
    if job.url.is_empty() {
        bail!("The job description is too short. Paste the full posting text.");
    }
    ctx.note(job.id, "Scout", "info", &format!("Reading the posting at {}", job.url), None);
    let page = fetch::fetch_job(&job.url).await.map_err(|e| match e {
        FetchError::Closed(m) => anyhow!(Closed(m)),
        other => anyhow!("{other}"),
    })?;
    if page.text.len() < MIN_FETCHED_CHARS {
        bail!("Couldn't read the job description from this link (a login wall or bot protection). \
               Open the posting, copy the description and paste it instead.");
    }
    let mut fields = vec![("raw_text", json!(page.text)), ("text_source", json!("fetched"))];
    for (k, v) in [("company", &page.company), ("title", &page.title), ("location", &page.location)] {
        if !v.is_empty() && job_field(job, k).is_empty() {
            fields.push((k, json!(v)));
        }
    }
    ctx.update(job.id, &fields)?;
    ctx.note(job.id, "Scout", "info", &format!("Captured {} characters of posting text", page.text.len()), None);
    Ok(page.text)
}

fn job_field<'a>(job: &'a Job, k: &str) -> &'a str {
    match k {
        "company" => &job.company,
        "title" => &job.title,
        _ => &job.location,
    }
}

/// A posting that no longer accepts applications.
#[derive(Debug)]
struct Closed(String);
impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Closed {}

async fn extract(ctx: &Ctx, ai: &Ai, job: &Job, raw: &str) -> Result<Value> {
    ctx.note(job.id, "Extractor", "info", "Reading the posting: requirements and the keywords an ATS searches for", None);
    let (mut jd, model) = ai.chat_json(&prompts::extractor(), &format!("JOB POSTING:\n\n{raw}"), 0.2).await?;
    let mut seen = Vec::new();
    jd["keywords"] = Value::Array(jd["keywords"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|k| {
        let k = if k.is_string() { json!({"term": k}) } else { k };
        let term = k["term"].as_str()?.trim().to_string();
        if term.is_empty() || seen.contains(&term.to_lowercase()) {
            return None;
        }
        seen.push(term.to_lowercase());
        let imp = if k["importance"].as_str().unwrap_or("").to_lowercase().starts_with("pref") { "preferred" } else { "required" };
        Some(json!({"term": term, "importance": imp, "aliases": strs(&k["aliases"])}))
    }).collect());
    for key in ["responsibilities", "requirements_must", "requirements_nice", "red_flags"] {
        jd[key] = json!(strs(&jd[key]));
    }
    if jd["keywords"].as_array().is_none_or(|a| a.is_empty()) || (strs(&jd["requirements_must"]).is_empty() && strs(&jd["responsibilities"]).is_empty()) {
        bail!("The page doesn't contain a readable job description (it may be closed or behind a login). \
               Open the posting, copy the description and paste it as a new application.");
    }
    let current = ctx.job(job.id)?;
    for key in ["company", "title", "location"] {
        let v = jd[key].as_str().unwrap_or("").trim().to_string();
        if v.is_empty() || ["unknown", "n/a", "none"].contains(&v.to_lowercase().as_str()) {
            jd[key] = json!(job_field(&current, key));
        }
    }
    ctx.update(job.id, &[("jd", jd.clone()), ("company", jd["company"].clone()), ("title", jd["title"].clone()), ("location", jd["location"].clone())])?;
    let n = jd["keywords"].as_array().map(|a| a.len()).unwrap_or(0);
    ctx.note(job.id, "Extractor", "info", &format!("{} at {}: {n} keywords ({model})", jd["title"].as_str().unwrap_or(""), jd["company"].as_str().unwrap_or("")), None);
    let (title, company) = (jd["title"].as_str().unwrap_or("").to_string(), jd["company"].as_str().unwrap_or("").to_string());
    let mut twin = db::lock(&ctx.store).twin(job.id, &company, &title)?;
    let mut how = "The same role";
    if twin.is_none() {
        // Same role re-posted under a new link or a reworded title: compare what the job is about.
        if let Some((other, sim)) = rag::similar_job(&ctx.store, job.id, &title, &company, &jd).await {
            let other = db::lock(&ctx.store).job(other).ok();
            let same_company = other.as_ref().is_some_and(|o| crate::scanner::company_norm(&o.company) == crate::scanner::company_norm(&company));
            if sim >= 0.97 || (same_company && sim >= 0.9) {
                twin = other;
                how = "A very similar role";
            }
        }
    }
    if let Some(t) = twin {
        let when = if t.applied_at.is_empty() { String::new() } else { format!(", applied {}", &t.applied_at[..t.applied_at.len().min(10)]) };
        ctx.note(job.id, "History", "warn", &format!("{how} is already tracked as #{} ({}{when})", t.id, t.status), Some(&json!({"duplicate_of": t.id})));
        jd["duplicate_of"] = json!(t.id);
        ctx.update(job.id, &[("jd", jd.clone())])?;
    }
    Ok(jd)
}

async fn evaluate(ctx: &Ctx, ai: &Ai, job: &Job, p: &Profile, jd: &Value, raw: &str) -> Result<Value> {
    ctx.note(job.id, "Evaluator", "info", "Scoring fit against the CV, targets and location policy", None);
    let focus = format!("{} {}", jd["title"].as_str().unwrap_or(""), strs(&jd["requirements_must"]).join(" "));
    let user = format!("{}\n\n=== STRUCTURED JD ===\n{}\n\n=== RAW POSTING ===\n{}",
                       context_block(ctx, ai, p, &focus).await, serde_json::to_string_pretty(jd)?, raw.chars().take(12000).collect::<String>());
    let (mut ev, model) = ai.chat_json(&prompts::evaluator(), &user, 0.2).await?;
    let score = ev["score"].as_f64().or_else(|| ev["score"].as_str().and_then(|s| s.parse().ok()))
        .map(|s| (s.clamp(1.0, 5.0) * 10.0).round() / 10.0);
    ev["score"] = json!(score);
    ev["skill_gaps"] = json!(strs(&ev["skill_gaps"]));
    ctx.update(job.id, &[("evaluation", ev.clone()), ("score", json!(score))])?;
    ctx.note(job.id, "Evaluator", "info", &format!("Fit {}/5: {} ({model})",
        score.map(|s| s.to_string()).unwrap_or("?".into()), ev["recommendation"].as_str().unwrap_or("")), Some(&json!({"score": score})));
    Ok(ev)
}

/// Classify each JD keyword against the candidate's sources; keep a "supported" verdict only if its quote is real.
async fn skill_gap(ctx: &Ctx, ai: &Ai, job: &Job, p: &Profile, jd: &mut Value) -> Result<()> {
    ctx.note(job.id, "Skill Gap", "info", "Checking which keywords the candidate's experience honestly backs", None);
    let keywords = Keyword::from_jd(jd);
    let terms: Vec<String> = keywords.iter().map(|k| k.term.clone()).collect();
    let ev_text = evidence(ctx, p);
    let (out, model) = ai.chat_json(&prompts::skill_gap(),
        &format!("=== CANDIDATE SOURCES (CV, profile, notes, documents) ===\n{ev_text}\n\n=== JD KEYWORDS ===\n{}", json!(terms)), 0.1).await?;
    let base_norm = ats::norm(&ev_text);
    let mut support: HashMap<String, bool> = HashMap::new();
    for k in out["keywords"].as_array().into_iter().flatten() {
        let Some(term) = k["term"].as_str() else { continue };
        if terms.iter().any(|t| t == term) {
            let ok = matches!(k["bucket"].as_str(), Some("existing" | "supported"))
                && ats::evidence_in_cv(k["evidence"].as_str().unwrap_or(""), &base_norm);
            support.insert(term.to_string(), ok);
        }
    }
    for kw in &keywords {
        support.entry(kw.term.clone()).or_insert_with(|| ats::cv_supports(kw, &base_norm));
    }
    let gaps: Vec<&String> = terms.iter().filter(|t| !support[*t]).collect();
    jd["keyword_support"] = json!(support);
    ctx.update(job.id, &[("jd", jd.clone())])?;
    ctx.note(job.id, "Skill Gap", "info", &format!("{}/{} keywords backed by the candidate's experience{} ({model})",
        terms.len() - gaps.len(), terms.len(),
        if gaps.is_empty() { String::new() } else { format!(". Gaps: {}", gaps.iter().take(8).map(|s| s.as_str()).collect::<Vec<_>>().join(", ")) }),
        Some(&json!({"gaps": gaps})));
    Ok(())
}

async fn tailor_request(ctx: &Ctx, ai: &Ai, p: &Profile, jd: &Value, ev: &Value, previous: Option<(&Resume, &Value)>) -> Result<String> {
    let focus = format!("{} {}", jd["title"].as_str().unwrap_or(""), strs(&jd["requirements_must"]).join(" "));
    let gaps: Vec<String> = {
        let mut g = strs(&ev["skill_gaps"]);
        if let Some(s) = jd["keyword_support"].as_object() {
            g.extend(s.iter().filter(|(_, ok)| ok == &&json!(false)).map(|(t, _)| t.clone()));
        }
        g.sort();
        g.dedup();
        g
    };
    let mut parts = vec![
        context_block(ctx, ai, p, &focus).await,
        "=== TARGET JOB ===".into(),
        serde_json::to_string_pretty(&trimmed(jd, &["company", "title", "location", "seniority", "summary", "responsibilities",
                                                     "requirements_must", "requirements_nice", "keywords"]))?,
        "=== FIT EVALUATION ===".into(),
        serde_json::to_string_pretty(&json!({"archetype": ev["archetype"], "strengths": ev["strengths"], "skill_gaps (do NOT claim these)": gaps}))?,
    ];
    if let Some((draft, c)) = previous {
        parts.push("=== YOUR PREVIOUS DRAFT ===".into());
        parts.push(serde_json::to_string_pretty(&draft.for_prompt())?);
        parts.push("=== ATS CRITIC FEEDBACK — revise the draft ===".into());
        parts.push(format!("ATS score {}/100 (target {ATS_TARGET}).", c["score"]));
        parts.push(format!("Keywords missing from the PDF text. Work in each one the candidate genuinely supports, using the exact term, \
                            in summary / competencies / first bullets / skills: {}", c["missing"]));
        if !strs(&c["missing_roles"]).is_empty() {
            parts.push(format!("You dropped these roles from the base CV. Restore each one (exact title, company, dates; older roles may have 1-2 bullets): {}", c["missing_roles"]));
        }
        if !strs(&c["unsupported_keywords"]).is_empty() {
            parts.push(format!("These JD terms appear in your draft but the candidate's sources have NO evidence for them. Remove every claim about them: {}", c["unsupported_keywords"]));
        }
        if !strs(&c["unsupported_numbers"]).is_empty() {
            parts.push(format!("These metrics do NOT appear in the candidate's sources. Remove or correct them: {}", c["unsupported_numbers"]));
        }
        if c["pages"].as_i64().unwrap_or(1) > 2 {
            parts.push(format!("The PDF is {} pages. Trim older-role bullets to fit 2 pages.", c["pages"]));
        }
    }
    Ok(parts.join("\n\n"))
}

/// Draft, render, critique, revise. Keeps the best draft as a new resume version.
async fn tailor_loop(ctx: &Ctx, ai: &Ai, job: &Job, p: &Profile, template: &str) -> Result<i64> {
    let (jd, ev) = (job.jd.clone(), job.evaluation.clone());
    let mut best: Option<((bool, bool, i64), Resume, Checked, String, usize)> = None;
    let mut previous: Option<(Resume, Value)> = None;
    for round in 1..=MAX_ROUNDS {
        ctx.note(job.id, "Tailor", "info", &if round == 1 { "Drafting the tailored resume from the candidate's experience".to_string() } else { format!("Revising the draft (round {round})") },
                 Some(&json!({"round": round, "phase": "draft"})));
        let request = tailor_request(ctx, ai, p, &jd, &ev, previous.as_ref().map(|(r, c)| (r, c))).await?;
        let (raw, model) = ai.chat_json(&prompts::tailor(), &request, 0.3).await?;
        let resume = Resume::from_model(&raw)?;
        ctx.note(job.id, "Tailor", "info", &format!("Draft {round} ready ({model})"), Some(&json!({"round": round, "phase": "drafted", "changes": resume.changes})));

        ctx.note(job.id, "ATS Critic", "info", "Rendering the PDF and reading it back like an ATS", Some(&json!({"round": round, "phase": "checking"})));
        let checked = check_resume(ctx, job, p, &resume, template).await?;
        let r = &checked.report;
        let clean = is_clean(r);
        let pages = r["pages"].as_i64().unwrap_or(1);
        let score = r["score"].as_i64().unwrap_or(0);
        let level = if score >= ATS_TARGET && clean { "success" } else { "warn" };
        let mut data = r.clone();
        data["round"] = json!(round);
        data["phase"] = json!("checked");
        ctx.note(job.id, "ATS Critic", level, &report_line(r), Some(&data));
        for (key, label) in [("unsupported_numbers", "Metrics not in the candidate's sources"), ("unsupported_keywords", "Claims not backed by experience"), ("missing_roles", "Roles dropped from the CV")] {
            let items = strs(&r[key]);
            if !items.is_empty() {
                ctx.note(job.id, "Fact Check", "warn", &format!("{label}: {}", items.join(", ")), None);
            }
        }
        let rank = (clean, pages <= 2, score); // honest first, then fits 2 pages, then ATS
        let done = score >= ATS_TARGET && clean;
        let nothing_fixable = strs(&r["missing"]).is_empty() && clean && pages <= 2;
        let report = r.clone();
        if best.as_ref().is_none_or(|b| rank > b.0) {
            best = Some((rank, resume.clone(), checked, model, round));
        }
        if done || nothing_fixable {
            break;
        }
        previous = Some((resume, report));
    }
    let (_, resume, checked, model, round) = best.ok_or_else(|| anyhow!("no draft was produced"))?;
    let note = format!("Best of the drafts (round {round})");
    let vid = db::lock(&ctx.store).add_version(job.id, "tailored", &note, &serde_json::to_value(&resume)?, &checked.report,
                                               template, &checked.pdf_path, &model)?;
    ctx.note(job.id, "ATS Critic", "success", &format!("Kept draft {round}: ATS {}/100", checked.report["score"]), Some(&json!({"version": vid, "phase": "kept", "round": round})));
    Ok(vid)
}

async fn cover(ctx: &Ctx, ai: &Ai, job: &Job, p: &Profile, instruction: Option<&str>) -> Result<String> {
    ctx.note(job.id, "Writer", "info", if instruction.is_some() { "Revising the cover letter" } else { "Writing the cover letter" }, None);
    let mut user = format!("{}\n\n=== TARGET JOB ===\n{}\n\n=== FIT NOTES ===\n{}",
        context_block(ctx, ai, p, job.jd["title"].as_str().unwrap_or("")).await,
        serde_json::to_string_pretty(&trimmed(&job.jd, &["company", "title", "location", "summary", "requirements_must"]))?,
        serde_json::to_string_pretty(&trimmed(&job.evaluation, &["strengths", "work_auth"]))?);
    if let Some(i) = instruction {
        user.push_str(&format!("\n\n=== CURRENT COVER LETTER ===\n{}\n\n=== REVISION REQUEST ===\n\
            Rewrite the cover letter applying this instruction, keeping every truthfulness rule:\n{i}", job.cover_letter));
    }
    let (out, model) = ai.chat_json(&prompts::cover(), &user, 0.4).await?;
    let letter = out["cover_letter"].as_str().unwrap_or("").trim().to_string();
    if letter.is_empty() {
        bail!("the writer returned an empty letter");
    }
    ctx.note(job.id, "Writer", "success", &format!("Cover letter ready ({model})"), None);
    Ok(letter)
}

// ---------- entry points ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    /// Read the posting, evaluate, tailor, write.
    Full,
    /// Reuse the extracted posting and evaluation; tailor a new resume version.
    Tailor,
}

pub async fn run(ctx: Ctx, job_id: i64, start: Start) {
    let _permit = ctx.sem.clone().acquire_owned().await;
    if let Err(e) = run_inner(&ctx, job_id, start).await {
        if let Some(closed) = e.downcast_ref::<Closed>() {
            let msg = closed.0.clone();
            let _ = ctx.update(job_id, &[("stage", json!("failed")), ("error", json!(msg)), ("status", json!("Discarded"))]);
            let _ = db::lock(&ctx.store).add_event(job_id, "status", "Discarded", &msg);
            ctx.note(job_id, "Scout", "error", &msg, None);
        } else {
            let msg = format!("{e:#}");
            let _ = ctx.update(job_id, &[("stage", json!("failed")), ("error", json!(msg))]);
            ctx.note(job_id, "Pipeline", "error", &format!("Stopped: {msg}"), None);
        }
    }
}

async fn run_inner(ctx: &Ctx, job_id: i64, start: Start) -> Result<()> {
    let ai = ctx.ai()?;
    let mut job = ctx.job(job_id)?;
    let p = ctx.profile_for(&job)?;
    if p.cv_md.trim().len() < 200 {
        bail!("Add a CV to the profile “{}” first (Profile page).", p.name);
    }
    {
        let db = db::lock(&ctx.store);
        rag::sync_profile(&db, &p)?;
    }
    let _ = rag::embed_pending(&ctx.store, Some(&ai)).await; // best effort; keyword search works without it

    if start == Start::Full || job.jd.is_null() || job.evaluation.is_null() {
        ctx.update(job_id, &[("stage", json!("fetching")), ("error", Value::Null)])?;
        let raw = scout(ctx, &job).await?;
        ctx.update(job_id, &[("stage", json!("extracting"))])?;
        job = ctx.job(job_id)?;
        let jd = extract(ctx, &ai, &job, &raw).await?;
        ctx.update(job_id, &[("stage", json!("evaluating"))])?;
        let ev = evaluate(ctx, &ai, &job, &p, &jd, &raw).await?;
        job = ctx.job(job_id)?;
        if job.status == "New" {
            ctx.update(job_id, &[("status", json!("Evaluated"))])?;
            db::lock(&ctx.store).add_event(job_id, "status", "Evaluated", &format!("Fit score {}/5", ev["score"]))?;
        }
        // A poor fit isn't worth a tailored resume unless the user asks for one ("Tailor anyway").
        if let Some(score) = ev["score"].as_f64().filter(|s| *s < MIN_FIT_TO_TAILOR) {
            ctx.update(job_id, &[("stage", json!("ready"))])?;
            {
                let db = db::lock(&ctx.store);
                rag::sync_job(&db, &db.job(job_id)?)?;
            }
            ctx.note(job_id, "Pipeline", "warn", &format!(
                "Stopped after the fit check: {score}/5 is a weak match, so no resume was tailored. \
                 Use “Tailor anyway” on the Resume tab if you still want one."), Some(&json!({"skipped_tailoring": true})));
            return Ok(());
        }
    }

    ctx.update(job_id, &[("stage", json!("tailoring")), ("error", Value::Null)])?;
    job = ctx.job(job_id)?;
    if job.jd["keyword_support"].is_null() {
        let mut jd = job.jd.clone();
        if let Err(e) = skill_gap(ctx, &ai, &job, &p, &mut jd).await {
            ctx.note(job_id, "Skill Gap", "warn", &format!("Skipped ({e:#}); using word matching"), None);
        }
        job = ctx.job(job_id)?;
    }
    let template = if p.template.is_empty() { "classic".to_string() } else { p.template.clone() };
    let vid = tailor_loop(ctx, &ai, &job, &p, &template).await?;

    ctx.update(job_id, &[("stage", json!("writing"))])?;
    job = ctx.job(job_id)?;
    if job.cover_letter.trim().is_empty() || start == Start::Full {
        match cover(ctx, &ai, &job, &p, None).await {
            Ok(letter) => ctx.update(job_id, &[("cover_letter", json!(letter))])?,
            Err(e) => ctx.note(job_id, "Writer", "warn", &format!("Cover letter skipped: {e:#}"), None),
        }
    }

    ctx.update(job_id, &[("stage", json!("ready"))])?;
    job = ctx.job(job_id)?;
    {
        let db = db::lock(&ctx.store);
        if matches!(job.status.as_str(), "New" | "Evaluated") {
            db.update_job(job_id, &[("status", json!("Ready"))])?;
            db.add_event(job_id, "status", "Ready", &format!("Tailored resume ready (ATS {})", job.ats_score.unwrap_or(0)))?;
        }
        rag::sync_job(&db, &db.job(job_id)?)?;
    }
    let _ = rag::embed_pending(&ctx.store, Some(&ai)).await;
    let _ = vid;
    ctx.note(job_id, "Pipeline", "success", "Done. The tailored resume is ready to review and send.", None);
    Ok(())
}

/// Apply a plain-language change to the current resume ("shorter summary", "drop the 2015 role").
pub async fn revise(ctx: Ctx, job_id: i64, instruction: String) {
    let _permit = ctx.sem.clone().acquire_owned().await;
    let result: Result<()> = async {
        let ai = ctx.ai()?;
        let job = ctx.job(job_id)?;
        let p = ctx.profile_for(&job)?;
        let current = db::lock(&ctx.store).job_resume(job_id)?.ok_or_else(|| anyhow!("Create a resume first"))?;
        let template = job.current_version.and_then(|v| db::lock(&ctx.store).version(v).ok()).map(|v| v.template)
            .unwrap_or_else(|| p.template.clone());
        ctx.update(job_id, &[("stage", json!("revising")), ("error", Value::Null)])?;
        ctx.note(job_id, "Tailor", "info", &format!("Revising the resume: {instruction}"), Some(&json!({"phase": "draft", "round": 1})));
        let user = format!("{}\n\n=== CURRENT DRAFT ===\n{}\n\n{}\n\nInstruction: {instruction}",
            tailor_request(&ctx, &ai, &p, &job.jd, &job.evaluation, None).await?,
            serde_json::to_string_pretty(&serde_json::from_value::<Resume>(current)?.for_prompt())?,
            prompts::REVISE_NOTE);
        let (raw, model) = ai.chat_json(&prompts::tailor(), &user, 0.3).await?;
        let resume = Resume::from_model(&raw)?;
        let checked = check_resume(&ctx, &job, &p, &resume, &template).await?;
        let mut data = checked.report.clone();
        data["phase"] = json!("checked");
        data["round"] = json!(1);
        ctx.note(job_id, "ATS Critic", if checked.report["score"].as_i64().unwrap_or(0) >= ATS_TARGET { "success" } else { "warn" },
                 &format!("Revised resume: {}", report_line(&checked.report)), Some(&data));
        let unsupported = [strs(&checked.report["unsupported_keywords"]), strs(&checked.report["unsupported_numbers"])].concat();
        if !unsupported.is_empty() {
            ctx.note(job_id, "Fact Check", "warn", &format!("Not backed by the candidate's sources: {}", unsupported.join(", ")), None);
        }
        let vid = db::lock(&ctx.store).add_version(job_id, "revision", &instruction, &serde_json::to_value(&resume)?, &checked.report,
                                                   &template, &checked.pdf_path, &model)?;
        ctx.update(job_id, &[("stage", json!("ready"))])?;
        {
            let db = db::lock(&ctx.store);
            rag::sync_job(&db, &db.job(job_id)?)?;
        }
        ctx.note(job_id, "Tailor", "success", "Saved as a new version", Some(&json!({"version": vid, "phase": "kept", "round": 1})));
        Ok(())
    }.await;
    if let Err(e) = result {
        let _ = ctx.update(job_id, &[("stage", json!("ready")), ("error", json!(format!("Revision failed: {e:#}")))]);
        ctx.note(job_id, "Pipeline", "error", &format!("Revision failed: {e:#}"), None);
    }
}

pub async fn revise_cover(ctx: Ctx, job_id: i64, instruction: String) {
    let result: Result<()> = async {
        let ai = ctx.ai()?;
        let job = ctx.job(job_id)?;
        let p = ctx.profile_for(&job)?;
        ctx.update(job_id, &[("stage", json!("writing"))])?;
        let letter = cover(&ctx, &ai, &job, &p, Some(&instruction)).await?;
        ctx.update(job_id, &[("cover_letter", json!(letter)), ("stage", json!("ready"))])?;
        let db = db::lock(&ctx.store);
        rag::sync_job(&db, &db.job(job_id)?)?;
        Ok(())
    }.await;
    if let Err(e) = result {
        let _ = ctx.update(job_id, &[("stage", json!("ready")), ("error", json!(format!("Cover letter failed: {e:#}")))]);
        ctx.note(job_id, "Writer", "error", &format!("Cover letter failed: {e:#}"), None);
    }
}

/// Re-render an existing version with another design template (no AI; ATS re-checked).
pub async fn restyle(ctx: Ctx, job_id: i64, version_id: i64, template: String) -> Result<i64> {
    let job = ctx.job(job_id)?;
    let p = ctx.profile_for(&job)?;
    let v = db::lock(&ctx.store).version(version_id)?;
    let resume: Resume = serde_json::from_value(v.resume.clone())?;
    let checked = check_resume(&ctx, &job, &p, &resume, &template).await?;
    let name = pdf::TEMPLATES.iter().find(|t| t.id == template).map(|t| t.name).unwrap_or("another");
    let vid = db::lock(&ctx.store).add_version(job_id, "template", &format!("The same content in the {name} design"),
                                               &v.resume, &checked.report, &template, &checked.pdf_path, &v.model)?;
    ctx.note(job_id, "Designer", "success", &format!("Rendered in the {name} design: {}", report_line(&checked.report)), Some(&json!({"version": vid})));
    Ok(vid)
}

/// Turn an uploaded resume (PDF or text) into the profile's CV markdown and contact facts.
pub async fn import_cv(ctx: &Ctx, profile_id: i64, raw_text: &str) -> Result<()> {
    let ai = ctx.ai()?;
    let (out, _) = ai.chat_json(prompts::CV_IMPORT, &format!("RESUME TEXT:\n\n{raw_text}"), 0.1).await?;
    let cv = out["cv_md"].as_str().unwrap_or("").trim().to_string();
    if cv.len() < 200 {
        bail!("Couldn't read a CV from that file");
    }
    let db = db::lock(&ctx.store);
    let mut p = db.profile(profile_id)?;
    p.cv_md = cv;
    // Fill only empty fields: what the user typed wins over what the model read.
    merge_missing(&mut p.data, &out["profile"]);
    db.save_profile(&p)?;
    rag::sync_profile(&db, &p)?;
    Ok(())
}

fn merge_missing(into: &mut Value, from: &Value) {
    let (Some(dst), Some(src)) = (into.as_object_mut(), from.as_object()) else { return };
    for (k, v) in src {
        match dst.get_mut(k) {
            Some(existing) if existing.is_object() && v.is_object() => merge_missing(existing, v),
            Some(existing) if !(existing.is_null() || existing == "" || existing.as_array().is_some_and(|a| a.is_empty())) => {}
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Re-queue jobs interrupted when the app last closed.
pub fn resume_unfinished(ctx: &Ctx, rt: &tokio::runtime::Handle) {
    let jobs = db::lock(&ctx.store).unfinished().unwrap_or_default();
    for (id, stage) in jobs {
        if stage == "revising" || stage == "writing" {
            let _ = db::lock(&ctx.store).update_job(id, &[("stage", json!("ready"))]);
        } else {
            rt.spawn(run(ctx.clone(), id, Start::Full));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_dropped_roles() {
        let cv = "### Data Scientist — Acme, Germany\n\n### ML Intern – Globex";
        let r = Resume { experience: vec![crate::resume::Experience { role: "Data Scientist".into(), company: "Acme GmbH".into(), ..Default::default() }], ..Default::default() };
        assert_eq!(missing_roles(&r, cv), vec!["ML Intern — Globex"]);
    }

    #[test]
    fn merge_keeps_user_values() {
        let mut a = json!({"candidate": {"full_name": "Ana", "email": ""}});
        merge_missing(&mut a, &json!({"candidate": {"full_name": "X", "email": "a@b.c", "phone": "1"}}));
        assert_eq!(a, json!({"candidate": {"full_name": "Ana", "email": "a@b.c", "phone": "1"}}));
    }
}
