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

/// The debug adapter command line, whatever the machine has: `lldb-dap` from Xcode (macOS), the
/// PATH (`lldb-vscode` on older LLVM) or LLVM's folders; else GDB 14+ and its built-in DAP (MinGW,
/// Linux distributions); else CodeLLDB (VS Code's), downloaded once. `{port}`: a TCP adapter.
pub fn adapter_command() -> anyhow::Result<Vec<String>> {
    #[cfg(target_os = "macos")]
    if let Ok(out) = crate::util::command("xcrun").args(["-f", "lldb-dap"]).output() {
        if out.status.success() {
            return Ok(vec![String::from_utf8_lossy(&out.stdout).trim().to_string()]);
        }
    }
    if let Some(found) = ["lldb-dap", "lldb-vscode"].into_iter().find_map(crate::lsp::install::which) {
        return Ok(vec![found.display().to_string()]);
    }
    // Linux distributions version it (`lldb-dap-18`); Windows' LLVM installer leaves it off the PATH.
    let dirs: Vec<std::path::PathBuf> = if cfg!(windows) {
        ["ProgramFiles", "ProgramW6432"].iter().filter_map(|v| std::env::var(v).ok()).map(|p| std::path::Path::new(&p).join("LLVM").join("bin")).collect()
    } else {
        // Debian / Ubuntu: /usr/lib/llvm-18/bin.
        let llvm = std::fs::read_dir("/usr/lib").into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("llvm-"))).map(|p| p.join("bin"));
        std::env::split_paths(&crate::lsp::install::search_path()).chain(llvm).collect()
    };
    let mut versioned: Vec<std::path::PathBuf> = dirs
        .iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| { let n = n.to_string_lossy(); n == "lldb-dap.exe" || n == "lldb-dap" || n.starts_with("lldb-dap-") || n.starts_with("lldb-vscode-") }))
        .collect();
    versioned.sort_by_key(|p| std::cmp::Reverse(p.file_name().and_then(|n| n.to_str()).and_then(|n| n.rsplit('-').next()?.parse::<u32>().ok()).unwrap_or(0)));
    if let Some(found) = versioned.first() {
        return Ok(vec![found.display().to_string()]);
    }
    // GDB: on the PATH, or next to the compiler (MinGW ships it in the same `bin`).
    let next_to_compiler = ["gcc", "g++"].iter().filter_map(|c| crate::lsp::install::which(c)?.parent().map(|d| d.join(format!("gdb{}", std::env::consts::EXE_SUFFIX))));
    let gdb = crate::lsp::install::which("gdb").into_iter().chain(next_to_compiler).find(|p| p.is_file());
    if let Some(gdb) = gdb {
        let version = crate::util::command(&gdb).arg("--version").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        if gdb_version(&version).is_some_and(|v| v >= 14) {
            return Ok(vec![gdb.display().to_string(), "--interpreter=dap".into()]);
        }
    }
    codelldb()
}

/// CodeLLDB's adapter with its own LLDB (MIT, github.com/vadimcn/codelldb), installed under the
/// tools folder the first time: debugs MSVC (PDB) and GCC / Clang (DWARF) programs on every OS.
pub fn codelldb() -> anyhow::Result<Vec<String>> {
    use anyhow::Context as _;
    let dir = crate::lsp::install::tools_dir().join("codelldb");
    let ext = dir.join("extension");
    let exe = ext.join("adapter").join(format!("codelldb{}", std::env::consts::EXE_SUFFIX));
    if !exe.is_file() {
        let os = if cfg!(windows) { "win32" } else if cfg!(target_os = "macos") { "darwin" } else { "linux" };
        let arch = if std::env::consts::ARCH == "aarch64" { "arm64" } else { "x64" };
        let asset = format!("codelldb-{os}-{arch}.vsix");
        let url = super::adapters::github_asset("vadimcn/codelldb", |n| n == asset).with_context(|| crate::i18n::t("No C/C++ debugger found and CodeLLDB could not be downloaded").to_string())?;
        std::fs::create_dir_all(&dir)?;
        // A .vsix is a zip.
        crate::lsp::install::unpack_url_into(&url, "codelldb.zip", &dir)?;
        #[cfg(unix)]
        for sub in ["adapter", "lldb/bin"] {
            use std::os::unix::fs::PermissionsExt;
            for entry in std::fs::read_dir(ext.join(sub)).into_iter().flatten().flatten() {
                let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o755));
            }
        }
        anyhow::ensure!(exe.is_file(), "{} {}", exe.display(), crate::i18n::t("missing after the download"));
    }
    let mut argv = vec![exe.display().to_string()];
    let lib = ["lldb/bin/liblldb.dll", "lldb/lib/liblldb.dylib", "lldb/lib/liblldb.so"].iter().map(|l| ext.join(l)).find(|l| l.is_file());
    if let Some(lib) = lib {
        argv.extend(["--liblldb".into(), lib.display().to_string()]);
    }
    argv.extend(["--port".into(), "{port}".into()]);
    Ok(argv)
}

/// `GNU gdb (GDB) 16.2` → 16.
fn gdb_version(text: &str) -> Option<u32> {
    text.lines().next()?.split_whitespace().find_map(|w| w.split('.').next()?.parse().ok().filter(|_| w.contains('.')))
}

/// Whether the adapter is GDB's own DAP or CodeLLDB: they take `env` as an object, not a list.
pub fn env_as_object(argv: &[String]) -> bool {
    argv.iter().any(|a| a == "--interpreter=dap") || argv.first().is_some_and(|p| std::path::Path::new(p).file_stem().is_some_and(|s| s == "codelldb"))
}

/// lldb-dap's launch arguments for GDB / CodeLLDB: `env` as an object, GDB's stop-at-main flag.
pub fn adapt_launch(mut launch: Value) -> Value {
    if let Some(env) = launch["env"].as_array() {
        let map: serde_json::Map<String, Value> = env.iter().filter_map(|e| e.as_str()?.split_once('=')).map(|(k, v)| (k.to_string(), v.into())).collect();
        launch["env"] = map.into();
    }
    if launch["stopOnEntry"] == true {
        launch["stopAtBeginningOfMainSubprogram"] = true.into();
    }
    launch
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
    fn speaks_gdb_s_launch_dialect() {
        assert_eq!(gdb_version("GNU gdb (GDB) 16.2\nCopyright"), Some(16));
        assert_eq!(gdb_version("GNU gdb (Ubuntu 12.1-0ubuntu1) 12.1"), Some(12));
        let launch = adapt_launch(serde_json::json!({"program": "a", "env": ["PATH=C:\\deps", "X=1=2"], "stopOnEntry": true}));
        assert_eq!((launch["env"]["PATH"].as_str(), launch["env"]["X"].as_str(), launch["stopAtBeginningOfMainSubprogram"].as_bool()), (Some("C:\\deps"), Some("1=2"), Some(true)));
        assert!(env_as_object(&["gdb".into(), "--interpreter=dap".into()]));
        assert!(env_as_object(&["/t/codelldb/extension/adapter/codelldb.exe".into(), "--port".into(), "{port}".into()]));
        assert!(!env_as_object(&["/usr/bin/lldb-dap".into()]));
    }

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
        let Ok(adapter) = adapter_command() else { return eprintln!("no debugger installed, skipping") };
        if env_as_object(&adapter) || crate::util::command(&adapter[0]).arg("--help").output().is_err() {
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
