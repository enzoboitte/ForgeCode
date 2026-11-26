//! Project setup advice, as notifications at the bottom right with one action button: what a
//! language needs from the user before errors and completions can be right (a Python
//! environment, installed dependencies, a compilation database, a missing toolchain…). Each
//! piece of advice is shown once per project and session.

use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Sizable as _, WindowExt as _};

use super::*;

#[derive(Clone, Debug, PartialEq)]
pub enum Fix {
    /// Run a command in the Output panel, from the project root.
    Run { title: String, command: String },
    /// Pick the project's Python interpreter among these.
    ChoosePython(Vec<PathBuf>),
    OpenUrl(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Advice {
    /// Shown once per project and session.
    pub key: String,
    pub message: String,
    pub fix: Option<(String, Fix)>,
}

/// What `lsp_id` files in the project at `root` need. `has` tells whether a program is installed.
pub fn advise(root: &Path, lsp_id: &str, server: Option<&str>, has: &dyn Fn(&str) -> bool) -> Vec<Advice> {
    let exists = |f: &str| root.join(f).exists();
    let mut out = vec![];
    let mut push = |key: &str, message: String, fix: Option<(String, Fix)>| out.push(Advice { key: key.into(), message, fix });
    // Any language: its server is missing and ForgeCode can't install it alone.
    if let Some(server) = server.filter(|s| !has(s) && !crate::lsp::install::can_install(s)) {
        let fix = crate::lsp::install::manual_install(server)
            .map(|command| (command.to_string(), Fix::Run { title: server.to_string(), command: command.to_string() }));
        push(&format!("server-{server}"), format!("{} {server} {}", t("The language server"), t("is not installed: no errors or completions for these files")), fix);
    }
    match lsp_id {
        "python" => {
            let venvs = language::project_venvs(root);
            let deps = ["requirements.txt", "pyproject.toml", "Pipfile", "setup.py"].into_iter().find(|f| exists(f));
            if venvs.is_empty() {
                if let Some(deps) = deps {
                    let install = if exists("uv.lock") && has("uv") {
                        "uv sync".to_string()
                    } else {
                        let py = if cfg!(windows) { "python" } else { "python3" };
                        let pip = if cfg!(windows) { ".venv\\Scripts\\pip" } else { ".venv/bin/pip" };
                        let what = if deps == "requirements.txt" { "-r requirements.txt" } else { "-e ." };
                        format!("{py} -m venv .venv && {pip} install {what}")
                    };
                    push(
                        "python-venv",
                        format!("{} ({deps})", t("No Python virtual environment: imports of the project's libraries can't be checked")),
                        Some((t("Create .venv and install").into(), Fix::Run { title: t("Python environment").into(), command: install })),
                    );
                }
            } else if venvs.len() > 1 {
                let used = venvs[0].file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                push(
                    "python-venvs",
                    format!("{} {used}", t("Several Python environments in this project; ForgeCode uses")),
                    Some((t("Choose…").into(), Fix::ChoosePython(venvs.iter().map(|v| language::venv_python(v)).collect()))),
                );
            }
        }
        "javascript" | "javascriptreact" | "typescript" | "typescriptreact" | "svelte" if exists("package.json") && !exists("node_modules") => {
            let command = if exists("pnpm-lock.yaml") {
                "pnpm install"
            } else if exists("yarn.lock") {
                "yarn install"
            } else if exists("bun.lockb") || exists("bun.lock") {
                "bun install"
            } else {
                "npm install"
            };
            push(
                "node-modules",
                t("Dependencies are not installed (node_modules): imports can't be resolved").into(),
                Some((command.into(), Fix::Run { title: t("Install dependencies").into(), command: command.into() })),
            );
        }
        "php" if exists("composer.json") && !exists("vendor") => push(
            "composer",
            t("Composer dependencies are not installed (vendor)").into(),
            Some(("composer install".into(), Fix::Run { title: t("Install dependencies").into(), command: "composer install".into() })),
        ),
        "rust" if !has("cargo") => push(
            "rust-toolchain",
            t("Rust is not installed: no errors or completions for Rust").into(),
            Some(("rustup.rs".into(), Fix::OpenUrl("https://rustup.rs".into()))),
        ),
        "go" if !has("go") => push(
            "go-toolchain",
            t("Go is not installed: no errors or completions for Go").into(),
            Some(("go.dev/dl".into(), Fix::OpenUrl("https://go.dev/dl/".into()))),
        ),
        "c" | "cpp" => {
            let database = exists("compile_commands.json") || exists("build/compile_commands.json");
            if !database && exists("CMakeLists.txt") && has("cmake") {
                push(
                    "compile-commands",
                    t("Generate compile_commands.json so errors use the project's real include paths and flags").into(),
                    Some((
                        t("Generate").into(),
                        Fix::Run {
                            title: "CMake".into(),
                            command: "cmake -S . -B build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON && ln -sf build/compile_commands.json compile_commands.json".into(),
                        },
                    )),
                );
            } else if !database && (exists("Makefile") || exists("makefile")) && has("bear") {
                push(
                    "compile-commands",
                    t("Generate compile_commands.json so errors use the project's real include paths and flags").into(),
                    Some((t("Generate").into(), Fix::Run { title: "bear".into(), command: "bear -- make".into() })),
                );
            }
        }
        _ => {}
    }
    out
}

impl Workspace {
    /// Show what `lsp_id` files of this project still need, once each.
    pub(super) fn advise(&mut self, lsp_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let has = |p: &str| crate::lsp::install::which(p).is_some();
        let server = self.active_buffer().and_then(|b| b.lang.server.first().cloned());
        for advice in advise(&self.root, lsp_id, server.as_deref(), &has) {
            if !self.advised.insert(advice.key.clone()) {
                continue;
            }
            let ws = cx.entity().downgrade();
            let mut note = Notification::warning(advice.message.clone()).placement(Anchor::BottomRight);
            if let Some((label, fix)) = advice.fix.clone() {
                note = note.action(move |_, _, _| {
                    let (ws, fix) = (ws.clone(), fix.clone());
                    Button::new("advice-fix").small().primary().label(label.clone()).on_click(move |_, window, cx| {
                        let fix = fix.clone();
                        let _ = ws.update(cx, |w, cx| w.apply_fix(fix, window, cx));
                    })
                });
            }
            window.push_notification(note, cx);
        }
    }

    fn apply_fix(&mut self, fix: Fix, window: &mut Window, cx: &mut Context<Self>) {
        match fix {
            Fix::Run { title, command } => {
                let root = self.root.clone();
                let env = self.command_env(cx);
                self.output.update(cx, |o, cx| o.run(&title, &command, &root, env, cx));
                self.show_bottom_tab(BottomTab::Output, window, cx);
            }
            Fix::OpenUrl(url) => cx.open_url(&url),
            Fix::ChoosePython(interpreters) => {
                let items = interpreters
                    .iter()
                    .map(|p| quick_open::PickerItem {
                        label: p.strip_prefix(&self.root).unwrap_or(p).display().to_string(),
                        detail: String::new(),
                        keys: String::new(),
                        icon: IconName::Terminal,
                        tint: None,
                    })
                    .collect();
                quick_open::Picker::open(items, t("Python interpreter").into(), window, cx, move |ix, _, cx| {
                    let path = interpreters[ix].display().to_string();
                    Settings::update(cx, |s| s.languages.entry("python".into()).or_default().runtime = path);
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Fix, advise};

    #[test]
    fn advises_what_each_language_needs() {
        let root = std::env::temp_dir().join(format!("forge-advice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let all = |_: &str| true;
        assert!(advise(&root, "python", None, &all).is_empty(), "no dependencies, no advice");
        std::fs::write(root.join("requirements.txt"), "flask\n").unwrap();
        let a = advise(&root, "python", None, &all);
        assert!(matches!(&a[0].fix, Some((_, Fix::Run { command, .. })) if command.contains("venv .venv") && command.contains("-r requirements.txt")));
        for venv in [".venv", "myenv"] {
            let python = crate::language::venv_python(&root.join(venv));
            std::fs::create_dir_all(python.parent().unwrap()).unwrap();
            std::fs::write(&python, "").unwrap();
            std::fs::write(root.join(venv).join("pyvenv.cfg"), "").unwrap();
        }
        let a = advise(&root, "python", None, &all);
        assert!(matches!(&a[0].fix, Some((_, Fix::ChoosePython(p))) if p.len() == 2), "{a:?}");
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("yarn.lock"), "").unwrap();
        assert!(matches!(&advise(&root, "typescript", None, &all)[0].fix, Some((_, Fix::Run { command, .. })) if command == "yarn install"));
        assert!(advise(&root, "rust", None, &all).is_empty());
        assert_eq!(advise(&root, "rust", None, &|_: &str| false)[0].key, "rust-toolchain");
        let ruby = advise(&root, "ruby", Some("ruby-lsp"), &|_: &str| false);
        assert!(matches!(&ruby[0].fix, Some((_, Fix::Run { command, .. })) if command == "gem install ruby-lsp"));
        assert!(advise(&root, "python", Some("pyright-langserver"), &|_: &str| false).iter().all(|a| !a.key.starts_with("server-")), "installed by ForgeCode itself");
        std::fs::remove_dir_all(root).unwrap();
    }
}
