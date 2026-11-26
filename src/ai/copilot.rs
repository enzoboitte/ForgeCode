//! GitHub Copilot: device-flow sign-in for several GitHub accounts, account info and quotas,
//! session tokens, account selection with failover, model list and inline completions.
//!
//! Blocking HTTP: call the network functions from a background task.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use gpui_kit::{App, Global, Image, ImageFormat};
use serde::Deserialize;
use serde_json::{Value, json};

use super::llm::{ApiKind, Endpoint, is_quota_error};
use crate::settings::{Settings, config_dir};
use crate::util::write_private;

/// OAuth app id used by the official Copilot editor plugins (copilot.vim, VS Code).
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const EDITOR_HEADERS: [(&str, &str); 3] = [
    ("Editor-Version", "vscode/1.99.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.26.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];
/// An account that hit a rate limit or quota is skipped this long.
const COOLDOWN_SECS: u64 = 10 * 60;

#[derive(Clone, Debug, PartialEq)]
pub enum SignIn {
    Idle,
    /// Waiting for the user to enter `user_code` at `uri`.
    Pending { user_code: String, uri: String },
    Error(String),
}

#[derive(Clone, Debug)]
pub struct Quota {
    /// API id: `premium_interactions`, `chat`, `completions`.
    pub name: String,
    pub remaining: f64,
    pub entitlement: f64,
    pub unlimited: bool,
    /// Extra requests beyond the entitlement are billed instead of refused.
    pub overage: bool,
}

impl Quota {
    pub fn used(&self) -> f64 {
        (self.entitlement - self.remaining).max(0.)
    }

    /// Remaining share in 0..=1.
    pub fn fraction(&self) -> f64 {
        if self.unlimited || self.entitlement <= 0. { 1. } else { (self.remaining / self.entitlement).clamp(0., 1.) }
    }
}

#[derive(Clone, Debug)]
pub struct AccountInfo {
    pub login: String,
    pub name: String,
    pub avatar: Option<Arc<Image>>,
    pub plan: String,
    pub quotas: Vec<Quota>,
    pub reset_date: String,
}

pub struct Copilot {
    github_token: String,
    /// (token, api base, expires_at unix seconds)
    session: Mutex<Option<(String, String, u64)>>,
    /// Base URL of Copilot's code-completion service (from the session token).
    proxy: Mutex<String>,
    pub info: Mutex<Option<AccountInfo>>,
    /// Chat models this account may use (plans differ: Student has fewer than Business).
    pub models: Mutex<Vec<String>>,
    cooldown_until: AtomicU64,
}

/// App-wide Copilot state.
pub struct CopilotState {
    pub sign_in: SignIn,
    pub accounts: Vec<Arc<Copilot>>,
    /// Chat models offered by Copilot.
    pub models: Vec<String>,
}

impl Global for CopilotState {}

#[derive(Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
}

fn accounts_path() -> std::path::PathBuf {
    config_dir().join("github_accounts.json")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn save_tokens(accounts: &[Arc<Copilot>]) {
    let tokens: Vec<&str> = accounts.iter().map(|a| a.github_token.as_str()).collect();
    let _ = write_private(&accounts_path(), serde_json::to_string(&tokens).unwrap_or_default().as_bytes());
}

impl CopilotState {
    pub fn init(cx: &mut App) {
        let mut tokens: Vec<String> = std::fs::read_to_string(accounts_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // Single-account file from earlier versions.
        let legacy = config_dir().join("github_token");
        if let Ok(t) = std::fs::read_to_string(&legacy) {
            if !tokens.contains(&t.trim().to_string()) {
                tokens.push(t.trim().to_string());
            }
            let _ = std::fs::remove_file(legacy);
        }
        let accounts: Vec<_> = tokens.into_iter().filter(|t| !t.is_empty()).map(Copilot::new).collect();
        save_tokens(&accounts);
        cx.set_global(Self { sign_in: SignIn::Idle, accounts, models: vec![] });
        Self::refresh(cx);
    }

    fn update(cx: &mut App, f: impl FnOnce(&mut Self)) {
        f(cx.global_mut::<Self>());
        cx.refresh_windows();
    }

    /// Fetch account info, quotas and the model list in the background.
    pub fn refresh(cx: &mut App) {
        let accounts = cx.global::<Self>().accounts.clone();
        if accounts.is_empty() {
            return;
        }
        cx.spawn(async move |cx| {
            let models = cx
                .background_executor()
                .spawn(async move {
                    let mut all: Vec<String> = vec![];
                    for a in &accounts {
                        match a.fetch_info() {
                            Ok(info) => *a.info.lock().unwrap() = Some(info),
                            Err(e) => eprintln!("copilot account: {e:#}"),
                        }
                        let models = a.models().unwrap_or_default();
                        for m in &models {
                            if !all.contains(m) {
                                all.push(m.clone());
                            }
                        }
                        *a.models.lock().unwrap() = models;
                    }
                    all
                })
                .await;
            cx.update(|cx| Self::update(cx, |s| s.models = models));
        })
        .detach();
    }

    /// Run the device flow and add the account.
    pub fn sign_in(cx: &mut App) {
        cx.spawn(async move |cx| {
            let code = match cx.background_executor().spawn(async { start_device_flow() }).await {
                Ok(code) => code,
                Err(e) => return cx.update(|cx| Self::update(cx, |s| s.sign_in = SignIn::Error(e.to_string()))),
            };
            cx.update(|cx| {
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(code.user_code.clone()));
                cx.open_url(&code.verification_uri);
                Self::update(cx, |s| s.sign_in = SignIn::Pending { user_code: code.user_code.clone(), uri: code.verification_uri.clone() });
            });
            let account = cx
                .background_executor()
                .spawn(async move {
                    let account = Copilot::new(poll_access_token(&code)?);
                    *account.info.lock().unwrap() = Some(account.fetch_info()?);
                    anyhow::Ok(account)
                })
                .await;
            cx.update(|cx| {
                match account {
                    Ok(account) => Self::update(cx, |s| {
                        let login = account.login();
                        s.accounts.retain(|a| a.login() != login);
                        s.accounts.push(account);
                        s.sign_in = SignIn::Idle;
                        save_tokens(&s.accounts);
                    }),
                    Err(e) => Self::update(cx, |s| s.sign_in = SignIn::Error(e.to_string())),
                }
                Self::refresh(cx);
            });
        })
        .detach();
    }

    pub fn remove(cx: &mut App, login: &str) {
        Self::update(cx, |s| {
            s.accounts.retain(|a| a.login() != login);
            save_tokens(&s.accounts);
        });
    }

    /// Accounts to try, best first: the chosen one, or in "auto" mode the healthiest by remaining quota.
    pub fn candidates(cx: &App) -> Vec<Arc<Copilot>> {
        let accounts = &cx.global::<Self>().accounts;
        let choice = &Settings::get(cx).copilot_account;
        if let Some(a) = accounts.iter().find(|a| &a.login() == choice) {
            return vec![a.clone()];
        }
        rank(accounts)
    }
}

/// Prefer the accounts whose plan lists `model`. When no account lists it (unknown lists, or a
/// model outside the picker such as a completion model), keep them all and let the API decide.
pub fn supporting(accounts: &[Arc<Copilot>], model: &str) -> Vec<Arc<Copilot>> {
    let listed: Vec<_> = accounts.iter().filter(|a| a.models.lock().unwrap().iter().any(|m| m == model)).cloned().collect();
    if listed.is_empty() { accounts.to_vec() } else { listed }
}

/// Auto mode: accounts in cooldown last, then by remaining premium/chat quota.
pub fn rank(accounts: &[Arc<Copilot>]) -> Vec<Arc<Copilot>> {
    let mut v = accounts.to_vec();
    v.sort_by(|a, b| {
        (a.cooling_down(), -a.headroom()).partial_cmp(&(b.cooling_down(), -b.headroom())).unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}

/// Try `f` on each account in order, moving on when one is rate limited or out of quota.
pub fn with_failover<T>(accounts: &[Arc<Copilot>], mut f: impl FnMut(&Copilot) -> Result<T>) -> Result<T> {
    let mut last = anyhow!("No GitHub Copilot account. Sign in from Settings › AI.");
    for a in accounts {
        match f(a) {
            Err(e) if e.to_string().contains("model_not_supported") => {
                last = anyhow!("{}: {e}", a.login());
            }
            Err(e) if is_quota_error(&e) => {
                a.cooldown_until.store(now() + COOLDOWN_SECS, Ordering::Relaxed);
                last = anyhow!("{} ({}): {e}", a.login(), "rate limited");
            }
            other => return other,
        }
    }
    Err(last)
}

pub fn start_device_flow() -> Result<DeviceCode> {
    Ok(ureq::post("https://github.com/login/device/code")
        .set("Accept", "application/json")
        .send_json(json!({"client_id": CLIENT_ID, "scope": "read:user"}))?
        .into_json()?)
}

pub fn poll_access_token(code: &DeviceCode) -> Result<String> {
    let mut interval = code.interval.max(5);
    for _ in 0..180 {
        std::thread::sleep(Duration::from_secs(interval));
        let resp: Value = ureq::post("https://github.com/login/oauth/access_token")
            .set("Accept", "application/json")
            .send_json(json!({
                "client_id": CLIENT_ID,
                "device_code": code.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))?
            .into_json()?;
        if let Some(token) = resp["access_token"].as_str() {
            return Ok(token.to_string());
        }
        match resp["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some(err) => bail!("GitHub sign-in failed: {err}"),
            None => bail!("unexpected GitHub response"),
        }
    }
    bail!("GitHub sign-in timed out")
}

/// Readable plan from the subscription SKU (`copilot_plan` alone says "individual" for Pro, Pro+ and students).
pub fn plan_name(sku: &str, plan: &str) -> String {
    let name = match sku {
        s if s.contains("business") => "Business",
        s if s.contains("enterprise") => "Enterprise",
        s if s.contains("educational") => "Student",
        s if s.contains("free_limited") => "Free",
        s if s.starts_with("plus") || s.contains("pro_plus") => "Pro+",
        s if s.contains("subscriber") || s.contains("monthly") || s.contains("yearly") => "Pro",
        _ if !plan.is_empty() => plan,
        _ => "?",
    };
    name.to_string()
}

fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some(ImageFormat::Png),
        [0xFF, 0xD8, ..] => Some(ImageFormat::Jpeg),
        [b'G', b'I', b'F', ..] => Some(ImageFormat::Gif),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(ImageFormat::Webp),
        _ => None,
    }
}

impl Copilot {
    pub fn new(github_token: String) -> Arc<Self> {
        Arc::new(Self { github_token, session: Mutex::new(None), proxy: Mutex::new(String::new()), info: Mutex::new(None), models: Mutex::new(vec![]), cooldown_until: AtomicU64::new(0) })
    }

    pub fn login(&self) -> String {
        self.info.lock().unwrap().as_ref().map(|i| i.login.clone()).unwrap_or_else(|| "…".into())
    }

    pub fn cooling_down(&self) -> bool {
        self.cooldown_until.load(Ordering::Relaxed) > now()
    }

    /// Lowest remaining quota share (1.0 when unknown or unlimited).
    fn headroom(&self) -> f64 {
        self.info.lock().unwrap().as_ref().map_or(1., |i| i.quotas.iter().map(Quota::fraction).fold(1., f64::min))
    }

    fn github_get(&self, url: &str) -> Result<Value> {
        Ok(ureq::get(url)
            .set("Authorization", &format!("token {}", self.github_token))
            .set("Accept", "application/json")
            .set(EDITOR_HEADERS[0].0, EDITOR_HEADERS[0].1)
            .call()?
            .into_json()?)
    }

    pub fn fetch_info(&self) -> Result<AccountInfo> {
        let user = self.github_get("https://api.github.com/user")?;
        let copilot = self.github_get("https://api.github.com/copilot_internal/user").unwrap_or(Value::Null);
        let avatar = user["avatar_url"].as_str().and_then(|url| {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut ureq::get(&format!("{url}&s=64")).call().ok()?.into_reader(), &mut bytes).ok()?;
            Some(Arc::new(Image::from_bytes(image_format(&bytes)?, bytes)))
        });
        let quotas = copilot["quota_snapshots"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, q)| Quota {
                name: name.clone(),
                remaining: q["quota_remaining"].as_f64().or(q["remaining"].as_f64()).unwrap_or(0.),
                entitlement: q["entitlement"].as_f64().unwrap_or(0.),
                unlimited: q["unlimited"].as_bool().unwrap_or(false),
                overage: q["overage_permitted"].as_bool().unwrap_or(false),
            })
            .collect();
        Ok(AccountInfo {
            login: user["login"].as_str().unwrap_or("?").into(),
            name: user["name"].as_str().unwrap_or("").into(),
            avatar,
            plan: plan_name(copilot["access_type_sku"].as_str().unwrap_or(""), copilot["copilot_plan"].as_str().unwrap_or("")),
            quotas,
            reset_date: copilot["quota_reset_date"].as_str().unwrap_or("").into(),
        })
    }

    /// A valid Copilot session token and API base URL, refreshed when close to expiry.
    fn session(&self) -> Result<(String, String)> {
        if let Some((token, api, exp)) = self.session.lock().unwrap().clone() {
            if exp > now() + 60 {
                return Ok((token, api));
            }
        }
        let resp = self
            .github_get("https://api.github.com/copilot_internal/v2/token")
            .map_err(|e| anyhow!("Copilot unavailable for {}: {e}", self.login()))?;
        let token = resp["token"].as_str().ok_or_else(|| anyhow!("no Copilot token"))?.to_string();
        let api = resp["endpoints"]["api"].as_str().unwrap_or("https://api.githubcopilot.com").to_string();
        let exp = resp["expires_at"].as_u64().unwrap_or(now() + 600);
        *self.proxy.lock().unwrap() = resp["endpoints"]["proxy"].as_str().unwrap_or("https://copilot-proxy.githubusercontent.com").to_string();
        *self.session.lock().unwrap() = Some((token.clone(), api.clone(), exp));
        Ok((token, api))
    }

    /// Copilot's own code-completion model, as VS Code uses for gray suggestions: it fills the
    /// gap between `prompt` and `suffix` directly (no chat model, no instructions), and streams.
    pub fn code_completion(&self, engine: &str, prompt: &str, suffix: &str, language: &str, mut on_text: impl FnMut(&str)) -> Result<String> {
        let (token, _) = self.session()?;
        let proxy = self.proxy.lock().unwrap().clone();
        let mut req = ureq::post(&format!("{}/v1/engines/{engine}/completions", proxy.trim_end_matches('/')))
            .timeout(Duration::from_secs(20))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Openai-Intent", "copilot-ghost")
            .set("X-Copilot-Repository", "")
            .set("Content-Type", "application/json");
        for (k, v) in EDITOR_HEADERS {
            req = req.set(k, v);
        }
        let body = json!({
            "prompt": prompt, "suffix": suffix, "max_tokens": 500, "temperature": 0, "top_p": 1, "n": 1,
            "stop": ["\n\n\n"], "stream": true,
            "extra": {"language": language, "next_indent": 0, "trim_by_indentation": true},
        });
        let resp = req.send_json(body).map_err(|e| match e {
            ureq::Error::Status(code, r) => anyhow!("HTTP {code}: {}", r.into_string().unwrap_or_default()),
            other => anyhow!(other),
        })?;
        let mut text = String::new();
        for line in std::io::BufRead::lines(std::io::BufReader::new(resp.into_reader())) {
            let line = line?;
            let Some(data) = line.strip_prefix("data: ") else { continue };
            if data == "[DONE]" {
                break;
            }
            if let Some(t) = serde_json::from_str::<Value>(data).ok().and_then(|v| v["choices"][0]["text"].as_str().map(String::from)) {
                text.push_str(&t);
                on_text(&text);
            }
        }
        Ok(text)
    }

    /// OpenAI-compatible endpoint for chat / agent use.
    pub fn endpoint(&self, model: &str) -> Result<Endpoint> {
        let (token, api) = self.session()?;
        Ok(Endpoint {
            kind: ApiKind::OpenAi,
            base_url: api,
            api_key: token,
            model: model.to_string(),
            headers: EDITOR_HEADERS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            sampling: None,
        })
    }

    /// Chat models enabled in the Copilot model picker.
    pub fn models(&self) -> Result<Vec<String>> {
        let (token, api) = self.session()?;
        let mut req = ureq::get(&format!("{api}/models")).set("Authorization", &format!("Bearer {token}"));
        for (k, v) in EDITOR_HEADERS {
            req = req.set(k, v);
        }
        let resp: Value = req.call()?.into_json()?;
        let mut models: Vec<String> = resp["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["model_picker_enabled"].as_bool().unwrap_or(true) && m["capabilities"]["type"].as_str().unwrap_or("chat") == "chat")
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect();
        models.dedup();
        Ok(models)
    }
}

/// Ghost-text suggestion for the cursor position.
// shortcut: uses the chat endpoint with a fill-in-the-middle prompt; switch to Copilot's
// dedicated completions endpoint if latency matters.
/// An inline suggestion, streamed: `on_text` gets the whole suggestion so far, shaped for the
/// cursor, each time the model writes more. Returns the final one.
pub fn inline_completion(accounts: &[Arc<Copilot>], path: &str, language: &str, prefix: &str, suffix: &str, model: &str, mut on_text: impl FnMut(&str)) -> Result<Option<String>> {
    let prefix = tail(prefix, 6000);
    let suffix = head(suffix, 2000);
    let messages = vec![
        json!({"role": "system", "content": "You are a code completion engine inside an editor. Output ONLY the text to insert at <CURSOR>: no explanations, no markdown fences, no repetition of the code before or after the cursor. Prefer completing the current statement or block; at most 12 lines. When the cursor ends a line that opens a block, start your output with the line break. Output nothing if no completion is useful."}),
        json!({"role": "user", "content": format!("File: {path} ({language})\n{prefix}<CURSOR>{suffix}")}),
    ];
    let line_before = prefix.rsplit('\n').next().unwrap_or("");
    let shape = |raw: &str| shape_suggestion(trim_overlap(strip_fences(raw), suffix), line_before);
    // Copilot's code-completion model first, as VS Code: fast and made for this.
    let comment = if matches!(language, "python" | "ruby" | "bash" | "shellscript" | "yaml" | "toml" | "make" | "cmake" | "perl" | "r") { "#" } else { "//" };
    let prompt = format!("{comment} Path: {path}\n{prefix}");
    let mut shown = String::new();
    let fim = with_failover(accounts, |a| {
        a.code_completion("gpt-4o-copilot", &prompt, suffix, language, |so_far| {
            let now = shape(so_far);
            if now != shown {
                on_text(&now);
                shown = now;
            }
        })
    });
    if let Ok(raw) = fim {
        let insert = shape(&raw);
        return Ok((!insert.trim().is_empty()).then_some(insert));
    }
    // Else a chat model, told to act as one.
    let reply = with_failover(accounts, |a| {
        let mut raw = String::new();
        let mut shown = String::new();
        a.endpoint(model)?.chat(&messages, &[], |delta| {
            raw.push_str(delta);
            let now = shape(&raw);
            if now != shown {
                on_text(&now);
                shown = now;
            }
        })
    })?;
    let insert = shape(&reply.content);
    Ok((!insert.trim().is_empty()).then_some(insert))
}

/// Fit a raw suggestion to the cursor: drop what the user already typed on this line
/// (e.g. `//` or indentation the model repeated) and stay on the current line when it has code.
pub fn shape_suggestion(insert: &str, line_before: &str) -> String {
    let typed = line_before.trim_start();
    let mut s = insert;
    // Already in a comment: drop the comment marker the model repeats (`// ` + `/// text`).
    const MARKERS: [&str; 8] = ["///", "//!", "//", "/*", "--", "#", ";", "*"];
    if MARKERS.iter().any(|m| typed.starts_with(m)) {
        let body = s.trim_start();
        if let Some(m) = MARKERS.iter().find(|m| body.starts_with(**m)) {
            let rest = body[m.len()..].trim_start_matches(['/', '!']).trim_start();
            let sep = if typed.ends_with(char::is_whitespace) || rest.is_empty() { "" } else { " " };
            return format!("{sep}{}", rest.lines().next().unwrap_or("").trim_end());
        }
    }
    if typed.is_empty() {
        // Cursor in indentation: don't indent twice.
        let indent = line_before.len().min(s.len() - s.trim_start_matches([' ', '\t']).len());
        s = &s[indent..];
    } else if let Some(rest) = s.trim_start().strip_prefix(typed) {
        // The model rewrote the whole line.
        s = rest;
    } else {
        // Repeated symbols at the end of what was typed (`//`, `/*`, `->`, `::`), never identifiers.
        let overlap = (2..=typed.len())
            .rev()
            .filter(|&k| typed.is_char_boundary(typed.len() - k))
            .map(|k| &typed[typed.len() - k..])
            .find(|tail| !tail.chars().any(char::is_alphanumeric) && s.starts_with(*tail));
        if let Some(tail) = overlap {
            s = &s[tail.len()..];
        }
    }
    // Mid-line, the current line only; a suggestion starting on the next line (a block after
    // `:` or `{`) or one at the start of a line may span lines (12 at most, like the prompt).
    let multi = typed.is_empty() || s.starts_with('\n');
    let s = if multi { s.lines().take(13).collect::<Vec<_>>().join("\n") } else { s.lines().next().unwrap_or("").to_string() };
    s.trim_end().to_string()
}

/// The next word of a suggestion, with its leading spaces: what `⌘→` accepts.
pub fn next_word(s: &str) -> &str {
    let start = s.len() - s.trim_start_matches([' ', '\t']).len();
    let rest = &s[start..];
    let Some(first) = rest.chars().next() else { return s };
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let len = if first == '\n' {
        1
    } else if is_word(first) {
        rest.find(|c: char| !is_word(c)).unwrap_or(rest.len())
    } else {
        rest.find(|c: char| is_word(c) || c.is_whitespace()).unwrap_or(rest.len()).max(first.len_utf8())
    };
    &s[..start + len]
}

fn tail(s: &str, max: usize) -> &str {
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn head(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Models sometimes wrap the answer in a fenced block anyway.
fn strip_fences(s: &str) -> &str {
    // The leading line break is kept: it puts the suggestion on the next line.
    let t = s.trim_end_matches('\n');
    if let Some(rest) = t.trim_start_matches('\n').strip_prefix("```") {
        let body = rest.split_once('\n').map_or("", |(_, b)| b);
        return body.strip_suffix("```").unwrap_or(body).trim_end_matches('\n');
    }
    t
}

/// Drop the end of the suggestion that repeats the text already after the cursor (e.g. `);`).
fn trim_overlap<'a>(insert: &'a str, suffix: &str) -> &'a str {
    let rest = suffix.lines().next().unwrap_or("").trim_end();
    if rest.is_empty() {
        return insert;
    }
    (1..=rest.len().min(insert.len()))
        .rev()
        .filter(|&n| rest.is_char_boundary(n) && insert.is_char_boundary(insert.len() - n))
        .find(|&n| insert.ends_with(&rest[..n]))
        .map_or(insert, |n| &insert[..insert.len() - n])
}

#[cfg(test)]
mod tests {
    /// Live check against the user's own Copilot account (run by hand, with consent).
    #[test]
    #[ignore]
    fn live_code_completion() {
        let path = dirs::config_dir().unwrap().join("forgecode/github_accounts.json");
        let tokens: Vec<String> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let a = super::Copilot::new(tokens[0].clone());
        for engine in ["gpt-41-copilot", "gpt-4o-copilot", "copilot-codex"] {
            let t = std::time::Instant::now();
            let r = a.code_completion(engine, "// Path: main.cpp\n#include <vector>\nint sum(const std::vector<int>& v) {\n    int total = 0;\n    ", "\n}\n", "cpp", |_| {});
            eprintln!("{engine}: {:?} in {:?}", r.as_ref().map(|s| s.chars().take(120).collect::<String>()), t.elapsed());
        }
        // The editor's path: shaped for the cursor.
        let t = std::time::Instant::now();
        let r = super::inline_completion(&[a.clone()], "src/main.cpp", "cpp", "#include <vector>\nint sum(const std::vector<int>& v) {\n    int total = 0;\n    ", "\n}\n", "gpt-4.1", |_| {});
        eprintln!("inline: {r:?} in {:?}", t.elapsed());
        assert!(r.unwrap().is_some_and(|s| s.contains("total")));
    }

    use super::*;

    #[test]
    fn names_plans_from_sku() {
        assert_eq!(plan_name("copilot_for_business_seat_quota", "business"), "Business");
        assert_eq!(plan_name("free_educational_quota", "individual"), "Student");
        assert_eq!(plan_name("", "individual"), "individual");
    }

    #[test]
    fn strips_markdown_fences() {
        assert_eq!(strip_fences("```rust\nlet x = 1;\n```"), "let x = 1;");
        assert_eq!(strip_fences("x + 1"), "x + 1");
        assert_eq!(strip_fences("\n    return x\n"), "\n    return x", "the line break before a block is kept");
    }

    #[test]
    fn slicing_respects_char_boundaries() {
        assert_eq!(tail("ééé", 3), "é");
        assert_eq!(head("ééé", 3), "é");
    }

    #[test]
    fn trims_text_already_after_cursor() {
        assert_eq!(trim_overlap("square(i));", "));\n}"), "square(i");
        assert_eq!(trim_overlap("foo()", ""), "foo()");
    }

    #[test]
    fn shapes_suggestions_to_the_cursor() {
        assert_eq!(shape_suggestion("// compute the square", "    //"), " compute the square");
        assert_eq!(shape_suggestion("/// Read a file to a string.", "// "), "Read a file to a string.");
        assert_eq!(shape_suggestion(" /// Read a file", "//"), " Read a file");
        assert_eq!(shape_suggestion("# install deps", "# "), "install deps");
        assert_eq!(shape_suggestion("    return y;", "    "), "return y;");
        assert_eq!(shape_suggestion("int y = x * x;\nreturn y;", "    int y"), " = x * x;");
        assert_eq!(shape_suggestion("x * x;\nreturn y;", "    int y = "), "x * x;");
        assert_eq!(shape_suggestion("if (x) {\n  y();\n}", ""), "if (x) {\n  y();\n}");
        assert_eq!(shape_suggestion("    let ab", "    let ab"), "", "whole-line rewrite with nothing new");
        assert_eq!(shape_suggestion("b + 1", "a = b"), "b + 1", "identifiers are never treated as repeats");
        // After `def f(x):`, the body goes on the next lines, not glued to the colon.
        assert_eq!(shape_suggestion("\n    y = x * 2\n    return y", "def f(x):"), "\n    y = x * 2\n    return y");
        // While it streams, a partial first line is shown as is.
        assert_eq!(shape_suggestion("\n    y = x", "def f(x):"), "\n    y = x");
    }

    #[test]
    fn splits_suggestions_word_by_word() {
        assert_eq!(next_word(" compute the square"), " compute");
        assert_eq!(next_word("(i));"), "(");
        assert_eq!(next_word("square(i)"), "square");
        assert_eq!(next_word("\n    return"), "\n");
    }

    #[test]
    fn auto_mode_prefers_accounts_with_quota() {
        let low = Copilot::new("a".into());
        let high = Copilot::new("b".into());
        let quota = |r| Quota { name: "chat".into(), remaining: r, entitlement: 100., unlimited: false, overage: false };
        let info = |login: &str, r| AccountInfo { login: login.into(), name: String::new(), avatar: None, plan: String::new(), quotas: vec![quota(r)], reset_date: String::new() };
        *low.info.lock().unwrap() = Some(info("low", 5.));
        *high.info.lock().unwrap() = Some(info("high", 80.));
        assert_eq!(rank(&[low.clone(), high.clone()])[0].login(), "high");
        high.cooldown_until.store(now() + 100, Ordering::Relaxed);
        assert_eq!(rank(&[low, high])[0].login(), "low");
    }

    #[test]
    fn models_go_to_accounts_that_have_them() {
        let student = Copilot::new("s".into());
        let business = Copilot::new("b".into());
        *student.models.lock().unwrap() = vec!["gpt-4.1".into()];
        *business.models.lock().unwrap() = vec!["gpt-4.1".into(), "claude-opus-5.5".into()];
        let both = [student.clone(), business.clone()];
        assert_eq!(supporting(&both, "claude-opus-5.5").len(), 1);
        assert_eq!(supporting(&both, "gpt-4.1").len(), 2);
        assert_eq!(supporting(&both, "gpt-4o-mini").len(), 2, "unlisted models are tried everywhere");
        let r = with_failover(&both, |a| if a.github_token == "s" { Err(anyhow!("HTTP 400: model_not_supported")) } else { Ok(1) });
        assert_eq!(r.unwrap(), 1, "a refused model moves on to the next account");
    }

    #[test]
    fn failover_skips_rate_limited_accounts() {
        let a = Copilot::new("a".into());
        let b = Copilot::new("b".into());
        let mut calls = 0;
        let r = with_failover(&[a.clone(), b], |acc| {
            calls += 1;
            if acc.github_token == "a" { Err(anyhow!("HTTP 429: limit")) } else { Ok(acc.github_token.clone()) }
        });
        assert_eq!(r.unwrap(), "b");
        assert_eq!(calls, 2);
        assert!(a.cooling_down());
    }
}
