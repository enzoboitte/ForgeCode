//! Minimal LSP client: initialize handshake, requests, full-text document sync, diagnostics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::rpc;

/// Notifications forwarded to the UI.
pub enum LspEvent {
    Diagnostics { uri: String, diagnostics: Vec<lsp_types::Diagnostic> },
    /// The server process ended (crash, or stopped on purpose); `id` is [`LspClient::id`].
    Exited { id: u64 },
}

type Pending = Arc<Mutex<HashMap<i64, smol::channel::Sender<Result<Value>>>>>;

pub struct LspClient {
    /// Unique in this run: tells a restarted server from the one it replaces.
    pub id: u64,
    pub name: String,
    stdin: Arc<Mutex<ChildStdin>>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    pending: Pending,
    /// uri -> (version, text last sent): skips redundant didChange.
    docs: Mutex<HashMap<String, (i32, String)>>,
    /// Settings answered to `workspace/configuration` (see [`crate::language::server_settings`]).
    settings: Arc<Mutex<Value>>,
}

// shortcut: only spaces and `%` are percent-encoded; enough for local paths, use the `url` crate if servers complain.
// Windows paths become `file:///C:/dir/file`.
pub fn uri(path: &Path) -> String {
    let p = path.display().to_string().replace('\\', "/").replace('%', "%25").replace(' ', "%20");
    format!("file://{}{p}", if p.starts_with('/') { "" } else { "/" })
}

pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let p = uri.strip_prefix("file://")?.replace("%20", " ").replace("%3A", ":").replace("%3a", ":").replace("%25", "%");
    // `/C:/dir` → `C:/dir`
    let drive = p.len() > 2 && p.as_bytes()[2] == b':' && p.as_bytes()[1].is_ascii_alphabetic();
    Some(PathBuf::from(if drive { &p[1..] } else { &p[..] }))
}

impl LspClient {
    /// Start the server and complete the `initialize` handshake. Blocking: call off the UI thread.
    pub fn start(
        argv: &[String],
        root: &Path,
        events: smol::channel::Sender<LspEvent>,
        settings: Value,
    ) -> Result<Arc<Self>> {
        let pending: Pending = Default::default();
        let stdin_slot: Arc<Mutex<Option<Arc<Mutex<ChildStdin>>>>> = Default::default();
        let settings = Arc::new(Mutex::new(settings));
        let (pending_r, stdin_r, settings_r) = (pending.clone(), stdin_slot.clone(), settings.clone());
        let pending_exit = pending.clone();
        let program = argv.first().cloned().unwrap_or_default();
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let exit_events = events.clone();
        let (child, stdin) = rpc::spawn_with_exit(
            argv,
            root,
            move |msg| handle_incoming(msg, &pending_r, &stdin_r, &settings_r, &events),
            // A server that dies (at start, or later) fails every request still waiting, with its
            // own explanation, instead of leaving them, and the editor, waiting forever.
            move |stderr| {
                let why = if stderr.trim().is_empty() { format!("{program} stopped") } else { format!("{program} stopped: {}", stderr.trim()) };
                for (_, tx) in pending_exit.lock().unwrap().drain() {
                    let _ = tx.try_send(Err(anyhow::anyhow!(why.clone())));
                }
                let _ = exit_events.try_send(LspEvent::Exited { id });
            },
        )?;
        let stdin = Arc::new(Mutex::new(stdin));
        *stdin_slot.lock().unwrap() = Some(stdin.clone());
        let client = Arc::new(Self {
            id,
            name: argv[0].clone(),
            stdin,
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending,
            docs: Default::default(),
            settings: settings.clone(),
        });
        let initial = settings.lock().unwrap().clone();
        let params = json!({
            "processId": std::process::id(),
            "initializationOptions": initial,
            "rootUri": uri(root),
            "workspaceFolders": [{"uri": uri(root), "name": root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()}],
            "capabilities": {
                "textDocument": {
                    "synchronization": {"didSave": true},
                    // resolveSupport: servers then send the import a completion needs (auto-import).
                    "completion": {"completionItem": {
                        "snippetSupport": true,
                        "labelDetailsSupport": true,
                        "documentationFormat": ["markdown", "plaintext"],
                        "resolveSupport": {"properties": ["documentation", "detail", "additionalTextEdits"]}
                    }},
                    // Quick fixes (⌘.): without literal support servers only send bare commands.
                    "codeAction": {
                        "codeActionLiteralSupport": {"codeActionKind": {"valueSet": ["", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source", "source.organizeImports", "source.fixAll"]}},
                        "isPreferredSupport": true,
                        "dataSupport": true,
                        "resolveSupport": {"properties": ["edit"]}
                    },
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "publishDiagnostics": {"relatedInformation": false},
                    "definition": {}
                },
                "workspace": {"configuration": true, "workspaceFolders": true, "applyEdit": true, "didChangeWatchedFiles": {"dynamicRegistration": false}}
            }
        });
        smol::block_on(client.request("initialize", params))?;
        client.notify("initialized", json!({}));
        client.notify("workspace/didChangeConfiguration", json!({"settings": initial}));
        Ok(client)
    }

    /// New settings (type checking level…): servers re-read them and re-check open files.
    pub fn configure(&self, settings: Value) {
        if *self.settings.lock().unwrap() == settings {
            return;
        }
        *self.settings.lock().unwrap() = settings.clone();
        self.notify("workspace/didChangeConfiguration", json!({"settings": settings}));
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = smol::channel::bounded(1);
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        rx.recv().await.map_err(|_| anyhow!("{} stopped", self.name))?
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn send(&self, msg: Value) -> Result<()> {
        rpc::write_message(&mut *self.stdin.lock().unwrap(), &msg)?;
        Ok(())
    }

    pub fn did_open(&self, path: &Path, language_id: &str, text: &str) {
        let uri = uri(path);
        self.docs.lock().unwrap().insert(uri.clone(), (1, text.to_string()));
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}}),
        );
    }

    /// Send the full text if it changed since the last sync.
    pub fn sync(&self, path: &Path, text: &str) {
        let uri = uri(path);
        let version = {
            let mut docs = self.docs.lock().unwrap();
            let Some(doc) = docs.get_mut(&uri) else { return };
            if doc.1 == text {
                return;
            }
            doc.0 += 1;
            doc.1 = text.to_string();
            doc.0
        };
        self.notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}),
        );
    }

    pub fn did_save(&self, path: &Path) {
        self.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(path)}}));
    }

    pub fn did_close(&self, path: &Path) {
        let uri = uri(path);
        self.docs.lock().unwrap().remove(&uri);
        self.notify("textDocument/didClose", json!({"textDocument": {"uri": uri}}));
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

fn handle_incoming(
    msg: Value,
    pending: &Pending,
    stdin: &Mutex<Option<Arc<Mutex<ChildStdin>>>>,
    settings: &Mutex<Value>,
    events: &smol::channel::Sender<LspEvent>,
) {
    let method = msg.get("method").and_then(Value::as_str);
    match (msg.get("id"), method) {
        // Response to one of our requests.
        (Some(id), None) => {
            let Some(id) = id.as_i64() else { return };
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let result = match msg.get("error") {
                    Some(err) => Err(anyhow!("{}", err["message"].as_str().unwrap_or("LSP error"))),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.try_send(result);
            }
        }
        // Server -> client request: answer so the server does not stall.
        (Some(id), Some(method)) => {
            let result = match method {
                "workspace/configuration" => {
                    let settings = settings.lock().unwrap();
                    let items = msg["params"]["items"].as_array().cloned().unwrap_or_default();
                    Value::Array(items.iter().map(|item| config_section(&settings, item["section"].as_str().unwrap_or(""))).collect())
                }
                _ => Value::Null,
            };
            if let Some(stdin) = stdin.lock().unwrap().as_ref() {
                let _ = rpc::write_message(
                    &mut *stdin.lock().unwrap(),
                    &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                );
            }
        }
        (None, Some("textDocument/publishDiagnostics")) => {
            let params = &msg["params"];
            let diagnostics = serde_json::from_value(params["diagnostics"].clone()).unwrap_or_default();
            if let Some(uri) = params["uri"].as_str() {
                let _ = events.try_send(LspEvent::Diagnostics { uri: uri.to_string(), diagnostics });
            }
        }
        _ => {}
    }
}

/// `section` ("python.analysis") of the settings tree, or null.
fn config_section(settings: &Value, section: &str) -> Value {
    section.split('.').filter(|k| !k.is_empty()).fold(settings, |v, key| &v[key]).clone()
}

#[cfg(test)]
mod tests {
    use super::{config_section, path_from_uri, uri};

    #[test]
    fn answers_configuration_sections() {
        let s = serde_json::json!({"python": {"analysis": {"typeCheckingMode": "strict"}}});
        assert_eq!(config_section(&s, "python.analysis")["typeCheckingMode"], "strict");
        assert_eq!(config_section(&s, ""), s);
        assert!(config_section(&s, "rust-analyzer").is_null());
    }
    use std::path::{Path, PathBuf};

    #[test]
    fn uris_round_trip_on_unix_and_windows_paths() {
        assert_eq!(uri(Path::new("/w/my file.rs")), "file:///w/my%20file.rs");
        assert_eq!(path_from_uri("file:///w/my%20file.rs"), Some(PathBuf::from("/w/my file.rs")));
        assert_eq!(uri(Path::new(r"C:\w\a.rs")), "file:///C:/w/a.rs");
        assert_eq!(path_from_uri("file:///c%3A/w/a.rs"), Some(PathBuf::from("c:/w/a.rs")));
    }

    /// Real pyright on a project with a `.venv` library: a misspelled class must be an error,
    /// as in VS Code. Needs pyright installed by ForgeCode: `cargo test -- --ignored pyright`.
    #[test]
    #[ignore]
    fn pyright_flags_misspelled_library_class() {
        use super::{LspClient, LspEvent};
        let root = std::env::temp_dir().join(format!("forge-pyright-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // macOS: /var is /private/var; the app opens canonical paths too.
        let root = root.canonicalize().unwrap();
        let ok = std::process::Command::new("python3").args(["-m", "venv", ".venv"]).current_dir(&root).status().unwrap();
        assert!(ok.success());
        let site = std::fs::read_dir(root.join(".venv/lib")).unwrap().next().unwrap().unwrap().path().join("site-packages/mylib");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("__init__.py"), "class Session:\n    def get(self) -> int:\n        return 1\n").unwrap();
        std::fs::write(root.join("helpers.py"), "def compute_total(a, b):\n    return a + b\n").unwrap();
        let main = root.join("main.py");
        let text = "from mylib import Sesion\nimport mylib\ns = mylib.Sessoin()\nok = mylib.Session()\nok.gett()\n";
        std::fs::write(&main, text).unwrap();
        let settings = crate::language::server_settings(&crate::settings::Settings::default(), &root);
        let argv = crate::lsp::install::resolve(&["pyright-langserver".into(), "--stdio".into()]).unwrap();
        let (tx, rx) = smol::channel::unbounded();
        let client = LspClient::start(&argv, &root, tx, settings).unwrap();
        client.did_open(&main, "python", text);
        let diags = smol::block_on(async {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                let ev = smol::future::or(async { rx.recv().await.ok() }, async {
                    smol::Timer::after(std::time::Duration::from_secs(60)).await;
                    None
                })
                .await;
                match ev {
                    Some(LspEvent::Diagnostics { uri, diagnostics }) if uri.ends_with("main.py") && !diagnostics.is_empty() => break diagnostics,
                    _ if std::time::Instant::now() > deadline => break vec![],
                    _ => {}
                }
            }
        });
        for d in &diags {
            eprintln!("DIAG line {} {:?}: {}", d.range.start.line + 1, d.severity, d.message);
        }
        let errors_on = |line: u32| diags.iter().any(|d| d.range.start.line + 1 == line && d.severity == Some(lsp_types::DiagnosticSeverity::ERROR));
        assert!(errors_on(1), "unknown import symbol");
        assert!(errors_on(3), "unknown module attribute");
        assert!(errors_on(5), "unknown method");

        // A function from another project file: completing its name imports it.
        let other = root.join("use.py");
        let text = std::env::var("FORGE_PROBE").unwrap_or("x = 1\ncompute_to".into());
        let text = text.as_str();
        std::fs::write(&other, text).unwrap();
        if std::env::var("FORGE_OPEN_HELPERS").is_ok() {
            client.did_open(&root.join("helpers.py"), "python", "def compute_total(a, b):\n    return a + b\n");
        }
        client.did_open(&other, "python", text);
        // Pyright answers "incomplete" until its index is ready, like an editor re-asking as you type.
        let mut list = serde_json::Value::Null;
        for _ in 0..40 {
            list = smol::block_on(client.request(
                "textDocument/completion",
                serde_json::json!({"textDocument": {"uri": super::uri(&other)}, "position": {"line": 1, "character": text.lines().nth(1).map_or(0, |l| l.len())}, "context": {"triggerKind": 1}}),
            ))
            .unwrap();
            eprintln!("RAW {}", list.to_string().chars().take(300).collect::<String>());
            if list.to_string().contains("compute_total") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let items = list["items"].as_array().cloned().or_else(|| list.as_array().cloned()).unwrap_or_default();
        eprintln!("RAW {}", list.to_string().chars().take(400).collect::<String>());
        eprintln!("LABELS {:?}", items.iter().map(|i| i["label"].as_str().unwrap_or("")).take(15).collect::<Vec<_>>());
        let item = items.iter().find(|i| i["label"] == "compute_total").expect("compute_total offered");
        eprintln!("ITEM {item}");
        let item = if item["additionalTextEdits"].is_null() { smol::block_on(client.request("completionItem/resolve", item.clone())).unwrap() } else { item.clone() };
        eprintln!("RESOLVED edits {}", item["additionalTextEdits"]);
        assert!(item["additionalTextEdits"].to_string().contains("from helpers import compute_total"));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// What pyright answers in a real project: `FORGE_ROOT=… FORGE_FILE=… cargo test -- --ignored pyright_in_project --nocapture`.
    #[test]
    #[ignore]
    fn pyright_in_project() {
        use super::LspClient;
        let root = PathBuf::from(std::env::var("FORGE_ROOT").unwrap());
        let file = PathBuf::from(std::env::var("FORGE_FILE").unwrap());
        let text = std::fs::read_to_string(&file).unwrap() + "\n";
        let settings = crate::language::server_settings(&crate::settings::Settings::default(), &root);
        eprintln!("PYTHON {}", settings["python"]["pythonPath"]);
        let argv = crate::lsp::install::resolve(&["pyright-langserver".into(), "--stdio".into()]).unwrap();
        let (tx, _rx) = smol::channel::unbounded();
        let client = LspClient::start(&argv, &root, tx, settings).unwrap();
        client.did_open(&file, "python", &text);
        let line = text.lines().count();
        for wait in [0u64, 2, 5, 15] {
            std::thread::sleep(std::time::Duration::from_secs(wait));
            let t0 = std::time::Instant::now();
            let v = smol::block_on(client.request("textDocument/completion", serde_json::json!({"textDocument": {"uri": super::uri(&file)}, "position": {"line": line, "character": 0}, "context": {"triggerKind": 1}})));
            match v {
                Ok(v) => {
                    let items = v["items"].as_array().cloned().or_else(|| v.as_array().cloned()).unwrap_or_default();
                    eprintln!("AFTER {wait}s: {} items in {:?}, null={} incomplete={}", items.len(), t0.elapsed(), v.is_null(), v["isIncomplete"]);
                }
                Err(e) => eprintln!("AFTER {wait}s: ERROR {e}"),
            }
        }
    }

    /// Real clangd on a CMake project using a library: after ForgeCode's configure step, the
    /// library's classes and their members are completed. `cargo test -- --ignored clangd_completes`.
    #[test]
    #[ignore]
    fn clangd_completes_cmake_library_symbols() {
        use super::LspClient;
        let root = std::env::temp_dir().join(format!("forge-cmake-lsp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("libs/geo/include/geo")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("libs/geo/include/geo/shape.hpp"), "#pragma once\nnamespace geo { class Circle { public: double radius_value() const; double area() const; }; }\n").unwrap();
        std::fs::write(root.join("libs/geo/shape.cpp"), "#include <geo/shape.hpp>\ndouble geo::Circle::area() const { return 0; }\ndouble geo::Circle::radius_value() const { return 0; }\n").unwrap();
        std::fs::write(
            root.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.16)\nproject(demo CXX)\nadd_library(geo libs/geo/shape.cpp)\ntarget_include_directories(geo PUBLIC libs/geo/include)\nadd_executable(app src/main.cpp)\ntarget_link_libraries(app geo)\n",
        )
        .unwrap();
        let main = root.join("src/main.cpp");
        let text = "#include <geo/shape.hpp>\nint main() {\n  geo::Circle c;\n  c.rad\n}\n";
        std::fs::write(&main, text).unwrap();
        let cmd = crate::project::configure_command(&root).expect("a CMake project");
        let out = crate::ai::tools::run_shell(cmd, &root, &[], 120);
        assert!(root.join("build/compile_commands.json").is_file(), "{out:?}");
        let lang = crate::language::detect(&main, &crate::plugins::Plugins::default());
        let argv = crate::lsp::install::resolve(&lang.server).unwrap();
        let settings = crate::language::server_settings(&crate::settings::Settings::default(), &root);
        let (tx, rx) = smol::channel::unbounded();
        eprintln!("ARGV {argv:?}");
        let client = LspClient::start(&argv, &root, tx, settings).unwrap();
        client.did_open(&main, "cpp", text);
        // The first diagnostics mean the file is parsed.
        let first = smol::block_on(smol::future::or(async { rx.recv().await.ok() }, async {
            smol::Timer::after(std::time::Duration::from_secs(30)).await;
            None
        }));
        if let Some(super::LspEvent::Diagnostics { diagnostics, .. }) = &first {
            eprintln!("FIRST {:?}", diagnostics.iter().map(|d| d.message.clone()).collect::<Vec<_>>());
        }
        let labels = |line: u32, character: u32| {
            let v = smol::block_on(client.request("textDocument/completion", serde_json::json!({"textDocument": {"uri": super::uri(&main)}, "position": {"line": line, "character": character}}))).unwrap();
            eprintln!("RAW {}", v.to_string().chars().take(300).collect::<String>());
            v["items"].as_array().cloned().unwrap_or_default().iter().map(|i| i["filterText"].as_str().or(i["label"].as_str()).unwrap_or("").to_string()).collect::<Vec<_>>()
        };
        let members = labels(3, 7);
        eprintln!("MEMBERS {members:?}");
        assert!(members.iter().any(|l| l.contains("radius_value")), "member of a library class");
        let types = labels(2, 9);
        eprintln!("TYPES {:?}", types.iter().take(5).collect::<Vec<_>>());
        assert!(types.iter().any(|l| l.contains("Circle")), "class of the library");
        std::fs::remove_dir_all(root).unwrap();
    }
}
