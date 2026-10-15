//! Suggestions for the profile's pickers: complete country and language lists bundled with the
//! app (instant, offline), plus live lookups for cities (Open-Meteo geocoding) and job titles and
//! skills (the EU's ESCO classification). Both services are free and need no account.
use serde_json::Value;
use std::time::Duration;

pub const COUNTRIES: &[&str] = &[
    "Remote", "Afghanistan", "Albania", "Algeria", "Andorra", "Angola", "Antigua and Barbuda", "Argentina", "Armenia",
    "Australia", "Austria", "Azerbaijan", "Bahamas", "Bahrain", "Bangladesh", "Barbados", "Belarus", "Belgium", "Belize",
    "Benin", "Bhutan", "Bolivia", "Bosnia and Herzegovina", "Botswana", "Brazil", "Brunei", "Bulgaria", "Burkina Faso",
    "Burundi", "Cabo Verde", "Cambodia", "Cameroon", "Canada", "Central African Republic", "Chad", "Chile", "China",
    "Colombia", "Comoros", "Congo", "Costa Rica", "Côte d'Ivoire", "Croatia", "Cuba", "Cyprus", "Czechia",
    "Democratic Republic of the Congo", "Denmark", "Djibouti", "Dominica", "Dominican Republic", "Ecuador", "Egypt",
    "El Salvador", "Equatorial Guinea", "Eritrea", "Estonia", "Eswatini", "Ethiopia", "Fiji", "Finland", "France", "Gabon",
    "Gambia", "Georgia", "Germany", "Ghana", "Greece", "Grenada", "Guatemala", "Guinea", "Guinea-Bissau", "Guyana", "Haiti",
    "Honduras", "Hong Kong", "Hungary", "Iceland", "India", "Indonesia", "Iran", "Iraq", "Ireland", "Israel", "Italy",
    "Jamaica", "Japan", "Jordan", "Kazakhstan", "Kenya", "Kiribati", "Kosovo", "Kuwait", "Kyrgyzstan", "Laos", "Latvia",
    "Lebanon", "Lesotho", "Liberia", "Libya", "Liechtenstein", "Lithuania", "Luxembourg", "Macao", "Madagascar", "Malawi",
    "Malaysia", "Maldives", "Mali", "Malta", "Marshall Islands", "Mauritania", "Mauritius", "Mexico", "Micronesia",
    "Moldova", "Monaco", "Mongolia", "Montenegro", "Morocco", "Mozambique", "Myanmar", "Namibia", "Nauru", "Nepal",
    "Netherlands", "New Zealand", "Nicaragua", "Niger", "Nigeria", "North Korea", "North Macedonia", "Norway", "Oman",
    "Pakistan", "Palau", "Palestine", "Panama", "Papua New Guinea", "Paraguay", "Peru", "Philippines", "Poland", "Portugal",
    "Puerto Rico", "Qatar", "Romania", "Russia", "Rwanda", "Saint Kitts and Nevis", "Saint Lucia",
    "Saint Vincent and the Grenadines", "Samoa", "San Marino", "São Tomé and Príncipe", "Saudi Arabia", "Senegal", "Serbia",
    "Seychelles", "Sierra Leone", "Singapore", "Slovakia", "Slovenia", "Solomon Islands", "Somalia", "South Africa",
    "South Korea", "South Sudan", "Spain", "Sri Lanka", "Sudan", "Suriname", "Sweden", "Switzerland", "Syria", "Taiwan",
    "Tajikistan", "Tanzania", "Thailand", "Timor-Leste", "Togo", "Tonga", "Trinidad and Tobago", "Tunisia", "Turkey",
    "Turkmenistan", "Tuvalu", "Uganda", "Ukraine", "United Arab Emirates", "United Kingdom", "United States", "Uruguay",
    "Uzbekistan", "Vanuatu", "Vatican City", "Venezuela", "Vietnam", "Yemen", "Zambia", "Zimbabwe",
];

/// Common titles shown instantly; ESCO adds the rest as the user types.
pub const ROLES: &[&str] = &[
    "Data Scientist", "Senior Data Scientist", "Machine Learning Engineer", "ML Engineer", "AI Engineer",
    "Applied Scientist", "Research Scientist", "Applied AI Engineer", "GenAI Engineer", "LLM Engineer", "MLOps Engineer",
    "Data Engineer", "Analytics Engineer", "Data Analyst", "Software Engineer", "Backend Engineer", "Frontend Engineer",
    "Full Stack Engineer", "Platform Engineer", "DevOps Engineer", "Site Reliability Engineer", "Product Manager",
    "Technical Product Manager", "Engineering Manager", "Solutions Architect", "Forward Deployed Engineer",
    "Product Designer", "UX Designer", "Business Analyst",
];

const LANGUAGE_NAMES: &[&str] = &[
    "Afrikaans", "Albanian", "Amharic", "Arabic", "Armenian", "Azerbaijani", "Basque", "Belarusian", "Bengali", "Bosnian",
    "Bulgarian", "Burmese", "Cantonese", "Catalan", "Croatian", "Czech", "Danish", "Dutch", "English", "Estonian",
    "Filipino", "Finnish", "French", "Galician", "Georgian", "German", "Greek", "Gujarati", "Hausa", "Hebrew", "Hindi",
    "Hungarian", "Icelandic", "Igbo", "Indonesian", "Irish", "Italian", "Japanese", "Kannada", "Kazakh", "Khmer", "Korean",
    "Kurdish", "Lao", "Latvian", "Lithuanian", "Luxembourgish", "Macedonian", "Malay", "Malayalam", "Maltese", "Mandarin",
    "Marathi", "Mongolian", "Nepali", "Norwegian", "Pashto", "Persian", "Polish", "Portuguese", "Punjabi", "Romanian",
    "Russian", "Serbian", "Sinhala", "Slovak", "Slovenian", "Somali", "Spanish", "Swahili", "Swedish", "Tagalog", "Tamil",
    "Telugu", "Thai", "Turkish", "Ukrainian", "Urdu", "Uzbek", "Vietnamese", "Welsh", "Yoruba", "Zulu",
];

const LEVELS: [&str; 4] = ["native", "fluent", "professional", "basic"];

/// Bundled suggestions for a picker, instantly: prefix matches first, minus ones already chosen.
pub fn local(field: &str, typed: &str, chosen: &[String]) -> Vec<String> {
    let t = typed.trim().to_lowercase();
    if t.is_empty() {
        return vec![];
    }
    let languages: Vec<String>;
    let list: Vec<&str> = match field {
        "countries" | "authorized-in" | "avoid-places" => COUNTRIES.to_vec(),
        "target-roles" => ROLES.to_vec(),
        "languages" => {
            // "dutch" offers "Dutch (native)", "Dutch (fluent)", ...
            let base = t.split('(').next().unwrap_or("").trim().to_string();
            languages = LANGUAGE_NAMES.iter().filter(|l| l.to_lowercase().starts_with(&base))
                .flat_map(|l| LEVELS.iter().map(move |lv| format!("{l} ({lv})"))).collect();
            languages.iter().map(String::as_str).collect()
        }
        _ => vec![],
    };
    let fresh = |s: &&&str| !chosen.iter().any(|c| c.eq_ignore_ascii_case(s));
    let mut out: Vec<String> = list.iter().filter(fresh).filter(|s| s.to_lowercase().starts_with(&t)).map(|s| s.to_string()).collect();
    if field != "languages" {
        out.extend(list.iter().filter(fresh).filter(|s| !s.to_lowercase().starts_with(&t) && s.to_lowercase().contains(&t)).map(|s| s.to_string()));
    }
    out.truncate(8);
    out
}

/// Whether a picker also asks an online service.
pub fn has_remote(field: &str) -> bool {
    matches!(field, "cities" | "target-roles" | "strengths" | "avoid-places")
}

fn title_case(s: &str) -> String {
    s.split(' ').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" ")
}

/// Live suggestions: cities from Open-Meteo, job titles and skills from ESCO. Empty on any failure.
pub async fn remote(field: &str, typed: &str) -> Vec<String> {
    let q = typed.trim();
    if q.chars().count() < 2 {
        return vec![];
    }
    // A plain client: the page fetcher's browser-style Accept header makes ESCO answer 406.
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| reqwest::Client::builder().user_agent("JobCommandCenter/0.1").no_brotli().no_gzip().no_deflate().build().unwrap_or_default());
    let get = |url: url::Url| {
        let req = client.get(url).header(reqwest::header::ACCEPT, "application/json").timeout(Duration::from_secs(5));
        async move { serde_json::from_slice::<Value>(&req.send().await.ok()?.bytes().await.ok()?).ok() }
    };
    match field {
        "cities" | "avoid-places" => {
            let Ok(url) = url::Url::parse_with_params("https://geocoding-api.open-meteo.com/v1/search",
                &[("name", q), ("count", "8"), ("language", "en"), ("format", "json")]) else { return vec![] };
            let Some(v) = get(url).await else { return vec![] };
            let mut out: Vec<String> = Vec::new();
            for r in v["results"].as_array().into_iter().flatten() {
                let name = r["name"].as_str().unwrap_or("");
                let country = r["country"].as_str().unwrap_or("");
                let label = if country.is_empty() { name.to_string() } else { format!("{name}, {country}") };
                if !name.is_empty() && !out.contains(&label) {
                    out.push(label);
                }
            }
            out
        }
        "target-roles" | "strengths" => {
            let kind = if field == "target-roles" { "occupation" } else { "skill" };
            let Ok(url) = url::Url::parse_with_params("https://ec.europa.eu/esco/api/search",
                &[("text", q), ("type", kind), ("language", "en"), ("limit", "20")]) else { return vec![] };
            let Some(v) = get(url).await else { return vec![] };
            // ESCO ranks loosely related titles too; keep the ones that contain what was typed.
            let words: Vec<String> = q.to_lowercase().split_whitespace().map(String::from).collect();
            v["_embedded"]["results"].as_array().into_iter().flatten()
                .filter_map(|r| r["title"].as_str())
                .filter(|t| { let t = t.to_lowercase(); words.iter().all(|w| t.contains(w.as_str())) })
                .map(|t| if kind == "occupation" { title_case(t) } else { t[..1].to_uppercase() + &t[1..] })
                .collect()
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_lists() {
        assert_eq!(local("countries", "neth", &[])[0], "Netherlands");
        assert!(local("countries", "neth", &["Netherlands".into()]).is_empty());
        assert_eq!(local("languages", "dut", &[])[0], "Dutch (native)");
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn live_lookups() {
        let cities = remote("cities", "Rotterd").await;
        println!("{cities:?}");
        assert!(cities.iter().any(|c| c.starts_with("Rotterdam")));
        let roles = remote("target-roles", "data scien").await;
        println!("{roles:?}");
        assert!(roles.iter().any(|r| r == "Data Scientist"));
        println!("{:?}", remote("strengths", "machine learn").await);
    }
}
