//! The server hosting the repository and its CI: GitHub Actions (through `gh`), GitLab CI/CD and
//! Gitea / Forgejo Actions (through their REST API). Runs, jobs and logs come back in GitHub's
//! shape (status + conclusion) so one view shows them all.
//!
//! Sign-in for GitLab / Gitea, first found: `GITLAB_TOKEN` / `GITEA_TOKEN`, the token saved in
//! ForgeCode for that server, the credentials git already holds for it (HTTPS remotes).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use serde_json::{Value, json};

use super::actions::{Job, Run, Workflow};
use crate::i18n::t;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    GitHub,
    GitLab,
    Gitea,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::GitHub => "GITHUB ACTIONS",
            Kind::GitLab => "GITLAB CI/CD",
            Kind::Gitea => "GITEA ACTIONS",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Forge {
    pub kind: Kind,
    /// `https://host[:port]`
    pub base: String,
    /// `group/sub/project` or `owner/repo`
    pub project: String,
    pub root: PathBuf,
}

/// `https://host/path.git`, `git@host:path.git`, `ssh://git@host:22/path.git` → (`https://host`, `path`).
pub fn parse_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (host, path) = if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        let (host, path) = rest.split_once('/')?;
        let host = host.rsplit('@').next()?; // credentials in the URL
        let scheme = if url.starts_with("http://") { "http" } else { "https" };
        (format!("{scheme}://{host}"), path)
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (host, path) = rest.split_once('/')?;
        let host = host.rsplit('@').next()?.split(':').next()?; // the SSH port is not the web one
        (format!("https://{host}"), path)
    } else {
        let (user_host, path) = url.split_once(':')?;
        (format!("https://{}", user_host.rsplit('@').next()?), path)
    };
    let path = path.trim_matches('/').trim_end_matches(".git").to_string();
    (!path.is_empty()).then_some((host, path))
}

/// The server of the repository at `root` (its `origin`), recognised by its name, the CI files of
/// the project, or by asking it.
pub fn detect(root: &Path) -> Result<Forge> {
    let repo = super::Repo::discover(root).context(t("Not a Git repository"))?;
    let url = repo.remote_url().context(t("No remote: link the repository to a server first"))?;
    let (base, project) = parse_remote(&url).with_context(|| format!("{}: {url}", t("Unknown remote address")))?;
    let host = base.split("://").nth(1).unwrap_or("").to_lowercase();
    let has = |p: &str| root.join(p).exists();
    let kind = if host == "github.com" || host.ends_with(".github.com") {
        Kind::GitHub
    } else if host.contains("gitlab") || has(".gitlab-ci.yml") {
        Kind::GitLab
    } else if host.contains("gitea") || host.contains("forgejo") || host == "codeberg.org" || has(".gitea/workflows") || has(".forgejo/workflows") {
        Kind::Gitea
    } else {
        probe(&base).with_context(|| format!("{}: {base}", t("Unknown Git server (GitHub, GitLab or Gitea expected)")))?
    };
    Ok(Forge { kind, base, project, root: root.to_path_buf() })
}

/// Gitea answers `/api/v1/version` to anyone; GitLab answers `/api/v4/version` (401 without sign-in).
fn probe(base: &str) -> Option<Kind> {
    let agent = agent();
    if let Ok(r) = agent.get(&format!("{base}/api/v1/version")).call() {
        if r.into_json::<Value>().ok().is_some_and(|v| v["version"].is_string()) {
            return Some(Kind::Gitea);
        }
    }
    match agent.get(&format!("{base}/api/v4/version")).call() {
        Ok(_) | Err(ureq::Error::Status(401, _)) => Some(Kind::GitLab),
        _ => None,
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(20)).build()
}

// ---- sign-in -------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Auth {
    Token(String),
    /// What git's credential helper holds (an OAuth token or a password).
    Basic(String, String),
}

fn tokens_file() -> PathBuf {
    crate::settings::config_dir().join("forge-tokens.json")
}

fn saved_tokens() -> BTreeMap<String, String> {
    std::fs::read_to_string(tokens_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Keep `token` for the server `base` (in a file only the user can read).
pub fn save_token(base: &str, token: &str) -> Result<()> {
    let mut all = saved_tokens();
    if token.trim().is_empty() {
        all.remove(base);
    } else {
        all.insert(base.to_string(), token.trim().to_string());
    }
    let file = tokens_file();
    std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
    crate::util::write_private(&file, serde_json::to_string_pretty(&all)?.as_bytes())?;
    Ok(())
}

/// Where to create a token on that server.
pub fn token_page(forge: &Forge) -> String {
    match forge.kind {
        Kind::GitLab => format!("{}/-/user_settings/personal_access_tokens?name=ForgeCode&scopes=api", forge.base),
        Kind::Gitea => format!("{}/user/settings/applications", forge.base),
        Kind::GitHub => "https://github.com/settings/tokens".into(),
    }
}

fn auth(forge: &Forge) -> Option<Auth> {
    let var = if forge.kind == Kind::GitLab { "GITLAB_TOKEN" } else { "GITEA_TOKEN" };
    if let Ok(t) = std::env::var(var).map(|t| t.trim().to_string()) {
        if !t.is_empty() {
            return Some(Auth::Token(t));
        }
    }
    if let Some(t) = saved_tokens().get(&forge.base) {
        return Some(Auth::Token(t.clone()));
    }
    git_credential(forge)
}

/// `git credential fill` for the server, never prompting (a missing one is asked for in the view).
fn git_credential(forge: &Forge) -> Option<Auth> {
    use std::io::Write as _;
    let (protocol, host) = forge.base.split_once("://")?;
    let mut child = crate::util::command("git")
        .args(["credential", "fill"])
        .current_dir(&forge.root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_ASKPASS", "")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(format!("protocol={protocol}\nhost={host}\npath={}.git\n\n", forge.project).as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(String::from);
    Some(Auth::Basic(field("username").unwrap_or_default(), field("password").filter(|p| !p.is_empty())?))
}

// ---- REST ----------------------------------------------------------------------------------------

/// An error asking for a token: the view then shows the token field.
pub const NEEDS_TOKEN: &str = "forge-needs-token";

fn request(forge: &Forge, method: &str, path: &str, body: Option<Value>) -> Result<ureq::Response> {
    let prefix = if forge.kind == Kind::GitLab { "/api/v4" } else { "/api/v1" };
    let mut req = agent().request(method, &format!("{}{prefix}{path}", forge.base));
    match auth(forge) {
        // GitLab takes personal and OAuth tokens as Bearer; Gitea its tokens as `token`.
        Some(Auth::Token(t)) if forge.kind == Kind::GitLab => req = req.set("Authorization", &format!("Bearer {t}")),
        Some(Auth::Token(t)) => req = req.set("Authorization", &format!("token {t}")),
        // GitLab's API takes no password: git's credential there is a token (OAuth or personal).
        Some(Auth::Basic(_, pass)) if forge.kind == Kind::GitLab => req = req.set("Authorization", &format!("Bearer {pass}")),
        Some(Auth::Basic(user, pass)) => req = req.set("Authorization", &format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()))),
        None => {}
    }
    let result = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    match result {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(401 | 403, _)) => Err(anyhow!(NEEDS_TOKEN)),
        Err(ureq::Error::Status(code, r)) => Err(anyhow!("{code}: {}", r.into_string().unwrap_or_default().chars().take(300).collect::<String>())),
        Err(e) => Err(anyhow!("{e}")),
    }
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    data.chunks(3)
        .flat_map(|c| {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            (0..4).map(move |i| if i <= c.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' })
        })
        .collect()
}

fn get(forge: &Forge, path: &str) -> Result<Value> {
    Ok(request(forge, "GET", path, None)?.into_json()?)
}

fn get_text(forge: &Forge, path: &str) -> Result<String> {
    Ok(request(forge, "GET", path, None)?.into_string()?)
}

fn gitlab_project(forge: &Forge) -> String {
    format!("/projects/{}", forge.project.replace('/', "%2F"))
}

fn gitea_repo(forge: &Forge) -> String {
    format!("/repos/{}", forge.project)
}

/// GitLab's and Gitea's single status → GitHub's (status, conclusion).
fn normalize(status: &str) -> (String, String) {
    let done = |c: &str| ("completed".to_string(), c.to_string());
    match status {
        "running" | "in_progress" => ("in_progress".into(), String::new()),
        "success" => done("success"),
        "failed" | "failure" => done("failure"),
        "canceled" | "cancelled" => done("cancelled"),
        "skipped" => done("skipped"),
        "manual" | "blocked" => done("action_required"),
        _ => ("queued".into(), String::new()),
    }
}

fn str_of(v: &Value, keys: &[&str]) -> String {
    keys.iter().find_map(|k| v[*k].as_str().filter(|s| !s.is_empty())).unwrap_or("").to_string()
}

pub fn runs(forge: &Forge) -> Result<Vec<Run>> {
    match forge.kind {
        Kind::GitHub => super::actions::gh_runs(&forge.root),
        Kind::GitLab => {
            let list = get(forge, &format!("{}/pipelines?per_page=20", gitlab_project(forge)))?;
            Ok(list.as_array().into_iter().flatten().map(|p| {
                let (status, conclusion) = normalize(p["status"].as_str().unwrap_or(""));
                let id = p["id"].as_u64().unwrap_or(0);
                let sha: String = p["sha"].as_str().unwrap_or("").chars().take(8).collect();
                Run {
                    database_id: id,
                    display_title: p["name"].as_str().map(String::from).unwrap_or_else(|| format!("Pipeline #{id} {sha}")),
                    workflow_name: str_of(p, &["source"]),
                    head_branch: str_of(p, &["ref"]),
                    event: str_of(p, &["source"]),
                    status,
                    conclusion,
                    created_at: str_of(p, &["created_at"]),
                    url: str_of(p, &["web_url"]),
                }
            }).collect())
        }
        Kind::Gitea => {
            // Gitea 1.25+ and Forgejo: `/actions/runs`; older Gitea: `/actions/tasks` (one per job).
            let list = get(forge, &format!("{}/actions/runs?limit=20", gitea_repo(forge))).or_else(|_| get(forge, &format!("{}/actions/tasks?limit=20", gitea_repo(forge))))?;
            Ok(list["workflow_runs"].as_array().into_iter().flatten().map(|r| {
                let (status, conclusion) = match r["conclusion"].as_str().filter(|c| !c.is_empty()) {
                    Some(c) => ("completed".to_string(), c.to_string()),
                    None => normalize(r["status"].as_str().unwrap_or("")),
                };
                let workflow = str_of(r, &["path", "name", "workflow_id"]);
                Run {
                    database_id: r["id"].as_u64().unwrap_or(0),
                    display_title: str_of(r, &["display_title", "name"]),
                    workflow_name: workflow.rsplit('/').next().unwrap_or("").split('@').next().unwrap_or("").to_string(),
                    head_branch: str_of(r, &["head_branch"]),
                    event: str_of(r, &["event"]),
                    status,
                    conclusion,
                    created_at: str_of(r, &["created_at", "run_started_at"]),
                    url: str_of(r, &["html_url", "url"]),
                }
            }).collect())
        }
    }
}

pub fn jobs(forge: &Forge, run: u64) -> Result<Vec<Job>> {
    let list = match forge.kind {
        Kind::GitHub => return super::actions::gh_jobs(&forge.root, run),
        Kind::GitLab => get(forge, &format!("{}/pipelines/{run}/jobs?per_page=100&include_retried=false", gitlab_project(forge)))?,
        Kind::Gitea => get(forge, &format!("{}/actions/runs/{run}/jobs", gitea_repo(forge)))?["jobs"].clone(),
    };
    Ok(list.as_array().into_iter().flatten().map(|j| {
        let (status, conclusion) = match j["conclusion"].as_str().filter(|c| !c.is_empty()) {
            Some(c) => ("completed".to_string(), c.to_string()),
            None => normalize(j["status"].as_str().unwrap_or("")),
        };
        let stage = j["stage"].as_str().map(|s| format!("{s}: ")).unwrap_or_default();
        Job { id: j["id"].as_u64().unwrap_or(0), name: format!("{stage}{}", str_of(j, &["name"])), status, conclusion, url: str_of(j, &["web_url", "html_url"]) }
    }).collect())
}

/// What can be started by hand: GitHub's and Gitea's workflows; GitLab's pipeline of a branch.
pub fn workflows(forge: &Forge) -> Result<Vec<Workflow>> {
    match forge.kind {
        Kind::GitHub => super::actions::gh_workflows(&forge.root),
        Kind::GitLab => Ok(vec![Workflow { id: String::new(), name: t("New pipeline").into(), state: "active".into() }]),
        Kind::Gitea => {
            let list = get(forge, &format!("{}/actions/workflows", gitea_repo(forge)))?;
            Ok(list["workflows"].as_array().into_iter().flatten().map(|w| {
                let file = str_of(w, &["path"]).rsplit('/').next().unwrap_or("").to_string();
                Workflow { id: if file.is_empty() { str_of(w, &["id"]) } else { file }, name: str_of(w, &["name"]), state: str_of(w, &["state"]) }
            }).collect())
        }
    }
}

/// The log of a run: its failed jobs' (all of them when none failed), without colours.
pub fn log(forge: &Forge, run: &Run) -> Result<String> {
    if forge.kind == Kind::GitHub {
        return super::actions::gh_log(&forge.root, run);
    }
    let jobs = jobs(forge, run.database_id)?;
    let failed: Vec<&Job> = jobs.iter().filter(|j| j.conclusion == "failure").collect();
    let shown: Vec<&Job> = if failed.is_empty() { jobs.iter().collect() } else { failed };
    let mut out = String::new();
    for job in shown {
        let path = match forge.kind {
            Kind::GitLab => format!("{}/jobs/{}/trace", gitlab_project(forge), job.id),
            _ => format!("{}/actions/jobs/{}/logs", gitea_repo(forge), job.id),
        };
        out.push_str(&format!("==== {} ====\n", job.name));
        match get_text(forge, &path) {
            Ok(text) => out.push_str(&clean_log(&text)),
            Err(e) => out.push_str(&format!("{e}\n")),
        }
        out.push('\n');
    }
    Ok(out)
}

/// Without ANSI colours and GitLab's collapsible-section markers.
fn clean_log(text: &str) -> String {
    let ansi = regex::Regex::new(r"\x1b\[[0-9;]*[A-Za-z]|section_(start|end):\d+:[^\r\n]*?\r").expect("valid regex");
    ansi.replace_all(text, "").replace('\r', "")
}

pub enum Op {
    Cancel(u64),
    Rerun(u64),
    /// Start the workflow `id` (GitLab: a pipeline) on `branch`.
    Dispatch(String, String),
}

pub fn act(forge: &Forge, op: Op) -> Result<()> {
    match (forge.kind, op) {
        (Kind::GitHub, op) => super::actions::gh_act(&forge.root, op),
        (Kind::GitLab, Op::Cancel(id)) => request(forge, "POST", &format!("{}/pipelines/{id}/cancel", gitlab_project(forge)), None).map(drop),
        (Kind::GitLab, Op::Rerun(id)) => request(forge, "POST", &format!("{}/pipelines/{id}/retry", gitlab_project(forge)), None).map(drop),
        (Kind::GitLab, Op::Dispatch(_, branch)) => request(forge, "POST", &format!("{}/pipeline", gitlab_project(forge)), Some(json!({"ref": branch}))).map(drop),
        (Kind::Gitea, Op::Dispatch(id, branch)) => request(forge, "POST", &format!("{}/actions/workflows/{id}/dispatches", gitea_repo(forge)), Some(json!({"ref": branch}))).map(drop),
        (Kind::Gitea, Op::Cancel(id)) => request(forge, "POST", &format!("{}/actions/runs/{id}/cancel", gitea_repo(forge)), None).map(drop),
        (Kind::Gitea, Op::Rerun(id)) => request(forge, "POST", &format!("{}/actions/runs/{id}/rerun", gitea_repo(forge)), None).map(drop),
    }
}

pub fn needs_token(e: &anyhow::Error) -> bool {
    e.to_string() == NEEDS_TOKEN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_remote_addresses() {
        let p = |u: &str| parse_remote(u);
        assert_eq!(p("https://github.com/o/r.git"), Some(("https://github.com".into(), "o/r".into())));
        assert_eq!(p("git@gitlab.corp.com:team/sub/lib.git"), Some(("https://gitlab.corp.com".into(), "team/sub/lib".into())));
        assert_eq!(p("ssh://git@gitea.local:2222/me/app.git"), Some(("https://gitea.local".into(), "me/app".into())));
        assert_eq!(p("https://user:pw@git.example.org:8443/me/app"), Some(("https://git.example.org:8443".into(), "me/app".into())));
        assert_eq!(p("http://10.0.0.5:3000/me/app.git"), Some(("http://10.0.0.5:3000".into(), "me/app".into())));
        assert_eq!(p("nonsense"), None);
    }

    #[test]
    fn encodes_basic_credentials() {
        assert_eq!((base64(b"me:pw"), base64(b"ab"), base64(b"a")), ("bWU6cHc=".into(), "YWI=".into(), "YQ==".into()));
    }

    #[test]
    fn maps_statuses_to_github_s() {
        assert_eq!(normalize("running"), ("in_progress".into(), String::new()));
        assert_eq!(normalize("failed"), ("completed".into(), "failure".into()));
        assert_eq!(normalize("canceled"), ("completed".into(), "cancelled".into()));
        assert_eq!(normalize("pending"), ("queued".into(), String::new()));
    }

    #[test]
    fn cleans_gitlab_traces() {
        let raw = "\x1b[0Ksection_start:1700000000:build[collapsed=true]\r\x1b[0K\x1b[36;1mBuilding\x1b[0;m\r\nok\n";
        assert_eq!(clean_log(raw), "Building\nok\n");
    }

    /// A fake GitLab answering one pipeline and its job: the rows the view shows.
    #[test]
    fn reads_gitlab_pipelines() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in server.incoming().take(2).flatten() {
                use std::io::{BufRead as _, Write as _};
                let mut reader = std::io::BufReader::new(&stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let body = if first.contains("/jobs") {
                    r#"[{"id": 7, "name": "compile", "stage": "build", "status": "failed", "web_url": "u/7"}]"#
                } else {
                    r#"[{"id": 42, "status": "running", "ref": "main", "sha": "0123456789", "source": "push", "created_at": "2026-11-27T10:00:00Z", "web_url": "u/42"}]"#
                };
                let mut s = &stream;
                write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let forge = Forge { kind: Kind::GitLab, base, project: "team/lib".into(), root: std::env::temp_dir() };
        let runs = runs(&forge).unwrap();
        assert_eq!((runs[0].database_id, runs[0].display_title.as_str(), runs[0].running()), (42, "Pipeline #42 01234567", true));
        let jobs = jobs(&forge, 42).unwrap();
        assert_eq!((jobs[0].id, jobs[0].name.as_str(), jobs[0].conclusion.as_str()), (7, "build: compile", "failure"));
    }
}
