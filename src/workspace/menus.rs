//! The application menus: native menu bar on macOS, drawn in the title bar on Windows and Linux.

use gpui_kit::component::GlobalState;
use gpui_kit::component::input::{Copy, Cut, GoToDefinition, Paste, Redo, SelectAll, Undo};
use gpui_kit::{App, Menu, MenuItem, OsAction};

use super::*;
use crate::i18n::t;

fn menu(name: &'static str, items: Vec<MenuItem>) -> Menu {
    Menu { name: t(name).into(), items, disabled: false }
}

fn item(name: &'static str, action: impl gpui_kit::Action) -> MenuItem {
    MenuItem::action(t(name), action)
}

pub fn app_menus(plugins: &Plugins) -> Vec<Menu> {
    vec![
        menu("ForgeCode", vec![item("Settings", ToggleSettings), item("Toggle theme", ToggleTheme), MenuItem::separator(), item("Quit ForgeCode", Quit)]),
        menu("File", vec![
            item("Open folder", OpenFolder),
            item("New project…", NewProject),
            item("Go to file", QuickOpenFile),
            MenuItem::separator(),
            item("Save", Save),
            item("Close tab", CloseTab),
            MenuItem::separator(),
            item("AI instructions (.forgecode)", OpenProjectInstructions),
            item("Project settings (.forgecode)", OpenProjectSettings),
        ]),
        menu("Edit", vec![
            item("Undo", Undo),
            item("Redo", Redo),
            MenuItem::separator(),
            MenuItem::os_action(t("Cut"), Cut, OsAction::Cut),
            MenuItem::os_action(t("Copy"), Copy, OsAction::Copy),
            MenuItem::os_action(t("Paste"), Paste, OsAction::Paste),
            MenuItem::os_action(t("Select all"), SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            item("Toggle line comment", ToggleLineComment),
            item("Search in project", ShowSearch),
            item("Format document", FormatDocument),
            item("Go to definition", GoToDefinition),
        ]),
        menu("Selection", vec![
            item("Select next occurrence", SelectNextOccurrence),
            item("Select all occurrences", SelectAllOccurrences),
            MenuItem::separator(),
            item("Move line up", MoveLineUp),
            item("Move line down", MoveLineDown),
            item("Copy line up", CopyLineUp),
            item("Copy line down", CopyLineDown),
            item("Delete line", DeleteLine),
            MenuItem::separator(),
            item("Insert line below", InsertLineBelow),
            item("Insert line above", InsertLineAbove),
        ]),
        menu("View", vec![
            item("Command palette", CommandPalette),
            MenuItem::separator(),
            item("Explorer", ShowExplorer),
            item("Search", ShowSearch),
            item("Source Control", ShowGit),
            item("History graph", ShowGitGraph),
            item("Problems", ShowProblems),
            MenuItem::separator(),
            item("Toggle sidebar", ToggleLeft),
            item("Toggle panel", ToggleBottom),
            item("Toggle chat", ToggleRight),
            item("Split right", SplitRight),
        ]),
        menu("Run", vec![
            item("Run", RunFile),
            item("Debug", StartDebug),
            item("Stop debugging", StopDebug),
            MenuItem::separator(),
            item("Step over", StepOver),
            item("Step into", StepIn),
            item("Step out", StepOut),
            item("Toggle breakpoint", ToggleBreakpoint),
        ]),
        menu("Terminal", vec![item("Terminal", ToggleTerminal), item("New terminal", NewTerminal)]),
        menu("AI", vec![item("Ask the agent", FocusChat), item("Add to chat", AddSelectionToChat)]),
        Menu { name: t("Modules").into(), items: super::modules::menu_items(plugins), disabled: false },
    ]
}

/// Install the menus: the OS menu bar on macOS, the in-window bar elsewhere reads them back.
/// Called again after modules are reloaded, so the Modules menu lists their commands.
pub fn install(cx: &mut App) {
    let plugins = Plugins::global(cx).clone();
    GlobalState::global_mut(cx).set_app_menus(app_menus(&plugins).into_iter().map(Menu::owned).collect());
    cx.set_menus(app_menus(&plugins));
}
