//! Agent Client Protocol client: ForgeCode drives an external coding agent (Hermes) over stdio,
//! as Zed does. Newline-delimited JSON-RPC 2.0. The agent streams its work as `session/update`
//! notifications and asks the editor for permissions (edits, commands) and file access.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use serde_json::{Value, json};

/// What the agent tells the editor.
#[derive(Debug)]
pub enum AcpEvent {
    /// A `session/update` (message/thought chunk, tool call, plan…): `update` is its `update` field.
    Update { update: Value },
    /// The agent waits for the user's choice; answer with [`AcpClient::answer_permission`].
    /// `request` is the JSON-RPC id to answer (a number or a string).
    Permission { request: Value, tool_call: Value, options: Vec<PermissionOption> },
    /// The agent wrote a file through the editor (reload it if open).
    FileWritten(PathBuf),
    /// The agent process ended; the text is the end of its log.
    Exited(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

/// A session as `session/new` describes it.
#[derive(Clone, Debug, Default)]
pub struct Session {
    pub id: String,
    /// (id, name) of the models the agent offers.
    pub models: Vec<(String, String)>,
    pub current_model: String,
    /// (id, name, description) of the agent's modes (how much it asks before acting).
    pub modes: Vec<(String, String, String)>,
    pub current_mode: String,
}

type Pending = Arc<Mutex<HashMap<i64, smol::channel::Sender<Result<Value>>>>>;

pub struct AcpClient {
    /// Every message both ways and the agent's stderr, for diagnosing (`None`: not kept).
    trace: Option<Mutex<std::fs::File>>,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    pending: Pending,
    /// `initialize`'s answer (agent name and version, capabilities).
    pub agent_info: std::sync::OnceLock<Value>,
}

impl AcpClient {
    /// Start the agent and complete `initialize`, writing the protocol traffic to `trace`.
    /// Blocking: call off the UI thread.
    pub fn start_traced(argv: &[String], cwd: &Path, env: &[(String, String)], events: smol::channel::Sender<AcpEvent>, trace: Option<PathBuf>) -> Result<Arc<Self>> {
        let (program, args) = argv.split_first().context("empty agent command")?;
        let mut child = crate::util::command(program)
            .args(args)
            .current_dir(cwd)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start `{program}`"))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        let pending: Pending = Default::default();
        let trace = trace.and_then(|p| {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::File::create(p).ok().map(Mutex::new)
        });
        let client = Arc::new(Self { trace, stdin: Mutex::new(stdin), child: Mutex::new(child), next_id: AtomicI64::new(1), pending: pending.clone(), agent_info: Default::default() });

        // The agent logs on stderr: keep the end, it explains a crash.
        let log = Arc::new(Mutex::new(std::collections::VecDeque::<String>::new()));
        {
            let log = log.clone();
            let weak = Arc::downgrade(&client);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if let Some(c) = weak.upgrade() {
                        c.log("stderr", &line);
                    }
                    let mut l = log.lock().unwrap();
                    l.push_back(line);
                    if l.len() > 30 {
                        l.pop_front();
                    }
                }
            });
        }
        {
            let weak = Arc::downgrade(&client);
            let events = events.clone();
            std::thread::spawn(move || {
                // Lines are read as bytes: one invalid UTF-8 byte must not stop the reader (the
                // agent would then block writing to a full pipe).
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match std::io::BufRead::read_until(&mut reader, b'\n', &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let line = String::from_utf8_lossy(&buf);
                    let Some(client) = weak.upgrade() else { break };
                    client.log("<-", line.trim_end());
                    let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else { continue };
                    client.handle(msg, &events);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                let tail = log.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
                for (_, tx) in pending.lock().unwrap().drain() {
                    let _ = tx.try_send(Err(anyhow!("the agent stopped: {}", tail.trim())));
                }
                let _ = events.try_send(AcpEvent::Exited(tail));
            });
        }
        let info = smol::block_on(client.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": false},
                "clientInfo": {"name": "ForgeCode", "version": env!("CARGO_PKG_VERSION")}
            }),
        ))?;
        let _ = client.agent_info.set(info);
        Ok(client)
    }

    /// The agent takes images in a prompt (`promptCapabilities.image`).
    pub fn accepts_images(&self) -> bool {
        self.agent_info.get().is_some_and(|i| i["agentCapabilities"]["promptCapabilities"]["image"] == true)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = smol::channel::bounded(1);
        self.pending.lock().unwrap().insert(id, tx);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        rx.recv().await.map_err(|_| anyhow!("the agent stopped"))?
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn log(&self, dir: &str, text: &str) {
        if let Some(trace) = &self.trace {
            let text: String = text.chars().take(2000).collect();
            let _ = writeln!(trace.lock().unwrap(), "{} {dir} {text}", chrono_now());
        }
    }

    fn send(&self, msg: &Value) -> Result<()> {
        self.log("->", &msg.to_string());
        let mut stdin = self.stdin.lock().unwrap();
        writeln!(stdin, "{msg}")?;
        stdin.flush()?;
        Ok(())
    }

    fn reply(&self, id: &Value, result: Result<Value>) {
        let msg = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": e.to_string()}}),
        };
        let _ = self.send(&msg);
    }

    fn handle(&self, msg: Value, events: &smol::channel::Sender<AcpEvent>) {
        let method = msg["method"].as_str();
        match (msg.get("id"), method) {
            (Some(id), None) => {
                let Some(id) = id.as_i64() else { return };
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let result = match msg.get("error") {
                        Some(e) => Err(anyhow!("{}", e["message"].as_str().unwrap_or("agent error"))),
                        None => Ok(msg["result"].clone()),
                    };
                    let _ = tx.try_send(result);
                }
            }
            (None, Some("session/update")) => {
                let p = &msg["params"];
                let _ = events.try_send(AcpEvent::Update { update: p["update"].clone() });
            }
            (Some(id), Some("session/request_permission")) => {
                let p = &msg["params"];
                let options = p["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| PermissionOption {
                        id: o["optionId"].as_str().unwrap_or("").into(),
                        name: o["name"].as_str().unwrap_or("").into(),
                        kind: o["kind"].as_str().unwrap_or("").into(),
                    })
                    .collect();
                let _ = events.try_send(AcpEvent::Permission {
                    request: id.clone(),
                    tool_call: p["toolCall"].clone(),
                    options,
                });
            }
            (Some(id), Some("fs/read_text_file")) => {
                let p = &msg["params"];
                let result = read_text(p).map(|content| json!({"content": content}));
                self.reply(id, result);
            }
            (Some(id), Some("fs/write_text_file")) => {
                let p = &msg["params"];
                let path = PathBuf::from(p["path"].as_str().unwrap_or(""));
                let result = (|| -> Result<Value> {
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&path, p["content"].as_str().unwrap_or(""))?;
                    Ok(Value::Null)
                })();
                if result.is_ok() {
                    let _ = events.try_send(AcpEvent::FileWritten(path));
                }
                self.reply(id, result);
            }
            // Anything else the editor doesn't offer.
            (Some(id), Some(other)) => {
                let _ = self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("ForgeCode does not support `{other}`")}}));
            }
            _ => {}
        }
    }

    /// The user's answer to a permission request (`None` = cancelled).
    pub fn answer_permission(&self, request: &Value, option: Option<&str>) {
        let outcome = match option {
            Some(id) => json!({"outcome": "selected", "optionId": id}),
            None => json!({"outcome": "cancelled"}),
        };
        self.reply(request, Ok(json!({"outcome": outcome})));
    }

    /// A conversation in `cwd`; `mcp` are MCP servers the agent gets for it (ForgeCode's
    /// databases tools…).
    pub async fn new_session(&self, cwd: &Path, mcp: &[Value]) -> Result<Session> {
        let v = self.request("session/new", json!({"cwd": cwd.display().to_string(), "mcpServers": mcp})).await?;
        Ok(session_from(v["sessionId"].as_str().unwrap_or(""), &v))
    }

    /// Reopen an earlier conversation with its whole context.
    pub async fn load_session(&self, id: &str, cwd: &Path, mcp: &[Value]) -> Result<Session> {
        let v = self.request("session/load", json!({"sessionId": id, "cwd": cwd.display().to_string(), "mcpServers": mcp})).await?;
        Ok(session_from(id, &v))
    }

    /// Send a message made of content blocks (text, `resource_link` to attached files…);
    /// resolves with the stop reason once the agent is done with it.
    pub async fn prompt_blocks(&self, session: &str, blocks: Vec<Value>) -> Result<String> {
        let v = self.request("session/prompt", json!({"sessionId": session, "prompt": blocks})).await?;
        Ok(v["stopReason"].as_str().unwrap_or("end_turn").to_string())
    }

    pub async fn set_mode(&self, session: &str, mode: &str) -> Result<()> {
        self.request("session/set_mode", json!({"sessionId": session, "modeId": mode})).await.map(drop)
    }

    pub fn cancel(&self, session: &str) {
        self.notify("session/cancel", json!({"sessionId": session}));
    }

    pub async fn set_model(&self, session: &str, model: &str) -> Result<()> {
        self.request("session/set_model", json!({"sessionId": session, "modelId": model})).await.map(drop)
    }
}

impl AcpClient {
    pub fn pid(&self) -> u32 {
        self.child.lock().unwrap().id()
    }

    /// Stop the agent now, even while requests (a running turn) still hold this client.
    pub fn kill(&self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

/// Seconds since the epoch with milliseconds, enough to order a trace.
fn chrono_now() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:03}", t.as_secs(), t.subsec_millis())
}

fn session_from(id: &str, v: &Value) -> Session {
    let models = v["models"]["availableModels"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let id = m["modelId"].as_str().unwrap_or("").to_string();
            (id.clone(), m["name"].as_str().map(String::from).unwrap_or(id))
        })
        .collect();
    let modes = v["modes"]["availableModes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| (m["id"].as_str().unwrap_or("").to_string(), m["name"].as_str().unwrap_or("").to_string(), m["description"].as_str().unwrap_or("").to_string()))
        .collect();
    Session {
        id: id.into(),
        models,
        current_model: v["models"]["currentModelId"].as_str().unwrap_or("").into(),
        modes,
        current_mode: v["modes"]["currentModeId"].as_str().unwrap_or("").into(),
    }
}

/// `fs/read_text_file`: the whole file, or `limit` lines from 1-based `line`.
fn read_text(p: &Value) -> Result<String> {
    let text = std::fs::read_to_string(p["path"].as_str().unwrap_or(""))?;
    let (Some(line), limit) = (p["line"].as_u64(), p["limit"].as_u64()) else {
        return Ok(match p["limit"].as_u64() {
            Some(limit) => text.lines().take(limit as usize).map(|l| format!("{l}\n")).collect(),
            None => text,
        });
    };
    let lines = text.lines().skip(line.saturating_sub(1) as usize);
    Ok(match limit {
        Some(limit) => lines.take(limit as usize).map(|l| format!("{l}\n")).collect(),
        None => lines.map(|l| format!("{l}\n")).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ACP agent in Python: answers initialize/new/prompt, streams a chunk, asks a
    /// permission, then writes a file through the editor.
    const FAKE_AGENT: &str = r#"
import json, sys
def send(m): sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
def read(): return json.loads(sys.stdin.readline())
while True:
    m = read()
    if m["method"] == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1, "agentInfo": {"name": "fake"}}})
    elif m["method"] == "session/new":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "s1", "models": {"currentModelId": "copilot/gpt-4.1", "availableModels": [{"modelId": "copilot/gpt-4.1", "name": "GPT-4.1"}]}}})
    elif m["method"] == "session/prompt":
        pid = m["id"]
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hello " + m["params"]["prompt"][0]["text"]}}}})
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": "s1", "toolCall": {"toolCallId": "t1", "title": "Approve edit"}, "options": [{"optionId": "allow_once", "name": "Allow", "kind": "allow_once"}, {"optionId": "deny", "name": "Deny", "kind": "reject_once"}]}})
        answer = read()
        send({"jsonrpc": "2.0", "id": 901, "method": "fs/write_text_file", "params": {"sessionId": "s1", "path": sys.argv[1], "content": answer["result"]["outcome"]["optionId"]}})
        read()
        send({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "end_turn"}})
"#;

    #[test]
    fn drives_an_acp_agent() {
        let dir = std::env::temp_dir().join(format!("forge-acp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("agent.py");
        std::fs::write(&script, FAKE_AGENT).unwrap();
        let out = dir.join("out.txt");
        let (tx, rx) = smol::channel::unbounded();
        let argv = vec!["/usr/bin/python3".to_string(), script.display().to_string(), out.display().to_string()];
        let client = AcpClient::start_traced(&argv, &dir, &[], tx, None).unwrap();
        assert_eq!(client.agent_info.get().unwrap()["agentInfo"]["name"], "fake");
        smol::block_on(async {
            let session = client.new_session(&dir, &[]).await.unwrap();
            assert_eq!((session.id.as_str(), session.current_model.as_str()), ("s1", "copilot/gpt-4.1"));
            assert_eq!(session.models[0].1, "GPT-4.1");
            let prompt = {
                let client = client.clone();
                smol::spawn(async move { client.prompt_blocks("s1", vec![json!({"type": "text", "text": "world"})]).await })
            };
            let AcpEvent::Update { update, .. } = rx.recv().await.unwrap() else { panic!("update first") };
            assert_eq!(update["content"]["text"], "hello world");
            let AcpEvent::Permission { request, options, .. } = rx.recv().await.unwrap() else { panic!("permission") };
            assert_eq!(options[1].kind, "reject_once");
            client.answer_permission(&request, Some("allow_once"));
            assert!(matches!(rx.recv().await.unwrap(), AcpEvent::FileWritten(p) if p == out));
            assert_eq!(prompt.await.unwrap(), "end_turn");
        });
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "allow_once");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The Hermes ForgeCode installed, driven by this client like the panel does:
    /// `cargo test -- --ignored real_hermes --nocapture`.
    #[test]
    #[ignore]
    fn real_hermes_opens_a_session() {
        let home = std::env::var("HOME").unwrap();
        let fc = PathBuf::from(&home).join("Library/Application Support/forgecode");
        let exe = fc.join("plugins/hermes/runtime/src/venv/bin/hermes");
        let state = std::env::temp_dir().join(format!("forge-hermes-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        if std::env::var("FORGE_REAL_HOME").is_ok() {
            // A copy of the real state (memory, sessions, skills), not the live folder.
            let ok = crate::util::command("cp").arg("-R").arg(fc.join("agents/hermes")).arg(&state).status().unwrap();
            assert!(ok.success());
        } else {
            std::fs::create_dir_all(&state).unwrap();
            std::fs::copy(fc.join("agents/hermes/config.yaml"), state.join("config.yaml")).unwrap();
        }
        let trace = state.join("trace.log");
        let (tx, rx) = smol::channel::unbounded();
        let cwd = std::env::var("FORGE_ROOT").map(PathBuf::from).unwrap_or_else(|_| std::env::current_dir().unwrap());
        let env = vec![("HERMES_HOME".to_string(), state.display().to_string())];
        let t0 = std::time::Instant::now();
        let client = AcpClient::start_traced(&[exe.display().to_string(), "acp".into()], &cwd, &env, tx, Some(trace.clone())).unwrap();
        eprintln!("initialize ok in {:?}", t0.elapsed());
        let session = smol::block_on(smol::future::or(async { client.new_session(&cwd, &[]).await.map_err(|e| e.to_string()) }, async {
            smol::Timer::after(std::time::Duration::from_secs(90)).await;
            Err("timeout".to_string())
        }));
        eprintln!("session/new after {:?}: {:?}", t0.elapsed(), session.as_ref().map(|s| (&s.id, s.models.len())));
        while let Ok(ev) = rx.try_recv() {
            eprintln!("event: {:?}", format!("{ev:?}").chars().take(200).collect::<String>());
        }
        if session.is_err() {
            eprintln!("TRACE:\n{}", std::fs::read_to_string(&trace).unwrap_or_default().lines().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
        assert!(session.is_ok());
    }
}
