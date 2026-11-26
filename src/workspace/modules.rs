//! Module commands in the interface: icon buttons where each module asks for them, the native
//! Modules menu, and automatic runs (on open, on save, on startup).

use gpui_kit::assets::IconName;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};

use super::*;
use crate::plugins::{Command, Placement};

/// Run a module command by plugin directory name and index: what the native menu sends.
#[derive(Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = forge, no_json)]
pub struct RunModuleCommand {
    pub plugin: String,
    pub index: usize,
}

impl Workspace {
    /// Commands of the modules active in this project that apply to the active file.
    fn module_commands(&self, cx: &App) -> Vec<(PathBuf, usize, Command)> {
        let lsp_id = self.active_buffer().map(|b| b.lang.lsp_id.clone());
        Plugins::global(cx)
            .plugins
            .iter()
            .filter(|p| p.active_in(&self.root))
            .flat_map(|p| p.manifest.commands.iter().enumerate().map(move |(ix, c)| (p.dir.clone(), ix, c.clone())))
            .filter(|(_, _, c)| c.languages.is_empty() || lsp_id.as_deref().is_some_and(|id| c.applies_to(id)))
            .collect()
    }

    /// Icon buttons of the commands placed at `placement`.
    pub(super) fn module_buttons(&self, placement: Placement, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.module_commands(cx)
            .into_iter()
            .filter(|(_, _, c)| c.placed(placement))
            .map(|(dir, ix, command)| {
                let id = SharedString::from(format!("module-{placement:?}-{}-{ix}", dir.display()));
                let icon = crate::icons::module_icon(command.icon.as_deref(), &dir, IconName::Blocks, px(14.));
                let title = SharedString::from(command.title.clone());
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .child(icon)
                    .tooltip(title)
                    .on_click(cx.listener(move |t, _, w, cx| t.run_module_command(&dir, &command, w, cx)))
                    .into_any_element()
            })
            .collect()
    }

    pub(super) fn run_module_by_index(&mut self, action: &RunModuleCommand, window: &mut Window, cx: &mut Context<Self>) {
        let found = Plugins::global(cx)
            .plugins
            .iter()
            .find(|p| p.dir.file_name().is_some_and(|n| n.to_string_lossy() == action.plugin))
            .and_then(|p| Some((p.dir.clone(), p.manifest.commands.get(action.index)?.clone())));
        if let Some((dir, command)) = found {
            self.run_module_command(&dir, &command, window, cx);
        }
    }

    /// Run the commands `trigger` selects for a file of `lsp_id` (or for the project when `None`).
    pub(super) fn run_triggered(&mut self, lsp_id: Option<&str>, trigger: fn(&Command) -> bool, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let hooks: Vec<_> = Plugins::global(cx)
            .commands()
            .filter(|(p, c)| trigger(c) && p.active_in(&root) && lsp_id.is_none_or(|id| c.applies_to(id)))
            .map(|(p, c)| (p.dir.clone(), c.clone()))
            .collect();
        for (dir, command) in hooks {
            self.run_module_command(&dir, &command, window, cx);
        }
    }
}

/// The native Modules menu: every module's menu commands, then module management.
pub fn menu_items(plugins: &Plugins) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = plugins
        .plugins
        .iter()
        .flat_map(|p| {
            let plugin = p.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            p.manifest.commands.iter().enumerate().filter(|(_, c)| c.placed(Placement::Menu)).map(move |(index, c)| {
                MenuItem::action(c.title.clone(), RunModuleCommand { plugin: plugin.clone(), index })
            })
        })
        .collect();
    if !items.is_empty() {
        items.push(MenuItem::separator());
    }
    items.extend([
        MenuItem::action(t("New module…"), NewModule),
        MenuItem::action(t("Reload modules"), ReloadModules),
        MenuItem::action(t("Marketplace"), ToggleSettings),
    ]);
    items
}
