//! Git status for the file browsers.
//!
//! Both the TUI and the web UI colour entries by status: modified yellow,
//! untracked and added green, deleted and conflicted red, renamed purple,
//! ignored dimmed. `statuses` runs one `git status` and keeps the repository
//! root next to the map, so a listing asks for the status of an absolute path
//! and never has to spell the root the way git would.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What the tree shows for one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// A file the repository ignores; lowest priority, so a real change in the
    /// same directory still wins.
    Ignored,
    Untracked,
    Added,
    Deleted,
    Renamed,
    Modified,
    Conflicted,
}

impl Status {
    /// Which status a directory badge shows when its children disagree: the
    /// one that would cost the most to miss.
    pub fn rank(self) -> u8 {
        match self {
            Self::Ignored => 0,
            Self::Untracked => 1,
            Self::Added => 2,
            Self::Renamed => 3,
            Self::Deleted => 4,
            Self::Modified => 5,
            Self::Conflicted => 6,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ignored => "ignored",
            Self::Untracked => "untracked",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Modified => "modified",
            Self::Conflicted => "conflicted",
        }
    }
}

/// Every change in the work tree that contains `dir`, plus the root the paths
/// are relative to. `None` when `dir` is not inside a git work tree or `git` is
/// not installed.
pub struct Statuses {
    root: PathBuf,
    entries: BTreeMap<PathBuf, Status>,
}

impl Statuses {
    /// The status of one entry, given its absolute path.
    pub fn of(&self, abs: &Path, is_dir: bool) -> Option<Status> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        self.find(rel, is_dir)
    }

    /// The entry's own status, or the strongest status of anything below it.
    fn find(&self, rel: &Path, is_dir: bool) -> Option<Status> {
        let own = self.entries.get(rel).copied();
        if !is_dir {
            return own;
        }
        // A directory can be both (an untracked folder) and contain changes.
        // Keys are sorted, so the descendants are one contiguous run.
        self.entries
            .range(rel.to_path_buf()..)
            .take_while(|(path, _)| path.starts_with(rel))
            .map(|(_, status)| *status)
            .chain(own)
            .max_by_key(|status| status.rank())
    }
}

pub fn statuses(dir: &Path) -> Option<Statuses> {
    let root = toplevel(dir)?;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&root)
        .args(["status", "--porcelain=v1", "-z", "--ignored=matching"])
        .env("GIT_OPTIONAL_LOCKS", "0");
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut map = BTreeMap::new();
    let mut fields = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        let Ok(field) = std::str::from_utf8(field) else {
            continue;
        };
        if field.len() < 4 {
            continue;
        }
        let (code, path) = field.split_at(3);
        let mut path = path.to_string();
        // A rename is followed by the original path as its own field.
        if code.starts_with('R') || code.starts_with('C') {
            let _ = fields.next();
        }
        if let Some(rest) = path.strip_suffix('/') {
            path = rest.to_string();
        }
        map.entry(PathBuf::from(path))
            .and_modify(|status: &mut Status| {
                let next = classification(code);
                if next.rank() > status.rank() {
                    *status = next;
                }
            })
            .or_insert_with(|| classification(code));
    }
    Some(Statuses { root, entries: map })
}

fn classification(code: &str) -> Status {
    let index = code.chars().next().unwrap_or(' ');
    let worktree = code.chars().nth(1).unwrap_or(' ');
    let both = [index, worktree];
    if both.contains(&'U') || (index == 'A' && worktree == 'A') || (index == 'D' && worktree == 'D')
    {
        return Status::Conflicted;
    }
    if both.contains(&'!') || index == '!' || worktree == '!' {
        return Status::Ignored;
    }
    if both.contains(&'?') {
        return Status::Untracked;
    }
    if both.contains(&'R') || both.contains(&'C') {
        return Status::Renamed;
    }
    if both.contains(&'D') {
        return Status::Deleted;
    }
    if both.contains(&'A') {
        return Status::Added;
    }
    Status::Modified
}

/// `None` when `dir` is not in a repository. Working directories inside a
/// linked worktree resolve to the worktree root, not the main one.
fn toplevel(dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let root = PathBuf::from(text.trim());
    if !root.is_dir() {
        return None;
    }
    // Canonical, so it can be compared against paths built from a canonical
    // listing: git reports `/private/var/...` on macOS and `C:/...` on Windows
    // while a directory handle may say `/var/...` or use backslashes.
    Some(dunce::canonicalize(&root).unwrap_or(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_codes_map_to_tree_colours() {
        assert_eq!(classification(" M"), Status::Modified);
        assert_eq!(classification("M "), Status::Modified);
        assert_eq!(classification("??"), Status::Untracked);
        assert_eq!(classification("!!"), Status::Ignored);
        assert_eq!(classification("A "), Status::Added);
        assert_eq!(classification(" D"), Status::Deleted);
        assert_eq!(classification("R "), Status::Renamed);
        assert_eq!(classification("UU"), Status::Conflicted);
        assert_eq!(classification("AA"), Status::Conflicted);
    }

    #[test]
    fn a_directory_is_as_dirty_as_its_dirtiest_child() {
        let statuses = Statuses {
            root: PathBuf::from("/repo"),
            entries: BTreeMap::from([
                (PathBuf::from("src"), Status::Ignored),
                (PathBuf::from("src/lib.rs"), Status::Modified),
                (PathBuf::from("src/old.rs"), Status::Deleted),
                (PathBuf::from("srcfoo/x.rs"), Status::Conflicted),
            ]),
        };
        assert_eq!(
            statuses.find(Path::new("src"), true),
            Some(Status::Modified),
            "a change outranks a deletion in the same directory"
        );
        assert_eq!(
            statuses.of(Path::new("/repo/src/lib.rs"), false),
            Some(Status::Modified)
        );
        assert_eq!(
            statuses.of(Path::new("/repo/srcfoo/x.rs"), false),
            Some(Status::Conflicted)
        );
        assert_eq!(
            statuses.of(Path::new("/repo/src"), true),
            Some(Status::Modified)
        );
        assert_eq!(statuses.of(Path::new("/repo/README.md"), false), None);
        assert_eq!(
            statuses.of(Path::new("/elsewhere/src"), true),
            None,
            "a path outside the root has no status"
        );
    }
}
