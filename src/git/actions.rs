//! The repository's CI, like VS Code's GitHub Actions view: the latest runs (GitHub Actions,
//! GitLab pipelines, Gitea / Forgejo Actions), their jobs, re-run / cancel / start one and read a
//! run's log. GitHub through its CLI (`gh`), which holds the sign-in; the others through
//! [`super::forge`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::Deserialize;

use super::forge::{self, Forge, Op};
use crate::i18n::t;
use gpui_kit::component::input::{Input, InputState};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub database_id: u64,
    pub display_title: String,
    pub workflow_name: String,
    pub head_branch: String,
    pub event: String,
    pub status: String,
    pub conclusion: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Job {
    #[serde(default, alias = "databaseId")]
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    /// What starts it: GitHub's id, Gitea's file name (empty for GitLab's pipeline).
    pub id: String,
    pub name: String,
    pub state: String,
}

impl Run {
    pub fn running(&self) -> bool {
        self.status != "completed"
    }
}

/// `gh` in the repository, its output or its error message.
fn gh(root: &Path, args: &[&str]) -> Result<String> {
    // Found where Homebrew & co. put it, even when the app was started from the Dock.
    let out = crate::util::command(crate::lsp::install::which("gh").unwrap_or_else(|| "gh".into()))
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|_| anyhow!("{}", t("GitHub CLI (gh) not found: install it, then run `gh auth login`.")))?;
    if !out.status.success() {
        return Err(anyhow!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn gh_runs(root: &Path) -> Result<Vec<Run>> {
    let json = gh(root, &["run", "list", "-L", "20", "--json", "databaseId,displayTitle,workflowName,headBranch,event,status,conclusion,createdAt,url"])?;
    Ok(serde_json::from_str(&json)?)
}

pub fn gh_jobs(root: &Path, run: u64) -> Result<Vec<Job>> {
    #[derive(Deserialize)]
    struct Jobs {
        jobs: Vec<Job>,
    }
    let json = gh(root, &["run", "view", &run.to_string(), "--json", "jobs"])?;
    Ok(serde_json::from_str::<Jobs>(&json)?.jobs)
}

pub fn gh_workflows(root: &Path) -> Result<Vec<Workflow>> {
    #[derive(Deserialize)]
    struct Gh {
        id: u64,
        name: String,
        state: String,
    }
    let list: Vec<Gh> = serde_json::from_str(&gh(root, &["workflow", "list", "--json", "id,name,state"])?)?;
    Ok(list.into_iter().map(|w| Workflow { id: w.id.to_string(), name: w.name, state: w.state }).collect())
}

pub fn gh_act(root: &Path, op: Op) -> Result<()> {
    let args: Vec<String> = match op {
        Op::Cancel(id) => vec!["run".into(), "cancel".into(), id.to_string()],
        Op::Rerun(id) => vec!["run".into(), "rerun".into(), id.to_string()],
        // On the current branch; it needs `on: workflow_dispatch`, else gh says so.
        Op::Dispatch(id, branch) => {
            let mut a = vec!["workflow".into(), "run".into(), id];
            if !branch.is_empty() {
                a.extend(["--ref".into(), branch]);
            }
            a
        }
    };
    gh(root, &args.iter().map(String::as_str).collect::<Vec<_>>()).map(drop)
}

/// The run's log: only the failed steps when it failed (the whole log is huge).
pub fn gh_log(root: &Path, run: &Run) -> Result<String> {
    let id = run.database_id.to_string();
    let failed = run.conclusion == "failure";
    gh(root, &["run", "view", &id, if failed { "--log-failed" } else { "--log" }])
}

/// The state shown for a run or a job.
fn state_icon(status: &str, conclusion: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (icon, color) = match (status, conclusion) {
        ("completed", "success") => (IconName::CircleCheck, theme.success),
        ("completed", "failure" | "timed_out" | "startup_failure") => (IconName::CircleX, theme.danger),
        ("completed", "cancelled") => (IconName::SquareSlash, theme.muted_foreground),
        ("completed", _) => (IconName::Circle, theme.muted_foreground),
        ("in_progress", _) => return Spinner::new().xsmall().color(theme.warning).into_any_element(),
        _ => (IconName::Clock, theme.warning),
    };
    div().flex_none().text_color(color).child(icon).into_any_element()
}

/// "3 min", "2 h", "5 d" since an RFC 3339 time.
fn ago(created: &str) -> String {
    let parse = |s: &str| -> Option<i64> {
        // 2026-11-26T19:50:30Z → seconds since the epoch, enough for "how long ago".
        let (date, time) = s.trim_end_matches('Z').split_once('T')?;
        let d: Vec<i64> = date.split('-').filter_map(|p| p.parse().ok()).collect();
        let h: Vec<i64> = time.split(':').filter_map(|p| p.parse().ok()).collect();
        let (y, m, day) = (*d.first()?, *d.get(1)?, *d.get(2)?);
        let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
        let days = 365 * y2 + y2 / 4 - y2 / 100 + y2 / 400 + (153 * m2 + 2) / 5 + day - 719469;
        Some(days * 86400 + h.first()? * 3600 + h.get(1)? * 60 + h.get(2).copied().unwrap_or(0))
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let Some(at) = parse(created) else { return String::new() };
    match (now - at).max(0) {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86400 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86400),
    }
}

pub enum ActionsEvent {
    /// Show a run's log (title, text).
    Log(String, String),
}

pub struct ActionsView {
    root: PathBuf,
    /// The server, once recognised.
    forge: Option<Forge>,
    /// The server refused or wanted a sign-in: the token field is shown.
    needs_token: bool,
    token: Entity<InputState>,
    runs: Vec<Run>,
    workflows: Vec<Workflow>,
    /// The run whose jobs are shown, and its jobs.
    expanded: Option<(u64, Vec<Job>)>,
    error: Option<String>,
    loading: bool,
    collapsed: bool,
    _poll: Option<Task<()>>,
}

impl EventEmitter<ActionsEvent> for ActionsView {}

impl ActionsView {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let token = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(t("Access token")));
        let mut this = Self { root, forge: None, needs_token: false, token, runs: vec![], workflows: vec![], expanded: None, error: None, loading: false, collapsed: false, _poll: None };
        this.refresh(cx);
        this
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.root = root;
        self.forge = None;
        self.runs.clear();
        self.expanded = None;
        self.refresh(cx);
    }

    /// Read the runs again; while one is running, again every 10 s (as VS Code's view does).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let (root, known) = (self.root.clone(), self.forge.clone());
        let expanded = self.expanded.as_ref().map(|(id, _)| *id);
        self.loading = true;
        cx.notify();
        self._poll = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let forge = match known {
                        Some(f) => f,
                        None => forge::detect(&root)?,
                    };
                    let found = forge.clone();
                    let fetched = (|| {
                        let runs = forge::runs(&forge)?;
                        let workflows = forge::workflows(&forge).unwrap_or_default();
                        let jobs = expanded.map(|id| (id, forge::jobs(&forge, id).unwrap_or_default()));
                        Ok::<_, anyhow::Error>((runs, workflows, jobs))
                    })();
                    Ok::<_, anyhow::Error>((found, fetched))
                })
                .await;
            let again = this
                .update(cx, |this, cx| {
                    this.loading = false;
                    let result = match result {
                        Ok((found, fetched)) => {
                            this.forge = Some(found);
                            fetched
                        }
                        Err(e) => Err(e),
                    };
                    this.needs_token = result.as_ref().err().is_some_and(forge::needs_token);
                    match result {
                        Ok((runs, workflows, jobs)) => {
                            this.error = None;
                            this.runs = runs;
                            this.workflows = workflows;
                            if jobs.is_some() {
                                this.expanded = jobs;
                            }
                        }
                        Err(e) if forge::needs_token(&e) => this.error = Some(t("Sign-in needed: paste an access token for this server.").into()),
                        Err(e) => this.error = Some(e.to_string()),
                    }
                    cx.notify();
                    this.runs.iter().any(Run::running)
                })
                .unwrap_or(false);
            if again {
                cx.background_executor().timer(Duration::from_secs(10)).await;
                let _ = this.update(cx, |this, cx| this.refresh(cx));
            }
        }));
    }

    /// Cancel, re-run or start a run on the server, then read the runs again.
    fn act(&mut self, op: Op, cx: &mut Context<Self>) {
        let Some(forge) = self.forge.clone() else { return };
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { forge::act(&forge, op) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.error = Some(e.to_string());
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    fn toggle_jobs(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.expanded.as_ref().is_some_and(|(e, _)| *e == id) {
            self.expanded = None;
            return cx.notify();
        }
        let Some(forge) = self.forge.clone() else { return };
        self.expanded = Some((id, vec![]));
        cx.spawn(async move |this, cx| {
            let jobs = cx.background_spawn(async move { forge::jobs(&forge, id) }).await;
            let _ = this.update(cx, |this, cx| {
                match jobs {
                    Ok(jobs) if this.expanded.as_ref().is_some_and(|(e, _)| *e == id) => this.expanded = Some((id, jobs)),
                    Err(e) => this.error = Some(e.to_string()),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn show_log(&mut self, run: Run, cx: &mut Context<Self>) {
        let Some(forge) = self.forge.clone() else { return };
        cx.spawn(async move |this, cx| {
            let log = cx.background_spawn({
                let run = run.clone();
                async move { forge::log(&forge, &run) }
            })
            .await;
            let _ = this.update(cx, |this, cx| match log {
                Ok(text) => cx.emit(ActionsEvent::Log(format!("{} #{}", run.workflow_name, run.database_id), text)),
                Err(e) => {
                    this.error = Some(e.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn render_run(&self, run: &Run, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let id = run.database_id;
        let open = self.expanded.as_ref().filter(|(e, _)| *e == id).map(|(_, jobs)| jobs.clone());
        let (url, r1, r2, r3) = (run.url.clone(), run.clone(), run.clone(), run.clone());
        let action = |name: &str, icon: IconName, tip: &'static str| Button::new(SharedString::from(format!("{name}-{id}"))).ghost().xsmall().icon(icon).tooltip(t(tip));
        v_flex()
            .child(
                h_flex()
                    .id(SharedString::from(format!("run-{id}")))
                    .group(SharedString::from(format!("run-{id}")))
                    .h(px(36.))
                    .px_2()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_jobs(id, cx)))
                    .child(div().text_color(theme.muted_foreground).child(if open.is_some() { IconName::ChevronDown } else { IconName::ChevronRight }))
                    .child(state_icon(&run.status, &run.conclusion, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(run.display_title.clone()))
                            .child(div().text_xs().truncate().text_color(theme.muted_foreground).child(format!("{} | {} | {}", run.workflow_name, run.head_branch, ago(&run.created_at)))),
                    )
                    .child(
                        h_flex()
                            .invisible()
                            .group_hover(SharedString::from(format!("run-{id}")), |s| s.visible())
                            .child(action("log", IconName::ScrollText, "Show the log").on_click(cx.listener(move |this, _, _, cx| this.show_log(r1.clone(), cx))))
                            .when(run.running(), |el| {
                                el.child(action("cancel", IconName::Square, "Cancel").on_click(cx.listener(move |this, _, _, cx| this.act(Op::Cancel(r2.database_id), cx))))
                            })
                            .when(!run.running(), |el| {
                                el.child(action("rerun", IconName::RefreshCw, "Re-run").on_click(cx.listener(move |this, _, _, cx| this.act(Op::Rerun(r3.database_id), cx))))
                            })
                            .child(action("open", IconName::ExternalLink, "Open in the browser").on_click(move |_, _, cx| cx.open_url(&url))),
                    ),
            )
            .when_some(open, |el, jobs| {
                el.child(v_flex().pl_8().when(jobs.is_empty(), |el| el.child(div().h(px(24.)).child(Spinner::new().xsmall()))).children(jobs.into_iter().enumerate().map(|(i, job)| {
                    let url = job.url.clone();
                    h_flex()
                        .id(SharedString::from(format!("job-{id}-{i}")))
                        .h(px(24.))
                        .px_2()
                        .gap_2()
                        .text_xs()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .child(state_icon(&job.status, &job.conclusion, cx))
                        .child(div().flex_1().truncate().child(job.name))
                })))
            })
            .into_any_element()
    }
}

impl Render for ActionsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let collapsed = self.collapsed;
        let runs: Vec<AnyElement> = if collapsed { vec![] } else { self.runs.clone().iter().map(|r| self.render_run(r, cx)).collect() };
        let view = cx.entity().downgrade();
        let workflows = self.workflows.clone();
        let branch_root = self.root.clone();
        v_flex()
            .w_full()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .id("actions-header")
                    .h(px(28.))
                    .px_2()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.collapsed = !this.collapsed;
                        cx.notify();
                    }))
                    .child(div().text_color(theme.muted_foreground).child(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }))
                    .child(div().flex_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(self.forge.as_ref().map_or("CI", |f| f.kind.title())))
                    .when(self.loading, |el| el.child(Spinner::new().xsmall()))
                    .child(Button::new("actions-run").ghost().xsmall().icon(IconName::Play).tooltip(t("Run a workflow")).dropdown_menu(move |mut menu, _, _| {
                        let manual: Vec<&Workflow> = workflows.iter().filter(|w| w.state == "active").collect();
                        if manual.is_empty() {
                            menu = menu.label(t("No workflow"));
                        }
                        for w in manual {
                            let (view, id, root) = (view.clone(), w.id.clone(), branch_root.clone());
                            menu = menu.item(PopupMenuItem::new(w.name.clone()).on_click(move |_, _, cx| {
                                // On the current branch.
                                let branch = crate::git::Repo::discover(&root).and_then(|r| r.status().ok()).map(|s| s.branch).unwrap_or_default();
                                let _ = view.update(cx, |this, cx| this.act(Op::Dispatch(id.clone(), branch), cx));
                            }));
                        }
                        menu
                    }))
                    .child(Button::new("actions-refresh").ghost().xsmall().icon(IconName::RefreshCw).tooltip(t("Refresh")).on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))),
            )
            .when(!collapsed, |el| {
                el.when_some(self.error.clone(), |el, e| el.child(div().px_3().py_1().text_xs().text_color(theme.danger).child(e)))
                    .when_some(self.forge.clone().filter(|f| self.needs_token && f.kind != forge::Kind::GitHub), |el, f| {
                        let page = forge::token_page(&f);
                        el.child(
                            h_flex()
                                .px_2()
                                .gap_1()
                                .child(div().flex_1().child(Input::new(&self.token).xsmall()))
                                .child(Button::new("token-save").xsmall().primary().label(t("Save")).on_click(cx.listener(move |this, _, window, cx| {
                                    let token = this.token.read(cx).value().to_string();
                                    match forge::save_token(&f.base, &token) {
                                        Ok(()) => {
                                            this.token.update(cx, |i, cx| i.set_value("", window, cx));
                                            this.refresh(cx);
                                        }
                                        Err(e) => this.error = Some(e.to_string()),
                                    }
                                })))
                                .child(Button::new("token-create").xsmall().ghost().icon(IconName::ExternalLink).tooltip(t("Create a token")).on_click(move |_, _, cx| cx.open_url(&page))),
                        )
                    })
                    .when(self.runs.is_empty() && self.error.is_none() && !self.loading, |el| {
                        el.child(div().px_3().py_1().text_xs().text_color(theme.muted_foreground).child(t("No workflow run yet.")))
                    })
                    .child(div().id("actions-runs").max_h(px(320.)).overflow_y_scroll().children(runs))
            })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_gh_runs_and_ages() {
        let json = r#"[{"conclusion":"","createdAt":"2026-11-26T19:50:30Z","databaseId":45886620401,"displayTitle":"Release","event":"workflow_dispatch","headBranch":"main","status":"in_progress","url":"https://github.com/o/r/actions/runs/1","workflowName":"Release"}]"#;
        let runs: Vec<super::Run> = serde_json::from_str(json).unwrap();
        assert_eq!((runs[0].database_id, runs[0].workflow_name.as_str(), runs[0].running()), (45886620401, "Release", true));
        // The epoch itself is decades ago; a bad date gives nothing.
        assert!(super::ago("1970-01-01T00:00:00Z").ends_with(" d"));
        assert_eq!(super::ago("nonsense"), "");
    }

    /// Against this repository on GitHub (run by hand: needs gh signed in).
    #[test]
    #[ignore]
    fn live_runs_of_this_repository() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let forge = super::forge::detect(root).unwrap();
        assert_eq!(forge.kind, super::forge::Kind::GitHub);
        let runs = super::forge::runs(&forge).unwrap();
        eprintln!("{runs:#?}");
        let first = runs.first().expect("a run");
        eprintln!("{:#?}", super::gh_jobs(root, first.database_id).unwrap());
        eprintln!("{:#?}", super::gh_workflows(root).unwrap());
    }
}
