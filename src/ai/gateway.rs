//! A local OpenAI-compatible endpoint (`http://127.0.0.1:<port>/v1`) through which external
//! agents (Hermes) reach the models ForgeCode is signed in to: Copilot accounts, with their token
//! exchange and failover, and the user's OpenAI-compatible servers (LM Studio, Ollama, vLLM…).
//! Only the models allowed in Settings are listed and served. A per-run secret keeps other local
//! programs out.

use std::io::{BufRead as _, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

use super::agent::Backend;
use super::llm::Endpoint;
use super::models::Resolver;
use crate::settings::ProviderKind;

/// What the gateway serves; replaced whenever accounts, providers or the allowed list change.
#[derive(Clone)]
pub struct Routes {
    pub resolver: Resolver,
    /// Model references (`copilot/gpt-4.1`, `lmstudio/qwen3-coder`) the agent may use.
    pub allowed: Vec<String>,
}

pub struct Gateway {
    pub port: u16,
    pub secret: String,
    routes: Arc<Mutex<Routes>>,
}

static GATEWAY: OnceLock<Gateway> = OnceLock::new();

/// A question for the user (a database change an agent wants to make) and where to send the answer.
pub type Confirmation = (Value, smol::channel::Sender<bool>);
static CONFIRM: OnceLock<smol::channel::Sender<Confirmation>> = OnceLock::new();

/// Where `POST /forge/confirm` requests go: the editor shows them and answers.
pub fn set_confirm_handler(tx: smol::channel::Sender<Confirmation>) {
    let _ = CONFIRM.set(tx);
}

impl Gateway {
    /// The running gateway, started on first use.
    pub fn get(routes: Routes) -> Result<&'static Gateway> {
        if let Some(g) = GATEWAY.get() {
            g.update(routes);
            return Ok(g);
        }
        let g = Self::start(routes)?;
        Ok(GATEWAY.get_or_init(|| g))
    }

    /// The gateway keeps its port and key across runs: agents store the endpoint with each
    /// conversation, so a reopened conversation must find it at the same address.
    fn start(routes: Routes) -> Result<Self> {
        Self::start_at(routes, Some(&crate::settings::config_dir().join("gateway.json")))
    }

    fn start_at(routes: Routes, saved: Option<&std::path::Path>) -> Result<Self> {
        let previous: Value = saved.and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let wanted = previous["port"].as_u64().unwrap_or(0) as u16;
        // The saved port, unless something else took it.
        let listener = (wanted != 0).then(|| TcpListener::bind(("127.0.0.1", wanted)).ok()).flatten();
        let listener = match listener {
            Some(l) => l,
            None => TcpListener::bind("127.0.0.1:0").context("start the local AI gateway")?,
        };
        let port = listener.local_addr()?.port();
        let secret = previous["secret"].as_str().filter(|s| s.len() >= 16).map(String::from).unwrap_or_else(|| {
            format!("forge-{:x}{:x}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0))
        });
        if let Some(path) = saved {
            if std::fs::write(path, json!({"port": port, "secret": secret}).to_string()).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
        let routes = Arc::new(Mutex::new(routes));
        let (shared, key) = (routes.clone(), secret.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (routes, key) = (shared.clone(), key.clone());
                std::thread::spawn(move || {
                    let _ = serve(stream, &routes, &key);
                });
            }
        });
        Ok(Self { port, secret, routes })
    }

    /// The gateway if something already started it.
    pub fn running() -> Option<&'static Gateway> {
        GATEWAY.get()
    }

    pub fn update(&self, routes: Routes) {
        *self.routes.lock().unwrap() = routes;
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }
}

/// Models the gateway can serve: Copilot's and OpenAI-compatible servers' (Claude's Messages API
/// isn't relayed).
pub fn servable(model_ref: &str, resolver: &Resolver) -> bool {
    let provider = model_ref.split('/').next().unwrap_or("");
    provider == "copilot" || resolver.providers.iter().any(|p| p.id == provider && p.kind == ProviderKind::OpenAi)
}

struct Request {
    method: String,
    path: String,
    auth: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut BufReader<TcpStream>) -> Result<Request> {
    let mut line = String::new();
    stream.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let (mut len, mut auth) = (0usize, String::new());
    loop {
        let mut header = String::new();
        if stream.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap_or((&header, ""));
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => len = value.trim().parse()?,
            "authorization" => auth = value.trim().trim_start_matches("Bearer ").to_string(),
            _ => {}
        }
    }
    let mut body = vec![0; len];
    stream.read_exact(&mut body)?;
    Ok(Request { method, path, auth, body })
}

fn respond(out: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    write!(out, "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reason(status), body.len())?;
    out.write_all(body)?;
    Ok(())
}

fn error(out: &mut TcpStream, status: u16, message: &str) -> Result<()> {
    respond(out, status, "application/json", json!({"error": {"message": message, "type": "forgecode_gateway"}}).to_string().as_bytes())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    }
}

fn serve(stream: TcpStream, routes: &Mutex<Routes>, secret: &str) -> Result<()> {
    let mut out = stream.try_clone()?;
    let req = read_request(&mut BufReader::new(stream))?;
    if req.auth != secret {
        return error(&mut out, 401, "Invalid ForgeCode gateway key");
    }
    let routes = routes.lock().unwrap().clone();
    let path = req.path.trim_end_matches('/');
    match (req.method.as_str(), path) {
        ("POST", "/forge/confirm") => {
            let request: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let allowed = match CONFIRM.get() {
                Some(tx) => {
                    let (answer_tx, answer_rx) = smol::channel::bounded(1);
                    tx.try_send((request, answer_tx)).is_ok()
                        && smol::block_on(smol::future::or(async { answer_rx.recv().await.unwrap_or(false) }, async {
                            smol::Timer::after(std::time::Duration::from_secs(600)).await;
                            false
                        }))
                }
                None => false,
            };
            respond(&mut out, 200, "application/json", json!({"allowed": allowed}).to_string().as_bytes())
        }
        ("GET", "/v1/models") => {
            let data: Vec<Value> = routes.allowed.iter().map(|m| json!({"id": m, "object": "model", "owned_by": "forgecode"})).collect();
            respond(&mut out, 200, "application/json", json!({"object": "list", "data": data}).to_string().as_bytes())
        }
        ("POST", "/v1/chat/completions") => {
            let mut body: Value = serde_json::from_slice(&req.body).context("invalid JSON body")?;
            let model_ref = body["model"].as_str().unwrap_or("").to_string();
            if !routes.allowed.contains(&model_ref) {
                return error(&mut out, 403, &format!("Model `{model_ref}` is not allowed for agents in ForgeCode (Settings › AI › Hermes)."));
            }
            match forward(&routes.resolver, &model_ref, &mut body, &mut out) {
                Ok(()) => Ok(()),
                Err(e) => error(&mut out, 502, &e.to_string()),
            }
        }
        _ => error(&mut out, 404, "Not found"),
    }
}

/// Send the request upstream and stream its answer back unchanged (SSE when `stream: true`).
fn forward(resolver: &Resolver, model_ref: &str, body: &mut Value, out: &mut TcpStream) -> Result<()> {
    let upstream = |endpoint: &Endpoint, body: &mut Value| -> Result<ureq::Response> {
        body["model"] = Value::String(endpoint.model.clone());
        // Some servers cut answers short when no `max_tokens` is given (a long file written by the
        // agent then arrives without its last arguments): ask for the model's usual budget.
        if body.get("max_tokens").is_none()
            && body.get("max_completion_tokens").is_none()
            && let Some(max) = endpoint.sampling.as_ref().and_then(|s| s.max_tokens)
        {
            body["max_tokens"] = max.into();
        }
        let mut req = ureq::post(&format!("{}/chat/completions", endpoint.base_url.trim_end_matches('/'))).set("Content-Type", "application/json");
        if !endpoint.api_key.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", endpoint.api_key));
        }
        for (k, v) in &endpoint.headers {
            req = req.set(k, v);
        }
        match req.send_string(&body.to_string()) {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(429, resp)) => bail!("rate limited: {}", resp.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(_, resp)) => Ok(resp),
            Err(e) => Err(anyhow!(e)),
        }
    };
    fn send(b: &Backend, upstream: &dyn Fn(&Endpoint, &mut Value) -> Result<ureq::Response>, body: &mut Value) -> Result<ureq::Response> {
        match b {
            Backend::Direct(endpoint) => upstream(endpoint, body),
            // Copilot: next account when one is rate limited, as the built-in chat does.
            Backend::Copilot { accounts, model } => super::copilot::with_failover(accounts, |a| upstream(&a.endpoint(model)?, body)),
            // Then the other accounts and servers.
            Backend::Chain(chain) => super::agent::first_ok(chain, |b| send(b, upstream, body)),
        }
    }
    let resp = send(&resolver.backend(model_ref)?, &upstream, body)?;
    let status = resp.status();
    let content_type = resp.header("Content-Type").unwrap_or("application/json").to_string();
    write!(out, "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n", reason(status))?;
    // Byte for byte, flushing as it arrives so tokens stream to the agent live.
    let mut reader = resp.into_reader();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Provider;

    /// A fake OpenAI server behind the gateway: models are filtered, the key checked, the
    /// model renamed upstream, and the streamed answer relayed.
    #[test]
    fn keeps_its_address_across_runs() {
        let file = std::env::temp_dir().join(format!("forge-gateway-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let routes = || Routes { resolver: Resolver { providers: vec![], copilot: vec![] }, allowed: vec![] };
        let (port, secret) = {
            let g = Gateway::start_at(routes(), Some(&file)).unwrap();
            (g.port, g.secret.clone())
        };
        // The first one's listener thread still holds the port in this process: only the key
        // (and the wish for the same port) can be checked here.
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!((saved["port"].as_u64(), saved["secret"].as_str()), (Some(port as u64), Some(secret.as_str())));
        let again = Gateway::start_at(routes(), Some(&file)).unwrap();
        assert_eq!(again.secret, secret);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn relays_allowed_models_only() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in upstream.incoming().flatten() {
                let mut out = stream.try_clone().unwrap();
                let req = read_request(&mut BufReader::new(stream)).unwrap();
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let sse = format!("data: {{\"model\":\"{}\",\"key\":\"{}\",\"max\":{}}}\n\ndata: [DONE]\n\n", body["model"].as_str().unwrap(), req.auth, body["max_tokens"]);
                respond(&mut out, 200, "text/event-stream", sse.as_bytes()).unwrap();
            }
        });
        let provider: Provider = serde_json::from_value(json!({
            "id": "lm", "name": "LM", "kind": "openai", "base_url": format!("http://127.0.0.1:{up_port}/v1"), "api_key": "local-key"
        }))
        .unwrap();
        let routes = Routes { resolver: Resolver { providers: vec![provider], copilot: vec![] }, allowed: vec!["lm/qwen".into()] };
        let g = Gateway::start_at(routes, None).unwrap();
        let models: Value = ureq::get(&format!("{}/models", g.base_url())).set("Authorization", &format!("Bearer {}", g.secret)).call().unwrap().into_json().unwrap();
        assert_eq!(models["data"][0]["id"], "lm/qwen");
        let post = |model: &str, key: &str| {
            ureq::post(&format!("{}/chat/completions", g.base_url()))
                .set("Authorization", &format!("Bearer {key}"))
                .send_json(json!({"model": model, "messages": [], "stream": true}))
        };
        let text = post("lm/qwen", &g.secret).unwrap().into_string().unwrap();
        assert!(text.contains("\"model\":\"qwen\"") && text.contains("\"key\":\"local-key\""), "{text}");
        // A self-hosted server gets the model's output budget when the agent gives none.
        assert!(text.contains("\"max\":16384"), "{text}");
        assert!(matches!(post("lm/other", &g.secret), Err(ureq::Error::Status(403, _))), "not allowed");
        assert!(matches!(post("lm/qwen", "wrong"), Err(ureq::Error::Status(401, _))), "bad key");
        // No editor answering: an agent's database change is refused.
        let answer: Value = ureq::post(&format!("{}/forge/confirm", g.base_url().trim_end_matches("/v1")))
            .set("Authorization", &format!("Bearer {}", g.secret))
            .send_json(json!({"connection": "main", "sql": "delete from t"}))
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(answer["allowed"], false);
    }
}
