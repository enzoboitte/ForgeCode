//! Agent modules (Hermes): ForgeCode installs the agent itself, in the background, into the
//! module's `runtime/` folder (removing the module removes it), then starts it as an ACP server
//! wired to the local AI gateway, so it uses the models ForgeCode is signed in to.

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

use super::{AgentSpec, Plugin};
use crate::lsp::install::{tools_dir, unpack_url, which};

pub fn runtime_dir(plugin: &Plugin) -> PathBuf {
    plugin.dir.join("runtime")
}

/// Where a `source` agent is unpacked.
pub fn checkout(plugin: &Plugin) -> PathBuf {
    runtime_dir(plugin).join("src")
}

/// The agent's executable once installed by ForgeCode.
pub fn program(plugin: &Plugin, spec: &AgentSpec) -> PathBuf {
    let name = spec.command.first().map(String::as_str).unwrap_or("");
    if spec.source.is_some() {
        let path = checkout(plugin).join(name);
        // `.venv/bin/hermes` is `.venv\Scripts\hermes.exe` on Windows.
        return if cfg!(windows) { PathBuf::from(path.display().to_string().replace("/bin/", "/Scripts/") + ".exe") } else { path };
    }
    runtime_dir(plugin).join("bin").join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// An agent the user already installed (on PATH, or the usual `~/.local/bin`), if it speaks ACP.
pub fn system_program(spec: &AgentSpec) -> Option<PathBuf> {
    // The executable's name (`hermes` for `.venv/bin/hermes`).
    let name = Path::new(spec.command.first()?).file_name()?.to_str()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let found = which(name).or_else(|| home.map(|h| h.join(".local/bin").join(name)).filter(|p| p.is_file()))?;
    // `<agent> acp --version` fails when its ACP extra isn't installed.
    let mut check = crate::util::command(&found);
    check.args(spec.command[1..].iter()).arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    check.status().ok().filter(|s| s.success()).map(|_| found)
}

/// The agent to run: the user's own install first, else the module's.
pub fn resolve(plugin: &Plugin, spec: &AgentSpec) -> Option<PathBuf> {
    system_program(spec).or_else(|| Some(program(plugin, spec)).filter(|p| p.is_file()))
}

pub fn is_installed(plugin: &Plugin, spec: &AgentSpec) -> bool {
    resolve(plugin, spec).is_some()
}

/// Whether process `pid` is still running.
fn alive(pid: u32) -> bool {
    if cfg!(windows) {
        crate::util::command("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
    } else {
        crate::util::command("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }
}

/// Conversations saved while the gateway had another local address (its port was taken, or an
/// older ForgeCode used a new port each run) are pointed at the current one, else the agent
/// would call a dead endpoint when they are reopened. Run while the agent is stopped.
pub fn repoint_sessions(plugin: &Plugin, base_url: &str) {
    let db = agent_home(plugin).join("state.db");
    repoint_db(&checkout(plugin).join(if cfg!(windows) { "venv/Scripts/python.exe" } else { "venv/bin/python" }), &db, base_url);
}

fn repoint_db(python: &Path, db: &Path, base_url: &str) {
    const SCRIPT: &str = r#"
import json, sqlite3, sys
db, url = sys.argv[1], sys.argv[2]
c = sqlite3.connect(db, timeout=5)
local = lambda u: isinstance(u, str) and u.startswith("http://127.0.0.1:") and u != url
n = 0
for sid, mc, billing in c.execute("select id, model_config, billing_base_url from sessions").fetchall():
    try:
        meta = json.loads(mc) if mc else None
    except ValueError:
        meta = None
    changed = False
    if isinstance(meta, dict) and local(meta.get("base_url")):
        meta["base_url"] = url
        changed = True
    if changed or local(billing):
        c.execute("update sessions set model_config = ?, billing_base_url = ? where id = ?",
                  (json.dumps(meta) if changed else mc, url if local(billing) else billing, sid))
        n += 1
c.commit()
print(n)
"#;
    if db.is_file() && python.is_file() {
        let _ = crate::util::command(python).arg("-c").arg(SCRIPT).arg(db).arg(base_url).stdin(Stdio::null()).output();
    }
}

/// Remember which agent process this ForgeCode started (`<agent pid> <forgecode pid>`).
pub fn record_agent(plugin: &Plugin, agent_pid: u32) {
    let _ = std::fs::write(runtime_dir(plugin).join("agent.pid"), format!("{agent_pid} {}", std::process::id()));
}

/// An agent left running by a ForgeCode that crashed or was killed holds the conversation (the
/// next agent waits on it forever): stop it. Returns its pid if one was stopped.
pub fn stop_orphan(plugin: &Plugin) -> Option<u32> {
    let text = std::fs::read_to_string(runtime_dir(plugin).join("agent.pid")).ok()?;
    let mut parts = text.split_whitespace().filter_map(|p| p.parse::<u32>().ok());
    let (agent, owner) = (parts.next()?, parts.next()?);
    if owner == std::process::id() || alive(owner) || !alive(agent) {
        return None;
    }
    // Only if that pid is still the agent (pids are reused).
    let command = crate::util::command("ps").args(["-o", "command=", "-p", &agent.to_string()]).output().ok()?;
    if !String::from_utf8_lossy(&command.stdout).contains(&runtime_dir(plugin).display().to_string()) {
        return None;
    }
    let _ = if cfg!(windows) {
        crate::util::command("taskkill").args(["/F", "/PID", &agent.to_string()]).status()
    } else {
        crate::util::command("kill").arg(agent.to_string()).status()
    };
    Some(agent)
}

/// The ForgeCode process installing into `runtime`, if one still is (another window or instance).
fn installer(runtime: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(runtime.join(".installing")).ok()?.trim().parse().ok()?;
    (pid != std::process::id() && alive(pid)).then_some(pid)
}

/// uv from PATH or ForgeCode's tools, else its official standalone build, downloaded once.
fn uv() -> Result<PathBuf> {
    if let Some(uv) = which("uv") {
        return Ok(uv);
    }
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", _) => "x86_64-apple-darwin",
        ("windows", _) => "x86_64-pc-windows-msvc",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        _ => "x86_64-unknown-linux-gnu",
    };
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let name = format!("uv-{triple}.{ext}");
    unpack_url(&format!("https://github.com/astral-sh/uv/releases/latest/download/{name}"), &name)?;
    // The archive holds `uv-<triple>/uv` (bare `uv.exe` on Windows).
    let unpacked = tools_dir().join(format!("uv-{triple}"));
    let exe = format!("uv{}", std::env::consts::EXE_SUFFIX);
    [unpacked.join(&exe), tools_dir().join(&exe)].into_iter().find(|p| p.is_file()).context("uv not found in its download")
}

/// Environment that keeps uv's tools and Python inside the module.
fn uv_env(runtime: &Path) -> Vec<(String, PathBuf)> {
    vec![
        ("UV_TOOL_DIR".into(), runtime.join("tools")),
        ("UV_TOOL_BIN_DIR".into(), runtime.join("bin")),
        ("UV_PYTHON_INSTALL_DIR".into(), runtime.join("python")),
        ("UV_CACHE_DIR".into(), tools_dir().join("uv-cache")),
    ]
}

/// Modules being installed: a second start waits for the running install instead of racing it
/// (both would block on uv's cache lock).
static INSTALLING: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Install the agent; `progress` gets each output line. Blocking: run in the background.
pub fn install(plugin: &Plugin, spec: &AgentSpec, mut progress: impl FnMut(&str)) -> Result<()> {
    let runtime = runtime_dir(plugin);
    // An install already running, here or in another ForgeCode: wait for it instead of a second one.
    let busy = |r: &Path| INSTALLING.lock().unwrap().iter().any(|x| x == r) || installer(r).is_some();
    if busy(&runtime) {
        progress(&format!("{} {}…", spec.name, "install already in progress, waiting"));
        while busy(&runtime) {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        return if is_installed(plugin, spec) { Ok(()) } else { bail!("{} could not be installed", spec.name) };
    }
    INSTALLING.lock().unwrap().push(runtime.clone());
    std::fs::create_dir_all(&runtime)?;
    let _ = std::fs::write(runtime.join(".installing"), std::process::id().to_string());
    let result = install_now(plugin, spec, &runtime, &mut progress);
    let _ = std::fs::remove_file(runtime.join(".installing"));
    INSTALLING.lock().unwrap().retain(|r| *r != runtime);
    result
}

fn install_now(plugin: &Plugin, spec: &AgentSpec, runtime: &Path, progress: &mut impl FnMut(&str)) -> Result<()> {
    if let Some(source) = &spec.source {
        return install_source(plugin, spec, source, runtime, progress);
    }
    std::fs::create_dir_all(runtime)?;
    progress("uv…");
    let uv = uv()?;
    let mut child = crate::util::command(uv)
        .args(["tool", "install", "--force", "--python", &spec.python, &spec.package])
        .envs(uv_env(runtime))
        .env("PATH", crate::lsp::install::search_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start uv")?;
    // uv reports on stderr; read both without blocking either.
    let stdout = child.stdout.take().context("no stdout")?;
    let out_thread = std::thread::spawn(move || BufReader::new(stdout).lines().map_while(Result::ok).collect::<Vec<_>>());
    let mut last = vec![];
    for line in BufReader::new(child.stderr.take().context("no stderr")?).lines().map_while(Result::ok) {
        progress(&line);
        last.push(line);
        if last.len() > 20 {
            last.remove(0);
        }
    }
    let _ = out_thread.join();
    if !child.wait()?.success() {
        bail!("{}", last.join("\n"));
    }
    if !program(plugin, spec).is_file() {
        bail!("{} was installed but `{}` is missing", spec.name, program(plugin, spec).display());
    }
    Ok(())
}

/// Run `cmd`, streaming its output lines (stdout and stderr) to `progress`; fail with its last lines.
fn run_streaming(mut cmd: Command, progress: &mut impl FnMut(&str)) -> Result<String> {
    // No terminal input: a tool waiting for a password or a confirmation would hang forever.
    let mut child = cmd
        .stdin(Stdio::null())
        .env("UV_NO_PROGRESS", "1")
        .env("UV_HTTP_TIMEOUT", "120")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {cmd:?}"))?;
    // Watchdog: silent for too long means stuck; stop it and say so.
    let last_output = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let killed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (last_output, done, killed, pid) = (last_output.clone(), done.clone(), killed.clone(), child.id());
        std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(5));
                if last_output.lock().unwrap().elapsed() > STALL {
                    killed.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = if cfg!(windows) {
                        crate::util::command("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).status()
                    } else {
                        crate::util::command("kill").arg(pid.to_string()).status()
                    };
                    break;
                }
            }
        });
    }
    let stdout = child.stdout.take().context("no stdout")?;
    let out_thread = {
        let last_output = last_output.clone();
        std::thread::spawn(move || {
            BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .inspect(|_| *last_output.lock().unwrap() = std::time::Instant::now())
                .collect::<Vec<_>>()
        })
    };
    let mut last = vec![];
    for line in BufReader::new(child.stderr.take().context("no stderr")?).lines().map_while(Result::ok) {
        *last_output.lock().unwrap() = std::time::Instant::now();
        progress(&line);
        last.push(line);
        if last.len() > 20 {
            last.remove(0);
        }
    }
    let out = out_thread.join().unwrap_or_default();
    for line in out.iter().rev().take(5).rev() {
        progress(line);
    }
    let status = child.wait()?;
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    if killed.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("{} ({} min): {}", "stopped after no progress", STALL.as_secs() / 60, last.join("\n"));
    }
    if !status.success() {
        let tail: Vec<String> = out.iter().rev().take(10).rev().cloned().chain(last).collect();
        bail!("{}", tail.join("\n"));
    }
    Ok(out.join("\n"))
}

/// A setup step printing nothing for this long is considered stuck.
const STALL: std::time::Duration = std::time::Duration::from_secs(600);

/// `{uv}`, `{python}` and `{src}` in a setup command.
fn expand(text: &str, uv: &Path, python: &str, src: &Path) -> String {
    text.replace("{uv}", &uv.display().to_string()).replace("{python}", python).replace("{src}", &src.display().to_string())
}

/// Agents that run from a source checkout (Hermes refuses wheel installs): unpack the release
/// archive, get the Python it wants through uv, run its own setup, and record its environment.
fn install_source(plugin: &Plugin, spec: &AgentSpec, source: &str, runtime: &Path, progress: &mut impl FnMut(&str)) -> Result<()> {
    let src = checkout(plugin);
    progress(&format!("↓ {source}"));
    let unpack = runtime.join("unpack");
    let _ = std::fs::remove_dir_all(&unpack);
    std::fs::create_dir_all(&unpack)?;
    crate::lsp::install::unpack_url_into(source, "source.tar.gz", &unpack)?;
    // GitHub archives hold one top folder (`hermes-agent-2026.9.24/`).
    let top = std::fs::read_dir(&unpack)?.flatten().map(|e| e.path()).find(|p| p.is_dir()).context("empty source archive")?;
    let _ = std::fs::remove_dir_all(&src);
    std::fs::rename(&top, &src)?;
    let _ = std::fs::remove_dir_all(&unpack);

    let uv = uv()?;
    for step in &spec.setup {
        let args: Vec<String> = step.iter().map(|a| expand(a, &uv, &spec.python, &src)).collect();
        let Some((program, args)) = args.split_first() else { continue };
        progress(&format!("$ {} {}", Path::new(program).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), args.join(" ")));
        let mut c = crate::util::command(program);
        c.args(args)
            .current_dir(&src)
            // uv's Pythons and cache stay in ForgeCode's folders.
            .envs(uv_env(runtime))
            .env("HERMES_HOME", agent_home(plugin))
            .env("PATH", crate::lsp::install::search_path())
            .env_remove("PYTHONHOME")
            .env_remove("PYTHONPATH")
            .env_remove("VIRTUAL_ENV")
            .env_remove("UV_PYTHON");
        for (k, v) in &spec.setup_env {
            c.env(k, expand(v, &uv, &spec.python, &src));
        }
        run_streaming(c, progress)?;
    }
    if let Some((program, args)) = spec.environment.split_first() {
        let program = expand(program, &uv, &spec.python, &src);
        let mut c = crate::util::command(program);
        c.args(args.iter().map(|a| expand(a, &uv, &spec.python, &src))).current_dir(&src).env("PYTHONPATH", &src).env("HERMES_HOME", agent_home(plugin));
        let json = run_streaming(c, &mut |_| {})?;
        // Only the last line is the JSON object (a tool may log before it).
        let line = json.lines().rev().find(|l| l.trim_start().starts_with('{')).context("no environment printed")?;
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(line).context("invalid environment JSON")?;
        std::fs::write(runtime.join("env.json"), line)?;
    }
    if !program(plugin, spec).is_file() {
        bail!("{} was set up but `{}` is missing", spec.name, program(plugin, spec).display());
    }
    Ok(())
}

/// The environment the agent's own setup recorded (its PATH and tool variables).
fn recorded_env(plugin: &Plugin) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(runtime_dir(plugin).join("env.json")) else { return vec![] };
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text) else { return vec![] };
    map.into_iter().filter_map(|(k, v)| v.as_str().map(|v| (k, v.to_string()))).collect()
}

/// The module's Python (`runtime/venv`), once its packages are installed.
pub fn module_python(plugin: &Plugin) -> PathBuf {
    let venv = runtime_dir(plugin).join("venv");
    if cfg!(windows) { venv.join("Scripts").join("python.exe") } else { venv.join("bin").join("python") }
}

/// Create the module's Python environment and install its packages (once; again when the
/// package list changes). `progress` gets the output lines.
pub fn ensure_python(plugin: &Plugin, spec: &super::PythonSpec, mut progress: impl FnMut(&str)) -> Result<PathBuf> {
    let runtime = runtime_dir(plugin);
    let python = module_python(plugin);
    let stamp = runtime.join("python-packages.txt");
    let wanted = format!("{}\n{}", spec.version, spec.packages.join("\n"));
    if python.is_file() && std::fs::read_to_string(&stamp).is_ok_and(|s| s == wanted) {
        return Ok(python);
    }
    std::fs::create_dir_all(&runtime)?;
    let uv = uv()?;
    let mut venv = crate::util::command(&uv);
    venv.args(["venv", "--allow-existing", "--python", &spec.version]).arg(runtime.join("venv")).envs(uv_env(&runtime)).env("PATH", crate::lsp::install::search_path());
    run_streaming(venv, &mut progress)?;
    if !spec.packages.is_empty() {
        let mut pip = crate::util::command(&uv);
        pip.args(["pip", "install", "--python"]).arg(&python).args(&spec.packages).envs(uv_env(&runtime)).env("PATH", crate::lsp::install::search_path());
        run_streaming(pip, &mut progress)?;
    }
    std::fs::write(stamp, wanted)?;
    Ok(python)
}

/// Where the agent keeps its state (memory, skills, sessions): ForgeCode's own, not `~/.hermes`.
pub fn agent_home(plugin: &Plugin) -> PathBuf {
    crate::settings::config_dir().join("agents").join(plugin.dir.file_name().unwrap_or_default())
}

/// The model block of Hermes' `config.yaml`, pointing at the local gateway.
pub fn hermes_config(base_url: &str, key: &str, model: &str, context_tokens: u32) -> String {
    format!(
        "# Written by ForgeCode on every start: models come from ForgeCode (Settings › AI › Agent).\n\
         model:\n  provider: custom\n  base_url: {base_url}\n  api_key: {key}\n  default: {model}\n  context_length: {context_tokens}\n\
         compression:\n  enabled: true\n"
    )
}

/// Write the agent's configuration, keeping the user's own additions below ForgeCode's block.
pub fn write_config(home: &Path, managed: &str) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let path = home.join("config.yaml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    // Everything after ForgeCode's block is the user's (or the agent's) and is kept.
    let rest: String = existing
        .lines()
        .skip_while(|l| l.starts_with('#') || l.starts_with(' ') || l.trim().is_empty() || l.starts_with("model:") || l.starts_with("compression:"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(path, format!("{managed}{rest}"))?;
    Ok(())
}

const GUIDANCE_START: &str = "<!-- forgecode:start -->";
const GUIDANCE_END: &str = "<!-- forgecode:end -->";

/// The persona text with ForgeCode's standing instructions as its last block (replaced, never doubled).
pub fn with_guidance(persona: &str, guidance: &str) -> String {
    let own = match (persona.find(GUIDANCE_START), persona.find(GUIDANCE_END)) {
        (Some(a), Some(b)) if a < b => format!("{}{}", &persona[..a], &persona[b + GUIDANCE_END.len()..]),
        _ => persona.to_string(),
    };
    format!("{}\n\n{GUIDANCE_START}\n{guidance}\n{GUIDANCE_END}\n", own.trim_end())
}

/// Write ForgeCode's standing instructions in the agent's persona file (`SOUL.md`), keeping the rest.
pub fn write_guidance(home: &Path, guidance: &str) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let path = home.join("SOUL.md");
    let persona = std::fs::read_to_string(&path).unwrap_or_default();
    let next = with_guidance(&persona, guidance);
    if next != persona {
        std::fs::write(path, next)?;
    }
    Ok(())
}

/// Command and environment starting the agent's ACP server.
pub fn launch(plugin: &Plugin, spec: &AgentSpec) -> (Vec<String>, Vec<(String, String)>) {
    let mut argv: Vec<String> = spec.command.clone();
    if let Some(first) = argv.first_mut() {
        *first = resolve(plugin, spec).unwrap_or_else(|| program(plugin, spec)).display().to_string();
    }
    let bin = runtime_dir(plugin).join("bin");
    // The agent's recorded environment (PATH of its tools…), with ForgeCode's tools in front.
    let mut env: Vec<(String, String)> = recorded_env(plugin).into_iter().filter(|(k, _)| k != "HERMES_HOME").collect();
    let path = env.iter().position(|(k, _)| k == "PATH").map(|i| env.remove(i).1);
    let mut dirs = vec![bin, tools_dir().join("bin")];
    if let Some(path) = path {
        dirs.extend(std::env::split_paths(&path));
    }
    env.push(("PATH".into(), crate::util::path_with(dirs)));
    env.push(("HERMES_HOME".into(), agent_home(plugin).display().to_string()));
    (argv, env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_is_added_once_and_keeps_the_persona() {
        let once = with_guidance("You are Hermes.", "Search the web.");
        assert!(once.starts_with("You are Hermes.\n\n<!-- forgecode:start -->\nSearch the web.\n"));
        let twice = with_guidance(&once, "Always search the web.");
        assert_eq!(twice.matches("forgecode:start").count(), 1);
        assert!(twice.contains("Always search the web.") && twice.starts_with("You are Hermes."));
        assert_eq!(with_guidance(&twice, "Always search the web."), twice);
    }

    #[test]
    fn source_agents_run_from_their_checkout() {
        let plugin = Plugin { dir: PathBuf::from("/m/hermes"), manifest: toml::from_str("name = \"h\"").unwrap() };
        let spec = AgentSpec {
            name: "Hermes".into(),
            package: String::new(),
            source: Some("https://x/v1.tar.gz".into()),
            setup: vec![vec!["{uv}".into(), "venv".into(), "venv".into(), "--python".into(), "{python}".into()]],
            setup_env: [("UV_PROJECT_ENVIRONMENT".to_string(), "{src}/venv".to_string())].into(),
            environment: vec![],
            python: "3.11".into(),
            command: vec!["venv/bin/hermes".into(), "acp".into()],
            install_note: String::new(),
        };
        if !cfg!(windows) {
            assert_eq!(program(&plugin, &spec), PathBuf::from("/m/hermes/runtime/src/venv/bin/hermes"));
        }
        assert_eq!(expand(&spec.setup[0][4], Path::new("/uv"), &spec.python, Path::new("/s")), "3.11");
        assert_eq!(expand("{src}/venv", Path::new("/uv"), &spec.python, Path::new("/s")), "/s/venv");
        let (argv, env) = launch(&plugin, &spec);
        assert_eq!(argv[1], "acp");
        assert!(env.iter().any(|(k, v)| k == "HERMES_HOME" && v.ends_with("agents/hermes")));
    }

    #[test]
    #[cfg(unix)]
    fn stops_only_orphaned_agents_of_this_module() {
        let dir = std::env::temp_dir().join(format!("forge-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let plugin = Plugin { dir: dir.clone(), manifest: toml::from_str("name = \"h\"").unwrap() };
        std::fs::create_dir_all(runtime_dir(&plugin)).unwrap();
        // A dead ForgeCode: a pid no process can have (a real exited pid may be reused meanwhile).
        struct Gone;
        impl Gone {
            fn id(&self) -> u32 {
                0x7fff_fff0
            }
        }
        let gone = Gone;
        // Not this module's agent: left alone even though its owner is dead.
        let mut other = crate::util::command("sleep").arg("5").spawn().unwrap();
        std::fs::write(runtime_dir(&plugin).join("agent.pid"), format!("{} {}", other.id(), gone.id())).unwrap();
        assert_eq!(stop_orphan(&plugin), None);
        other.kill().unwrap();
        other.wait().unwrap();
        // The module's agent (run from its runtime folder) whose owner is dead: stopped.
        let fake = runtime_dir(&plugin).join("sleep");
        std::fs::copy("/bin/sleep", &fake).unwrap();
        let mut agent = crate::util::command(&fake).arg("5").spawn().unwrap();
        std::fs::write(runtime_dir(&plugin).join("agent.pid"), format!("{} {}", agent.id(), gone.id())).unwrap();
        assert_eq!(stop_orphan(&plugin), Some(agent.id()));
        assert!(!agent.wait().unwrap().success(), "killed");
        // Owned by this ForgeCode: never stopped.
        let mut mine = crate::util::command(&fake).arg("5").spawn().unwrap();
        record_agent(&plugin, mine.id());
        assert_eq!(stop_orphan(&plugin), None);
        mine.kill().unwrap();
        mine.wait().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repoints_saved_conversations_to_the_gateway() {
        let dir = std::env::temp_dir().join(format!("forge-repoint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        let py = Path::new("/usr/bin/python3");
        let run = |code: &str| String::from_utf8(crate::util::command(py).args(["-c", code]).arg(&db).output().unwrap().stdout).unwrap();
        run("import sqlite3,sys,json; c=sqlite3.connect(sys.argv[1]); c.execute('create table sessions(id text, model_config text, billing_base_url text)'); c.execute('insert into sessions values (?,?,?)', ('a', json.dumps({'base_url':'http://127.0.0.1:57550/v1','cwd':'/p'}), 'http://127.0.0.1:57550/v1')); c.execute('insert into sessions values (?,?,?)', ('b', json.dumps({'base_url':'https://api.example.com/v1'}), None)); c.commit()");
        repoint_db(py, &db, "http://127.0.0.1:59071/v1");
        let out = run("import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('select id, model_config, billing_base_url from sessions order by id').fetchall())");
        assert!(out.contains("59071") && !out.contains("57550"), "{out}");
        assert!(out.contains("/p") && out.contains("https://api.example.com/v1"), "other fields and remote endpoints untouched: {out}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn waits_only_for_a_live_installer() {
        let runtime = std::env::temp_dir().join(format!("forge-runtime-{}", std::process::id()));
        std::fs::create_dir_all(&runtime).unwrap();
        assert_eq!(installer(&runtime), None);
        std::fs::write(runtime.join(".installing"), std::process::id().to_string()).unwrap();
        assert_eq!(installer(&runtime), None, "our own lock");
        let mut other = crate::util::command("sleep").arg("5").spawn().unwrap();
        std::fs::write(runtime.join(".installing"), other.id().to_string()).unwrap();
        assert_eq!(installer(&runtime), Some(other.id()), "another live process");
        other.kill().unwrap();
        other.wait().unwrap();
        assert_eq!(installer(&runtime), None, "a dead installer's lock is stale");
        std::fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn config_keeps_user_settings_below_the_managed_block() {
        let home = std::env::temp_dir().join(format!("forge-agent-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        write_config(&home, &hermes_config("http://127.0.0.1:1/v1", "k", "copilot/gpt-4.1", 250_000)).unwrap();
        let path = home.join("config.yaml");
        let mut text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("  default: copilot/gpt-4.1\n") && text.contains("context_length: 250000"));
        text.push_str("agent:\n  max_turns: 90\n");
        std::fs::write(&path, text).unwrap();
        write_config(&home, &hermes_config("http://127.0.0.1:2/v1", "k", "lm/qwen", 128_000)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("127.0.0.1:2") && text.contains("default: lm/qwen") && !text.contains("127.0.0.1:1"));
        assert!(text.ends_with("agent:\n  max_turns: 90\n"), "{text}");
        std::fs::remove_dir_all(home).unwrap();
    }
}
