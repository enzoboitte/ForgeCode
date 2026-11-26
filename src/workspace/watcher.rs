//! Watching the project folder: files created, changed or deleted outside ForgeCode (an agent such
//! as Hermes, a terminal command, another editor) refresh the explorer, git, open files that
//! have no unsaved edits, and the language servers' view of the project.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher as _};

use super::*;

pub struct ProjectWatcher {
    _watcher: notify::RecommendedWatcher,
    _task: Task<()>,
}

impl Workspace {
    /// (Re)start watching `self.root`.
    pub(super) fn watch_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The UI tests' deterministic scheduler rejects wakeups from the OS watcher's thread.
        if cfg!(test) {
            return;
        }
        let root = self.root.clone();
        let (tx, rx) = smol::channel::unbounded::<PathBuf>();
        let watched = root.clone();
        let venvs = crate::language::project_venvs(&root);
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            for path in event.paths {
                if !venvs.iter().any(|v| path.starts_with(v)) && !ignored_quick(&path, &watched) {
                    let _ = tx.try_send(path);
                }
            }
        });
        let Ok(mut watcher) = watcher else { return };
        if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
            return;
        }
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                // Gather the burst (an agent writing several files, a git checkout…).
                let mut changed = BTreeSet::from([first]);
                loop {
                    let next = smol::future::or(async { rx.recv().await.ok() }, async {
                        smol::Timer::after(Duration::from_millis(300)).await;
                        None
                    })
                    .await;
                    match next {
                        Some(p) => {
                            changed.insert(p);
                        }
                        None => break,
                    }
                }
                let ok = this.update_in(cx, |this, window, cx| this.on_disk_changes(changed.into_iter().collect(), window, cx));
                if ok.is_err() {
                    break;
                }
            }
        });
        self.watcher = Some(ProjectWatcher { _watcher: watcher, _task: task });
    }

    fn on_disk_changes(&mut self, changed: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        // Open files without unsaved edits follow the disk.
        let stale: Vec<PathBuf> = self
            .buffers()
            .filter(|b| !b.dirty && changed.contains(&b.path) && std::fs::read_to_string(&b.path).is_ok_and(|disk| disk != b.text(cx)))
            .map(|b| b.path.clone())
            .collect();
        for b in self.buffers_mut().filter(|b| stale.contains(&b.path)) {
            b.reload(window, cx);
        }
        if let Some(cmake) = changed.iter().find(|p| crate::project::is_cmake_file(p)) {
            self.reconfigure_if_cmake(cmake, cx);
        }
        self.file_tree.update(cx, |t, cx| t.refresh(cx));
        self.git.update(cx, |g, cx| g.refresh(cx));
        // Language servers learn about new and changed files (a new module becomes importable).
        let changes: Vec<serde_json::Value> = changed
            .iter()
            .map(|p| serde_json::json!({"uri": crate::lsp::client::uri(p), "type": if p.exists() { 2 } else { 3 }}))
            .collect();
        for slot in self.servers.values() {
            if let LspSlot::Ready(client) = slot {
                client.notify("workspace/didChangeWatchedFiles", serde_json::json!({ "changes": changes }));
            }
        }
        cx.notify();
    }
}

/// Folders whose churn means nothing to the editor (build output, dependencies, git objects).
/// Virtualenvs are left out separately, found once when watching starts.
fn ignored_quick(path: &Path, root: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        matches!(name.as_ref(), "target" | "node_modules" | "__pycache__" | ".mypy_cache" | ".pytest_cache" | ".ruff_cache" | "dist" | ".next")
            || name.ends_with(".pyc")
    }) || rel.starts_with(".git/objects")
        || rel.starts_with(".git/logs")
}

#[cfg(test)]
mod tests {
    use super::ignored_quick as ignored;
    use std::path::Path;

    #[test]
    fn ignores_build_output_and_dependencies() {
        let root = Path::new("/p");
        assert!(ignored(Path::new("/p/target/debug/x"), root));
        assert!(ignored(Path::new("/p/web/node_modules/a/index.js"), root));
        assert!(ignored(Path::new("/p/.git/objects/ab/cd"), root));
        assert!(!ignored(Path::new("/p/backend/docker-compose.yml"), root));
        assert!(!ignored(Path::new("/p/.git/HEAD"), root), "branch switches refresh git");
    }
}
