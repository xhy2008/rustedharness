use std::path::{Component, Path, PathBuf};

use crate::error::{HarnessError, Result};

/// Directories that are never useful to browse or index.
pub const SKIP_DIRS: &[&str] = &["target", "node_modules", "dist", "build"];

/// R1–R4 path rules.
///
/// * R1 relative paths are anchored at the session cwd
/// * R2 shell invocations never inherit a persistent cwd (handled by proc)
/// * R3 outputs are displayed relative to the workspace root
/// * R4 any resolved path escaping the workspace root is rejected
#[derive(Debug, Clone)]
pub struct WorkspacePathResolver {
    cwd: PathBuf,
    root: PathBuf,
}

impl WorkspacePathResolver {
    pub fn new(cwd: impl Into<PathBuf>) -> Result<Self> {
        let cwd = cwd.into();
        let cwd = dunce::canonicalize(&cwd).map_err(|e| {
            HarnessError::tool(
                "E_INVALID_CWD",
                format!("cannot use session cwd {}: {e}", cwd.display()),
            )
        })?;
        Ok(Self {
            root: cwd.clone(),
            cwd,
        })
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user/model supplied path while enforcing R4.
    pub fn resolve(&self, input: &str) -> Result<PathBuf> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(self.cwd.clone());
        }
        let raw = Path::new(input);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.cwd.join(raw)
        };
        let lexical = normalize_lexically(&joined);
        let resolved = if lexical.exists() {
            dunce::canonicalize(&lexical).map_err(|e| {
                HarnessError::tool(
                    "E_IO",
                    format!("cannot resolve {}: {e}", self.display_rel(&lexical)),
                )
            })?
        } else {
            // For new files resolve the nearest existing ancestor and re-attach
            // the remaining components, following symlinks on the way.
            let (base, rest) = split_existing_ancestor(&lexical);
            let base = dunce::canonicalize(&base).unwrap_or(base);
            let candidate = rest.iter().fold(base, |p, c| p.join(c));
            candidate
        };
        if !resolved.starts_with(&self.root) {
            return Err(HarnessError::PathEscape {
                path: input.to_string(),
                root: self.display_rel(&self.root),
            });
        }
        Ok(resolved)
    }

    /// Resolve an existing path (file or directory).
    pub fn resolve_existing(&self, input: &str) -> Result<PathBuf> {
        let path = self.resolve(input)?;
        if !path.exists() {
            return Err(HarnessError::tool(
                "E_NOT_FOUND",
                format!("not found: {}", self.display_rel(&path)),
            ));
        }
        Ok(path)
    }

    /// Path for display: relative to the workspace root, forward slashes.
    pub fn display_rel(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let mut s = rel.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            s = ".".to_string();
        }
        s
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::Prefix(_) => out.push(comp.as_os_str()),
            Component::Normal(c) => out.push(c),
        }
    }
    out
}

fn split_existing_ancestor(path: &Path) -> (PathBuf, Vec<std::ffi::OsString>) {
    let mut base = path.to_path_buf();
    let mut rest = Vec::new();
    while !base.exists() {
        match base.file_name() {
            Some(name) => {
                rest.push(name.to_os_string());
                let parent = base.parent().map(|p| p.to_path_buf());
                match parent {
                    Some(p) if p != base => base = p,
                    _ => break,
                }
            }
            None => break,
        }
    }
    rest.reverse();
    (base, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace() -> (tempfile::TempDir, WorkspacePathResolver) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.txt"), "world").unwrap();
        let resolver = WorkspacePathResolver::new(dir.path()).unwrap();
        (dir, resolver)
    }

    #[test]
    fn r1_relative_paths_anchor_at_cwd() {
        let (dir, r) = workspace();
        assert_eq!(
            r.resolve("a.txt").unwrap(),
            dunce::canonicalize(dir.path()).unwrap().join("a.txt")
        );
        assert_eq!(r.display_rel(&r.resolve("sub/b.txt").unwrap()), "sub/b.txt");
    }

    #[test]
    fn r1_dot_resolves_to_cwd() {
        let (_d, r) = workspace();
        assert_eq!(r.resolve(".").unwrap(), r.cwd());
        assert_eq!(r.display_rel(&r.resolve("").unwrap()), ".");
    }

    #[test]
    fn r4_parent_escape_is_rejected() {
        let (_d, r) = workspace();
        let err = r.resolve("../outside.txt").unwrap_err();
        assert_eq!(err.code(), "E_PATH_ESCAPE");
        let err = r.resolve("sub/../../outside.txt").unwrap_err();
        assert_eq!(err.code(), "E_PATH_ESCAPE");
    }

    #[test]
    fn r4_absolute_escape_is_rejected() {
        let (_d, r) = workspace();
        let err = r.resolve("/etc/passwd").unwrap_err();
        assert_eq!(err.code(), "E_PATH_ESCAPE");
    }

    #[test]
    fn absolute_path_inside_root_is_allowed() {
        let (dir, r) = workspace();
        let inside = dunce::canonicalize(dir.path()).unwrap().join("a.txt");
        assert_eq!(r.resolve(inside.to_str().unwrap()).unwrap(), inside);
    }

    #[test]
    fn new_nested_file_resolves_inside_root() {
        let (dir, r) = workspace();
        let p = r.resolve("new/deep/file.txt").unwrap();
        assert!(p.starts_with(dunce::canonicalize(dir.path()).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;
        let (dir, r) = workspace();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "x").unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();
        let err = r.resolve("link/secret").unwrap_err();
        assert_eq!(err.code(), "E_PATH_ESCAPE");
    }

    #[test]
    fn display_rel_is_forward_slashed() {
        let (_d, r) = workspace();
        let p = r.resolve("sub/b.txt").unwrap();
        assert!(!r.display_rel(&p).contains('\\'));
    }
}
