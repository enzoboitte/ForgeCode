//! Tools the agent can call, in the spirit of Claude Code's toolset.

use std::io::Read;
use std::process::Child;
use std::sync::{Arc, Mutex, OnceLock};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::plugins::Tool as PluginTool;

/// Tool output is cut to this many characters so small context windows survive.
pub const MAX_OUTPUT: usize = 8_000;

#[derive(Clone)]
pub struct Toolbox {
    pub workspace: PathBuf,
    /// (plugin dir, tool)
    pub plugin_tools: Vec<(PathBuf, PluginTool)>,
    /// Only these tools are offered (sub-agent roles, plan mode); None = all.
    pub allowed: Option<Vec<&'static str>>,
    /// Files as they were before this turn changed them, to undo it.
    pub checkpoint: Option<Checkpoint>,
}

/// Shared with sub-agents, so their writes can be undone too.
pub type Checkpoint = std::sync::Arc<std::sync::Mutex<super::changes::Originals>>;

/// Tools that never modify anything.
pub const READ_ONLY: &[&str] = &["read_file", "list_dir", "glob", "grep", "open_file", "web_search", "web_fetch"];

/// What executing a tool asks of the UI besides its text result.
#[derive(Debug, PartialEq)]
pub enum SideEffect {
    None,
    OpenFile(PathBuf, Option<u32>),
    /// Show what a chat turn changed in a file: (file, its content before; None = created).
    OpenChanges(PathBuf, Option<Vec<u8>>),
    /// A chat turn changed these files: show them for Keep / Undo in their editors.
    ReviewChanges(Vec<super::changes::FileChange>),
    /// Keep (true) or undo everything the chat changed and that waits for review.
    SettleReviews(bool),
    /// A file on disk changed; open buffers should reload it.
    FileChanged(PathBuf),
    /// Anything may have changed (a shell command ran): refresh files, Git and open buffers.
    WorkspaceChanged,
}

fn schema(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({"type": "function", "function": {
        "name": name, "description": description,
        "parameters": {"type": "object", "properties": props, "required": required}
    }})
}

impl Toolbox {
    pub fn definitions(&self) -> Vec<Value> {
        let s = |d: &str| json!({"type": "string", "description": d});
        let n = |d: &str| json!({"type": "integer", "description": d});
        let mut defs = vec![
            schema("read_file", "Read a text file. Returns numbered lines.", json!({"path": s("File path, relative to the workspace"), "offset": n("First line (1-based), optional"), "limit": n("Max lines, optional")}), &["path"]),
            schema("write_file", "Create or overwrite a file with the given content.", json!({"path": s("File path"), "content": s("Full file content")}), &["path", "content"]),
            schema("edit_file", "Replace an exact snippet in a file. `old` must match exactly once unless replace_all is true.", json!({"path": s("File path"), "old": s("Exact text to replace"), "new": s("Replacement text"), "replace_all": json!({"type": "boolean"})}), &["path", "old", "new"]),
            schema("list_dir", "List a directory.", json!({"path": s("Directory, default workspace root")}), &[]),
            schema("glob", "Find files by glob pattern, e.g. `src/**/*.rs`.", json!({"pattern": s("Glob pattern")}), &["pattern"]),
            schema("grep", "Search file contents with a regex. Returns path:line: text.", json!({"pattern": s("Regular expression"), "path": s("Sub-directory, optional"), "glob": s("Only files matching this glob, optional")}), &["pattern"]),
            schema("bash", "Run a shell command in the workspace and return stdout+stderr. A command still running after timeout_secs keeps running as a background job; set background for servers, watchers and long builds.", json!({"command": s("Command line"), "timeout_secs": n("Seconds to wait before moving it to the background, default 120"), "background": json!({"type": "boolean", "description": "Start it as a background job and return at once"})}), &["command"]),
            schema("bash_output", "Read what a background job printed since the last read, and whether it finished.", json!({"job": n("Job number"), "wait_secs": n("Wait up to this long for it to finish, optional")}), &["job"]),
            schema("kill_job", "Stop a background job.", json!({"job": n("Job number")}), &["job"]),
            schema("open_file", "Open a file in the editor for the user, optionally at a line.", json!({"path": s("File path"), "line": n("1-based line, optional")}), &["path"]),
            schema("web_search", "Search the web. Returns titles, URLs and snippets.", json!({"query": s("Search query")}), &["query"]),
            schema("web_fetch", "Fetch a web page as plain text (docs, articles, issues).", json!({"url": s("http(s) URL")}), &["url"]),
        ];
        if self.allowed.is_none() {
            for (_, tool) in &self.plugin_tools {
                let props: serde_json::Map<String, Value> = tool.parameters.iter().map(|(k, d)| (k.clone(), s(d))).collect();
                defs.push(schema(&tool.name, &tool.description, Value::Object(props), &[]));
            }
        }
        if let Some(allowed) = &self.allowed {
            defs.retain(|d| d["function"]["name"].as_str().is_some_and(|n| allowed.contains(&n)));
        }
        defs
    }

    pub fn is_allowed(&self, name: &str) -> bool {
        self.allowed.as_ref().is_none_or(|a| a.contains(&name))
    }

    /// Whether the user must approve this call (it writes or runs code).
    pub fn needs_approval(&self, name: &str) -> bool {
        // Reading or stopping a job the agent already started runs nothing new.
        !READ_ONLY.contains(&name) && !matches!(name, "bash_output" | "kill_job")
    }

    pub fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            return p.to_path_buf();
        }
        // Small models often drop the leading `/` of absolute paths ("private/tmp/…", "Users/…").
        let rooted = Path::new("/").join(p);
        if !self.workspace.join(p).exists() && (rooted.exists() || rooted.starts_with(&self.workspace)) {
            return rooted;
        }
        self.workspace.join(p)
    }

    /// Accept the argument names small models use instead of ours (`file_path`, `filename`, …).
    fn normalize(args: &Value) -> Value {
        let mut a = args.clone();
        let aliases: [(&str, &[&str]); 4] = [
            ("path", &["file_path", "filepath", "file", "filename", "dir", "directory"]),
            ("content", &["contents", "text", "code", "data"]),
            ("old", &["old_string", "old_str", "search", "find"]),
            ("new", &["new_string", "new_str", "replace", "replacement"]),
        ];
        if let Some(obj) = a.as_object_mut() {
            for (key, alts) in aliases {
                if !obj.contains_key(key) {
                    if let Some(v) = alts.iter().find_map(|k| obj.get(*k).cloned()) {
                        obj.insert(key.to_string(), v);
                    }
                }
            }
        }
        a
    }

    /// Writes stay inside the workspace.
    fn resolve_for_write(&self, path: &str) -> Result<PathBuf> {
        let p = self.resolve(path);
        if p.components().any(|c| c == std::path::Component::ParentDir) {
            bail!("`..` is not allowed in paths: {}", p.display());
        }
        // Check the nearest existing ancestor (symlinks resolved) before creating anything.
        let existing = p.ancestors().find(|a| a.exists()).context("invalid path")?;
        if !existing.canonicalize()?.starts_with(self.workspace.canonicalize()?) {
            bail!("refusing to write outside the workspace: {}", p.display());
        }
        std::fs::create_dir_all(p.parent().context("invalid path")?)?;
        Ok(p)
    }

    /// Keep the content `path` had before its first change in this turn.
    fn remember(&self, path: &Path) {
        if let Some(checkpoint) = &self.checkpoint {
            if let Ok(mut originals) = checkpoint.lock() {
                originals.entry(path.to_path_buf()).or_insert_with(|| std::fs::read(path).ok());
            }
        }
    }

    #[cfg(test)]
    pub fn run(&self, name: &str, args: &Value) -> (String, SideEffect) {
        self.run_watched(name, args, &mut |_| Signal::Wait)
    }

    /// `run`, showing a command's output as it comes; `watch` may move it to the background or kill it.
    pub fn run_watched(&self, name: &str, args: &Value, watch: &mut dyn FnMut(&str) -> Signal) -> (String, SideEffect) {
        match self.run_inner(name, args, watch) {
            Ok((out, effect)) => (truncate(out), effect),
            Err(e) => (format!("Error: {e:#}"), SideEffect::None),
        }
    }

    fn run_inner(&self, name: &str, a: &Value, watch: &mut dyn FnMut(&str) -> Signal) -> Result<(String, SideEffect)> {
        let normalized = Self::normalize(a);
        let a = &normalized;
        let str_arg = |k: &str| a[k].as_str().with_context(|| format!("missing argument `{k}`"));
        let none = |s: String| Ok((s, SideEffect::None));
        if !self.is_allowed(name) {
            bail!("tool `{name}` is not available here");
        }
        match name {
            "web_search" => none(super::web::search(str_arg("query")?)?),
            "web_fetch" => none(super::web::fetch(str_arg("url")?)?),
            "read_file" => {
                let text = std::fs::read_to_string(self.resolve(str_arg("path")?))?;
                let offset = a["offset"].as_u64().unwrap_or(1).max(1) as usize;
                let limit = a["limit"].as_u64().unwrap_or(2000) as usize;
                none(text.lines().enumerate().skip(offset - 1).take(limit)
                    .map(|(i, l)| format!("{:>5}\t{l}\n", i + 1)).collect())
            }
            "write_file" => {
                let path = self.resolve_for_write(str_arg("path")?)?;
                let content = str_arg("content")?;
                self.remember(&path);
                std::fs::write(&path, content)?;
                Ok((format!("Wrote {} ({} bytes)", path.display(), content.len()), SideEffect::FileChanged(path)))
            }
            "edit_file" => {
                let path = self.resolve_for_write(str_arg("path")?)?;
                let (old, new) = (str_arg("old")?, str_arg("new")?);
                let text = std::fs::read_to_string(&path)?;
                let count = text.matches(old).count();
                let all = a["replace_all"].as_bool().unwrap_or(false);
                if count == 0 {
                    bail!("`old` not found in {}", path.display());
                }
                if count > 1 && !all {
                    bail!("`old` matches {count} times; add context or set replace_all");
                }
                let edited = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
                self.remember(&path);
                std::fs::write(&path, edited)?;
                Ok((format!("Edited {} ({count} replacement(s))", path.display()), SideEffect::FileChanged(path)))
            }
            "list_dir" => {
                let dir = self.resolve(a["path"].as_str().unwrap_or("."));
                let mut entries: Vec<String> = std::fs::read_dir(&dir)?
                    .flatten()
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if e.path().is_dir() { format!("{name}/") } else { name }
                    })
                    .collect();
                entries.sort();
                none(entries.join("\n"))
            }
            "glob" => {
                let pattern = str_arg("pattern")?;
                let mut ob = ignore::overrides::OverrideBuilder::new(&self.workspace);
                ob.add(pattern)?;
                let matcher = ob.build()?;
                let files: Vec<String> = self.walk(&self.workspace)
                    .filter(|p| matcher.matched(p, false).is_whitelist())
                    .take(500)
                    .map(|p| self.relative(&p))
                    .collect();
                none(if files.is_empty() { "No files found".into() } else { files.join("\n") })
            }
            "grep" => {
                let re = regex::Regex::new(str_arg("pattern")?)?;
                let root = self.resolve(a["path"].as_str().unwrap_or("."));
                let filter = match a["glob"].as_str() {
                    Some(g) => {
                        let mut ob = ignore::overrides::OverrideBuilder::new(&self.workspace);
                        ob.add(g)?;
                        Some(ob.build()?)
                    }
                    None => None,
                };
                let mut out = Vec::new();
                'files: for path in self.walk(&root) {
                    if filter.as_ref().is_some_and(|f| !f.matched(&path, false).is_whitelist()) {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(&path) else { continue };
                    for (i, line) in text.lines().enumerate() {
                        if re.is_match(line) {
                            out.push(format!("{}:{}: {}", self.relative(&path), i + 1, line.trim()));
                            if out.len() >= 200 {
                                break 'files;
                            }
                        }
                    }
                }
                none(if out.is_empty() { "No matches".into() } else { out.join("\n") })
            }
            "bash" => {
                let timeout = if a["background"].as_bool() == Some(true) { 0 } else { a["timeout_secs"].as_u64().unwrap_or(120) };
                Ok((run_job(str_arg("command")?, &self.workspace, &[], timeout, true, watch)?, SideEffect::WorkspaceChanged))
            }
            "bash_output" => {
                let job = a["job"].as_u64().context("missing argument `job`")? as usize;
                Ok((job_output(job, a["wait_secs"].as_u64().unwrap_or(0), watch)?, SideEffect::WorkspaceChanged))
            }
            "kill_job" => {
                let job = a["job"].as_u64().context("missing argument `job`")? as usize;
                let pid = jobs().lock().unwrap().get(job.wrapping_sub(1)).map(|j| j.pid).with_context(|| format!("no job {job}"))?;
                crate::util::kill_tree(pid);
                none(format!("Job {job} stopped."))
            }
            "open_file" => {
                let path = self.resolve(str_arg("path")?);
                let line = a["line"].as_u64().map(|l| l as u32);
                Ok((format!("Opened {}", path.display()), SideEffect::OpenFile(path, line)))
            }
            other => {
                let (dir, tool) = self.plugin_tools.iter().find(|(_, t)| t.name == other)
                    .with_context(|| format!("unknown tool `{other}`"))?;
                let mut env = crate::plugins::command_env(dir);
                env.push(("FORGE_ARGS".to_string(), a.to_string()));
                for k in tool.parameters.keys() {
                    env.push((format!("FORGE_ARG_{k}"), a[k].as_str().map(String::from).unwrap_or_else(|| a[k].to_string().replace("null", ""))));
                }
                none(run_shell(&tool.run, &self.workspace, &env, 300)?)
            }
        }
    }

    fn walk(&self, root: &Path) -> impl Iterator<Item = PathBuf> {
        ignore::WalkBuilder::new(root)
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path())
    }

    fn relative(&self, p: &Path) -> String {
        p.strip_prefix(&self.workspace).unwrap_or(p).display().to_string()
    }
}

/// What the watcher of a running command wants.
#[derive(Clone, Copy, PartialEq)]
pub enum Signal {
    Wait,
    /// Stop waiting: the command keeps running as a background job.
    Background,
    Kill,
}

/// A command started by the agent, possibly still running.
struct Job {
    pid: u32,
    output: Arc<Mutex<String>>,
    child: Arc<Mutex<Child>>,
    /// How much of `output` the agent has read.
    read: usize,
}

fn jobs() -> &'static Mutex<Vec<Job>> {
    static JOBS: OnceLock<Mutex<Vec<Job>>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
}

/// Run `command` in the platform shell, killing it after `timeout_secs`.
pub fn run_shell(command: &str, cwd: &Path, env: &[(String, String)], timeout_secs: u64) -> Result<String> {
    run_job(command, cwd, env, timeout_secs, false, &mut |_| Signal::Wait)
}

/// Start `command` and wait for it, passing new output to `watch`. Past `timeout_secs` (or on
/// `Signal::Background`) it keeps running as a job when `can_detach`, otherwise it is killed.
fn run_job(command: &str, cwd: &Path, env: &[(String, String)], timeout_secs: u64, can_detach: bool, watch: &mut dyn FnMut(&str) -> Signal) -> Result<String> {
    let mut cmd = crate::util::shell_command(command);
    // Its own process group, so stopping it reaches what it started (compilers, servers…).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.current_dir(cwd).envs(env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let output = Arc::new(Mutex::new(String::new()));
    // Drain pipes on threads so a chatty command cannot block on a full pipe.
    let readers: Vec<_> = [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), child.stderr.take().map(|p| Box::new(p) as _)]
        .into_iter()
        .flatten()
        .map(|mut pipe| {
            let output = output.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = pipe.read(&mut buf) {
                    output.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            })
        })
        .collect();
    let pid = child.id();
    let child = Arc::new(Mutex::new(child));
    let start = Instant::now();
    let mut shown = 0;
    loop {
        let status = child.lock().unwrap().try_wait()?;
        let fresh = { let o = output.lock().unwrap(); let f = o[shown..].to_string(); shown = o.len(); f };
        let signal = watch(&fresh);
        if let Some(status) = status {
            readers.into_iter().for_each(|r| drop(r.join()));
            let mut out = std::mem::take(&mut *output.lock().unwrap());
            if !status.success() {
                out.push_str(&format!("\n[exit status: {}]", status.code().unwrap_or(-1)));
            }
            return Ok(out);
        }
        let timed_out = start.elapsed() >= Duration::from_secs(timeout_secs);
        if signal == Signal::Kill || (timed_out && !can_detach) {
            crate::util::kill_tree(pid);
            let _ = child.lock().unwrap().wait();
            readers.into_iter().for_each(|r| drop(r.join()));
            let mut out = std::mem::take(&mut *output.lock().unwrap());
            out.push_str(&if signal == Signal::Kill { "\n[stopped by the user]".to_string() } else { format!("\n[killed after {timeout_secs}s]") });
            return Ok(out);
        }
        if signal == Signal::Background || timed_out {
            let mut jobs = jobs().lock().unwrap();
            jobs.push(Job { pid, output: output.clone(), child, read: shown });
            let n = jobs.len();
            let so_far = output.lock().unwrap()[..shown].to_string();
            return Ok(format!("{so_far}\n[still running: it continues as background job {n}. Call bash_output with job {n} to read its new output or wait for it, kill_job to stop it.]"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// New output of a background job, waiting up to `wait_secs` for it to finish.
fn job_output(job: usize, wait_secs: u64, watch: &mut dyn FnMut(&str) -> Signal) -> Result<String> {
    let (output, child) = {
        let jobs = jobs().lock().unwrap();
        let j = jobs.get(job.wrapping_sub(1)).with_context(|| format!("no job {job}"))?;
        (j.output.clone(), j.child.clone())
    };
    let start = Instant::now();
    let status = loop {
        let status = child.lock().unwrap().try_wait()?;
        if status.is_some() || start.elapsed() >= Duration::from_secs(wait_secs) || watch("") != Signal::Wait {
            break status;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // The pipes may still hold the last lines right after it exits.
    if status.is_some() {
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut jobs = jobs().lock().unwrap();
    let j = &mut jobs[job - 1];
    let o = output.lock().unwrap();
    let fresh = o[j.read..].to_string();
    j.read = o.len();
    let state = match status {
        None => "still running".to_string(),
        Some(s) => format!("finished, exit status {}", s.code().unwrap_or(-1)),
    };
    Ok(format!("{fresh}\n[job {job}: {state}]"))
}

pub fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n[output truncated]");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toolbox() -> (Toolbox, PathBuf) {
        let dir = std::env::temp_dir().join(format!("forge-tools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (Toolbox { workspace: dir.clone(), plugin_tools: vec![], allowed: None, checkpoint: None }, dir)
    }

    #[test]
    fn write_edit_read_grep() {
        let (tb, dir) = toolbox();
        tb.run("write_file", &json!({"path": "a.txt", "content": "one\ntwo\ntwo\n"}));
        let (out, _) = tb.run("edit_file", &json!({"path": "a.txt", "old": "two", "new": "2"}));
        assert!(out.contains("matches 2 times"), "{out}");
        tb.run("edit_file", &json!({"path": "a.txt", "old": "one", "new": "1"}));
        assert!(tb.run("read_file", &json!({"path": "a.txt"})).0.contains("1\t1"));
        assert!(tb.run("grep", &json!({"pattern": "^two"})).0.contains("a.txt:2"));
        assert!(tb.run("write_file", &json!({"path": "../escape.txt", "content": "x"})).0.starts_with("Error"));
        assert!(tb.run("bash", &json!({"command": "echo hi"})).0.contains("hi"));
        // Too slow: it goes on in the background and its output can be read later.
        let (out, _) = tb.run("bash", &json!({"command": "echo start; sleep 1; echo end", "timeout_secs": 0}));
        let job = out.split("background job ").nth(1).and_then(|r| r.split('.').next()).unwrap().to_string();
        let (later, _) = tb.run("bash_output", &json!({"job": job.parse::<u64>().unwrap(), "wait_secs": 10}));
        assert!(later.contains("end") && later.contains("finished, exit status 0"), "{later}");
        tb.run("write_file", &json!({"file_path": "alias.txt", "contents": "ok"}));
        assert_eq!(std::fs::read_to_string(dir.join("alias.txt")).unwrap(), "ok", "argument aliases");
        let rel = dir.strip_prefix("/").unwrap().join("alias.txt").display().to_string();
        assert!(tb.run("read_file", &json!({"path": rel})).0.contains("ok"), "absolute path without its leading slash");
        let read_only = Toolbox { allowed: Some(READ_ONLY.to_vec()), ..tb.clone() };
        assert!(read_only.run("write_file", &json!({"path": "x", "content": ""})).0.contains("not available"));
        assert!(read_only.definitions().iter().all(|d| READ_ONLY.contains(&d["function"]["name"].as_str().unwrap())));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
