//! Deterministic ATS check, run against the text extracted from the real PDF.
//!
//! This is what an ATS sees: if a keyword doesn't survive PDF text extraction it
//! doesn't count, whatever the resume JSON says.
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::LazyLock;

const LIGATURES: [char; 5] = ['ﬀ', 'ﬁ', 'ﬂ', 'ﬃ', 'ﬄ'];
const SECTION_HEADERS: [&str; 4] = ["professional summary", "work experience", "education", "skills"];
const STOP: [&str; 19] = ["and", "with", "for", "the", "of", "in", "to", "on", "using", "based", "data", "systems", "system",
    "experience", "engineering", "development", "management", "skills", "tools"];

#[derive(Clone, Debug, Default)]
pub struct Keyword {
    pub term: String,
    pub required: bool,
    pub aliases: Vec<String>,
}

impl Keyword {
    pub fn from_jd(jd: &Value) -> Vec<Keyword> {
        jd["keywords"].as_array().into_iter().flatten().filter_map(|k| {
            let term = k["term"].as_str()?.trim().to_string();
            (!term.is_empty()).then(|| Keyword {
                required: !k["importance"].as_str().unwrap_or("").to_lowercase().starts_with("pref"),
                aliases: crate::resume::strs(&k["aliases"]),
                term,
            })
        }).collect()
    }

    fn terms(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.term).chain(self.aliases.iter())
    }
}

/// Lowercase, '&' to 'and', dashes/slashes/underscores to spaces, collapsed whitespace.
pub fn norm(text: &str) -> String {
    let lower = text.to_lowercase().replace('&', " and ");
    let mapped: String = lower.chars().map(|c| if matches!(c, '‐'..='―' | '/' | '_' | '-') { ' ' } else { c }).collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whole-term match that tolerates symbols in terms like "c++" or "node.js" and simple plurals.
pub fn has_term(norm_text: &str, term: &str) -> bool {
    let t = norm(term);
    if t.is_empty() {
        return false;
    }
    let is_word = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut from = 0;
    while let Some(pos) = norm_text[from..].find(&t) {
        let start = from + pos;
        let mut end = start + t.len();
        let before_ok = norm_text[..start].chars().next_back().is_none_or(|c| !is_word(c));
        let rest = &norm_text[end..];
        if rest.starts_with("es") && rest[2..].chars().next().is_none_or(|c| !is_word(c)) {
            end += 2;
        } else if rest.starts_with('s') && rest[1..].chars().next().is_none_or(|c| !is_word(c)) {
            end += 1;
        }
        let after_ok = norm_text[end..].chars().next().is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + norm_text[start..].chars().next().map(char::len_utf8).unwrap_or(1);
    }
    false
}

static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9+#.]{4,}").unwrap());
static EVIDENCE_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9+#]{3,}").unwrap());
static NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b\d+(?:[.,]\d+)?\s?(?:%|\+|k\b|x\b|m\b)").unwrap());

/// Does the candidate's evidence show this keyword? (term/alias, or a significant word of it.)
pub fn cv_supports(kw: &Keyword, base_norm: &str) -> bool {
    if kw.terms().any(|t| has_term(base_norm, t)) {
        return true;
    }
    kw.terms()
        .flat_map(|t| WORD.find_iter(&norm(t)).map(|m| m.as_str().to_string()).collect::<Vec<_>>())
        .filter(|w| !STOP.contains(&w.as_str()))
        .any(|w| has_term(base_norm, &w))
}

/// The agent's quoted evidence must really be in the CV (≥80% of its words).
pub fn evidence_in_cv(evidence: &str, base_norm: &str) -> bool {
    let n = norm(evidence);
    let words: Vec<&str> = EVIDENCE_WORD.find_iter(&n).map(|m| m.as_str()).collect();
    if words.is_empty() {
        return false;
    }
    if base_norm.contains(&n) {
        return true;
    }
    words.iter().filter(|w| has_term(base_norm, w)).count() as f32 / words.len() as f32 >= 0.8
}

/// Metrics in the resume that never appear in the candidate's sources (likely invented).
pub fn unsupported_numbers(resume_text: &str, base_text: &str) -> Vec<String> {
    let key = |m: &str| m.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let base: Vec<String> = NUM.find_iter(base_text).map(|m| key(m.as_str())).collect();
    let mut out: Vec<String> = Vec::new();
    for m in NUM.find_iter(resume_text) {
        let k = key(m.as_str());
        if !base.contains(&k) && !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

/// The full ATS report as JSON (stored with each resume version and shown in the app).
pub fn check(pdf_text: &str, pages: usize, keywords: &[Keyword], evidence: &str, email: &str, gaps: &[String],
             support: Option<&HashMap<String, bool>>) -> Value {
    let text_norm = norm(pdf_text);
    let (matched, missing): (Vec<&Keyword>, Vec<&Keyword>) =
        keywords.iter().partition(|k| k.terms().any(|t| has_term(&text_norm, t)));
    let weight = |k: &&Keyword| if k.required { 2 } else { 1 };
    let total: i32 = keywords.iter().map(|k| weight(&k)).sum::<i32>().max(1);
    let coverage = (100.0 * matched.iter().map(weight).sum::<i32>() as f32 / total as f32).round() as i64;

    let chars = pdf_text.trim().chars().count();
    let missing_headers: Vec<String> = SECTION_HEADERS.iter().filter(|h| !text_norm.contains(*h)).map(|h| title_case(h)).collect();
    let checks = [
        ("Text is machine-readable", chars > 800, format!("{chars} characters extracted from the PDF")),
        ("Standard section headers", missing_headers.is_empty(),
         if missing_headers.is_empty() { "All present".into() } else { format!("Missing: {}", missing_headers.join(", ")) }),
        ("Contact email parseable", !email.is_empty() && pdf_text.to_lowercase().contains(&email.to_lowercase()),
         if email.is_empty() { "No email in the profile".into() } else { email.to_string() }),
        ("No ligature glyphs", !pdf_text.chars().any(|c| LIGATURES.contains(&c)), "Keywords like 'workflow' extract cleanly".into()),
        ("Length ≤ 2 pages", pages <= 2, format!("{pages} page(s)")),
    ];
    let structure = (100.0 * checks.iter().filter(|c| c.1).count() as f32 / checks.len() as f32).round() as i64;

    let gap_set: Vec<String> = gaps.iter().map(|g| g.to_lowercase()).collect();
    let base_norm = norm(evidence);
    let supported = |k: &Keyword| match support.and_then(|s| s.get(&k.term)) {
        Some(ok) => *ok,
        None => cv_supports(k, &base_norm) && !gap_set.contains(&k.term.to_lowercase()),
    };
    let terms = |ks: Vec<&&Keyword>| ks.iter().map(|k| k.term.clone()).collect::<Vec<_>>();

    json!({
        "score": (0.8 * coverage as f32 + 0.2 * structure as f32).round() as i64,
        "coverage": coverage,
        "structure": structure,
        "matched": terms(matched.iter().collect()),
        // missing but backed by the candidate's sources: the tailor can legitimately work these in
        "missing": terms(missing.iter().filter(|k| supported(k)).collect()),
        // missing and not backed: genuine gaps, never claimed
        "missing_gaps": terms(missing.iter().filter(|k| !supported(k)).collect()),
        // in the resume but not backed: likely fabricated, must be removed
        "unsupported_keywords": terms(matched.iter().filter(|k| !supported(k)).collect()),
        "checks": checks.iter().map(|(n, ok, d)| json!({"name": n, "ok": ok, "detail": d})).collect::<Vec<_>>(),
        "unsupported_numbers": unsupported_numbers(pdf_text, evidence),
        "pages": pages,
    })
}

fn title_case(s: &str) -> String {
    s.split(' ').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kw(t: &str) -> Keyword {
        Keyword { term: t.into(), required: true, aliases: vec![] }
    }

    #[test]
    fn term_matching() {
        let t = norm("Built CI/CD pipelines in C++ and Node.js; Kubernetes clusters; workflow-automation");
        assert!(has_term(&t, "ci cd"));
        assert!(has_term(&t, "C++"));
        assert!(has_term(&t, "node.js"));
        assert!(has_term(&t, "Kubernetes cluster"));
        assert!(has_term(&t, "workflow automation"));
        assert!(!has_term(&norm("Storage systems"), "RAG"));
    }

    #[test]
    fn report_flags_invented_numbers_and_unsupported_terms() {
        let cv = "Data scientist. Built RAG with 40% faster retrieval. Python.";
        let pdf = "Professional Summary Work Experience Education Skills a@b.c RAG Python Kubernetes 70% faster";
        let r = check(pdf, 1, &[kw("RAG"), kw("Python"), kw("Kubernetes"), kw("Go")], cv, "a@b.c", &[], None);
        assert_eq!(r["unsupported_keywords"], json!(["Kubernetes"]));
        assert_eq!(r["unsupported_numbers"], json!(["70%"]));
        assert_eq!(r["missing_gaps"], json!(["Go"]));
        assert_eq!(r["structure"], 80); // text shorter than 800 chars
    }
}
