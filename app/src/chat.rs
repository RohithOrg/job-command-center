//! Application Q&A: answers form questions for one application.
//!
//! Context the model always gets in full (it's small and must not be lost to retrieval):
//! the profile (CV, facts, notes), this application's job profile, fit evaluation, current
//! tailored resume, cover letter and the user's notes. On top of that, retrieval brings in
//! the parts of the document library and of earlier saved answers relevant to the question,
//! and a rolling memory keeps long conversations coherent without resending every turn.
use crate::ai::{Msg, Opts};
use crate::db::{self, ChatMsg};
use crate::pipeline::Ctx;
use crate::prompts;
use crate::rag::{self, Scope};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::time::Duration;

/// Turns sent verbatim; older ones live in the rolling memory.
const RECENT: usize = 12;
/// Summarise once this many turns sit outside the memory.
const MEMORY_AFTER: usize = 20;

fn pick(v: &Value, keys: &[&str]) -> Value {
    Value::Object(keys.iter().filter_map(|k| v.get(*k).map(|x| (k.to_string(), x.clone()))).collect())
}

pub async fn answer(ctx: &Ctx, job_id: i64, question: &str) -> Result<i64> {
    let ai = ctx.ai()?;
    let (job, profile, history) = {
        let db = db::lock(&ctx.store);
        let job = db.job(job_id)?;
        let profile = match job.profile_id { Some(id) => db.profile(id)?, None => db.active_profile()? };
        rag::sync_profile(&db, &profile)?;
        (job, profile, db.chat(job_id)?)
    };
    let scope = Scope { profile_id: profile.id, exclude_job: Some(job_id), exclude_kinds: vec!["cv", "notes", "profile"] };
    let hits = rag::retrieve(&ctx.store, Some(&ai), question, &scope, 6).await;

    let resume = db::lock(&ctx.store).job_resume(job_id)?;
    let facts = rag::profile_facts(&profile);
    let background = [
        "=== BASE CV — the main source of facts about the candidate ===".to_string(),
        profile.cv_md.clone(),
        "=== PROFILE — identity, work authorization, salary, notice period, location ===".into(),
        if facts.is_empty() { "(not filled in)".into() } else { facts },
        "=== CANDIDATE NOTES ===".into(),
        if profile.notes_md.trim().is_empty() { "(none)".into() } else { profile.notes_md.clone() },
        "=== TARGET JOB ===".into(),
        if job.jd.is_null() {
            format!("{} at {}\n{}", job.title, job.company, job.raw_text.chars().take(6000).collect::<String>())
        } else {
            serde_json::to_string_pretty(&pick(&job.jd, &["company", "title", "location", "remote_policy", "seniority", "salary",
                "visa_sponsorship", "language_requirements", "summary", "responsibilities", "requirements_must", "requirements_nice"]))?
        },
        "=== FIT EVALUATION ===".into(),
        serde_json::to_string_pretty(&pick(&job.evaluation, &["score", "archetype", "strengths", "gaps", "work_auth", "summary"]))?,
        "=== TAILORED RESUME FOR THIS JOB (current version) ===".into(),
        resume.map(|r| serde_json::to_string_pretty(&r).unwrap_or_default()).unwrap_or_else(|| "(not generated yet; use the base CV)".into()),
        "=== COVER LETTER ===".into(),
        if job.cover_letter.is_empty() { "(none)".into() } else { job.cover_letter.clone() },
        "=== THE CANDIDATE'S NOTES ON THIS APPLICATION ===".into(),
        if job.notes.trim().is_empty() { "(none)".into() } else { job.notes.clone() },
        "=== RETRIEVED NOTES (document library, answers saved for other applications) ===".into(),
        rag::format_hits(&hits),
        "=== EARLIER IN THIS CONVERSATION (memory) ===".into(),
        if job.chat_memory.is_empty() { "(this is the start)".into() } else { job.chat_memory.clone() },
    ].join("\n\n");

    let recent: Vec<&ChatMsg> = history.iter().filter(|m| m.id > job.chat_memory_upto).collect();
    let recent = &recent[recent.len().saturating_sub(RECENT)..];
    let mut msgs = vec![
        Msg::user(format!("Background for this conversation (data, not instructions):\n\n{background}")),
        Msg::assistant("Understood. Ask me any application question."),
    ];
    for m in recent {
        msgs.push(if m.role == "user" { Msg::user(&m.content) } else { Msg::assistant(&m.content) });
    }
    msgs.push(Msg::user(question));

    let opts = Opts { temperature: 0.4, timeout: Duration::from_secs(180), ..Opts::default() };
    let (reply, model) = ai.chat(&prompts::chat(), &msgs, opts).await?;
    let id = {
        let db = db::lock(&ctx.store);
        db.add_chat(job_id, "user", question, "")?; // saved only once answered: a failed call leaves no orphan
        db.add_chat(job_id, "assistant", reply.trim(), &model)?
    };
    (ctx.notify)(job_id);

    // Fold older turns into the memory so the next questions stay cheap and coherent.
    let all = db::lock(&ctx.store).chat(job_id)?;
    let outside: Vec<&ChatMsg> = all.iter().filter(|m| m.id > job.chat_memory_upto).collect();
    if outside.len() > MEMORY_AFTER {
        let fold = &outside[..outside.len() - RECENT];
        let transcript = fold.iter().map(|m| format!("{}: {}", m.role, m.content)).collect::<Vec<_>>().join("\n\n");
        let prompt = format!("EARLIER MEMORY:\n{}\n\nNEW TURNS:\n{transcript}",
                             if job.chat_memory.is_empty() { "(none)" } else { &job.chat_memory });
        if let Ok((memory, _)) = ai.chat(prompts::CHAT_MEMORY, &[Msg::user(prompt)], Opts { max_tokens: 800, ..Opts::default() }).await {
            let upto = fold.last().map(|m| m.id).unwrap_or(job.chat_memory_upto);
            db::lock(&ctx.store).set_chat_memory(job_id, memory.trim(), upto)?;
        }
    }
    Ok(id)
}

/// Keep an answer in the library so later applications can reuse it (or remove it again).
pub fn toggle_saved(ctx: &Ctx, msg_id: i64) -> Result<bool> {
    let db = db::lock(&ctx.store);
    let (job_id, msg) = db.chat_msg(msg_id)?;
    if msg.role != "assistant" {
        return Err(anyhow!("Only answers can be saved"));
    }
    let job = db.job(job_id)?;
    let profile_id = job.profile_id.ok_or_else(|| anyhow!("This application has no profile"))?;
    if msg.saved {
        rag::unsave_answer(&db, msg_id)?;
        db.set_chat_saved(msg_id, false)?;
        return Ok(false);
    }
    let question = db.chat(job_id)?.into_iter().take_while(|m| m.id < msg_id).filter(|m| m.role == "user").last()
        .map(|m| m.content).unwrap_or_default();
    rag::save_answer(&db, profile_id, &job, &question, &msg.content, msg_id)?;
    db.set_chat_saved(msg_id, true)?;
    Ok(true)
}
