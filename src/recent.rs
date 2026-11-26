//! Recently opened projects, shown on the welcome screen.

use std::path::{Path, PathBuf};

const MAX: usize = 10;

fn file() -> PathBuf {
    crate::settings::config_dir().join("recent.json")
}

pub fn list() -> Vec<PathBuf> {
    std::fs::read_to_string(file())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<PathBuf>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

/// Put `root` first, without duplicates.
pub fn touch(root: &Path) {
    if cfg!(test) {
        return; // UI tests open temporary folders: keep the user's list clean
    }
    let _ = std::fs::write(file(), serde_json::to_string(&push(list(), root)).unwrap_or_default());
}

fn push(mut list: Vec<PathBuf>, root: &Path) -> Vec<PathBuf> {
    list.retain(|p| p != root);
    list.insert(0, root.to_path_buf());
    list.truncate(MAX);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_first_without_duplicates() {
        let l = push(vec![PathBuf::from("/a"), PathBuf::from("/b")], Path::new("/b"));
        assert_eq!(l, vec![PathBuf::from("/b"), PathBuf::from("/a")]);
        let many = (0..20).fold(vec![], |l, i| push(l, Path::new(&format!("/{i}"))));
        assert_eq!(many.len(), MAX);
    }
}
