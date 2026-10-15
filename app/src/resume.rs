//! The tailored resume: what the Tailor agent returns and every PDF template renders.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Resume {
    pub summary: String,
    pub competencies: Vec<String>,
    pub experience: Vec<Experience>,
    pub projects: Vec<Project>,
    pub education: Vec<Education>,
    pub certifications: Vec<Certification>,
    pub skills: Vec<SkillGroup>,
    /// What the agent changed and why (shown in the app, never printed).
    pub changes: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Experience {
    pub company: String,
    pub role: String,
    pub location: String,
    pub dates: String,
    pub bullets: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Project {
    pub name: String,
    pub description: String,
    pub tech: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Education {
    pub title: String,
    pub org: String,
    pub year: String,
    pub description: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Certification {
    pub title: String,
    pub org: String,
    pub year: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SkillGroup {
    pub category: String,
    pub items: Vec<String>,
}

/// The name and contact line printed at the top of every template.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Header {
    pub name: String,
    pub headline: String,
    /// In print order: phone, email, links, location. `url` is empty for plain text.
    pub contacts: Vec<Contact>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Contact {
    pub text: String,
    pub url: String,
}

fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A string or a list of strings, as models return either.
pub fn strs(v: &Value) -> Vec<String> {
    match v {
        Value::String(x) if !x.trim().is_empty() => vec![x.trim().to_string()],
        Value::Array(a) => a.iter().map(s).filter(|x| !x.is_empty()).collect(),
        _ => vec![],
    }
}

impl Resume {
    /// Lenient parse of a model reply: accepts loosely typed fields, rejects a draft without the essentials.
    pub fn from_model(r: &Value) -> anyhow::Result<Resume> {
        let list = |k: &str| r[k].as_array().cloned().unwrap_or_default();
        let resume = Resume {
            summary: s(&r["summary"]),
            competencies: strs(&r["competencies"]).into_iter().take(10).collect(),
            experience: list("experience").iter().filter(|e| e.is_object()).map(|e| Experience {
                company: s(&e["company"]), role: s(&e["role"]), location: s(&e["location"]),
                dates: s(&e["dates"]), bullets: strs(&e["bullets"]),
            }).collect(),
            projects: list("projects").iter().filter(|e| e.is_object()).map(|e| Project {
                name: s(&e["name"]), description: s(&e["description"]), tech: s(&e["tech"]),
            }).collect(),
            education: list("education").iter().filter(|e| e.is_object()).map(|e| Education {
                title: s(&e["title"]), org: s(&e["org"]), year: s(&e["year"]), description: s(&e["description"]),
            }).collect(),
            certifications: list("certifications").iter().filter(|e| e.is_object()).map(|e| Certification {
                title: s(&e["title"]), org: s(&e["org"]), year: s(&e["year"]),
            }).collect(),
            skills: list("skills").iter().filter(|e| e.is_object()).map(|e| SkillGroup {
                category: s(&e["category"]), items: strs(&e["items"]),
            }).collect(),
            changes: strs(&r["changes"]),
        };
        if resume.summary.is_empty() || resume.experience.is_empty() {
            anyhow::bail!("the tailored resume is missing its summary or experience");
        }
        Ok(resume)
    }

    /// The draft without the agent's change notes, for feeding back to a model.
    pub fn for_prompt(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() { o.remove("changes"); }
        v
    }
}
