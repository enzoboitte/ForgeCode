//! Choosing a language's interpreter or runtime (Python, PHP, Node) without typing a path: the
//! ones installed on the machine are found, each checked with `--version`, and the choice shows
//! whether it works (spinner, then ✓ with its version or ✕ with the reason).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::i18n::t;
use crate::lsp::install::which;
use crate::settings::Settings;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Runtime {
    Python,
    Php,
    Node,
}

impl Runtime {
    fn programs(self) -> &'static [&'static str] {
        match self {
            Self::Python => &["python3", "python"],
            Self::Php => &["php"],
            Self::Node => &["node"],
        }
    }
}

/// `<exe> --version` reduced to its version (`Python 3.12.1` → `3.12.1`, `v22.1.0` → `22.1.0`,
/// `PHP 8.3.4 (cli)…` → `8.3.4`).
pub fn parse_version(output: &str) -> Option<String> {
    let first = output.lines().find(|l| !l.trim().is_empty())?;
    first
        .split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('.'))
        .map(String::from)
}

/// Run `<exe> --version` (a few seconds at most): its version, or why it doesn't work.
pub fn check(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err(t("File not found").into());
    }
    let mut child = crate::util::command(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", t("Cannot run it")))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(30)),
            _ => {
                let _ = child.kill();
                return Err(t("No answer").into());
            }
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    // Python 2 prints its version on stderr.
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    match (out.status.success(), parse_version(&text)) {
        (true, Some(v)) => Ok(v),
        _ => Err(text.lines().find(|l| !l.trim().is_empty()).unwrap_or("error").chars().take(120).collect()),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// `<dir>/*/<rel>` that exist.
fn each(dir: &Path, rel: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path().join(rel)).filter(|p| p.is_file()).collect();
    found.sort();
    found.reverse(); // newest version first
    found
}

/// Installed runtimes of `kind`, project ones first, without duplicates.
pub fn candidates(kind: Runtime, root: Option<&Path>) -> Vec<PathBuf> {
    let h = home();
    let mut all: Vec<PathBuf> = vec![];
    match kind {
        Runtime::Python => {
            if let Some(root) = root {
                all.extend(crate::language::project_venvs(root).iter().map(|v| crate::language::venv_python(v)));
            }
            all.extend(each(&h.join(".pyenv/versions"), "bin/python"));
            all.extend(each(&h.join(".local/share/uv/python"), "bin/python3"));
            for conda in ["miniconda3", "anaconda3", "miniforge3", "mambaforge"] {
                all.push(h.join(conda).join("bin/python"));
                all.extend(each(&h.join(conda).join("envs"), "bin/python"));
            }
            for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
                all.extend(each_named(Path::new(dir), "python3."));
            }
        }
        Runtime::Php => {
            if let Some(root) = root {
                all.extend(project_programs(kind, root));
            }
            all.extend(each(Path::new("/opt/homebrew/opt"), "bin/php").into_iter().filter(|p| p.to_string_lossy().contains("/php")));
            all.extend(each(&h.join("Library/Application Support/Herd/bin"), "php"));
            all.extend(each_named(&h.join("Library/Application Support/Herd/bin"), "php"));
            all.extend(each(Path::new("/Applications/MAMP/bin/php"), "bin/php"));
        }
        Runtime::Node => {
            if let Some(root) = root {
                all.extend(project_programs(kind, root));
            }
            all.extend(crate::lsp::install::node_bins().into_iter().map(|d| d.join("node")));
        }
    }
    all.extend(kind.programs().iter().filter_map(|p| which(p)));
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        all.extend(kind.programs().iter().map(|p| Path::new(dir).join(p)));
    }
    let mut seen = std::collections::HashSet::new();
    all.into_iter()
        .filter(|p| p.is_file())
        // Same binary through a symlink counts once (Homebrew's `python3` → `python3.12`), but a
        // virtualenv's `python` links to its base interpreter and is still its own environment.
        .filter(|p| {
            let in_venv = p.parent().and_then(Path::parent).is_some_and(|env| env.join("pyvenv.cfg").is_file());
            seen.insert(if in_venv { p.clone() } else { p.canonicalize().unwrap_or_else(|_| p.clone()) })
        })
        .collect()
}

/// Files of `dir` named `<prefix>…` (`python3.12`, `php83`), excluding config helpers.
fn each_named(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            name.starts_with(prefix) && !name.contains("config") && !name.contains('-') && p.is_file()
        })
        .collect();
    found.sort();
    found.reverse();
    found
}

/// The executable of `kind` a chosen folder holds: a virtualenv (`.venv` → `bin/python`), a folder
/// containing one, an install folder (`…/node-v22` → `bin/node`), or a `bin` folder itself.
pub fn resolve_folder(kind: Runtime, dir: &Path) -> Option<PathBuf> {
    if dir.is_file() {
        return Some(dir.to_path_buf());
    }
    let names: Vec<String> = kind.programs().iter().map(|p| format!("{p}{}", std::env::consts::EXE_SUFFIX)).collect();
    let in_dir = |d: &Path| -> Option<PathBuf> {
        ["bin", "Scripts", ""].iter().flat_map(|sub| names.iter().map(move |n| d.join(sub).join(n))).find(|p| p.is_file())
    };
    if let Some(found) = in_dir(dir) {
        return Some(found);
    }
    // A project or folder containing environments / installs: the first one found.
    match kind {
        Runtime::Python => crate::language::project_venvs(dir).first().map(|v| crate::language::venv_python(v)),
        _ => project_programs(kind, dir).into_iter().next(),
    }
}

/// Executables of `kind` inside the project (a `bin/node` or `bin/php` shipped with it).
fn project_programs(kind: Runtime, root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, names: &[&str], depth: usize, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if depth > 1 && !crate::language::SKIPPED_DIRS.contains(&name.as_str()) && !path.join("pyvenv.cfg").is_file() {
                    walk(&path, names, depth - 1, out);
                }
            } else if names.contains(&name.trim_end_matches(".exe")) && path.parent().is_some_and(|p| p.ends_with("bin")) {
                out.push(path);
            }
        }
    }
    let mut out = vec![];
    walk(root, kind.programs(), 4, &mut out);
    out
}

/// What "Automatic" resolves to.
fn automatic(kind: Runtime, root: Option<&Path>) -> Option<PathBuf> {
    match kind {
        Runtime::Python => root.and_then(crate::language::project_venv).map(|v| crate::language::venv_python(&v)).or_else(|| kind.programs().iter().find_map(|p| which(p))),
        _ => kind.programs().iter().find_map(|p| which(p)),
    }
}

#[derive(Clone)]
enum Status {
    Checking,
    Ok(String),
    Failed(String),
}

pub struct RuntimeField {
    key: &'static str,
    kind: Runtime,
    root: Option<PathBuf>,
    /// Found runtimes and their check result.
    found: Vec<(PathBuf, Status)>,
    /// The chosen (or automatic) one's check.
    current: Option<(PathBuf, Status)>,
    _task: Option<Task<()>>,
}

struct Fields(HashMap<&'static str, Entity<RuntimeField>>);
impl Global for Fields {}

/// The field of language `key`, kept across settings page renders.
pub fn field(key: &'static str, kind: Runtime, cx: &mut App) -> AnyElement {
    if !cx.has_global::<Fields>() {
        cx.set_global(Fields(HashMap::new()));
    }
    if let Some(f) = cx.global::<Fields>().0.get(key) {
        return f.clone().into_any_element();
    }
    let root = super::current_root(cx);
    let entity = cx.new(|cx| {
        let mut f = RuntimeField { key, kind, root, found: vec![], current: None, _task: None };
        f.rescan(cx);
        f
    });
    cx.global_mut::<Fields>().0.insert(key, entity.clone());
    entity.into_any_element()
}

impl RuntimeField {
    fn configured(&self, cx: &App) -> String {
        Settings::get(cx).lang(self.key).runtime.clone()
    }

    /// Find the installed runtimes and check them, and the current choice, in the background.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        // The project may have changed since the field was created.
        self.root = super::current_root(cx);
        let (kind, root, configured) = (self.kind, self.root.clone(), self.configured(cx));
        let current = if configured.is_empty() { automatic(kind, root.as_deref()) } else { Some(PathBuf::from(&configured)) };
        self.current = Some((current.clone().unwrap_or_default(), Status::Checking));
        cx.notify();
        self._task = Some(cx.spawn(async move |this, cx| {
            let (current_status, found) = cx
                .background_spawn(async move {
                    let current_status = match &current {
                        Some(p) => check(p).map_or_else(Status::Failed, Status::Ok),
                        None => Status::Failed(t("Not found on this machine").into()),
                    };
                    let found: Vec<(PathBuf, Status)> =
                        candidates(kind, root.as_deref()).into_iter().map(|p| { let s = check(&p).map_or_else(Status::Failed, Status::Ok); (p, s) }).collect();
                    (current_status, found)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some((_, status)) = &mut this.current {
                    *status = current_status;
                }
                this.found = found;
                cx.notify();
            });
        }));
    }

    fn choose(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let key = self.key;
        let value = path.map(|p| p.display().to_string()).unwrap_or_default();
        Settings::update(cx, |s| s.languages.entry(key.to_string()).or_default().runtime = value);
        self.rescan(cx);
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        // A file, or a folder (`.venv`, a Node install…) in which the executable is found.
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: true, multiple: false, prompt: None });
        let kind = self.kind;
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| match resolve_folder(kind, &path) {
                        Some(exe) => this.choose(Some(exe), cx),
                        None => {
                            this.current = Some((path, Status::Failed(t("No interpreter in this folder").into())));
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
    }

    fn label(&self, path: &Path) -> String {
        match self.root.as_deref().and_then(|r| path.strip_prefix(r).ok()) {
            Some(rel) => rel.display().to_string(),
            None => path.display().to_string().replace(&home().display().to_string(), "~"),
        }
    }
}

impl Render for RuntimeField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let automatic_mode = self.configured(cx).is_empty();
        let (path, status) = self.current.clone().unwrap_or((PathBuf::new(), Status::Checking));
        let title = if automatic_mode { format!("{} - {}", t("Automatic"), self.label(&path)) } else { self.label(&path) };
        let found = self.found.clone();
        let view = cx.entity().downgrade();
        let labels: Vec<String> = found.iter().map(|(p, _)| self.label(p)).collect();
        // The verdict fades in once the check is done, so the change is noticed.
        let badge = match &status {
            Status::Checking => h_flex().gap_1().text_color(theme.muted_foreground).child(Spinner::new().xsmall()).child(t("Checking…")).into_any_element(),
            Status::Ok(v) => h_flex()
                .id("runtime-ok")
                .gap_1()
                .px_1p5()
                .rounded_md()
                .bg(theme.green.opacity(0.12))
                .text_color(theme.green)
                .child(IconName::CircleCheck)
                .child(v.clone())
                .with_animation("runtime-ok-in", Animation::new(Duration::from_millis(350)).with_easing(ease_out_quint()), |el, d| el.opacity(d))
                .into_any_element(),
            Status::Failed(e) => h_flex()
                .id("runtime-ko")
                .gap_1()
                .px_1p5()
                .rounded_md()
                .bg(theme.danger.opacity(0.12))
                .text_color(theme.danger)
                .child(IconName::CircleX)
                .child(div().truncate().child(e.clone()))
                .with_animation("runtime-ko-in", Animation::new(Duration::from_millis(350)).with_easing(ease_out_quint()), |el, d| el.opacity(d))
                .into_any_element(),
        };
        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("runtime-{}", self.key))).small().outline().icon(IconName::ChevronsUpDown).label(title).flex_1().dropdown_menu(move |mut menu, _, _| {
                            let auto_view = view.clone();
                            menu = menu.item(PopupMenuItem::new(t("Automatic (project environment, then PATH)")).checked(automatic_mode).on_click(move |_, _, cx| {
                                let _ = auto_view.update(cx, |f, cx| f.choose(None, cx));
                            }));
                            if !found.is_empty() {
                                menu = menu.separator();
                            }
                            for ((p, status), label) in found.iter().cloned().zip(labels.clone()) {
                                let view = view.clone();
                                let text = match &status {
                                    Status::Ok(v) => format!("{v}   {label}"),
                                    Status::Failed(_) => format!("✕   {label}"),
                                    Status::Checking => label,
                                };
                                let chosen = !automatic_mode && p == path;
                                menu = menu.item(PopupMenuItem::new(text).checked(chosen).on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |f, cx| f.choose(Some(p.clone()), cx));
                                }));
                            }
                            let browse_view = view.clone();
                            menu.separator().item(PopupMenuItem::new(t("Browse…")).icon(IconName::FolderOpen).on_click(move |_, _, cx| {
                                let _ = browse_view.update(cx, |f, cx| f.browse(cx));
                            }))
                        }),
                    )
                    .child(Button::new(SharedString::from(format!("runtime-rescan-{}", self.key))).small().ghost().icon(IconName::RefreshCw).tooltip(t("Search again")).on_click(cx.listener(|f, _, _, cx| f.rescan(cx)))),
            )
            .child(div().text_xs().child(badge))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn reads_versions() {
        assert_eq!(parse_version("Python 3.12.1\n").as_deref(), Some("3.12.1"));
        assert_eq!(parse_version("v22.11.0\n").as_deref(), Some("22.11.0"));
        assert_eq!(parse_version("PHP 8.3.4 (cli) (built: Mar 12 2024)\nCopyright").as_deref(), Some("8.3.4"));
        assert_eq!(parse_version("command not found").as_deref(), None);
    }

    #[test]
    fn finds_the_executable_in_a_chosen_folder() {
        use super::{Runtime, resolve_folder};
        let root = std::env::temp_dir().join(format!("forge-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let venv = root.join("backend/.venv");
        let python = crate::language::venv_python(&venv);
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, "").unwrap();
        std::fs::write(venv.join("pyvenv.cfg"), "").unwrap();
        assert_eq!(resolve_folder(Runtime::Python, &venv), Some(python.clone()), "the venv itself");
        assert_eq!(resolve_folder(Runtime::Python, &root), Some(python.clone()), "a project holding it");
        let node = root.join("tools/node-v22/bin/node");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(&node, "").unwrap();
        assert_eq!(resolve_folder(Runtime::Node, &root.join("tools/node-v22")), Some(node.clone()));
        assert_eq!(resolve_folder(Runtime::Node, &root), Some(node));
        assert_eq!(resolve_folder(Runtime::Php, &root), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checks_a_real_interpreter() {
        assert!(super::check(std::path::Path::new("/usr/bin/python3")).is_ok() || !std::path::Path::new("/usr/bin/python3").exists());
        assert!(super::check(std::path::Path::new("/no/such/python")).is_err());
    }

    /// Lists what this machine has: `cargo test -- --ignored lists_installed --nocapture`.
    #[test]
    #[ignore]
    fn lists_installed_runtimes() {
        for kind in [super::Runtime::Python, super::Runtime::Php, super::Runtime::Node] {
            let root = std::env::var("FORGE_ROOT").ok().map(std::path::PathBuf::from);
            for p in super::candidates(kind, root.as_deref()) {
                eprintln!("FOUND {kind:?} {} -> {:?}", p.display(), super::check(&p));
            }
        }
    }
}
