//! L0 code intelligence: an incremental symbol index backed by SQLite and
//! real tree-sitter grammars. It is the transparent fallback whenever no LSP
//! server is available (or is still warming up) and is intentionally labelled
//! `name-based`.
//!
//! The index is purely syntactic: definitions come from language-specific AST
//! node kinds and references from identifier leaves. Semantics, types and
//! diagnostics still require a language server.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::Result;
use crate::lsp::LspOp;
use crate::syntax;

const MAX_FILES: usize = 20_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const SCAN_THROTTLE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize)]
pub struct IndexHit {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub name: String,
    pub kind: String,
    pub snippet: String,
}

pub struct CodeIndex {
    root: PathBuf,
    db: Mutex<Connection>,
    last_scan: Mutex<Option<Instant>>,
}

/// Directory holding the index inside the workspace.
///
/// The index describes this workspace: it goes stale with it and disappears
/// with it, so it lives next to the code instead of in the config directory.
pub const INDEX_DIR: &str = ".rustedharness";

/// Open the workspace-local database, creating the directory. A read-only
/// workspace falls back to an in-memory index at the call site.
fn open_index(root: &Path) -> Result<Connection> {
    let dir = root.join(INDEX_DIR);
    std::fs::create_dir_all(&dir)?;
    // The workspace's own `.gitignore` is not ours to edit, so keep this
    // directory out of `git status` from the inside.
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "*\n");
    }
    let conn = Connection::open(dir.join("index.sqlite"))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;",
    )?;
    Ok(conn)
}

impl CodeIndex {
    pub fn new(root: &Path) -> Arc<Self> {
        let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let conn = open_index(&root)
            .unwrap_or_else(|_| Connection::open_in_memory().expect("in-memory SQLite"));
        let _ = conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS files(
                 id INTEGER PRIMARY KEY,
                 path TEXT UNIQUE NOT NULL,
                 language TEXT NOT NULL,
                 mtime INTEGER NOT NULL,
                 size INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS defs(
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 line INTEGER NOT NULL,
                 col INTEGER NOT NULL,
                 snippet TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS refs(
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 line INTEGER NOT NULL,
                 col INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_defs_name ON defs(name);
             CREATE INDEX IF NOT EXISTS idx_refs_name ON refs(name);
             CREATE INDEX IF NOT EXISTS idx_defs_file ON defs(file_id);",
        );
        Arc::new(Self {
            root,
            db: Mutex::new(conn),
            last_scan: Mutex::new(None),
        })
    }

    #[cfg(test)]
    pub fn in_memory(root: &Path) -> Arc<Self> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, language TEXT NOT NULL, mtime INTEGER NOT NULL, size INTEGER NOT NULL);
             CREATE TABLE defs(id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL, line INTEGER NOT NULL, col INTEGER NOT NULL, snippet TEXT NOT NULL);
             CREATE TABLE refs(id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL, name TEXT NOT NULL, line INTEGER NOT NULL, col INTEGER NOT NULL);
             CREATE INDEX idx_defs_name ON defs(name);
             CREATE INDEX idx_refs_name ON refs(name);",
        )
        .unwrap();
        Arc::new(Self {
            root: root.to_path_buf(),
            db: Mutex::new(conn),
            last_scan: Mutex::new(None),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the database lives for a workspace, for diagnostics.
    pub fn db_path(root: &Path) -> PathBuf {
        root.join(INDEX_DIR).join("index.sqlite")
    }

    /// Mark a file stale after a write/edit. The next query re-parses it.
    pub fn invalidate(&self, path: &Path) {
        if let Ok(conn) = self.db.lock() {
            let rel = self.rel(path);
            let _ = conn.execute(
                "DELETE FROM defs WHERE file_id IN (SELECT id FROM files WHERE path = ?1)",
                params![rel],
            );
            let _ = conn.execute(
                "DELETE FROM refs WHERE file_id IN (SELECT id FROM files WHERE path = ?1)",
                params![rel],
            );
            let _ = conn.execute("DELETE FROM files WHERE path = ?1", params![rel]);
        }
    }

    /// Incremental workspace scan. Returns the number of (re)indexed files.
    pub fn refresh(&self) -> Result<usize> {
        {
            let last = self.last_scan.lock().unwrap();
            if let Some(at) = *last {
                if at.elapsed() < SCAN_THROTTLE {
                    return Ok(0);
                }
            }
        }
        let count = self.scan()?;
        *self.last_scan.lock().unwrap() = Some(Instant::now());
        Ok(count)
    }

    fn scan(&self) -> Result<usize> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut indexed = 0usize;
        let walker = ignore::WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .require_git(false)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    ".git"
                        | "target"
                        | "node_modules"
                        | ".venv"
                        | "venv"
                        | "dist"
                        | "build"
                        | ".next"
                        | "__pycache__"
                        | ".idea"
                        | ".cache"
                        | INDEX_DIR
                )
            })
            .build();
        for entry in walker.flatten() {
            if seen.len() >= MAX_FILES {
                break;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(language) = crate::lsp::language_for_path(path) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let rel = self.rel(path);
            seen.insert(rel.clone());
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let size = meta.len() as i64;
            let unchanged = {
                let conn = self.db.lock().unwrap();
                conn.query_row(
                    "SELECT mtime, size FROM files WHERE path = ?1",
                    params![rel],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .ok()
                .flatten()
                .map(|(m, s)| m == mtime && s == size)
                .unwrap_or(false)
            };
            if unchanged {
                continue;
            }
            if let Ok(()) = self.index_file(path, &rel, language, mtime, size) {
                indexed += 1;
            }
        }
        // Drop files that disappeared, but only after a complete walk.
        if seen.len() < MAX_FILES {
            if let Ok(conn) = self.db.lock() {
                if let Ok(mut stmt) = conn.prepare("SELECT id, path FROM files") {
                    let stale: Vec<(i64, String)> = stmt
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                        .map(|rows| rows.flatten().collect())
                        .unwrap_or_default();
                    for (id, path) in stale {
                        if !seen.contains(&path) {
                            let _ =
                                conn.execute("DELETE FROM defs WHERE file_id = ?1", params![id]);
                            let _ =
                                conn.execute("DELETE FROM refs WHERE file_id = ?1", params![id]);
                            let _ = conn.execute("DELETE FROM files WHERE id = ?1", params![id]);
                        }
                    }
                }
            }
        }
        Ok(indexed)
    }

    fn index_file(
        &self,
        path: &Path,
        rel: &str,
        language: &str,
        mtime: i64,
        size: i64,
    ) -> Result<()> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Ok(()),
        };
        // `.tsx` needs the TSX grammar even though the LSP registry treats it
        // as TypeScript.
        let grammar = if language == "typescript"
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("tsx"))
        {
            "tsx"
        } else {
            language
        };
        let syntax = syntax::parse_file(grammar, &text);
        let lines: Vec<&str> = text.lines().collect();
        let defs: Vec<(String, String, usize, usize, String)> = syntax
            .as_ref()
            .map(|parsed| {
                parsed
                    .definitions
                    .iter()
                    .map(|def| {
                        (
                            def.name.clone(),
                            def.kind.to_string(),
                            def.line,
                            def.column,
                            snippet_at(&lines, def.line),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let refs: Vec<(String, usize, usize)> = syntax
            .as_ref()
            .map(|parsed| {
                parsed
                    .references
                    .iter()
                    .map(|reference| (reference.name.clone(), reference.line, reference.column))
                    .collect()
            })
            .unwrap_or_default();

        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM defs WHERE file_id IN (SELECT id FROM files WHERE path = ?1)",
            params![rel],
        )?;
        tx.execute(
            "DELETE FROM refs WHERE file_id IN (SELECT id FROM files WHERE path = ?1)",
            params![rel],
        )?;
        tx.execute("DELETE FROM files WHERE path = ?1", params![rel])?;
        tx.execute(
            "INSERT INTO files(path, language, mtime, size) VALUES (?1, ?2, ?3, ?4)",
            params![rel, language, mtime, size],
        )?;
        let file_id = tx.last_insert_rowid();
        for (name, kind, line, col, snippet) in &defs {
            tx.execute(
                "INSERT INTO defs(file_id, name, kind, line, col, snippet) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![file_id, name, kind, *line as i64, *col as i64, snippet],
            )?;
        }
        for (name, line, col) in &refs {
            tx.execute(
                "INSERT INTO refs(file_id, name, line, col) VALUES (?1, ?2, ?3, ?4)",
                params![file_id, name, *line as i64, *col as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Indexed files per language, most common first. Used to decide which
    /// language server is worth starting before the first query arrives.
    pub fn language_counts(&self) -> Vec<(String, u64)> {
        let _ = self.refresh();
        let conn = self.db.lock().unwrap();
        let Ok(mut statement) =
            conn.prepare("SELECT language, COUNT(*) FROM files GROUP BY language ORDER BY 2 DESC")
        else {
            return Vec::new();
        };
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        });
        match rows {
            Ok(rows) => rows
                .flatten()
                .map(|(language, count)| (language, count.max(0) as u64))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn identifier_at(&self, path: &Path, line: u32, column: u32) -> Option<String> {
        let language = crate::lsp::language_for_path(path)?;
        let text = std::fs::read_to_string(path).ok()?;
        syntax::identifier_at(language, &text, line, column)
    }

    fn definitions(&self, name: &str, limit: usize) -> Vec<IndexHit> {
        let conn = self.db.lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT f.path, d.line, d.col, d.name, d.kind, d.snippet
             FROM defs d JOIN files f ON f.id = d.file_id
             WHERE d.name = ?1 ORDER BY f.path, d.line LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![name, limit as i64], |row| {
            Ok(IndexHit {
                path: row.get(0)?,
                line: row.get::<_, i64>(1)? as u32 + 1,
                column: row.get::<_, i64>(2)? as u32 + 1,
                name: row.get(3)?,
                kind: row.get(4)?,
                snippet: row.get(5)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    fn references(&self, name: &str, limit: usize) -> Vec<IndexHit> {
        let conn = self.db.lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT f.path, r.line, r.col, r.name
             FROM refs r JOIN files f ON f.id = r.file_id
             WHERE r.name = ?1 ORDER BY f.path, r.line LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![name, limit as i64], |row| {
            let path: String = row.get(0)?;
            Ok(IndexHit {
                path,
                line: row.get::<_, i64>(1)? as u32 + 1,
                column: row.get::<_, i64>(2)? as u32 + 1,
                name: row.get(3)?,
                kind: "ref".to_string(),
                snippet: String::new(),
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    fn file_symbols(&self, path: &Path, limit: usize) -> Vec<IndexHit> {
        let rel = self.rel(path);
        let conn = self.db.lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT f.path, d.line, d.col, d.name, d.kind, d.snippet
             FROM defs d JOIN files f ON f.id = d.file_id
             WHERE f.path = ?1 ORDER BY d.line LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![rel, limit as i64], |row| {
            Ok(IndexHit {
                path: row.get(0)?,
                line: row.get::<_, i64>(1)? as u32 + 1,
                column: row.get::<_, i64>(2)? as u32 + 1,
                name: row.get(3)?,
                kind: row.get(4)?,
                snippet: row.get(5)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    fn workspace_symbols(&self, query: &str, limit: usize) -> Vec<IndexHit> {
        let conn = self.db.lock().unwrap();
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let Ok(mut stmt) = conn.prepare(
            "SELECT f.path, d.line, d.col, d.name, d.kind, d.snippet
             FROM defs d JOIN files f ON f.id = d.file_id
             WHERE d.name LIKE ?1 ESCAPE '\\' ORDER BY d.name, f.path LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![pattern, limit as i64], |row| {
            Ok(IndexHit {
                path: row.get(0)?,
                line: row.get::<_, i64>(1)? as u32 + 1,
                column: row.get::<_, i64>(2)? as u32 + 1,
                name: row.get(3)?,
                kind: row.get(4)?,
                snippet: row.get(5)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// L0 query. Returns a formatted text block and structured JSON.
    pub fn query(
        &self,
        op: LspOp,
        path: &Path,
        line: u32,
        column: u32,
        symbol: Option<&str>,
        limit: usize,
    ) -> Result<(String, Value)> {
        let _ = self.refresh();
        let limit = limit.clamp(1, 200);
        let identifier = symbol
            .map(|s| s.to_string())
            .or_else(|| self.identifier_at(path, line, column));

        let (title, hits): (String, Vec<IndexHit>) = match op {
            LspOp::Definition | LspOp::TypeDefinition | LspOp::Implementation => {
                let Some(name) = identifier.clone() else {
                    return Ok((
                        "no symbol at position (name-based index)".to_string(),
                        json!({ "match": "name-based", "hits": [] }),
                    ));
                };
                (
                    format!("definitions of `{name}` (name-based):"),
                    self.definitions(&name, limit),
                )
            }
            LspOp::References => {
                let Some(name) = identifier.clone() else {
                    return Ok((
                        "no symbol at position (name-based index)".to_string(),
                        json!({ "match": "name-based", "hits": [] }),
                    ));
                };
                let mut hits = self.references(&name, limit);
                if hits.len() < limit {
                    hits.extend(self.definitions(&name, limit - hits.len()));
                    hits.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
                }
                (format!("references of `{name}` (name-based):"), hits)
            }
            LspOp::Hover => {
                let Some(name) = identifier.clone() else {
                    return Ok((
                        "no symbol at position (name-based index)".to_string(),
                        json!({ "match": "name-based", "hits": [] }),
                    ));
                };
                let defs = self.definitions(&name, limit);
                let text = if let Some(def) = defs.first() {
                    format!(
                        "{}:{}:{} {} {}\n```\n{}\n```",
                        def.path, def.line, def.column, def.kind, def.name, def.snippet
                    )
                } else {
                    format!("`{name}`: no definition found (name-based index)")
                };
                return Ok((
                    text,
                    json!({ "match": "name-based", "name": name, "hits": defs }),
                ));
            }
            LspOp::DocumentSymbols => (
                format!("symbols in {}", self.rel(path)),
                self.file_symbols(path, limit),
            ),
            LspOp::WorkspaceSymbols => {
                let query = symbol.unwrap_or("");
                (
                    format!("symbols matching `{query}` (name-based):"),
                    self.workspace_symbols(query, limit),
                )
            }
            LspOp::CallHierarchy => {
                let Some(name) = identifier.clone() else {
                    return Ok((
                        "no symbol at position (name-based index)".to_string(),
                        json!({ "match": "name-based", "hits": [] }),
                    ));
                };
                let mut hits = self.references(&name, limit);
                if hits.len() < limit {
                    hits.extend(self.definitions(&name, limit - hits.len()));
                }
                (format!("call sites of `{name}` (name-based):"), hits)
            }
            LspOp::Rename => {
                // `symbol` is the NEW name for renames; the preview must be
                // anchored on the symbol at the cursor position, never on the
                // requested target name.
                let name = self
                    .identifier_at(path, line, column)
                    .or_else(|| symbol.map(|s| s.to_string()));
                let Some(name) = name else {
                    return Ok((
                        "no symbol at position (name-based index)".to_string(),
                        json!({ "match": "name-based", "hits": [] }),
                    ));
                };
                let mut hits = self.definitions(&name, limit);
                if hits.len() < limit {
                    hits.extend(self.references(&name, limit - hits.len()));
                }
                hits.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
                hits.dedup_by(|a, b| a.path == b.path && a.line == b.line);
                let arrow = symbol.map(|s| format!(" → `{s}`")).unwrap_or_default();
                (
                    format!(
                        "rename `{name}`{arrow} preview (name-based only; install a language server to apply a precise workspace edit):"
                    ),
                    hits,
                )
            }
            LspOp::Diagnostics => {
                return Ok((
                    "no diagnostics (name-based index has no semantic analysis)".to_string(),
                    json!({ "match": "name-based", "diagnostics": [] }),
                ));
            }
            // Rejected above; listing them keeps the match exhaustive.
            LspOp::Install | LspOp::Restart => {
                return Err(crate::error::HarnessError::tool(
                    "E_LSP_INTERNAL",
                    "server management has no name-based equivalent",
                ))
            }
        };

        if hits.is_empty() {
            return Ok((
                format!("{title}\nno matches"),
                json!({ "match": "name-based", "hits": [] }),
            ));
        }
        let more = hits.len() == limit;
        let lines: Vec<String> = hits
            .iter()
            .map(|hit| {
                if hit.snippet.is_empty() {
                    format!(
                        "{}:{}:{} {} {}",
                        hit.path, hit.line, hit.column, hit.kind, hit.name
                    )
                } else {
                    format!(
                        "{}:{}:{} {} {}\n    {}",
                        hit.path, hit.line, hit.column, hit.kind, hit.name, hit.snippet
                    )
                }
            })
            .collect();
        let mut text = format!("{title}\n{}", lines.join("\n"));
        if more {
            text.push_str("\n... (limit reached)");
        }
        Ok((
            text,
            json!({ "match": "name-based", "name": identifier, "hits": hits }),
        ))
    }
}

fn snippet_at(lines: &[&str], line: usize) -> String {
    let raw = lines.get(line).copied().unwrap_or("").trim();
    let mut snippet = raw.to_string();
    if snippet.chars().count() > 160 {
        snippet = snippet.chars().take(157).collect::<String>() + "...";
    }
    snippet
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(root: &Path, rel: &str, content: &str) -> PathBuf {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    /// The index belongs to the workspace it describes: it is created inside
    /// it, ignores itself, and never grows into the config directory.
    #[test]
    fn the_index_lives_in_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "src/lib.rs",
            "pub fn alpha() {}
",
        );
        let index = CodeIndex::new(dir.path());
        index.refresh().unwrap();

        let db = dir.path().join(INDEX_DIR).join("index.sqlite");
        assert!(db.is_file(), "{} was not created", db.display());
        assert_eq!(CodeIndex::db_path(dir.path()), db);
        let ignore = std::fs::read_to_string(dir.path().join(INDEX_DIR).join(".gitignore"));
        assert_eq!(ignore.unwrap().trim(), "*", "the directory ignores itself");

        // Its own files must not be walked into the index.
        let counts = index.language_counts();
        assert_eq!(counts.iter().map(|(_, n)| n).sum::<u64>(), 1, "{counts:?}");
    }

    #[test]
    fn indexes_definitions_and_references() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write(
            dir.path(),
            "src/lib.rs",
            "pub fn alpha() {}\nstruct Beta;\nfn caller() { alpha(); }\n",
        );
        let py = write(
            dir.path(),
            "app/main.py",
            "def gamma():\n    pass\ngamma()\n",
        );
        let index = CodeIndex::in_memory(dir.path());
        index.refresh().unwrap();

        let (text, _) = index
            .query(LspOp::Definition, &lib, 1, 8, None, 10)
            .unwrap();
        assert!(text.contains("definitions of `alpha`"));
        assert!(text.contains("src/lib.rs:1:8"));

        let (text, _) = index
            .query(LspOp::References, &lib, 3, 15, None, 10)
            .unwrap();
        assert!(text.contains("references of `alpha`"));
        assert!(text.contains("caller") || text.contains("src/lib.rs:3"));

        let (text, _) = index
            .query(LspOp::DocumentSymbols, &py, 1, 1, None, 10)
            .unwrap();
        assert!(text.contains("gamma"));

        let (text, _) = index
            .query(LspOp::WorkspaceSymbols, &lib, 1, 1, Some("Beta"), 10)
            .unwrap();
        assert!(text.contains("Beta"));
    }

    #[test]
    fn incremental_refresh_picks_up_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "a.rs", "fn one() {}\n");
        let index = CodeIndex::in_memory(dir.path());
        index.refresh().unwrap();
        assert!(index.definitions("one", 10).len() == 1);
        std::thread::sleep(Duration::from_millis(5));
        write(dir.path(), "a.rs", "fn two() {}\n");
        index.last_scan.lock().unwrap().take();
        index.refresh().unwrap();
        assert!(index.definitions("one", 10).is_empty());
        assert_eq!(index.definitions("two", 10).len(), 1);
        let _ = file;
    }

    #[test]
    fn identifier_at_handles_cursor_edges() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "a.rs", "  alpha_beta() {}\n");
        let index = CodeIndex::in_memory(dir.path());
        assert_eq!(
            index.identifier_at(&file, 1, 6).as_deref(),
            Some("alpha_beta")
        );
        assert_eq!(
            index.identifier_at(&file, 1, 12).as_deref(),
            Some("alpha_beta")
        );
        assert!(index.identifier_at(&file, 1, 1).is_none());
        assert!(index.identifier_at(&file, 1, 14).is_none());
    }
}
