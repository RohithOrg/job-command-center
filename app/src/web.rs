//! The web companion: the desktop app serves a phone-friendly page on the home network,
//! so applications, resumes and the Q&A can be used from a phone or another computer.
//!
//! Off by default (Settings > Phone and web). Access needs the passcode shown in the app;
//! a correct passcode gives an HttpOnly, SameSite=Strict session cookie, every POST must also
//! carry `X-JCC: 1`, and repeated wrong tries lock sign-in for a minute. Profile, settings and API
//! keys stay on the desktop: nothing secret is ever sent to the page. API: web/API.md.
use crate::db::{self, Job, Store};
use crate::pipeline::{self, Ctx, Start};
use crate::{chat, rag, scanner, ui};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PORT: u16 = 8765;
const PAGE: usize = 25;
const MAX_FAILS: u32 = 8;

const INDEX: &str = include_str!("../web/index.html");
const FONT_DISPLAY: &[u8] = include_bytes!("../ui/fonts/BricolageGrotesque.ttf");
const FONT_BODY: &[u8] = include_bytes!("../ui/fonts/AtkinsonHyperlegibleNext.ttf");
const ICON: &[u8] = include_bytes!("../assets/icon-256.png");
const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");

/// Lets phones install the page as an app (home-screen icon, own window). Installing needs HTTPS,
/// which the ngrok address provides; on plain home Wi-Fi "Add to Home Screen" still works.
const MANIFEST: &str = r##"{
  "name": "Job Command Center",
  "short_name": "Jobs",
  "start_url": "/",
  "display": "standalone",
  "background_color": "#0F3A3E",
  "theme_color": "#0F3A3E",
  "icons": [
    { "src": "/icon.png", "sizes": "256x256", "type": "image/png" },
    { "src": "/icon-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any maskable" }
  ]
}"##;

/// Caches only the page shell and fonts so the app opens instantly; data always comes live from the computer.
const SERVICE_WORKER: &str = r#"const SHELL = 'jcc-shell-v1';
const FILES = ['/', '/fonts/BricolageGrotesque.ttf', '/fonts/AtkinsonHyperlegibleNext.ttf', '/icon.png'];
self.addEventListener('install', e => e.waitUntil(caches.open(SHELL).then(c => c.addAll(FILES)).then(() => self.skipWaiting())));
self.addEventListener('activate', e => e.waitUntil(self.clients.claim()));
self.addEventListener('fetch', e => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET' || url.pathname.startsWith('/api/')) return; // never cache data
  e.respondWith(fetch(e.request).then(r => { const copy = r.clone(); caches.open(SHELL).then(c => c.put(e.request, copy)); return r; })
    .catch(() => caches.match(e.request)));
});
"#;

pub struct Web {
    store: Store,
    ctx: Ctx,
    /// The passcode sign-in accepts right now (rotated by the desktop app).
    pin: Mutex<String>,
    sessions: Mutex<HashSet<String>>,
    /// Signs PDF links so they open without the session cookie (see `pdf_key`).
    secret: [u8; 32],
    /// (wrong tries, locked until)
    fails: Mutex<(u32, Option<Instant>)>,
}

pub type Shared = Arc<Web>;

impl Web {
    /// A key for one application's PDFs. Apps added to a phone's home screen open PDFs in the
    /// browser, which doesn't have the app's sign-in cookie; the key lets that one link through.
    /// Keys change whenever the server restarts.
    fn pdf_key(&self, job: i64) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.secret);
        h.update(job.to_le_bytes());
        h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect()
    }

    /// Change the passcode. Devices already signed in stay signed in.
    pub fn set_pin(&self, pin: &str) {
        *self.pin.lock().unwrap() = pin.to_string();
        *self.fails.lock().unwrap() = (0, None);
    }
}

/// Errors as `{ "error": "..." }` (plus optional extra fields) with a status code.
struct ApiError(StatusCode, String, Value);

impl ApiError {
    fn new(code: StatusCode, msg: impl Into<String>) -> Self {
        ApiError(code, msg.into(), Value::Null)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.1 });
        if let (Some(o), Some(extra)) = (body.as_object_mut(), self.2.as_object()) {
            o.extend(extra.clone());
        }
        (self.0, Json(body)).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

// ---------- starting and stopping ----------

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let _ = getrandom::fill(&mut b);
    b
}

/// A new random 6-digit PIN (for the home network).
pub fn new_pin() -> String {
    format!("{:06}", u32::from_le_bytes(random_bytes::<4>()) % 1_000_000)
}

fn token() -> String {
    random_bytes::<32>().iter().map(|x| format!("{x:02x}")).collect()
}

/// This computer's address on the local network (no packets are sent: a UDP "connect" only picks a route).
pub fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.168.0.1:80").or_else(|_| s.connect("10.0.0.1:80")).ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback())
}

/// Start serving on every interface (home network + this computer). Returns the task to abort on stop.
pub async fn start(store: Store, ctx: Ctx, pin: String) -> anyhow::Result<(Shared, tokio::task::JoinHandle<()>, u16)> {
    let state: Shared = Arc::new(Web { store, ctx, pin: Mutex::new(pin), sessions: Mutex::new(HashSet::new()), secret: random_bytes::<32>(), fails: Mutex::new((0, None)) });
    let app = router(state.clone());
    let mut last = None;
    for port in PORT..PORT + 10 {
        match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
            Ok(listener) => {
                let task = tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                });
                return Ok((state, task, port));
            }
            Err(e) => last = Some(e),
        }
    }
    Err(anyhow::anyhow!("no free port near {PORT}: {}", last.map(|e| e.to_string()).unwrap_or_default()))
}

fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], INDEX) }))
        .route("/fonts/BricolageGrotesque.ttf", get(|| async { ([(header::CONTENT_TYPE, "font/ttf")], FONT_DISPLAY) }))
        .route("/fonts/AtkinsonHyperlegibleNext.ttf", get(|| async { ([(header::CONTENT_TYPE, "font/ttf")], FONT_BODY) }))
        .route("/icon.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], ICON) }))
        .route("/icon-512.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], ICON_512) }))
        .route("/manifest.webmanifest", get(|| async { ([(header::CONTENT_TYPE, "application/manifest+json")], MANIFEST) }))
        .route("/sw.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "no-cache")], SERVICE_WORKER) }))
        .route("/api/settings", get(settings))
        .route("/api/settings/search", post(save_search_settings))
        .route("/api/scan", post(scan_now))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/today", get(today))
        .route("/api/jobs", get(jobs).post(add_job))
        .route("/api/jobs/close", post(close_many))
        .route("/api/jobs/{id}", get(job))
        .route("/api/jobs/{id}/pdf", get(pdf))
        .route("/api/jobs/{id}/close", post(close_one))
        .route("/api/jobs/{id}/reopen", post(reopen))
        .route("/api/jobs/{id}/status", post(set_status))
        .route("/api/jobs/{id}/progress", post(progress))
        .route("/api/jobs/{id}/tailor", post(tailor))
        .route("/api/jobs/{id}/revise", post(revise))
        .route("/api/jobs/{id}/ask", post(ask))
        .route("/api/chat/{id}/save", post(save_answer))
        .route("/api/profile", get(profile).post(save_profile))
        .route("/api/suggest", get(suggest))
        .route("/api/discover", get(discover))
        .route("/api/discover/{id}/tailor", post(discover_tailor))
        .route("/api/discover/{id}/dismiss", post(discover_dismiss))
        .layer(axum::middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

// ---------- auth ----------

fn cookie(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix("jcc_session=").map(String::from))
}

/// Every /api call except login needs a session; every POST needs the X-JCC header (blocks
/// cross-site form posts, which can't set custom headers).
async fn guard(State(web): State<Shared>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    if !path.starts_with("/api/") {
        return next.run(req).await;
    }
    if req.method() == Method::POST && req.headers().get("x-jcc").and_then(|v| v.to_str().ok()) != Some("1") {
        return ApiError::new(StatusCode::FORBIDDEN, "Missing request header").into_response();
    }
    if path != "/api/login" && !pdf_link_ok(&web, &path, req.uri().query().unwrap_or("")) {
        let ok = cookie(req.headers()).is_some_and(|t| web.sessions.lock().unwrap().contains(&t));
        if !ok {
            return ApiError::new(StatusCode::UNAUTHORIZED, "Sign in with the passcode shown in the desktop app").into_response();
        }
    }
    next.run(req).await
}

/// `/api/jobs/{id}/pdf?...&key=K` with the right key for that application.
fn pdf_link_ok(web: &Web, path: &str, query: &str) -> bool {
    let Some(id) = path.strip_prefix("/api/jobs/").and_then(|p| p.strip_suffix("/pdf")).and_then(|id| id.parse::<i64>().ok()) else { return false };
    let want = web.pdf_key(id);
    query.split('&').filter_map(|kv| kv.strip_prefix("key=")).any(|k| k.len() == want.len()
        && k.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0)
}

#[derive(Deserialize)]
struct Login {
    pin: String,
}

async fn login(State(web): State<Shared>, Json(body): Json<Login>) -> Response {
    {
        let mut f = web.fails.lock().unwrap();
        if let Some(until) = f.1 {
            if Instant::now() < until {
                return ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Too many tries. Wait a minute and try again.").into_response();
            }
            *f = (0, None);
        }
    }
    // Constant-time compare: no timing hints about how much of the passcode matched.
    let given = body.pin.trim().as_bytes();
    let current = web.pin.lock().unwrap().clone();
    let want = current.as_bytes();
    let same = given.len() == want.len() && given.iter().zip(want).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    if !same {
        let mut f = web.fails.lock().unwrap();
        f.0 += 1;
        if f.0 >= MAX_FAILS {
            f.1 = Some(Instant::now() + Duration::from_secs(60));
        }
        return ApiError::new(StatusCode::UNAUTHORIZED, "That passcode isn't right. It's shown in the desktop app under Settings → Phone and web.").into_response();
    }
    *web.fails.lock().unwrap() = (0, None);
    let t = token();
    web.sessions.lock().unwrap().insert(t.clone());
    let cookie = format!("jcc_session={t}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000");
    ([(header::SET_COOKIE, cookie)], Json(json!({}))).into_response()
}

async fn logout(State(web): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie(&headers) {
        web.sessions.lock().unwrap().remove(&t);
    }
    ([(header::SET_COOKIE, "jcc_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0".to_string())], Json(json!({}))).into_response()
}

// ---------- shapes ----------

fn job_row(j: &Job, reason: &str, board: &str) -> Value {
    let (position, ended) = ui::track_position(j);
    let (initial, hue) = ui::monogram(&j.company);
    json!({
        "id": j.id, "title": ui::title_or(j), "company": j.company, "location": j.location, "status": j.status,
        "fit": j.score.map(|s| format!("{s:.1}")).unwrap_or_default(),
        "ats": j.ats_score.map(|a| a.to_string()).unwrap_or_default(),
        "position": position, "ended": ended, "working": ui::working(j), "stage_label": ui::stage_label(&j.stage),
        "when": if board == "Boarding" && j.next_step_date.is_empty() { "Now".to_string() } else { ui::when_text(j) },
        "board": board, "reason": reason, "updated": ui::short_date(&j.created_at), "initial": initial, "hue": hue,
    })
}

fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

const CLOSE_REASONS: [&str; 6] = ["I'm not interested", "The posting closed", "They turned me down", "I withdrew", "I took another offer", "Other"];

fn job_detail(store: &Store, id: i64) -> anyhow::Result<Value> {
    let db = db::lock(store);
    let j = db.job(id)?;
    let mut d = job_row(&j, "", "");
    let (ev, jd) = (&j.evaluation, &j.jd);
    let strengths: Vec<String> = ev["strengths"].as_array().into_iter().flatten()
        .map(|x| if x.is_object() { format!("{}: {}", s(&x["requirement"]), s(&x["evidence"])) } else { s(x) }).collect();
    let gaps: Vec<String> = ev["gaps"].as_array().into_iter().flatten()
        .map(|x| if x.is_object() { format!("{} ({})", s(&x["requirement"]), s(&x["severity"])) } else { s(x) }).collect();
    let versions = db.versions(id)?;
    let total = versions.len();
    let report = j.current_version.and_then(|c| versions.iter().find(|v| v.id == c)).map(|v| {
        let a = &v.ats;
        let l = |k: &str| crate::resume::strs(&a[k]);
        let unsupported = [l("unsupported_keywords"), l("unsupported_numbers")].concat();
        json!({ "score": v.ats_score, "coverage": a["coverage"], "structure": a["structure"], "pages": a["pages"],
                "matched": l("matched"), "missing": l("missing"), "gaps": l("missing_gaps"), "unsupported": unsupported })
    });
    let msg = |m: &db::ChatMsg| json!({ "id": m.id, "role": m.role, "content": m.content,
        "meta": if m.model.is_empty() { ui::clock(&m.ts) } else { format!("{}, {}", ui::clock(&m.ts), m.model) }, "saved": m.saved });
    let o = d.as_object_mut().unwrap();
    for (k, v) in [
        ("url", json!(j.url)), ("recommendation", ev["recommendation"].clone()), ("fit_summary", ev["summary"].clone()),
        ("archetype", ev["archetype"].clone()), ("work_auth", ev["work_auth"].clone()), ("strengths", json!(strengths)),
        ("gaps", json!(gaps)), ("red_flags", json!(crate::resume::strs(&jd["red_flags"]))), ("job_summary", jd["summary"].clone()),
        ("salary", jd["salary"].clone()), ("remote", jd["remote_policy"].clone()), ("seniority", jd["seniority"].clone()),
        ("next_step", json!(j.next_step)), ("next_step_date", json!(j.next_step_date)),
        ("applied", json!(j.applied_at.get(..10).unwrap_or(""))), ("notes", json!(j.notes)), ("cover_letter", json!(j.cover_letter)),
        ("error", json!(j.error)),
        ("duplicate_note", json!(jd["duplicate_of"].as_i64().map(|d| format!("You already track this role as application #{d}.")).unwrap_or_default())),
        ("versions", json!(versions.iter().enumerate().map(|(i, v)| json!({
            "id": v.id, "label": format!("Version {}", total - i), "kind": ui::version_kind(&v.kind), "note": v.note,
            "ats": v.ats_score, "template_name": ui::template_name(&v.template), "created": ui::short_date(&v.created_at),
            "current": Some(v.id) == j.current_version })).collect::<Vec<_>>())),
        ("current_version", json!(j.current_version)),
        ("ats_report", report.unwrap_or(Value::Null)),
        ("chat", json!(db.chat(id)?.iter().map(msg).collect::<Vec<_>>())),
        ("events", json!(db.events(id)?.iter().rev().map(|e| json!({ "time": ui::short_date(&e.ts),
            "text": if e.text.is_empty() { e.status.clone() } else { e.text.clone() } })).collect::<Vec<_>>())),
        ("work", json!(db.logs(id)?.iter().map(|l| json!({ "time": ui::clock(&l.ts), "agent": l.agent, "level": l.level, "message": l.message }))
            .collect::<Vec<_>>())),
        ("statuses", json!(db::STATUSES)),
        ("close_reasons", json!(CLOSE_REASONS)),
    ] {
        o.insert(k.into(), v);
    }
    Ok(d)
}

fn paged<T>(all: Vec<T>, page: usize) -> (Vec<T>, usize, usize, usize) {
    let total = all.len();
    let pages = total.div_ceil(PAGE).max(1);
    let page = page.min(pages - 1);
    (all.into_iter().skip(page * PAGE).take(PAGE).collect(), page, pages, total)
}

/// Agents need a connected AI; say so plainly instead of failing halfway.
fn need_ai(web: &Web) -> Result<(), ApiError> {
    if rag::saved_ai(&web.store).is_none() {
        return Err(ApiError::new(StatusCode::CONFLICT, "Your AI isn't connected. Open Job Command Center on your computer and connect it in Settings."));
    }
    Ok(())
}

// ---------- reads ----------

async fn me(State(web): State<Shared>) -> ApiResult {
    let (name, working) = {
        let db = db::lock(&web.store);
        (db.active_profile()?.full_name(), db.jobs()?.iter().filter(|j| ui::working(j)).count())
    };
    let working = working + scanner::is_running() as usize;
    Ok(Json(json!({ "name": name.split_whitespace().next().unwrap_or(""), "working": working,
        "ai_connected": rag::saved_ai(&web.store).is_some(), "close_reasons": CLOSE_REASONS })))
}

async fn today(State(web): State<Shared>) -> ApiResult {
    let all = db::lock(&web.store).jobs()?;
    let items: Vec<Value> = ui::queue(&all).into_iter().map(|(j, reason, board)| job_row(j, &reason, board)).collect();
    Ok(Json(json!({ "date": chrono::Local::now().format("%A %-d %B").to_string(), "items": items })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListQuery {
    status: String,
    q: String,
    page: usize,
    country: String,
    version: Option<i64>,
}

async fn jobs(State(web): State<Shared>, Query(q): Query<ListQuery>) -> ApiResult {
    let all = db::lock(&web.store).jobs()?;
    let rows: Vec<Value> = ui::filtered_jobs(&all, &q.q, if q.status.is_empty() { "Active" } else { &q.status })
        .into_iter().map(|j| job_row(j, "", "")).collect();
    let (items, page, pages, total) = paged(rows, q.page);
    Ok(Json(json!({ "items": items, "page": page, "pages": pages, "total": total })))
}

async fn job(State(web): State<Shared>, Path(id): Path<i64>) -> ApiResult {
    let mut d = job_detail(&web.store, id).map_err(|e| ApiError::new(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    d["pdf_key"] = json!(web.pdf_key(id));
    Ok(Json(d))
}

async fn pdf(State(web): State<Shared>, Path(id): Path<i64>, Query(q): Query<ListQuery>) -> Result<Response, ApiError> {
    let (path, name) = {
        let db = db::lock(&web.store);
        let j = db.job(id)?;
        let vid = q.version.or(j.current_version).ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "No resume yet"))?;
        let v = db.version(vid)?;
        if v.job_id != id {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "No such version"));
        }
        let name = std::path::Path::new(&v.pdf_path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "resume.pdf".into());
        (v.pdf_path, name)
    };
    let bytes = std::fs::read(&path).map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "The PDF file is missing on the computer"))?;
    // ASCII fallback filename plus the UTF-8 one (names contain "–").
    let ascii: String = name.chars().map(|c| if c.is_ascii() && c != '"' { c } else { '-' }).collect();
    let encoded: String = name.bytes().map(|b| if b.is_ascii_alphanumeric() || b".-_".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/pdf")
        .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"))
        .body(Body::from(bytes))
        .unwrap())
}

async fn discover(State(web): State<Shared>, Query(q): Query<ListQuery>) -> ApiResult {
    let all = scanner::discovered(&db::lock(&web.store), None)?;
    let mut countries: Vec<String> = all.iter().map(|d| scanner::country_of(&d.location)).filter(|c| !c.is_empty()).collect();
    countries.sort();
    countries.dedup();
    countries.insert(0, "All countries".into());
    let needle = q.q.to_lowercase();
    let rows: Vec<Value> = all.iter()
        .filter(|d| d.state == "new" && d.job_id.is_none())
        .filter(|d| q.country.is_empty() || q.country == "All countries" || scanner::country_of(&d.location) == q.country)
        .filter(|d| needle.is_empty() || format!("{} {} {}", d.title, d.company, d.location).to_lowercase().contains(&needle))
        .map(|d| {
            let (initial, hue) = ui::monogram(&d.company);
            json!({ "id": d.id, "title": d.title, "company": d.company, "location": d.location, "country": scanner::country_of(&d.location),
                    "url": d.url, "seen": ui::short_date(&d.first_seen), "state": d.state, "job_id": d.job_id, "initial": initial, "hue": hue })
        }).collect();
    let (items, page, pages, total) = paged(rows, q.page);
    Ok(Json(json!({ "items": items, "countries": countries, "page": page, "pages": pages, "total": total })))
}

// ---------- writes ----------

#[derive(Deserialize, Default)]
#[serde(default)]
struct Form {
    url: String,
    text: String,
    force: bool,
    reason: String,
    note: String,
    ids: Vec<i64>,
    status: String,
    next_step: String,
    date: String,
    notes: String,
    instruction: String,
    question: String,
}

fn notify(web: &Web, id: i64) {
    (web.ctx.notify)(id);
}

fn reason(f: &Form) -> &str {
    if f.reason.trim().is_empty() { "Closed" } else { f.reason.trim() }
}

async fn add_job(State(web): State<Shared>, Json(b): Json<Form>) -> ApiResult {
    need_ai(&web)?;
    let url = b.url.trim().to_string();
    if url.is_empty() && b.text.trim().is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Paste the job link or its description"));
    }
    if !url.is_empty() && !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Use the full link, starting with https://"));
    }
    let id = {
        let db = db::lock(&web.store);
        if !url.is_empty() && !b.force {
            let key = scanner::url_key(&url);
            if let Some(j) = db.jobs()?.into_iter().find(|j| !j.url.is_empty() && scanner::url_key(&j.url) == key) {
                let state = if db::is_closed(&j.status) { "it's closed".to_string() } else { format!("it's at {}", j.status) };
                return Err(ApiError(StatusCode::CONFLICT,
                    format!("You've seen this job before: {} at {}, added {}, and {state}.", ui::title_or(&j), j.company, ui::short_date(&j.created_at)),
                    json!({ "duplicate_of": j.id })));
            }
        }
        let pid = db.active_profile().ok().map(|p| p.id);
        db.insert_job(&url, b.text.trim(), "", "", "", pid, "phone")?
    };
    tokio::spawn(pipeline::run(web.ctx.clone(), id, Start::Full));
    notify(&web, id);
    Ok(Json(json!({ "id": id })))
}

async fn close_one(State(web): State<Shared>, Path(id): Path<i64>, Json(b): Json<Form>) -> ApiResult {
    ui::close_job(&web.store, id, reason(&b), &b.note)?;
    notify(&web, id);
    Ok(Json(json!({})))
}

async fn close_many(State(web): State<Shared>, Json(b): Json<Form>) -> ApiResult {
    let closed = b.ids.iter().filter(|id| ui::close_job(&web.store, **id, reason(&b), &b.note).is_ok()).count();
    notify(&web, -1);
    Ok(Json(json!({ "closed": closed })))
}

async fn reopen(State(web): State<Shared>, Path(id): Path<i64>) -> ApiResult {
    {
        let db = db::lock(&web.store);
        let j = db.job(id)?;
        let status = if !j.applied_at.is_empty() { "Applied" } else if j.current_version.is_some() { "Ready" } else if !j.evaluation.is_null() { "Evaluated" } else { "New" };
        db.update_job(id, &[("status", json!(status))])?;
        db.add_event(id, "status", status, "Reopened")?;
    }
    notify(&web, id);
    Ok(Json(json!({})))
}

async fn set_status(State(web): State<Shared>, Path(id): Path<i64>, Json(b): Json<Form>) -> ApiResult {
    if !db::STATUSES.contains(&b.status.as_str()) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Unknown status"));
    }
    {
        let db = db::lock(&web.store);
        let j = db.job(id)?;
        if j.status != b.status {
            let mut fields = vec![("status", json!(b.status))];
            if b.status == "Applied" && j.applied_at.is_empty() {
                fields.push(("applied_at", json!(db::now())));
            }
            db.update_job(id, &fields)?;
            db.add_event(id, "status", &b.status, &format!("Moved to {}", b.status))?;
        }
    }
    notify(&web, id);
    Ok(Json(json!({})))
}

async fn progress(State(web): State<Shared>, Path(id): Path<i64>, Json(b): Json<Form>) -> ApiResult {
    let date = b.date.trim();
    if !date.is_empty() && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Write the date as YYYY-MM-DD"));
    }
    db::lock(&web.store).update_job(id, &[("next_step", json!(b.next_step.trim())), ("next_step_date", json!(date)), ("notes", json!(b.notes))])?;
    notify(&web, id);
    Ok(Json(json!({})))
}

async fn tailor(State(web): State<Shared>, Path(id): Path<i64>) -> ApiResult {
    need_ai(&web)?;
    db::lock(&web.store).update_job(id, &[("stage", json!("queued")), ("error", Value::Null)])?;
    tokio::spawn(pipeline::run(web.ctx.clone(), id, Start::Tailor));
    notify(&web, id);
    Ok(Json(json!({})))
}

async fn revise(State(web): State<Shared>, Path(id): Path<i64>, Json(b): Json<Form>) -> ApiResult {
    need_ai(&web)?;
    if b.instruction.trim().is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Say what to change"));
    }
    db::lock(&web.store).update_job(id, &[("stage", json!("revising"))])?;
    tokio::spawn(pipeline::revise(web.ctx.clone(), id, b.instruction.trim().to_string()));
    notify(&web, id);
    Ok(Json(json!({})))
}

async fn ask(State(web): State<Shared>, Path(id): Path<i64>, Json(b): Json<Form>) -> ApiResult {
    need_ai(&web)?;
    if b.question.trim().is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Type a question"));
    }
    let msg_id = chat::answer(&web.ctx, id, b.question.trim()).await?;
    let (_, m) = db::lock(&web.store).chat_msg(msg_id)?;
    Ok(Json(json!({ "id": m.id, "role": m.role, "content": m.content,
                     "meta": if m.model.is_empty() { ui::clock(&m.ts) } else { format!("{}, {}", ui::clock(&m.ts), m.model) }, "saved": m.saved })))
}

async fn save_answer(State(web): State<Shared>, Path(id): Path<i64>) -> ApiResult {
    let saved = chat::toggle_saved(&web.ctx, id)?;
    let store = web.store.clone();
    tokio::spawn(async move { let _ = rag::embed_builtin(&store).await; });
    Ok(Json(json!({ "saved": saved })))
}

async fn discover_tailor(State(web): State<Shared>, Path(id): Path<i64>) -> ApiResult {
    need_ai(&web)?;
    let (jid, created) = scanner::queue_discovered(&db::lock(&web.store), id)?;
    if created {
        tokio::spawn(pipeline::run(web.ctx.clone(), jid, Start::Full));
    }
    notify(&web, jid);
    Ok(Json(json!({ "job_id": jid })))
}

async fn discover_dismiss(State(web): State<Shared>, Path(id): Path<i64>) -> ApiResult {
    scanner::set_discovered_state(&db::lock(&web.store), id, "dismissed")?;
    Ok(Json(json!({})))
}

// ---------- settings: only what's safe to change from a browser ----------
// Desktop only (never exposed here): AI provider, endpoint and keys (changing them could send the
// CV elsewhere), the passcode and Wi-Fi/ngrok access, the data folder, and deleting anything.

async fn settings(State(web): State<Shared>) -> ApiResult {
    let ai = rag::saved_ai(&web.store);
    let db = db::lock(&web.store);
    let prefs = scanner::prefs(&db);
    let favourites = scanner::companies(&db)?.iter().filter(|c| c.favorite).count();
    Ok(Json(json!({
        "ai": { "connected": ai.is_some(), "provider": ai.as_ref().map(|a| a.label()).unwrap_or(""),
                "model": ai.as_ref().map(|a| a.settings.model().to_string()).unwrap_or_default() },
        "search": { "auto": prefs.auto_process, "limit": prefs.auto_process_limit, "interval": prefs.scan_interval_hours },
        "favourite_companies": favourites,
        "scanning": scanner::is_running(),
        "desktop_only": ["AI service and keys", "Phone and web access, passcode", "Where your files are kept", "Deleting applications or profiles"],
    })))
}

#[derive(Deserialize)]
struct SearchSettings {
    auto: bool,
    limit: u32,
    interval: f64,
}

async fn save_search_settings(State(web): State<Shared>, Json(b): Json<SearchSettings>) -> ApiResult {
    let db = db::lock(&web.store);
    let mut p = scanner::prefs(&db);
    p.auto_process = b.auto;
    p.auto_process_limit = b.limit.min(50);
    p.scan_interval_hours = b.interval.clamp(0.0, 24.0 * 30.0);
    scanner::save_prefs(&db, &p)?;
    Ok(Json(json!({ "ok": true })))
}

async fn scan_now(State(web): State<Shared>) -> ApiResult {
    if scanner::is_running() {
        return Err(ApiError::new(StatusCode::CONFLICT, "A scan is already running"));
    }
    let store = web.store.clone();
    let ctx = web.ctx.clone();
    tokio::spawn(async move {
        let summary = scanner::scan(&store, None, |_| {}).await;
        (ctx.notify)(-1);
        let _ = summary;
    });
    Ok(Json(json!({ "started": true })))
}

// ---------- profile (same mapping as the desktop Profile page; never API keys) ----------

fn sv(m: &slint::ModelRc<slint::SharedString>) -> Vec<String> {
    ui::strings_vec(m)
}

fn form_json(f: &crate::ProfileForm) -> Value {
    json!({
        "id": f.id, "name": f.name.as_str(), "full_name": f.full_name.as_str(), "headline": f.headline.as_str(), "email": f.email.as_str(), "phone": f.phone.as_str(),
        "city": f.city.as_str(), "country": f.country.as_str(), "linkedin": f.linkedin.as_str(), "github": f.github.as_str(), "website": f.website.as_str(), "other_link": f.other_link.as_str(),
        "target_roles": sv(&f.target_roles), "seniority": f.seniority.as_str(), "work_mode": f.work_mode.as_str(), "countries": sv(&f.countries),
        "cities": sv(&f.cities), "relocate": f.relocate, "avoid_places": sv(&f.avoid_places), "exclude_words": sv(&f.exclude_words),
        "salary_target": f.salary_target.as_str(), "salary_minimum": f.salary_minimum.as_str(), "currency": f.currency.as_str(),
        "authorized_in": sv(&f.authorized_in), "needs_sponsorship": f.needs_sponsorship, "visa_notes": f.visa_notes.as_str(),
        "notice": f.notice.as_str(), "languages": sv(&f.languages), "story": f.story.as_str(), "strengths": sv(&f.strengths), "avoid": f.avoid.as_str(),
        "tone": f.tone.as_str(), "notes_md": f.notes_md.as_str(), "cv_md": f.cv_md.as_str(),
    })
}

fn form_from(v: &Value, template: &str) -> crate::ProfileForm {
    let t = |k: &str| slint::SharedString::from(v[k].as_str().unwrap_or("").to_string());
    let l = |k: &str| ui::strings(&crate::resume::strs(&v[k]));
    crate::ProfileForm {
        id: v["id"].as_i64().unwrap_or(0) as i32, name: t("name"), full_name: t("full_name"), headline: t("headline"), email: t("email"),
        phone: t("phone"), city: t("city"), country: t("country"), linkedin: t("linkedin"), github: t("github"), website: t("website"),
        other_link: t("other_link"), target_roles: l("target_roles"), seniority: t("seniority"), work_mode: t("work_mode"),
        countries: l("countries"), cities: l("cities"), relocate: v["relocate"].as_bool().unwrap_or(false), avoid_places: l("avoid_places"),
        exclude_words: l("exclude_words"), salary_target: t("salary_target"), salary_minimum: t("salary_minimum"), currency: t("currency"),
        authorized_in: l("authorized_in"), needs_sponsorship: v["needs_sponsorship"].as_bool().unwrap_or(false), visa_notes: t("visa_notes"),
        notice: t("notice"), languages: l("languages"), story: t("story"), strengths: l("strengths"), avoid: t("avoid"), tone: t("tone"),
        notes_md: t("notes_md"), cv_md: t("cv_md"), template: template.into(),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProfileQuery {
    id: Option<i64>,
}

async fn profile(State(web): State<Shared>, Query(q): Query<ProfileQuery>) -> ApiResult {
    let db = db::lock(&web.store);
    let p = match q.id {
        Some(id) => db.profile(id).map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "No such profile"))?,
        None => db.active_profile()?,
    };
    let profiles: Vec<Value> = db.profiles()?.iter().map(|x| json!({ "id": x.id, "name": x.name, "active": x.active })).collect();
    Ok(Json(json!({
        "profiles": profiles,
        "profile": form_json(&ui::profile_form(&p)),
        "options": {
            "countries": crate::options::COUNTRIES.iter().filter(|c| **c != "Remote").collect::<Vec<_>>(),
            "currencies": ["EUR", "USD", "GBP", "CHF", "INR", "CAD", "AUD", "SEK", "DKK", "NOK", "PLN", "SGD", "AED", "JPY"],
            "seniority": ["Any level", "Entry level", "Mid level", "Senior", "Staff / Principal", "Lead / Manager", "Director and above"],
            "work_modes": ["Any", "Remote", "Hybrid", "On-site"],
            "notice": ["Can start now", "1 week", "2 weeks", "1 month", "2 months", "3 months", "More than 3 months"],
            "tones": ["Professional", "Warm", "Direct", "Enthusiastic"],
        },
    })))
}

async fn save_profile(State(web): State<Shared>, Json(v): Json<Value>) -> ApiResult {
    {
        let db = db::lock(&web.store);
        let id = v["id"].as_i64().ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Missing profile id"))?;
        let mut p = db.profile(id)?;
        let form = form_from(&v, &p.template.clone());
        ui::apply_form(&mut p, &form);
        db.save_profile(&p)?;
        rag::sync_profile(&db, &p)?;
        if p.active {
            ui::sync_search(&db, &p)?;
        }
    }
    let store = web.store.clone();
    tokio::spawn(async move { let _ = rag::embed_builtin(&store).await; });
    notify(&web, -1);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SuggestQuery {
    field: String,
    q: String,
}

async fn suggest(Query(q): Query<SuggestQuery>) -> ApiResult {
    let mut items = crate::options::local(&q.field, &q.q, &[]);
    if crate::options::has_remote(&q.field) && q.q.trim().chars().count() >= 2 {
        for f in crate::options::remote(&q.field, &q.q).await {
            if !items.iter().any(|x| x.eq_ignore_ascii_case(&f)) {
                items.push(f);
            }
        }
    }
    items.truncate(10);
    Ok(Json(json!({ "items": items })))
}

/// A passcode for reaching the app from anywhere: 12 characters in three easy-to-type groups
/// (about 60 bits, against 20 for the home-network PIN), e.g. "k7mq-x4tn-9vbr".
pub fn new_passphrase() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789"; // no 0/o, 1/l/i
    let bytes = random_bytes::<12>();
    let chars: Vec<char> = bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    chars.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

/// Open an ngrok tunnel to the local server so the page is reachable from anywhere (in-process:
/// no ngrok program needed, only the user's ngrok authtoken). With `google_email`, ngrok itself
/// asks visitors to sign in with that Google account before they reach the app.
/// Returns the public address and the task that keeps the tunnel open (abort it to close).
pub async fn start_remote(authtoken: &str, port: u16, google_email: Option<String>) -> anyhow::Result<(String, tokio::task::JoinHandle<()>)> {
    use ngrok::config::ForwarderBuilder;
    use ngrok::tunnel::EndpointInfo;
    let session = ngrok::Session::builder().authtoken(authtoken).connect().await
        .map_err(|e| anyhow::anyhow!("couldn't sign in to ngrok (check the token): {e}"))?;
    let mut endpoint = session.http_endpoint();
    if let Some(email) = google_email.filter(|e| !e.trim().is_empty()) {
        let mut oauth = ngrok::config::OauthOptions::new("google");
        oauth.allow_email(email.trim());
        endpoint.oauth(oauth);
    }
    let local = url::Url::parse(&format!("http://localhost:{port}"))?;
    let mut forwarder = endpoint.listen_and_forward(local).await.map_err(|e| anyhow::anyhow!("ngrok refused the tunnel: {e}"))?;
    let url = forwarder.url().to_string();
    let task = tokio::spawn(async move {
        let _session = session; // keep the session alive as long as the tunnel
        let _ = forwarder.join().await;
    });
    Ok((url, task))
}

/// QR code for an address, as RGBA pixels (`scale` px per module, with a quiet zone).
pub fn qr_rgba(text: &str, scale: u32) -> Option<(Vec<u8>, u32)> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let n = code.width() as u32;
    let quiet = 4;
    let size = (n + 2 * quiet) * scale;
    let colors = code.to_colors();
    let mut px = vec![255u8; (size * size * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            if colors[(y * n + x) as usize] == qrcode::Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let (xx, yy) = ((x + quiet) * scale + dx, (y + quiet) * scale + dy);
                        let i = ((yy * size + xx) * 4) as usize;
                        px[i..i + 3].copy_from_slice(&[15, 42, 46]);
                    }
                }
            }
        }
    }
    Some((px, size))
}
