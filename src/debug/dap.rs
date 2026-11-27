//! Debug Adapter Protocol client, over an adapter's stdio (lldb-dap, debugpy, php-debug) or a
//! TCP port it listens on (js-debug, delve).

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use serde_json::{Value, json};

use crate::rpc;

#[derive(Debug)]
pub enum DapEvent {
    Initialized,
    Stopped { thread_id: i64, reason: String },
    Continued,
    Output(String),
    Terminated,
    /// The adapter asks for a child session (js-debug): the `startDebugging` arguments.
    StartDebugging(Value),
}

type Pending = Arc<Mutex<HashMap<i64, smol::channel::Sender<Result<Value>>>>>;
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct DapClient {
    writer: Writer,
    child: Mutex<Option<Child>>,
    seq: AtomicI64,
    pending: Pending,
}

impl DapClient {
    /// An adapter speaking DAP on its stdin/stdout.
    pub fn start(argv: &[String], cwd: &Path, events: smol::channel::Sender<DapEvent>) -> Result<Arc<Self>> {
        let pending: Pending = Default::default();
        let slot: Arc<Mutex<Option<Writer>>> = Default::default();
        let (pending_r, slot_r) = (pending.clone(), slot.clone());
        let (child, stdin) = rpc::spawn(argv, cwd, move |msg| {
            let writer = slot_r.lock().unwrap().clone();
            handle_incoming(msg, &pending_r, writer.as_ref(), &events)
        })?;
        let writer: Writer = Arc::new(Mutex::new(Box::new(stdin)));
        *slot.lock().unwrap() = Some(writer.clone());
        Ok(Arc::new(Self { writer, child: Mutex::new(Some(child)), seq: AtomicI64::new(1), pending }))
    }

    /// Start an adapter that listens on `port` (`{port}` in `argv` is replaced), then connect.
    pub fn start_tcp(argv: &[String], cwd: &Path, port: u16, events: smol::channel::Sender<DapEvent>) -> Result<Arc<Self>> {
        let argv: Vec<String> = argv.iter().map(|a| a.replace("{port}", &port.to_string())).collect();
        let (program, args) = argv.split_first().context("empty adapter command")?;
        let child = crate::util::command(program)
            .args(args)
            .current_dir(cwd)
            .env("PATH", crate::lsp::install::search_path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to start `{program}`"))?;
        let client = Self::connect(port, events)?;
        *client.child.lock().unwrap() = Some(child);
        Ok(client)
    }

    /// Connect to an adapter already listening on `port` (retrying while it starts).
    pub fn connect(port: u16, events: smol::channel::Sender<DapEvent>) -> Result<Arc<Self>> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let stream = loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => break s,
                Err(e) if Instant::now() > deadline => return Err(anyhow!("debug adapter not reachable on port {port}: {e}")),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        };
        let pending: Pending = Default::default();
        let writer: Writer = Arc::new(Mutex::new(Box::new(stream.try_clone()?)));
        let (pending_r, writer_r) = (pending.clone(), writer.clone());
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            while let Ok(Some(msg)) = rpc::read_message(&mut reader) {
                handle_incoming(msg, &pending_r, Some(&writer_r), &events);
            }
            let _ = events.try_send(DapEvent::Terminated);
        });
        Ok(Arc::new(Self { writer, child: Mutex::new(None), seq: AtomicI64::new(1), pending }))
    }

    /// Send a request and wait for its response body.
    pub async fn request(&self, command: &str, arguments: Value) -> Result<Value> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = smol::channel::bounded(1);
        self.pending.lock().unwrap().insert(seq, tx);
        rpc::write_message(
            &mut *self.writer.lock().unwrap(),
            &json!({"seq": seq, "type": "request", "command": command, "arguments": arguments}),
        )?;
        rx.recv().await.map_err(|_| anyhow!("debug adapter exited"))?
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

/// A free local TCP port for an adapter to listen on.
pub fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

fn handle_incoming(msg: Value, pending: &Pending, writer: Option<&Writer>, events: &smol::channel::Sender<DapEvent>) {
    match msg["type"].as_str() {
        Some("response") => {
            let Some(seq) = msg["request_seq"].as_i64() else { return };
            if let Some(tx) = pending.lock().unwrap().remove(&seq) {
                let result = if msg["success"].as_bool().unwrap_or(false) {
                    Ok(msg.get("body").cloned().unwrap_or(Value::Null))
                } else {
                    Err(anyhow!("{}", msg["message"].as_str().unwrap_or("request failed")))
                };
                let _ = tx.try_send(result);
            }
        }
        // Reverse request: accept it so the adapter goes on.
        Some("request") => {
            if msg["command"] == "startDebugging" {
                let _ = events.try_send(DapEvent::StartDebugging(msg["arguments"].clone()));
            }
            if let Some(writer) = writer {
                let response = json!({"seq": 0, "type": "response", "request_seq": msg["seq"], "command": msg["command"], "success": true});
                let _ = rpc::write_message(&mut *writer.lock().unwrap(), &response);
            }
        }
        Some("event") => {
            let body = &msg["body"];
            let event = match msg["event"].as_str() {
                Some("initialized") => DapEvent::Initialized,
                Some("stopped") => DapEvent::Stopped {
                    thread_id: body["threadId"].as_i64().unwrap_or(0),
                    reason: body["reason"].as_str().unwrap_or("").to_string(),
                },
                Some("continued") => DapEvent::Continued,
                Some("output") if body["category"] != "telemetry" => DapEvent::Output(body["output"].as_str().unwrap_or("").to_string()),
                Some("terminated") => DapEvent::Terminated,
                _ => return,
            };
            let _ = events.try_send(event);
        }
        _ => {}
    }
}

/// The debug adapter command line: `lldb-dap` from Xcode (macOS) or PATH (`lldb-vscode` on older LLVM).
pub fn adapter_command() -> Vec<String> {
    #[cfg(target_os = "macos")]
    if let Ok(out) = crate::util::command("xcrun").args(["-f", "lldb-dap"]).output() {
        if out.status.success() {
            return vec![String::from_utf8_lossy(&out.stdout).trim().to_string()];
        }
    }
    let found = ["lldb-dap", "lldb-vscode"].into_iter().find(|n| crate::lsp::install::which(n).is_some());
    vec![found.unwrap_or("lldb-dap").into()]
}

/// The last binary (non-test) executable reported by `cargo build --message-format=json`.
pub fn cargo_executable(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["reason"] == "compiler-artifact" && v["profile"]["test"] == false)
        .filter_map(|v| v["executable"].as_str().map(String::from))
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_cargo_executable() {
        let out = r#"{"reason":"compiler-artifact","profile":{"test":false},"executable":null}
not json
{"reason":"compiler-artifact","profile":{"test":false},"executable":"/w/target/debug/app"}
{"reason":"build-finished","success":true}"#;
        assert_eq!(cargo_executable(out).as_deref(), Some("/w/target/debug/app"));
    }

    /// End-to-end against the real adapter: stops at a breakpoint and reads locals.
    #[test]
    fn lldb_dap_stops_at_breakpoint() {
        let adapter = adapter_command();
        if crate::util::command(&adapter[0]).arg("--help").output().is_err() {
            return eprintln!("lldb-dap not installed, skipping");
        }
        let dir = std::env::temp_dir().join(format!("forge-dap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("t.c");
        std::fs::write(&src, "int main(void) {\n  int answer = 42;\n  return answer - 42;\n}\n").unwrap();
        let plan = crate::debug::adapters::plan(&dir, &src, "c").unwrap();
        let crate::project::Program::Path(exe) = plan.program else { panic!("single file builds to a known path") };
        crate::ai::tools::run_shell(&plan.build.unwrap(), &dir, &[], 60).unwrap();

        let (tx, rx) = smol::channel::unbounded();
        let client = DapClient::start(&adapter, &dir, tx).unwrap();
        smol::block_on(async {
            client.request("initialize", json!({"adapterID": "lldb-dap", "linesStartAt1": true, "pathFormat": "path"})).await.unwrap();
            let launch = { let c = client.clone(); let exe = exe.clone(); smol::spawn(async move { c.request("launch", json!({"program": exe})).await }) };
            loop { if let DapEvent::Initialized = rx.recv().await.unwrap() { break } }
            client.request("setBreakpoints", json!({"source": {"path": src.display().to_string()}, "breakpoints": [{"line": 3}]})).await.unwrap();
            client.request("configurationDone", json!({})).await.unwrap();
            launch.await.unwrap();
            let thread_id = loop { if let DapEvent::Stopped { thread_id, .. } = rx.recv().await.unwrap() { break thread_id } };
            let st = client.request("stackTrace", json!({"threadId": thread_id})).await.unwrap();
            assert_eq!(st["stackFrames"][0]["line"], 3);
            let frame = st["stackFrames"][0]["id"].as_i64().unwrap();
            let scopes = client.request("scopes", json!({"frameId": frame})).await.unwrap();
            let vars = client.request("variables", json!({"variablesReference": scopes["scopes"][0]["variablesReference"]})).await.unwrap();
            assert!(vars["variables"].as_array().unwrap().iter().any(|v| v["name"] == "answer" && v["value"] == "42"), "{vars}");
            let _ = client.request("disconnect", json!({"terminateDebuggee": true})).await;
        });
    }
}
