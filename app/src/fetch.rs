//! Turn a job posting URL into plain text.
//!
//! Order: Workday / Greenhouse / SmartRecruiters JSON APIs (cleaner than their HTML, and
//! they detect closed postings), then plain HTTP (JSON-LD JobPosting, visible text,
//! embedded JSON blobs), then other ATS APIs (Lever, Ashby) when the page is JS-rendered.

use std::{fmt, sync::OnceLock, time::Duration};

use regex::Regex;
use reqwest::{header, StatusCode};
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::Value;
use url::Url;

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/128.0 Safari/537.36";
const MIN_USEFUL_CHARS: usize = 800;
const MAX_CHARS: usize = 30_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    pub company: String,
    pub location: String,
    pub text: String,
}

#[derive(Debug)]
pub enum FetchError {
    Closed(String),
    Unsafe(String),
    Other(anyhow::Error),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(m) | Self::Unsafe(m) => f.write_str(m),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<reqwest::Error> for FetchError {
    fn from(e: reqwest::Error) -> Self {
        Self::Other(e.into())
    }
}

/// Shared HTTP client: browser-like UA, 25s timeout, follows redirects.
pub fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let mut h = header::HeaderMap::new();
            h.insert(
                header::ACCEPT,
                header::HeaderValue::from_static(
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                ),
            );
            h.insert(
                header::ACCEPT_LANGUAGE,
                header::HeaderValue::from_static("en-US,en;q=0.9"),
            );
            reqwest::Client::builder()
                .user_agent(UA)
                .default_headers(h)
                .timeout(Duration::from_secs(25))
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .expect("failed to build HTTP client")
        })
        .clone()
}

pub async fn fetch_job(url: &str) -> Result<Page, FetchError> {
    let parsed =
        Url::parse(url.trim()).map_err(|e| FetchError::Unsafe(format!("Invalid URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(FetchError::Unsafe(format!(
            "Only http(s) URLs are supported: {url}"
        )));
    }
    let http = client();

    if let Some(page) = workday(&http, &parsed).await? {
        return Ok(page);
    }

    let useful = |p: &Page| p.text.chars().count() >= MIN_USEFUL_CHARS;
    let mut best = Page::default();
    let host = parsed.host_str().unwrap_or("");
    let api_first = host.ends_with("greenhouse.io") || host.ends_with("smartrecruiters.com");
    if api_first {
        if let Some(page) = try_ats(&http, &parsed).await? {
            if useful(&page) {
                return Ok(page);
            }
            best = page;
        }
    }

    let mut final_url = parsed.clone();
    let mut failure: Option<FetchError> = None;
    match http.get(parsed.clone()).send().await {
        Ok(r) if matches!(r.status(), StatusCode::NOT_FOUND | StatusCode::GONE) => {
            return Err(FetchError::Closed(format!(
                "This posting is gone (HTTP {}).",
                r.status().as_u16()
            )));
        }
        Ok(r) if r.status().is_success() => {
            final_url = r.url().clone();
            match r.text().await {
                Ok(body) => {
                    best = Some(from_html(&body))
                        .filter(|p| p.text.len() > best.text.len())
                        .unwrap_or(best)
                }
                Err(e) => failure = Some(e.into()),
            }
        }
        Ok(r) => {
            failure = Some(FetchError::Other(anyhow::anyhow!(
                "HTTP {} from {url}",
                r.status()
            )))
        }
        Err(e) => failure = Some(e.into()),
    }

    // JS-rendered page: read the posting from the ATS's public API instead.
    if !useful(&best) {
        let mut urls = vec![&final_url];
        if final_url != parsed {
            urls.push(&parsed);
        }
        for u in urls.into_iter().filter(|u| !(api_first && *u == &parsed)) {
            if let Some(page) = try_ats(&http, u)
                .await?
                .filter(|p| p.text.len() > best.text.len())
            {
                best = page;
                break;
            }
        }
    }
    match failure {
        Some(e) if best.text.is_empty() => Err(e),
        _ => Ok(best),
    }
}

/// ATS lookup where only a definite "closed" is an error; API hiccups fall back to HTML.
async fn try_ats(http: &reqwest::Client, u: &Url) -> Result<Option<Page>, FetchError> {
    match ats(http, u).await {
        Err(FetchError::Closed(m)) => Err(FetchError::Closed(m)),
        Ok(Some(p)) if !p.text.is_empty() => Ok(Some(p)),
        _ => Ok(None),
    }
}

// ---------- HTML -> text ----------

const ALWAYS_SKIP: &[&str] = &["script", "style", "noscript", "template", "head", "title"];
const CHROME: &[&str] = &["svg", "nav", "footer", "header", "iframe"];
#[rustfmt::skip]
const PARAGRAPH: &[&str] = &[
    "p", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "dl", "table", "blockquote", "pre", "section",
];
#[rustfmt::skip]
const BLOCK: &[&str] = &[
    "div", "article", "main", "aside", "li", "tr", "dt", "dd", "hr", "form", "fieldset", "figure",
    "figcaption", "address", "details", "summary", "body",
];

fn consent_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)cookie|consent|cybot|onetrust|gdpr|privacy-banner").unwrap())
}

fn is_consent(el: &scraper::node::Element) -> bool {
    if matches!(el.name(), "html" | "body" | "main" | "article") {
        return false;
    }
    let re = consent_re();
    el.id().is_some_and(|id| re.is_match(id)) || el.classes().any(|c| re.is_match(c))
}

fn walk(el: ElementRef, out: &mut String, strip_chrome: bool, pre: bool) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) if pre => out.push_str(t),
            Node::Text(t) => {
                for (i, w) in t.split_whitespace().enumerate() {
                    if i > 0 || t.starts_with(char::is_whitespace) {
                        out.push(' ');
                    }
                    out.push_str(w);
                }
                if t.ends_with(char::is_whitespace) && !t.trim().is_empty() {
                    out.push(' ');
                }
            }
            Node::Element(e) => {
                let name = e.name();
                if ALWAYS_SKIP.contains(&name)
                    || (strip_chrome && (CHROME.contains(&name) || is_consent(e)))
                {
                    continue;
                }
                let breaks = match name {
                    "br" => {
                        out.push('\n');
                        continue;
                    }
                    "td" | "th" => {
                        out.push(' ');
                        0
                    }
                    n if PARAGRAPH.contains(&n) => 2,
                    n if BLOCK.contains(&n) => 1,
                    _ => 0,
                };
                line_breaks(out, breaks);
                if name == "li" {
                    out.push_str("• ");
                }
                if let Some(child_el) = ElementRef::wrap(child) {
                    walk(child_el, out, strip_chrome, pre || name == "pre");
                }
                line_breaks(out, breaks);
            }
            _ => {}
        }
    }
}

/// Ensure `out` ends with at least `n` newlines (trailing spaces ignored).
fn line_breaks(out: &mut String, n: usize) {
    let have = out
        .trim_end_matches(' ')
        .chars()
        .rev()
        .take_while(|c| *c == '\n')
        .count();
    if !out.trim_end().is_empty() {
        out.extend(std::iter::repeat_n('\n', n.saturating_sub(have)));
    }
}

fn element_text(el: ElementRef, strip_chrome: bool) -> String {
    let mut out = String::new();
    walk(el, &mut out, strip_chrome, false);
    clean(&out)
}

/// HTML (fragment or document) to readable text, keeping paragraph and list breaks.
/// Also accepts entity-escaped HTML (`&lt;p&gt;...`) as some ATS APIs return it.
pub fn html_to_text(html: &str) -> String {
    let unescaped;
    let html = if html.contains("&lt;") && !html.contains('<') {
        unescaped = Html::parse_fragment(html)
            .root_element()
            .text()
            .collect::<String>();
        &unescaped
    } else {
        html
    };
    element_text(Html::parse_fragment(html).root_element(), false)
}

fn clean(text: &str) -> String {
    static RES: OnceLock<[Regex; 3]> = OnceLock::new();
    let [spaces, edges, blanks] = RES.get_or_init(|| {
        [
            Regex::new(r"[ \t\r\f\v\u{a0}\u{200b}]+").unwrap(),
            Regex::new(r" ?\n ?").unwrap(),
            Regex::new(r"\n\s*\n+").unwrap(),
        ]
    });
    let t = spaces.replace_all(text, " ");
    let t = edges.replace_all(&t, "\n");
    blanks.replace_all(&t, "\n\n").trim().to_string()
}

fn cap(mut text: String) -> String {
    if let Some((i, _)) = text.char_indices().nth(MAX_CHARS) {
        text.truncate(i);
    }
    text
}

fn make_page(title: &str, company: &str, location: &str, body: &str) -> Page {
    let header: Vec<&str> = [title, company, location]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    Page {
        title: title.trim().into(),
        company: company.trim().into(),
        location: location.trim().into(),
        text: cap(clean(&format!("{}\n\n{body}", header.join("\n")))),
    }
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

// ---------- page parsing ----------

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn name_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(x)) => x.trim().into(),
        Some(o @ Value::Object(_)) => s(o, "name").into(),
        Some(Value::Array(a)) => name_of(a.first()),
        _ => String::new(),
    }
}

fn find_posting(v: &Value) -> Option<&Value> {
    match v {
        Value::Array(a) => a.iter().find_map(find_posting),
        Value::Object(o) => {
            let is_posting = match o.get("@type") {
                Some(Value::String(t)) => t == "JobPosting",
                Some(Value::Array(ts)) => ts.iter().any(|t| t == "JobPosting"),
                _ => false,
            };
            if is_posting {
                Some(v)
            } else {
                o.get("@graph").and_then(find_posting)
            }
        }
        _ => None,
    }
}

fn jsonld_page(doc: &Html) -> Option<Page> {
    let posting = doc
        .select(&sel(r#"script[type="application/ld+json"]"#))
        .filter_map(|tag| serde_json::from_str::<Value>(tag.text().collect::<String>().trim()).ok())
        .find_map(|data| find_posting(&data).cloned())?;

    let loc = match posting.get("jobLocation") {
        Some(Value::Array(a)) => a.first(),
        other => other,
    };
    let location = match loc.and_then(|l| l.get("address")) {
        Some(Value::String(a)) => a.trim().to_string(),
        Some(addr) => [
            s(addr, "addressLocality").to_string(),
            name_of(addr.get("addressCountry")),
        ]
        .into_iter()
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join(", "),
        None if s(&posting, "jobLocationType") == "TELECOMMUTE" => "Remote".into(),
        None => String::new(),
    };
    Some(make_page(
        s(&posting, "title"),
        &name_of(posting.get("hiringOrganization")),
        &location,
        &html_to_text(s(&posting, "description")),
    ))
}

/// Description buried in `__NEXT_DATA__`, `application/json` or `window.__x = {...}` scripts.
fn embedded_page(doc: &Html) -> Option<Page> {
    static ASSIGN: OnceLock<Regex> = OnceLock::new();
    let assign = ASSIGN.get_or_init(|| Regex::new(r"window\.__\w+\s*=\s*").unwrap());
    let mut blobs = Vec::new();
    for tag in doc.select(&sel("script")) {
        let raw: String = tag.text().collect();
        let ty = tag.attr("type").unwrap_or("");
        if tag.attr("id") == Some("__NEXT_DATA__") || ty.contains("json") && !ty.contains("ld+json")
        {
            blobs.extend(serde_json::from_str::<Value>(raw.trim()).ok());
        } else if ty.is_empty() || ty.contains("javascript") {
            for m in assign.find_iter(&raw) {
                let mut it =
                    serde_json::Deserializer::from_str(&raw[m.end()..]).into_iter::<Value>();
                blobs.extend(it.next().and_then(Result::ok));
            }
        }
    }
    let mut best: Option<(usize, String, &Value)> = None;
    for blob in &blobs {
        longest_description(blob, &mut best);
    }
    let (_, desc, parent) = best?;
    let title = ["title", "jobTitle", "name"]
        .iter()
        .map(|k| s(parent, k))
        .find(|t| !t.is_empty());
    let company = [
        "companyName",
        "company",
        "organizationName",
        "hiringOrganization",
    ]
    .iter()
    .map(|k| name_of(parent.get(*k)))
    .find(|c| !c.is_empty())
    .unwrap_or_default();
    let location = ["location", "locationName", "jobLocation"]
        .iter()
        .map(|k| name_of(parent.get(*k)))
        .find(|c| !c.is_empty())
        .unwrap_or_default();
    Some(make_page(
        title.unwrap_or(""),
        &company,
        &location,
        &html_to_text(&desc),
    ))
}

fn longest_description<'a>(v: &'a Value, best: &mut Option<(usize, String, &'a Value)>) {
    static KEY: OnceLock<Regex> = OnceLock::new();
    let key_re = KEY.get_or_init(|| Regex::new(r"(?i)description|content|body").unwrap());
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                if let Value::String(text) = child {
                    if key_re.is_match(k)
                        && text.len() > 200
                        && best.as_ref().is_none_or(|b| text.len() > b.0)
                    {
                        *best = Some((text.len(), text.clone(), v));
                    }
                } else {
                    longest_description(child, best);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| longest_description(x, best)),
        _ => {}
    }
}

fn from_html(page_html: &str) -> Page {
    let doc = Html::parse_document(page_html);
    let mut out = Page::default();
    if let Some(p) = jsonld_page(&doc) {
        if p.text.chars().count() >= MIN_USEFUL_CHARS {
            return p;
        }
        out = p;
    }

    // Page chrome is skipped while walking. Not <form>: ASP.NET sites wrap the whole page in one.
    let text = ["main", "article", "body"]
        .iter()
        .filter_map(|t| doc.select(&sel(t)).next())
        .map(|el| element_text(el, true))
        .max_by_key(String::len)
        .unwrap_or_default();
    if text.len() > out.text.len() {
        out.text = cap(text);
    }
    if out.title.is_empty() {
        if let Some(t) = doc.select(&sel("title")).next() {
            out.title = clean(&t.text().collect::<String>());
        }
    }
    if out.text.chars().count() < MIN_USEFUL_CHARS {
        if let Some(p) = embedded_page(&doc).filter(|p| p.text.len() > out.text.len()) {
            let keep = |new: String, old: String| if new.is_empty() { old } else { new };
            out = Page {
                title: keep(p.title, out.title),
                company: keep(p.company, out.company),
                location: keep(p.location, out.location),
                text: p.text,
            };
        }
    }
    out
}

// ---------- JSON APIs ----------

async fn get_json(http: &reqwest::Client, url: &str) -> Result<Option<Value>, FetchError> {
    let r = http
        .get(url)
        .header(header::ACCEPT, "application/json")
        .send()
        .await?;
    if matches!(r.status(), StatusCode::NOT_FOUND | StatusCode::GONE) {
        return Ok(None);
    }
    Ok(Some(json_body(r.error_for_status()?).await?))
}

async fn json_body(r: reqwest::Response) -> Result<Value, FetchError> {
    serde_json::from_slice(&r.bytes().await?).map_err(|e| FetchError::Other(e.into()))
}

fn segments(u: &Url) -> Vec<&str> {
    u.path_segments()
        .map(|s| s.filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

fn workday_api_url(u: &Url) -> Option<String> {
    let host = u.host_str()?;
    if !host.ends_with("myworkdayjobs.com") {
        return None;
    }
    static LOCALE: OnceLock<Regex> = OnceLock::new();
    let locale = LOCALE.get_or_init(|| Regex::new(r"^[a-z]{2}-[A-Z]{2}$").unwrap());
    let parts: Vec<&str> = segments(u)
        .into_iter()
        .filter(|p| !locale.is_match(p))
        .collect();
    let job = parts.iter().position(|p| *p == "job")?;
    if parts.len() < 3 || job == 0 {
        return None;
    }
    let tenant = host.split('.').next()?;
    Some(format!(
        "https://{host}/wday/cxs/{tenant}/{}/{}",
        parts[0],
        parts[job..].join("/")
    ))
}

/// Workday renders postings client-side; read them from its JSON API instead.
async fn workday(http: &reqwest::Client, u: &Url) -> Result<Option<Page>, FetchError> {
    let Some(api) = workday_api_url(u) else {
        return Ok(None);
    };
    let Ok(r) = http
        .get(&api)
        .header(header::ACCEPT, "application/json")
        .send()
        .await
    else {
        return Ok(None);
    };
    if matches!(r.status().as_u16(), 403 | 404 | 410) {
        return Err(FetchError::Closed(
            "This posting is closed — Workday no longer serves it.".into(),
        ));
    }
    let Ok(data) = json_body(r).await else {
        return Ok(None);
    };
    let info = &data["jobPostingInfo"];
    if info["canApply"] == Value::Bool(false) {
        return Err(FetchError::Closed(
            "This posting no longer accepts applications.".into(),
        ));
    }
    Ok(Some(make_page(
        s(info, "title"),
        &name_of(data.get("hiringOrganization")),
        s(info, "location"),
        &html_to_text(s(info, "jobDescription")),
    )))
}

fn closed(ats: &str) -> FetchError {
    FetchError::Closed(format!(
        "This posting is closed — {ats} no longer lists it."
    ))
}

/// Public JSON APIs of common applicant tracking systems.
async fn ats(http: &reqwest::Client, u: &Url) -> Result<Option<Page>, FetchError> {
    let host = u.host_str().unwrap_or("");
    let seg = segments(u);
    let (Some(&org), Some(&id)) = (seg.first(), seg.get(1)) else {
        return Ok(None);
    };
    let id_after = |marker: &str| {
        seg.iter()
            .position(|p| *p == marker)
            .and_then(|i| seg.get(i + 1))
            .copied()
    };

    if host.ends_with("greenhouse.io") && !host.starts_with("boards-api") {
        let Some(job) = id_after("jobs") else {
            return Ok(None);
        };
        let api = format!("https://boards-api.greenhouse.io/v1/boards/{org}/jobs/{job}");
        let v = get_json(http, &api)
            .await?
            .ok_or_else(|| closed("Greenhouse"))?;
        return Ok(Some(make_page(
            s(&v, "title"),
            s(&v, "company_name"),
            &name_of(v.get("location")),
            &html_to_text(s(&v, "content")),
        )));
    }
    if host.ends_with("lever.co") && host.starts_with("jobs.") && id.len() == 36 {
        let api_host = host.replacen("jobs.", "api.", 1);
        let api = format!("https://{api_host}/v0/postings/{org}/{id}");
        let v = get_json(http, &api).await?.ok_or_else(|| closed("Lever"))?;
        let mut body = html_to_text(s(&v, "description"));
        for list in v["lists"].as_array().into_iter().flatten() {
            body += &format!(
                "\n\n{}\n{}",
                s(list, "text"),
                html_to_text(&format!("<ul>{}</ul>", s(list, "content")))
            );
        }
        body += &format!("\n\n{}", html_to_text(s(&v, "additional")));
        return Ok(Some(make_page(
            s(&v, "text"),
            org,
            s(&v["categories"], "location"),
            &body,
        )));
    }
    if host == "jobs.ashbyhq.com" {
        let api =
            format!("https://api.ashbyhq.com/posting-api/job-board/{org}?includeCompensation=true");
        let Some(v) = get_json(http, &api).await? else {
            return Ok(None);
        };
        let Some(job) = v["jobs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|j| s(j, "id") == id)
        else {
            return Ok(None);
        };
        let mut body = html_to_text(s(job, "descriptionHtml"));
        let pay = s(&job["compensation"], "compensationTierSummary");
        if !pay.is_empty() {
            body += &format!("\n\nCompensation: {pay}");
        }
        return Ok(Some(make_page(
            s(job, "title"),
            org,
            s(job, "location"),
            &body,
        )));
    }
    if host.ends_with("smartrecruiters.com") && host.starts_with("jobs.") {
        let job: String = id.chars().take_while(char::is_ascii_digit).collect();
        if job.is_empty() {
            return Ok(None);
        }
        let api = format!("https://api.smartrecruiters.com/v1/companies/{org}/postings/{job}");
        let v = get_json(http, &api)
            .await?
            .ok_or_else(|| closed("SmartRecruiters"))?;
        let sections = &v["jobAd"]["sections"];
        let body = [
            "companyDescription",
            "jobDescription",
            "qualifications",
            "additionalInformation",
        ]
        .iter()
        .map(|k| {
            format!(
                "{}\n{}",
                s(&sections[k], "title"),
                html_to_text(s(&sections[k], "text"))
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
        let loc = &v["location"];
        let location = match s(loc, "fullLocation") {
            "" => [s(loc, "city"), s(loc, "country")]
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
                .join(", "),
            full => full.to_string(),
        };
        return Ok(Some(make_page(
            s(&v, "name"),
            &name_of(v.get("company")),
            &location,
            &body,
        )));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filler(n: usize) -> String {
        (0..n)
            .map(|i| {
                format!(
                    "<p>Responsibility number {i}: build reliable systems that customers love.</p>"
                )
            })
            .collect()
    }

    const NAV: &str = "<header>Site Header</header><nav>Home | Jobs | About</nav>";
    const FOOT: &str = "<footer>Copyright Footer</footer>";

    #[test]
    fn jsonld_posting() {
        let desc = format!(
            "<p>About the role</p><ul><li>Write Rust</li><li>Ship apps</li></ul>{}",
            filler(15)
        );
        let ld = serde_json::json!({
            "@context": "https://schema.org", "@type": "JobPosting", "title": "Senior Rust Engineer",
            "hiringOrganization": {"@type": "Organization", "name": "Acme Corp"},
            "jobLocation": [{"@type": "Place", "address": {"addressLocality": "Berlin", "addressCountry": {"name": "DE"}}}],
            "description": desc,
        });
        let html = format!(
            r#"<html><head><title>Ignored</title><script type="application/ld+json">{ld}</script></head>
            <body>{NAV}<main>short</main>{FOOT}</body></html>"#
        );
        let p = from_html(&html);
        assert_eq!(
            (p.title.as_str(), p.company.as_str(), p.location.as_str()),
            ("Senior Rust Engineer", "Acme Corp", "Berlin, DE")
        );
        assert!(p.text.starts_with("Senior Rust Engineer\nAcme Corp\nBerlin, DE\n\nAbout the role\n\n• Write Rust\n• Ship apps"), "{}", p.text);
        assert!(!p.text.contains("Site Header"));
    }

    #[test]
    fn jsonld_graph_short_description_falls_back_to_body() {
        let ld = r#"{"@context":"https://schema.org","@graph":[{"@type":"WebPage","name":"x"},
            {"@type":["JobPosting"],"title":"Data Analyst","hiringOrganization":"Globex",
             "jobLocation":{"address":{"addressLocality":"Austin","addressCountry":"US"}},
             "description":"&lt;p&gt;Short teaser&lt;/p&gt;"}]}"#;
        let html = format!(
            r#"<html><head><script type="application/ld+json">{ld}</script></head>
            <body>{NAV}<div id="content"><h1>Data Analyst</h1>{}</div>{FOOT}</body></html>"#,
            filler(15)
        );
        let p = from_html(&html);
        assert_eq!(
            (p.title.as_str(), p.company.as_str(), p.location.as_str()),
            ("Data Analyst", "Globex", "Austin, US")
        );
        assert!(p.text.contains("Responsibility number 14"));
        assert!(
            !p.text.contains("Short teaser")
                && !p.text.contains("Home | Jobs")
                && !p.text.contains("Copyright")
        );
    }

    #[test]
    fn consent_banners_removed_but_form_kept() {
        let html = format!(
            r#"<html><head><title> Platform Engineer | Initech </title><style>p{{}}</style></head>
            <body class="has-cookie-banner"><form id="aspnetForm">{NAV}
            <div id="onetrust-banner-sdk">We use cookies to improve your experience</div>
            <div class="CybotCookiebotDialog">Allow all cookies</div>
            <section class="gdpr-notice"><p>GDPR text</p></section>
            <main><h2>Platform Engineer</h2>{}<iframe>frame</iframe><svg><text>icon</text></svg></main>
            <script>var x = "script text";</script>{FOOT}</form></body></html>"#,
            filler(3)
        );
        let p = from_html(&html);
        assert_eq!(p.title, "Platform Engineer | Initech");
        assert!(
            p.text
                .starts_with("Platform Engineer\n\nResponsibility number 0"),
            "{}",
            p.text
        );
        for bad in [
            "cookies",
            "GDPR",
            "script text",
            "frame",
            "icon",
            "Site Header",
            "Copyright",
        ] {
            assert!(!p.text.contains(bad), "{bad} leaked: {}", p.text);
        }
    }

    #[test]
    fn plain_article_page() {
        let html = "<html><head><title>Barista</title></head><body><div class=\"sidebar\">Other jobs</div>\
            <article><h1>Barista</h1><p>Make   coffee\n  for <b>great</b> people.</p><p>Second<br>line</p></article></body></html>";
        let p = from_html(html);
        assert_eq!(p.title, "Barista");
        // body wins (longest), still keeps paragraph breaks and inline words
        assert_eq!(
            p.text,
            "Other jobs\n\nBarista\n\nMake coffee for great people.\n\nSecond\nline"
        );
    }

    #[test]
    fn next_data_blob() {
        let desc = format!("<p>Intro</p>{}", filler(15));
        let data = serde_json::json!({"props": {"pageProps": {"job": {
            "title": "Designer", "companyName": "Hooli", "location": {"name": "Remote"}, "descriptionHtml": desc}}}});
        let html = format!(
            r#"<html><body><div id="__next">Loading…</div>
            <script id="__NEXT_DATA__" type="application/json">{data}</script></body></html>"#
        );
        let p = from_html(&html);
        assert_eq!(
            (p.title.as_str(), p.company.as_str(), p.location.as_str()),
            ("Designer", "Hooli", "Remote")
        );
        assert!(
            p.text
                .contains("Designer\nHooli\nRemote\n\nIntro\n\nResponsibility number 0"),
            "{}",
            p.text
        );

        let html =
            format!(r#"<html><body><script>window.__appData = {data};</script></body></html>"#);
        assert_eq!(from_html(&html).title, "Designer");
    }

    #[test]
    fn html_to_text_escaped_and_lists() {
        assert_eq!(html_to_text("&lt;h2&gt;Perks&lt;/h2&gt;&lt;ul&gt;&lt;li&gt;Tea &amp;amp; cake&lt;/li&gt;&lt;/ul&gt;"), "Perks\n\n• Tea & cake");
        assert_eq!(
            html_to_text("<ol><li>One</li><li>Two</li></ol><p>A &amp; B</p>"),
            "• One\n• Two\n\nA & B"
        );
    }

    #[test]
    fn caps_text() {
        let p = make_page("t", "", "", &"é".repeat(40_000));
        assert_eq!(p.text.chars().count(), MAX_CHARS);
    }

    #[test]
    fn workday_url() {
        let u = Url::parse(
            "https://acme.wd5.myworkdayjobs.com/en-US/External/job/Austin-TX/Engineer_R123?src=x",
        )
        .unwrap();
        assert_eq!(
            workday_api_url(&u).unwrap(),
            "https://acme.wd5.myworkdayjobs.com/wday/cxs/acme/External/job/Austin-TX/Engineer_R123"
        );
        assert!(workday_api_url(
            &Url::parse("https://acme.wd5.myworkdayjobs.com/External").unwrap()
        )
        .is_none());
    }

    #[tokio::test]
    async fn rejects_non_http() {
        for u in ["file:///etc/passwd", "ftp://example.com/x", "not a url"] {
            assert!(
                matches!(fetch_job(u).await, Err(FetchError::Unsafe(_))),
                "{u}"
            );
        }
    }

    async fn first_job(api: &str, pick: impl Fn(&Value) -> Option<String>) -> String {
        let v = get_json(&client(), api).await.unwrap().unwrap();
        pick(&v).expect("no live job found")
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn live_postings() {
        let gh = first_job(
            "https://boards-api.greenhouse.io/v1/boards/anthropic/jobs",
            |v| v["jobs"][0]["absolute_url"].as_str().map(String::from),
        )
        .await;
        let lever = first_job("https://api.lever.co/v0/postings/palantir?limit=1", |v| {
            v[0]["hostedUrl"].as_str().map(String::from)
        })
        .await;
        let ashby = first_job(
            "https://api.ashbyhq.com/posting-api/job-board/openai",
            |v| v["jobs"][0]["jobUrl"].as_str().map(String::from),
        )
        .await;
        let sr = first_job(
            "https://api.smartrecruiters.com/v1/companies/BoschGroup/postings?limit=1",
            |v| {
                v["content"][0]["id"]
                    .as_str()
                    .map(|id| format!("https://jobs.smartrecruiters.com/BoschGroup/{id}"))
            },
        )
        .await;
        let wd_site =
            "https://nvidia.wd5.myworkdayjobs.com/wday/cxs/nvidia/NVIDIAExternalCareerSite/jobs";
        let r = client()
            .post(wd_site)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(r#"{"limit":1,"offset":0,"appliedFacets":{},"searchText":""}"#)
            .send()
            .await
            .unwrap();
        let wd = format!(
            "https://nvidia.wd5.myworkdayjobs.com/en-US/NVIDIAExternalCareerSite{}",
            json_body(r).await.unwrap()["jobPostings"][0]["externalPath"]
                .as_str()
                .unwrap()
        );
        for url in [
            gh.as_str(),
            lever.as_str(),
            ashby.as_str(),
            sr.as_str(),
            wd.as_str(),
        ] {
            let p = fetch_job(url)
                .await
                .unwrap_or_else(|e| panic!("{url}: {e}"));
            println!(
                "{url}\n  title={:?} company={:?} location={:?} chars={}\n  {:?}\n",
                p.title,
                p.company,
                p.location,
                p.text.chars().count(),
                p.text.chars().take(160).collect::<String>()
            );
            assert!(
                p.text.chars().count() >= MIN_USEFUL_CHARS && !p.title.is_empty(),
                "{url}"
            );
        }
        // ATS APIs directly, even where the HTML page alone would have sufficed.
        let http = client();
        for url in [gh.as_str(), lever.as_str(), ashby.as_str()] {
            let p = ats(&http, &Url::parse(url).unwrap())
                .await
                .unwrap()
                .expect(url);
            println!(
                "API {url}: title={:?} company={:?} location={:?} chars={}",
                p.title,
                p.company,
                p.location,
                p.text.len()
            );
            assert!(p.text.chars().count() >= MIN_USEFUL_CHARS, "{url}");
        }
        let gone = format!("{}9999", gh.trim_end_matches(|c: char| c.is_ascii_digit()));
        assert!(
            matches!(fetch_job(&gone).await, Err(FetchError::Closed(_))),
            "{gone}"
        );
    }
}
