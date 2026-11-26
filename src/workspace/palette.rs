//! Command palette (⌘⇧P): every IDE action and every module command, searchable.

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::*;

use super::quick_open::{Picker, PickerItem};
use super::*;
use crate::i18n::t;

enum Entry {
    Action(Box<dyn Action>),
    Module(PathBuf, crate::plugins::Command),
}

/// (title, shortcut, icon, action)
fn actions() -> Vec<(&'static str, &'static str, IconName, Box<dyn Action>)> {
    vec![
        ("Open folder", "⌘O", IconName::FolderOpen, Box::new(OpenFolder)),
        ("New project…", "", IconName::FolderPlus, Box::new(NewProject)),
        ("Go to file…", "⌘P", IconName::FileSearch, Box::new(QuickOpenFile)),
        ("Search in project", "⌘⇧F", IconName::Search, Box::new(ShowSearch)),
        ("Save", "⌘S", IconName::Save, Box::new(Save)),
        ("Format document", "⇧⌥F", IconName::TextAlignStart, Box::new(FormatDocument)),
        ("Go to definition", "F12", IconName::SquareArrowOutUpRight, Box::new(gpui_kit::component::input::GoToDefinition)),
        ("Run", "⌘R", IconName::Play, Box::new(RunFile)),
        ("Debug", "F5", IconName::Bug, Box::new(StartDebug)),
        ("Toggle breakpoint", "F9", IconName::CircleDot, Box::new(ToggleBreakpoint)),
        ("Split right", "⌘\\", IconName::Columns2, Box::new(SplitRight)),
        ("Explorer", "⇧⌘E", IconName::Files, Box::new(ShowExplorer)),
        ("Source Control", "⌃⇧G", IconName::GitBranch, Box::new(ShowGit)),
        ("History graph", "", IconName::GitGraph, Box::new(ShowGitGraph)),
        ("Problems", "⇧⌘M", IconName::CircleAlert, Box::new(ShowProblems)),
        ("Terminal", "⌃`", IconName::SquareTerminal, Box::new(ToggleTerminal)),
        ("New terminal", "⌃⇧`", IconName::SquareTerminal, Box::new(NewTerminal)),
        ("Toggle sidebar", "⌘B", IconName::PanelLeft, Box::new(ToggleLeft)),
        ("Toggle panel", "⌘J", IconName::PanelBottom, Box::new(ToggleBottom)),
        ("Toggle chat", "⌥⌘B", IconName::PanelRight, Box::new(ToggleRight)),
        ("Ask the agent", "⌘L", IconName::Bot, Box::new(FocusChat)),
        ("Add to chat", "", IconName::TextCursorInput, Box::new(AddSelectionToChat)),
        ("AI instructions (.forgecode)", "", IconName::Bot, Box::new(OpenProjectInstructions)),
        ("Project settings (.forgecode)", "", IconName::Settings2, Box::new(OpenProjectSettings)),
        ("Settings", "⌘,", IconName::Settings, Box::new(ToggleSettings)),
        ("Toggle theme", "", IconName::SunMoon, Box::new(ToggleTheme)),
        ("New module…", "", IconName::Blocks, Box::new(NewModule)),
        ("Reload modules", "", IconName::RefreshCw, Box::new(ReloadModules)),
    ]
}

impl Workspace {
    /// Branch switcher opened from the status bar, like VS Code's: a picker at the top of the window.
    pub(super) fn branch_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let git = self.git.read(cx);
        if git.repo.is_none() {
            return;
        }
        let branches = git.branches().to_vec();
        let current = branches.iter().find(|(_, c)| *c).map(|(n, _)| n.clone());
        let others: Vec<String> = branches.iter().filter(|(_, c)| !c).map(|(n, _)| n.clone()).collect();

        let mut items = vec![PickerItem { label: t("Create branch…").into(), detail: String::new(), keys: String::new(), icon: IconName::Plus, tint: None }];
        if current.is_some() {
            items.push(PickerItem { label: t("Rename branch…").into(), detail: String::new(), keys: String::new(), icon: IconName::Pencil, tint: None });
        }
        if !others.is_empty() {
            items.push(PickerItem { label: t("Delete branch…").into(), detail: String::new(), keys: String::new(), icon: IconName::Trash, tint: None });
        }
        let actions = items.len();
        for (name, is_current) in &branches {
            let detail = if *is_current { t("current").to_string() } else { String::new() };
            items.push(PickerItem { label: name.clone(), detail, keys: String::new(), icon: IconName::GitBranch, tint: None });
        }

        let ws = cx.entity().downgrade();
        Picker::open(items, t("Select a branch to checkout").into(), window, cx, move |ix, window, cx| {
            let (ws, current, others, branches) = (ws.clone(), current.clone(), others.clone(), branches.clone());
            // Run after the picker's dialog closed, so a follow-up dialog can open.
            window.defer(cx, move |window, cx| {
                let _ = ws.update(cx, |w, cx| {
                    let git = w.git.clone();
                    if ix == 0 {
                        git.update(cx, |_, cx| cx.emit(GitEvent::NewBranch));
                    } else if ix == 1 && current.is_some() {
                        w.rename_branch(current.clone().unwrap_or_default(), window, cx);
                    } else if ix < actions {
                        let items = others.iter().map(|n| PickerItem { label: n.clone(), detail: String::new(), keys: String::new(), icon: IconName::GitBranch, tint: None }).collect();
                        let ws = cx.entity().downgrade();
                        Picker::open(items, t("Select a branch to delete").into(), window, cx, move |ix, window, cx| {
                            let (ws, name) = (ws.clone(), others[ix].clone());
                            window.defer(cx, move |window, cx| {
                                let _ = ws.update(cx, |w, cx| w.delete_branch(name, window, cx));
                            });
                        });
                    } else if let Some((name, false)) = branches.get(ix - actions).cloned() {
                        git.update(cx, |g, cx| g.checkout(name, false, cx));
                    }
                });
            });
        });
    }

    pub(super) fn command_palette(&mut self, _: &CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        let mut items = vec![];
        let mut entries = vec![];
        for (title, keys, icon, action) in actions() {
            items.push(PickerItem { label: t(title).to_string(), detail: String::new(), keys: crate::util::keys(keys), icon, tint: None });
            entries.push(Entry::Action(action));
        }
        for (plugin, command) in Plugins::global(cx).commands() {
            items.push(PickerItem { label: command.title.clone(), detail: plugin.manifest.name.clone(), keys: String::new(), icon: IconName::Blocks, tint: None });
            entries.push(Entry::Module(plugin.dir.clone(), command.clone()));
        }
        let ws = cx.entity().downgrade();
        Picker::open(items, t("Command palette").into(), window, cx, move |ix, window, cx| match &entries[ix] {
            // Dispatch after the dialog closed, so the action reaches the focused editor.
            Entry::Action(action) => {
                let action = action.boxed_clone();
                let ws = ws.clone();
                window.defer(cx, move |window, cx| {
                    // The closed dialog leaves nothing focused: focus the editor (or the workspace)
                    // so the action reaches its handlers.
                    let target = ws.upgrade().map(|w| {
                        let w = w.read(cx);
                        w.active_buffer().map(|b| b.editor.read(cx).focus_handle(cx)).unwrap_or(w.focus.clone())
                    });
                    if let Some(target) = target {
                        target.focus(window, cx);
                    }
                    window.dispatch_action(action, cx);
                });
            }
            Entry::Module(dir, command) => {
                let _ = ws.update(cx, |w, cx| w.run_module_command(dir, command, window, cx));
            }
        });
    }
}
