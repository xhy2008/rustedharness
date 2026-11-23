use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::WalkBuilder;
use rayon::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{HarnessError, Result};
use crate::hashline;
use crate::tools::{parse_args, schema_of, Tool, ToolContext, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    Content,
    FilesWithMatches,
    Count,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    pub pattern: String,
    /// File or directory scope (default ".").
    pub path: Option<String>,
    /// Path glob filter, e.g. "**/*.rs".
    pub glob: Option<String>,
    pub case_insensitive: Option<bool>,
    /// Context lines before/after (0-5).
    pub context: Option<usize>,
    /// Max returned matches (default 50, hard max 200).
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub output_mode: Option<OutputMode>,
    /// Emit `file:line#anchor|content` so matches can feed edit directly;
    /// pass only `line#anchor` to edit, not the matched line.
    pub anchors: Option<bool>,
    /// Treat pattern as a literal string.
    pub literal: Option<bool>,
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> String {
        "Search file contents with gitignore-aware traversal. Modes: content \
         (default), files_with_matches, count. Set anchors=true to get `file:line#anchor` \
         prefixes; pass only the `line#anchor` part to `edit` as pos/end, never the \
         matched line itself. Output paths are relative to the session cwd."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<GrepArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: GrepArgs = parse_args(args)?;
        if args.pattern.is_empty() {
            return Err(HarnessError::tool(
                "E_INVALID_ARGUMENT",
                "pattern must not be empty",
            ));
        }
        let limit = args.limit.unwrap_or(50).clamp(1, 200);
        let offset = args.offset.unwrap_or(0);
        let context = args.context.unwrap_or(0).min(5);
        let output_mode = args.output_mode.unwrap_or(OutputMode::Content);
        let literal = args.literal.unwrap_or(false);
        let anchors = args.anchors.unwrap_or(false) && output_mode == OutputMode::Content;
        let mut matcher_builder = RegexMatcherBuilder::new();
        matcher_builder.case_insensitive(args.case_insensitive.unwrap_or(false));
        let matcher = if literal {
            matcher_builder.build_literals(&[args.pattern.as_str()])
        } else {
            matcher_builder.build(&args.pattern)
        }
        .map_err(|e| HarnessError::tool("E_INVALID_PATTERN", e.to_string()))?;

        let root = ctx.resolver.resolve(args.path.as_deref().unwrap_or("."))?;
        let scope_root = if root.is_file() {
            root.parent().unwrap_or(&root).to_path_buf()
        } else {
            root.clone()
        };
        let globset = args.glob.as_deref().map(build_globset).transpose()?;

        let files = if root.is_file() {
            vec![root.clone()]
        } else {
            collect_files(&root, &scope_root, globset.as_ref())
        };

        let per_file: Vec<(String, u64, Vec<FileHits>)> = files
            .par_iter()
            .filter_map(|path| {
                let rel = ctx.rel(path);
                let mut sink = GrepSink {
                    path: rel.clone(),
                    hits: Vec::new(),
                    pending_before: Vec::new(),
                    binary: false,
                };
                let mut searcher = SearcherBuilder::new();
                searcher
                    .line_number(true)
                    .before_context(context)
                    .after_context(context)
                    .binary_detection(BinaryDetection::quit(b'\0'))
                    .build()
                    .search_path(&matcher, path, &mut sink)
                    .ok()?;
                if sink.binary || sink.hits.is_empty() {
                    return None;
                }
                if anchors {
                    let text = std::fs::read_to_string(path).ok()?;
                    let lines: Vec<&str> = text.lines().collect();
                    let anchors = hashline::anchors(&lines, ctx.config.edit.anchor_width);
                    for hit in &mut sink.hits {
                        hit.anchor = anchors.get(hit.line - 1).cloned();
                    }
                }
                let mtime = std::fs::metadata(path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Some((rel, mtime, sink.hits))
            })
            .collect();

        let mut per_file = per_file;
        per_file.sort_by(|a, b| a.0.cmp(&b.0));
        let total: usize = per_file.iter().map(|(_, _, hits)| hits.len()).sum();
        let mut flat: Vec<&FileHits> = Vec::new();
        for (_, _, hits) in &per_file {
            for hit in hits {
                flat.push(hit);
            }
        }
        let selected: Vec<&&FileHits> = flat.iter().skip(offset).take(limit).collect();

        let mut body = String::new();
        match output_mode {
            OutputMode::FilesWithMatches => {
                for (path, _, _) in &per_file {
                    body.push_str(path);
                    body.push('\n');
                }
            }
            OutputMode::Count => {
                for (path, _, hits) in &per_file {
                    body.push_str(&format!("{path}:{}\n", hits.len()));
                }
            }
            OutputMode::Content => {
                body.push_str(&content_body(&selected, context));
            }
        }
        if body.is_empty() {
            body.push_str("(no matches)\n");
        }
        if matches!(output_mode, OutputMode::Content) && (total > limit || offset > 0) {
            body.push_str(&format!(
                "[shown {} of {} matches; use offset/limit to page]\n",
                selected.len(),
                total
            ));
        }

        Ok(ToolOutput::ok(body).with_metadata(json!({
            "matches": total,
            "returned": selected.len(),
            "files": per_file.len(),
            "mode": match output_mode {
                OutputMode::Content => "content",
                OutputMode::FilesWithMatches => "files_with_matches",
                OutputMode::Count => "count",
            },
        })))
    }
}

struct GrepSink {
    path: String,
    hits: Vec<FileHits>,
    pending_before: Vec<(usize, String)>,
    binary: bool,
}

impl Sink for GrepSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> std::io::Result<bool> {
        let Some(line) = mat.line_number().map(|line| line as usize) else {
            return Ok(false);
        };
        self.hits.push(FileHits {
            path: self.path.clone(),
            line,
            text: line_text(mat.bytes()),
            before: std::mem::take(&mut self.pending_before),
            after: Vec::new(),
            anchor: None,
        });
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        context: &SinkContext<'_>,
    ) -> std::io::Result<bool> {
        let Some(line) = context.line_number().map(|line| line as usize) else {
            return Ok(true);
        };
        let text = line_text(context.bytes());
        match context.kind() {
            SinkContextKind::Before => self.pending_before.push((line, text)),
            SinkContextKind::After => {
                if let Some(hit) = self.hits.last_mut() {
                    hit.after.push((line, text));
                }
            }
            SinkContextKind::Other => {}
        }
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &grep_searcher::Searcher) -> std::io::Result<bool> {
        self.pending_before.clear();
        Ok(true)
    }

    fn binary_data(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        _offset: u64,
    ) -> std::io::Result<bool> {
        self.binary = true;
        Ok(false)
    }
}

fn line_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

#[derive(Debug, Clone)]
struct FileHits {
    path: String,
    line: usize,
    text: String,
    before: Vec<(usize, String)>,
    after: Vec<(usize, String)>,
    anchor: Option<String>,
}

/// Render content-mode output. Collect every line each hit wants to show,
/// then merge by (path, line number) so overlapping context blocks render
/// exactly once. Hit lines keep `:` separators and beat context lines.
fn content_body(hits: &[&&FileHits], context: usize) -> String {
    let mut body = String::new();
    let mut rows: Vec<(&str, usize, String, bool)> = Vec::new();
    let mut anchors: BTreeMap<(&str, usize), &String> = BTreeMap::new();
    for hit in hits {
        let path = hit.path.as_str();
        if let Some(anchor) = &hit.anchor {
            anchors.insert((path, hit.line), anchor);
        }
        if context > 0 {
            for (line, text) in &hit.before {
                rows.push((path, *line, text.clone(), false));
            }
        }
        rows.push((path, hit.line, hit.text.clone(), true));
        if context > 0 {
            for (line, text) in &hit.after {
                rows.push((path, *line, text.clone(), false));
            }
        }
    }
    rows.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));
    let mut merged: Vec<(&str, usize, String, bool)> = Vec::new();
    for row in rows {
        if let Some(last) = merged.last_mut() {
            if last.0 == row.0 && last.1 == row.1 {
                last.3 |= row.3;
                continue;
            }
        }
        merged.push(row);
    }
    let mut prev: Option<(&str, usize)> = None;
    for (path, line, text, is_hit) in merged {
        if let Some((prev_path, prev_line)) = prev {
            if prev_path == path && line > prev_line + 1 {
                body.push_str("--\n");
            }
        }
        match (is_hit, anchors.get(&(path, line))) {
            (true, Some(anchor)) => body.push_str(&format!("{path}:{line}#{anchor}|{text}\n")),
            (true, None) => body.push_str(&format!("{path}:{line}:{text}\n")),
            (false, _) => body.push_str(&format!("{path}-{line}-{text}\n")),
        }
        prev = Some((path, line));
    }
    body
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GlobArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub limit: Option<usize>,
}

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> String {
        "Find files by gitignore-aware path glob (e.g. \"**/*.rs\"), sorted by \
         modification time (newest first). Complements grep for structural discovery."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<GlobArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: GlobArgs = parse_args(args)?;
        let root = ctx.resolver.resolve(args.path.as_deref().unwrap_or("."))?;
        let scope_root = if root.is_file() {
            root.parent().unwrap_or(&root).to_path_buf()
        } else {
            root.clone()
        };
        let globset = build_globset(&args.pattern)?;
        let limit = args.limit.unwrap_or(200).clamp(1, 2_000);
        let mut entries: Vec<(PathBuf, u64)> = Vec::new();
        let walker = WalkBuilder::new(&root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .build();
        for entry in walker.flatten() {
            let path = entry.path();
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rel = path.strip_prefix(&scope_root).unwrap_or(path);
            if globset.is_match(rel) || globset.is_match(path) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                entries.push((path.to_path_buf(), mtime));
            }
        }
        entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let total = entries.len();
        let mut body = String::new();
        for (path, _) in entries.iter().take(limit) {
            body.push_str(&ctx.rel(path));
            body.push('\n');
        }
        if body.is_empty() {
            body.push_str("(no files matched)\n");
        }
        if total > limit {
            body.push_str(&format!("[showing {limit} of {total} files]\n"));
        }
        Ok(ToolOutput::ok(body).with_metadata(json!({
            "files": total,
            "returned": total.min(limit),
        })))
    }
}

fn build_globset(pattern: &str) -> Result<GlobSet> {
    let glob =
        Glob::new(pattern).map_err(|e| HarnessError::tool("E_INVALID_GLOB", e.to_string()))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    builder
        .build()
        .map_err(|e| HarnessError::tool("E_INVALID_GLOB", e.to_string()))
}

fn collect_files(root: &Path, scope_root: &Path, globset: Option<&GlobSet>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(globset) = globset {
            let rel = path.strip_prefix(scope_root).unwrap_or(path);
            if !globset.is_match(rel) && !globset.is_match(path) {
                continue;
            }
        }
        files.push(path.to_path_buf());
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn content_body_merges_overlapping_context_blocks() {
        let h1 = FileHits {
            path: "dup.txt".into(),
            line: 1,
            text: "alpha beta".into(),
            before: vec![],
            after: vec![(2, "gamma alpha".into())],
            anchor: None,
        };
        let h2 = FileHits {
            path: "dup.txt".into(),
            line: 2,
            text: "gamma alpha".into(),
            before: vec![(1, "alpha beta".into())],
            after: vec![(3, "delta".into())],
            anchor: None,
        };
        let hits: Vec<&FileHits> = vec![&h1, &h2];
        let refs: Vec<&&FileHits> = hits.iter().collect();
        let out = content_body(&refs, 1);
        assert_eq!(
            out,
            "dup.txt:1:alpha beta\ndup.txt:2:gamma alpha\ndup.txt-3-delta\n"
        );
    }

    #[test]
    fn content_body_separates_disjoint_blocks() {
        let h1 = FileHits {
            path: "a.txt".into(),
            line: 1,
            text: "one".into(),
            before: vec![],
            after: vec![],
            anchor: None,
        };
        let h2 = FileHits {
            path: "a.txt".into(),
            line: 5,
            text: "five".into(),
            before: vec![],
            after: vec![],
            anchor: None,
        };
        let hits: Vec<&FileHits> = vec![&h1, &h2];
        let refs: Vec<&&FileHits> = hits.iter().collect();
        let out = content_body(&refs, 0);
        assert_eq!(out, "a.txt:1:one\n--\na.txt:5:five\n");
    }

    #[test]
    fn searcher_collects_matches_and_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "one\ntwo alpha\nthree\nfour\nalpha five\n").unwrap();
        let matcher = RegexMatcherBuilder::new().build("alpha").unwrap();
        let mut sink = GrepSink {
            path: "a.txt".into(),
            hits: Vec::new(),
            pending_before: Vec::new(),
            binary: false,
        };
        SearcherBuilder::new()
            .line_number(true)
            .before_context(1)
            .after_context(1)
            .build()
            .search_path(&matcher, &path, &mut sink)
            .unwrap();
        assert!(!sink.binary);
        assert_eq!(sink.hits.len(), 2);
        assert_eq!(sink.hits[0].line, 2);
        assert_eq!(sink.hits[0].before, vec![(1, "one".to_string())]);
        assert_eq!(sink.hits[0].after, vec![(3, "three".to_string())]);
        assert_eq!(sink.hits[1].line, 5);
    }

    #[test]
    fn globset_matches_relative_paths() {
        let set = build_globset("**/*.rs").unwrap();
        assert!(set.is_match("src/main.rs"));
        assert!(!set.is_match("README.md"));
    }

    #[test]
    fn collect_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(dir.path().join("ignored")).unwrap();
        fs::write(dir.path().join("ignored/a.rs"), "x").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/b.rs"), "x").unwrap();
        let files = collect_files(dir.path(), dir.path(), None);
        assert!(files.iter().any(|p| p.ends_with("src/b.rs")));
        assert!(!files.iter().any(|p| p.ends_with("ignored/a.rs")));
    }
}
