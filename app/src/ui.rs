//! Binds the window to the data and the agents.
//!
//! Callbacks run on the UI thread; anything slow (agents, scans, model lists) goes to the
//! tokio runtime and reports back with `upgrade_in_event_loop`. Agents call `notify(job)`
//! after every step, which refreshes the lists and, if it's open, that application's page.
use crate::ai::{self, AiSettings};
use crate::db::{self, Job, Profile, Store, Version, RUNNING, STATUSES};
use crate::pipeline::{self, Ctx, Start};
use crate::{chat, pdf, rag, scanner};
use crate::{
    AiForm, App, AppWindow, SearchForm, SetupStep, ChatRow, CheckRow, CompanyRow, DiscoverRow, DocRow, EventRow, JobDetail, JobRow, PostingRow,
    ProfileForm, ProfileRow, ProviderRow, RoundRow, TemplateRow, VersionDetail, VersionRow, WorkLine,
};
use serde_json::{json, Value};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use tokio::runtime::Handle;

// ---------- view state that lives on the UI thread ----------

#[derive(Default)]
struct View {
    jobs_page: usize,
    disc_page: usize,
    post_page: usize,
    /// Applications ticked in the list, for closing several at once.
    selected: std::collections::HashSet<i64>,
    /// A new application held back because its link matched an earlier one: (url, text, profile).
    pending_add: Option<(String, String, i32)>,
    search: String,
    status: String,
    version: Option<i64>,
    disc_search: String,
    disc_country: String,
    disc_state: String,
}

/// What the resume preview currently shows, so refreshes only re-render when something changed.
#[derive(Default)]
struct PreviewState {
    version: i64,
    key: Option<(i64, String)>,
    thumbs_for: Option<i64>,
    rt: Option<Handle>,
}

thread_local! {
    static PREVIEW: RefCell<PreviewState> = RefCell::new(PreviewState::default());
    static VIEW: RefCell<View> = RefCell::new(View { status: "Active".into(), disc_state: "New".into(), ..Default::default() });
}

fn view<R>(f: impl FnOnce(&mut View) -> R) -> R {
    VIEW.with(|v| f(&mut v.borrow_mut()))
}

const PAGE: usize = 25;

/// One page of a list, clamping the page number. Returns (rows, page, pages).
fn paged<T>(all: Vec<T>, page: usize) -> (Vec<T>, usize, usize) {
    let pages = all.len().div_ceil(PAGE).max(1);
    let page = page.min(pages - 1);
    (all.into_iter().skip(page * PAGE).take(PAGE).collect(), page, pages)
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

pub(crate) fn strings(v: &[String]) -> ModelRc<SharedString> {
    model(v.iter().map(SharedString::from).collect())
}

fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub(crate) fn short_date(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts).map(|d| d.with_timezone(&chrono::Local).format("%-d %b").to_string())
        .unwrap_or_else(|_| ts.get(..10).unwrap_or("").to_string())
}

pub(crate) fn clock(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts).map(|d| {
        let d = d.with_timezone(&chrono::Local);
        if d.date_naive() == chrono::Local::now().date_naive() { d.format("%H:%M").to_string() } else { d.format("%-d %b").to_string() }
    }).unwrap_or_default()
}

fn toast(ui: &AppWindow, msg: impl Into<SharedString>) {
    let app = ui.global::<App>();
    let msg: SharedString = msg.into();
    app.set_toast(msg.clone());
    let weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_secs(5), move || {
        if let Some(ui) = weak.upgrade() {
            if ui.global::<App>().get_toast() == msg {
                ui.global::<App>().set_toast("".into());
            }
        }
    });
}

// ---------- rows ----------

/// The application's place on the line: Found(0) → Fit checked(1) → Tailored(2) → Applied(3) → Interview(4) → Offer(5).
pub(crate) fn track_position(j: &Job) -> (i32, bool) {
    let evaluated = !j.evaluation.is_null();
    match j.status.as_str() {
        "Offer" => (5, false),
        "Interview" => (4, false),
        "Applied" => (3, false),
        "Ready" => (2, false),
        "Evaluated" => (1, false),
        "Rejected" | "Discarded" | "Closed" => {
            let pos = if !j.applied_at.is_empty() { 3 } else if j.current_version.is_some() { 2 } else if evaluated { 1 } else { 0 };
            (pos, true)
        }
        _ => (if evaluated { 1 } else { 0 }, false),
    }
}

pub fn monogram(company: &str) -> (String, i32) {
    let initial = company.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
    let seed = company.to_lowercase().bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    (initial, (seed % 1000) as i32)
}

pub(crate) fn working(j: &Job) -> bool {
    RUNNING.contains(&j.stage.as_str())
}

pub(crate) fn stage_label(stage: &str) -> &'static str {
    match stage {
        "queued" => "Waiting to start",
        "fetching" => "Reading the posting",
        "extracting" => "Pulling out requirements",
        "evaluating" => "Scoring the fit",
        "tailoring" => "Tailoring the resume",
        "revising" => "Revising the resume",
        "writing" => "Writing the cover letter",
        _ => "",
    }
}

pub(crate) fn due(j: &Job) -> bool {
    !j.next_step_date.is_empty() && j.next_step_date <= today() && !db::is_closed(&j.status) && j.status != "Offer"
}

pub(crate) fn when_text(j: &Job) -> String {
    let t = today();
    if j.next_step_date.is_empty() {
        return "–".into();
    }
    if j.next_step_date < t {
        return "Overdue".into();
    }
    if j.next_step_date == t {
        return "Today".into();
    }
    chrono::NaiveDate::parse_from_str(&j.next_step_date, "%Y-%m-%d").map(|d| d.format("%-d %b").to_string()).unwrap_or_else(|_| j.next_step_date.clone())
}

pub(crate) fn title_or(j: &Job) -> String {
    if !j.title.is_empty() { j.title.clone() } else if !j.url.is_empty() { j.url.clone() } else { "New application".into() }
}

fn row(j: &Job, reason: &str, board: &str) -> JobRow {
    let (position, ended) = track_position(j);
    let (initial, hue) = monogram(&j.company);
    JobRow {
        id: j.id as i32,
        title: title_or(j).into(),
        company: j.company.clone().into(),
        location: j.location.clone().into(),
        initial: initial.into(),
        hue,
        status: j.status.clone().into(),
        fit: j.score.map(|s| format!("{s:.1}")).unwrap_or_default().into(),
        ats: j.ats_score.map(|a| a.to_string()).unwrap_or_default().into(),
        position,
        ended,
        next_step: j.next_step.clone().into(),
        when: if board == "Boarding" && j.next_step_date.is_empty() { "Now".into() } else { when_text(j).into() },
        board: board.into(),
        reason: reason.into(),
        has_pdf: !j.pdf_path.is_empty(),
        working: working(j),
        stage_label: stage_label(&j.stage).into(),
        updated: short_date(&j.created_at).into(),
        selected: view(|v| v.selected.contains(&j.id)),
    }
}

/// What needs the user, most urgent first: (job, reason, board flag). Shared with the web companion.
pub(crate) fn queue<'a>(jobs: &'a [Job]) -> Vec<(&'a Job, String, &'static str)> {
    let mut out = Vec::new();
    for j in jobs.iter().filter(|j| due(j)) {
        let board = if j.next_step_date < today() { "Delayed" } else { "Boarding" };
        out.push((j, if j.next_step.is_empty() { "Follow up".to_string() } else { j.next_step.clone() }, board));
    }
    for j in jobs.iter().filter(|j| j.status == "Ready" && !due(j)) {
        out.push((j, "Resume is ready. Review it and apply.".into(), "Boarding"));
    }
    for j in jobs.iter().filter(|j| j.stage == "failed" && !db::is_closed(&j.status)) {
        out.push((j, "The agents stopped. Open it to see why.".into(), "Stopped"));
    }
    for j in jobs.iter().filter(|j| working(j)) {
        out.push((j, stage_label(&j.stage).into(), "Working"));
    }
    out
}

fn today_queue(jobs: &[Job]) -> Vec<JobRow> {
    queue(jobs).into_iter().map(|(j, reason, board)| row(j, &reason, board)).collect()
}

pub(crate) fn filtered_jobs<'a>(jobs: &'a [Job], search: &str, status: &str) -> Vec<&'a Job> {
    let needle = search.trim().to_lowercase();
    jobs.iter()
        .filter(|j| match status {
            "All" => true,
            "Active" | "" => !db::is_closed(&j.status),
            "Closed" => db::is_closed(&j.status),
            s => j.status == s,
        })
        .filter(|j| needle.is_empty() || format!("{} {} {}", j.title, j.company, j.location).to_lowercase().contains(&needle))
        .collect()
}

fn filtered(jobs: &[Job], search: &str, status: &str) -> Vec<JobRow> {
    filtered_jobs(jobs, search, status).into_iter().map(|j| row(j, "", "")).collect()
}

pub(crate) fn template_name(id: &str) -> String {
    pdf::TEMPLATES.iter().find(|t| t.id == id).map(|t| t.name).unwrap_or("Classic").to_string()
}

pub(crate) fn version_kind(kind: &str) -> &'static str {
    match kind {
        "tailored" => "Tailored",
        "revision" => "Your revision",
        "template" => "New design",
        _ => "Imported",
    }
}

fn version_row(v: &Version, current: Option<i64>, n: usize) -> VersionRow {
    VersionRow {
        id: v.id as i32,
        label: format!("Version {n}").into(),
        kind: version_kind(&v.kind).into(),
        note: v.note.clone().into(),
        ats: v.ats_score.unwrap_or(0) as i32,
        has_ats: v.ats_score.is_some(),
        template_name: template_name(&v.template).into(),
        created: short_date(&v.created_at).into(),
        current: current == Some(v.id),
    }
}

fn version_detail(v: &Version, current: Option<i64>, n: usize) -> VersionDetail {
    let a = &v.ats;
    let list = |k: &str| crate::resume::strs(&a[k]);
    VersionDetail {
        id: v.id as i32,
        label: format!("Version {n}").into(),
        note: if v.note.is_empty() { version_kind(&v.kind).into() } else { v.note.clone().into() },
        model: v.model.clone().into(),
        created: short_date(&v.created_at).into(),
        ats: v.ats_score.unwrap_or(0) as i32,
        coverage: a["coverage"].as_i64().unwrap_or(0) as i32,
        structure: a["structure"].as_i64().unwrap_or(0) as i32,
        pages: a["pages"].as_i64().unwrap_or(0) as i32,
        template: v.template.clone().into(),
        template_name: template_name(&v.template).into(),
        pdf: v.pdf_path.clone().into(),
        current: current == Some(v.id),
        matched: strings(&list("matched")),
        missing: strings(&list("missing")),
        gaps: strings(&list("missing_gaps")),
        unsupported: strings(&[list("unsupported_keywords"), list("unsupported_numbers"), list("missing_roles")].concat()),
        checks: model(a["checks"].as_array().into_iter().flatten().map(|c| CheckRow {
            name: s(&c["name"]).into(), ok: c["ok"].as_bool().unwrap_or(false), detail: s(&c["detail"]).into(),
        }).collect()),
        changes: strings(&crate::resume::strs(&v.resume["changes"])),
    }
}

fn detail(j: &Job, profile_name: &str, current_pdf: bool) -> JobDetail {
    let (position, ended) = track_position(j);
    let (initial, hue) = monogram(&j.company);
    let ev = &j.evaluation;
    let jd = &j.jd;
    let strengths: Vec<String> = ev["strengths"].as_array().into_iter().flatten()
        .map(|x| if x.is_object() { format!("{}: {}", s(&x["requirement"]), s(&x["evidence"])) } else { s(x) }).collect();
    let gaps: Vec<String> = ev["gaps"].as_array().into_iter().flatten()
        .map(|x| if x.is_object() {
            let m = s(&x["mitigation"]);
            format!("{} ({}){}", s(&x["requirement"]), s(&x["severity"]), if m.is_empty() { String::new() } else { format!(". {m}") })
        } else { s(x) }).collect();
    let dup = jd["duplicate_of"].as_i64().map(|d| format!("You already track this role as application #{d}. Check it before applying twice.")).unwrap_or_default();
    JobDetail {
        id: j.id as i32,
        title: title_or(j).into(),
        company: j.company.clone().into(),
        location: j.location.clone().into(),
        initial: initial.into(),
        hue,
        url: j.url.clone().into(),
        status: j.status.clone().into(),
        status_index: STATUSES.iter().position(|s| *s == j.status).unwrap_or(0) as i32,
        position,
        ended,
        fit: j.score.map(|s| format!("{s:.1}")).unwrap_or_default().into(),
        recommendation: s(&ev["recommendation"]).into(),
        fit_summary: s(&ev["summary"]).into(),
        archetype: s(&ev["archetype"]).into(),
        work_auth: s(&ev["work_auth"]).into(),
        strengths: strings(&strengths),
        gaps: strings(&gaps),
        red_flags: strings(&crate::resume::strs(&jd["red_flags"])),
        ats: j.ats_score.map(|a| a.to_string()).unwrap_or_default().into(),
        job_summary: s(&jd["summary"]).into(),
        salary: s(&jd["salary"]).into(),
        remote: { let r = s(&jd["remote_policy"]); if r == "Unknown" { String::new() } else { r } }.into(),
        seniority: { let r = s(&jd["seniority"]); if r == "Unknown" { String::new() } else { r } }.into(),
        next_step: j.next_step.clone().into(),
        next_step_date: j.next_step_date.clone().into(),
        applied: j.applied_at.get(..10).unwrap_or("").into(),
        notes: j.notes.clone().into(),
        has_pdf: current_pdf,
        has_resume: j.current_version.is_some(),
        has_jd: !jd.is_null(),
        cover_letter: j.cover_letter.clone().into(),
        working: working(j),
        stage_label: stage_label(&j.stage).into(),
        error: j.error.clone().into(),
        duplicate_note: dup.into(),
        profile_name: profile_name.into(),
    }
}

/// ATS rounds of the latest tailoring or revision run, from the structured agent log.
fn rounds(logs: &[db::LogLine]) -> Vec<RoundRow> {
    let start = logs.iter().rposition(|l| l.data["phase"] == "draft" && l.data["round"] == 1).unwrap_or(logs.len());
    let mut out: Vec<RoundRow> = Vec::new();
    for l in &logs[start..] {
        let d = &l.data;
        let Some(r) = d["round"].as_i64() else { continue };
        let r = r as i32;
        if !out.iter().any(|x| x.round == r) {
            out.push(RoundRow { round: r, state: "drafting".into(), ..Default::default() });
        }
        let row = out.iter_mut().find(|x| x.round == r).unwrap();
        match d["phase"].as_str().unwrap_or("") {
            "checking" => row.state = "checking".into(),
            "checked" => {
                row.state = "done".into();
                row.score = d["score"].as_i64().unwrap_or(0) as i32;
                row.coverage = d["coverage"].as_i64().unwrap_or(0) as i32;
                row.pages = d["pages"].as_i64().unwrap_or(0) as i32;
                row.missing = crate::resume::strs(&d["missing"]).into_iter().take(5).collect::<Vec<_>>().join(", ").into();
                let issues = [crate::resume::strs(&d["unsupported_keywords"]), crate::resume::strs(&d["unsupported_numbers"])].concat();
                row.issues = if issues.is_empty() { String::new() } else { format!("Removed next round: {}", issues.join(", ")) }.into();
            }
            "kept" => row.state = "kept".into(),
            _ => {}
        }
    }
    out
}

// ---------- refreshers ----------

pub fn refresh_lists(ui: &AppWindow, store: &Store) {
    let app = ui.global::<App>();
    let Ok(all) = db::lock(store).jobs() else { return };
    app.set_today(model(today_queue(&all)));
    let (search, status) = view(|v| (v.search.clone(), v.status.clone()));
    let rows = filtered(&all, &search, &status);
    app.set_jobs_total(rows.len() as i32);
    let (rows, page, pages) = paged(rows, view(|v| v.jobs_page));
    view(|v| v.jobs_page = page);
    app.set_jobs(model(rows));
    app.set_jobs_page(page as i32);
    app.set_jobs_pages(pages as i32);
    let n_working = all.iter().filter(|j| working(j)).count();
    app.set_working_count(n_working as i32);
    // Forget ticks on applications that no longer exist or are already closed.
    let open_ids: std::collections::HashSet<i64> = all.iter().filter(|j| !db::is_closed(&j.status)).map(|j| j.id).collect();
    let count = view(|v| { v.selected.retain(|id| open_ids.contains(id)); v.selected.len() });
    app.set_selected_count(count as i32);
    app.set_status_line(if n_working > 0 {
        format!("Agents are working on {n_working} application{}", if n_working == 1 { "" } else { "s" })
    } else {
        format!("{} applications tracked", all.len())
    }.into());
}

pub fn refresh_job(ui: &AppWindow, store: &Store) {
    let app = ui.global::<App>();
    let id = app.get_open_job() as i64;
    if id < 0 {
        return;
    }
    let db = db::lock(store);
    let Ok(j) = db.job(id) else {
        app.set_open_job(-1);
        return;
    };
    let profile_name = j.profile_id.and_then(|p| db.profile(p).ok()).map(|p| p.name).unwrap_or_default();
    let versions = db.versions(id).unwrap_or_default();
    let total = versions.len();
    let wanted = view(|v| v.version).filter(|w| versions.iter().any(|x| x.id == *w)).or(j.current_version).or(versions.first().map(|v| v.id));
    app.set_versions(model(versions.iter().enumerate().map(|(i, v)| version_row(v, j.current_version, total - i)).collect()));
    match versions.iter().enumerate().find(|(_, v)| Some(v.id) == wanted) {
        Some((i, v)) => app.set_version(version_detail(v, j.current_version, total - i)),
        None => app.set_version(VersionDetail::default()),
    }
    drop(db);
    ensure_preview(ui, store);
    let db = db::lock(store);
    let current_pdf = j.current_version.and_then(|c| versions.iter().find(|v| v.id == c)).is_some_and(|v| !v.pdf_path.is_empty());
    app.set_job(detail(&j, &profile_name, current_pdf));
    let logs = db.logs(id).unwrap_or_default();
    app.set_rounds(model(rounds(&logs)));
    app.set_work(model(logs.iter().map(|l| WorkLine {
        agent: l.agent.clone().into(), level: l.level.clone().into(), message: l.message.clone().into(), time: clock(&l.ts).into(),
    }).collect()));
    app.set_events(model(db.events(id).unwrap_or_default().iter().rev().map(|e| EventRow {
        time: short_date(&e.ts).into(), text: if e.text.is_empty() { e.status.clone().into() } else { e.text.clone().into() },
    }).collect()));
    app.set_chat(model(db.chat(id).unwrap_or_default().iter().map(|m| ChatRow {
        id: m.id as i32, role: m.role.clone().into(), content: m.content.clone().into(),
        meta: if m.model.is_empty() { clock(&m.ts) } else { format!("{}, {}", clock(&m.ts), m.model) }.into(), saved: m.saved,
    }).collect()));
}

fn to_image(p: &pdf::PageImage) -> slint::Image {
    slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&p.rgba, p.width, p.height))
}

/// Render the selected version's pages (and design thumbnails) in the background when they changed.
fn ensure_preview(ui: &AppWindow, store: &Store) {
    let app = ui.global::<App>();
    let version = app.get_version();
    let vid = version.id as i64;
    let Some(rt) = PREVIEW.with(|p| p.borrow().rt.clone()) else { return };
    if vid <= 0 {
        PREVIEW.with(|p| { let mut p = p.borrow_mut(); p.key = None; p.version = 0; });
        app.set_preview_pages(model(vec![]));
        return;
    }
    // A newly selected version starts in its own design.
    if PREVIEW.with(|p| p.borrow().version) != vid {
        PREVIEW.with(|p| p.borrow_mut().version = vid);
        app.set_preview_template(version.template.clone());
        app.set_preview_pages(model(vec![]));
    }
    let template = app.get_preview_template().to_string();
    let key = (vid, template.clone());
    let need_pages = PREVIEW.with(|p| p.borrow().key.as_ref() != Some(&key));
    let need_thumbs = PREVIEW.with(|p| p.borrow().thumbs_for != Some(vid));
    if !need_pages && !need_thumbs {
        return;
    }
    let inputs = (|| -> anyhow::Result<_> {
        let db = db::lock(store);
        let v = db.version(vid)?;
        let job = db.job(v.job_id)?;
        let profile = match job.profile_id { Some(id) => db.profile(id)?, None => db.active_profile()? };
        let resume: crate::resume::Resume = serde_json::from_value(v.resume)?;
        Ok((resume, pipeline::header(&profile), pipeline::page_size(&job.jd)))
    })();
    let Ok((resume, header, size)) = inputs else { return };
    let weak = ui.as_weak();
    if need_pages {
        PREVIEW.with(|p| p.borrow_mut().key = Some(key.clone()));
        app.set_preview_busy(true);
        let (resume, header, weak) = (resume.clone(), header.clone(), weak.clone());
        rt.spawn(async move {
            let pages = tokio::task::spawn_blocking(move || pdf::render_pages(&resume, &header, &template, size, 2.6, 8)).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                // Ignore a render that finished after the user moved on.
                if PREVIEW.with(|p| p.borrow().key.as_ref() != Some(&key)) {
                    return;
                }
                let app = ui.global::<App>();
                app.set_preview_busy(false);
                if let Ok(Ok(pages)) = pages {
                    app.set_preview_pages(model(pages.iter().map(to_image).collect()));
                }
            });
        });
    }
    if need_thumbs {
        PREVIEW.with(|p| p.borrow_mut().thumbs_for = Some(vid));
        app.set_design_thumbs(model(vec![]));
        rt.spawn(async move {
            let thumbs = tokio::task::spawn_blocking(move || {
                pdf::TEMPLATES.iter().map(|t| pdf::render_pages(&resume, &header, t.id, size, 0.5, 1).ok().and_then(|mut v| v.pop()))
                    .collect::<Vec<_>>()
            }).await.unwrap_or_default();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if PREVIEW.with(|p| p.borrow().thumbs_for != Some(vid)) {
                    return;
                }
                ui.global::<App>().set_design_thumbs(model(thumbs.iter().map(|t| t.as_ref().map(to_image).unwrap_or_default()).collect()));
            });
        });
    }
}

fn list(v: &Value) -> Vec<String> {
    crate::resume::strs(v)
}

pub(crate) fn strings_vec(m: &ModelRc<SharedString>) -> Vec<String> {
    m.iter().map(|s| s.to_string()).collect()
}

pub(crate) fn profile_form(p: &Profile) -> ProfileForm {
    let d = &p.data;
    let c = &d["candidate"];
    let loc = &d["location"];
    // Older profiles kept "City, Country" in one field.
    let (city, country) = {
        let (c1, c2) = (s(&loc["city"]), s(&loc["country"]));
        if c1.is_empty() && c2.is_empty() {
            let whole = s(&c["location"]);
            match whole.rsplit_once(',') { Some((a, b)) => (a.trim().to_string(), b.trim().to_string()), None => (whole, String::new()) }
        } else {
            (c1, c2)
        }
    };
    let notice = {
        let n = s(&d["logistics"]["notice_period"]);
        if !n.is_empty() { n } else {
            match d["cover_letter"]["notice_period_days"].as_i64() {
                Some(0) => "Can start now".into(),
                Some(7) => "1 week".into(),
                Some(14) => "2 weeks".into(),
                Some(30) | Some(31) => "1 month".into(),
                Some(60) => "2 months".into(),
                Some(90) => "3 months".into(),
                Some(x) => format!("{x} days"),
                None => String::new(),
            }
        }
    };
    ProfileForm {
        id: p.id as i32,
        name: p.name.clone().into(),
        full_name: s(&c["full_name"]).into(),
        headline: s(&d["narrative"]["headline"]).into(),
        email: s(&c["email"]).into(),
        phone: s(&c["phone"]).into(),
        city: city.into(),
        country: country.into(),
        linkedin: s(&c["linkedin"]).into(),
        github: s(&c["github"]).into(),
        website: s(&c["portfolio_url"]).into(),
        other_link: s(&c["other_url"]).into(),
        target_roles: strings(&list(&d["target_roles"]["primary"])),
        seniority: s(&d["target_roles"]["seniority"]).into(),
        work_mode: s(&d["target_roles"]["work_mode"]).into(),
        countries: strings(&list(&loc["preferred_countries"])),
        cities: strings(&list(&loc["preferred_cities"])),
        relocate: loc["open_to_relocation"].as_bool().unwrap_or(false),
        avoid_places: strings(&list(&loc["avoid"])),
        exclude_words: strings(&list(&d["target_roles"]["exclude_words"])),
        salary_target: s(&d["compensation"]["target_range"]).into(),
        salary_minimum: s(&d["compensation"]["minimum"]).into(),
        currency: { let c = s(&d["compensation"]["currency"]); if c.is_empty() { "EUR".into() } else { c } }.into(),
        authorized_in: strings(&list(&loc["authorized_in"])),
        needs_sponsorship: loc["needs_sponsorship"].as_bool().unwrap_or(false),
        visa_notes: s(&loc["visa_status"]).into(),
        notice: notice.into(),
        languages: strings(&list(&d["languages"])),
        story: s(&d["narrative"]["exit_story"]).into(),
        strengths: strings(&list(&d["narrative"]["superpowers"])),
        avoid: s(&d["narrative"]["avoid"]).into(),
        tone: { let t = s(&d["writing"]["tone"]); if t.is_empty() { "Professional".into() } else { t } }.into(),
        notes_md: p.notes_md.clone().into(),
        cv_md: p.cv_md.clone().into(),
        template: if p.template.is_empty() { "classic".into() } else { p.template.clone().into() },
    }
}

/// The profile's chip lists, by the field name the UI uses.
fn chip_list(app: &App, field: &str) -> Option<Vec<String>> {
    let p = app.get_profile();
    Some(strings_vec(&match field {
        "target-roles" => p.target_roles,
        "exclude-words" => p.exclude_words,
        "countries" => p.countries,
        "cities" => p.cities,
        "avoid-places" => p.avoid_places,
        "authorized-in" => p.authorized_in,
        "languages" => p.languages,
        "strengths" => p.strengths,
        _ => return None,
    }))
}

fn set_chip_list(app: &App, field: &str, items: Vec<String>) {
    let m = strings(&items);
    let mut f = app.get_profile();
    match field {
        "target-roles" => f.target_roles = m,
        "exclude-words" => f.exclude_words = m,
        "countries" => f.countries = m,
        "cities" => f.cities = m,
        "avoid-places" => f.avoid_places = m,
        "authorized-in" => f.authorized_in = m,
        "languages" => f.languages = m,
        "strengths" => f.strengths = m,
        _ => return,
    }
    app.set_profile(f);
}

/// Bumped on every keystroke so a slow online lookup never overwrites newer suggestions.
static SUGGEST_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The scanner searches for what the active profile wants: its titles, places and exclusions.
pub(crate) fn sync_search(db: &db::Db, p: &Profile) -> anyhow::Result<()> {
    let d = &p.data;
    let mut prefs = scanner::prefs(db);
    prefs.title_positive = list(&d["target_roles"]["primary"]);
    prefs.title_negative = list(&d["target_roles"]["exclude_words"]);
    let mut places = list(&d["location"]["preferred_countries"]);
    places.extend(list(&d["location"]["preferred_cities"]));
    if s(&d["target_roles"]["work_mode"]) == "Remote" && !places.iter().any(|x| x == "Remote") {
        places.push("Remote".into());
    }
    prefs.locations_allow = places;
    prefs.locations_block = list(&d["location"]["avoid"]);
    scanner::save_prefs(db, &prefs)
}

/// Profiles made before the search moved into the profile: take over the scanner's old lists once.
fn adopt_search_prefs(db: &db::Db, p: &mut Profile) -> anyhow::Result<()> {
    let d = &mut p.data;
    if !d.is_object() || d["search_adopted"] == true {
        return Ok(());
    }
    let prefs = scanner::prefs(db);
    let fill = |cur: &mut Value, v: &[String]| if list(cur).is_empty() && !v.is_empty() { *cur = json!(v); };
    if !d["target_roles"].is_object() { d["target_roles"] = json!({}); }
    if !d["location"].is_object() { d["location"] = json!({}); }
    fill(&mut d["target_roles"]["primary"], &prefs.title_positive);
    fill(&mut d["target_roles"]["exclude_words"], &prefs.title_negative);
    fill(&mut d["location"]["preferred_countries"], &prefs.locations_allow);
    fill(&mut d["location"]["avoid"], &prefs.locations_block);
    d["search_adopted"] = json!(true);
    db.save_profile(p)
}

/// Write the form back into the profile, keeping every key the form doesn't show.
pub(crate) fn apply_form(p: &mut Profile, f: &ProfileForm) {
    if !p.data.is_object() {
        p.data = json!({});
    }
    let t = |x: &SharedString| json!(x.trim());
    let l = |m: &ModelRc<SharedString>| json!(strings_vec(m));
    let location = match (f.city.trim(), f.country.trim()) {
        ("", c) | (c, "") => c.to_string(),
        (a, b) => format!("{a}, {b}"),
    };
    let d = &mut p.data;
    for (path, val) in [
        (&["candidate", "full_name"][..], t(&f.full_name)),
        (&["candidate", "email"], t(&f.email)),
        (&["candidate", "phone"], t(&f.phone)),
        (&["candidate", "location"], json!(location)),
        (&["candidate", "linkedin"], t(&f.linkedin)),
        (&["candidate", "github"], t(&f.github)),
        (&["candidate", "portfolio_url"], t(&f.website)),
        (&["candidate", "other_url"], t(&f.other_link)),
        (&["narrative", "headline"], t(&f.headline)),
        (&["narrative", "exit_story"], t(&f.story)),
        (&["narrative", "superpowers"], l(&f.strengths)),
        (&["narrative", "avoid"], t(&f.avoid)),
        (&["target_roles", "primary"], l(&f.target_roles)),
        (&["target_roles", "seniority"], t(&f.seniority)),
        (&["target_roles", "work_mode"], t(&f.work_mode)),
        (&["location", "city"], t(&f.city)),
        (&["location", "country"], t(&f.country)),
        (&["location", "preferred_countries"], l(&f.countries)),
        (&["location", "preferred_cities"], l(&f.cities)),
        (&["location", "open_to_relocation"], json!(f.relocate)),
        (&["location", "avoid"], l(&f.avoid_places)),
        (&["target_roles", "exclude_words"], l(&f.exclude_words)),
        (&["location", "authorized_in"], l(&f.authorized_in)),
        (&["location", "needs_sponsorship"], json!(f.needs_sponsorship)),
        (&["location", "visa_status"], t(&f.visa_notes)),
        (&["compensation", "target_range"], t(&f.salary_target)),
        (&["compensation", "minimum"], t(&f.salary_minimum)),
        (&["compensation", "currency"], t(&f.currency)),
        (&["logistics", "notice_period"], t(&f.notice)),
        (&["languages"], l(&f.languages)),
        (&["writing", "tone"], t(&f.tone)),
    ] {
        let mut cur = &mut *d;
        for key in &path[..path.len() - 1] {
            if !cur[*key].is_object() {
                cur[*key] = json!({});
            }
            cur = &mut cur[*key];
        }
        cur[path[path.len() - 1]] = val;
    }
    p.name = if f.name.trim().is_empty() { "Profile".into() } else { f.name.trim().to_string() };
    p.cv_md = f.cv_md.to_string();
    p.notes_md = f.notes_md.to_string();
    p.template = f.template.to_string();
}

fn doc_rows(docs: &[rag::Doc], kind: &str) -> Vec<DocRow> {
    docs.iter().filter(|d| d.kind == kind).map(|d| DocRow {
        id: d.id as i32,
        kind: d.kind.clone().into(),
        title: d.title.clone().into(),
        preview: d.content.chars().take(160).collect::<String>().replace('\n', " ").into(),
        chunks: d.chunks as i32,
        updated: short_date(&d.updated_at).into(),
    }).collect()
}

pub fn refresh_profiles(ui: &AppWindow, store: &Store, select: Option<i64>) {
    let app = ui.global::<App>();
    let db = db::lock(store);
    if let Ok(mut a) = db.active_profile() {
        let _ = adopt_search_prefs(&db, &mut a);
    }
    let Ok(profiles) = db.profiles() else { return };
    let active = db.active_profile().ok();
    let shown = select.or(Some(app.get_profile().id as i64)).filter(|id| profiles.iter().any(|p| p.id == *id))
        .or(active.as_ref().map(|p| p.id));
    app.set_profiles(model(profiles.iter().map(|p| ProfileRow { id: p.id as i32, name: p.name.clone().into(), active: p.active }).collect()));
    app.set_profile_names(model(profiles.iter().map(|p| SharedString::from(p.name.as_str())).collect()));
    app.set_active_profile_index(profiles.iter().position(|p| p.active).unwrap_or(0) as i32);
    if let Some(p) = shown.and_then(|id| profiles.iter().find(|p| p.id == id)) {
        if select.is_some() || app.get_profile().id as i64 != p.id {
            app.set_profile(profile_form(p));
        }
        let docs = rag::library(&db, p.id).unwrap_or_default();
        app.set_library(model(doc_rows(&docs, "doc")));
        app.set_answers(model(doc_rows(&docs, "answer")));
    }
    if let Some(a) = active {
        app.set_user_first_name(a.full_name().split_whitespace().next().unwrap_or("").into());
    }
}

fn load_ai(store: &Store) -> AiSettings {
    serde_json::from_value(db::lock(store).setting("ai").unwrap_or(Value::Null)).unwrap_or_default()
}

pub fn refresh_settings(ui: &AppWindow, store: &Store) {
    let app = ui.global::<App>();
    let s = load_ai(store);
    let index = ai::PROVIDERS.iter().position(|p| p.id == s.provider).unwrap_or(0);
    let p = &ai::PROVIDERS[index];
    app.set_ai(AiForm {
        provider_index: index as i32,
        base_url: if s.base_url.is_empty() { p.base_url.into() } else { s.base_url.clone().into() },
        models: if s.models.is_empty() { p.default_model.into() } else { s.models.join(", ").into() },
        key_status: if s.provider.is_empty() { SharedString::new() } else { ai::mask(&ai::load_key(&s.provider)).into() },
        new_key: SharedString::new(),
        search: if s.embed_provider == "builtin" || s.provider.is_empty() { 0 } else { 1 },
    });
    app.set_ai_connected(!s.provider.is_empty() && !s.models.is_empty());
    let prefs = scanner::prefs(&db::lock(store));
    app.set_search(SearchForm {
        auto: prefs.auto_process,
        limit: prefs.auto_process_limit.to_string().into(),
        interval: if prefs.scan_interval_hours > 0.0 { format!("{}", prefs.scan_interval_hours).into() } else { "0".into() },
    });
    let (total, embedded) = rag::index_status(&db::lock(store), crate::embed::MODEL_ID).unwrap_or((0, 0));
    app.set_index_status(if total == 0 {
        "Nothing to search yet. Add your CV on the Profile page.".to_string()
    } else if embedded >= total {
        "Smart search is up to date with everything you've added.".to_string()
    } else {
        format!("Smart search has read {}% of what you've added; the rest is still being prepared.", 100 * embedded / total.max(1))
    }.into());
}

/// Whether a scanned posting matched (newer scans store `matched`; older ones only a verdict).
pub(crate) fn posting_matched(p: &Value) -> bool {
    match p["matched"].as_bool() {
        Some(m) => m,
        None => matches!(p["verdict"].as_str(), Some("new" | "duplicate" | "seen")),
    }
}

fn discover_rows(store: &Store) -> (Vec<DiscoverRow>, Vec<String>) {
    let (search, country, state) = view(|v| (v.disc_search.to_lowercase(), v.disc_country.clone(), v.disc_state.clone()));
    let all = scanner::discovered(&db::lock(store), None).unwrap_or_default();
    let mut countries: Vec<String> = all.iter().map(|d| scanner::country_of(&d.location)).filter(|c| !c.is_empty()).collect();
    countries.sort();
    countries.dedup();
    countries.insert(0, "All countries".into());
    let rows = all.iter()
        .filter(|d| match state.as_str() {
            "All" => true,
            "Dismissed" => d.state == "dismissed",
            "Tailoring" => d.job_id.is_some(),
            _ => d.state == "new" && d.job_id.is_none(),
        })
        .filter(|d| country.is_empty() || country == "All countries" || scanner::country_of(&d.location) == country)
        .filter(|d| search.is_empty() || format!("{} {} {}", d.title, d.company, d.location).to_lowercase().contains(&search))
        .map(|d| {
            let (initial, hue) = monogram(&d.company);
            DiscoverRow {
                id: d.id as i32, title: d.title.clone().into(), company: d.company.clone().into(), location: d.location.clone().into(),
                country: scanner::country_of(&d.location).into(), url: d.url.clone().into(), initial: initial.into(), hue,
                seen: short_date(&d.first_seen).into(), state: d.state.clone().into(), job_id: d.job_id.unwrap_or(-1) as i32,
            }
        }).collect();
    (rows, countries)
}

pub fn refresh_discover(ui: &AppWindow, store: &Store) {
    let app = ui.global::<App>();
    let (rows, countries) = discover_rows(store);
    app.set_discover_total(rows.len() as i32);
    let (rows, page, pages) = paged(rows, view(|v| v.disc_page));
    view(|v| v.disc_page = page);
    app.set_discovered(model(rows));
    app.set_discover_page(page as i32);
    app.set_discover_pages(pages as i32);
    app.set_countries(strings(&countries));
}

pub fn refresh_companies(ui: &AppWindow, store: &Store) {
    let app = ui.global::<App>();
    let db = db::lock(store);
    let prefs = scanner::prefs(&db);
    app.set_favorites_only(prefs.favorites_only);
    let mut all = scanner::companies(&db).unwrap_or_default();
    if prefs.favorites_only {
        all.retain(|c| c.favorite);
    }
    all.sort_by(|a, b| b.favorite.cmp(&a.favorite).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    let selected_id = {
        let i = app.get_open_company();
        let rows = app.get_companies();
        if i >= 0 && (i as usize) < rows.row_count() { rows.row_data(i as usize).map(|r| r.id as i64) } else { None }
    };
    let rows: Vec<CompanyRow> = all.iter().map(|c| {
        let (initial, hue) = monogram(&c.name);
        let postings = c.last_postings.as_array().cloned().unwrap_or_default();
        CompanyRow {
            id: c.id as i32, name: c.name.clone().into(), url: c.careers_url.clone().into(), initial: initial.into(), hue,
            favorite: c.favorite, last_scan: if c.last_scan_at.is_empty() { SharedString::new() } else { short_date(&c.last_scan_at).into() },
            status: c.last_status.clone().into(),
            status_failed: { let st = c.last_status.to_lowercase(); st.starts_with("error") || st.contains("failed") || st.contains("couldn't") },
            // The postings list is what the user sees, so count it; older scans stored a different number.
            found: if postings.is_empty() { c.last_found.unwrap_or(0) as i32 } else { postings.len() as i32 },
            matched: postings.iter().filter(|p| posting_matched(p)).count() as i32,
        }
    }).collect();
    let index = selected_id.and_then(|id| rows.iter().position(|r| r.id as i64 == id));
    app.set_companies(model(rows));
    app.set_open_company(index.map(|i| i as i32).unwrap_or(-1));
    let postings = index.map(|i| all[i].last_postings.as_array().cloned().unwrap_or_default()).unwrap_or_default();
    let mut posting_rows: Vec<PostingRow> = postings.iter().map(|p| PostingRow {
        title: s(&p["title"]).into(), location: s(&p["location"]).into(), url: s(&p["url"]).into(),
        matched: posting_matched(p), reason: s(&p["reason"]).into(),
    }).collect();
    posting_rows.sort_by_key(|p| !p.matched);
    let (rows, page, pages) = paged(posting_rows, view(|v| v.post_page));
    view(|v| v.post_page = page);
    app.set_postings(model(rows));
    app.set_postings_page(page as i32);
    app.set_postings_pages(pages as i32);
}

// ---------- wiring ----------

pub struct Wiring {
    pub store: Store,
    pub ctx: Ctx,
    pub rt: Handle,
    pub data_dir: std::path::PathBuf,
}

pub fn wire(ui: &AppWindow, w: Rc<Wiring>) {
    PREVIEW.with(|p| p.borrow_mut().rt = Some(w.rt.clone()));
    let app = ui.global::<App>();
    app.set_data_dir(friendly_path(&w.data_dir.to_string_lossy()).into());
    if let Some(a) = db::lock(&w.store).setting("appearance").ok().and_then(|v| v.as_str().map(String::from)) {
        app.set_appearance(a.into());
    }
    app.set_date_line(chrono::Local::now().format("%A %-d %B").to_string().into());
    app.set_templates(model(pdf::TEMPLATES.iter().map(|t| TemplateRow { id: t.id.into(), name: t.name.into(), description: t.description.into() }).collect()));
    app.set_providers(model(ai::PROVIDERS.iter().map(|p| ProviderRow {
        id: p.id.into(), label: p.label.into(), needs_key: p.needs_key, base_url: p.base_url.into(),
        key_hint: p.key_hint.into(), key_url: p.key_url.into(), default_model: p.default_model.into(),
    }).collect()));
    app.set_country_options(model(crate::options::COUNTRIES.iter().filter(|c| **c != "Remote").map(|c| SharedString::from(*c)).collect()));
    app.set_provider_labels(model(ai::PROVIDERS.iter().map(|p| SharedString::from(p.label)).collect()));
    refresh_lists(ui, &w.store);
    refresh_profiles(ui, &w.store, None);
    refresh_settings(ui, &w.store);
    refresh_discover(ui, &w.store);
    refresh_companies(ui, &w.store);
    wire_web(ui, &w);

    macro_rules! on {
        ($name:ident, |$ui:ident, $w:ident $(, $arg:ident)*| $body:block) => {{
            let weak = ui.as_weak();
            let w = w.clone();
            app.$name(move |$($arg),*| {
                let Some($ui) = weak.upgrade() else { return Default::default() };
                let $w = &w;
                $body
            });
        }};
    }

    // navigation
    on!(on_go, |ui, w, page| {
        let app = ui.global::<App>();
        app.set_open_job(-1);
        app.set_page(page);
        match page {
            2 => refresh_discover(&ui, &w.store),
            3 => refresh_companies(&ui, &w.store),
            4 => refresh_profiles(&ui, &w.store, None),
            5 => refresh_settings(&ui, &w.store),
            _ => refresh_lists(&ui, &w.store),
        }
    });
    on!(on_open_job_page, |ui, w, id| {
        let app = ui.global::<App>();
        view(|v| v.version = None);
        let fresh = app.get_open_job() != id;
        app.set_open_job(id);
        if fresh {
            app.set_job_tab(0);
        }
        refresh_job(&ui, &w.store);
        let j = app.get_job();
        if j.working {
            app.set_job_tab(1);
        }
    });
    on!(on_close_job, |ui, w| {
        let app = ui.global::<App>();
        app.set_open_job(-1);
        if app.get_page() == 0 || app.get_page() > 1 {
            app.set_page(1);
        }
        refresh_lists(&ui, &w.store);
    });
    on!(on_filter_changed, |ui, w, search, status| {
        view(|v| { v.search = search.to_string(); v.status = status.to_string(); v.jobs_page = 0; });
        refresh_lists(&ui, &w.store);
    });

    // applications
    on!(on_add_application, |ui, w, url, text, profile| {
        let url = url.trim().to_string();
        if !url.is_empty() && !(url.starts_with("http://") || url.starts_with("https://")) {
            toast(&ui, "Use the full link, starting with https://");
            return;
        }
        // The same posting tracked before (open or closed)? Say so before starting the agents again.
        if !url.is_empty() {
            let key = scanner::url_key(&url);
            let earlier = db::lock(&w.store).jobs().unwrap_or_default().into_iter().find(|j| !j.url.is_empty() && scanner::url_key(&j.url) == key);
            if let Some(j) = earlier {
                let app = ui.global::<App>();
                let when = if !j.applied_at.is_empty() { format!("You applied on {}", short_date(&j.applied_at)) } else { format!("You added it on {}", short_date(&j.created_at)) };
                let state = if db::is_closed(&j.status) { "it's closed".to_string() } else { format!("it's at {}", j.status) };
                app.set_duplicate_note(format!("You've seen this job before: {} at {}. {when}, and {state}.", title_or(&j), j.company).into());
                app.set_duplicate_job(j.id as i32);
                view(|v| v.pending_add = Some((url, text.to_string(), profile)));
                return;
            }
        }
        start_application(&ui, w, &url, &text, profile);
    });
    on!(on_add_anyway, |ui, w| {
        if let Some((url, text, profile)) = view(|v| v.pending_add.take()) {
            start_application(&ui, w, &url, &text, profile);
        }
    });
    on!(on_close_application, |ui, w, id, reason, note| {
        let r = close_job(&w.store, id as i64, &reason, &note);
        toast(&ui, match r { Ok(()) => "Closed. It stays in your history.".to_string(), Err(e) => format!("{e}") });
        refresh_job(&ui, &w.store);
        refresh_lists(&ui, &w.store);
    });
    on!(on_close_selected, |ui, w, reason, note| {
        let ids: Vec<i64> = view(|v| v.selected.drain().collect());
        let failed = ids.iter().filter(|id| close_job(&w.store, **id, &reason, &note).is_err()).count();
        toast(&ui, if failed == 0 {
            format!("Closed {} application{}. They stay in your history.", ids.len(), if ids.len() == 1 { "" } else { "s" })
        } else {
            format!("Closed {} of {}; the others no longer exist.", ids.len() - failed, ids.len())
        });
        refresh_lists(&ui, &w.store);
    });
    on!(on_toggle_select, |ui, w, id| {
        view(|v| if !v.selected.remove(&(id as i64)) { v.selected.insert(id as i64); });
        refresh_lists(&ui, &w.store);
    });
    on!(on_clear_selection, |ui, w| {
        view(|v| v.selected.clear());
        refresh_lists(&ui, &w.store);
    });
    on!(on_reopen_application, |ui, w, id| {
        let r = (|| -> anyhow::Result<()> {
            let db = db::lock(&w.store);
            let j = db.job(id as i64)?;
            let status = if !j.applied_at.is_empty() { "Applied" } else if j.current_version.is_some() { "Ready" } else if !j.evaluation.is_null() { "Evaluated" } else { "New" };
            db.update_job(id as i64, &[("status", json!(status))])?;
            db.add_event(id as i64, "status", status, "Reopened")?;
            Ok(())
        })();
        if let Err(e) = r {
            toast(&ui, format!("{e}"));
        }
        refresh_job(&ui, &w.store);
        refresh_lists(&ui, &w.store);
    });
    on!(on_go_page, |ui, w, list, page| {
        let page = page.max(0) as usize;
        match list.as_str() {
            "jobs" => { view(|v| v.jobs_page = page); refresh_lists(&ui, &w.store); }
            "discover" => { view(|v| v.disc_page = page); refresh_discover(&ui, &w.store); }
            _ => { view(|v| v.post_page = page); refresh_companies(&ui, &w.store); }
        }
    });
    on!(on_check_updates, |ui, w| { check_updates(&ui, w, true); });
    on!(on_set_update_check, |ui, w, on| {
        let _ = db::lock(&w.store).set_setting("update_check", &json!(on));
        ui.global::<App>().set_update_check(on);
        if on { check_updates(&ui, w, true); }
    });
    on!(on_install_update, |ui, w| {
        let app = ui.global::<App>();
        if app.get_update_busy() { return; }
        let Some(a) = PENDING_UPDATE.with(|p| p.borrow().clone()) else { return };
        app.set_update_busy(true);
        app.set_update_status(format!("Downloading version {}…", a.version).into());
        let (weak, dir) = (ui.as_weak(), w.data_dir.clone());
        let pw = weak.clone();
        let progress = move |f: f32| {
            let _ = pw.upgrade_in_event_loop(move |ui| {
                let app = ui.global::<App>();
                app.set_update_progress(f);
                app.set_update_status(if f >= 1.0 { "Checking the download and installing…".into() } else { format!("Downloading… {:.0}%", f * 100.0).into() });
            });
        };
        w.rt.spawn(async move {
            let r = crate::update::install(dir, a, progress).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let app = ui.global::<App>();
                match r {
                    Ok(()) => { app.set_update_status("Installed. Restarting…".into()); let _ = slint::quit_event_loop(); }
                    Err(e) => { app.set_update_busy(false); app.set_update_status(format!("The update didn't install: {e:#}").into()); }
                }
            });
        });
    });
    on!(on_open_url, |ui, w, url| {
        let _ = w;
        if let Err(e) = open::that_detached(url.as_str()) {
            toast(&ui, format!("Couldn't open the link: {e}"));
        }
    });
    on!(on_open_file, |ui, w, path| {
        let _ = w;
        if path.is_empty() || !std::path::Path::new(path.as_str()).exists() {
            toast(&ui, "The PDF file is missing. Render the version again from its Resume tab.");
        } else if let Err(e) = open::that_detached(path.as_str()) {
            toast(&ui, format!("Couldn't open the file: {e}"));
        }
    });
    on!(on_show_file, |ui, w, path| {
        let _ = w;
        if let Some(dir) = std::path::Path::new(path.as_str()).parent() {
            if let Err(e) = open::that_detached(dir) {
                toast(&ui, format!("Couldn't open the folder: {e}"));
            }
        }
    });
    on!(on_copy, |ui, w, text| {
        let _ = w;
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())) {
            Ok(()) => toast(&ui, "Copied"),
            Err(e) => toast(&ui, format!("Couldn't copy: {e}")),
        }
    });
    on!(on_set_status, |ui, w, id, status| {
        let id = id as i64;
        let result = (|| -> anyhow::Result<()> {
            let db = db::lock(&w.store);
            let j = db.job(id)?;
            if j.status == status.as_str() {
                return Ok(());
            }
            let mut fields = vec![("status", json!(status.as_str()))];
            if status == "Applied" && j.applied_at.is_empty() {
                fields.push(("applied_at", json!(db::now())));
            }
            db.update_job(id, &fields)?;
            db.add_event(id, "status", &status, &format!("Moved to {status}"))?;
            Ok(())
        })();
        if let Err(e) = result {
            toast(&ui, format!("Couldn't change the status: {e}"));
        }
        refresh_job(&ui, &w.store);
        refresh_lists(&ui, &w.store);
    });
    on!(on_save_progress, |ui, w, id, step, date, notes| {
        let date = date.trim().to_string();
        if !date.is_empty() && chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
            toast(&ui, "Write the date as YYYY-MM-DD");
            return;
        }
        let r = db::lock(&w.store).update_job(id as i64, &[("next_step", json!(step.trim())), ("next_step_date", json!(date)), ("notes", json!(notes.as_str()))]);
        toast(&ui, if r.is_ok() { "Saved".to_string() } else { format!("Couldn't save: {}", r.unwrap_err()) });
        refresh_job(&ui, &w.store);
        refresh_lists(&ui, &w.store);
    });
    on!(on_delete_job, |ui, w, id| {
        let confirmed = rfd::MessageDialog::new()
            .set_title("Delete this application?")
            .set_description("Its resumes, cover letter, answers and history are deleted too. This can't be undone.")
            .set_buttons(rfd::MessageButtons::OkCancelCustom("Delete".into(), "Cancel".into()))
            .show();
        if confirmed != rfd::MessageDialogResult::Custom("Delete".into()) {
            return;
        }
        let _ = db::lock(&w.store).delete_job(id as i64);
        let app = ui.global::<App>();
        app.set_open_job(-1);
        refresh_lists(&ui, &w.store);
        toast(&ui, "Application deleted");
    });
    on!(on_rerun, |ui, w, id| {
        if !require_ai(&ui, w) { return; }
        w.rt.spawn(pipeline::run(w.ctx.clone(), id as i64, Start::Full));
        let _ = db::lock(&w.store).update_job(id as i64, &[("stage", json!("queued")), ("error", Value::Null)]);
        ui.global::<App>().set_job_tab(1);
        refresh_job(&ui, &w.store);
    });
    on!(on_tailor, |ui, w, id| {
        if !require_ai(&ui, w) { return; }
        w.rt.spawn(pipeline::run(w.ctx.clone(), id as i64, Start::Tailor));
        let _ = db::lock(&w.store).update_job(id as i64, &[("stage", json!("queued")), ("error", Value::Null)]);
        refresh_job(&ui, &w.store);
    });
    on!(on_revise, |ui, w, id, instruction| {
        if !require_ai(&ui, w) { return; }
        w.rt.spawn(pipeline::revise(w.ctx.clone(), id as i64, instruction.to_string()));
        let _ = db::lock(&w.store).update_job(id as i64, &[("stage", json!("revising"))]);
        view(|v| v.version = None);
        refresh_job(&ui, &w.store);
    });
    on!(on_restyle, |ui, w, id, version, template| {
        let (ctx, weak, store) = (w.ctx.clone(), ui.as_weak(), w.store.clone());
        toast(&ui, "Rendering…");
        w.rt.spawn(async move {
            let r = pipeline::restyle(ctx, id as i64, version as i64, template.to_string()).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                match r {
                    Ok(_) => { view(|v| v.version = None); toast(&ui, "New version saved in the new design"); }
                    Err(e) => toast(&ui, format!("Couldn't render it: {e:#}")),
                }
                refresh_job(&ui, &store);
            });
        });
    });
    on!(on_make_current, |ui, w, id, version| {
        let r = db::lock(&w.store).make_current(id as i64, version as i64);
        if let Err(e) = r {
            toast(&ui, format!("{e}"));
        } else {
            let db = db::lock(&w.store);
            if let Ok(j) = db.job(id as i64) {
                let _ = rag::sync_job(&db, &j);
            }
        }
        refresh_job(&ui, &w.store);
        refresh_lists(&ui, &w.store);
    });
    on!(on_open_viewer, |ui, w| {
        let app = ui.global::<App>();
        app.set_viewer_pages(model(vec![]));
        app.set_viewer_zoom(1.0);
        app.set_viewer_open(true);
        // Print resolution (about 290 dpi), so text stays sharp when zoomed in.
        let vid = app.get_version().id as i64;
        let template = app.get_preview_template().to_string();
        let inputs = (|| -> anyhow::Result<_> {
            let db = db::lock(&w.store);
            let v = db.version(vid)?;
            let job = db.job(v.job_id)?;
            let profile = match job.profile_id { Some(id) => db.profile(id)?, None => db.active_profile()? };
            Ok((serde_json::from_value::<crate::resume::Resume>(v.resume)?, pipeline::header(&profile), pipeline::page_size(&job.jd)))
        })();
        let Ok((resume, header, size)) = inputs else { return };
        let weak = ui.as_weak();
        w.rt.spawn(async move {
            let pages = tokio::task::spawn_blocking(move || pdf::render_pages(&resume, &header, &template, size, 4.0, 8)).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let app = ui.global::<App>();
                if let (true, Ok(Ok(pages))) = (app.get_viewer_open(), pages) {
                    app.set_viewer_pages(model(pages.iter().map(to_image).collect()));
                }
            });
        });
    });
    on!(on_preview_design, |ui, w, template| {
        ui.global::<App>().set_preview_template(template);
        ensure_preview(&ui, &w.store);
    });
    on!(on_select_version, |ui, w, version| {
        view(|v| v.version = Some(version as i64));
        refresh_job(&ui, &w.store);
    });
    on!(on_revise_cover, |ui, w, id, instruction| {
        if !require_ai(&ui, w) { return; }
        w.rt.spawn(pipeline::revise_cover(w.ctx.clone(), id as i64, instruction.to_string()));
        let _ = db::lock(&w.store).update_job(id as i64, &[("stage", json!("writing"))]);
        refresh_job(&ui, &w.store);
    });
    on!(on_save_cover, |ui, w, id, text| {
        let db = db::lock(&w.store);
        let _ = db.update_job(id as i64, &[("cover_letter", json!(text.as_str()))]);
        if let Ok(j) = db.job(id as i64) {
            let _ = rag::sync_job(&db, &j);
        }
        drop(db);
        toast(&ui, "Cover letter saved");
        refresh_job(&ui, &w.store);
    });
    on!(on_ask, |ui, w, id, question| {
        if !require_ai(&ui, w) { return; }
        let app = ui.global::<App>();
        app.set_chat_busy(true);
        // Show the question right away; it's stored once answered.
        let rows = app.get_chat();
        let mut all: Vec<ChatRow> = rows.iter().collect();
        all.push(ChatRow { id: -1, role: "user".into(), content: question.clone(), meta: SharedString::new(), saved: false });
        app.set_chat(model(all));
        let (ctx, weak, store) = (w.ctx.clone(), ui.as_weak(), w.store.clone());
        w.rt.spawn(async move {
            let r = chat::answer(&ctx, id as i64, &question).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.global::<App>().set_chat_busy(false);
                if let Err(e) = r {
                    toast(&ui, format!("No answer: {e:#}"));
                }
                refresh_job(&ui, &store);
            });
        });
    });
    on!(on_toggle_saved, |ui, w, msg| {
        match chat::toggle_saved(&w.ctx, msg as i64) {
            Ok(true) => toast(&ui, "Saved to your library. Later applications can reuse it."),
            Ok(false) => toast(&ui, "Removed from your library"),
            Err(e) => toast(&ui, format!("{e}")),
        }
        refresh_job(&ui, &w.store);
        let embed_store = w.store.clone();
        let ctx = w.ctx.clone();
        let _ = ctx;
        w.rt.spawn(async move { let _ = rag::embed_builtin(&embed_store).await; });
    });

    // profile
    on!(on_select_profile, |ui, w, id| { refresh_profiles(&ui, &w.store, Some(id as i64)); });
    on!(on_new_profile, |ui, w| {
        let id = db::lock(&w.store).create_profile("New profile");
        if let Ok(id) = id {
            refresh_profiles(&ui, &w.store, Some(id));
            toast(&ui, "New profile created. Add a CV to it.");
        }
    });
    on!(on_duplicate_profile, |ui, w| {
        let f = ui.global::<App>().get_profile();
        if let Ok(id) = db::lock(&w.store).duplicate_profile(f.id as i64, &format!("{} (copy)", f.name)) {
            refresh_profiles(&ui, &w.store, Some(id));
        }
    });
    on!(on_delete_profile, |ui, w| {
        let f = ui.global::<App>().get_profile();
        let confirmed = rfd::MessageDialog::new()
            .set_title(format!("Delete the profile “{}”?", f.name))
            .set_description("Its CV, documents and saved answers are deleted. Applications made with it are kept.")
            .set_buttons(rfd::MessageButtons::OkCancelCustom("Delete".into(), "Cancel".into()))
            .show();
        if confirmed != rfd::MessageDialogResult::Custom("Delete".into()) {
            return;
        }
        match db::lock(&w.store).delete_profile(f.id as i64) {
            Ok(()) => toast(&ui, "Profile deleted"),
            Err(e) => toast(&ui, format!("{e}")),
        }
        refresh_profiles(&ui, &w.store, Some(-1));
    });
    on!(on_activate_profile, |ui, w| {
        let id = ui.global::<App>().get_profile().id as i64;
        {
            let db = db::lock(&w.store);
            let _ = db.set_active_profile(id);
            if let Ok(p) = db.profile(id) {
                let _ = sync_search(&db, &p);
            }
        }
        refresh_profiles(&ui, &w.store, Some(id));
        toast(&ui, "New applications will use this profile");
    });
    on!(on_save_profile, |ui, w| {
        let f = ui.global::<App>().get_profile();
        let r = (|| -> anyhow::Result<()> {
            let db = db::lock(&w.store);
            let mut p = db.profile(f.id as i64)?;
            apply_form(&mut p, &f);
            db.save_profile(&p)?;
            rag::sync_profile(&db, &p)?;
            if p.active {
                sync_search(&db, &p)?;
            }
            Ok(())
        })();
        match r {
            Ok(()) => {
                toast(&ui, "Profile saved");
                let store = w.store.clone();
                w.rt.spawn(async move { let _ = rag::embed_builtin(&store).await; });
            }
            Err(e) => toast(&ui, format!("Couldn't save: {e}")),
        }
        refresh_profiles(&ui, &w.store, Some(f.id as i64));
    });
    on!(on_import_cv, |ui, w| {
        let Some(path) = rfd::FileDialog::new().add_filter("Resume", &["pdf", "md", "txt"]).set_title("Choose your resume").pick_file() else { return };
        let text = match read_document(&path) {
            Ok(t) => t,
            Err(e) => { toast(&ui, format!("Couldn't read it: {e}")); return; }
        };
        let pid = ui.global::<App>().get_profile().id as i64;
        let (ctx, weak, store) = (w.ctx.clone(), ui.as_weak(), w.store.clone());
        ui.global::<App>().set_profile_busy(true);
        toast(&ui, "Reading your resume…");
        w.rt.spawn(async move {
            let r = pipeline::import_cv(&ctx, pid, &text).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.global::<App>().set_profile_busy(false);
                match r {
                    Ok(()) => toast(&ui, "CV imported. Check it, then save."),
                    Err(e) => toast(&ui, format!("Couldn't import: {e:#}")),
                }
                refresh_profiles(&ui, &store, Some(pid));
            });
        });
    });
    on!(on_add_document, |ui, w, title, text| {
        let pid = ui.global::<App>().get_profile().id as i64;
        let r = rag::upsert(&db::lock(&w.store), Some(pid), None, "doc", title.trim(), &text);
        match r {
            Ok(_) => {
                toast(&ui, "Added to the library");
                let store = w.store.clone();
                w.rt.spawn(async move { let _ = rag::embed_builtin(&store).await; });
            }
            Err(e) => toast(&ui, format!("{e}")),
        }
        refresh_profiles(&ui, &w.store, Some(pid));
    });
    on!(on_import_document, |ui, w| {
        let Some(path) = rfd::FileDialog::new().add_filter("Document", &["pdf", "md", "txt"]).pick_file() else { return };
        let pid = ui.global::<App>().get_profile().id as i64;
        let title = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Document".into());
        match read_document(&path).and_then(|text| rag::upsert(&db::lock(&w.store), Some(pid), None, "doc", &title, &text)) {
            Ok(_) => {
                toast(&ui, format!("Added “{title}” to the library"));
                let store = w.store.clone();
                w.rt.spawn(async move { let _ = rag::embed_builtin(&store).await; });
            }
            Err(e) => toast(&ui, format!("Couldn't read it: {e}")),
        }
        refresh_profiles(&ui, &w.store, Some(pid));
    });
    on!(on_delete_document, |ui, w, id| {
        let _ = rag::delete_document(&db::lock(&w.store), id as i64);
        let pid = ui.global::<App>().get_profile().id as i64;
        refresh_profiles(&ui, &w.store, Some(pid));
    });

    // settings
    on!(on_provider_changed, |ui, w, index| {
        let _ = w;
        let app = ui.global::<App>();
        let p = &ai::PROVIDERS[index.clamp(0, ai::PROVIDERS.len() as i32 - 1) as usize];
        let mut f = app.get_ai();
        f.provider_index = index;
        f.base_url = p.base_url.into();
        f.models = p.default_model.into();
        f.key_status = ai::mask(&ai::load_key(p.id)).into();
        f.new_key = SharedString::new();
        app.set_ai(f);
        app.set_model_options(model(vec![]));
        app.set_ai_status(SharedString::new());
    });
    on!(on_load_models, |ui, w| {
        let app = ui.global::<App>();
        let (settings, key) = form_settings(&app.get_ai());
        let weak = ui.as_weak();
        app.set_ai_busy(true);
        app.set_ai_status(SharedString::new());
        w.rt.spawn(async move {
            let r = async { ai::Ai::new(settings, key)?.list_models().await }.await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let app = ui.global::<App>();
                app.set_ai_busy(false);
                match r {
                    Ok(models) if models.is_empty() => { app.set_ai_ok(false); app.set_ai_status("The provider listed no models".into()); }
                    Ok(models) => { app.set_ai_ok(true); app.set_ai_status(format!("{} models available", models.len()).into()); app.set_model_options(strings(&models)); }
                    Err(e) => { app.set_ai_ok(false); app.set_ai_status(format!("{e:#}").into()); }
                }
            });
        });
    });
    on!(on_test_ai, |ui, w| {
        let app = ui.global::<App>();
        let (settings, key) = form_settings(&app.get_ai());
        let weak = ui.as_weak();
        app.set_ai_busy(true);
        app.set_ai_status(SharedString::new());
        w.rt.spawn(async move {
            let r = async { ai::Ai::new(settings, key)?.test().await }.await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let app = ui.global::<App>();
                app.set_ai_busy(false);
                app.set_ai_ok(r.is_ok());
                app.set_ai_status(match r { Ok(m) => m, Err(e) => format!("{e:#}") }.into());
            });
        });
    });
    on!(on_save_ai, |ui, w| {
        let app = ui.global::<App>();
        let (settings, key) = form_settings(&app.get_ai());
        let r = (|| -> anyhow::Result<()> {
            if settings.models.is_empty() {
                anyhow::bail!("Enter at least one model");
            }
            if let Some(k) = &key {
                ai::save_key(&settings.provider, k)?;
            }
            ai::Ai::new(settings.clone(), None)?; // validates URL and key presence
            db::lock(&w.store).set_setting("ai", &serde_json::to_value(&settings)?)?;
            Ok(())
        })();
        app.set_ai_ok(r.is_ok());
        app.set_ai_status(match &r { Ok(()) => "Saved.".to_string(), Err(e) => format!("{e:#}") }.into());
        if r.is_ok() {
            refresh_settings(&ui, &w.store);
            check_ai(&ui, w);
            let store = w.store.clone();
            w.rt.spawn(async move { let _ = rag::embed_builtin(&store).await; });
        }
    });
    on!(on_reindex, |ui, w| {
        {
            let db = db::lock(&w.store);
            for p in db.profiles().unwrap_or_default() {
                let _ = rag::sync_profile(&db, &p);
            }
            for j in db.jobs().unwrap_or_default() {
                let _ = rag::sync_job(&db, &j);
            }
        }
        toast(&ui, "Re-reading your documents…");
        let (store, weak) = (w.store.clone(), ui.as_weak());
        w.rt.spawn(async move {
            let r = rag::embed_builtin(&store).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                toast(&ui, match r { Ok(_) => "Smart search is up to date".to_string(), Err(e) => format!("Smart search couldn't finish: {e:#}") });
                refresh_settings(&ui, &store);
            });
        });
    });

    on!(on_show_data_folder, |ui, w| {
        if let Err(e) = open::that_detached(&w.data_dir) {
            toast(&ui, format!("Couldn't open the folder: {e}"));
        }
    });
    on!(on_move_data_folder, |ui, w| {
        let Some(parent) = rfd::FileDialog::new().set_title("Choose where to keep your Job Command Center folder").pick_folder() else { return };
        let target = parent.join("Job Command Center");
        match move_data(&w.store, &w.data_dir, &target) {
            Ok(()) => toast(&ui, format!("Copied to {}. Quit and reopen the app to use it; the old folder is left as it was.", friendly_path(&target.to_string_lossy()))),
            Err(e) => toast(&ui, format!("Couldn't move it: {e:#}")),
        }
    });
    on!(on_set_appearance, |ui, w, value| {
        ui.global::<App>().set_appearance(value.clone());
        let _ = db::lock(&w.store).set_setting("appearance", &json!(value.as_str()));
    });
    on!(on_chip_add, |ui, w, field, value| {
        let _ = w;
        let app = ui.global::<App>();
        let v = value.trim().trim_end_matches(',').trim().to_string();
        if let Some(mut items) = chip_list(&app, &field) {
            if !v.is_empty() && !items.iter().any(|x| x.eq_ignore_ascii_case(&v)) {
                items.push(v);
                set_chip_list(&app, &field, items);
            }
        }
        app.set_suggestions(model(vec![]));
    });
    on!(on_chip_remove, |ui, w, field, index| {
        let _ = w;
        let app = ui.global::<App>();
        if let Some(items) = chip_list(&app, &field) {
            let kept: Vec<String> = items.into_iter().enumerate().filter(|(i, _)| *i != index as usize).map(|(_, x)| x).collect();
            set_chip_list(&app, &field, kept);
        }
    });
    on!(on_chip_typed, |ui, w, field, text| {
        use std::sync::atomic::Ordering;
        let app = ui.global::<App>();
        let chosen = chip_list(&app, &field).unwrap_or_default();
        let local = crate::options::local(&field, &text, &chosen);
        app.set_suggest_field(field.clone());
        app.set_suggestions(strings(&local));
        let gen = SUGGEST_GEN.fetch_add(1, Ordering::SeqCst) + 1;
        if !crate::options::has_remote(&field) || text.trim().chars().count() < 2 {
            return;
        }
        let weak = ui.as_weak();
        w.rt.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await; // wait for a pause in typing
            if SUGGEST_GEN.load(Ordering::SeqCst) != gen {
                return;
            }
            let found = crate::options::remote(&field, &text).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if SUGGEST_GEN.load(Ordering::SeqCst) != gen {
                    return;
                }
                let mut all = local;
                for f in found {
                    if !all.iter().any(|x| x.eq_ignore_ascii_case(&f)) && !chosen.iter().any(|x| x.eq_ignore_ascii_case(&f)) {
                        all.push(f);
                    }
                }
                all.truncate(10);
                ui.global::<App>().set_suggestions(strings(&all));
            });
        });
    });
    on!(on_save_search, |ui, w| {
        let f = ui.global::<App>().get_search();
        let (Ok(limit), Ok(interval)) = (f.limit.trim().parse::<u32>(), f.interval.trim().parse::<f64>()) else {
            toast(&ui, "Use whole numbers for the limit and hours");
            return;
        };
        let db = db::lock(&w.store);
        let mut p = scanner::prefs(&db);
        p.auto_process = f.auto;
        p.auto_process_limit = limit;
        p.scan_interval_hours = interval.max(0.0);
        let r = scanner::save_prefs(&db, &p);
        drop(db);
        toast(&ui, match r { Ok(()) => "Saved".to_string(), Err(e) => format!("{e}") });
        refresh_settings(&ui, &w.store);
        refresh_discover(&ui, &w.store);
    });

    // discover + companies
    on!(on_discover_filter, |ui, w, search, country, state| {
        view(|v| { v.disc_search = search.to_string(); v.disc_country = country.to_string(); v.disc_state = state.to_string(); v.disc_page = 0; });
        refresh_discover(&ui, &w.store);
    });
    on!(on_queue_discovered, |ui, w, id| {
        if !require_ai(&ui, w) { return; }
        let r = scanner::queue_discovered(&db::lock(&w.store), id as i64);
        match r {
            Ok((jid, created)) => {
                if created {
                    w.rt.spawn(pipeline::run(w.ctx.clone(), jid, Start::Full));
                    toast(&ui, "The agents are on it. It's in Applications.");
                } else {
                    toast(&ui, "You already track this role");
                }
            }
            Err(e) => toast(&ui, format!("{e}")),
        }
        refresh_discover(&ui, &w.store);
        refresh_lists(&ui, &w.store);
    });
    on!(on_dismiss_discovered, |ui, w, id| {
        let _ = scanner::set_discovered_state(&db::lock(&w.store), id as i64, "dismissed");
        refresh_discover(&ui, &w.store);
    });
    on!(on_scan, |ui, w, company| {
        let app = ui.global::<App>();
        if app.get_scanning() {
            return;
        }
        app.set_scanning(true);
        app.set_scan_progress(0.0);
        app.set_scan_line("Starting…".into());
        let (store, weak, ctx) = (w.store.clone(), ui.as_weak(), w.ctx.clone());
        let ids = if company >= 0 { Some(vec![company as i64]) } else { None };
        w.rt.spawn(async move {
            let progress_weak = weak.clone();
            let summary = scanner::scan(&store, ids, move |p: scanner::Progress| {
                let line = format!("{}: {}", p.company, p.message);
                let frac = if p.total == 0 { 0.0 } else { p.done as f32 / p.total as f32 };
                let _ = progress_weak.upgrade_in_event_loop(move |ui| {
                    let app = ui.global::<App>();
                    app.set_scan_progress(frac);
                    app.set_scan_line(line.into());
                });
            }).await;
            let started = auto_process(&ctx, &summary);
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let app = ui.global::<App>();
                app.set_scanning(false);
                let mut msg = format!("Scanned {} companies: {} open roles, {} match, {} new",
                                      summary.companies, summary.postings, summary.matched, summary.new);
                if !summary.errors.is_empty() {
                    msg.push_str(&format!(". {} couldn't be read", summary.errors.len()));
                }
                if started > 0 {
                    msg.push_str(&format!(". Tailoring {started} automatically"));
                }
                toast(&ui, msg);
                refresh_discover(&ui, &store);
                refresh_companies(&ui, &store);
            });
        });
    });
    on!(on_select_company, |ui, w, id| {
        let app = ui.global::<App>();
        let i = app.get_companies().iter().position(|c| c.id == id).map(|i| i as i32).unwrap_or(-1);
        app.set_open_company(i);
        view(|v| v.post_page = 0);
        refresh_companies(&ui, &w.store);
    });
    on!(on_toggle_favorite, |ui, w, id| {
        let db = db::lock(&w.store);
        let fav = scanner::companies(&db).unwrap_or_default().iter().find(|c| c.id == id as i64).map(|c| c.favorite).unwrap_or(false);
        let _ = scanner::set_favorite(&db, id as i64, !fav);
        drop(db);
        refresh_companies(&ui, &w.store);
        if ui.global::<App>().get_setup_stage() >= 0 {
            refresh_setup_companies(&ui, &w.store);
        }
    });
    on!(on_setup_connect_ai, |ui, w| {
        let app = ui.global::<App>();
        let (settings, key) = form_settings(&app.get_ai());
        let saved = (|| -> anyhow::Result<()> {
            if settings.models.is_empty() {
                anyhow::bail!("Enter the model to use");
            }
            if let Some(k) = &key {
                ai::save_key(&settings.provider, k)?;
            }
            ai::Ai::new(settings.clone(), None)?;
            db::lock(&w.store).set_setting("ai", &serde_json::to_value(&settings)?)?;
            Ok(())
        })();
        if let Err(e) = saved {
            app.set_ai_ok(false);
            app.set_ai_status(format!("{e:#}").into());
            return;
        }
        app.set_ai_busy(true);
        app.set_ai_status("Asking the AI to reply…".into());
        let (store, weak) = (w.store.clone(), ui.as_weak());
        w.rt.spawn(async move {
            let r = async { ai::Ai::new(settings, None)?.test().await }.await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let app = ui.global::<App>();
                app.set_ai_busy(false);
                match r {
                    Ok(m) => {
                        app.set_ai_ok(true);
                        app.set_ai_status(format!("Connected. {m}").into());
                        app.set_ai_problem(SharedString::new());
                        app.set_ai_connected(true);
                        app.set_setup_stage(2);
                        refresh_settings(&ui, &store);
                    }
                    Err(e) => {
                        app.set_ai_ok(false);
                        app.set_ai_status(format!("It didn't answer: {e:#}").into());
                    }
                }
            });
        });
    });
    on!(on_finish_setup, |ui, w| {
        let _ = db::lock(&w.store).set_setting("setup_done", &json!(true));
        let app = ui.global::<App>();
        app.set_setup_stage(-1);
        app.invoke_go(0);
        let favourites = scanner::companies(&db::lock(&w.store)).unwrap_or_default().iter().filter(|c| c.favorite).count();
        if favourites > 0 {
            app.invoke_scan(-1);
            toast(&ui, format!("Scanning your {favourites} companies for matching roles"));
        }
    });
    on!(on_add_company, |ui, w, name, url| {
        let url = url.trim().to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            toast(&ui, "Use the full careers page link, starting with https://");
            return;
        }
        match scanner::add_company(&db::lock(&w.store), name.trim(), &url) {
            Ok(id) => {
                let _ = scanner::set_favorite(&db::lock(&w.store), id, true);
                refresh_companies(&ui, &w.store);
                let app = ui.global::<App>();
                let i = app.get_companies().iter().position(|c| c.id as i64 == id).map(|i| i as i32).unwrap_or(-1);
                app.set_open_company(i);
                app.invoke_scan(id as i32);
            }
            Err(e) => toast(&ui, format!("{e}")),
        }
    });
    on!(on_delete_company, |ui, w, id| {
        let _ = scanner::delete_company(&db::lock(&w.store), id as i64);
        ui.global::<App>().set_open_company(-1);
        refresh_companies(&ui, &w.store);
    });
    on!(on_set_favorites_only, |ui, w, on| {
        let db = db::lock(&w.store);
        let mut p = scanner::prefs(&db);
        p.favorites_only = on;
        let _ = scanner::save_prefs(&db, &p);
        drop(db);
        ui.global::<App>().set_open_company(-1);
        refresh_companies(&ui, &w.store);
    });
}

/// When "tailor new matches automatically" is on, start the agents on the newest matches.
/// Must run inside the tokio runtime. Returns how many applications were started.
fn auto_process(ctx: &Ctx, summary: &scanner::ScanSummary) -> usize {
    let (prefs, queued) = {
        let db = db::lock(&ctx.store);
        let prefs = scanner::prefs(&db);
        if !prefs.auto_process {
            return 0;
        }
        let queued: Vec<i64> = summary.new_ids.iter().take(prefs.auto_process_limit as usize)
            .filter_map(|id| scanner::queue_discovered(&db, *id).ok())
            .filter(|(_, created)| *created).map(|(jid, _)| jid).collect();
        (prefs, queued)
    };
    let _ = prefs;
    for jid in &queued {
        tokio::spawn(pipeline::run(ctx.clone(), *jid, Start::Full));
    }
    queued.len()
}

/// Background scans on the user's schedule ("Scan every N hours"); checks every 10 minutes.
pub fn start_scheduler(ui: &AppWindow, w: &Wiring) {
    let (store, weak, ctx) = (w.store.clone(), ui.as_weak(), w.ctx.clone());
    w.rt.spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
        loop {
            tick.tick().await;
            let due = {
                let db = db::lock(&store);
                let hours = scanner::prefs(&db).scan_interval_hours;
                let last: Option<String> = db.conn.query_row("SELECT MAX(last_scan_at) FROM companies", [], |r| r.get(0)).ok().flatten();
                hours > 0.0 && last.and_then(|l| chrono::DateTime::parse_from_rfc3339(&l).ok())
                    .is_none_or(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_minutes() as f64 >= hours * 60.0)
            };
            if !due || scanner::is_running() {
                continue;
            }
            let summary = scanner::scan(&store, None, |_| {}).await;
            let started = auto_process(&ctx, &summary);
            let store2 = store.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if summary.new > 0 {
                    toast(&ui, format!("Scheduled scan: {} new matches{}", summary.new,
                                       if started > 0 { format!(", tailoring {started}") } else { String::new() }));
                }
                refresh_discover(&ui, &store2);
                refresh_companies(&ui, &store2);
                refresh_lists(&ui, &store2);
            });
        }
    });
}

fn company_rows(all: &[scanner::Company]) -> Vec<CompanyRow> {
    all.iter().map(|c| {
        let (initial, hue) = monogram(&c.name);
        CompanyRow { id: c.id as i32, name: c.name.clone().into(), url: c.careers_url.clone().into(), initial: initial.into(), hue,
                     favorite: c.favorite, ..Default::default() }
    }).collect()
}

pub fn refresh_setup_companies(ui: &AppWindow, store: &Store) {
    let mut all = scanner::companies(&db::lock(store)).unwrap_or_default();
    all.sort_by_key(|c| c.name.to_lowercase());
    ui.global::<App>().set_setup_companies(model(company_rows(&all)));
}

/// What the first-launch screen reports about work already done before the window opened.
pub struct Startup {
    pub first_run: bool,
    pub data_dir: String,
    pub applications: usize,
    pub imported: usize,
    pub companies: usize,
}

/// "~/Documents/Job Command Center" instead of the full home path.
fn friendly_path(p: &str) -> String {
    match directories::UserDirs::new().map(|u| u.home_dir().to_string_lossy().to_string()) {
        Some(home) if p.starts_with(&home) => format!("~{}", &p[home.len()..]),
        _ => p.to_string(),
    }
}

/// Copy the database (consistently, while open) and the resumes into `target`, rewriting the
/// stored PDF paths. The old folder is left untouched.
fn move_data(store: &Store, from: &std::path::Path, target: &std::path::Path) -> anyhow::Result<()> {
    if target.join("job-command-center.db").exists() {
        anyhow::bail!("That folder already has Job Command Center data in it");
    }
    std::fs::create_dir_all(target)?;
    let new_db = target.join("job-command-center.db");
    db::lock(store).conn.execute("VACUUM INTO ?1", rusqlite::params![new_db.to_string_lossy()])?;
    for sub in ["Resumes", "output"] {
        copy_dir(&from.join(sub), &target.join(sub))?;
    }
    let (old, new) = (from.to_string_lossy().to_string(), target.to_string_lossy().to_string());
    let conn = rusqlite::Connection::open(&new_db)?;
    conn.execute("UPDATE jobs SET pdf_path = replace(pdf_path, ?1, ?2) WHERE pdf_path LIKE ?1 || '%'", rusqlite::params![old, new])?;
    conn.execute("UPDATE resume_versions SET pdf_path = replace(pdf_path, ?1, ?2) WHERE pdf_path LIKE ?1 || '%'", rusqlite::params![old, new])?;
    let pointer = crate::pointer_file().ok_or_else(|| anyhow::anyhow!("no settings folder on this system"))?;
    std::fs::create_dir_all(pointer.parent().unwrap_or(target))?;
    std::fs::write(pointer, new)?;
    Ok(())
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn step(title: &str, detail: &str, state: &str) -> SetupStep {
    SetupStep { title: title.into(), detail: detail.into(), state: state.into() }
}

/// Load the search model and index everything, reporting each step. On first launch this
/// drives the "Setting up" screen; afterwards it runs quietly with a line in the sidebar.
pub fn start_setup(ui: &AppWindow, w: &Wiring, info: Startup) {
    let app = ui.global::<App>();
    let mut steps = vec![
        step("Your folder", &format!("Your resumes and everything you add are kept in {}", friendly_path(&info.data_dir)), "done"),
        step("Your applications", &if info.imported > 0 {
            format!("{} applications brought over from your earlier setup", info.imported)
        } else if info.applications > 0 {
            format!("{} applications ready", info.applications)
        } else {
            "Ready for your first application".into()
        }, "done"),
        step("Companies", &format!("{} companies whose job pages the app can check for you", info.companies), "done"),
        step("Smart search", "Helps the assistant find the right parts of your CV and past answers. Built in, works offline.", "working"),
        step("Your documents", "Waiting to start", "waiting"),
    ];
    if info.first_run {
        refresh_setup_companies(ui, &w.store);
        app.set_setup_steps(model(steps.clone()));
        app.set_setup_progress(0.5);
        app.set_setup_stage(0);
    }
    let (store, weak, first) = (w.store.clone(), ui.as_weak(), info.first_run);
    w.rt.spawn(async move {
        let push = |steps: Vec<SetupStep>, progress: f32, ready: bool, line: String| {
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let app = ui.global::<App>();
                if first {
                    app.set_setup_steps(model(steps));
                    app.set_setup_progress(progress);
                    app.set_setup_ready(ready);
                }
                app.set_index_line(line.into());
            });
        };
        let loaded = crate::ai::builtin_embed(&["warm up".to_string()]).await;
        steps[3] = match &loaded {
            Ok(_) => step("Smart search", "Ready. It runs on this computer, so it works offline.", "done"),
            Err(e) => step("Smart search", &format!("Couldn't start ({e:#}). Plain word search still works."), "failed"),
        };
        steps[4] = step("Your documents", "Reading your CV, documents and applications", "working");
        push(steps.clone(), 0.65, false, "Preparing your documents…".into());
        {
            let db = db::lock(&store);
            for p in db.profiles().unwrap_or_default() {
                let _ = rag::sync_profile(&db, &p);
            }
            for j in db.jobs().unwrap_or_default() {
                let _ = rag::sync_job(&db, &j);
            }
        }
        let ai = rag::saved_ai(&store);
        let total = rag::pending_count(&store, ai.as_ref());
        let result = if loaded.is_ok() {
            let report = |done: usize, total: usize| {
                let mut s = steps.clone();
                s[4] = step("Your documents", &format!("Reading your CV, documents and applications: {}% done", 100 * done / total.max(1)), "working");
                push(s, 0.65 + 0.35 * done as f32 / total.max(1) as f32, false, format!("Preparing your documents: {}%", 100 * done / total.max(1)));
            };
            rag::embed_pending_progress(&store, ai.as_ref(), &report).await
        } else {
            Ok(0)
        };
                steps[4] = match result {
            Ok(_) => step("Your documents", "Ready. The assistant can now draw on everything you've added.", "done"),
            Err(e) => step("Your documents", &format!("Plain word search is ready; smart search stopped: {e:#}"), "failed"),
        };
        let _ = total;
        push(steps, 1.0, true, String::new());
    });
}

/// Close an application with a reason: kept in history, recognised if the job comes back.
pub(crate) fn close_job(store: &Store, id: i64, reason: &str, note: &str) -> anyhow::Result<()> {
    let note = note.trim();
    let db = db::lock(store);
    let j = db.job(id)?;
    let mut notes = j.notes.clone();
    if !note.is_empty() {
        notes = format!("{}{}Closed: {note}", notes, if notes.is_empty() { "" } else { "\n\n" });
    }
    db.update_job(id, &[("status", json!("Closed")), ("notes", json!(notes))])?;
    db.add_event(id, "status", "Closed", &format!("Closed: {reason}{}", if note.is_empty() { String::new() } else { format!(". {note}") }))?;
    Ok(())
}

/// Agents need a connected AI. Says so (and opens Settings) instead of failing halfway.
fn require_ai(ui: &AppWindow, w: &Wiring) -> bool {
    if rag::saved_ai(&w.store).is_some() {
        return true;
    }
    toast(ui, "Connect your AI first: Settings → AI service.");
    let app = ui.global::<App>();
    app.set_open_job(-1);
    app.invoke_go(5);
    false
}

/// Ask the saved AI for a one-word reply in the background and show any problem on Today.
pub fn check_ai(ui: &AppWindow, w: &Wiring) {
    let (store, weak) = (w.store.clone(), ui.as_weak());
    w.rt.spawn(async move {
        let Some(ai) = rag::saved_ai(&store) else { return };
        let problem = match ai.test().await {
            Ok(_) => String::new(),
            Err(e) => format!("{e:#}").chars().take(220).collect(),
        };
        let _ = weak.upgrade_in_event_loop(move |ui| ui.global::<App>().set_ai_problem(problem.into()));
    });
}

/// Create the application and start the agents on it, then show it.
fn start_application(ui: &AppWindow, w: &Wiring, url: &str, text: &str, profile: i32) {
    if !require_ai(ui, w) {
        ui.global::<App>().set_adding(false);
        return;
    }
    let app = ui.global::<App>();
    app.set_duplicate_job(-1);
    app.set_duplicate_note(SharedString::new());
    let pid = if profile >= 0 { Some(profile as i64) } else { db::lock(&w.store).active_profile().ok().map(|p| p.id) };
    match db::lock(&w.store).insert_job(url, text.trim(), "", "", "", pid, "manual") {
        Ok(id) => {
            w.rt.spawn(pipeline::run(w.ctx.clone(), id, Start::Full));
            app.set_adding(false);
            app.set_open_job(id as i32);
            app.set_job_tab(1);
            view(|v| v.version = None);
            refresh_job(ui, &w.store);
            refresh_lists(ui, &w.store);
        }
        Err(e) => toast(ui, format!("Couldn't add it: {e}")),
    }
}

/// Settings from the form, plus the newly typed key (None = use the saved one).
fn form_settings(f: &AiForm) -> (AiSettings, Option<String>) {
    let p = &ai::PROVIDERS[(f.provider_index.max(0) as usize).min(ai::PROVIDERS.len() - 1)];
    let base = f.base_url.trim();
    let settings = AiSettings {
        provider: p.id.into(),
        models: f.models.split(',').map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect(),
        base_url: if base == p.base_url { String::new() } else { base.to_string() },
        embed_model: if f.search == 0 { crate::embed::MODEL_ID.into() } else { String::new() },
        embed_provider: if f.search == 0 { "builtin".into() } else { String::new() },
        embed_base_url: String::new(),
    };
    let key = f.new_key.trim();
    (settings, if key.is_empty() { None } else { Some(key.to_string()) })
}

fn read_document(path: &std::path::Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let text = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) {
        pdf::extract_text(&bytes)?.0
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };
    if text.trim().len() < 50 {
        anyhow::bail!("no readable text in that file");
    }
    Ok(text)
}

// ---------- phone and web access ----------

/// The web server the app runs for phones and other computers, and the optional ngrok tunnel.
#[derive(Default)]
struct WebRun {
    shared: Option<crate::web::Shared>,
    server: Option<tokio::task::JoinHandle<()>>,
    port: u16,
    tunnel: Option<tokio::task::JoinHandle<()>>,
    remote_url: String,
    pin: String,
    pin_set: Option<std::time::Instant>,
}

thread_local! {
    static WEB: RefCell<WebRun> = RefCell::new(WebRun::default());
}

/// Passcodes last a day; devices already signed in stay signed in.
const PIN_LIFETIME: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

fn web_saved(store: &Store) -> Value {
    db::lock(store).setting("web").ok().filter(Value::is_object).unwrap_or_else(|| json!({}))
}

fn web_save(store: &Store, patch: Value) {
    let mut v = web_saved(store);
    if let (Some(o), Some(p)) = (v.as_object_mut(), patch.as_object()) {
        o.extend(p.clone());
    }
    let _ = db::lock(store).set_setting("web", &v);
}

/// A fresh passcode: 6 digits on the home network, a longer phrase once the app is reachable from anywhere.
fn web_rotate(ui: &AppWindow) {
    WEB.with(|r| {
        let mut r = r.borrow_mut();
        let pin = if r.tunnel.is_some() { crate::web::new_passphrase() } else { crate::web::new_pin() };
        if let Some(s) = &r.shared {
            s.set_pin(&pin);
        }
        #[cfg(debug_assertions)]
        if std::env::var("JCC_DO").is_ok() {
            eprintln!("web passcode: {pin}"); // end-to-end checks sign in with it
        }
        r.pin = pin;
        r.pin_set = Some(std::time::Instant::now());
    });
    web_show(ui);
}

/// Put the server's state on screen: switches, address, QR code and passcode.
fn web_show(ui: &AppWindow) {
    let app = ui.global::<App>();
    WEB.with(|r| {
        let r = r.borrow();
        let on = r.server.is_some();
        let local = if on {
            let host = crate::web::lan_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "localhost".into());
            format!("http://{host}:{}", r.port)
        } else {
            String::new()
        };
        let remote = r.tunnel.is_some();
        let shown = if remote && !r.remote_url.is_empty() { r.remote_url.clone() } else { local.clone() };
        app.set_web_on(on);
        app.set_web_remote_on(remote);
        app.set_web_local_url(local.into());
        app.set_web_remote_url(r.remote_url.clone().into());
        app.set_web_pin(r.pin.clone().into());
        app.set_web_pin_age(if remote {
            "Reachable from anywhere, so the passcode is longer. It changes by itself every 24 hours.".into()
        } else {
            "Changes by itself every 24 hours. Phones already signed in stay signed in.".into()
        });
        if let Some((px, size)) = (!shown.is_empty()).then(|| crate::web::qr_rgba(&shown, 6)).flatten() {
            app.set_web_qr(slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&px, size, size)));
        }
    });
    app.set_ngrok_key_status(ai::mask(&ai::load_key("ngrok")).into());
}

fn web_stop(ui: &AppWindow) {
    WEB.with(|r| {
        let mut r = r.borrow_mut();
        for t in [r.server.take(), r.tunnel.take()].into_iter().flatten() {
            t.abort();
        }
        *r = WebRun::default();
    });
    web_show(ui);
}

/// Start the server (if it isn't running), then the tunnel when `remote`.
fn web_start(ui: &AppWindow, w: &Rc<Wiring>, remote: bool) {
    let app = ui.global::<App>();
    app.set_web_status("".into());
    let running = WEB.with(|r| r.borrow().server.is_some());
    if !remote && running {
        return web_show(ui);
    }
    let token = ai::load_key("ngrok");
    if remote && token.is_empty() {
        app.set_web_status("Paste your ngrok authtoken below first.".into());
        return web_show(ui);
    }
    let email = web_saved(&w.store)["google_email"].as_str().map(String::from);
    app.set_web_busy(true);
    let (store, ctx, weak) = (w.store.clone(), w.ctx.clone(), ui.as_weak());
    let existing = WEB.with(|r| r.borrow().shared.clone().map(|s| (s, r.borrow().port)));
    w.rt.spawn(async move {
        let mut started = None;
        let port = match existing {
            Some((_, port)) => Ok(port),
            None => crate::web::start(store, ctx, crate::web::new_pin()).await.map(|(s, task, port)| {
                started = Some((s, task));
                port
            }),
        };
        let tunnel = match (&port, remote) {
            (Ok(port), true) => Some(crate::web::start_remote(&token, *port, email).await),
            _ => None,
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let app = ui.global::<App>();
            app.set_web_busy(false);
            let port = match port {
                Ok(p) => p,
                Err(e) => {
                    app.set_web_status(format!("Couldn't start: {e:#}").into());
                    return web_show(&ui);
                }
            };
            WEB.with(|r| {
                let mut r = r.borrow_mut();
                if let Some((s, task)) = started {
                    r.shared = Some(s);
                    r.server = Some(task);
                    r.port = port;
                }
            });
            match tunnel {
                Some(Ok((url, task))) => WEB.with(|r| {
                    let mut r = r.borrow_mut();
                    r.tunnel = Some(task);
                    r.remote_url = url;
                }),
                Some(Err(e)) => app.set_web_status(format!("{e:#}").into()),
                None => {}
            }
            // New passcode whenever the server starts or the reach changes (the remote one is longer).
            web_rotate(&ui);
        });
    });
}

fn wire_web(ui: &AppWindow, w: &Rc<Wiring>) {
    let app = ui.global::<App>();
    let saved = web_saved(&w.store);
    app.set_web_google_email(saved["google_email"].as_str().unwrap_or("").into());
    web_show(ui);

    macro_rules! on {
        ($name:ident, |$ui:ident, $w:ident $(, $arg:ident)*| $body:block) => {{
            let weak = ui.as_weak();
            let w = w.clone();
            app.$name(move |$($arg),*| {
                let Some($ui) = weak.upgrade() else { return Default::default() };
                let $w = &w;
                $body
            });
        }};
    }
    on!(on_web_toggle, |ui, w, on| {
        web_save(&w.store, json!({ "enabled": on, "remote": on && WEB.with(|r| r.borrow().tunnel.is_some()) }));
        if on { web_start(&ui, w, false) } else { web_stop(&ui) }
    });
    on!(on_web_remote_toggle, |ui, w, on| {
        let app = ui.global::<App>();
        let key = app.get_ngrok_new_key().trim().to_string();
        if !key.is_empty() {
            if let Err(e) = ai::save_key("ngrok", &key) {
                app.set_web_status(format!("{e:#}").into());
                return;
            }
            app.set_ngrok_new_key("".into());
        }
        web_save(&w.store, json!({ "google_email": app.get_web_google_email().trim(), "remote": on }));
        if on {
            web_save(&w.store, json!({ "enabled": true }));
            web_start(&ui, w, true);
        } else {
            WEB.with(|r| {
                let mut r = r.borrow_mut();
                if let Some(t) = r.tunnel.take() {
                    t.abort();
                }
                r.remote_url.clear();
            });
            web_rotate(&ui);
        }
    });
    on!(on_web_new_pin, |ui, w| {
        let _ = w;
        web_rotate(&ui);
        toast(&ui, "New passcode. Phones already signed in stay signed in.");
    });
    on!(on_web_open_here, |ui, w| {
        let _ = w;
        let port = WEB.with(|r| r.borrow().port);
        if port > 0 {
            let _ = open::that_detached(format!("http://localhost:{port}"));
        } else {
            toast(&ui, "Turn on phone access first.");
        }
    });

    // Rotate the passcode once a day.
    let weak = ui.as_weak();
    let timer = Box::leak(Box::new(slint::Timer::default()));
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(600), move || {
        let due = WEB.with(|r| r.borrow().pin_set.is_some_and(|t| t.elapsed() >= PIN_LIFETIME));
        if let (true, Some(ui)) = (due, weak.upgrade()) {
            web_rotate(&ui);
        }
    });

    // Pick up where the user left it.
    if saved["enabled"] == json!(true) {
        web_start(ui, w, saved["remote"] == json!(true));
    }
}

thread_local! {
    static PENDING_UPDATE: std::cell::RefCell<Option<crate::update::Available>> = const { std::cell::RefCell::new(None) };
}

/// Once a day (if allowed in Settings), ask the catalog whether a newer version exists.
pub fn start_updates(ui: &AppWindow, w: &Wiring) {
    let app = ui.global::<App>();
    app.set_app_version(crate::update::VERSION.into());
    let db = db::lock(&w.store);
    let enabled = db.setting("update_check").ok().and_then(|v| v.as_bool()).unwrap_or(true);
    let last = db.setting("update_checked_at").ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    drop(db);
    app.set_update_check(enabled);
    let due = chrono::DateTime::parse_from_rfc3339(&last).map(|t| chrono::Utc::now().signed_duration_since(t).num_hours() >= 24).unwrap_or(true);
    if enabled && due {
        check_updates(ui, w, false);
    }
}

fn check_updates(ui: &AppWindow, w: &Wiring, by_hand: bool) {
    let (store, weak) = (w.store.clone(), ui.as_weak());
    if by_hand {
        ui.global::<App>().set_update_status("Checking…".into());
    }
    w.rt.spawn(async move {
        let r = crate::update::check().await;
        let _ = db::lock(&store).set_setting("update_checked_at", &json!(chrono::Utc::now().to_rfc3339()));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let app = ui.global::<App>();
            match r {
                Ok(Some(a)) => {
                    app.set_update_version(a.version.clone().into());
                    app.set_update_notes(a.notes.clone().into());
                    app.set_update_status(SharedString::new());
                    toast(&ui, format!("Version {} is available. Install it from Today or Settings.", a.version));
                    PENDING_UPDATE.with(|p| *p.borrow_mut() = Some(a));
                }
                Ok(None) => { if by_hand { app.set_update_status(format!("You have the latest version ({}).", crate::update::VERSION).into()); } }
                Err(e) => { if by_hand { app.set_update_status(format!("Couldn't check: {e:#}").into()); } }
            }
        });
    });
}
