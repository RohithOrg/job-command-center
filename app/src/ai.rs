//! One chat + embeddings interface over every AI provider a user can connect.
//!
//! Each vendor is called over its own HTTP API (OpenAI, Anthropic, Gemini); anything
//! OpenAI-compatible (Ollama, LM Studio, OpenRouter, Groq, a private proxy) shares one
//! client. API keys live in the operating system's keychain, never in the database.
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    OpenAi,
    Anthropic,
    Gemini,
    /// POST {base_url}/chat/completions, any vendor.
    Compat,
}

pub struct Provider {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub base_url: &'static str,
    pub needs_key: bool,
    pub default_model: &'static str,
    /// Embedding model used for document search when this provider is chosen; "" = none offered.
    pub key_hint: &'static str,
    pub key_url: &'static str,
}

pub const PROVIDERS: &[Provider] = &[
    Provider { id: "anthropic", label: "Anthropic (Claude)", kind: Kind::Anthropic, base_url: "https://api.anthropic.com/v1",
               needs_key: true, default_model: "claude-opus-5-5", key_hint: "sk-ant-…",
               key_url: "https://console.anthropic.com/settings/keys" },
    Provider { id: "openai", label: "OpenAI", kind: Kind::OpenAi, base_url: "https://api.openai.com/v1",
               needs_key: true, default_model: "gpt-5.5", key_hint: "sk-…",
               key_url: "https://platform.openai.com/api-keys" },
    Provider { id: "gemini", label: "Google Gemini", kind: Kind::Gemini, base_url: "https://generativelanguage.googleapis.com/v1beta",
               needs_key: true, default_model: "gemini-3.5-flash", key_hint: "AIza…",
               key_url: "https://aistudio.google.com/apikey" },
    Provider { id: "openrouter", label: "OpenRouter", kind: Kind::Compat, base_url: "https://openrouter.ai/api/v1",
               needs_key: true, default_model: "", key_hint: "sk-or-…", key_url: "https://openrouter.ai/keys" },
    Provider { id: "groq", label: "Groq", kind: Kind::Compat, base_url: "https://api.groq.com/openai/v1",
               needs_key: true, default_model: "", key_hint: "gsk_…", key_url: "https://console.groq.com/keys" },
    Provider { id: "ollama", label: "Ollama (on this computer)", kind: Kind::Compat, base_url: "http://localhost:11434/v1",
               needs_key: false, default_model: "", key_hint: "", key_url: "" },
    Provider { id: "lmstudio", label: "LM Studio (on this computer)", kind: Kind::Compat, base_url: "http://localhost:1234/v1",
               needs_key: false, default_model: "", key_hint: "", key_url: "" },
    Provider { id: "compat", label: "Custom OpenAI-compatible URL", kind: Kind::Compat, base_url: "",
               needs_key: false, default_model: "", key_hint: "", key_url: "" },
];

pub fn provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

// Claude models that support Anthropic's server-side refusal fallback.
const CLAUDE_FALLBACK_MODELS: [&str; 5] = ["claude-fable-5-1", "claude-fable-5", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];

/// What the user chose in Settings (stored as the "ai" setting; the key is in the keychain).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiSettings {
    pub provider: String,
    /// Tried in order; the first is the main model, the rest are fallbacks.
    pub models: Vec<String>,
    pub base_url: String,
    /// Embedding model for document search; "" turns semantic search off (keyword search still works).
    pub embed_model: String,
    /// Where embeddings come from when not the chat provider (e.g. local Ollama next to a hosted chat model).
    pub embed_provider: String,
    pub embed_base_url: String,
}

impl AiSettings {
    pub fn model(&self) -> &str {
        self.models.first().map(String::as_str).unwrap_or("")
    }
}

// ---------- API keys in the OS keychain ----------

const KEYCHAIN_SERVICE: &str = "Job Command Center";

pub fn save_key(provider: &str, key: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, provider)?;
    if key.is_empty() {
        let _ = entry.delete_credential();
        return Ok(());
    }
    entry.set_password(key).context("couldn't save the key in this computer's keychain")
}

pub fn load_key(provider: &str) -> String {
    keyring::Entry::new(KEYCHAIN_SERVICE, provider).and_then(|e| e.get_password()).unwrap_or_default()
}

/// "••••abcd" for display, never the key itself.
pub fn mask(key: &str) -> String {
    match key.chars().count() {
        0 => String::new(),
        n if n > 8 => format!("••••{}", key.chars().skip(n - 4).collect::<String>()),
        _ => "••••".into(),
    }
}

// ---------- the client ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub role: Role,
    pub content: String,
}

impl Msg {
    pub fn user(c: impl Into<String>) -> Self { Msg { role: Role::User, content: c.into() } }
    pub fn assistant(c: impl Into<String>) -> Self { Msg { role: Role::Assistant, content: c.into() } }
}

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    pub temperature: f32,
    /// Ask the provider for a JSON object (when it supports a JSON mode).
    pub json: bool,
    pub timeout: Duration,
    pub max_tokens: u32,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { temperature: 0.2, json: false, timeout: Duration::from_secs(300), max_tokens: 16000 }
    }
}

#[derive(Clone)]
pub struct Ai {
    pub settings: AiSettings,
    kind: Kind,
    base_url: String,
    key: String,
    http: reqwest::Client,
    /// A separate client for embeddings when they come from another provider.
    embedder: Option<Box<Ai>>,
}

impl Ai {
    /// A client for these settings, with the key from the keychain (or `key` when testing unsaved input).
    pub fn new(settings: AiSettings, key: Option<String>) -> Result<Ai> {
        let p = provider(&settings.provider).ok_or_else(|| anyhow!("Choose an AI provider in Settings"))?;
        let base_url = if settings.base_url.trim().is_empty() { p.base_url } else { settings.base_url.trim() }
            .trim_end_matches('/').to_string();
        if base_url.is_empty() {
            bail!("Enter the endpoint URL, for example https://my-host/v1");
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            bail!("The endpoint URL must start with http:// or https://");
        }
        let key = key.unwrap_or_else(|| load_key(p.id));
        if p.needs_key && key.is_empty() {
            bail!("{} needs an API key", p.label);
        }
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).build()?;
        let embedder = match settings.embed_provider.as_str() {
            "" | "builtin" => None,
            other if other == settings.provider && settings.embed_base_url.trim().is_empty() => None,
            other => {
                let es = AiSettings { provider: other.to_string(), models: vec![], base_url: settings.embed_base_url.clone(),
                                      embed_model: settings.embed_model.clone(), embed_provider: String::new(), embed_base_url: String::new() };
                Some(Box::new(Ai::new(es, None).context("embedding provider")?))
            }
        };
        Ok(Ai { settings, kind: p.kind, base_url, key, http, embedder })
    }

    pub fn label(&self) -> &'static str {
        provider(&self.settings.provider).map(|p| p.label).unwrap_or("AI")
    }

    /// (reply text, model that answered). Each model in the list is tried in turn.
    pub async fn chat(&self, system: &str, msgs: &[Msg], opts: Opts) -> Result<(String, String)> {
        if self.settings.models.is_empty() {
            bail!("Choose a model in Settings");
        }
        let mut errors = Vec::new();
        for model in &self.settings.models {
            match self.chat_once(model, system, msgs, opts).await {
                Ok(text) if !text.trim().is_empty() => return Ok((text, model.clone())),
                Ok(_) => errors.push(format!("{model}: empty reply (it may have declined the request)")),
                Err(e) => errors.push(format!("{model}: {e:#}")),
            }
        }
        bail!("{} request failed: {}", self.label(), errors.join(" | "))
    }

    /// Chat expecting a JSON object back; re-asks once if the reply doesn't parse.
    pub async fn chat_json(&self, system: &str, user: &str, temperature: f32) -> Result<(Value, String)> {
        let opts = Opts { temperature, json: true, ..Opts::default() };
        let mut msgs = vec![Msg::user(user)];
        let mut last = String::new();
        for _ in 0..2 {
            let (text, model) = self.chat(system, &msgs, opts).await?;
            match extract_json(&text) {
                Ok(v) => return Ok((v, model)),
                Err(e) => {
                    // Keep the start of what the model said: it is usually the real reason
                    // (a provider notice, a refusal, a quota message), not a formatting slip.
                    let said: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect();
                    last = if said.is_empty() { e.to_string() } else { format!("{e}. It said: “{said}”") };
                    msgs.push(Msg::assistant(text));
                    msgs.push(Msg::user("That was not valid JSON. Reply with ONLY the JSON object, no prose."));
                }
            }
        }
        bail!("The model never returned valid JSON: {last}")
    }

    async fn chat_once(&self, model: &str, system: &str, msgs: &[Msg], opts: Opts) -> Result<String> {
        match self.kind {
            Kind::Anthropic => self.anthropic(model, system, msgs, opts).await,
            Kind::Gemini => self.gemini(model, system, msgs, opts).await,
            Kind::OpenAi | Kind::Compat => self.openai(model, system, msgs, opts).await,
        }
    }

    async fn post(&self, url: &str, body: &Value, timeout: Duration, headers: &[(&str, &str)]) -> Result<Value> {
        let mut req = self.http.post(url).json(body).timeout(timeout);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let r = req.send().await.map_err(|e| anyhow!("{}", short_err(&e)))?;
        let status = r.status();
        let text = r.text().await?;
        if !status.is_success() {
            bail!("HTTP {} {}", status.as_u16(), api_error(&text));
        }
        serde_json::from_str(&text).context("malformed response")
    }

    async fn openai(&self, model: &str, system: &str, msgs: &[Msg], opts: Opts) -> Result<String> {
        let mut messages = vec![json!({"role": "system", "content": system})];
        messages.extend(msgs.iter().map(|m| json!({"role": role_name(m.role), "content": m.content})));
        let mut body = json!({"model": model, "messages": messages, "stream": false});
        if opts.json {
            body["response_format"] = json!({"type": "json_object"});
        }
        // OpenAI's current reasoning models reject a custom temperature; keep their default.
        if self.kind == Kind::Compat {
            body["temperature"] = json!(opts.temperature);
        }
        let auth = format!("Bearer {}", self.key);
        let headers: Vec<(&str, &str)> = if self.key.is_empty() { vec![] } else { vec![("Authorization", auth.as_str())] };
        let url = format!("{}/chat/completions", self.base_url);
        let mut reply = self.post(&url, &body, opts.timeout, &headers).await;
        // Some compatible servers reject response_format or temperature: retry once without them.
        if let Err(e) = &reply {
            let msg = e.to_string().to_lowercase();
            if msg.starts_with("http 400") && (msg.contains("temperature") || msg.contains("response_format")) {
                let o = body.as_object_mut().unwrap();
                o.remove("temperature");
                o.remove("response_format");
                reply = self.post(&url, &body, opts.timeout, &headers).await;
            }
        }
        Ok(reply?["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string())
    }

    async fn anthropic(&self, model: &str, system: &str, msgs: &[Msg], opts: Opts) -> Result<String> {
        // No temperature: current Claude models reject sampling parameters.
        let mut body = json!({
            "model": model,
            "max_tokens": opts.max_tokens,
            "system": system,
            "messages": msgs.iter().map(|m| json!({"role": role_name(m.role), "content": m.content})).collect::<Vec<_>>(),
        });
        let mut headers = vec![("x-api-key", self.key.as_str()), ("anthropic-version", "2023-06-01")];
        if CLAUDE_FALLBACK_MODELS.iter().any(|m| model.starts_with(m)) {
            // Server-side refusal fallback: a declined request is retried on a suitable model.
            body["fallbacks"] = json!("default");
            headers.push(("anthropic-beta", "server-side-fallback-2026-07-01"));
        }
        let v = self.post(&format!("{}/messages", self.base_url), &body, opts.timeout, &headers).await?;
        if v["stop_reason"] == "refusal" {
            bail!("the model declined this request");
        }
        Ok(v["content"].as_array().map(|blocks| {
            blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("")
        }).unwrap_or_default())
    }

    async fn gemini(&self, model: &str, system: &str, msgs: &[Msg], opts: Opts) -> Result<String> {
        let mut config = json!({"temperature": opts.temperature});
        if opts.json {
            config["responseMimeType"] = json!("application/json");
        }
        let body = json!({
            "systemInstruction": {"parts": [{"text": system}]},
            "contents": msgs.iter().map(|m| json!({
                "role": if m.role == Role::User { "user" } else { "model" },
                "parts": [{"text": m.content}],
            })).collect::<Vec<_>>(),
            "generationConfig": config,
        });
        let url = format!("{}/models/{}:generateContent", self.base_url, model);
        let v = self.post(&url, &body, opts.timeout, &[("x-goog-api-key", self.key.as_str())]).await?;
        Ok(v["candidates"][0]["content"]["parts"].as_array().map(|parts| {
            parts.iter().filter(|p| p["thought"] != true).filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("")
        }).unwrap_or_default())
    }

    /// Embeddings for document search, or None when no embedding model is set up.
    pub async fn embed(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        if self.settings.embed_provider == "builtin" {
            return builtin_embed(texts).await.map(Some);
        }
        if let Some(e) = &self.embedder {
            return Box::pin(e.embed(texts)).await;
        }
        let model = self.settings.embed_model.trim();
        if model.is_empty() || texts.is_empty() {
            return Ok(None);
        }
        let timeout = Duration::from_secs(120);
        let vecs = |arr: Option<&Vec<Value>>, field: &str| -> Result<Vec<Vec<f32>>> {
            arr.ok_or_else(|| anyhow!("malformed embeddings response"))?
                .iter()
                .map(|d| d[field].as_array().map(|xs| xs.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect())
                    .ok_or_else(|| anyhow!("malformed embeddings response")))
                .collect()
        };
        let out = match self.kind {
            Kind::Gemini => {
                let body = json!({"requests": texts.iter().map(|t| json!({
                    "model": format!("models/{model}"), "content": {"parts": [{"text": t}]}})).collect::<Vec<_>>()});
                let url = format!("{}/models/{}:batchEmbedContents", self.base_url, model);
                let v = self.post(&url, &body, timeout, &[("x-goog-api-key", self.key.as_str())]).await?;
                vecs(v["embeddings"].as_array(), "values")?
            }
            Kind::OpenAi | Kind::Compat => {
                let auth = format!("Bearer {}", self.key);
                let headers: Vec<(&str, &str)> = if self.key.is_empty() { vec![] } else { vec![("Authorization", auth.as_str())] };
                let v = self.post(&format!("{}/embeddings", self.base_url), &json!({"model": model, "input": texts}), timeout, &headers).await?;
                vecs(v["data"].as_array(), "embedding")?
            }
            Kind::Anthropic => bail!("Anthropic doesn't offer embeddings; semantic search is off"),
        };
        if out.len() != texts.len() {
            bail!("embeddings response has {} vectors for {} texts", out.len(), texts.len());
        }
        Ok(Some(out))
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        let get = |url: String, headers: Vec<(&'static str, String)>| {
            let mut req = self.http.get(url).timeout(Duration::from_secs(20));
            for (k, v) in headers {
                req = req.header(k, v);
            }
            async move {
                let r = req.send().await.map_err(|e| anyhow!("{}", short_err(&e)))?;
                let status = r.status();
                let text = r.text().await?;
                if !status.is_success() {
                    bail!("HTTP {} {}", status.as_u16(), api_error(&text));
                }
                Ok::<Value, anyhow::Error>(serde_json::from_str(&text)?)
            }
        };
        let mut ids: Vec<String> = match self.kind {
            Kind::Anthropic => {
                let v = get(format!("{}/models?limit=100", self.base_url),
                            vec![("x-api-key", self.key.clone()), ("anthropic-version", "2023-06-01".into())]).await?;
                v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect()
            }
            Kind::Gemini => {
                let v = get(format!("{}/models?pageSize=1000", self.base_url), vec![("x-goog-api-key", self.key.clone())]).await?;
                v["models"].as_array().into_iter().flatten()
                    .filter(|m| m["supportedGenerationMethods"].as_array().is_some_and(|a| a.iter().any(|x| x == "generateContent")))
                    .filter_map(|m| m["name"].as_str().map(|n| n.trim_start_matches("models/").to_string()))
                    .collect()
            }
            Kind::OpenAi | Kind::Compat => {
                let headers = if self.key.is_empty() { vec![] } else { vec![("Authorization", format!("Bearer {}", self.key))] };
                let v = get(format!("{}/models", self.base_url), headers).await?;
                let all = v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from));
                if self.kind == Kind::OpenAi {
                    all.filter(|id| id.starts_with("gpt") || id.starts_with('o') && id[1..].starts_with(|c: char| c.is_ascii_digit())).collect()
                } else {
                    all.collect()
                }
            }
        };
        if self.kind != Kind::Anthropic {
            ids.sort();
        }
        Ok(ids)
    }

    /// Send a tiny prompt to the main model and report what came back.
    pub async fn test(&self) -> Result<String> {
        let one = Ai { settings: AiSettings { models: self.settings.models.iter().take(1).cloned().collect(), ..self.settings.clone() }, ..self.clone() };
        let opts = Opts { timeout: Duration::from_secs(60), max_tokens: 50, ..Opts::default() };
        let (reply, model) = one.chat("You are a connection test.", &[Msg::user("Reply with exactly: OK")], opts).await?;
        let mut note = format!("{model} replied “{}”", reply.trim().chars().take(40).collect::<String>());
        if !self.settings.embed_model.is_empty() {
            match self.embed(&["connection test".into()]).await {
                Ok(Some(v)) => note.push_str(&format!(". Document search is on ({} dimensions).", v[0].len())),
                Ok(None) => {}
                Err(e) => note.push_str(&format!(". Document search failed: {e:#}")),
            }
        }
        Ok(note)
    }
}

/// The sentence-transformer bundled with the app; runs on this computer, no service needed.
pub async fn builtin_embed(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let texts = texts.to_vec();
    tokio::task::spawn_blocking(move || crate::embed::embed(&texts)).await?
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn short_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        "couldn't connect (is the server running and the URL right?)".into()
    } else {
        e.to_string()
    }
}

/// The readable part of an API error body.
fn api_error(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = v["error"]["message"].as_str().or_else(|| v["error"].as_str()).or_else(|| v["message"].as_str());
    msg.map(String::from).unwrap_or_else(|| body.chars().take(200).collect())
}

/// The first JSON object in a model reply (handles ```json fences and surrounding prose).
pub fn extract_json(text: &str) -> Result<Value> {
    let start = text.find('{').ok_or_else(|| anyhow!("no JSON object in the reply"))?;
    let bytes = text.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for i in start..bytes.len() {
        let ch = bytes[i];
        if in_str {
            match ch {
                _ if esc => esc = false,
                b'\\' => esc = true,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match ch {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(serde_json::from_str(&text[start..=i])?);
                    }
                }
                _ => {}
            }
        }
    }
    bail!("unterminated JSON object in the reply")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_from_fenced_reply() {
        let v = extract_json("Sure!\n```json\n{\"a\": \"x}\", \"b\": {\"c\": 1}}\n```").unwrap();
        assert_eq!(v["a"], "x}");
        assert_eq!(v["b"]["c"], 1);
    }

    #[test]
    fn masks_keys() {
        assert_eq!(mask("sk-ant-1234567890"), "••••7890");
        assert_eq!(mask("short"), "••••");
        assert_eq!(mask(""), "");
    }
}
