//! The databases module (extensions/databases): connections of the project, the engine process
//! (`db.py serve`, every SQL DBMS through Python drivers) and the views on it — the sidebar panel,
//! table and SQL console tabs.

pub mod console;
pub mod panel;
pub mod table;

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::plugins::{Plugins, agent_runtime};

pub const KINDS: &[(&str, &str)] = &[
    ("sqlite", "SQLite"),
    ("postgresql", "PostgreSQL"),
    ("mysql", "MySQL"),
    ("mariadb", "MariaDB"),
    ("sqlserver", "SQL Server"),
    ("oracle", "Oracle"),
];

/// A saved connection (no password: see [`secret`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Conn {
    pub id: String,
    pub name: String,
    pub kind: String,
    /// SQLite file.
    pub path: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub database: String,
    pub readonly: bool,
    /// Found in the project rather than saved (SQLite files).
    #[serde(skip)]
    pub detected: bool,
}

impl Conn {
    pub fn kind_name(&self) -> &'static str {
        KINDS.iter().find(|(k, _)| *k == self.kind).map_or("SQL", |(_, n)| n)
    }

    pub fn subtitle(&self) -> String {
        if self.kind == "sqlite" {
            return self.path.clone();
        }
        let user = if self.user.is_empty() { String::new() } else { format!("{}@", self.user) };
        format!("{user}{}:{} / {}", if self.host.is_empty() { "localhost" } else { &self.host }, self.port, self.database)
    }

    /// What `db.py` connects with (password from the keychain).
    pub fn config(&self, root: &Path) -> Value {
        json!({
            "kind": self.kind, "path": self.path, "host": self.host, "port": self.port, "user": self.user,
            "database": self.database, "readonly": self.readonly, "password": secret::get(root, &self.id),
        })
    }
}

fn store_path(root: &Path) -> PathBuf {
    crate::dotforge::dir(root).join("databases.json")
}

/// Saved connections, then SQLite files of the project not saved yet.
pub fn load(root: &Path) -> Vec<Conn> {
    let mut conns: Vec<Conn> = std::fs::read_to_string(store_path(root)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    for file in sqlite_files(root) {
        let path = file.display().to_string();
        if !conns.iter().any(|c| c.path == path) {
            let name = file.strip_prefix(root).unwrap_or(&file).display().to_string();
            conns.push(Conn { id: format!("file:{path}"), name, kind: "sqlite".into(), path, detected: true, ..Default::default() });
        }
    }
    conns
}

pub fn save(root: &Path, conns: &[Conn]) -> Result<()> {
    let saved: Vec<&Conn> = conns.iter().filter(|c| !c.detected).collect();
    std::fs::create_dir_all(crate::dotforge::dir(root))?;
    std::fs::write(store_path(root), serde_json::to_string_pretty(&saved)?)?;
    Ok(())
}

/// SQLite files of the project (by extension, then checked by their header), 4 levels deep.
pub fn sqlite_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if depth > 1 && !crate::language::SKIPPED_DIRS.contains(&name.as_str()) && !path.join("pyvenv.cfg").exists() && !name.starts_with('.') {
                    walk(&path, depth - 1, out);
                }
            } else if is_sqlite(&path) {
                out.push(path);
            }
        }
    }
    let mut out = vec![];
    walk(root, 4, &mut out);
    out.sort();
    out
}

/// `.db`/`.sqlite`/`.sqlite3` files that really are SQLite (their 16-byte header).
pub fn is_sqlite(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !matches!(ext.as_str(), "db" | "sqlite" | "sqlite3" | "db3") {
        return false;
    }
    let mut head = [0u8; 16];
    std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head)).is_ok() && &head == b"SQLite format 3\0"
}

/// Passwords: the macOS keychain (service `forgecode-db`), else `.forgecode/db-secrets.json`,
/// which `.forgecode/.gitignore` keeps out of git.
pub mod secret {
    use std::path::Path;
    use std::process::Command;

    const SERVICE: &str = "forgecode-db";

    pub fn set(root: &Path, id: &str, password: &str) {
        if cfg!(target_os = "macos") {
            let _ = Command::new("security").args(["add-generic-password", "-U", "-a", id, "-s", SERVICE, "-w", password]).output();
            return;
        }
        let path = file(root);
        let mut all: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        all.insert(id.into(), password.into());
        let _ = std::fs::write(&path, serde_json::to_string(&all).unwrap_or_default());
        crate::workspace::databases::ignore_in_git(root, "db-secrets.json");
    }

    pub fn get(root: &Path, id: &str) -> String {
        if cfg!(target_os = "macos") {
            return Command::new("security")
                .args(["find-generic-password", "-a", id, "-s", SERVICE, "-w"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim_end_matches('\n').to_string())
                .unwrap_or_default();
        }
        std::fs::read_to_string(file(root))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v[id].as_str().map(String::from))
            .unwrap_or_default()
    }

    pub fn remove(root: &Path, id: &str) {
        if cfg!(target_os = "macos") {
            let _ = Command::new("security").args(["delete-generic-password", "-a", id, "-s", SERVICE]).output();
        }
        let _ = root;
    }

    /// The command printing the password, for the agent's MCP server (it runs as the user).
    pub fn command(id: &str) -> Option<Vec<String>> {
        cfg!(target_os = "macos").then(|| ["security", "find-generic-password", "-a", id, "-s", SERVICE, "-w"].iter().map(|s| s.to_string()).collect())
    }

    fn file(root: &Path) -> std::path::PathBuf {
        crate::dotforge::dir(root).join("db-secrets.json")
    }
}

/// Add `name` to `.forgecode/.gitignore`.
pub fn ignore_in_git(root: &Path, name: &str) {
    let ignore = crate::dotforge::dir(root).join(".gitignore");
    let text = std::fs::read_to_string(&ignore).unwrap_or_default();
    if !text.lines().any(|l| l.trim() == name) {
        let _ = std::fs::write(&ignore, format!("{text}{}{name}\n", if text.is_empty() || text.ends_with('\n') { "" } else { "\n" }));
    }
}

// ---------------------------------------------------------------- engine process

type Pending = Arc<Mutex<HashMap<i64, smol::channel::Sender<Result<Value>>>>>;

/// `db.py serve`: one JSON request per line, one answer per line.
pub struct Engine {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next: AtomicI64,
    pending: Pending,
}

impl Engine {
    pub fn start(python: &Path, script: &Path) -> Result<Arc<Self>> {
        let mut child = Command::new(python)
            .arg(script)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("start {}", script.display()))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let pending: Pending = Default::default();
        let reader = pending.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                let Some(id) = msg["id"].as_i64() else { continue };
                if let Some(tx) = reader.lock().unwrap().remove(&id) {
                    let result = match msg.get("error") {
                        Some(e) => Err(anyhow!("{}", e.as_str().unwrap_or("error"))),
                        None => Ok(msg["result"].clone()),
                    };
                    let _ = tx.try_send(result);
                }
            }
            for (_, tx) in reader.lock().unwrap().drain() {
                let _ = tx.try_send(Err(anyhow!("the database engine stopped")));
            }
        });
        Ok(Arc::new(Self { stdin: Mutex::new(stdin), child: Mutex::new(child), next: AtomicI64::new(1), pending }))
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = smol::channel::bounded(1);
        self.pending.lock().unwrap().insert(id, tx);
        {
            let mut stdin = self.stdin.lock().unwrap();
            writeln!(stdin, "{}", json!({"id": id, "method": method, "params": params}))?;
            stdin.flush()?;
        }
        rx.recv().await.map_err(|_| anyhow!("the database engine stopped"))?
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

// ---------------------------------------------------------------- shared state

#[derive(Clone, Debug, Deserialize, Default)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type", default)]
    pub ty: String,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub pk: bool,
    #[serde(default)]
    pub default: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct TableInfo {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub columns: Vec<Column>,
}

/// Rows a query returned.
#[derive(Clone, Debug, Default)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub total: Option<i64>,
    pub truncated: bool,
}

impl Rows {
    pub fn from(v: &Value) -> Self {
        Rows {
            columns: v["columns"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(String::from)).collect(),
            rows: v["rows"].as_array().into_iter().flatten().map(|r| r.as_array().cloned().unwrap_or_default()).collect(),
            total: v["total"].as_i64(),
            truncated: v["truncated"].as_bool().unwrap_or(false),
        }
    }
}

pub enum HubEvent {
    Changed,
    /// Open a table or a console tab.
    OpenTable(String, String),
    OpenConsole(String),
    Notify(String),
}

/// The project's connections and the engine, shared by the panel and the tabs.
pub struct DbHub {
    pub root: PathBuf,
    pub conns: Vec<Conn>,
    /// Schema of each open connection.
    pub schemas: HashMap<String, Vec<TableInfo>>,
    pub open: std::collections::HashSet<String>,
    pub errors: HashMap<String, String>,
    pub busy: Option<String>,
    engine: Option<Arc<Engine>>,
    /// Whether the engine runs the module's Python (drivers installed).
    engine_has_drivers: bool,
}

impl EventEmitter<HubEvent> for DbHub {}

impl DbHub {
    pub fn new(root: PathBuf) -> Self {
        let conns = load(&root);
        Self { root, conns, schemas: HashMap::new(), open: Default::default(), errors: HashMap::new(), busy: None, engine: None, engine_has_drivers: false }
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.root = root;
        self.reload(cx);
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.conns = load(&self.root);
        self.schemas.clear();
        self.open.clear();
        self.errors.clear();
        cx.emit(HubEvent::Changed);
        cx.notify();
    }

    pub fn conn(&self, id: &str) -> Option<&Conn> {
        self.conns.iter().find(|c| c.id == id)
    }

    /// The engine, with drivers when a server connection needs them (installed on first use).
    pub(super) fn engine(&mut self, needs_drivers: bool, cx: &mut Context<Self>) -> Task<Result<Arc<Engine>>> {
        if let Some(e) = self.engine.clone().filter(|_| self.engine_has_drivers || !needs_drivers) {
            return Task::ready(Ok(e));
        }
        let Some((plugin, spec)) = Plugins::global(cx).database().map(|(p, s)| (p.clone(), s.clone())) else {
            return Task::ready(Err(anyhow!("The Databases module is not installed")));
        };
        let script = plugin.dir.join(&spec.engine);
        let python_spec = plugin.manifest.python.clone();
        if needs_drivers {
            self.busy = Some(crate::i18n::t("Installing database drivers…").into());
            cx.notify();
        }
        let task = cx.background_spawn(async move {
            let python = if needs_drivers || agent_runtime::module_python(&plugin).is_file() {
                match &python_spec {
                    Some(py) => agent_runtime::ensure_python(&plugin, py, |_| {})?,
                    None => agent_runtime::module_python(&plugin),
                }
            } else {
                // SQLite needs no driver: any Python 3 does until drivers are installed.
                crate::lsp::install::which("python3").or_else(|| crate::lsp::install::which("python")).context("Python 3 is not installed")?
            };
            let drivers = python == agent_runtime::module_python(&plugin);
            Ok::<_, anyhow::Error>((Engine::start(&python, &script)?, drivers))
        });
        cx.spawn(async move |this, cx| {
            let (engine, drivers) = task.await?;
            this.update(cx, |hub, cx| {
                hub.busy = None;
                hub.engine = Some(engine.clone());
                hub.engine_has_drivers = drivers;
                // Connections were opened in the previous engine.
                hub.open.clear();
                cx.notify();
            })?;
            Ok(engine)
        })
    }

    /// Call the engine for connection `id`, connecting first if needed.
    pub fn call(&mut self, id: &str, method: &'static str, mut params: Value, cx: &mut Context<Self>) -> Task<Result<Value>> {
        let Some(conn) = self.conn(id).cloned() else { return Task::ready(Err(anyhow!("Unknown connection"))) };
        let engine = self.engine(conn.kind != "sqlite", cx);
        let config = conn.config(&self.root);
        let id = id.to_string();
        params["id"] = json!(id);
        cx.spawn(async move |this, cx| {
            let engine = engine.await.inspect_err(|e| {
                let msg = e.to_string();
                let _ = this.update(cx, |hub, cx| {
                    hub.busy = None;
                    hub.errors.insert(String::new(), msg);
                    cx.notify();
                });
            })?;
            let opened = this.update(cx, |hub, _| hub.open.contains(&id))?;
            if !opened {
                engine.call("open", json!({"id": id, "config": config})).await?;
                this.update(cx, |hub, _| hub.open.insert(id.clone()))?;
            }
            engine.call(method, params).await
        })
    }

    /// Connect and read the schema.
    pub fn refresh_schema(&mut self, id: &str, cx: &mut Context<Self>) {
        let task = self.call(id, "schema", json!({}), cx);
        let id = id.to_string();
        self.errors.remove(&id);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |hub, cx| {
                match result.and_then(|v| Ok(serde_json::from_value::<Vec<TableInfo>>(v)?)) {
                    Ok(tables) => {
                        hub.schemas.insert(id, tables);
                    }
                    Err(e) => {
                        hub.open.remove(&id);
                        hub.errors.insert(id, e.to_string());
                    }
                }
                cx.emit(HubEvent::Changed);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn add_or_update(&mut self, conn: Conn, password: Option<String>, cx: &mut Context<Self>) {
        if let Some(pw) = password {
            secret::set(&self.root, &conn.id, &pw);
        }
        match self.conns.iter_mut().find(|c| c.id == conn.id) {
            Some(c) => *c = conn.clone(),
            None => self.conns.insert(0, conn.clone()),
        }
        if let Err(e) = save(&self.root, &self.conns) {
            cx.emit(HubEvent::Notify(e.to_string()));
        }
        self.open.remove(&conn.id);
        self.write_agent_file();
        self.refresh_schema(&conn.id, cx);
        cx.notify();
    }

    pub fn remove(&mut self, id: &str, cx: &mut Context<Self>) {
        secret::remove(&self.root, id);
        self.conns.retain(|c| c.id != id);
        let _ = save(&self.root, &self.conns);
        self.schemas.remove(id);
        self.write_agent_file();
        cx.emit(HubEvent::Changed);
        cx.notify();
    }

    /// The connections file the agent's MCP server reads (no passwords: a command fetches them).
    pub fn write_agent_file(&self) -> PathBuf {
        let path = crate::dotforge::dir(&self.root).join("databases-agent.json");
        let list: Vec<Value> = self
            .conns
            .iter()
            .map(|c| {
                json!({
                    "name": c.name, "kind": c.kind, "path": c.path, "host": c.host, "port": c.port, "user": c.user,
                    "database": c.database, "readonly": c.readonly,
                    "password_command": secret::command(&c.id),
                    "password": if secret::command(&c.id).is_none() { secret::get(&self.root, &c.id) } else { String::new() },
                })
            })
            .collect();
        let _ = std::fs::create_dir_all(crate::dotforge::dir(&self.root));
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&list).unwrap_or_default());
        ignore_in_git(&self.root, "databases-agent.json");
        path
    }
}

/// An agent wants to change a database: show the statement, the user allows or refuses.
pub fn confirm_agent_change(request: Value, answer: smol::channel::Sender<bool>, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
    let connection = request["connection"].as_str().unwrap_or("").to_string();
    let sql = request["sql"].as_str().unwrap_or("").to_string();
    let answered = std::rc::Rc::new(std::cell::Cell::new(false));
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme().clone();
        let (yes, no) = (answer.clone(), answer.clone());
        let (a1, a2) = (answered.clone(), answered.clone());
        dialog
            .title(format!("{} · {connection}", crate::i18n::t("Hermes wants to change the database")))
            .w(px(560.))
            .on_close({
                let answer = answer.clone();
                let answered = answered.clone();
                move |_, _, _| {
                    if !answered.get() {
                        let _ = answer.try_send(false);
                    }
                }
            })
            .child(
                v_flex()
                    .gap_3()
                    .child(div().p_2().rounded_md().bg(theme.secondary).font_family("Menlo").text_xs().whitespace_normal().child(sql.clone()))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("agent-db-no").small().ghost().label(crate::i18n::t("Refuse")).on_click(move |_, w, cx| {
                                a1.set(true);
                                let _ = no.try_send(false);
                                w.close_dialog(cx);
                            }))
                            .child(Button::new("agent-db-yes").small().primary().label(crate::i18n::t("Allow")).on_click(move |_, w, cx| {
                                a2.set(true);
                                let _ = yes.try_send(true);
                                w.close_dialog(cx);
                            })),
                    ),
            )
    });
}

/// The MCP server Hermes gets for the project's databases (`session/new`'s `mcpServers`).
pub fn agent_mcp_server(root: &Path, cx: &App) -> Option<Value> {
    let (plugin, spec) = Plugins::global(cx).database()?;
    let python = agent_runtime::module_python(plugin);
    let python = if python.is_file() { python } else { crate::lsp::install::which("python3")? };
    let hub = DbHub { root: root.to_path_buf(), conns: load(root), schemas: HashMap::new(), open: Default::default(), errors: HashMap::new(), busy: None, engine: None, engine_has_drivers: false };
    let file = hub.write_agent_file();
    let mut env = vec![];
    if let Some(g) = crate::ai::gateway::Gateway::running() {
        env.push(json!({"name": "FORGE_CONFIRM_URL", "value": format!("{}/forge/confirm", g.base_url().trim_end_matches("/v1"))}));
        env.push(json!({"name": "FORGE_CONFIRM_KEY", "value": g.secret}));
    }
    Some(json!({
        "name": "forgecode-databases",
        "command": python.display().to_string(),
        "args": [plugin.dir.join(&spec.engine).display().to_string(), "mcp", file.display().to_string()],
        "env": env,
    }))
}

#[cfg(test)]
mod tests {
    use super::{Engine, Rows, load, save, sqlite_files, store_path};
    use serde_json::json;

    /// The Rust client driving the real engine on a SQLite file.
    #[test]
    fn engine_reads_and_guards_writes() {
        let dir = std::env::temp_dir().join(format!("forge-engine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("app.db");
        let ok = std::process::Command::new("/usr/bin/python3")
            .args(["-c", "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.executescript('create table t(id integer primary key, name text); insert into t(name) values (\"a\"),(\"b\");'); c.commit()"])
            .arg(&db)
            .status()
            .unwrap();
        assert!(ok.success());
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions/databases/db.py");
        let engine = Engine::start(std::path::Path::new("/usr/bin/python3"), &script).unwrap();
        smol::block_on(async {
            engine.call("open", json!({"id": "x", "config": {"kind": "sqlite", "path": db.display().to_string()}})).await.unwrap();
            let schema = engine.call("schema", json!({"id": "x"})).await.unwrap();
            assert_eq!(schema[0]["name"], "t");
            let rows = Rows::from(&engine.call("rows", json!({"id": "x", "table": "t"})).await.unwrap());
            assert_eq!((rows.total, rows.rows.len()), (Some(2), 2));
            let write = engine.call("query", json!({"id": "x", "sql": "delete from t"})).await.unwrap();
            assert_eq!(write["needs_confirmation"], true, "writes wait for the user");
            assert!(engine.call("query", json!({"id": "x", "sql": "select * from nope"})).await.is_err());
        });
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finds_project_sqlite_files_by_header() {
        let root = std::env::temp_dir().join(format!("forge-dbs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("backend/instance")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        let mut real = b"SQLite format 3\0".to_vec();
        real.extend([0u8; 84]);
        std::fs::write(root.join("backend/instance/app.db"), &real).unwrap();
        std::fs::write(root.join("node_modules/x/cache.db"), &real).unwrap();
        std::fs::write(root.join("notes.db"), "not sqlite").unwrap();
        let found = sqlite_files(&root);
        assert_eq!(found, vec![root.join("backend/instance/app.db")]);
        let conns = load(&root);
        assert!(conns[0].detected && conns[0].name == "backend/instance/app.db");
        save(&root, &conns).unwrap();
        assert_eq!(std::fs::read_to_string(store_path(&root)).unwrap().trim(), "[]", "detected files aren't saved");
        std::fs::remove_dir_all(root).unwrap();
    }
}
