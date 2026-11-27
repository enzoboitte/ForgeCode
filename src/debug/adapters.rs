//! Which debug adapter debugs which language, how to get it and how to launch the program.
//!
//! | Languages | Adapter | Got from |
//! |---|---|---|
//! | C, C++, Rust, Zig, Swift | lldb-dap | Xcode / LLVM |
//! | Python | debugpy | pip, in a venv under `<config>/servers` |
//! | JavaScript, TypeScript | js-debug | GitHub release, run with Node |
//! | PHP | vscode-php-debug (needs Xdebug in PHP) | GitHub release, run with Node |
//! | Go | delve (`dlv dap`) | `go install` |

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::lsp::install::{search_path, tools_dir, which};
use crate::project::{BuildSystem, Program, detect};
use crate::settings::LangSettings;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Adapter {
    Lldb,
    Python,
    Node,
    Php,
    Go,
}

/// How to debug a file: optional build step, the program, and the adapter.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub build: Option<String>,
    pub program: Program,
    pub adapter: Adapter,
}

pub fn plan(root: &Path, file: &Path, lsp_id: &str) -> Option<Plan> {
    let script = |adapter| Some(Plan { build: None, program: Program::Path(file.display().to_string()), adapter });
    let stem = file.file_stem()?.to_string_lossy().to_string();
    let exe = std::env::temp_dir().join(format!("forgecode-debug-{stem}{}", std::env::consts::EXE_SUFFIX)).display().to_string();
    let native = |build: String, program| Some(Plan { build: Some(build), program, adapter: Adapter::Lldb });
    match (lsp_id, detect(root)) {
        ("rust", _) => native("cargo build --message-format=json-render-diagnostics".into(), Program::FromCargo),
        ("c" | "cpp", BuildSystem::CMake) => native(crate::project::CMAKE_BUILD.into(), Program::Newest(root.join("build"))),
        ("c" | "cpp", BuildSystem::Make) => native("make".into(), Program::Newest(root.to_path_buf())),
        ("c", _) => native(format!("cc -g -O0 {} \"{}\" -o \"{exe}\"", crate::language::include_flags(root), file.display()), Program::Path(exe)),
        ("cpp", _) => native(format!("c++ -std=c++20 -g -O0 {} \"{}\" -o \"{exe}\"", crate::language::include_flags(root), file.display()), Program::Path(exe)),
        ("zig", _) => native(format!("zig build-exe -O Debug \"{}\" -femit-bin=\"{exe}\"", file.display()), Program::Path(exe)),
        ("swift", _) => native(format!("swiftc -g -Onone \"{}\" -o \"{exe}\"", file.display()), Program::Path(exe)),
        ("python", _) => script(Adapter::Python),
        ("javascript" | "javascriptreact" | "typescript" | "typescriptreact", _) => script(Adapter::Node),
        ("php", _) => script(Adapter::Php),
        ("go", _) => script(Adapter::Go),
        _ => None,
    }
}

/// The adapter's command line (`{port}` for TCP adapters) and whether it listens on TCP.
/// Installs the adapter when missing. Blocking.
pub fn command(adapter: Adapter, settings: &LangSettings, root: &Path) -> Result<(Vec<String>, bool)> {
    Ok(match adapter {
        Adapter::Lldb => (super::dap::adapter_command(), false),
        Adapter::Python => {
            let venv = tools_dir().join("debugpy");
            let python = venv_python(&venv);
            if !python.is_file() || run(crate::util::command(&python).args(["-c", "import debugpy"])).is_err() {
                let base = python_interpreter(settings, root).context("Python is not installed (python3 / python)")?;
                run(crate::util::command(&base).args(["-m", "venv"]).arg(&venv))?;
                run(crate::util::command(&python).args(["-m", "pip", "install", "--quiet", "debugpy"]))?;
            }
            (vec![python.display().to_string(), "-m".into(), "debugpy.adapter".into()], false)
        }
        Adapter::Node => {
            let server = tools_dir().join("js-debug").join("src").join("dapDebugServer.js");
            if !server.is_file() {
                let url = github_asset("microsoft/vscode-js-debug", |n| n.starts_with("js-debug-dap-") && n.ends_with(".tar.gz"))?;
                crate::lsp::install::unpack_url(&url, "js-debug.tar.gz")?;
            }
            (vec![node()?, server.display().to_string(), "{port}".into(), "127.0.0.1".into()], true)
        }
        Adapter::Php => {
            let dir = tools_dir().join("php-debug");
            let script = dir.join("extension").join("out").join("phpDebug.js");
            if !script.is_file() {
                let url = github_asset("xdebug/vscode-php-debug", |n| n.ends_with(".vsix"))?;
                std::fs::create_dir_all(&dir)?;
                crate::lsp::install::unpack_url_into(&url, "php-debug.zip", &dir)?;
            }
            let php = if settings.runtime.is_empty() { "php".to_string() } else { settings.runtime.clone() };
            let modules = run(crate::util::command(&php).arg("-m")).context("PHP is not installed")?;
            if !modules.to_lowercase().contains("xdebug") {
                bail!("PHP debugging needs the Xdebug extension: install it (`pecl install xdebug`, or your package manager / https://xdebug.org/wizard), then enable xdebug.mode=debug");
            }
            (vec![node()?, script.display().to_string()], false)
        }
        Adapter::Go => {
            if which("dlv").is_none() {
                let go = which("go").context("Go is not installed")?;
                run(crate::util::command(go).args(["install", "github.com/go-delve/delve/cmd/dlv@latest"]).env("GOBIN", tools_dir().join("bin")))?;
            }
            let dlv = which("dlv").context("dlv not found after installing it")?;
            (vec![dlv.display().to_string(), "dap".into(), "--listen=127.0.0.1:{port}".into()], true)
        }
    })
}

/// The `launch` request arguments.
pub fn launch_args(adapter: Adapter, program: &str, root: &Path, settings: &LangSettings) -> Value {
    let cwd = root.display().to_string();
    let args: Vec<String> = settings.args.split_whitespace().map(String::from).collect();
    let mut launch = json!({"program": program, "cwd": cwd, "args": args, "stopOnEntry": settings.stop_on_entry});
    match adapter {
        Adapter::Lldb | Adapter::Go => {
            if adapter == Adapter::Go {
                launch["mode"] = "debug".into();
            }
        }
        Adapter::Python => {
            launch["console"] = "internalConsole".into();
            launch["justMyCode"] = settings.just_my_code.into();
            if let Some(python) = python_interpreter(settings, root) {
                launch["python"] = python.display().to_string().into();
            }
        }
        Adapter::Node => {
            launch["type"] = "pwa-node".into();
            launch["console"] = "internalConsole".into();
            launch["outputCapture"] = "std".into();
            launch["sourceMaps"] = true.into();
            if !settings.runtime.is_empty() {
                launch["runtimeExecutable"] = settings.runtime.clone().into();
            }
            if matches!(Path::new(program).extension().and_then(|e| e.to_str()), Some("ts" | "mts" | "cts" | "tsx")) {
                // A project's tsx, else Node's own type stripping (Node 22.6+).
                let runtime_args = if root.join("node_modules/tsx").is_dir() { ["--import", "tsx"].as_slice() } else { ["--experimental-strip-types"].as_slice() };
                launch["runtimeArgs"] = json!(runtime_args);
            }
        }
        Adapter::Php => {
            launch["port"] = 9003.into();
            if !settings.runtime.is_empty() {
                launch["runtimeExecutable"] = settings.runtime.clone().into();
            }
        }
    }
    launch
}

/// The configured interpreter, the project's virtualenv, or the system Python.
fn python_interpreter(settings: &LangSettings, root: &Path) -> Option<PathBuf> {
    crate::language::project_python(settings, root).or_else(|| which("python3").or_else(|| which("python")))
}

use crate::language::venv_python;

fn node() -> Result<String> {
    Ok(which("node").or_else(|| crate::lsp::install::ensure_node().ok()).context("Node.js is not installed")?.display().to_string())
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.env("PATH", search_path()).output().with_context(|| format!("failed to run {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Download URL of the latest release asset of `repo` whose name matches.
fn github_asset(repo: &str, matches: impl Fn(&str) -> bool) -> Result<String> {
    let release: Value = ureq::get(&format!("https://api.github.com/repos/{repo}/releases/latest")).call()?.into_json()?;
    release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["browser_download_url"].as_str())
        .find(|u| u.rsplit('/').next().is_some_and(&matches))
        .map(String::from)
        .with_context(|| format!("no matching release asset in {repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_an_adapter_per_language() {
        let root = Path::new("/nonexistent");
        let adapter = |file: &str, id: &str| plan(root, Path::new(file), id).map(|p| p.adapter);
        assert_eq!(adapter("/w/a.py", "python"), Some(Adapter::Python));
        assert_eq!(adapter("/w/a.ts", "typescript"), Some(Adapter::Node));
        assert_eq!(adapter("/w/a.php", "php"), Some(Adapter::Php));
        assert_eq!(adapter("/w/a.go", "go"), Some(Adapter::Go));
        assert_eq!(adapter("/w/a.c", "c"), Some(Adapter::Lldb));
        assert_eq!(adapter("/w/a.txt", "plaintext"), None);
        let settings = LangSettings { args: "-v x".into(), stop_on_entry: true, ..Default::default() };
        let launch = launch_args(Adapter::Node, "/w/a.ts", root, &settings);
        assert_eq!((launch["type"].as_str(), launch["args"][1].as_str(), launch["stopOnEntry"].as_bool()), (Some("pwa-node"), Some("x"), Some(true)));
        assert_eq!(launch["runtimeArgs"][0], "--experimental-strip-types");
        assert_eq!(launch_args(Adapter::Python, "/w/a.py", root, &settings)["justMyCode"], true);
    }

    /// End-to-end with debugpy (installs it, needs Python and network): `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn debugpy_stops_at_breakpoint() {
        use crate::debug::dap::{DapClient, DapEvent};
        let dir = std::env::temp_dir().join(format!("forge-py-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("t.py");
        std::fs::write(&src, "answer = 41\nanswer += 1\nprint(answer)\n").unwrap();
        let settings = LangSettings::default();
        let (argv, tcp) = command(Adapter::Python, &settings, &dir).unwrap();
        assert!(!tcp);
        let (tx, rx) = smol::channel::unbounded();
        let client = DapClient::start(&argv, &dir, tx).unwrap();
        smol::block_on(async {
            client.request("initialize", json!({"adapterID": "debugpy", "linesStartAt1": true, "pathFormat": "path"})).await.unwrap();
            let launch = { let c = client.clone(); let args = launch_args(Adapter::Python, &src.display().to_string(), &dir, &settings); smol::spawn(async move { c.request("launch", args).await }) };
            loop { if let DapEvent::Initialized = rx.recv().await.unwrap() { break } }
            client.request("setBreakpoints", json!({"source": {"path": src.display().to_string()}, "breakpoints": [{"line": 3}]})).await.unwrap();
            client.request("configurationDone", json!({})).await.unwrap();
            launch.await.unwrap();
            let thread_id = loop { if let DapEvent::Stopped { thread_id, .. } = rx.recv().await.unwrap() { break thread_id } };
            let st = client.request("stackTrace", json!({"threadId": thread_id})).await.unwrap();
            assert_eq!(st["stackFrames"][0]["line"], 3);
            let _ = client.request("disconnect", json!({"terminateDebuggee": true})).await;
        });
    }
}
