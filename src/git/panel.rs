//! Source Control view: branch, sync, commit message, staged and unstaged changes.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::t;

use super::{FileStatus, Operation, Repo, Side, Status};

pub enum GitEvent {
    /// A Git operation (checkout, pull, stash…) finished: files on disk may have changed.
    OperationDone,
    OpenDiff { file: FileStatus, staged: bool },
    OpenFile(PathBuf),
    OpenGraph,
    /// Repository state changed (refresh decorations).
    Changed,
    /// Ask the workspace to confirm a destructive action.
    ConfirmDiscard(Vec<FileStatus>),
    NewBranch,
    RenameBranch(String),
    DeleteBranch(String),
    /// Ask for the server address to link as `origin`; `publish` pushes the branch once linked.
    SetRemote { publish: bool },
}

pub struct GitPanel {
    pub repo: Option<Repo>,
    workspace: PathBuf,
    pub status: Status,
    branches: Vec<(String, bool)>,
    message: Entity<TextareaState>,
    busy: Option<&'static str>,
    error: Option<String>,
    files_scroll: UniformListScrollHandle,
    /// The repository's GitHub Actions runs, under the changes (like VS Code's view).
    pub actions: Entity<super::actions::ActionsView>,
    /// File lines built by renders so far (tests check only those on screen are).
    #[cfg(test)]
    built: std::cell::Cell<usize>,
    _task: Option<Task<()>>,
}

impl EventEmitter<GitEvent> for GitPanel {}

/// Color for a status letter.
pub fn letter_color(letter: char, cx: &App) -> Hsla {
    let t = cx.theme();
    match letter {
        'A' | 'U' | '?' => t.green,
        'D' => t.red,
        'R' | 'C' => t.blue,
        _ => t.yellow,
    }
}

/// The model's commit message as one line: the first that isn't a fence or blank.
fn one_line(text: &str) -> String {
    text.lines().map(|l| l.trim().trim_matches('`').trim()).find(|l| !l.is_empty()).unwrap_or("").trim_end_matches('.').to_string()
}

impl GitPanel {
    pub fn new(workspace: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let message = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 6).placeholder("Message"));
        let actions = cx.new(|cx| super::actions::ActionsView::new(workspace.clone(), cx));
        let mut this = Self { repo: None, workspace, status: Status::default(), branches: vec![], message, busy: None, error: None, files_scroll: UniformListScrollHandle::new(), actions, #[cfg(test)] built: Default::default(), _task: None };
        this.set_workspace(this.workspace.clone(), cx);
        this
    }

    pub fn set_workspace(&mut self, workspace: PathBuf, cx: &mut Context<Self>) {
        self.repo = Repo::discover(&workspace);
        if workspace != self.workspace {
            self.actions.update(cx, |a, cx| a.set_root(workspace.clone(), cx));
        }
        self.workspace = workspace;
        self.refresh(cx);
    }

    pub fn changed_count(&self) -> usize {
        self.status.files.len()
    }

    /// The Refresh button: ask the remotes for their branches and commits first (new or deleted
    /// branches, commits to pull), then read the repository again.
    pub fn fetch_and_refresh(&mut self, cx: &mut Context<Self>) {
        if self.repo.is_none() {
            return self.refresh(cx);
        }
        self.run("Fetching", cx, |r| r.fetch().map(drop));
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        // The folder may have become a repository since (`git init` in the terminal or by the AI).
        if self.repo.is_none() {
            self.repo = Repo::discover(&self.workspace);
        }
        let Some(repo) = self.repo.clone() else { return cx.notify() };
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { Ok::<_, anyhow::Error>((repo.status()?, repo.branches()?)) }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    // Keeps the error of the operation that triggered this refresh on screen.
                    Ok((status, branches)) => {
                        this.status = status;
                        this.branches = branches;
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.emit(GitEvent::Changed);
                cx.notify();
            });
        }));
    }

    /// Run a git operation in the background, then refresh.
    fn run(&mut self, label: &'static str, cx: &mut Context<Self>, op: impl FnOnce(&Repo) -> anyhow::Result<()> + Send + 'static) {
        let Some(repo) = self.repo.clone() else { return };
        self.busy = Some(label);
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { op(&repo) }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                this.error = result.err().map(|e| e.to_string());
                this.refresh(cx);
                cx.emit(GitEvent::OperationDone);
            });
        })
        .detach();
    }

    pub fn stage(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        self.run("Staging", cx, move |r| r.stage(&paths));
    }

    pub fn untrack(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        self.run("Untracking", cx, move |r| r.untrack(&paths));
    }

    pub fn unstage(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        self.run("Unstaging", cx, move |r| r.unstage(&paths));
    }

    pub fn discard(&mut self, files: Vec<FileStatus>, cx: &mut Context<Self>) {
        self.run("Discarding", cx, move |r| r.discard(&files));
    }

    /// A new branch from the current one, switched to; `publish` pushes it so it exists on the
    /// remote too (tracked), else it stays local until pushed.
    pub fn create_branch(&mut self, branch: String, publish: bool, cx: &mut Context<Self>) {
        self.run("Creating branch", cx, move |r| {
            r.checkout(&branch, true)?;
            if publish {
                r.publish_branch(&branch)?;
            }
            Ok(())
        });
    }

    pub fn checkout(&mut self, branch: String, create: bool, cx: &mut Context<Self>) {
        self.run("Switching branch", cx, move |r| r.checkout(&branch, create));
    }

    /// Local branches, current first.
    pub fn branches(&self) -> &[(String, bool)] {
        &self.branches
    }

    pub fn delete_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        self.run("Deleting branch", cx, move |r| r.delete_branch(&branch));
    }

    pub fn rename_branch(&mut self, from: String, to: String, cx: &mut Context<Self>) {
        self.run("Renaming branch", cx, move |r| r.rename_branch(&from, &to));
    }

    pub fn merge(&mut self, branch: String, cx: &mut Context<Self>) {
        self.run("Merging", cx, move |r| r.merge(&branch));
    }

    pub fn rebase(&mut self, onto: String, cx: &mut Context<Self>) {
        self.run("Rebasing", cx, move |r| r.rebase(&onto));
    }

    fn resolve(&mut self, path: String, side: Side, cx: &mut Context<Self>) {
        self.run("Resolving", cx, move |r| r.resolve(&path, side));
    }

    /// Link to the server at `url` (`origin`), then fetch its branches or publish the current one.
    pub fn set_remote(&mut self, url: String, publish: bool, cx: &mut Context<Self>) {
        let status = self.status.clone();
        self.run("Linking", cx, move |r| {
            r.set_remote(&url)?;
            if publish { r.push(&status).map(drop) } else { r.fetch().map(drop) }
        });
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        // Nothing to publish to yet: ask for the server first.
        if self.status.upstream.is_none() && self.repo.as_ref().is_some_and(|r| r.remote_url().is_none()) {
            return cx.emit(GitEvent::SetRemote { publish: true });
        }
        let status = self.status.clone();
        self.run("Syncing", cx, move |r| {
            if status.behind > 0 || status.upstream.is_some() {
                r.pull()?;
            }
            r.push(&status).map(drop)
        });
    }

    /// Ask the chat model for a one-line conventional commit message describing the pending diff.
    fn generate_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        let backend = match crate::ai::chat_panel::chat_backend(cx) {
            Ok(b) => b,
            Err(e) => {
                self.error = Some(e.to_string());
                return cx.notify();
            }
        };
        self.busy = Some("…");
        cx.notify();
        let message = self.message.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let mut diff = repo.pending_diff()?;
                    if diff.trim().is_empty() {
                        anyhow::bail!("No changes to describe.");
                    }
                    diff = crate::ai::tools::truncate(diff);
                    let messages = [
                        serde_json::json!({"role": "system", "content": "Write the shortest possible git commit message for this diff, in the Conventional Commits style: a single line `type(scope): summary`, ideally under 50 characters, no body, no bullet points, no trailing period. Reply with that line only, no quotes or code fences."}),
                        serde_json::json!({"role": "user", "content": diff}),
                    ];
                    backend.complete(&messages, 300)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(text) => {
                        let text = one_line(&text);
                        message.update(cx, |m, cx| m.set_value(text, window, cx));
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn commit(&mut self, amend: bool, push: bool, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.message.read(cx).value().trim().to_string();
        if message.is_empty() && !amend {
            self.error = Some("Write a commit message first.".into());
            return cx.notify();
        }
        // Like VS Code: with nothing staged, commit every change.
        let stage_all = !self.status.files.iter().any(FileStatus::staged);
        let all: Vec<String> = self.status.files.iter().map(|f| f.path.clone()).collect();
        let status = self.status.clone();
        self.message.update(cx, |m, cx| m.set_value("", window, cx));
        self.run("Committing", cx, move |r| {
            if stage_all {
                r.stage(&all)?;
            }
            r.commit(&message, amend)?;
            if push {
                r.push(&status)?;
            }
            Ok(())
        });
    }

    fn init_repo(&mut self, cx: &mut Context<Self>) {
        match Repo::init(&self.workspace) {
            Ok(repo) => self.repo = Some(repo),
            Err(e) => self.error = Some(e.to_string()),
        }
        self.refresh(cx);
    }

    fn render_file(&self, file: &FileStatus, staged: bool, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        self.built.set(self.built.get() + 1);
        let theme = cx.theme();
        let letter = file.letter(staged);
        let (dir, name) = file.path.rsplit_once('/').map_or(("", file.path.as_str()), |(d, n)| (d, n));
        let id = SharedString::from(format!("{}-{}", if staged { "s" } else { "u" }, file.path));
        let root = self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_default();
        let (f1, f2, f3) = (file.clone(), file.clone(), file.clone());
        h_flex()
            .id(id.clone())
            .group(id.clone())
            // List items are as wide as their content otherwise: the letter must sit on the right edge.
            .w_full()
            .relative()
            .h(px(24.))
            .pl_4()
            .pr_1()
            .gap_1()
            .text_sm()
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(GitEvent::OpenDiff { file: f1.clone(), staged })))
            .child(div().min_w_0().truncate().child(name.to_string()))
            .child(div().debug_selector({ let p = file.path.clone(); move || format!("git-path-{p}") }).flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(dir.to_string()))
            // Over the path on hover, so they take no room the rest of the time.
            .child(
                h_flex()
                    .absolute()
                    .top_0()
                    .right(px(22.))
                    .h_full()
                    .pl_1()
                    .bg(theme.list_hover)
                    .invisible()
                    .group_hover(id, |s| s.visible())
                    .child(Button::new(SharedString::from(format!("open-{}", file.path))).ghost().xsmall().icon(IconName::FileText).tooltip(t("Open file"))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(GitEvent::OpenFile(root.join(&f2.path))))))
                    .when(!staged, |el| {
                        let f = f3.clone();
                        el.child(Button::new(SharedString::from(format!("discard-{}", file.path))).ghost().xsmall().icon(IconName::Undo2).tooltip(t("Discard changes"))
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(GitEvent::ConfirmDiscard(vec![f.clone()])))))
                    })
                    .child({
                        let path = file.path.clone();
                        Button::new(SharedString::from(format!("toggle-{}-{staged}", file.path))).ghost().xsmall()
                            .icon(if staged { IconName::Minus } else { IconName::Plus })
                            .tooltip(if staged { "Unstage" } else { "Stage" })
                            .on_click(cx.listener(move |t, _, _, cx| if staged { t.unstage(vec![path.clone()], cx) } else { t.stage(vec![path.clone()], cx) }))
                    }),
            )
            .child(div().debug_selector({ let p = file.path.clone(); move || format!("git-letter-{p}") }).flex_none().w(px(14.)).text_xs().font_weight(FontWeight::BOLD).text_color(letter_color(letter, cx)).child(letter.to_string()))
    }

    /// Merge/rebase in progress: what's going on, Continue (once resolved) and Abort.
    fn render_operation(&self, op: Operation, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let conflicts = self.status.files.iter().filter(|f| f.conflicted()).count();
        let title = match op {
            Operation::Merge => t("Merge in progress"),
            Operation::Rebase => t("Rebase in progress"),
            Operation::CherryPick => t("Cherry-pick in progress"),
            Operation::Revert => t("Revert in progress"),
        };
        let detail = if conflicts > 0 { format!("{conflicts} {}", t("conflict(s) to resolve")) } else { t("All conflicts resolved").to_string() };
        v_flex()
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(theme.warning)
            .bg(theme.warning.opacity(0.08))
            .child(h_flex().gap_1().text_xs().font_weight(FontWeight::MEDIUM).child(IconName::GitMerge).child(title))
            .child(div().text_xs().text_color(theme.muted_foreground).child(detail))
            .child(
                h_flex()
                    .gap_1()
                    .child(Button::new("op-continue").xsmall().primary().label(t("Continue")).disabled(conflicts > 0)
                        .on_click(cx.listener(move |t, _, _, cx| t.run("Continuing", cx, move |r| r.continue_(op)))))
                    .child(Button::new("op-abort").xsmall().ghost().label(t("Abort"))
                        .on_click(cx.listener(move |t, _, _, cx| t.run("Aborting", cx, move |r| r.abort(op))))),
            )
    }

    /// Conflicted files, each with VS Code's resolutions.
    fn render_conflicts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files: Vec<FileStatus> = self.status.files.iter().filter(|f| f.conflicted()).cloned().collect();
        let theme = cx.theme().clone();
        let root = self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_default();
        v_flex().when(!files.is_empty(), |el| {
            el.child(
                h_flex().h(px(24.)).px_2().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.danger)
                    .child(format!("{}  {}", t("MERGE CONFLICTS"), files.len())),
            )
            .children(files.into_iter().map(|f| {
                let (name, dir) = f.path.rsplit_once('/').map_or((f.path.clone(), String::new()), |(d, n)| (n.to_string(), d.to_string()));
                let id = SharedString::from(format!("c-{}", f.path));
                let (p1, p2, p3, p4) = (f.path.clone(), f.path.clone(), f.path.clone(), root.join(&f.path));
                let action = |id: String, icon: IconName, tip: &'static str| Button::new(SharedString::from(id)).ghost().xsmall().icon(icon).tooltip(t(tip));
                h_flex()
                    .id(id.clone())
                    .group(id.clone())
                    .h(px(24.))
                    .pl_4()
                    .pr_1()
                    .gap_1()
                    .text_sm()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(GitEvent::OpenFile(p4.clone()))))
                    .child(div().truncate().child(name))
                    .child(div().flex_1().truncate().text_xs().text_color(theme.muted_foreground).child(dir))
                    .child(
                        h_flex()
                            .invisible()
                            .group_hover(id, |s| s.visible())
                            .child(action(format!("ours-{}", f.path), IconName::ArrowLeft, "Accept current (whole file)")
                                .on_click(cx.listener(move |t, _, _, cx| t.resolve(p1.clone(), Side::Ours, cx))))
                            .child(action(format!("theirs-{}", f.path), IconName::ArrowRight, "Accept incoming (whole file)")
                                .on_click(cx.listener(move |t, _, _, cx| t.resolve(p2.clone(), Side::Theirs, cx))))
                            .child(action(format!("resolved-{}", f.path), IconName::Check, "Mark as resolved")
                                .on_click(cx.listener(move |t, _, _, cx| t.resolve(p3.clone(), Side::AsIs, cx)))),
                    )
                    .child(div().w(px(14.)).text_xs().font_weight(FontWeight::BOLD).text_color(theme.danger).child("!"))
            }))
        })
    }

    /// A section's title line: its name, count and actions on all its files.
    fn render_section_header(&self, staged: bool, cx: &mut Context<Self>) -> AnyElement {
        let title = if staged { "STAGED CHANGES" } else { "CHANGES" };
        let files: Vec<FileStatus> = self.status.files.iter().filter(|f| if staged { f.staged() } else { f.unstaged() }).cloned().collect();
        let muted = cx.theme().muted_foreground;
        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let count = files.len();
        let all = files;
        h_flex()
            .w_full()
            .h(px(24.))
            .px_2()
            .gap_1()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(muted)
            .child(div().flex_1().min_w_0().truncate().child(format!("{}  {}", t(title), count)))
            .when(!staged, |el| {
                el.child(Button::new("discard-all").ghost().xsmall().icon(IconName::Undo2).tooltip(t("Discard all changes"))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(GitEvent::ConfirmDiscard(all.clone())))))
            })
            .child(Button::new(if staged { "unstage-all" } else { "stage-all" }).ghost().xsmall()
                .icon(if staged { IconName::Minus } else { IconName::Plus })
                .tooltip(if staged { "Unstage all" } else { "Stage all" })
                .on_click(cx.listener(move |t, _, _, cx| if staged { t.unstage(paths.clone(), cx) } else { t.stage(paths.clone(), cx) })))
            .into_any_element()
    }

    /// The lines of the files list, in order: (`None` for a section title, else the file's
    /// index in the status; staged section or not).
    fn file_rows(&self) -> Vec<(Option<usize>, bool)> {
        let mut rows = vec![];
        for staged in [true, false] {
            let files: Vec<usize> = (0..self.status.files.len()).filter(|&i| if staged { self.status.files[i].staged() } else { self.status.files[i].unstaged() }).collect();
            if !files.is_empty() {
                rows.push((None, staged));
                rows.extend(files.into_iter().map(|i| (Some(i), staged)));
            }
        }
        rows
    }
}

impl Render for GitPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let conflicts_section = self.render_conflicts(cx).into_any_element();
        let operation = self.status.operation.map(|op| self.render_operation(op, cx).into_any_element());
        let rows = std::rc::Rc::new(self.file_rows());
        let theme = cx.theme().clone();
        let header = h_flex()
            .h(px(36.))
            .px_3()
            .gap_1()
            .child(div().flex_1().min_w_0().truncate().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(t("SOURCE CONTROL")))
            .when(self.repo.is_some(), |el| {
                el.child(Button::new("git-graph").ghost().xsmall().icon(IconName::Network).tooltip(t("History graph")).on_click(cx.listener(|_, _, _, cx| cx.emit(GitEvent::OpenGraph))))
                    .child(Button::new("git-refresh").ghost().xsmall().icon(IconName::RefreshCw).tooltip(t("Refresh")).on_click(cx.listener(|t, _, _, cx| t.fetch_and_refresh(cx))))
                    .child(Button::new("git-more").ghost().xsmall().icon(IconName::Ellipsis).dropdown_menu({
                        let view = cx.entity().downgrade();
                        move |menu, _, _| {
                            let (v1, v2, v3, v4, v5) = (view.clone(), view.clone(), view.clone(), view.clone(), view.clone());
                            menu.item(PopupMenuItem::new(t("Pull")).on_click(move |_, _, cx| { let _ = v1.update(cx, |t, cx| t.run("Pulling", cx, |r| r.pull().map(drop))); }))
                                .item(PopupMenuItem::new(t("Push")).on_click(move |_, _, cx| { let _ = v2.update(cx, |t, cx| { let s = t.status.clone(); t.run("Pushing", cx, move |r| r.push(&s).map(drop)) }); }))
                                .item(PopupMenuItem::new(t("Fetch")).on_click(move |_, _, cx| { let _ = v3.update(cx, |t, cx| t.run("Fetching", cx, |r| r.fetch().map(drop))); }))
                                .item(PopupMenuItem::new(t("Remote repository…")).on_click(move |_, _, cx| { let _ = v5.update(cx, |_, cx| cx.emit(GitEvent::SetRemote { publish: false })); }))
                                .separator()
                                .item(PopupMenuItem::new(t("Commit (amend)")).on_click(move |_, w, cx| { let _ = v4.update(cx, |t, cx| t.commit(true, false, w, cx)); }))
                        }
                    }))
            });

        let Some(_) = &self.repo else {
            return v_flex()
                .size_full()
                .bg(theme.sidebar)
                .child(header)
                .child(
                    v_flex().p_3().gap_2().text_sm().text_color(theme.muted_foreground)
                        .child(t("Not a Git repository."))
                        .child(Button::new("git-init").small().primary().label(t("Initialize Repository")).on_click(cx.listener(|t, _, _, cx| t.init_repo(cx)))),
                );
        };

        let status = &self.status;
        let branch_label = if status.branch.is_empty() { "…".to_string() } else { status.branch.clone() };
        let sync_label = match (status.upstream.is_some(), status.ahead, status.behind) {
            (false, _, _) => t("Publish").to_string(),
            (true, 0, 0) => t("Sync").to_string(),
            (true, a, b) => format!("↓{b} ↑{a}"),
        };
        let branches = self.branches.clone();
        let view = cx.entity().downgrade();

        v_flex()
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .bg(theme.sidebar)
            .child(header)
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .px_2()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(div().flex_1().min_w_0().overflow_hidden().child(Button::new("branch").ghost().xsmall().icon(IconName::GitBranch).label(branch_label).dropdown_menu(move |mut menu, window, cx| {
                                for (name, current) in branches.clone() {
                                    let view = view.clone();
                                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(current).on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |t, cx| t.checkout(name.clone(), false, cx));
                                    }));
                                }
                                // Merge / rebase the current branch with another one.
                                let others: Vec<String> = branches.iter().filter(|(_, current)| !current).map(|(n, _)| n.clone()).collect();
                                let (merge_view, rebase_view, new_view, rename_view, delete_view) = (view.clone(), view.clone(), view.clone(), view.clone(), view.clone());
                                let current_branch = branches.iter().find(|(_, c)| *c).map(|(n, _)| n.clone());
                                let delete_list = others.clone();
                                let merge_list = others.clone();
                                menu = menu.separator().submenu(t("Merge into current branch"), window, cx, move |mut sub, _, _| {
                                    for name in merge_list.clone() {
                                        let view = merge_view.clone();
                                        sub = sub.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                                            let _ = view.update(cx, |t, cx| t.merge(name.clone(), cx));
                                        }));
                                    }
                                    sub
                                });
                                menu = menu.submenu(t("Rebase current branch onto"), window, cx, move |mut sub, _, _| {
                                    for name in others.clone() {
                                        let view = rebase_view.clone();
                                        sub = sub.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                                            let _ = view.update(cx, |t, cx| t.rebase(name.clone(), cx));
                                        }));
                                    }
                                    sub
                                });
                                menu = menu.separator().item(PopupMenuItem::new(t("Create branch…")).icon(IconName::Plus).on_click(move |_, _, cx| {
                                    let _ = new_view.update(cx, |_, cx| cx.emit(GitEvent::NewBranch));
                                }));
                                if let Some(current) = current_branch.clone() {
                                    menu = menu.item(PopupMenuItem::new(t("Rename branch…")).icon(IconName::Pencil).on_click(move |_, _, cx| {
                                        let _ = rename_view.update(cx, |_, cx| cx.emit(GitEvent::RenameBranch(current.clone())));
                                    }));
                                }
                                menu.submenu(t("Delete branch"), window, cx, move |mut sub, _, _| {
                                    for name in delete_list.clone() {
                                        let view = delete_view.clone();
                                        sub = sub.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                                            let _ = view.update(cx, |_, cx| cx.emit(GitEvent::DeleteBranch(name.clone())));
                                        }));
                                    }
                                    sub
                                })
                            })))
                            .child(Button::new("sync").ghost().xsmall().icon(IconName::RefreshCw).label(sync_label).tooltip(t("Sync"))
                                .on_click(cx.listener(|t, _, _, cx| t.sync(cx)))),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .px_1()
                            .on_key_down(cx.listener(|t, e: &KeyDownEvent, w, cx| {
                                if e.keystroke.key == "enter" && e.keystroke.modifiers.platform {
                                    t.commit(false, false, w, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .child(Textarea::new(&self.message).appearance(false)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Button::new("commit").small().primary().flex_1().icon(IconName::Check)
                                .label(t(self.busy.unwrap_or("Commit")))
                                .loading(self.busy.is_some())
                                .on_click(cx.listener(|t, _, w, cx| t.commit(false, false, w, cx))))
                            .child(Button::new("commit-ai").small().icon(IconName::Sparkles).tooltip(t("Generate a commit message"))
                                .on_click(cx.listener(|t, _, w, cx| t.generate_message(w, cx))))
                            .child(Button::new("commit-push").small().icon(IconName::ArrowUp).tooltip(t("Commit & Push"))
                                .on_click(cx.listener(|t, _, w, cx| t.commit(false, true, w, cx)))),
                    )
                    // Stays until the next git action or a click on it.
                    .children(operation)
                    .when_some(self.error.clone(), |el, e| {
                        el.child(
                            h_flex()
                                .id("git-error")
                                .items_start()
                                .gap_1()
                                .p_2()
                                .rounded_md()
                                .bg(theme.danger.opacity(0.1))
                                .text_xs()
                                .text_color(theme.danger)
                                .child(div().flex_1().min_w_0().child(e))
                                .child(Button::new("git-error-close").ghost().xsmall().icon(IconName::Close).tooltip(t("Close"))
                                    .on_click(cx.listener(|t, _, _, cx| { t.error = None; cx.notify(); }))),
                        )
                    }),
            )
            .child(conflicts_section)
            .when(!self.status.ignored_tracked.is_empty(), |el| {
                let paths = self.status.ignored_tracked.clone();
                let shown = paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ") + if paths.len() > 3 { ", …" } else { "" };
                el.child(
                    v_flex().mx_2().mb_2().p_2().gap_1().rounded_md().border_1().border_color(theme.warning.opacity(0.5)).text_xs()
                        .child(div().text_color(theme.warning).child(format!("{} {}", paths.len(), t(if paths.len() == 1 { "tracked file is ignored by .gitignore" } else { "tracked files are ignored by .gitignore" }))))
                        .child(div().text_color(theme.muted_foreground).truncate().child(shown))
                        .child(h_flex().justify_end().child(
                            Button::new("untrack-ignored").xsmall().label(t("Stop tracking (keep on disk)"))
                                .on_click(cx.listener(move |t, _, _, cx| t.untrack(paths.clone(), cx))),
                        )),
                )
            })
            .when(self.status.files.is_empty(), |el| el.child(div().mt_2().px_3().text_sm().text_color(theme.muted_foreground).child(t("No changes."))))
            .child(
                // Only the lines on screen are built: thousands of changed files stay fluid.
                div().flex_1().min_h_0().mt_2().relative().child(
                    uniform_list(
                        "git-files",
                        rows.len(),
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .filter_map(|i| rows.get(i).copied())
                                .map(|row| match row {
                                    (None, staged) => this.render_section_header(staged, cx),
                                    (Some(ix), staged) => match this.status.files.get(ix).cloned() {
                                        Some(f) => this.render_file(&f, staged, cx).into_any_element(),
                                        None => div().h(px(24.)).into_any_element(),
                                    },
                                })
                                .collect()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.files_scroll),
                )
                .vertical_scrollbar(&self.files_scroll),
            )
            .child(self.actions.clone())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn commit_message_is_one_line() {
        assert_eq!(super::one_line("```\nfeat(git): untrack ignored files.\n\n- banner\n```"), "feat(git): untrack ignored files");
        assert_eq!(super::one_line("fix: x"), "fix: x");
    }

    use gpui_kit::{AppContext as _, TestAppContext};

    /// Thousands of changed files: only the lines on screen are built.
    #[gpui_kit::test]
    fn many_changes_only_build_what_is_on_screen(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("forgecode-changes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).status().unwrap().success());
        for i in 0..3000 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        cx.update(|cx| {
            gpui_kit::init(cx);
            let base = crate::settings::Settings::default();
            cx.set_global(base.clone());
            cx.set_global(crate::settings::Layers { base, ..Default::default() });
        });
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let root = dir.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| super::GitPanel::new(root.clone(), window, cx));
            *s.borrow_mut() = Some(panel.clone());
            gpui_kit::base::Root::new(panel, window, cx)
        });
        let panel = slot.borrow().clone().unwrap();
        for _ in 0..100 {
            cx.run_until_parked();
            if panel.read_with(cx, |p, _| p.status.files.len()) == 3000 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(panel.read_with(cx, |p, _| p.status.files.len()), 3000);
        panel.update(cx, |p, _| p.built.set(0));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let built = panel.read_with(cx, |p, _| p.built.get());
        assert!(built > 0 && built < 200, "built {built} lines for 3000 files");
        // Short and long names alike: the status letters line up on the panel's right edge.
        let window_w = cx.update(|window, _| window.viewport_size().width);
        let short = cx.debug_bounds("git-letter-f0.txt").unwrap().right();
        let long = cx.debug_bounds("git-letter-f1000.txt").unwrap().right();
        assert_eq!(short, long);
        assert!(window_w - short < gpui_kit::px(16.), "{short:?} vs {window_w:?}");
        // The path runs up to the letter: no room kept for the buttons shown on hover.
        let path_end = cx.debug_bounds("git-path-f0.txt").unwrap().right();
        let letter = cx.debug_bounds("git-letter-f0.txt").unwrap().left();
        assert!(letter - path_end < gpui_kit::px(8.), "{path_end:?} vs {letter:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

