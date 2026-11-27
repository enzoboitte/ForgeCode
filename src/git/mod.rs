//! Git through the `git` CLI: status, staging, commits, branches, remotes, diffs, history, line changes.
//! Blocking: call from background tasks.

pub mod actions;
pub mod forge;
pub mod graph;
pub mod panel;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq)]
pub struct FileStatus {
    /// Path relative to the repository root.
    pub path: String,
    /// Index (staged) status letter, `?` for untracked.
    pub index: char,
    /// Worktree status letter.
    pub worktree: char,
}

impl FileStatus {
    /// Both sides changed it in a merge, rebase or cherry-pick (`UU`, `AA`, `DU`…).
    pub fn conflicted(&self) -> bool {
        matches!((self.index, self.worktree), ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D'))
    }
    pub fn staged(&self) -> bool {
        !self.conflicted() && !matches!(self.index, ' ' | '?' | '!')
    }
    pub fn unstaged(&self) -> bool {
        !self.conflicted() && (self.worktree != ' ' || self.index == '?')
    }
    pub fn untracked(&self) -> bool {
        self.index == '?'
    }
    /// Letter shown for the worktree side (U for untracked, like VS Code).
    pub fn letter(&self, staged: bool) -> char {
        match (staged, self.index, self.worktree) {
            (_, '?', _) => 'U',
            (true, i, _) => i,
            (false, _, w) => w,
        }
    }
}

/// A multi-step git operation stopped on conflicts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl Operation {
    fn command(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
        }
    }
}

/// Which version of a conflicted file to keep. `Ours` is the current branch (VS Code's
/// "current"), `Theirs` the branch being merged in ("incoming"), `AsIs` the edited file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Side {
    Ours,
    Theirs,
    AsIs,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// Merge or rebase in progress.
    pub operation: Option<Operation>,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileStatus>,
    /// Tracked files that .gitignore ignores (committed before the rule was added).
    pub ignored_tracked: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub parents: Vec<String>,
    pub short: String,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub refs: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineChange {
    Added,
    Modified,
    /// Lines were deleted just above this line.
    Deleted,
}

#[derive(Clone, Debug)]
pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    /// The repository containing `dir`, if any.
    pub fn discover(dir: &Path) -> Option<Repo> {
        let out = crate::util::command("git").args(["rev-parse", "--show-toplevel"]).current_dir(dir).output().ok()?;
        out.status.success().then(|| Repo { root: PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()) })
    }

    pub fn init(dir: &Path) -> Result<Repo> {
        run(dir, &["init"])?;
        Ok(Repo { root: dir.to_path_buf() })
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        run(&self.root, args)
    }

    pub fn status(&self) -> Result<Status> {
        let mut status = parse_status(&self.git(&["status", "--porcelain=v1", "-b", "-z", "--untracked-files=all"])?);
        status.operation = self.operation();
        status.ignored_tracked = self.git(&["ls-files", "-ci", "--exclude-standard", "-z"])?.split('\0').filter(|p| !p.is_empty()).map(String::from).collect();
        Ok(status)
    }

    /// The merge, rebase, cherry-pick or revert waiting for conflicts to be resolved.
    pub fn operation(&self) -> Option<Operation> {
        let dir = self.git(&["rev-parse", "--git-dir"]).ok()?;
        let dir = self.root.join(dir.trim());
        if dir.join("rebase-merge").is_dir() || dir.join("rebase-apply").is_dir() {
            Some(Operation::Rebase)
        } else if dir.join("MERGE_HEAD").is_file() {
            Some(Operation::Merge)
        } else if dir.join("CHERRY_PICK_HEAD").is_file() {
            Some(Operation::CherryPick)
        } else if dir.join("REVERT_HEAD").is_file() {
            Some(Operation::Revert)
        } else {
            None
        }
    }

    /// Merge `branch` into the current one. Stopping on conflicts isn't an error: they show up
    /// in the status to be resolved.
    pub fn merge(&self, branch: &str) -> Result<()> {
        self.stop_on_conflicts(self.git(&["merge", "--no-edit", &self.resolve_branch(branch)]))
    }

    pub fn rebase(&self, onto: &str) -> Result<()> {
        self.stop_on_conflicts(self.git(&["rebase", &self.resolve_branch(onto)]))
    }

    fn stop_on_conflicts(&self, result: Result<String>) -> Result<()> {
        match result {
            Err(_) if self.status().is_ok_and(|s| s.files.iter().any(FileStatus::conflicted)) => Ok(()),
            r => r.map(drop),
        }
    }

    pub fn abort(&self, op: Operation) -> Result<()> {
        self.git(&[op.command(), "--abort"]).map(drop)
    }

    /// Finish the operation once every conflict is resolved, keeping git's default messages.
    pub fn continue_(&self, op: Operation) -> Result<()> {
        // No editor: git would wait for one to close.
        let args: &[&str] = match op {
            Operation::Merge => &["-c", "core.editor=true", "commit", "--no-edit"],
            _ => &["-c", "core.editor=true", op.command(), "--continue"],
        };
        self.stop_on_conflicts(self.git(args))
    }

    /// Resolve a conflicted file by keeping one side, then mark it resolved.
    pub fn resolve(&self, path: &str, side: Side) -> Result<()> {
        let flag = match side {
            Side::Ours => Some("--ours"),
            Side::Theirs => Some("--theirs"),
            Side::AsIs => None,
        };
        if let Some(flag) = flag {
            // The kept side deleted the file: the resolution is to delete it.
            if self.git(&["checkout", flag, "--", path]).is_err() {
                return self.git(&["rm", "--quiet", "--", path]).map(drop);
            }
        }
        self.git(&["add", "-f", "--", path]).map(drop)
    }

    /// `-f`: these paths come from `git status`, which lists ignored files only when they are
    /// tracked (a tracked file in a directory added to .gitignore later), and plain `git add`
    /// refuses those.
    pub fn stage(&self, paths: &[String]) -> Result<()> {
        let mut args = vec!["add", "-f", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.git(&args).map(drop)
    }

    /// Stop tracking `paths`: removed from the repository at the next commit, kept on disk.
    pub fn untrack(&self, paths: &[String]) -> Result<()> {
        let mut args = vec!["rm", "-r", "--cached", "--quiet", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.git(&args).map(drop)
    }

    pub fn unstage(&self, paths: &[String]) -> Result<()> {
        let mut args = vec!["reset", "-q", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.git(&args).map(drop)
    }

    /// Throw away worktree changes (deletes untracked files).
    pub fn discard(&self, files: &[FileStatus]) -> Result<()> {
        let (untracked, tracked): (Vec<_>, Vec<_>) = files.iter().partition(|f| f.untracked());
        if !tracked.is_empty() {
            let mut args = vec!["checkout", "--"];
            args.extend(tracked.iter().map(|f| f.path.as_str()));
            self.git(&args)?;
        }
        if !untracked.is_empty() {
            let mut args = vec!["clean", "-f", "--"];
            args.extend(untracked.iter().map(|f| f.path.as_str()));
            self.git(&args)?;
        }
        Ok(())
    }

    pub fn commit(&self, message: &str, amend: bool) -> Result<String> {
        let mut args = vec!["commit", "-m", message];
        if amend {
            args.push("--amend");
        }
        self.git(&args)
    }

    /// Local branches, current first.
    /// Local branches, then those only on a remote (as their plain name: checking one out
    /// creates the local branch tracking it). The current one first.
    pub fn branches(&self) -> Result<Vec<(String, bool)>> {
        let out = self.git(&["for-each-ref", "--format=%(HEAD)%(refname)", "refs/heads", "refs/remotes"])?;
        let mut v: Vec<(String, bool)> = vec![];
        // refs/heads sort before refs/remotes: a local branch hides the remote one of its name.
        for line in out.lines().filter(|l| !l.is_empty()) {
            let (current, name) = (line.starts_with('*'), &line[1..]);
            if let Some(local) = name.strip_prefix("refs/heads/") {
                v.push((local.to_string(), current));
            } else if let Some((_, branch)) = name.strip_prefix("refs/remotes/").and_then(|r| r.split_once('/'))
                && branch != "HEAD"
                && !v.iter().any(|(n, _)| n == branch)
            {
                v.push((branch.to_string(), false));
            }
        }
        v.sort_by_key(|(_, current)| !current);
        Ok(v)
    }

    /// `branch` as git can merge or rebase onto it: the local branch, else the remote one.
    fn resolve_branch(&self, branch: &str) -> String {
        if self.git(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
            return branch.to_string();
        }
        self.remote_of(branch).map_or(branch.to_string(), |remote| format!("{remote}/{branch}"))
    }

    /// The remote holding `branch`: the one it tracks, else any remote with a branch of that name.
    fn remote_of(&self, branch: &str) -> Option<String> {
        let tracked = self.git(&["for-each-ref", "--format=%(upstream:remotename)", &format!("refs/heads/{branch}")]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        tracked.or_else(|| {
            let out = self.git(&["for-each-ref", "--format=%(refname)", "refs/remotes"]).ok()?;
            out.lines().find_map(|r| {
                let (remote, name) = r.strip_prefix("refs/remotes/")?.split_once('/')?;
                (name == branch).then(|| remote.to_string())
            })
        })
    }

    pub fn checkout(&self, branch: &str, create: bool) -> Result<()> {
        let args: &[&str] = if create { &["checkout", "-b", branch] } else { &["checkout", branch] };
        self.git(args).map(drop)
    }

    /// Delete a branch here and on its remote, even with unmerged commits (the UI confirms
    /// first). A branch only on the remote is deleted there.
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        let remote = self.remote_of(branch);
        if self.git(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
            self.git(&["branch", "-D", branch])?;
        }
        if let Some(remote) = remote {
            self.git(&["push", &remote, "--delete", branch])?;
        }
        Ok(())
    }

    pub fn rename_branch(&self, from: &str, to: &str) -> Result<()> {
        self.git(&["branch", "-m", from, to]).map(drop)
    }

    /// The address of `origin`, if the repository has one.
    pub fn remote_url(&self) -> Option<String> {
        self.git(&["remote", "get-url", "origin"]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    /// Link this repository to the server at `url` as `origin` (replacing its address if set).
    pub fn set_remote(&self, url: &str) -> Result<()> {
        let verb = if self.remote_url().is_some() { "set-url" } else { "add" };
        self.git(&["remote", verb, "origin", url]).map(drop)
    }

    pub fn fetch(&self) -> Result<String> {
        self.git(&["fetch", "--all", "--prune"])
    }

    /// Pull like VS Code: when local and remote both have new commits, merge them (or rebase if
    /// the user configured `pull.rebase`) instead of failing on a non fast-forward.
    pub fn pull(&self) -> Result<String> {
        let configured = self.git(&["config", "--get", "pull.rebase"]).is_ok_and(|v| !v.trim().is_empty());
        if configured { self.git(&["pull", "--no-edit"]) } else { self.git(&["pull", "--no-rebase", "--no-edit"]) }
    }

    /// Push `branch` to the remote (`origin`, else the only one) and track it there.
    pub fn publish_branch(&self, branch: &str) -> Result<()> {
        let remotes = self.git(&["remote"])?;
        let remote = remotes.lines().find(|r| *r == "origin").or_else(|| remotes.lines().next()).ok_or_else(|| anyhow::anyhow!("no remote repository configured (git remote add origin <url>)"))?;
        self.git(&["push", "--set-upstream", remote, branch]).map(drop)
    }

    pub fn push(&self, status: &Status) -> Result<String> {
        if status.upstream.is_some() {
            self.git(&["push"])
        } else {
            self.git(&["push", "--set-upstream", "origin", &status.branch])
        }
    }

    /// Patch of one file: staged (index vs HEAD) or working tree (vs index).
    pub fn diff_file(&self, file: &FileStatus, staged: bool) -> Result<String> {
        if file.untracked() {
            // `--no-index` exits with 1 when files differ.
            let out = crate::util::command("git").args(["diff", "--no-index", "--", "/dev/null", &file.path]).current_dir(&self.root).output()?;
            return Ok(String::from_utf8_lossy(&out.stdout).to_string());
        }
        if staged { self.git(&["diff", "--cached", "--", &file.path]) } else { self.git(&["diff", "--", &file.path]) }
    }

    /// What the next commit will contain: the staged diff, or every change when nothing is staged.
    pub fn pending_diff(&self) -> Result<String> {
        let staged = self.git(&["diff", "--cached", "--stat", "--patch"])?;
        if !staged.trim().is_empty() {
            return Ok(staged);
        }
        self.git(&["diff", "--stat", "--patch"])
    }

    /// Patch introduced by a commit.
    pub fn show(&self, hash: &str) -> Result<String> {
        self.git(&["show", "--format=", "--patch", hash])
    }

    pub fn log(&self, limit: usize) -> Result<Vec<Commit>> {
        let n = limit.to_string();
        let out = self.git(&["log", "--all", "--date-order", "--date=format:%Y-%m-%d %H:%M", "--format=%h%x00%H%x00%P%x00%s%x00%an%x00%ad%x00%D", "-n", &n])?;
        Ok(parse_log(&out))
    }

    /// Changed lines of a file's working copy against HEAD, for gutter markers.
    pub fn line_changes(&self, path: &Path) -> Result<Vec<(u32, u32, LineChange)>> {
        let rel = path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
        Ok(parse_hunks(&self.git(&["diff", "-U0", "--no-color", "HEAD", "--", &rel]).unwrap_or_default()))
    }
}

fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = crate::util::command("git").args(args).current_dir(dir).env("GIT_TERMINAL_PROMPT", "0").output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    // Some commands (push, pull) report on stderr.
    Ok(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// Parse `git status --porcelain=v1 -b -z`.
pub fn parse_status(out: &str) -> Status {
    let mut status = Status::default();
    let mut entries = out.split('\0').filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        if let Some(head) = entry.strip_prefix("## ") {
            let (names, counts) = head.split_once(" [").unwrap_or((head, ""));
            let (branch, upstream) = names.split_once("...").map_or((names, None), |(b, u)| (b, Some(u.to_string())));
            status.branch = branch.trim_start_matches("No commits yet on ").to_string();
            status.upstream = upstream;
            for part in counts.trim_end_matches(']').split(", ") {
                if let Some(n) = part.strip_prefix("ahead ") {
                    status.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix("behind ") {
                    status.behind = n.parse().unwrap_or(0);
                }
            }
            continue;
        }
        if entry.len() < 4 {
            continue;
        }
        let mut chars = entry.chars();
        let (index, worktree) = (chars.next().unwrap(), chars.next().unwrap());
        // Renames/copies carry the original path as the next entry.
        if matches!(index, 'R' | 'C') {
            entries.next();
        }
        status.files.push(FileStatus { path: entry[3..].to_string(), index, worktree });
    }
    status
}

pub fn parse_log(out: &str) -> Vec<Commit> {
    out.lines()
        .map(|line| {
            let mut parts = line.split('\0');
            let mut f = || parts.next().unwrap_or("").to_string();
            let (short, hash) = (f(), f());
            let parents = f().split_whitespace().map(String::from).collect();
            Commit { short, hash, parents, subject: f(), author: f(), date: f(), refs: f() }
        })
        .collect()
}

/// Parse `git diff -U0` hunk headers into (first line, last line, kind), 1-based.
pub fn parse_hunks(diff: &str) -> Vec<(u32, u32, LineChange)> {
    let span = |s: &str| -> (u32, u32) {
        let (start, len) = s.split_once(',').unwrap_or((s, "1"));
        (start.parse().unwrap_or(0), len.parse().unwrap_or(1))
    };
    diff.lines()
        .filter_map(|l| l.strip_prefix("@@ -"))
        .filter_map(|l| {
            let (old, rest) = l.split_once(" +")?;
            let new = rest.split_once(' ')?.0;
            let ((_, old_len), (start, len)) = (span(old), span(new));
            Some(match (old_len, len) {
                (0, _) => (start, start + len - 1, LineChange::Added),
                (_, 0) => (start.max(1), start.max(1), LineChange::Deleted),
                _ => (start, start + len - 1, LineChange::Modified),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_status() {
        let out = "## main...origin/main [ahead 2, behind 1]\0M  src/a.rs\0 M b.txt\0?? new file.c\0R  new.rs\0old.rs\0";
        let s = parse_status(out);
        assert_eq!((s.branch.as_str(), s.upstream.as_deref(), s.ahead, s.behind), ("main", Some("origin/main"), 2, 1));
        assert_eq!(s.files.len(), 4);
        assert!(s.files[0].staged() && !s.files[0].unstaged());
        assert!(!s.files[1].staged() && s.files[1].unstaged());
        assert_eq!(s.files[2].path, "new file.c");
        assert_eq!(s.files[2].letter(false), 'U');
        assert_eq!(s.files[3].path, "new.rs");
    }

    #[test]
    fn parses_hunks() {
        let d = "@@ -3,0 +4,2 @@\n+a\n+b\n@@ -10,2 +12,0 @@\n-x\n@@ -20 +20 @@\n-y\n+z\n";
        assert_eq!(parse_hunks(d), vec![(4, 5, LineChange::Added), (12, 12, LineChange::Deleted), (20, 20, LineChange::Modified)]);
    }

    #[test]
    fn parses_graph_log() {
        let c = parse_log("abc\0abcdef\0p1 p2\0Fix\0Ana\02024-01-02 10:00\0HEAD -> main\n");
        assert_eq!(c[0].subject, "Fix");
        assert_eq!(c[0].refs, "HEAD -> main");
        assert_eq!(c[0].parents, ["p1", "p2"]);
    }

    #[test]
    fn real_repo_roundtrip() {
        let dir = std::env::temp_dir().join(format!("forge-git-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo::init(&dir).unwrap();
        run(&dir, &["config", "user.email", "t@t"]).unwrap();
        run(&dir, &["config", "user.name", "t"]).unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        repo.stage(&["a.txt".into()]).unwrap();
        repo.commit("first", false).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let s = repo.status().unwrap();
        assert_eq!(s.files[0].worktree, 'M');
        assert!(repo.diff_file(&s.files[0], false).unwrap().contains("+two"));
        assert_eq!(repo.line_changes(&dir.join("a.txt")).unwrap(), vec![(2, 2, LineChange::Added)]);
        assert_eq!(repo.log(10).unwrap()[0].subject, "first");
        repo.discard(&s.files).unwrap();
        assert!(repo.status().unwrap().files.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A merge stopping on a conflict, resolved by keeping the incoming side, then finished.
    #[test]
    fn merge_conflict_resolution() {
        let dir = std::env::temp_dir().join(format!("forge-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo::init(&dir).unwrap();
        run(&dir, &["config", "user.email", "t@t"]).unwrap();
        run(&dir, &["config", "user.name", "t"]).unwrap();
        let commit = |text: &str, msg: &str| {
            std::fs::write(dir.join("a.txt"), text).unwrap();
            repo.stage(&["a.txt".into()]).unwrap();
            repo.commit(msg, false).unwrap();
        };
        commit("base\n", "base");
        let main = repo.status().unwrap().branch;
        repo.checkout("feature", true).unwrap();
        commit("theirs\n", "feature");
        repo.checkout(&main, false).unwrap();
        commit("ours\n", "main");

        repo.merge("feature").unwrap();
        let s = repo.status().unwrap();
        assert_eq!(s.operation, Some(Operation::Merge));
        assert!(s.files[0].conflicted() && !s.files[0].staged() && !s.files[0].unstaged());
        repo.resolve("a.txt", Side::Theirs).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "theirs\n");
        repo.continue_(Operation::Merge).unwrap();
        let s = repo.status().unwrap();
        assert!(s.operation.is_none() && s.files.is_empty());
        assert_eq!(repo.log(1).unwrap()[0].parents.len(), 2, "a merge commit");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Branches made elsewhere show up after a fetch, can be merged and checked out; deleting
    /// a branch deletes it on the remote too, even one never checked out here.
    #[test]
    fn stages_a_tracked_file_in_an_ignored_directory() {
        let dir = std::env::temp_dir().join(format!("forge-ignored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".cache")).unwrap();
        let repo = Repo::init(&dir).unwrap();
        run(&dir, &["config", "user.email", "t@t"]).unwrap();
        run(&dir, &["config", "user.name", "t"]).unwrap();
        std::fs::write(dir.join(".cache/i.idx"), "a").unwrap();
        run(&dir, &["add", "."]).unwrap();
        run(&dir, &["commit", "-qm", "a"]).unwrap();
        std::fs::write(dir.join(".gitignore"), ".cache/\n").unwrap();
        std::fs::write(dir.join(".cache/i.idx"), "b").unwrap();
        let paths: Vec<String> = repo.status().unwrap().files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths, [".cache/i.idx", ".gitignore"]);
        repo.stage(&paths).unwrap();
        assert!(repo.status().unwrap().files.iter().all(FileStatus::staged));
        // ForgeCode offers to stop tracking it: gone from the index, still on disk.
        assert_eq!(repo.status().unwrap().ignored_tracked, [".cache/i.idx"]);
        repo.untrack(&repo.status().unwrap().ignored_tracked).unwrap();
        assert!(repo.status().unwrap().ignored_tracked.is_empty() && dir.join(".cache/i.idx").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_local_project_is_linked_to_a_server() {
        let base = std::env::temp_dir().join(format!("forge-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (origin, local) = (base.join("origin.git"), base.join("local"));
        std::fs::create_dir_all(&local).unwrap();
        run(&base, &["init", "-q", "--bare", origin.to_str().unwrap()]).unwrap();
        let repo = Repo::init(&local).unwrap();
        run(&local, &["config", "user.email", "t@t"]).unwrap();
        run(&local, &["config", "user.name", "t"]).unwrap();
        std::fs::write(local.join("a.txt"), "a").unwrap();
        run(&local, &["add", "."]).unwrap();
        run(&local, &["commit", "-qm", "a"]).unwrap();
        assert_eq!(repo.remote_url(), None);
        repo.set_remote("https://example.invalid/x.git").unwrap();
        repo.set_remote(origin.to_str().unwrap()).unwrap(); // replaces the address
        assert_eq!(repo.remote_url().as_deref(), origin.to_str());
        let branch = run(&local, &["branch", "--show-current"]).unwrap();
        repo.publish_branch(branch.trim()).unwrap();
        assert!(run(&base, &["--git-dir", origin.to_str().unwrap(), "log", "--oneline"]).unwrap().contains(" a"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remote_branches_are_listed_and_deleted_on_the_remote() {
        let base = std::env::temp_dir().join(format!("forge-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (origin, a, b) = (base.join("origin.git"), base.join("a"), base.join("b"));
        run(&base, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]).unwrap();
        for dir in [&a, &b] {
            run(&base, &["clone", "-q", origin.to_str().unwrap(), dir.to_str().unwrap()]).unwrap();
            run(dir, &["config", "user.email", "t@t"]).unwrap();
            run(dir, &["config", "user.name", "t"]).unwrap();
        }
        let (ra, rb) = (Repo { root: a.clone() }, Repo { root: b.clone() });
        std::fs::write(a.join("a.txt"), "a").unwrap();
        ra.stage(&["a.txt".into()]).unwrap();
        ra.commit("base", false).unwrap();
        run(&a, &["push", "-q", "-u", "origin", "main"]).unwrap();
        // Someone else publishes two branches.
        rb.fetch().unwrap();
        run(&b, &["checkout", "-q", "main"]).unwrap();
        for name in ["feature", "other"] {
            run(&b, &["checkout", "-q", "-b", name, "main"]).unwrap();
            std::fs::write(b.join(format!("{name}.txt")), name).unwrap();
            rb.stage(&[format!("{name}.txt")]).unwrap();
            rb.commit(name, false).unwrap();
            run(&b, &["push", "-q", "-u", "origin", name]).unwrap();
        }
        let names = |r: &Repo| r.branches().unwrap().into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names(&ra), ["main"]);
        ra.fetch().unwrap();
        assert_eq!(names(&ra), ["main", "feature", "other"]);
        // Merged straight from the remote, then checked out (a local branch tracking it).
        ra.merge("feature").unwrap();
        assert!(a.join("feature.txt").exists());
        ra.checkout("feature", false).unwrap();
        assert_eq!(names(&ra), ["feature", "main", "other"]);
        ra.checkout("main", false).unwrap();
        // A new branch published from here appears on the remote, tracked.
        ra.checkout("published", true).unwrap();
        ra.publish_branch("published").unwrap();
        assert!(run(&base, &["--git-dir", origin.to_str().unwrap(), "branch", "--format=%(refname:short)"]).unwrap().contains("published"));
        assert_eq!(ra.status().unwrap().upstream.as_deref(), Some("origin/published"));
        ra.checkout("main", false).unwrap();
        ra.delete_branch("published").unwrap();
        let on_remote = || run(&base, &["--git-dir", origin.to_str().unwrap(), "branch", "--format=%(refname:short)"]).unwrap();
        ra.delete_branch("feature").unwrap();
        ra.delete_branch("other").unwrap();
        assert_eq!(on_remote().trim(), "main");
        assert_eq!(names(&ra), ["main"]);
        std::fs::remove_dir_all(base).unwrap();
    }

    /// An unpublished branch with its own commit can be renamed, then deleted.
    #[test]
    fn rename_and_delete_branch() {
        let dir = std::env::temp_dir().join(format!("forge-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo::init(&dir).unwrap();
        run(&dir, &["config", "user.email", "t@t"]).unwrap();
        run(&dir, &["config", "user.name", "t"]).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        repo.stage(&["a.txt".into()]).unwrap();
        repo.commit("base", false).unwrap();
        let main = repo.status().unwrap().branch;
        repo.checkout("typo", true).unwrap();
        std::fs::write(dir.join("a.txt"), "b").unwrap();
        repo.stage(&["a.txt".into()]).unwrap();
        repo.commit("unmerged", false).unwrap();

        repo.rename_branch("typo", "fixed").unwrap();
        assert_eq!(repo.status().unwrap().branch, "fixed");
        repo.checkout(&main, false).unwrap();
        repo.delete_branch("fixed").unwrap();
        assert_eq!(repo.branches().unwrap(), vec![(main, true)]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
