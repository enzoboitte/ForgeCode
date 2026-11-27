//! Workspace file explorer, VS Code style: lazy folders, git decorations, inline create/rename.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::dnd::DraggedFile;
use crate::git::panel::letter_color;
use crate::i18n::t;
use crate::icons;

pub enum FileTreeEvent {
    Open(PathBuf),
    /// One click: open in a temporary tab.
    Preview(PathBuf),
    Delete(PathBuf),
    OpenInTerminal(PathBuf),
    /// A file or folder was created inline.
    Created(PathBuf),
    Renamed(PathBuf, PathBuf),
    Error(String),
}

#[derive(Clone, Copy, PartialEq)]
enum EditKind {
    NewFile,
    NewFolder,
    Rename,
}

/// The inline input row being edited.
struct Editing {
    kind: EditKind,
    /// Parent folder for new entries, the entry itself for a rename.
    target: PathBuf,
    input: Entity<InputState>,
    _sub: Subscription,
}

pub struct FileTree {
    root: PathBuf,
    state: Entity<TreeState>,
    expanded: HashSet<SharedString>,
    /// Absolute path -> git status letter.
    git: Rc<HashMap<PathBuf, char>>,
    editing: Option<Editing>,
    /// Folders seen by the last scan: rendering never touches the disk.
    dirs: Rc<HashSet<PathBuf>>,
    /// The file row pressed and where: it opens on release nearby, even when the pointer moved
    /// enough to start a drag (a trackpad click often moves a few pixels).
    press: Option<(PathBuf, Point<Pixels>)>,
    _sub: Subscription,
}

impl EventEmitter<FileTreeEvent> for FileTree {}

/// Id of the inline "new entry" row.
const NEW_ROW: &str = "\u{1}new";
/// Prefix of the invisible child that makes a not-yet-read folder show as a folder.
const PLACEHOLDER: &str = "\u{1}placeholder:";

fn is_placeholder(id: &str) -> bool {
    id.starts_with(PLACEHOLDER)
}

/// Children of `dir`; folders are only read when expanded (VS Code's lazy tree).
fn scan(dir: &Path, expanded: &HashSet<SharedString>, new_in: Option<&Path>, dirs: &mut HashSet<PathBuf>) -> Vec<TreeItem> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).map(|e| e.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    entries.retain(|p| p.file_name().is_some_and(|n| n != ".DS_Store"));
    entries.sort_by_key(|p| (!p.is_dir(), p.file_name().map(|n| n.to_ascii_lowercase())));
    let mut items = Vec::new();
    if new_in == Some(dir) {
        items.push(TreeItem::new(NEW_ROW, ""));
    }
    for path in entries {
        let id: SharedString = path.display().to_string().into();
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut item = TreeItem::new(id.clone(), name);
        if path.is_dir() {
            dirs.insert(path.clone());
            let open = expanded.contains(&id);
            // A collapsed folder needs a (never shown) child to display as a folder; an open empty
            // folder gets none, so it leaves no blank row.
            let children = if open { scan(&path, expanded, new_in, dirs) } else { vec![TreeItem::new(format!("{PLACEHOLDER}{id}"), "")] };
            item = item.children(children).expanded(open);
        }
        items.push(item);
    }
    items
}

/// Move `src` into the folder `dir` (drag & drop). Refuses overwrites and moving a folder into itself.
fn move_into(src: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    let dest = dir.join(src.file_name().ok_or_else(|| std::io::Error::other("invalid path"))?);
    if dest == src {
        return Ok(dest);
    }
    if dir.starts_with(src) {
        return Err(std::io::Error::other("cannot move a folder into itself"));
    }
    if dest.exists() {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} already exists", dest.display())));
    }
    std::fs::rename(src, &dest)?;
    Ok(dest)
}

/// Copy a file or folder from outside the project (Finder drop) into `dir`.
fn copy_into(src: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    let dest = dir.join(src.file_name().ok_or_else(|| std::io::Error::other("invalid path"))?);
    if dest.exists() {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} already exists", dest.display())));
    }
    if src.is_dir() {
        std::fs::create_dir_all(&dest)?;
        for entry in std::fs::read_dir(src)?.flatten() {
            copy_into(&entry.path(), &dest)?;
        }
    } else {
        std::fs::copy(src, &dest)?;
    }
    Ok(dest)
}

/// Create or rename on disk; returns the resulting path.
fn apply_edit(kind: EditKind, target: &Path, name: &str) -> std::io::Result<PathBuf> {
    if name.contains('/') && kind == EditKind::Rename {
        return Err(std::io::Error::other("'/' is not allowed in a name"));
    }
    let dest = match kind {
        EditKind::Rename => target.with_file_name(name),
        _ => target.join(name),
    };
    if dest.exists() && dest != target {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} already exists", dest.display())));
    }
    match kind {
        EditKind::NewFolder => std::fs::create_dir_all(&dest)?,
        EditKind::NewFile => {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::OpenOptions::new().write(true).create_new(true).open(&dest)?;
        }
        EditKind::Rename => std::fs::rename(target, &dest)?,
    }
    Ok(dest)
}

impl FileTree {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| TreeState::new(cx));
        let _sub = cx.subscribe(&state, |this, _, event: &TreeEvent, cx| match event {
            TreeEvent::Expanded(id) => {
                this.expanded.insert(id.clone());
                this.refresh(cx);
            }
            TreeEvent::Collapsed(id) => {
                this.expanded.remove(id);
            }
        });
        let mut this = Self { root, state, expanded: HashSet::new(), git: Rc::default(), editing: None, dirs: Rc::default(), press: None, _sub };
        this.refresh(cx);
        this
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.root = root;
        self.expanded.clear();
        self.editing = None;
        self.refresh(cx);
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let new_in = self.editing.as_ref().filter(|e| e.kind != EditKind::Rename).map(|e| e.target.clone());
        let mut dirs = HashSet::new();
        let items = scan(&self.root, &self.expanded, new_in.as_deref(), &mut dirs);
        self.dirs = Rc::new(dirs);
        self.state.update(cx, |state, cx| state.set_items(items, cx));
    }

    #[cfg(test)]
    pub fn shows_dir(&self, path: &std::path::Path) -> bool {
        self.dirs.contains(path)
    }

    /// Git status per file; folders containing changes get a dot.
    pub fn set_git_status(&mut self, files: impl IntoIterator<Item = (PathBuf, char)>, cx: &mut Context<Self>) {
        let mut map = HashMap::new();
        for (path, letter) in files {
            for dir in path.ancestors().skip(1).take_while(|d| d.starts_with(&self.root) && *d != self.root) {
                map.entry(dir.to_path_buf()).or_insert('•');
            }
            map.insert(path, letter);
        }
        self.git = Rc::new(map);
        cx.notify();
    }

    /// Drop handler: moves within the project, copies from outside.
    fn drop_into(&mut self, dir: PathBuf, src: PathBuf, cx: &mut Context<Self>) {
        let inside = src.starts_with(&self.root);
        let result = if inside { move_into(&src, &dir) } else { copy_into(&src, &dir) };
        match result {
            Ok(dest) if inside => cx.emit(FileTreeEvent::Renamed(src, dest)),
            Ok(_) => cx.emit(FileTreeEvent::Created(dir.clone())),
            Err(e) => cx.emit(FileTreeEvent::Error(e.to_string())),
        }
        if dir != self.root {
            self.expanded.insert(dir.display().to_string().into());
        }
        self.refresh(cx);
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.refresh(cx);
    }

    /// Folder for new entries: the selected folder, the selected file's folder, or the root.
    fn selected_dir(&self, cx: &App) -> PathBuf {
        let selected = self.state.read(cx).selected_item().map(|i| PathBuf::from(i.id.as_ref()));
        match selected {
            Some(p) if p.is_dir() => p,
            Some(p) if p.exists() => p.parent().map(Path::to_path_buf).unwrap_or(self.root.clone()),
            _ => self.root.clone(),
        }
    }

    fn start_edit(&mut self, kind: EditKind, target: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let initial = if kind == EditKind::Rename { target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() } else { String::new() };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let _sub = cx.subscribe(&input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } | InputEvent::Blur => this.finish_edit(true, cx),
            _ => {}
        });
        if kind != EditKind::Rename && target != self.root {
            self.expanded.insert(target.display().to_string().into());
        }
        input.update(cx, |i, cx| i.focus(window, cx));
        self.editing = Some(Editing { kind, target, input, _sub });
        self.refresh(cx);
    }

    /// Apply (or drop, when empty or `commit` is false) the inline edit.
    fn finish_edit(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(edit) = self.editing.take() else { return };
        let name = edit.input.read(cx).value().trim().to_string();
        let unchanged = edit.kind == EditKind::Rename && edit.target.file_name().is_some_and(|n| n.to_string_lossy() == name);
        if commit && !name.is_empty() && !unchanged {
            match apply_edit(edit.kind, &edit.target, &name) {
                Ok(dest) if edit.kind == EditKind::Rename => cx.emit(FileTreeEvent::Renamed(edit.target.clone(), dest)),
                Ok(dest) => cx.emit(FileTreeEvent::Created(dest)),
                Err(e) => cx.emit(FileTreeEvent::Error(e.to_string())),
            }
        }
        self.refresh(cx);
    }

    fn render_edit_row(&self, depth: usize, cx: &App) -> AnyElement {
        let Some(edit) = &self.editing else { return div().into_any_element() };
        let icon = match edit.kind {
            EditKind::NewFolder => IconName::Folder,
            EditKind::Rename if edit.target.is_dir() => IconName::Folder,
            _ => IconName::File,
        };
        h_flex()
            .w_full()
            .pl(px(8. + 12. * depth as f32))
            .pr_2()
            .gap_1p5()
            .child(div().text_color(cx.theme().muted_foreground).child(icon))
            .child(div().flex_1().child(Input::new(&edit.input).xsmall()))
            .into_any_element()
    }

    /// Context menu for an entry (`entry`) or for the empty area (`entry` = None).
    fn menu(&self, dir: PathBuf, entry: Option<PathBuf>, menu: PopupMenu, view: WeakEntity<Self>) -> PopupMenu {
        let act = |label: &'static str, f: Rc<dyn Fn(&mut Self, &mut Window, &mut Context<Self>)>| {
            let view = view.clone();
            PopupMenuItem::new(t(label)).on_click(move |_, w, cx| {
                let _ = view.update(cx, |this, cx| f(this, w, cx));
            })
        };
        let (d1, d2, d3) = (dir.clone(), dir.clone(), dir.clone());
        let mut menu = menu
            .item(act("New File…", Rc::new(move |this, w, cx| this.start_edit(EditKind::NewFile, d1.clone(), w, cx))))
            .item(act("New Folder…", Rc::new(move |this, w, cx| this.start_edit(EditKind::NewFolder, d2.clone(), w, cx))));
        if let Some(path) = entry {
            let (p1, p2, copy, reveal) = (path.clone(), path.clone(), path.display().to_string(), path.clone());
            menu = menu
                .separator()
                .item(act("Rename…", Rc::new(move |this, w, cx| this.start_edit(EditKind::Rename, p1.clone(), w, cx))))
                .item(act("Delete", Rc::new(move |_, _, cx| cx.emit(FileTreeEvent::Delete(p2.clone())))))
                .separator()
                .item(PopupMenuItem::new(t("Copy Path")).on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))))
                .item(PopupMenuItem::new(t("Reveal in Finder")).on_click(move |_, _, cx| cx.reveal_path(&reveal)));
        } else {
            let reveal = dir.clone();
            menu = menu
                .separator()
                .item(act("Refresh", Rc::new(|this, _, cx| this.refresh(cx))))
                .item(act("Collapse all", Rc::new(|this, _, cx| this.collapse_all(cx))))
                .item(PopupMenuItem::new(t("Reveal in Finder")).on_click(move |_, _, cx| cx.reveal_path(&reveal)));
        }
        menu.item(act("Open in Terminal", Rc::new(move |_, _, cx| cx.emit(FileTreeEvent::OpenInTerminal(d3.clone())))))
    }
}

impl Render for FileTree {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let tree_view = view.clone();
        let menu_view = view.clone();
        let git = self.git.clone();
        let theme = cx.theme();
        let title = self.root.file_name().map(|n| n.to_string_lossy().to_uppercase()).unwrap_or_default();
        let renaming = self.editing.as_ref().filter(|e| e.kind == EditKind::Rename).map(|e| e.target.clone());
        let root = self.root.clone();
        let dirs = self.dirs.clone();
        let expanded = Rc::new(self.expanded.clone());
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" && this.editing.is_some() {
                    this.finish_edit(false, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                h_flex()
                    .h(px(36.))
                    .pl_3()
                    .pr_1()
                    .gap_0p5()
                    .child(div().flex_1().truncate().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(title))
                    .child(Button::new("tree-new-file").ghost().xsmall().icon(IconName::FilePlus).tooltip(t("New file"))
                        .on_click(cx.listener(|t, _, w, cx| {
                            let d = t.selected_dir(cx);
                            t.start_edit(EditKind::NewFile, d, w, cx)
                        })))
                    .child(Button::new("tree-new-folder").ghost().xsmall().icon(IconName::FolderPlus).tooltip(t("New folder"))
                        .on_click(cx.listener(|t, _, w, cx| {
                            let d = t.selected_dir(cx);
                            t.start_edit(EditKind::NewFolder, d, w, cx)
                        })))
                    .child(Button::new("tree-refresh").ghost().xsmall().icon(IconName::RefreshCw).tooltip(t("Refresh"))
                        .on_click(cx.listener(|t, _, _, cx| t.refresh(cx))))
                    .child(Button::new("tree-collapse").ghost().xsmall().icon(IconName::ChevronsDownUp).tooltip(t("Collapse all"))
                        .on_click(cx.listener(|t, _, _, cx| t.collapse_all(cx)))),
            )
            .child(
                div()
                    .id("tree-area")
                    .flex_1()
                    .min_h_0()
                    .child(
                        tree(&self.state, move |ix, entry, selected, _window, cx| {
                            let item = entry.item();
                            let depth = entry.depth();
                            if is_placeholder(&item.id) {
                                return ListItem::new(ix).h(px(0.)).disabled(true);
                            }
                            let editing_row = item.id.as_ref() == NEW_ROW || renaming.as_ref().is_some_and(|r| r.as_os_str() == item.id.as_ref());
                            if editing_row {
                                let row = tree_view.upgrade().map(|v| v.read(cx).render_edit_row(depth, cx));
                                return ListItem::new(ix).children(row);
                            }
                            let path = PathBuf::from(item.id.as_ref());
                            let is_dir = dirs.contains(&path);
                            let open = expanded.contains(&item.id);
                            let letter = git.get(&path).copied();
                            let color = letter.map(|l| if l == '•' { cx.theme().yellow.opacity(0.8) } else { letter_color(l, cx) });
                            let (icon, tint) = if is_dir { icons::folder_icon(&path, open) } else { icons::file_icon(&path) };
                            let muted = cx.theme().muted_foreground;
                            let view = tree_view.clone();
                            let label = item.label.clone();
                            // An open empty folder is a leaf for the tree widget: toggle it ourselves.
                            let leaf_dir = is_dir && !entry.is_folder();
                            ListItem::new(ix)
                                .debug_selector(move || format!("tree-{label}"))
                                .selected(selected)
                                .py_0()
                                .pl(px(4. + 14. * depth as f32))
                                .on_drag(DraggedFile(path.clone()), |d, _, _, cx| cx.new(|_| d.clone()))
                                .when(is_dir, |el| {
                                    let (v1, v2, d1, d2) = (view.clone(), view.clone(), path.clone(), path.clone());
                                    let hint = cx.theme().primary.opacity(0.18);
                                    el.drag_over::<DraggedFile>(move |s, _, _, _| s.bg(hint))
                                        .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(hint))
                                        .on_drop(move |f: &DraggedFile, _, cx| {
                                            let _ = v1.update(cx, |t, cx| t.drop_into(d1.clone(), f.0.clone(), cx));
                                        })
                                        .on_drop(move |paths: &ExternalPaths, _, cx| {
                                            let _ = v2.update(cx, |t, cx| {
                                                for p in paths.paths() {
                                                    t.drop_into(d2.clone(), p.clone(), cx);
                                                }
                                            });
                                        })
                                })
                                .child(
                                    h_flex()
                                        .w_full()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .gap_1()
                                        .text_sm()
                                        .child(div().w(px(14.)).flex_none().text_color(muted).when(is_dir, |el| el.child(if open { IconName::ChevronDown } else { IconName::ChevronRight })))
                                        .child(div().flex_none().text_color(tint).child(icon))
                                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().when_some(color, |el, c| el.text_color(c)).child(item.label.clone()))
                                        .when_some(letter.filter(|l| *l != '•').zip(color), |el, (l, c)| el.child(div().flex_none().pr_2().text_xs().text_color(c).child(l.to_string()))),
                                )
                                .when(!is_dir, |el| {
                                    let (down, up, pressed) = (view.clone(), view.clone(), path.clone());
                                    el.on_mouse_down(MouseButton::Left, move |e, _, cx| {
                                        let _ = down.update(cx, |t, _| t.press = Some((pressed.clone(), e.position)));
                                    })
                                    .on_mouse_up(MouseButton::Left, move |e, _, cx| {
                                        // Like VS Code: one click opens a temporary tab, a double click keeps it.
                                        let _ = up.update(cx, |t, cx| {
                                            if let Some((p, at)) = t.press.take()
                                                && (e.position - at).magnitude() < 12.
                                            {
                                                cx.emit(if e.click_count >= 2 { FileTreeEvent::Open(p) } else { FileTreeEvent::Preview(p) });
                                            }
                                        });
                                    })
                                })
                                .on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |t, cx| {
                                        if leaf_dir {
                                            let id: SharedString = path.display().to_string().into();
                                            if !t.expanded.remove(&id) {
                                                t.expanded.insert(id);
                                            }
                                            t.refresh(cx);
                                        }
                                    });
                                })
                        })
                        .context_menu(move |_, entry, menu, _, cx| {
                            let path = PathBuf::from(entry.item().id.as_ref());
                            if is_placeholder(&entry.item().id) || entry.item().id.as_ref() == NEW_ROW {
                                return menu;
                            }
                            let dir = if path.is_dir() { path.clone() } else { path.parent().map(Path::to_path_buf).unwrap_or_default() };
                            match menu_view.upgrade() {
                                Some(v) => v.read(cx).menu(dir, Some(path), menu, menu_view.clone()),
                                None => menu,
                            }
                        })
                        .size_full(),
                    )
                    // Dropping on the empty space targets the project root.
                    .on_drop(cx.listener(|t, f: &DraggedFile, _, cx| { let root = t.root.clone(); t.drop_into(root, f.0.clone(), cx) }))
                    .on_drop(cx.listener(|t, paths: &ExternalPaths, _, cx| {
                        let root = t.root.clone();
                        for p in paths.paths() {
                            t.drop_into(root.clone(), p.clone(), cx);
                        }
                    }))
                    // Right-click on the empty space below the files: actions on the root folder.
                    .context_menu(move |menu, _, cx| match view.upgrade() {
                        Some(v) => v.read(cx).menu(root.clone(), None, menu, view.clone()),
                        None => menu,
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{EditKind, PLACEHOLDER, apply_edit, copy_into, move_into, scan};
    use std::collections::HashSet;

    #[test]
    fn folders_are_lazy_and_empty_ones_stay_folders() {
        let dir = std::env::temp_dir().join(format!("forgecode-tree-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        let items = scan(&dir, &HashSet::new(), None, &mut Default::default());
        let names: Vec<_> = items.iter().map(|i| i.label.to_string()).collect();
        assert_eq!(names, vec!["empty", "sub", "a.txt"], "folders first");
        assert!(items[0].is_folder(), "an empty folder still shows as a folder");
        assert!(items[1].children[0].id.starts_with(PLACEHOLDER), "unopened folders are not read");

        let created = apply_edit(EditKind::NewFolder, &dir, "made").unwrap();
        assert!(created.is_dir());
        let renamed = apply_edit(EditKind::Rename, &dir.join("a.txt"), "b.txt").unwrap();
        assert!(renamed.is_file() && !dir.join("a.txt").exists());
        assert!(apply_edit(EditKind::NewFile, &dir, "b.txt").is_err(), "no overwrite");
        let moved = move_into(&dir.join("b.txt"), &dir.join("made")).unwrap();
        assert!(moved.is_file() && moved.starts_with(dir.join("made")));
        assert!(move_into(&dir.join("made"), &dir.join("made")).is_err(), "a folder onto itself");
        assert!(move_into(&dir.join("sub"), &dir.join("sub/deep")).is_err(), "not into itself");
        let copied = copy_into(&dir.join("sub"), &dir.join("made")).unwrap();
        assert!(copied.join("deep").is_dir() && dir.join("sub").is_dir(), "folders copy recursively");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
