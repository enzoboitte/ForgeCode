//! Plugin ("module") system.
//!
//! A plugin is a directory in `<config>/forgecode/plugins/<name>/` holding a `plugin.toml`.
//! It can contribute, without any compilation step and in any language:
//! - `[[commands]]`: shell commands shown in the Modules menu, optionally run on save;
//! - `[[tools]]`: tools the AI agent can call (the agent sees their JSON schema);
//! - `[[languages]]`: new file extensions with highlighter, language server and run command;
//! - `themes`: a directory of theme JSON files.
//!
//! Commands talk back to the IDE by printing lines `@forge {json}` on stdout, see [`IdeAction`].

pub mod agent_runtime;
pub mod marketplace;
pub mod protocol;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::Global;
use serde::Deserialize;

use crate::language::LanguageInfo;
pub use protocol::IdeAction;

/// Where a command's button appears. Every command is also in the palette.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// The Modules menu (default).
    Menu,
    /// An icon in the title bar, top right.
    TitleBar,
    /// An icon in the toolbar of the file being edited.
    Editor,
    /// An icon in the status bar, bottom right.
    StatusBar,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Command {
    pub title: String,
    pub run: String,
    /// A Lucide icon name (`play`, `bug`, `git-branch`…) or an image file in the module.
    #[serde(default)]
    pub icon: Option<String>,
    /// Where its button goes; empty = the Modules menu.
    #[serde(default)]
    pub placement: Vec<Placement>,
    /// Restrict to these LSP language ids; empty = every file.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Run after saving a file it applies to.
    #[serde(default)]
    pub on_save: bool,
    /// Run when a file it applies to is opened.
    #[serde(default)]
    pub on_open: bool,
    /// Run once when a project the module is active in is opened.
    #[serde(default)]
    pub on_startup: bool,
}

/// When a module is active in a project.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Activation {
    /// Active only when one of these files exists at the project root (`package.json`,
    /// `*.sql`…); empty = every project.
    #[serde(default)]
    pub workspace_contains: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub run: String,
    /// Parameter name -> description. Values reach the command as `FORGE_ARG_<name>` and `FORGE_ARGS` (JSON).
    #[serde(default)]
    pub parameters: HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Language {
    pub extensions: Vec<String>,
    pub highlight: String,
    #[serde(default)]
    pub lsp_id: Option<String>,
    #[serde(default)]
    pub server: Vec<String>,
    #[serde(default)]
    pub run: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// Shown in the marketplace: `Linters`, `Languages`, `Themes`, `AI`…
    #[serde(default)]
    pub categories: Vec<String>,
    /// A Lucide icon name or an image file in the module.
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub activation: Activation,
    #[serde(default)]
    pub themes: Option<String>,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub tools: Vec<Tool>,
    #[serde(default)]
    pub languages: Vec<Language>,
    /// A coding agent this module brings (Hermes): ForgeCode installs it and drives it over ACP.
    #[serde(default)]
    pub agent: Option<AgentSpec>,
    /// The databases panel (see `workspace::databases`).
    #[serde(default)]
    pub database: Option<DatabaseSpec>,
    /// Python packages the module needs, installed with uv into `runtime/venv`.
    #[serde(default)]
    pub python: Option<PythonSpec>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct DatabaseSpec {
    /// The engine script in the module (`db.py`), run as `serve` and `mcp`.
    pub engine: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PythonSpec {
    pub version: String,
    #[serde(default)]
    pub packages: Vec<String>,
}

/// An external agent speaking the Agent Client Protocol.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct AgentSpec {
    pub name: String,
    /// Python package (pip requirement) ForgeCode installs with uv into the module's `runtime/`.
    #[serde(default)]
    pub package: String,
    /// Or a source archive, unpacked into `runtime/src` then set up by `setup` (for agents that
    /// only run from a source checkout, like Hermes).
    #[serde(default)]
    pub source: Option<String>,
    /// Commands run in the checkout once unpacked, in order. `{uv}` is uv, `{python}` the
    /// `python` version, `{src}` the checkout.
    #[serde(default)]
    pub setup: Vec<Vec<String>>,
    /// Environment of the setup commands (same placeholders).
    #[serde(default)]
    pub setup_env: std::collections::BTreeMap<String, String>,
    /// Optional, run in the checkout after setup: prints the agent's runtime environment as a JSON object.
    #[serde(default)]
    pub environment: Vec<String>,
    /// Python version uv provides for it.
    #[serde(default = "default_python")]
    pub python: String,
    /// Command starting the ACP server. Its program is looked up on PATH (the user's install),
    /// then in `runtime/bin`; with a `source`, a relative path (`.venv/bin/hermes`) is in the checkout.
    pub command: Vec<String>,
    /// Shown while installing.
    #[serde(default)]
    pub install_note: String,
}

fn default_python() -> String {
    "3.12".into()
}

#[derive(Clone, Debug)]
pub struct Plugin {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Default)]
pub struct Plugins {
    pub plugins: Vec<Plugin>,
    /// Manifests that failed to parse: (path, error).
    pub errors: Vec<(PathBuf, String)>,
}

impl Global for Plugins {}

impl Plugins {
    /// The installed databases module, if any.
    pub fn database(&self) -> Option<(&Plugin, &DatabaseSpec)> {
        self.plugins.iter().find_map(|p| p.manifest.database.as_ref().map(|d| (p, d)))
    }

    /// The installed module bringing an agent, if any.
    pub fn agent(&self) -> Option<(&Plugin, &AgentSpec)> {
        self.plugins.iter().find_map(|p| p.manifest.agent.as_ref().map(|a| (p, a)))
    }
}

pub fn plugins_dir() -> PathBuf {
    let dir = crate::settings::config_dir().join("plugins");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

impl Plugins {
    pub fn global(cx: &gpui_kit::App) -> &Self {
        cx.global()
    }

    pub fn load() -> Self {
        Self::load_from(&plugins_dir())
    }

    /// Every module directory under `root`.
    pub fn load_from(root: &Path) -> Self {
        let mut out = Self::default();
        let Ok(entries) = std::fs::read_dir(root) else {
            return out;
        };
        let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            let path = dir.join("plugin.toml");
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            match toml::from_str::<Manifest>(&text) {
                Ok(manifest) => out.plugins.push(Plugin { dir, manifest }),
                Err(e) => out.errors.push((path, e.to_string())),
            }
        }
        out
    }

    pub fn commands(&self) -> impl Iterator<Item = (&Plugin, &Command)> {
        self.plugins.iter().flat_map(|p| p.manifest.commands.iter().map(move |c| (p, c)))
    }

    pub fn tools(&self) -> impl Iterator<Item = (&Plugin, &Tool)> {
        self.plugins.iter().flat_map(|p| p.manifest.tools.iter().map(move |t| (p, t)))
    }

    pub fn theme_dirs(&self) -> Vec<PathBuf> {
        self.plugins
            .iter()
            .filter_map(|p| p.manifest.themes.as_ref().map(|t| p.dir.join(t)))
            .collect()
    }

    pub fn language_for_extension(&self, ext: &str) -> Option<LanguageInfo> {
        self.plugins.iter().flat_map(|p| &p.manifest.languages).find_map(|l| {
            l.extensions.iter().any(|e| e == ext).then(|| LanguageInfo {
                highlight: l.highlight.clone(),
                lsp_id: l.lsp_id.clone().unwrap_or_else(|| l.highlight.clone()),
                server: l.server.clone(),
                run: l.run.clone(),
            })
        })
    }
}

/// Environment for a module's command: its directory goes first on PATH so scripts run by name.
pub fn command_env(plugin_dir: &Path) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), crate::util::path_with([plugin_dir.to_path_buf()])),
        ("FORGE_PLUGIN_DIR".into(), plugin_dir.display().to_string()),
    ]
}

impl Command {
    pub fn applies_to(&self, lsp_id: &str) -> bool {
        self.languages.is_empty() || self.languages.iter().any(|l| l == lsp_id)
    }

    pub fn placed(&self, placement: Placement) -> bool {
        if self.placement.is_empty() { placement == Placement::Menu } else { self.placement.contains(&placement) }
    }
}

impl Plugin {
    /// Whether the module's `activation` matches the project at `root`.
    pub fn active_in(&self, root: &Path) -> bool {
        let patterns = &self.manifest.activation.workspace_contains;
        patterns.is_empty() || patterns.iter().any(|p| root_contains(root, p))
    }
}

/// A file at `root` named `pattern`, where `*` matches any run of characters (`*.sql`).
fn root_contains(root: &Path, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return root.join(pattern).exists();
    }
    let (prefix, suffix) = pattern.split_once('*').unwrap_or((pattern, ""));
    std::fs::read_dir(root).into_iter().flatten().flatten().any(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        name.len() >= prefix.len() + suffix.len() && name.starts_with(prefix) && name.ends_with(suffix)
    })
}

/// Create a documented example plugin and return its directory.
pub fn scaffold(name: &str) -> std::io::Result<PathBuf> {
    let dir = plugins_dir().join(name);
    std::fs::create_dir_all(dir.join("themes"))?;
    let manifest = include_str!("template.toml").replace("{{name}}", name);
    write_new(&dir.join("plugin.toml"), &manifest)?;
    write_new(&dir.join("hello.sh"), include_str!("template_hello.sh"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("hello.sh"), std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(dir)
}

/// Never overwrite a file the user may have edited.
fn write_new(path: &Path, content: &str) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_manifest_parses() {
        let m: Manifest = toml::from_str(&include_str!("template.toml").replace("{{name}}", "x")).unwrap();
        assert!(!m.commands.is_empty());
        assert!(!m.tools.is_empty());
        assert_eq!(m.languages[0].extensions, vec!["zig"]);
    }

    #[test]
    fn bundled_hermes_manifest_parses() {
        let m: Manifest = toml::from_str(include_str!("../../extensions/hermes/plugin.toml")).unwrap();
        let agent = m.agent.unwrap();
        assert_eq!(agent.python, "3.11");
        assert_eq!(agent.setup.len(), 2);
        assert_eq!(agent.setup_env["UV_PROJECT_ENVIRONMENT"], "{src}/venv");
        assert_eq!(agent.command, vec!["venv/bin/hermes", "acp"]);
    }

    #[test]
    fn placement_and_activation() {
        let m: Manifest = toml::from_str(
            r#"
            name = "x"
            icon = "shield"
            [activation]
            workspace_contains = ["Cargo.toml", "*.sql"]
            [[commands]]
            title = "Check"
            run = "check"
            icon = "bug"
            placement = ["title_bar", "editor"]
            [[commands]]
            title = "Plain"
            run = "plain"
            "#,
        )
        .unwrap();
        assert!(m.commands[0].placed(Placement::TitleBar) && !m.commands[0].placed(Placement::Menu));
        assert!(m.commands[1].placed(Placement::Menu) && !m.commands[1].placed(Placement::StatusBar));
        let root = std::env::temp_dir().join(format!("forgecode-activation-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let plugin = Plugin { dir: root.clone(), manifest: m };
        assert!(!plugin.active_in(&root));
        std::fs::write(root.join("dump.sql"), "").unwrap();
        assert!(plugin.active_in(&root), "matched by *.sql");
        std::fs::remove_dir_all(root).unwrap();
    }
}
