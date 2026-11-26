//! What an agent turn changed on disk: line counts per file, and undoing it.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Original content of each file a turn wrote (None = it did not exist).
pub type Originals = BTreeMap<PathBuf, Option<Vec<u8>>>;

#[derive(Clone, Debug, PartialEq)]
pub struct FileChange {
    pub path: PathBuf,
    pub added: usize,
    pub removed: usize,
    /// The file before the turn (None: it was created), to show before/after.
    pub before: Option<Vec<u8>>,
}

/// Lines added and removed between two versions of a file.
fn count(old: &[u8], new: &[u8]) -> (usize, usize) {
    let (old, new) = (String::from_utf8_lossy(old), String::from_utf8_lossy(new));
    let diff = similar::TextDiff::from_lines(old.as_ref(), new.as_ref());
    diff.iter_all_changes().fold((0, 0), |(a, r), c| match c.tag() {
        similar::ChangeTag::Insert => (a + 1, r),
        similar::ChangeTag::Delete => (a, r + 1),
        similar::ChangeTag::Equal => (a, r),
    })
}

/// Files that differ from their original now, with their line counts.
pub fn summarize(originals: &Originals) -> Vec<FileChange> {
    originals
        .iter()
        .filter_map(|(path, old)| {
            let new = std::fs::read(path).ok();
            if &new == old {
                return None;
            }
            let (added, removed) = count(old.as_deref().unwrap_or_default(), new.as_deref().unwrap_or_default());
            Some(FileChange { path: path.clone(), added, removed, before: old.clone() })
        })
        .collect()
}

/// The change from `before` to the file now, as a git-style patch for the diff view.
pub fn patch(path: &std::path::Path, name: &str, before: Option<&[u8]>) -> String {
    let now = std::fs::read(path).ok();
    let (old, new) = (String::from_utf8_lossy(before.unwrap_or_default()), String::from_utf8_lossy(now.as_deref().unwrap_or_default()));
    let (from, to) = (if before.is_some() { format!("a/{name}") } else { "/dev/null".into() }, if now.is_some() { format!("b/{name}") } else { "/dev/null".into() });
    let mode = match (before.is_some(), now.is_some()) {
        (false, _) => "new file mode 100644\n",
        (_, false) => "deleted file mode 100644\n",
        _ => "",
    };
    let body = similar::TextDiff::from_lines(old.as_ref(), new.as_ref()).unified_diff().context_radius(3).header(&from, &to).to_string();
    format!("diff --git a/{name} b/{name}\n{mode}{body}")
}

/// One block of changed lines: `old` lines of the text before became `new` lines now.
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    pub old: std::ops::Range<usize>,
    pub new: std::ops::Range<usize>,
}

/// The blocks of changed lines between two texts, in order.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = vec![];
    for op in similar::TextDiff::from_lines(old, new).ops() {
        if op.tag() == similar::DiffTag::Equal {
            continue;
        }
        let (o, n) = (op.old_range(), op.new_range());
        match out.last_mut() {
            Some(h) if h.old.end == o.start && h.new.end == n.start => {
                h.old.end = o.end;
                h.new.end = n.end;
            }
            _ => out.push(Hunk { old: o, new: n }),
        }
    }
    out
}

/// `base` with its lines `range` replaced by `with`'s lines `from`.
fn splice_lines(base: &str, range: std::ops::Range<usize>, with: &str, from: std::ops::Range<usize>) -> String {
    let (b, w): (Vec<&str>, Vec<&str>) = (base.split_inclusive('\n').collect(), with.split_inclusive('\n').collect());
    [&b[..range.start], &w[from], &b[range.end..]].concat().concat()
}

/// Keep one block: the text "before" now has it, so it is no longer a change.
pub fn keep_hunk(old: &str, new: &str, h: &Hunk) -> String {
    splice_lines(old, h.old.clone(), new, h.new.clone())
}

/// Undo one block: the current text gets the old lines back.
pub fn undo_hunk(old: &str, new: &str, h: &Hunk) -> String {
    splice_lines(new, h.new.clone(), old, h.old.clone())
}

/// Put files back as they were; created files are deleted. Returns the touched paths.
pub fn restore(originals: &Originals) -> Vec<PathBuf> {
    for (path, old) in originals {
        let _ = match old {
            Some(bytes) => std::fs::write(path, bytes),
            None => std::fs::remove_file(path),
        };
    }
    originals.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_kept_or_undone_one_by_one() {
        let (old, new) = ("a\nb\nc\nd\ne\n", "a\nB\nc\nd\nE\nF\n");
        let h = hunks(old, new);
        assert_eq!(h, [Hunk { old: 1..2, new: 1..2 }, Hunk { old: 4..5, new: 4..6 }]);
        // Undo the second block: the first change stays.
        assert_eq!(undo_hunk(old, new, &h[1]), "a\nB\nc\nd\ne\n");
        // Keep the first: only the second is still a change.
        let kept = keep_hunk(old, new, &h[0]);
        assert_eq!(kept, "a\nB\nc\nd\ne\n");
        assert_eq!(hunks(&kept, new), [Hunk { old: 4..5, new: 4..6 }]);
    }

    #[test]
    fn counts_changes_and_restores_files() {
        let dir = std::env::temp_dir().join(format!("forge-changes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (edited, created, untouched) = (dir.join("a.rs"), dir.join("b.rs"), dir.join("c.rs"));
        std::fs::write(&edited, "one\ntwo\nthree\n").unwrap();
        std::fs::write(&untouched, "same\n").unwrap();
        let originals: Originals = [
            (edited.clone(), Some(b"one\ntwo\nthree\n".to_vec())),
            (created.clone(), None),
            (untouched.clone(), Some(b"same\n".to_vec())),
        ]
        .into();

        std::fs::write(&edited, "one\n2\nthree\nfour\n").unwrap();
        std::fs::write(&created, "new\nfile\n").unwrap();
        assert_eq!(
            summarize(&originals),
            vec![
                FileChange { path: edited.clone(), added: 2, removed: 1, before: Some(b"one\ntwo\nthree\n".to_vec()) },
                FileChange { path: created.clone(), added: 2, removed: 0, before: None },
            ]
        );
        // Before / after, as the diff view reads it.
        let p = patch(&edited, "e.txt", Some(b"one\ntwo\nthree\n"));
        assert!(p.starts_with("diff --git a/e.txt b/e.txt\n--- a/e.txt\n+++ b/e.txt\n@@"), "{p}");
        assert!(p.contains("-two\n+2\n") && p.contains("+four\n"), "{p}");
        let files = gpui_kit::component::diff::DiffFile::parse(&p).unwrap();
        assert_eq!(files.len(), 1);
        assert!(patch(&created, "c.txt", None).contains("new file mode"));

        restore(&originals);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "one\ntwo\nthree\n");
        assert!(!created.exists());
        assert!(summarize(&originals).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
