//! Career-site scanner (port of the server's scanner.py).
//!
//! For every company on the watchlist: list its open postings through the ATS's public
//! JSON API (Greenhouse, Ashby, Lever, Workable, SmartRecruiters, Recruitee, Workday,
//! Personio, Teamtailor, amazon.jobs, Jibe, Eightfold, Alliander) or, for branded career
//! pages, by fetching the page and reading the job board it links to, its JSON-LD
//! postings, or its job links. Postings are kept only if the title matches the target
//! roles and the location matches the preferred places. Anything whose URL was ever seen
//! before is skipped, and a role already tracked for the same company is flagged as a
//! duplicate instead of being worked on twice.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use rusqlite::{params, OptionalExtension};
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::db::{self, Db, Store};
use crate::fetch;

const CATALOG: &str = include_str!("../catalog.json");
const DISCOVERED_LIMIT: usize = 2000;

// ---------- preferences ----------

/// The "search_prefs" setting. Keys match the server's JSON; unknown keys are kept.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchPrefs {
    /// Target roles: a title must contain one of these as a whole word (plural allowed).
    pub title_positive: Vec<String>,
    /// Excluded words: word prefixes ("Intern" catches "Internship"); a trailing space
    /// ("Java ") makes the word whole-word only.
    pub title_negative: Vec<String>,
    pub locations_allow: Vec<String>,
    pub locations_block: Vec<String>,
    pub auto_process: bool,
    #[serde(deserialize_with = "lenient_u32")]
    pub auto_process_limit: u32,
    pub scan_interval_hours: f64,
    /// Scans (with no explicit company list), the inbox and lists only cover favourites.
    pub favorites_only: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Default for SearchPrefs {
    fn default() -> Self {
        let words = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect();
        Self {
            title_positive: words(&["Data Scientist", "Machine Learning", "ML Engineer", "AI Engineer", "LLM", "GenAI"]),
            title_negative: words(&["Intern", "Junior", "Werkstudent"]),
            locations_allow: vec![],
            locations_block: vec![],
            auto_process: false,
            auto_process_limit: 10,
            scan_interval_hours: 0.0,
            favorites_only: true,
            extra: Default::default(),
        }
    }
}

fn lenient_u32<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u32, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).map(|n| n.clamp(1.0, 100.0) as u32).unwrap_or(10))
}

pub fn prefs(db: &Db) -> SearchPrefs {
    match db.setting("search_prefs") {
        Ok(v @ Value::Object(_)) => serde_json::from_value(v).unwrap_or_default(),
        _ => SearchPrefs::default(),
    }
}

/// Save preferences (cleaned like the server's PUT /preferences) and re-apply them to the inbox.
pub fn save_prefs(db: &Db, p: &SearchPrefs) -> Result<()> {
    let clean = |xs: &[String]| xs.iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
    let mut p = p.clone();
    p.title_positive = clean(&p.title_positive);
    p.locations_allow = clean(&p.locations_allow);
    p.locations_block = clean(&p.locations_block);
    // A trailing space on an excluded word means "whole word only" (whole-word convention).
    p.title_negative = p.title_negative.iter().filter(|x| !x.trim().is_empty()).map(|x| x.trim_start().to_string()).collect();
    p.auto_process_limit = p.auto_process_limit.clamp(1, 100);
    p.scan_interval_hours = p.scan_interval_hours.clamp(0.0, 168.0);
    db.set_setting("search_prefs", &serde_json::to_value(&p)?)?;
    refilter_inbox(db)?;
    Ok(())
}

// ---------- favourites ----------

/// "Uber (Amsterdam)" and "Uber" are the same company for favourite matching.
pub fn company_norm(name: &str) -> String {
    static PAREN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(.*?\)").unwrap());
    norm_words(&PAREN.replace_all(&name.to_lowercase(), ""))
}

/// Lowercase, every run of non-[a-z0-9] becomes one space, trimmed.
fn norm_words(s: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for ch in s.to_lowercase().chars() {
        if is_word_char(ch) {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(ch);
        } else {
            gap = true;
        }
    }
    out
}

/// Normalized favourite company names, or None when the favourites filter is off.
pub fn favorite_names(db: &Db) -> Result<Option<HashSet<String>>> {
    if !prefs(db).favorites_only {
        return Ok(None);
    }
    Ok(Some(companies(db)?.into_iter().filter(|c| c.favorite).map(|c| company_norm(&c.name)).collect()))
}

pub fn in_favorites(name: &str, favs: Option<&HashSet<String>>) -> bool {
    let Some(favs) = favs else { return true };
    let n = company_norm(name);
    !n.is_empty() && favs.iter().any(|f| n == *f || n.starts_with(&format!("{f} ")) || f.starts_with(&format!("{n} ")))
}

// ---------- filters (title and location filter semantics) ----------

/// Python's `[a-z0-9]` boundary class (ASCII only, applied to lowercased text).
fn is_word_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

/// `(?<![a-z0-9])kw` + tail check, without lookaround. `tail` gets the text after the match.
fn find_word(text: &str, kw: &str, tail: impl Fn(&str) -> bool) -> bool {
    let kw = kw.trim().to_lowercase();
    if kw.is_empty() {
        return false;
    }
    let text = text.to_lowercase();
    let mut from = 0;
    while let Some(i) = text[from..].find(&kw) {
        let at = from + i;
        let before_ok = text[..at].chars().next_back().is_none_or(|c| !is_word_char(c));
        if before_ok && tail(&text[at + kw.len()..]) {
            return true;
        }
        from = at + text[at..].chars().next().map_or(1, |c| c.len_utf8());
    }
    false
}

fn not_word_start(rest: &str) -> bool {
    rest.chars().next().is_none_or(|c| !is_word_char(c))
}

/// Whole-word match of `kw` in `text` (case-insensitive).
pub fn has_word(text: &str, kw: &str) -> bool {
    find_word(text, kw, not_word_start)
}

/// Whole word (plural allowed). `prefix` also matches longer words ("Intern" -> "Internship").
pub fn title_has(title: &str, kw: &str, prefix: bool) -> bool {
    find_word(title, kw, |rest| prefix || not_word_start(rest) || rest.strip_prefix('s').is_some_and(not_word_start))
}

/// Why a title is rejected, or None if it passes.
pub fn title_reason(title: &str, p: &SearchPrefs) -> Option<String> {
    // Include words: whole words ("RAG" must not match "Storage"). Exclude words: word prefixes
    // ("Intern" catches "Internship"), unless written with a trailing space ("Java ", "SAP "),
    // which stay whole-word so "JavaScript" / "SAPIENT" pass.
    if let Some(neg) = p.title_negative.iter().find(|n| !n.trim().is_empty() && title_has(title, n, !n.ends_with(' '))) {
        return Some(format!("Title contains excluded word \"{}\"", neg.trim()));
    }
    let pos: Vec<&String> = p.title_positive.iter().filter(|x| !x.trim().is_empty()).collect();
    if !p.title_positive.is_empty() && !pos.iter().any(|k| title_has(title, k, false)) {
        return Some("Title doesn't match any of your target roles".into());
    }
    None
}

/// Why a location is rejected, or None. Empty location passes. Block wins, then allow must
/// match (strict) — for scraped career pages (strict = false) unknown places pass.
pub fn location_reason(location: &str, p: &SearchPrefs, strict: bool) -> Option<String> {
    if location.trim().is_empty() {
        return None;
    }
    if let Some(b) = p.locations_block.iter().find(|b| !b.trim().is_empty() && has_word(location, b)) {
        return Some(format!("Location \"{location}\" is excluded ({b})"));
    }
    let allow: Vec<&String> = p.locations_allow.iter().filter(|a| !a.trim().is_empty()).collect();
    if allow.is_empty() || allow.iter().any(|a| has_word(location, a)) || !strict {
        return None;
    }
    Some(format!("Location \"{location}\" is outside your preferred places"))
}

pub fn title_ok(title: &str, p: &SearchPrefs) -> bool {
    title_reason(title, p).is_none()
}

pub fn location_ok(location: &str, p: &SearchPrefs, strict: bool) -> bool {
    location_reason(location, p, strict).is_none()
}

// ---------- dedupe keys ----------

static TRACKING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(utm_|ref$|source$|src$|gh_src$|lever-source|trk)").unwrap());

/// Python's `urllib.parse.quote_plus`.
fn quote_plus(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Normalize a posting URL so the same job is recognised across scans (same keys as the server).
pub fn url_key(url: &str) -> String {
    let raw = url.trim();
    let Ok(u) = Url::parse(raw) else {
        return raw.trim_end_matches('/').to_lowercase();
    };
    let mut pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, v)| !v.is_empty() && !TRACKING.is_match(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();
    let query = pairs.iter().map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v))).collect::<Vec<_>>().join("&");
    let host = u.host_str().unwrap_or("").to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = u.path().trim_end_matches('/').to_lowercase();
    if query.is_empty() { format!("{host}{path}") } else { format!("{host}{path}?{query}") }
}

pub fn role_key(company: &str, title: &str) -> String {
    norm_words(&format!("{company}|{title}"))
}

// ---------- companies ----------

#[derive(Clone, Debug)]
pub struct Company {
    pub id: i64,
    pub name: String,
    pub careers_url: String,
    /// Optional job-board API; "page" means: read the careers page only.
    pub api_url: String,
    pub notes: String,
    pub enabled: bool,
    pub favorite: bool,
    pub last_scan_at: String,
    pub last_status: String,
    pub last_found: Option<i64>,
    /// `[{title, location, url, verdict: new|duplicate|seen|rejected, matched: bool, reason}]`
    pub last_postings: Value,
}

const COMPANY_COLS: &str =
    "id, name, careers_url, api_url, notes, enabled, favorite, last_scan_at, last_status, last_found, last_postings";

fn company_row(r: &rusqlite::Row) -> rusqlite::Result<Company> {
    let s = |i: usize| r.get::<_, Option<String>>(i).map(|v| v.unwrap_or_default());
    let postings = s(10)?;
    Ok(Company {
        id: r.get(0)?,
        name: s(1)?,
        careers_url: s(2)?,
        api_url: s(3)?,
        notes: s(4)?,
        enabled: r.get::<_, Option<i64>>(5)?.unwrap_or(1) != 0,
        favorite: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
        last_scan_at: s(7)?,
        last_status: s(8)?,
        last_found: r.get(9)?,
        last_postings: serde_json::from_str(&postings).unwrap_or_else(|_| json!([])),
    })
}

pub fn companies(db: &Db) -> Result<Vec<Company>> {
    let mut st = db.conn.prepare(&format!("SELECT {COMPANY_COLS} FROM companies ORDER BY lower(name), id"))?;
    let rows = st.query_map([], company_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Add a company to the watchlist (as a favourite, like the server does for hand-added ones).
/// A Greenhouse embed URL (`…/embed/job_board?for=acme`) is stored as its board API.
#[cfg(test)]
fn company(db: &Db, id: i64) -> Result<Company> {
    db.conn
        .query_row(&format!("SELECT {COMPANY_COLS} FROM companies WHERE id = ?1"), params![id], company_row)
        .optional()?
        .ok_or_else(|| anyhow!("company {id} no longer exists"))
}

pub fn add_company(db: &Db, name: &str, careers_url: &str) -> Result<i64> {
    let name = name.trim();
    let careers_url = careers_url.trim();
    if name.is_empty() {
        bail!("Give the company a name");
    }
    let u = Url::parse(careers_url).map_err(|_| anyhow!("Enter the careers page as a full http(s) URL"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        bail!("Enter the careers page as a full http(s) URL");
    }
    let api_url = greenhouse_slug(careers_url, "").map(|s| format!("https://boards-api.greenhouse.io/v1/boards/{s}/jobs"));
    db.conn.execute(
        "INSERT INTO companies (name, careers_url, api_url, notes, enabled, favorite) VALUES (?1, ?2, ?3, '', 1, 1)",
        params![name, careers_url, api_url],
    )?;
    Ok(db.conn.last_insert_rowid())
}

pub fn set_favorite(db: &Db, id: i64, fav: bool) -> Result<()> {
    db.conn.execute("UPDATE companies SET favorite = ?1 WHERE id = ?2", params![fav as i64, id])?;
    Ok(())
}

pub fn delete_company(db: &Db, id: i64) -> Result<()> {
    db.conn.execute("DELETE FROM companies WHERE id = ?1", params![id])?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub careers_url: String,
    #[serde(default)]
    pub api_url: Option<String>,
    #[serde(default)]
    pub notes: String,
}

/// The suggested companies shipped with the app.
pub fn catalog() -> Vec<CatalogEntry> {
    serde_json::from_str(CATALOG).unwrap_or_default()
}

/// Add catalog companies missing by name (case-insensitive). Idempotent. They are enabled
/// but not favourites. Returns how many were added.
pub fn seed_catalog(db: &Db) -> Result<usize> {
    let mine: HashSet<String> = companies(db)?.into_iter().map(|c| c.name.to_lowercase()).collect();
    let mut added = 0;
    for c in catalog() {
        if mine.contains(&c.name.to_lowercase()) {
            continue;
        }
        db.conn.execute(
            "INSERT INTO companies (name, careers_url, api_url, notes, enabled, favorite) VALUES (?1, ?2, ?3, ?4, 1, 0)",
            params![c.name, c.careers_url, c.api_url.filter(|a| !a.is_empty()), c.notes],
        )?;
        added += 1;
    }
    Ok(added)
}

// ---------- discovered postings ----------

#[derive(Clone, Debug)]
pub struct Discovered {
    pub id: i64,
    pub url: String,
    pub company: String,
    pub title: String,
    pub location: String,
    pub first_seen: String,
    /// new | duplicate | filtered | dismissed | queued | archived
    pub state: String,
    pub reason: String,
    pub job_id: Option<i64>,
}

const DISC_COLS: &str = "id, url, company, title, location, first_seen, state, reason, job_id";

fn discovered_row(r: &rusqlite::Row) -> rusqlite::Result<Discovered> {
    let s = |i: usize| r.get::<_, Option<String>>(i).map(|v| v.unwrap_or_default());
    Ok(Discovered {
        id: r.get(0)?,
        url: s(1)?,
        company: s(2)?,
        title: s(3)?,
        location: s(4)?,
        first_seen: s(5)?,
        state: s(6)?,
        reason: s(7)?,
        job_id: r.get(8)?,
    })
}

fn all_discovered(db: &Db, state: Option<&str>) -> Result<Vec<Discovered>> {
    let mut st = db.conn.prepare(&format!(
        "SELECT {DISC_COLS} FROM discovered WHERE (?1 IS NULL OR state = ?1) ORDER BY first_seen DESC, id DESC"
    ))?;
    let rows = st.query_map(params![state], discovered_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The discovery inbox, newest first (at most 2000). Like the server's list, only postings
/// from favourite companies are shown while `favorites_only` is on. `None` = every state.
pub fn discovered(db: &Db, state: Option<&str>) -> Result<Vec<Discovered>> {
    let favs = favorite_names(db)?;
    Ok(all_discovered(db, state)?
        .into_iter()
        .filter(|d| in_favorites(&d.company, favs.as_ref()))
        .take(DISCOVERED_LIMIT)
        .collect())
}

pub fn set_discovered_state(db: &Db, id: i64, state: &str) -> Result<()> {
    const STATES: [&str; 6] = ["new", "duplicate", "filtered", "dismissed", "queued", "archived"];
    if !STATES.contains(&state) {
        bail!("unknown inbox state {state}");
    }
    db.conn.execute("UPDATE discovered SET state = ?1 WHERE id = ?2", params![state, id])?;
    Ok(())
}

/// Hand a discovered posting to the pipeline: reuse the job with the same URL, else create one
/// (source "scan"), mark the posting queued and return the job id. The caller starts the
/// pipeline run for a newly created job.
pub fn queue_discovered(db: &Db, id: i64) -> Result<(i64, bool)> {
    let d = db
        .conn
        .query_row(&format!("SELECT {DISC_COLS} FROM discovered WHERE id = ?1"), params![id], discovered_row)
        .optional()?
        .ok_or_else(|| anyhow!("posting {id} no longer exists"))?;
    let key = url_key(&d.url);
    let existing = {
        let mut st = db.conn.prepare("SELECT id, url FROM jobs WHERE url IS NOT NULL ORDER BY id DESC")?;
        let rows: Vec<(i64, String)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        rows.into_iter().find(|(_, u)| url_key(u) == key).map(|(id, _)| id)
    };
    let (job_id, created) = match existing {
        Some(j) => (j, false),
        None => {
            let profile = db.active_profile()?.id;
            (db.insert_job(&d.url, "", &d.company, &d.title, &d.location, Some(profile), "scan")?, true)
        }
    };
    db.conn.execute("UPDATE discovered SET state = 'queued', job_id = ?1 WHERE id = ?2", params![job_id, id])?;
    Ok((job_id, created))
}

/// Re-apply title/location preferences to the inbox: non-matching postings move to
/// "filtered" (other locations), and come back to "new" when preferences widen.
/// Returns (moved to filtered, moved to new).
pub fn refilter_inbox(db: &Db) -> Result<(usize, usize)> {
    let p = prefs(db);
    let (mut filtered, mut new) = (0, 0);
    let rows: Vec<Discovered> = all_discovered(db, Some("new"))?.into_iter().chain(all_discovered(db, Some("filtered"))?).collect();
    for d in rows {
        let ok = title_ok(&d.title, &p) && location_ok(&d.location, &p, true);
        let target = if ok { "new" } else { "filtered" };
        if d.state != target {
            db.conn.execute("UPDATE discovered SET state = ?1 WHERE id = ?2", params![target, d.id])?;
            if ok { new += 1 } else { filtered += 1 }
        }
    }
    Ok((filtered, new))
}

// ---------- country detection (Discover country filter) ----------

const COUNTRIES: &[(&str, &[&str])] = &[
    ("Netherlands", &["netherlands", "nederland", "amsterdam", "rotterdam", "utrecht", "the hague", "den haag",
                      "eindhoven", "amstelveen", "delft", "leiden", "groningen", "hilversum", "haarlem", "nijmegen"]),
    ("Germany", &["germany", "deutschland", "berlin", "munich", "münchen", "hamburg", "frankfurt", "cologne", "köln",
                  "stuttgart", "düsseldorf", "dusseldorf", "mannheim", "heidelberg", "karlsruhe", "leipzig", "walldorf"]),
    ("United Kingdom", &["united kingdom", "uk", "england", "scotland", "wales", "london", "edinburgh", "cambridge",
                         "manchester", "oxford", "bristol", "belfast", "glasgow"]),
    ("Ireland", &["ireland", "dublin", "cork"]),
    ("France", &["france", "paris", "lyon", "toulouse", "nice"]),
    ("Spain", &["spain", "madrid", "barcelona", "valencia", "malaga"]),
    ("Portugal", &["portugal", "lisbon", "lisboa", "porto"]),
    ("Italy", &["italy", "milan", "milano", "rome", "turin"]),
    ("Belgium", &["belgium", "brussels", "antwerp", "ghent"]),
    ("Luxembourg", &["luxembourg"]),
    ("Switzerland", &["switzerland", "zurich", "zürich", "geneva", "lausanne", "basel"]),
    ("Austria", &["austria", "vienna", "wien"]),
    ("Poland", &["poland", "warsaw", "krakow", "kraków", "wroclaw", "wrocław", "gdansk"]),
    ("Czech Republic", &["czech", "czechia", "prague", "brno"]),
    ("Denmark", &["denmark", "copenhagen", "aarhus"]),
    ("Sweden", &["sweden", "stockholm", "gothenburg", "malmö"]),
    ("Norway", &["norway", "oslo"]),
    ("Finland", &["finland", "helsinki"]),
    ("Estonia", &["estonia", "tallinn"]),
    ("Greece", &["greece", "athens"]),
    ("Cyprus", &["cyprus", "limassol", "paphos", "nicosia"]),
    ("Serbia", &["serbia", "belgrade"]),
    ("Romania", &["romania", "bucharest", "cluj"]),
    ("Armenia", &["armenia", "yerevan"]),
    ("Israel", &["israel", "tel aviv"]),
    ("United Arab Emirates", &["united arab emirates", "uae", "dubai", "abu dhabi"]),
    ("India", &["india", "bangalore", "bengaluru", "hyderabad", "pune", "mumbai", "delhi", "chennai", "gurgaon",
                "gurugram", "noida"]),
    ("Singapore", &["singapore"]),
    ("Japan", &["japan", "tokyo"]),
    ("Australia", &["australia", "sydney", "melbourne", "brisbane"]),
    ("Canada", &["canada", "toronto", "montreal", "montréal", "vancouver", "ottawa", "ontario", "quebec", "calgary"]),
    ("United States", &["united states", "usa", "u.s.", "new york", "san francisco", "seattle", "boston", "chicago",
                        "austin", "mountain view", "palo alto", "washington", "atlanta", "miami", "new jersey",
                        "los angeles", "denver", "california", "texas", "san jose", "menlo park", "sunnyvale"]),
    ("Brazil", &["brazil", "são paulo", "sao paulo"]),
    ("Mexico", &["mexico", "mexico city"]),
];

static US_STATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r",\s*(CA|NY|WA|MA|TX|IL|CO|GA|FL|NJ|DC|VA|OR|PA|NC|UT|AZ|MN|MI|OH)\b").unwrap());

/// Every country a location mentions (plus "Remote"), in the server's order.
pub fn countries_of(location: &str) -> Vec<String> {
    let mut out: Vec<String> = COUNTRIES
        .iter()
        .filter(|(_, names)| names.iter().any(|n| has_word(location, n)))
        .map(|(c, _)| c.to_string())
        .collect();
    if !out.iter().any(|c| c == "United States") && US_STATE.is_match(location) {
        out.push("United States".into());
    }
    if has_word(location, "remote") {
        out.push("Remote".into());
    }
    out
}

/// Best-effort single country for a location: the first of `countries_of` ("Netherlands"
/// comes first when a multi-city posting includes it), "Remote" only when nothing else
/// matched, "" when unknown.
pub fn country_of(location: &str) -> String {
    countries_of(location).into_iter().next().unwrap_or_default()
}

// ---------- providers: Ok(None) when the company isn't on that ATS ----------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Posting {
    pub title: String,
    pub url: String,
    pub location: String,
}

fn posting(title: &str, url: &str, location: impl Into<String>) -> Posting {
    Posting { title: title.trim().to_string(), url: url.trim().to_string(), location: location.into().trim().to_string() }
}

/// The parts of a company a provider looks at.
#[derive(Clone, Debug, Default)]
struct Board {
    careers_url: String,
    api_url: String,
}

fn host_of(url: &str) -> String {
    Url::parse(url.trim()).ok().and_then(|u| u.host_str().map(|h| h.to_lowercase())).unwrap_or_default()
}

/// First path segment when the host matches `suffix` (exactly or as a subdomain).
fn slug(url: &str, suffix: &str) -> Option<String> {
    let u = Url::parse(url.trim()).ok()?;
    let host = u.host_str()?.to_lowercase();
    if host != suffix && !host.ends_with(&format!(".{suffix}")) {
        return None;
    }
    u.path_segments()?.find(|s| !s.is_empty()).map(|s| s.to_string())
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn join_nonempty<'a>(parts: impl IntoIterator<Item = &'a str>, sep: &str) -> String {
    parts.into_iter().map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(sep)
}

async fn get_json(http: &reqwest::Client, url: &str) -> Result<Value> {
    let r = http.get(url).header("Accept", "application/json, text/plain, */*").send().await?.error_for_status()?;
    Ok(r.json().await?)
}

async fn get_text(http: &reqwest::Client, url: &str) -> Result<String> {
    Ok(http.get(url).send().await?.error_for_status()?.text().await?)
}

static GH_API: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"boards-api(?:\.eu)?\.greenhouse\.io/v1/boards/([^/?#]+)").unwrap());

fn greenhouse_slug(careers_url: &str, api_url: &str) -> Option<String> {
    if let Some(m) = GH_API.captures(api_url).or_else(|| GH_API.captures(careers_url)) {
        return Some(m[1].to_string());
    }
    let first = slug(careers_url, "greenhouse.io")?;
    if first == "embed" {
        // boards.greenhouse.io/embed/job_board?for=acme
        let u = Url::parse(careers_url.trim()).ok()?;
        return u.query_pairs().find(|(k, _)| k == "for").map(|(_, v)| v.into_owned());
    }
    Some(first)
}

async fn greenhouse(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let Some(slug) = greenhouse_slug(&c.careers_url, &c.api_url) else { return Ok(None) };
    let data = get_json(http, &format!("https://boards-api.greenhouse.io/v1/boards/{slug}/jobs")).await?;
    Ok(Some(
        data["jobs"].as_array().into_iter().flatten()
            .map(|j| posting(s(j, "title"), s(j, "absolute_url"), s(&j["location"], "name")))
            .collect(),
    ))
}

static ASHBY_API: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"ashbyhq\.com/posting-api/job-board/([^/?#]+)").unwrap());

async fn ashby(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let from_api = ASHBY_API.captures(&c.api_url).map(|m| m[1].to_string()).or_else(|| slug(&c.api_url, "ashbyhq.com"));
    let Some(slug) = from_api.or_else(|| slug(&c.careers_url, "ashbyhq.com")) else { return Ok(None) };
    let data = get_json(http, &format!("https://api.ashbyhq.com/posting-api/job-board/{slug}")).await?;
    let mut out = vec![];
    for j in data["jobs"].as_array().into_iter().flatten() {
        if j.get("isListed") == Some(&Value::Bool(false)) {
            continue;
        }
        let mut locs = vec![s(j, "location")];
        locs.extend(j["secondaryLocations"].as_array().into_iter().flatten().map(|x| s(x, "location")));
        if j["isRemote"].as_bool() == Some(true) {
            locs.push("Remote");
        }
        out.push(posting(s(j, "title"), s(j, "jobUrl"), join_nonempty(locs, ", ")));
    }
    Ok(Some(out))
}

async fn lever(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let Some(slug) = slug(&c.careers_url, "lever.co") else { return Ok(None) };
    let api_host = if host_of(&c.careers_url).contains(".eu.") { "api.eu.lever.co" } else { "api.lever.co" };
    let data = get_json(http, &format!("https://{api_host}/v0/postings/{slug}?mode=json")).await?;
    Ok(Some(
        data.as_array().into_iter().flatten()
            .map(|j| {
                let cat = &j["categories"];
                let locs: Vec<&str> = match cat["allLocations"].as_array() {
                    Some(a) if !a.is_empty() => a.iter().filter_map(Value::as_str).collect(),
                    _ => vec![s(cat, "location")],
                };
                posting(s(j, "text"), s(j, "hostedUrl"), join_nonempty(locs, ", "))
            })
            .collect(),
    ))
}

async fn workable(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let Some(slug) = slug(&c.careers_url, "workable.com") else { return Ok(None) };
    let data = get_json(http, &format!("https://apply.workable.com/api/v1/widget/accounts/{slug}")).await?;
    Ok(Some(
        data["jobs"].as_array().into_iter().flatten()
            .map(|j| {
                let url = Some(s(j, "url")).filter(|u| !u.is_empty()).unwrap_or(s(j, "shortlink"));
                let mut loc = join_nonempty([s(j, "city"), s(j, "country")], ", ");
                if j["telecommuting"].as_bool() == Some(true) {
                    loc.push_str(" (Remote)");
                }
                posting(s(j, "title"), url, loc)
            })
            .collect(),
    ))
}

async fn smartrecruiters(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let Some(slug) = slug(&c.careers_url, "smartrecruiters.com") else { return Ok(None) };
    let (mut out, mut offset) = (vec![], 0u64);
    while offset < 2000 {
        let data = get_json(http, &format!("https://api.smartrecruiters.com/v1/companies/{slug}/postings?limit=100&offset={offset}")).await?;
        for j in data["content"].as_array().into_iter().flatten() {
            let id = j["id"].as_str().map(String::from).unwrap_or_else(|| j["id"].to_string());
            out.push(posting(s(j, "name"), &format!("https://jobs.smartrecruiters.com/{slug}/{id}"),
                             join_nonempty([s(&j["location"], "city"), s(&j["location"], "country")], ", ")));
        }
        offset += 100;
        if offset >= data["totalFound"].as_u64().unwrap_or(0) {
            break;
        }
    }
    Ok(Some(out))
}

async fn recruitee(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let host = host_of(&c.careers_url);
    if !host.ends_with("recruitee.com") {
        return Ok(None);
    }
    let data = get_json(http, &format!("https://{host}/api/offers/")).await?;
    Ok(Some(
        data["offers"].as_array().into_iter().flatten()
            .map(|j| posting(s(j, "title"), s(j, "careers_url"), s(j, "location")))
            .collect(),
    ))
}

static LOCALE_SEG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z]{2}-[A-Z]{2}$").unwrap());
static N_LOCATIONS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+ Locations$").unwrap());

async fn workday(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let Ok(u) = Url::parse(c.careers_url.trim()) else { return Ok(None) };
    let host = u.host_str().unwrap_or("").to_lowercase();
    if !host.contains("myworkdayjobs.com") {
        return Ok(None);
    }
    let tenant = host.split('.').next().unwrap_or("").to_string();
    let Some(site) = u.path_segments().into_iter().flatten().find(|x| !x.is_empty() && !LOCALE_SEG.is_match(x)).map(String::from)
    else { return Ok(None) };
    // careers_url may carry ?q=Netherlands: big tenants (Salesforce, Cisco) list 1,500+ roles worldwide.
    let search = u.query_pairs().find(|(k, _)| k == "q").map(|(_, v)| v.into_owned()).unwrap_or_default();
    let (mut out, mut offset) = (vec![], 0u64);
    while offset < 1000 {
        let body = json!({"appliedFacets": {}, "limit": 20, "offset": offset, "searchText": search});
        let data: Value = http
            .post(format!("https://{host}/wday/cxs/{tenant}/{site}/jobs"))
            .header("Accept", "application/json")
            .json(&body)
            .send().await?.error_for_status()?.json().await?;
        let posts = data["jobPostings"].as_array().cloned().unwrap_or_default();
        for j in &posts {
            let path = s(j, "externalPath");
            if path.is_empty() {
                continue;
            }
            let mut loc = s(j, "locationsText").to_string();
            if !search.is_empty() && N_LOCATIONS.is_match(&loc) {
                loc = format!("{search} (one of {})", loc.to_lowercase()); // Workday hides multi-location lists
            }
            out.push(posting(s(j, "title"), &format!("https://{host}/en-US/{site}{path}"), loc));
        }
        offset += 20;
        if offset >= data["total"].as_u64().unwrap_or(0) || posts.is_empty() {
            break;
        }
    }
    Ok(Some(out))
}

/// amazon.jobs search page URL (e.g. ?base_query=machine+learning&normalized_country_code[]=NLD) -> its JSON API.
async fn amazon(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let Ok(u) = Url::parse(c.careers_url.trim()) else { return Ok(None) };
    if u.host_str().unwrap_or("").trim_start_matches("www.") != "amazon.jobs" {
        return Ok(None);
    }
    let data = get_json(http, &format!("https://www.amazon.jobs/en/search.json?{}&result_limit=100", u.query().unwrap_or(""))).await?;
    Ok(Some(
        data["jobs"].as_array().into_iter().flatten()
            .map(|j| {
                let loc = Some(s(j, "normalized_location")).filter(|x| !x.is_empty()).unwrap_or(s(j, "location"));
                posting(s(j, "title"), &format!("https://www.amazon.jobs{}", s(j, "job_path")), loc)
            })
            .collect(),
    ))
}

static JIBE_API: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/api/jobs(\?|$)").unwrap());

/// Jibe/iCIMS career sites (e.g. Booking.com): api_url like https://jobs.booking.com/api/jobs?location=Netherlands.
async fn jibe(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let api = c.api_url.trim();
    if !JIBE_API.is_match(api) {
        return Ok(None);
    }
    let sep = if api.contains('?') { '&' } else { '?' };
    let mut out = vec![];
    for page in 1..=10 {
        let data = get_json(http, &format!("{api}{sep}page={page}&limit=100")).await?;
        let jobs = data["jobs"].as_array().cloned().unwrap_or_default();
        for j in &jobs {
            let d = &j["data"];
            let url = match s(&d["meta_data"], "canonical_url") {
                "" => format!("{}/{}", c.careers_url.trim_end_matches('/'), s(d, "slug")),
                u => u.to_string(),
            };
            let loc = match s(d, "full_location") {
                "" => join_nonempty([s(d, "city"), s(d, "country")], ", "),
                l => l.to_string(),
            };
            out.push(posting(s(d, "title"), &url, loc));
        }
        if out.len() as u64 >= data["totalCount"].as_u64().unwrap_or(0) || jobs.is_empty() {
            break;
        }
    }
    Ok(Some(out))
}

/// Eightfold career sites (e.g. Netflix): api_url like https://<host>/api/apply/v2/jobs?domain=netflix.com&location=Amsterdam.
async fn eightfold(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let api = c.api_url.trim();
    if !api.contains("/api/apply/v2/jobs") {
        return Ok(None);
    }
    let sep = if api.contains('?') { '&' } else { '?' };
    let (mut out, mut start) = (vec![], 0u64);
    while start < 1000 {
        let data = get_json(http, &format!("{api}{sep}start={start}&num=100")).await?;
        let positions = data["positions"].as_array().cloned().unwrap_or_default();
        for j in &positions {
            let locs: Vec<&str> = match j["locations"].as_array() {
                Some(a) if !a.is_empty() => a.iter().filter_map(Value::as_str).collect(),
                _ => vec![s(j, "location")],
            };
            out.push(posting(s(j, "name"), s(j, "canonicalPositionUrl"), locs.join("; ")));
        }
        start += 100;
        if positions.is_empty() || start >= data["count"].as_u64().unwrap_or(0) {
            break;
        }
    }
    Ok(Some(out))
}

/// werkenbij.alliander.com has its own JSON API (GET with Accept: application/json).
async fn alliander(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    if !c.careers_url.contains("werkenbij.alliander.com") {
        return Ok(None);
    }
    let (mut out, mut offset) = (vec![], 0u64);
    while offset < 1000 {
        let data = get_json(http, &format!("https://werkenbij.alliander.com/api/vacancies?amount=50&offset={offset}")).await?;
        let vacancies = data["vacancies"].as_array().cloned().unwrap_or_default();
        for v in &vacancies {
            out.push(posting(s(v, "jobTitle"), &format!("https://werkenbij.alliander.com/vacatures{}", s(v, "path")),
                             format!("{}, Netherlands", s(v, "location"))));
        }
        offset += 50;
        if offset >= data["totalAmount"].as_u64().unwrap_or(0) || vacancies.is_empty() {
            break;
        }
    }
    Ok(Some(out))
}

static SITECORE_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"01-[0-9a-f]{8}-[0-9a-f]{40}").unwrap());
static NEXT_CHUNK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"src="([^"]*/_next/static/chunks/[^"]+\.js[^"]*)""#).unwrap());

/// asml.com job search runs on Sitecore Discover; its public client key sits in the page's
/// Next.js bundle (the server reads it from a browser request instead).
async fn asml(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    if !c.careers_url.contains("asml.com") {
        return Ok(None);
    }
    const PAGE: &str = "https://www.asml.com/en/careers/find-your-job";
    let html = get_text(http, PAGE).await?;
    let mut chunks: Vec<String> = NEXT_CHUNK.captures_iter(&html).map(|m| m[1].to_string()).collect();
    chunks.sort_by_key(|u| !u.contains("/pages/_app")); // the key lives in the app bundle
    let mut key = SITECORE_KEY.find(&html).map(|m| m.as_str().to_string());
    for u in chunks.iter().take(25) {
        if key.is_some() {
            break;
        }
        let Ok(url) = Url::parse(PAGE).and_then(|b| b.join(u)) else { continue };
        if let Ok(js) = get_text(http, url.as_str()).await {
            if js.contains("sitecorecloud.io/discover") {
                key = SITECORE_KEY.find(&js).map(|m| m.as_str().to_string());
            }
        }
    }
    let key = key.ok_or_else(|| anyhow!("ASML job search key not found on the page"))?;
    let (mut out, mut offset) = (vec![], 0u64);
    while offset < 2000 {
        let body = json!({"context": {"page": {"uri": PAGE}, "locale": {"country": "us", "language": "en"}},
                          "widget": {"items": [{"entity": "content", "rfk_id": "asml_job_search",
                                                "search": {"limit": 100, "offset": offset, "content": {}}}]}});
        let data: Value = http
            .post("https://discover-euc1.sitecorecloud.io/discover/v2/126200477")
            .header("authorization", &key)
            .header("Accept", "application/json")
            .json(&body)
            .send().await?.error_for_status()?.json().await?;
        let w = &data["widgets"][0];
        let items = w["content"].as_array().cloned().unwrap_or_default();
        out.extend(items.iter().map(|j| posting(s(j, "name"), s(j, "url"), s(j, "job_location"))));
        offset += 100;
        if items.is_empty() || offset >= w["total_item"].as_u64().unwrap_or(0) {
            break;
        }
    }
    Ok(Some(out))
}

fn xml_tag<'a>(block: &'a str, tag: &str) -> &'a str {
    let open = format!("<{tag}>");
    let Some(i) = block.find(&open) else { return "" };
    let rest = &block[i + open.len()..];
    let end = rest.find(&format!("</{tag}>")).unwrap_or(rest.len());
    let v = rest[..end].trim();
    v.strip_prefix("<![CDATA[").and_then(|x| x.strip_suffix("]]>")).unwrap_or(v).trim()
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&#39;", "'").replace("&amp;", "&")
}

fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        if !after.starts_with(['>', ' ']) {
            rest = after;
            continue;
        }
        let Some(j) = after.find(&close) else { break };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

/// Personio: https://<company>.jobs.personio.de/xml lists every open position.
async fn personio(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let host = host_of(&c.careers_url);
    if !host.contains(".jobs.personio.") {
        return Ok(None);
    }
    let xml = get_text(http, &format!("https://{host}/xml?language=en")).await?;
    Ok(Some(
        xml_blocks(&xml, "position")
            .into_iter()
            .map(|b| {
                let office = xml_unescape(xml_tag(b, "office"));
                let extra: Vec<String> = xml_blocks(b, "additionalOffices").first()
                    .map(|a| xml_blocks(a, "office").iter().map(|o| xml_unescape(o.trim())).collect())
                    .unwrap_or_default();
                let loc = join_nonempty(std::iter::once(office.as_str()).chain(extra.iter().map(String::as_str)), ", ");
                posting(&xml_unescape(xml_tag(b, "name")), &format!("https://{host}/job/{}", xml_tag(b, "id")), loc)
            })
            .collect(),
    ))
}

/// Teamtailor: https://<company>.teamtailor.com/jobs.rss.
async fn teamtailor(http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    let host = host_of(&c.careers_url);
    if !host.ends_with(".teamtailor.com") {
        return Ok(None);
    }
    let xml = get_text(http, &format!("https://{host}/jobs.rss")).await?;
    Ok(Some(teamtailor_items(&xml)))
}

fn teamtailor_items(xml: &str) -> Vec<Posting> {
    xml_blocks(xml, "item")
        .into_iter()
        .map(|b| {
            let mut places: Vec<String> = vec![];
            for l in xml_blocks(b, "tt:location") {
                let p = join_nonempty([xml_tag(l, "tt:city"), xml_tag(l, "tt:country")], ", ");
                if !p.is_empty() && !places.contains(&p) {
                    places.push(p);
                }
            }
            let remote = xml_tag(b, "remoteStatus");
            if remote == "fully" {
                places.push("Remote".into());
            }
            posting(&xml_unescape(xml_tag(b, "title")), &xml_unescape(xml_tag(b, "link")), xml_unescape(&places.join("; ")))
        })
        .collect()
}

const PROVIDERS: [&str; 14] = ["greenhouse", "ashby", "lever", "workable", "smartrecruiters", "recruitee", "workday",
    "amazon", "jibe", "eightfold", "alliander", "asml", "personio", "teamtailor"];

async fn run_provider(name: &str, http: &reqwest::Client, c: &Board) -> Result<Option<Vec<Posting>>> {
    match name {
        "greenhouse" => greenhouse(http, c).await,
        "ashby" => ashby(http, c).await,
        "lever" => lever(http, c).await,
        "workable" => workable(http, c).await,
        "smartrecruiters" => smartrecruiters(http, c).await,
        "recruitee" => recruitee(http, c).await,
        "workday" => workday(http, c).await,
        "amazon" => amazon(http, c).await,
        "jibe" => jibe(http, c).await,
        "eightfold" => eightfold(http, c).await,
        "alliander" => alliander(http, c).await,
        "asml" => asml(http, c).await,
        "personio" => personio(http, c).await,
        "teamtailor" => teamtailor(http, c).await,
        _ => Ok(None),
    }
}

/// Try the API providers in the server's order. Ok(None) when none applies.
async fn api_providers(http: &reqwest::Client, c: &Board) -> Result<Option<(Vec<Posting>, &'static str)>> {
    for name in PROVIDERS {
        if let Some(jobs) = run_provider(name, http, c).await? {
            return Ok(Some((jobs, name)));
        }
    }
    Ok(None)
}

/// ATS discovery: a branded careers page often fronts a Greenhouse board with far
/// more postings than the page shows. Probe slugs from the company name and accept a board
/// only if its registered company name matches.
async fn discover_board(http: &reqwest::Client, name: &str) -> Option<String> {
    let norm = company_norm(name);
    let first = norm.split(' ').next()?.to_string();
    let mut slugs = vec![norm.replace(' ', ""), norm.replace(' ', "-"), first.clone()];
    slugs.dedup();
    let mut seen = HashSet::new();
    for slug in slugs.into_iter().filter(|s| !s.is_empty() && seen.insert(s.clone())) {
        let Ok(r) = http.get(format!("https://boards-api.greenhouse.io/v1/boards/{slug}")).send().await else { continue };
        if !r.status().is_success() {
            continue;
        }
        let Ok(v) = r.json::<Value>().await else { continue };
        let board_name = company_norm(s(&v, "name"));
        if board_name == norm || board_name == first {
            return Some(format!("https://boards-api.greenhouse.io/v1/boards/{slug}/jobs"));
        }
    }
    None
}

// ---------- branded careers pages ----------

static JOB_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)job|vacanc|vacature|position|opening|career|stellen|req|apply|greenhouse|lever\.co|ashbyhq|myworkdayjobs|smartrecruiters|recruitee|personio|teamtailor|workable").unwrap()
});
static ATS_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"greenhouse\.io|lever\.co|ashbyhq\.com|myworkdayjobs\.com|smartrecruiters\.com|recruitee\.com|workable\.com|jobs\.personio\.(de|com)|teamtailor\.com").unwrap()
});
/// ATS board URLs anywhere in the raw HTML (embeds and scripts a browser would run).
static ATS_IN_HTML: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?:(?:\\?/){2}[a-z0-9.-]*(?:greenhouse\.io|lever\.co|ashbyhq\.com|myworkdayjobs\.com|smartrecruiters\.com|recruitee\.com|workable\.com|jobs\.personio\.(?:de|com)|teamtailor\.com)(?:\\?/[^"'\s<>\\]*)*"#).unwrap()
});
/// A path segment that identifies one posting: a number, a UUID-ish token or a long slug.
static ID_SEG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\d{3,}|^[0-9a-f]{8}-[0-9a-f]{4}|^[a-z0-9]+(?:-[a-z0-9]+){2,}$|^[a-z0-9]{2,}_[a-z0-9_-]+$").unwrap()
});
static STRONG_SEG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(jobs?|vacanc(y|ies)|vacatures?|positions?|openings?|stellen(angebote)?|stelle|job-?details?|requisitions?|o|j)$").unwrap());
/// Listing pages ("alle-vacatures", "open-positions", "job-alert").
static LISTING_SEG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^((alle|all|open|our|current)-)?(jobs|vacatures|vacancies|positions|openings|roles|careers|opportunities)$|alert|^search").unwrap());
static JOB_SEG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(jobs?|vacanc(y|ies)|vacatures?|positions?|openings?|stellen(angebote)?|stelle|careers?|job-?detail|requisitions?|apply)$").unwrap());
const GENERIC_TEXT: &[&str] = &[
    "jobs", "all jobs", "see all jobs", "view all jobs", "open positions", "open roles", "vacancies", "careers", "apply",
    "apply now", "learn more", "read more", "view job", "see job", "more", "details", "search jobs", "job search",
    "vacatures", "alle vacatures", "bekijk vacature", "meer", "lees meer",
];

#[derive(Clone, Debug)]
struct Link {
    href: String,
    text: String,
    ctx: String,
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Closest container like a browser's `a.closest('li, article, tr, [class*=job], …')`.
fn link_context(a: ElementRef) -> String {
    let mut node = Some(a);
    while let Some(el) = node {
        let e = el.value();
        let class = e.attr("class").unwrap_or("").to_lowercase();
        if matches!(e.name(), "li" | "article" | "tr")
            || ["job", "vacanc", "position", "card"].iter().any(|k| class.contains(k))
        {
            let t = collapse(&el.text().collect::<String>());
            return t.chars().take(200).collect();
        }
        node = el.parent().and_then(ElementRef::wrap);
    }
    String::new()
}

fn page_links(doc: &Html, base: &Url) -> Vec<Link> {
    let sel = Selector::parse("a[href]").unwrap();
    let heading_sel = Selector::parse("h1, h2, h3, h4, h5, h6, [class*=title], [class*=Title]").unwrap();
    doc.select(&sel)
        .filter_map(|a| {
            let href = base.join(a.value().attr("href")?.trim()).ok()?;
            // A card link: its heading is the title, the rest (location, team) is context.
            let all = collapse(&a.text().collect::<String>());
            let heading = a.select(&heading_sel).map(|h| collapse(&h.text().collect::<String>())).find(|t| !t.is_empty());
            let (text, ctx) = match heading {
                Some(h) if h != all => (h, all),
                _ => (all, link_context(a)),
            };
            Some(Link { href: href.to_string(), text, ctx })
        })
        .collect()
}

/// ATS board URLs the page links to, embeds (iframe/script) or mentions in its scripts.
fn board_candidates(doc: &Html, html: &str, links: &[Link], base: &Url) -> Vec<String> {
    let mut out: Vec<String> = links.iter().map(|l| l.href.clone()).collect();
    let sel = Selector::parse("iframe[src], script[src], [data-src]").unwrap();
    for el in doc.select(&sel) {
        for attr in ["src", "data-src"] {
            if let Some(u) = el.value().attr(attr).and_then(|v| base.join(v.trim()).ok()) {
                out.push(u.to_string());
            }
        }
    }
    out.extend(ATS_IN_HTML.find_iter(html).map(|m| m.as_str().replace("\\/", "/")));
    let mut seen = HashSet::new();
    out.retain(|u| ATS_HOST.is_match(&host_of(u)) && seen.insert(u.clone()));
    out
}

/// If a branded careers page links to an ATS board, read the board through its API.
async fn linked_board(http: &reqwest::Client, urls: &[String]) -> Option<(Vec<Posting>, &'static str)> {
    let mut tried = HashSet::new();
    for href in urls.iter().take(40) {
        let Ok(u) = Url::parse(href) else { continue };
        let host = u.host_str().unwrap_or("").to_lowercase();
        let mut segs: Vec<&str> = u.path_segments().into_iter().flatten().filter(|s| !s.is_empty()).collect();
        if host.contains("myworkdayjobs.com") {
            segs.retain(|s| !LOCALE_SEG.is_match(s));
        }
        let board_url = if let Some(gh) = greenhouse_slug(href, "") {
            format!("https://boards.greenhouse.io/{gh}")
        } else if host.contains(".jobs.personio.") || host.ends_with("teamtailor.com") || host.ends_with("recruitee.com") {
            format!("https://{host}/")
        } else if let Some(first) = segs.first() {
            // Boards reached via a script/embed path ("/embed", "/api", "/v1") are not company slugs.
            if ["embed", "api", "v0", "v1", "widget", "static", "assets", "js"].contains(&first.to_lowercase().as_str()) {
                continue;
            }
            format!("{}://{host}/{first}", u.scheme())
        } else {
            continue;
        };
        if !tried.insert(board_url.clone()) {
            continue;
        }
        let board = Board { careers_url: board_url, ..Default::default() };
        for name in PROVIDERS {
            match run_provider(name, http, &board).await {
                Ok(Some(jobs)) if !jobs.is_empty() => return Some((jobs, name)),
                Ok(Some(_)) => break,
                _ => {}
            }
        }
    }
    None
}

/// JSON-LD JobPosting objects on the page.
fn json_ld_postings(doc: &Html, base: &Url) -> Vec<Posting> {
    fn walk(v: &Value, base: &Url, out: &mut Vec<Posting>) {
        match v {
            Value::Array(a) => a.iter().for_each(|x| walk(x, base, out)),
            Value::Object(o) => {
                let is_job = match &o.get("@type") {
                    Some(Value::String(t)) => t == "JobPosting",
                    Some(Value::Array(a)) => a.iter().any(|t| t == "JobPosting"),
                    _ => false,
                };
                if is_job {
                    let title = o.get("title").and_then(Value::as_str).unwrap_or("");
                    let url = o.get("url").and_then(Value::as_str).and_then(|u| base.join(u).ok()).map(|u| u.to_string());
                    let locs: Vec<String> = match o.get("jobLocation") {
                        Some(Value::Array(a)) => a.iter().map(ld_place).collect(),
                        Some(l) => vec![ld_place(l)],
                        None => vec![],
                    };
                    if let Some(url) = url.filter(|_| !title.trim().is_empty()) {
                        out.push(posting(title, &url, join_nonempty(locs.iter().map(String::as_str), "; ")));
                    }
                }
                for k in ["@graph", "itemListElement", "item"] {
                    if let Some(x) = o.get(k) {
                        walk(x, base, out);
                    }
                }
            }
            _ => {}
        }
    }
    fn ld_place(l: &Value) -> String {
        let a = &l["address"];
        let country = a["addressCountry"].as_str().map(String::from).or_else(|| a["addressCountry"]["name"].as_str().map(String::from)).unwrap_or_default();
        join_nonempty([s(a, "addressLocality"), s(a, "addressRegion"), country.as_str()], ", ")
    }
    let sel = Selector::parse(r#"script[type="application/ld+json"]"#).unwrap();
    let mut out = vec![];
    for el in doc.select(&sel) {
        if let Ok(v) = serde_json::from_str::<Value>(&el.text().collect::<String>()) {
            walk(&v, base, &mut out);
        }
    }
    out
}

/// Place names (from the country table) mentioned in a link's surroundings.
fn places_in(ctx: &str) -> String {
    let lower = ctx.to_lowercase();
    let mut found: Vec<(usize, String)> = vec![];
    for (_, names) in COUNTRIES {
        for n in *names {
            if n.len() > 3 && *n != "nice" && has_word(ctx, n) {
                if let Some(i) = lower.find(n) {
                    let orig: String = ctx.get(i..i + n.len()).map(String::from).unwrap_or_else(|| n.to_string());
                    if !found.iter().any(|(_, f)| f.eq_ignore_ascii_case(&orig)) {
                        found.push((i, orig));
                    }
                }
            }
        }
    }
    found.sort();
    found.into_iter().map(|(_, n)| n).collect::<Vec<_>>().join(", ")
}

/// Does this URL look like one posting (not a listing, team or blog page)? A job-ish URL
/// whose last path segment is an id / long slug, or a slug under a "jobs"/"vacatures" segment.
fn posting_url(u: &Url) -> bool {
    if !JOB_URL.is_match(&format!("{}{}", u.path(), u.host_str().unwrap_or(""))) {
        return false;
    }
    let segs: Vec<&str> = u.path_segments().into_iter().flatten().filter(|s| !s.is_empty()).collect();
    let Some(last) = segs.last() else { return false };
    if JOB_SEG.is_match(last) || LISTING_SEG.is_match(last) {
        return false;
    }
    let id_query = u.query_pairs().any(|(k, v)| {
        ["id", "jobid", "job_id", "gh_jid", "vacancyid", "req", "jobreq"].contains(&k.to_lowercase().as_str()) && !v.is_empty()
    });
    // A slug or id somewhere after a "jobs"/"vacatures" segment (…/vacancies/JX9XJ592/…/amsterdam).
    let under_strong = segs.iter().position(|s| STRONG_SEG.is_match(s)).is_some_and(|i| {
        segs[i + 1..].iter().any(|s| s.contains('-') || s.contains('_') || s.chars().any(|c| c.is_ascii_digit()))
    });
    (ID_SEG.is_match(last) && segs.len() >= 2) || under_strong || id_query
}

/// Job links on the page itself: a posting-like URL with a title-like link text. (The server
/// lets an LLM pick these from a browser-rendered page.)
fn link_postings(links: &[Link], page: &Url) -> Vec<Posting> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    let page_key = url_key(page.as_str());
    for l in links {
        let text = l.text.trim();
        let words = text.split_whitespace().count();
        if text.is_empty() || text.chars().count() > 160 || words > 20 || !l.href.starts_with("http") {
            continue;
        }
        if GENERIC_TEXT.contains(&text.to_lowercase().trim_end_matches(['>', '→', '›', ' ']).trim()) {
            continue;
        }
        let Ok(u) = Url::parse(&l.href) else { continue };
        if !posting_url(&u) {
            continue;
        }
        let key = url_key(&l.href);
        if key == page_key || !seen.insert(key) {
            continue;
        }
        let ctx = l.ctx.replacen(text, "", 1);
        out.push(posting(text, &l.href, places_in(&ctx)));
    }
    out
}

static JSON_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(?:url|Url|URL|NavigateUrl|navigateUrl|jobUrl|job_url|absolute_url|applyUrl|apply_url|link|href|canonicalUrl|canonical_url|detailUrl|detailsUrl|externalUrl|hostedUrl|jobPath|job_path|path|slug)"\s*:\s*("(?:[^"\\]|\\.){1,500}")"#).unwrap()
});
static JSON_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(?:title|Title|jobTitle|job_title|JobTitle|positionTitle|name|Name|text|postingTitle)"\s*:\s*("(?:[^"\\]|\\.){2,300}")"#).unwrap()
});
static JSON_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(?:location|Location|locationName|locationsText|city|City|jobLocation|workLocation|place|full_location|normalized_location)"\s*:\s*("(?:[^"\\]|\\.){1,200}")"#).unwrap()
});

fn json_str(quoted: &str) -> String {
    serde_json::from_str::<String>(quoted).unwrap_or_else(|_| quoted.trim_matches('"').to_string())
}

/// Script text the page embeds: inline scripts plus `data:` (base64) script sources.
fn page_scripts(doc: &Html) -> Vec<String> {
    let sel = Selector::parse("script").unwrap();
    let mut out = vec![];
    for el in doc.select(&sel) {
        if let Some(src) = el.value().attr("src") {
            if let Some(b64) = src.strip_prefix("data:text/javascript;base64,").or_else(|| src.strip_prefix("data:application/javascript;base64,")) {
                if let Some(bytes) = base64_decode(b64) {
                    out.push(String::from_utf8_lossy(&bytes).into_owned());
                }
            }
            continue;
        }
        let t = el.text().collect::<String>();
        if t.len() > 40 {
            out.push(t);
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..chunk.len().saturating_sub(1)]);
    }
    Some(out)
}

/// Postings in JSON the page embeds for its own scripts (e.g. a Vue/React job list): a
/// posting-like URL value with the nearest title (and location) key around it.
fn embedded_postings(scripts: &[String], page: &Url) -> Vec<Posting> {
    let page_key = url_key(page.as_str());
    let mut seen = HashSet::new();
    let mut out = vec![];
    for text in scripts {
        let titles: Vec<(usize, String)> = JSON_TITLE.captures_iter(text).map(|c| (c.get(0).unwrap().start(), json_str(&c[1]))).collect();
        let locs: Vec<(usize, String)> = JSON_LOC.captures_iter(text).map(|c| (c.get(0).unwrap().start(), json_str(&c[1]))).collect();
        let nearest = |list: &[(usize, String)], at: usize| {
            list.iter()
                .filter(|(i, _)| i.abs_diff(at) < 600)
                .min_by_key(|(i, _)| i.abs_diff(at))
                .map(|(_, v)| v.clone())
        };
        for c in JSON_URL.captures_iter(text) {
            let at = c.get(0).unwrap().start();
            let raw = json_str(&c[1]);
            let Ok(u) = page.join(raw.trim()) else { continue };
            if !matches!(u.scheme(), "http" | "https") || !posting_url(&u) {
                continue;
            }
            let Some(title) = nearest(&titles, at).map(|t| collapse(&fetch::html_to_text(&t))) else { continue };
            if title.is_empty() || title.chars().count() > 160 || title.starts_with("http") || title.contains('/') && !title.contains(' ') {
                continue;
            }
            let key = url_key(u.as_str());
            if key == page_key || !seen.insert(key) {
                continue;
            }
            out.push(posting(&title, u.as_str(), nearest(&locs, at).unwrap_or_default()));
        }
    }
    out
}

async fn get_page(http: &reqwest::Client, url: &str) -> Result<(Url, String)> {
    let r = match http.get(url).send().await {
        Ok(r) => r,
        // Some servers break HTTP/2 streams (careers.klm.com); retry once over HTTP/1.1.
        Err(e) if format!("{e:?}").to_lowercase().contains("http2") => http1().get(url).send().await?,
        Err(e) => return Err(e.into()),
    };
    let r = r.error_for_status()?;
    let base = r.url().clone();
    Ok((base, r.text().await?))
}

fn http1() -> reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0 Safari/537.36")
            .http1_only()
            .timeout(std::time::Duration::from_secs(25))
            .build()
            .unwrap_or_else(|_| fetch::client())
    });
    CLIENT.clone()
}

/// Branded careers page: fetch it, then read (in order) a Teamtailor feed on its own domain,
/// the job board it links to or embeds, its JSON-LD postings, postings in its embedded JSON,
/// or its job links. Returns (postings, via).
async fn career_page(http: &reqwest::Client, c: &Board) -> Result<(Vec<Posting>, &'static str)> {
    let (base, html) = get_page(http, c.careers_url.trim()).await?;
    let (links, candidates, ld, scripts) = {
        let doc = Html::parse_document(&html);
        let links = page_links(&doc, &base);
        let candidates = board_candidates(&doc, &html, &links, &base);
        (links, candidates, json_ld_postings(&doc, &base), page_scripts(&doc))
    };
    if html.contains("teamtailor") {
        // Teamtailor career sites on a custom domain (career.acme.com) serve the same feed.
        if let Some(host) = base.host_str() {
            if let Ok(xml) = get_text(http, &format!("https://{host}/jobs.rss")).await {
                let jobs = teamtailor_items(&xml);
                if !jobs.is_empty() {
                    return Ok((jobs, "teamtailor"));
                }
            }
        }
    }
    if let Some((jobs, via)) = linked_board(http, &candidates).await {
        return Ok((jobs, via));
    }
    if !ld.is_empty() {
        return Ok((ld, "careers page"));
    }
    let embedded = embedded_postings(&scripts, &base);
    let linked = link_postings(&links, &base);
    // Whichever reading found more postings; the other one adds what it alone saw.
    let (mut jobs, extra) = if embedded.len() > linked.len() { (embedded, linked) } else { (linked, embedded) };
    let have: HashSet<String> = jobs.iter().map(|j| url_key(&j.url)).collect();
    jobs.extend(extra.into_iter().filter(|j| !have.contains(&url_key(&j.url))));
    Ok((jobs, "careers page"))
}

// ---------- scan ----------

#[derive(Clone, Debug)]
pub struct Progress {
    pub company: String,
    pub done: usize,
    pub total: usize,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct ScanSummary {
    pub companies: usize,
    /// Open postings read across all companies.
    pub postings: usize,
    /// Postings that passed the title/location filters.
    pub matched: usize,
    /// New inbox entries (state "new").
    pub new: usize,
    /// "Company: message" per failed company (or a reason the scan didn't run).
    pub errors: Vec<String>,
    /// Ids of the new inbox entries, newest scan order (for auto_process).
    pub new_ids: Vec<i64>,
}

static RUNNING: AtomicBool = AtomicBool::new(false);

pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

struct RunGuard;
impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

struct Shared {
    known_urls: Mutex<HashSet<String>>,
    known_roles: HashMap<String, (i64, String)>,
    summary: Mutex<ScanSummary>,
}

/// What one company's fetch produced.
struct Fetched {
    jobs: Vec<Posting>,
    via: String,
    discovered_api: Option<String>,
}

async fn fetch_company(http: &reqwest::Client, c: &Company, report: &impl Fn(&str)) -> Result<Fetched> {
    for u in [&c.careers_url, &c.api_url] {
        if u.starts_with("http") {
            let p = Url::parse(u).with_context(|| format!("invalid URL {u}"))?;
            if p.host_str().is_none() {
                bail!("invalid URL {u}");
            }
        }
    }
    let mut board = Board { careers_url: c.careers_url.clone(), api_url: c.api_url.clone() };
    report("Checking job-board APIs");
    if let Some((jobs, via)) = api_providers(http, &board).await? {
        return Ok(Fetched { jobs, via: via.into(), discovered_api: None });
    }
    if c.api_url.trim().is_empty() {
        // api_url "page" = read the careers page only
        report("Looking for a job-board API behind the careers page");
        if let Some(api) = discover_board(http, &c.name).await {
            board.api_url = api.clone();
            let jobs = greenhouse(http, &board).await?.unwrap_or_default();
            return Ok(Fetched { jobs, via: "greenhouse (auto-detected)".into(), discovered_api: Some(api) });
        }
    }
    report("Reading the careers page");
    let (jobs, via) = career_page(http, &board).await?;
    Ok(Fetched { jobs, via: via.into(), discovered_api: None })
}

fn update_company(db: &Db, id: i64, status: &str, found: Option<i64>, postings: &Value) -> Result<()> {
    db.conn.execute(
        "UPDATE companies SET last_scan_at = ?1, last_status = ?2, last_found = COALESCE(?3, last_found), last_postings = ?4 WHERE id = ?5",
        params![db::now(), status, found, postings.to_string(), id],
    )?;
    Ok(())
}

/// Filter one company's postings and record the result. Runs under the DB lock (no awaits).
fn record(db: &Db, c: &Company, f: &Fetched, p: &SearchPrefs, shared: &Shared) -> Result<(usize, Vec<i64>)> {
    if let Some(api) = &f.discovered_api {
        db.conn.execute("UPDATE companies SET api_url = ?1 WHERE id = ?2", params![api, c.id])?;
    }
    let strict = f.via != "careers page";
    let mut postings: Vec<(u8, Value)> = vec![];
    let mut matched = 0;
    let mut new_ids = vec![];
    let mut seen_here = HashSet::new();
    for j in &f.jobs {
        if j.url.is_empty() || j.title.is_empty() || !seen_here.insert(j.url.clone()) {
            continue;
        }
        let row = |verdict: &str, reason: &str| {
            json!({"title": j.title, "url": j.url, "location": j.location, "verdict": verdict,
                   "matched": verdict != "rejected", "reason": reason})
        };
        if let Some(reason) = title_reason(&j.title, p).or_else(|| location_reason(&j.location, p, strict)) {
            postings.push((3, row("rejected", &reason)));
            continue;
        }
        matched += 1;
        let key = url_key(&j.url);
        if !shared.known_urls.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone()) {
            postings.push((2, row("seen", "Already seen in an earlier scan")));
            continue;
        }
        let (state, reason) = match shared.known_roles.get(&role_key(&c.name, &j.title)) {
            Some((id, status)) => ("duplicate", Some(format!("Same role already tracked as #{id} ({status})"))),
            None => ("new", None),
        };
        let inserted = db.conn.execute(
            "INSERT OR IGNORE INTO discovered (url, url_key, company, title, location, first_seen, state, reason, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'scan')",
            params![j.url, key, c.name, j.title, j.location, db::now(), state, reason],
        )?;
        if inserted == 1 && state == "new" {
            new_ids.push(db.conn.last_insert_rowid());
        }
        let shown = reason.unwrap_or_else(|| "New match — added to the inbox".into());
        postings.push((if state == "new" { 0 } else { 1 }, row(state, &shown)));
    }
    postings.sort_by_key(|(o, _)| *o);
    let list = Value::Array(postings.into_iter().map(|(_, v)| v).collect());
    let status = format!("ok · {} · {} open, {matched} match, {} new", f.via, f.jobs.len(), new_ids.len());
    update_company(db, c.id, &status, Some(matched as i64), &list)?;
    Ok((matched, new_ids))
}

/// Scan companies' career sites. `None` = favourites (or every enabled company when
/// `favorites_only` is off); `Some(ids)` = exactly those companies. Four companies are read
/// at a time; each one's result lands in `companies.last_*` and new matches in `discovered`.
pub async fn scan(store: &Store, company_ids: Option<Vec<i64>>, progress: impl Fn(Progress) + Send + Sync) -> ScanSummary {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return ScanSummary { errors: vec!["A scan is already running".into()], ..Default::default() };
    }
    let _guard = RunGuard;

    let setup = (|| -> Result<_> {
        let db = db::lock(store);
        let p = prefs(&db);
        let fav_only = p.favorites_only && company_ids.is_none();
        let targets: Vec<Company> = companies(&db)?
            .into_iter()
            .filter(|c| match &company_ids {
                Some(ids) => ids.contains(&c.id),
                None => c.enabled && (!fav_only || c.favorite),
            })
            .collect();
        let mut known_urls: HashSet<String> = {
            let mut st = db.conn.prepare("SELECT url_key FROM discovered")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut known_roles = HashMap::new();
        {
            let mut st = db.conn.prepare("SELECT id, url, company, title, status FROM jobs ORDER BY id")?;
            let rows = st.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?))
            })?;
            for row in rows {
                let (id, url, company, title, status) = row?;
                if let Some(u) = url.filter(|u| !u.is_empty()) {
                    known_urls.insert(url_key(&u));
                }
                if let Some(t) = title.filter(|t| !t.is_empty()) {
                    known_roles.insert(role_key(&company.unwrap_or_default(), &t), (id, status.unwrap_or_default()));
                }
            }
        }
        Ok((p, targets, known_urls, known_roles))
    })();
    let (p, targets, known_urls, known_roles) = match setup {
        Ok(x) => x,
        Err(e) => return ScanSummary { errors: vec![format!("{e:#}")], ..Default::default() },
    };

    let total = targets.len();
    let shared = Shared {
        known_urls: Mutex::new(known_urls),
        known_roles,
        summary: Mutex::new(ScanSummary { companies: total, ..Default::default() }),
    };
    let queue = Mutex::new(targets.into_iter().collect::<VecDeque<_>>());
    let done = AtomicUsize::new(0);
    let http = fetch::client();

    let worker = || async {
        loop {
            let Some(c) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() else { break };
            let report = |msg: &str| {
                progress(Progress { company: c.name.clone(), done: done.load(Ordering::SeqCst), total, message: msg.to_string() })
            };
            let fetched = fetch_company(&http, &c, &report).await;
            let outcome = fetched.and_then(|f| {
                report(&format!("Filtering {} postings", f.jobs.len()));
                let db = db::lock(store);
                record(&db, &c, &f, &p, &shared).map(|(m, ids)| (f.jobs.len(), m, ids))
            });
            let n = done.fetch_add(1, Ordering::SeqCst) + 1;
            let message = {
                let mut sum = shared.summary.lock().unwrap_or_else(|e| e.into_inner());
                match outcome {
                    Ok((open, matched, ids)) => {
                        sum.postings += open;
                        sum.matched += matched;
                        sum.new += ids.len();
                        let msg = format!("{open} open, {matched} match, {} new", ids.len());
                        sum.new_ids.extend(ids);
                        msg
                    }
                    Err(e) => {
                        let msg: String = format!("{e:#}").chars().take(200).collect();
                        let _ = update_company(&db::lock(store), c.id, &format!("error · {msg}"), None, &json!([]));
                        sum.errors.push(format!("{}: {msg}", c.name));
                        format!("error · {msg}")
                    }
                }
            };
            progress(Progress { company: c.name.clone(), done: n, total, message });
        }
    };
    // Four companies at a time (the server used 8 API + 2 browser slots).
    tokio::join!(worker(), worker(), worker(), worker());
    shared.summary.into_inner().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn p() -> SearchPrefs {
        SearchPrefs {
            title_positive: ["Data Scientist", "Machine Learning", "ML Engineer", "AI Engineer", "LLM", "RAG", "GenAI"]
                .map(String::from).to_vec(),
            title_negative: ["Junior", "Intern", ".NET", "Java ", "SAP ", "Werkstudent"].map(String::from).to_vec(),
            locations_allow: ["Netherlands", "Amsterdam", "NL"].map(String::from).to_vec(),
            locations_block: ["Remote - US"].map(String::from).to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn title_filter_whole_words_and_prefix_exclusions() {
        let p = p();
        assert!(title_ok("Senior Data Scientist", &p));
        assert!(title_ok("Data Scientists - Payments", &p)); // plural
        assert!(title_ok("Machine Learning Engineer II", &p));
        assert!(title_ok("Staff LLM Engineer", &p));
        assert!(!title_ok("Storage Engineer", &p)); // RAG must not match Storage
        assert!(title_ok("RAG Engineer", &p));
        assert!(!title_ok("Product Designer", &p));
        assert_eq!(title_reason("Machine Learning Internship", &p).unwrap(), "Title contains excluded word \"Intern\"");
        assert!(!title_ok("Junior Data Scientist", &p));
        assert!(!title_ok("Werkstudent Machine Learning (m/w/d)", &p));
        // "Java " is whole-word: JavaScript passes, Java fails.
        assert!(title_ok("Machine Learning Engineer (JavaScript)", &p));
        assert!(!title_ok("Machine Learning Engineer (Java)", &p));
        assert!(title_ok("SAPIENT AI Engineer", &p));
        assert!(!title_ok("SAP AI Engineer", &p));
        assert!(!title_ok("ML Engineer .NET", &p));
        assert!(title_ok("GenAI/LLM engineer", &p));
        // Empty include list accepts everything not excluded.
        let open = SearchPrefs { title_positive: vec![], ..p.clone() };
        assert!(title_ok("Chef", &open));
    }

    #[test]
    fn location_filter() {
        let p = p();
        assert!(location_ok("", &p, true));
        assert!(location_ok("Amsterdam, Netherlands", &p, true));
        assert!(location_ok("Utrecht, NL", &p, true));
        assert!(!location_ok("London, UK", &p, true));
        assert!(location_ok("London, UK", &p, false)); // careers pages: unknown places pass
        assert!(!location_ok("Remote - US", &p, false)); // block wins
        assert!(!location_ok("Nlanta", &p, true)); // whole word
        assert_eq!(location_reason("Paris", &p, true).unwrap(), "Location \"Paris\" is outside your preferred places");
    }

    #[test]
    fn url_keys_match_the_server() {
        assert_eq!(url_key("https://www.Example.com/Jobs/123/?utm_source=x&b=2&a=1&ref=y&gh_src=z"), "example.com/jobs/123?a=1&b=2");
        assert_eq!(url_key("https://jobs.booking.com/booking/jobs/30379?lang=en-us"), "jobs.booking.com/booking/jobs/30379?lang=en-us");
        assert_eq!(url_key("https://x.com/a?q=hello world&e="), "x.com/a?q=hello+world");
        assert_eq!(url_key("https://x.com/"), "x.com");
        assert_eq!(role_key("Uber (Amsterdam)", "ML Engineer, Maps"), "uber amsterdam ml engineer maps");
    }

    #[test]
    fn favourites_and_company_norm() {
        assert_eq!(company_norm("Mistral AI (Amsterdam)"), "mistral ai");
        let favs: HashSet<String> = ["uber".to_string(), "mistral ai".to_string()].into();
        assert!(in_favorites("Uber (Amsterdam)", Some(&favs)));
        assert!(in_favorites("Uber Eats", Some(&favs)));
        assert!(in_favorites("Mistral", Some(&favs)));
        assert!(!in_favorites("Ubernet", Some(&favs)));
        assert!(in_favorites("anything", None));
    }


    #[test]
    fn countries() {
        assert_eq!(country_of("Amsterdam, The Netherlands, Madrid, Spain"), "Netherlands");
        assert_eq!(country_of("San Francisco, CA"), "United States");
        assert_eq!(country_of("Austin, TX"), "United States");
        assert_eq!(country_of("Remote"), "Remote");
        assert_eq!(country_of("Atlantis"), "");
        assert_eq!(countries_of("Paris, London, Remote"), vec!["United Kingdom", "France", "Remote"]);
        assert_eq!(country_of("München"), "Germany");
    }

    #[test]
    fn career_page_links() {
        let html = r#"<html><body>
          <nav><a href="/careers">Careers</a><a href="/careers/teams/machine-learning">Machine Learning</a></nav>
          <ul>
            <li><a href="/vacatures/data-scientist-12345">Data Scientist</a> <span>Amsterdam · Full-time</span></li>
            <li><a href="/en/jobs/senior-ml-engineer-payments">Senior ML Engineer</a> London</li>
            <li><a href="/vacatures">See all jobs</a></li>
          </ul>
          <script type="application/ld+json">{"@type":"JobPosting","title":"AI Engineer","url":"/job/9","jobLocation":{"address":{"addressLocality":"Utrecht","addressCountry":"NL"}}}</script>
          <iframe src="https://boards.greenhouse.io/embed/job_board?for=acme"></iframe>
        </body></html>"#;
        let base = Url::parse("https://www.example.com/careers").unwrap();
        let doc = Html::parse_document(html);
        let links = page_links(&doc, &base);
        let jobs = link_postings(&links, &base);
        assert_eq!(jobs.len(), 2, "{jobs:?}");
        assert_eq!(jobs[0], posting("Data Scientist", "https://www.example.com/vacatures/data-scientist-12345", "Amsterdam"));
        assert_eq!(jobs[1].location, "London");
        let ld = json_ld_postings(&doc, &base);
        assert_eq!(ld, vec![posting("AI Engineer", "https://www.example.com/job/9", "Utrecht, NL")]);
        let boards = board_candidates(&doc, html, &links, &base);
        assert_eq!(boards, vec!["https://boards.greenhouse.io/embed/job_board?for=acme".to_string()]);
    }

    #[test]
    fn embedded_json_postings() {
        let js = r#"init({items:[{"ListDataID":1,"NavigateUrl":"https://www.example.nl/vacatures/it-en-data/senior-data-scientist","Title":"Senior Data Scientist","CustomFields":{"city":"Schiphol"}},
                    {"NavigateUrl":"https://www.example.nl/vacatures/alle-vacatures","Title":"Alle vacatures"},
                    {"url":"\/en\/jobs\/12345-ml-engineer","title":"ML Engineer &amp; MLOps","location":"Amsterdam"}]})"#;
        let b64 = "aGVsbG8gd29ybGQ="; // "hello world"
        assert_eq!(base64_decode(b64).unwrap(), b"hello world");
        let base = Url::parse("https://www.example.nl/vacatures").unwrap();
        let jobs = embedded_postings(&[js.to_string()], &base);
        assert_eq!(jobs, vec![
            posting("Senior Data Scientist", "https://www.example.nl/vacatures/it-en-data/senior-data-scientist", "Schiphol"),
            posting("ML Engineer & MLOps", "https://www.example.nl/en/jobs/12345-ml-engineer", "Amsterdam"),
        ]);
        let teamtailor = "<rss><item><title>AI Engineer</title><link>https://c.example/jobs/1-ai</link><tt:locations><tt:location><tt:city>Rotterdam</tt:city><tt:country>Netherlands</tt:country></tt:location></tt:locations><remoteStatus>none</remoteStatus></item></rss>";
        assert_eq!(teamtailor_items(teamtailor), vec![posting("AI Engineer", "https://c.example/jobs/1-ai", "Rotterdam, Netherlands")]);
    }

    #[test]
    fn personio_xml() {
        let xml = "<workzag-jobs><position><id>42</id><office>Berlin</office><additionalOffices><office>Amsterdam</office></additionalOffices><name>ML Engineer &amp; MLOps</name></position></workzag-jobs>";
        let b = xml_blocks(xml, "position");
        assert_eq!(b.len(), 1);
        assert_eq!(xml_unescape(xml_tag(b[0], "name")), "ML Engineer & MLOps");
        assert_eq!(xml_tag(b[0], "id"), "42");
    }

    #[test]
    fn prefs_round_trip_and_inbox() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        assert!(prefs(&db).favorites_only);
        db.set_setting("search_prefs", &json!({"auto_process_limit": 7.0, "scan_interval_hours": 0, "custom": 1,
            "locations_allow": ["Netherlands"], "title_positive": ["Data Scientist"], "title_negative": []})).unwrap();
        let mut sp = prefs(&db);
        assert_eq!(sp.auto_process_limit, 7);
        assert!(sp.favorites_only);
        sp.title_negative = vec!["  Java ".into(), " ".into()];
        sp.favorites_only = false;
        save_prefs(&db, &sp).unwrap();
        let back = prefs(&db);
        assert_eq!(back.title_negative, vec!["Java ".to_string()]);
        assert_eq!(back.extra["custom"], 1);

        assert_eq!(seed_catalog(&db).unwrap(), catalog().len());
        assert_eq!(seed_catalog(&db).unwrap(), 0);
        let id = add_company(&db, "Acme", "https://boards.greenhouse.io/embed/job_board?for=acme").unwrap();
        let c = company(&db, id).unwrap();
        assert!(c.favorite && c.api_url.ends_with("/boards/acme/jobs"));
        assert!(add_company(&db, "Bad", "not a url").is_err());

        let f = Fetched {
            jobs: vec![
                posting("Senior Data Scientist", "https://acme.com/jobs/1?utm_source=li", "Amsterdam, Netherlands"),
                posting("Data Scientist", "https://acme.com/jobs/2", "Paris, France"),
                posting("Chef", "https://acme.com/jobs/3", "Amsterdam"),
            ],
            via: "greenhouse".into(),
            discovered_api: None,
        };
        let shared = Shared { known_urls: Mutex::new(HashSet::new()), known_roles: HashMap::new(), summary: Default::default() };
        let (matched, ids) = record(&db, &c, &f, &back, &shared).unwrap();
        assert_eq!((matched, ids.len()), (1, 1));
        let c = company(&db, id).unwrap();
        assert_eq!(c.last_found, Some(1));
        assert_eq!(c.last_postings[0]["verdict"], "new");
        assert_eq!(c.last_postings[0]["matched"], true);
        assert_eq!(c.last_postings[2]["matched"], false);
        // Second time round the same URL is "seen".
        let (_, ids2) = record(&db, &c, &f, &back, &shared).unwrap();
        assert!(ids2.is_empty());
        let inbox = discovered(&db, Some("new")).unwrap();
        assert_eq!(inbox.len(), 1);
        set_discovered_state(&db, inbox[0].id, "dismissed").unwrap();
        assert!(set_discovered_state(&db, inbox[0].id, "bogus").is_err());
        let (job, created) = queue_discovered(&db, inbox[0].id).unwrap();
        assert!(created && job > 0);
        assert_eq!(queue_discovered(&db, inbox[0].id).unwrap(), (job, false));
    }

    /// cargo test -- --ignored --nocapture live_scan
    #[tokio::test]
    #[ignore]
    async fn live_scan() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        db.set_setting("search_prefs", &json!({
            "title_positive": ["Data Scientist", "Machine Learning", "ML Engineer", "AI Engineer", "Applied Scientist", "LLM", "MLOps"],
            "title_negative": ["Junior", "Intern"],
            "locations_allow": ["Netherlands", "Amsterdam", "NL"], "favorites_only": true})).unwrap();
        let picks = ["TomTom", "Mistral AI", "Adyen", "Wolters Kluwer", "Booking.com", "Alliander", "Schiphol"];
        let mut ids = vec![];
        for c in catalog().into_iter().filter(|c| picks.iter().any(|p| c.name.starts_with(p))) {
            db.conn.execute("INSERT INTO companies (name, careers_url, api_url, notes, favorite) VALUES (?1, ?2, ?3, '', 1)",
                            params![c.name, c.careers_url, c.api_url]).unwrap();
            ids.push(db.conn.last_insert_rowid());
        }
        let store = db.store();
        let t = std::time::Instant::now();
        let sum = scan(&store, None, |p| println!("[{}/{}] {}: {}", p.done, p.total, p.company, p.message)).await;
        println!("{sum:#?} in {:?}", t.elapsed());
        let db = db::lock(&store);
        for c in companies(&db).unwrap() {
            println!("{} | {} | {}", c.name, c.last_status, c.careers_url);
            for p in c.last_postings.as_array().unwrap().iter().filter(|p| p["matched"] == true).take(4) {
                println!("    {} | {} | {}", p["title"], p["location"], p["url"]);
            }
        }
        assert_eq!(sum.companies, ids.len());
        assert!(sum.postings > 0);
    }
}
