use std::path::PathBuf;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use similar::TextDiff;

use crate::error::{HarnessError, Result};
use crate::hashline::{self, PosSpec};
use crate::tools::fs_tools::atomic_write;
use crate::tools::{append_diagnostics, parse_args, schema_of, Tool, ToolContext, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EditOpKind {
    Replace,
    InsertBefore,
    InsertAfter,
    Delete,
    ReplaceText,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EditOpArgs {
    pub op: EditOpKind,
    /// 1-based line, optionally carrying an anchor: `12` or `"12#AB"`.
    pub pos: Value,
    /// Last line for replace/delete (defaults to pos).
    pub end: Option<Value>,
    /// Replacement/inserted raw lines. Omit for delete. Never include a
    /// `line#anchor|` prefix or unified-diff `+/-` markers.
    pub lines: Option<Vec<String>>,
    /// Substring for op=replace_text. Must be unique unless `all` is set.
    #[serde(rename = "oldText")]
    pub old_text: Option<String>,
    #[serde(rename = "newText")]
    pub new_text: Option<String>,
    /// op=replace_text only: replace every occurrence instead of requiring a
    /// unique one.
    pub all: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EditFileArgs {
    pub path: String,
    pub edits: Vec<EditOpArgs>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EditArgs {
    /// One entry per file. A refactor that spans several files is one call and
    /// one decision: either every file's batch validates or nothing is written.
    pub files: Vec<EditFileArgs>,
    /// Validate and produce a diff without touching disk.
    pub dry_run: Option<bool>,
}

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> String {
        "Patch one or more files: {files: [{path, edits: [...]}], dry_run?}. Each edit \
         is {op: replace|insert_before|insert_after|delete|replace_text, pos: line or \
         \"line#anchor\", end?, lines?, oldText?, newText?, all?}. Line numbers choose \
         the position and refer to the file as it is on disk now - the same snapshot \
         for every edit in the batch - and an anchor verifies that line's current \
         content. Edits are applied bottom-up, so a later edit may address a line above \
         one already patched without adjusting numbers. Any stale anchor rejects the \
         whole call with E_STALE_ANCHOR and the current anchor table; every file in the \
         call is validated before any is written, so a multi-file call is all or \
         nothing. replace_text uses oldText/newText, needs a unique oldText unless \
         all=true, and must be the only edit for that file. `lines[]` is the new \
         content only: never paste the `line#anchor|` prefix from `read`/`grep`, or \
         unified-diff `+/-` markers, into it. Anchors belong only in `pos`/`end` as \
         `\"12#AB\"`. Returns a unified diff per file, fresh anchors, and LSP \
         diagnostics when available. dry_run=true writes nothing."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<EditArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: EditArgs = parse_args(args)?;
        if args.files.is_empty() {
            return Err(HarnessError::tool("E_INVALID_PATCH", "files[] is empty"));
        }
        let dry_run = args.dry_run.unwrap_or(false);
        let prepared = prepare_files(ctx, &args.files)?;

        if prepared.iter().all(|file| !file.applied.changed) {
            return no_changes(ctx, &prepared, dry_run);
        }
        for file in prepared.iter().filter(|file| file.applied.changed) {
            ctx.noop.reset(&file.rel);
        }
        if dry_run {
            let mut content = String::from("dry_run: no files were written\n");
            for file in &prepared {
                content.push_str(&render(file));
            }
            let files: Vec<Value> = prepared
                .iter()
                .map(|file| {
                    json!({
                        "path": file.rel,
                        "changed": file.applied.changed,
                        "lines_added": file.applied.lines_added,
                        "lines_removed": file.applied.lines_removed,
                    })
                })
                .collect();
            return Ok(ToolOutput::ok(content).with_metadata(json!({
                "changed": true,
                "dry_run": true,
                "files": files,
            })));
        }

        // Every file is validated before any is written, and a file that changed
        // on disk since it was read aborts the whole call: a multi-file refactor
        // must not leave the workspace half-patched.
        for file in &prepared {
            let current = std::fs::read(&file.path)?;
            if current != file.raw {
                return Err(HarnessError::tool(
                    "E_STALE_SNAPSHOT",
                    format!(
                        "{} changed on disk after the snapshot was read; re-read it and retry",
                        file.rel
                    ),
                ));
            }
        }
        let mut content = String::new();
        let mut files: Vec<Value> = Vec::new();
        for file in prepared.iter().filter(|file| file.applied.changed) {
            let applied = &file.applied;
            let new_text = reconstruct(&applied.new_lines, applied.crlf, applied.trailing_newline);
            atomic_write(&file.path, new_text.as_bytes(), true)?;
            content.push_str(&render(file));
            let diagnostics = append_diagnostics(ctx, &file.path, &mut content).await;
            files.push(json!({
                "path": file.rel,
                "lines_added": applied.lines_added,
                "lines_removed": applied.lines_removed,
                "diagnostics": diagnostics,
            }));
        }
        Ok(ToolOutput::ok(content).with_metadata(json!({
            "changed": true,
            "dry_run": false,
            "files": files,
        })))
    }
}

/// One file of a prepared batch: read, decoded and validated, but not written.
struct PreparedFile {
    path: PathBuf,
    rel: String,
    raw: Vec<u8>,
    applied: Applied,
}

/// Read and validate every file of the call. Nothing touches disk here, which is
/// what makes a batch all-or-nothing.
fn prepare_files(ctx: &ToolContext, files: &[EditFileArgs]) -> Result<Vec<PreparedFile>> {
    let width = ctx.config.edit.anchor_width;
    let mut prepared = Vec::with_capacity(files.len());
    for file in files {
        if file.edits.is_empty() {
            return Err(HarnessError::tool(
                "E_INVALID_PATCH",
                format!("{}: edits[] is empty", file.path),
            ));
        }
        if let Some(max) = ctx.config.edit.max_ops_per_call {
            if file.edits.len() > max {
                return Err(HarnessError::tool(
                    "E_INVALID_PATCH",
                    format!(
                        "{}: {} edits supplied but edit.max_ops_per_call={max}; send at most \
                         {max} edit(s) per file",
                        file.path,
                        file.edits.len()
                    ),
                ));
            }
        }
        let path = ctx.resolver.resolve_existing(&file.path)?;
        let rel = ctx.rel(&path);
        if path.is_dir() {
            return Err(HarnessError::tool(
                "E_IS_DIR",
                format!("{rel} is a directory"),
            ));
        }
        let raw = std::fs::read(&path)?;
        if crate::tools::fs_tools::is_binary(&raw) {
            return Err(HarnessError::tool(
                "E_BINARY",
                format!("refusing to edit binary file {rel}"),
            ));
        }
        let original = std::str::from_utf8(&raw).map_err(|_| {
            // `is_binary` already rejects invalid UTF-8; keep this explicit so
            // the edit path never turns undecodable bytes into replacement
            // characters.
            HarnessError::tool("E_BINARY", format!("refusing to edit non-UTF-8 file {rel}"))
        })?;
        let applied = apply_edits(
            original,
            &file.edits,
            width,
            ctx.config.edit.enable_replace_text,
        )?;
        prepared.push(PreparedFile {
            path,
            rel,
            raw,
            applied,
        });
    }
    Ok(prepared)
}

/// Diff plus the anchors the next edit can address, tagged with the file it
/// belongs to.
fn render(file: &PreparedFile) -> String {
    let applied = &file.applied;
    if !applied.changed {
        return format!("=== {} ===\nno changes\n", file.rel);
    }
    format!(
        "=== {} ===\n{}\n--- Anchors {}-{} ---\n{}",
        file.rel, applied.diff, applied.anchor_start, applied.anchor_end, applied.anchor_table
    )
}

/// A batch that left every file alone is reported but writes nothing. Repeating
/// the same one is a loop the model has to break itself.
fn no_changes(ctx: &ToolContext, prepared: &[PreparedFile], dry_run: bool) -> Result<ToolOutput> {
    let mut content = format!("no changes (dry_run={dry_run})\n");
    let mut counts = Vec::new();
    for file in prepared {
        let fingerprint = format!("{:x}", xxhash_rust::xxh32::xxh32(file.rel.as_bytes(), 0));
        counts.push(ctx.noop.record(&file.rel, &fingerprint));
        content.push_str(&format!(
            "=== {} ===\nno changes\n--- Anchors {}# ---\n{}",
            file.rel, file.applied.anchor_start, file.applied.old_anchor_table
        ));
    }
    let repeated = counts.iter().copied().max().unwrap_or(0);
    if repeated >= 3 {
        return Err(HarnessError::tool(
            "E_NOOP_LOOP",
            format!(
                "the same no-op edit was applied {repeated} times in a row; stop repeating it and \
                 inspect the current file with `read`"
            ),
        ));
    }
    Ok(ToolOutput::ok(content).with_metadata(json!({ "changed": false })))
}

#[derive(Debug)]
pub struct Applied {
    pub new_lines: Vec<String>,
    pub diff: String,
    pub changed: bool,
    pub anchor_start: usize,
    pub anchor_end: usize,
    pub anchor_table: String,
    pub old_anchor_table: String,
    pub crlf: bool,
    pub trailing_newline: bool,
    pub lines_added: usize,
    pub lines_removed: usize,
}

#[derive(Debug, Clone)]
enum NormOp {
    Replace {
        start: usize,
        end: usize,
        lines: Vec<String>,
    },
    Delete {
        start: usize,
        end: usize,
    },
    InsertBefore {
        at: usize,
        lines: Vec<String>,
    },
    InsertAfter {
        at: usize,
        lines: Vec<String>,
    },
}

impl NormOp {
    fn touch_range(&self) -> (usize, usize) {
        match self {
            NormOp::Replace { start, end, .. } | NormOp::Delete { start, end } => (*start, *end),
            NormOp::InsertBefore { at, .. } | NormOp::InsertAfter { at, .. } => (*at, *at),
        }
    }

    /// Lines are inserted between `slot - 1` and `slot`. Both
    /// `insert_after N` and `insert_before N + 1` address the same slot.
    fn insertion_slot(&self) -> Option<usize> {
        match self {
            NormOp::InsertBefore { at, .. } => Some(*at),
            NormOp::InsertAfter { at, .. } => Some(at.saturating_add(1)),
            NormOp::Replace { .. } | NormOp::Delete { .. } => None,
        }
    }
}

/// Pure batch-validation + application, also used by the unit tests.
pub fn apply_edits(
    original: &str,
    edits: &[EditOpArgs],
    width: usize,
    enable_replace_text: bool,
) -> Result<Applied> {
    let crlf = original.contains("\r\n");
    let trailing_newline = original.ends_with('\n');
    // `str::lines` already removes CRLF; keep any embedded CR intact so an
    // edit round-trips the file exactly as `read` saw it.
    let original_lines: Vec<String> = original.lines().map(str::to_string).collect();
    let total = original_lines.len();
    let refs: Vec<&str> = original_lines.iter().map(|s| s.as_str()).collect();
    let anchors = hashline::anchors(&refs, width);

    let replace_text_count = edits
        .iter()
        .filter(|e| e.op == EditOpKind::ReplaceText)
        .count();
    if replace_text_count > 0 {
        if !enable_replace_text {
            return Err(HarnessError::tool(
                "E_INVALID_PATCH",
                "replace_text is disabled by edit.enable_replace_text=false",
            ));
        }
        if edits.len() > 1 {
            return Err(HarnessError::tool(
                "E_INVALID_PATCH",
                "replace_text cannot be combined with other edits in one call",
            ));
        }
        let op = &edits[0];
        let old = op.old_text.as_deref().unwrap_or("");
        let new = op.new_text.as_deref().unwrap_or("");
        if old.is_empty() {
            return Err(HarnessError::tool(
                "E_INVALID_PATCH",
                "replace_text requires a non-empty oldText",
            ));
        }
        let hits = original.matches(old).count();
        let every = op.all.unwrap_or(false);
        if hits == 0 || (!every && hits != 1) {
            return Err(HarnessError::tool_details(
                "E_INVALID_PATCH",
                if every {
                    "oldText matched nothing".to_string()
                } else {
                    format!("oldText must match exactly once; found {hits} matches")
                },
                json!({ "matches": hits }),
            ));
        }
        let new_text = if every {
            original.replace(old, new)
        } else {
            original.replacen(old, new, 1)
        };
        let new_lines: Vec<String> = new_text.lines().map(|s| s.to_string()).collect();
        let changed = new_text != original;
        let (start, end) = changed_range(&original_lines, &new_lines);
        let diff = unified_diff(original, &new_text);
        let (anchor_start, anchor_end, table) = anchor_block(&new_lines, start, end, width);
        return Ok(Applied {
            new_lines,
            diff,
            changed,
            anchor_start,
            anchor_end,
            anchor_table: table,
            old_anchor_table: anchor_table(&original_lines, 1, total, width),
            crlf,
            trailing_newline,
            lines_added: new_text.lines().count().saturating_sub(total),
            lines_removed: total.saturating_sub(new_text.lines().count()),
        });
    }

    let mut normalized = Vec::with_capacity(edits.len());
    let mut min_touched = usize::MAX;
    let mut max_touched = 0usize;
    for edit in edits {
        let pos = PosSpec::parse(&edit.pos)
            .map_err(|e| HarnessError::tool("E_INVALID_PATCH", format!("invalid pos: {e}")))?;
        let end_pos =
            match &edit.end {
                Some(value) => Some(PosSpec::parse(value).map_err(|e| {
                    HarnessError::tool("E_INVALID_PATCH", format!("invalid end: {e}"))
                })?),
                None => None,
            };
        if edit.op != EditOpKind::Delete && edit.lines.is_none() {
            return Err(HarnessError::tool(
                "E_INVALID_PATCH",
                format!("op={:?} requires lines[]", edit.op),
            ));
        }
        if edit.op == EditOpKind::Delete
            && edit.lines.as_ref().is_some_and(|lines| !lines.is_empty())
        {
            return Err(HarnessError::tool(
                "E_INVALID_PATCH",
                "op=delete must not include lines[]",
            ));
        }
        // Anchor validation against the frozen pre-edit snapshot.
        let mut stale = Vec::new();
        for spec in [Some(&pos), end_pos.as_ref()].into_iter().flatten() {
            if let Some(expected) = &spec.anchor {
                if spec.line < 1 || spec.line > total {
                    return Err(HarnessError::tool(
                        "E_INVALID_PATCH",
                        format!("line {} is outside the file (1..={total})", spec.line),
                    ));
                }
                let actual = &anchors[spec.line - 1];
                if actual != expected {
                    stale.push(json!({
                        "line": spec.line,
                        "expected": expected,
                        "actual": actual,
                        "content": original_lines[spec.line - 1],
                    }));
                }
            }
        }
        if !stale.is_empty() {
            let (lo, hi) = anchor_window(&pos, end_pos.as_ref(), total);
            return Err(HarnessError::tool_details(
                "E_STALE_ANCHOR",
                "one or more anchors no longer match the file; the whole batch was rejected and \
                 nothing was written. Re-read the file and use the anchors below.",
                json!({
                    "stale": stale,
                    "anchors": anchor_block(&original_lines, lo, hi, width).2,
                }),
            ));
        }

        let at = pos.line;
        let lines = edit.lines.clone().unwrap_or_default();
        let op = match edit.op {
            EditOpKind::Replace => {
                let end = end_pos.as_ref().map(|e| e.line).unwrap_or(at);
                if at < 1 || end < at || end > total {
                    return Err(HarnessError::tool(
                        "E_INVALID_PATCH",
                        format!("replace range {at}..={end} is outside 1..={total}"),
                    ));
                }
                NormOp::Replace {
                    start: at,
                    end,
                    lines,
                }
            }
            EditOpKind::Delete => {
                let end = end_pos.as_ref().map(|e| e.line).unwrap_or(at);
                if at < 1 || end < at || end > total {
                    return Err(HarnessError::tool(
                        "E_INVALID_PATCH",
                        format!("delete range {at}..={end} is outside 1..={total}"),
                    ));
                }
                NormOp::Delete { start: at, end }
            }
            EditOpKind::InsertBefore => {
                if at < 1 || at > total + 1 {
                    return Err(HarnessError::tool(
                        "E_INVALID_PATCH",
                        format!("insert_before position {at} is outside 1..={}", total + 1),
                    ));
                }
                NormOp::InsertBefore { at, lines }
            }
            EditOpKind::InsertAfter => {
                if at < 1 || at > total {
                    return Err(HarnessError::tool(
                        "E_INVALID_PATCH",
                        format!("insert_after position {at} is outside 1..={total}"),
                    ));
                }
                NormOp::InsertAfter { at, lines }
            }
            EditOpKind::ReplaceText => unreachable!(),
        };
        let (lo, hi) = op.touch_range();
        min_touched = min_touched.min(lo);
        max_touched = max_touched.max(hi);
        normalized.push(op);
    }

    // Overlap: ranges must be disjoint; two inserts at the same boundary
    // conflict even when one is `insert_after N` and the other is
    // `insert_before N + 1`.
    let mut slots: Vec<(usize, usize)> = normalized
        .iter()
        .enumerate()
        .filter_map(|(i, op)| op.insertion_slot().map(|slot| (slot, i)))
        .collect();
    slots.sort_unstable();
    for pair in slots.windows(2) {
        let (slot, i1) = pair[0];
        let (_, i2) = pair[1];
        if pair[0].0 == pair[1].0 {
            return Err(HarnessError::tool_details(
                "E_OVERLAP",
                format!(
                    "edits #{i1} and #{i2} insert at the same position; split them into separate \
                     calls"
                ),
                json!({ "first": [slot, slot], "second": [slot, slot] }),
            ));
        }
    }

    let mut spans: Vec<(usize, usize, usize)> = normalized
        .iter()
        .enumerate()
        .map(|(i, op)| {
            let (s, e) = op.touch_range();
            (s, e, i)
        })
        .collect();
    spans.sort();
    for pair in spans.windows(2) {
        let (s1, e1, i1) = pair[0];
        let (s2, e2, i2) = pair[1];
        // Ranges are inclusive line numbers; zero-width inserts at the same
        // point also conflict.
        if s2 <= e1 {
            return Err(HarnessError::tool_details(
                "E_OVERLAP",
                format!("edits #{i1} and #{i2} overlap; split them into separate calls"),
                json!({ "first": [s1, e1], "second": [s2, e2] }),
            ));
        }
    }

    let mut new_lines = original_lines.clone();
    normalized.sort_by(|a, b| {
        let (as_, ae) = a.touch_range();
        let (bs, be) = b.touch_range();
        ae.cmp(&be).then(as_.cmp(&bs))
    });
    for op in normalized.iter().rev() {
        match op {
            NormOp::Replace { start, end, lines } => {
                new_lines.splice(start - 1..*end, lines.iter().cloned());
            }
            NormOp::Delete { start, end } => {
                new_lines.drain(start - 1..*end);
            }
            NormOp::InsertBefore { at, lines } => {
                new_lines.splice(at - 1..at - 1, lines.iter().cloned());
            }
            NormOp::InsertAfter { at, lines } => {
                new_lines.splice(*at..*at, lines.iter().cloned());
            }
        }
    }

    let changed = new_lines != original_lines;
    let new_text = reconstruct(&new_lines, crlf, trailing_newline);
    let diff = unified_diff(original, &new_text);
    let (start, end) = changed_range(&original_lines, &new_lines);
    let (anchor_start, anchor_end, table) = anchor_block(&new_lines, start, end, width);
    let old_start = min_touched.min(total.max(1));
    let old_end = max_touched.min(total);
    Ok(Applied {
        lines_added: new_lines.len().saturating_sub(total),
        lines_removed: total.saturating_sub(new_lines.len()),
        new_lines,
        diff,
        changed,
        anchor_start,
        anchor_end,
        anchor_table: table,
        old_anchor_table: anchor_table(&original_lines, old_start, old_end, width),
        crlf,
        trailing_newline,
    })
}

fn reconstruct(lines: &[String], crlf: bool, trailing_newline: bool) -> String {
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut out = lines.join(eol);
    if trailing_newline && !lines.is_empty() {
        out.push_str(eol);
    }
    out
}

fn unified_diff(old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header("a", "b")
        .to_string()
}

fn changed_range(old: &[String], new: &[String]) -> (usize, usize) {
    let mut first = None;
    let mut last = 1usize;
    for (i, line) in new.iter().enumerate() {
        let same = old.get(i) == Some(line);
        if !same {
            if first.is_none() {
                first = Some(i + 1);
            }
            last = i + 1;
        }
    }
    if new.len() < old.len() && first.is_none() {
        first = Some(new.len().max(1));
        last = old.len().max(1);
    }
    (first.unwrap_or(1), last.max(first.unwrap_or(1)))
}

fn anchor_window(pos: &PosSpec, end: Option<&PosSpec>, total: usize) -> (usize, usize) {
    let lo = pos.line.saturating_sub(2).max(1);
    let hi = end
        .map(|e| e.line)
        .unwrap_or(pos.line)
        .saturating_add(2)
        .min(total.max(1));
    (lo, hi)
}

fn anchor_block(
    lines: &[String],
    start: usize,
    end: usize,
    width: usize,
) -> (usize, usize, String) {
    if lines.is_empty() {
        return (1, 0, "(empty file)".to_string());
    }
    let start = start.max(1).min(lines.len());
    let end = end.max(start).min(lines.len());
    let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let mut out = String::new();
    for (n, anchor) in hashline::anchors_range(&refs, start, end, width) {
        out.push_str(&hashline::format_line(n, &anchor, &lines[n - 1]));
        out.push('\n');
    }
    (start, end, out)
}

fn anchor_table(lines: &[String], start: usize, end: usize, width: usize) -> String {
    if lines.is_empty() {
        return "(empty file)".to_string();
    }
    let start = start.max(1).min(lines.len());
    let end = end.max(start).min(lines.len());
    anchor_block(lines, start, end, width).2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(op: EditOpKind, pos: u64, end: Option<u64>, lines: &[&str]) -> EditOpArgs {
        EditOpArgs {
            op,
            pos: json!(pos),
            end: end.map(|e| json!(e)),
            lines: Some(lines.iter().map(|s| s.to_string()).collect()),
            old_text: None,
            new_text: None,
            all: None,
        }
    }

    const SRC: &str = "one\ntwo\nthree\nfour\nfive\n";

    #[test]
    fn replace_single_line() {
        let applied =
            apply_edits(SRC, &[op(EditOpKind::Replace, 2, None, &["TWO"])], 2, true).unwrap();
        assert_eq!(applied.new_lines[1], "TWO");
        assert!(applied.changed);
        assert!(applied.diff.contains("-two"));
        assert!(applied.diff.contains("+TWO"));
    }

    #[test]
    fn insert_before_and_after() {
        let applied = apply_edits(
            SRC,
            &[
                op(EditOpKind::InsertBefore, 1, None, &["zero"]),
                op(EditOpKind::InsertAfter, 5, None, &["six"]),
            ],
            2,
            true,
        )
        .unwrap();
        assert_eq!(applied.new_lines.first().unwrap(), "zero");
        assert_eq!(applied.new_lines.last().unwrap(), "six");
        assert_eq!(applied.new_lines.len(), 7);
    }

    #[test]
    fn insert_after_and_insert_before_same_boundary_conflict() {
        let edits = vec![
            op(EditOpKind::InsertBefore, 3, None, &["B"]),
            op(EditOpKind::InsertAfter, 2, None, &["A"]),
        ];
        let err = apply_edits(SRC, &edits, 2, true).unwrap_err();
        assert_eq!(err.code(), "E_OVERLAP");
    }

    #[test]
    fn delete_range_and_multiple_ops() {
        let applied = apply_edits(
            SRC,
            &[
                op(EditOpKind::Delete, 2, Some(3), &[]),
                op(EditOpKind::Replace, 5, None, &["FIVE"]),
            ],
            2,
            true,
        )
        .unwrap();
        assert_eq!(applied.new_lines, vec!["one", "four", "FIVE"]);
    }

    #[test]
    fn stale_anchor_rejects_whole_batch() {
        let anchor = hashline::anchors(&SRC.lines().collect::<Vec<_>>(), 2)[1].clone();
        let bad = EditOpArgs {
            op: EditOpKind::Replace,
            pos: json!(format!("2#{}", if anchor == "ZZ" { "YY" } else { "ZZ" })),
            end: None,
            lines: Some(vec!["x".into()]),
            old_text: None,
            new_text: None,
            all: None,
        };
        let err = apply_edits(SRC, &[bad], 2, true).unwrap_err();
        assert_eq!(err.code(), "E_STALE_ANCHOR");
    }

    #[test]
    fn good_anchor_is_accepted() {
        let anchors = hashline::anchors(&SRC.lines().collect::<Vec<_>>(), 2);
        let edit = EditOpArgs {
            op: EditOpKind::Replace,
            pos: json!(format!("2#{}", anchors[1])),
            end: None,
            lines: Some(vec!["TWO".into()]),
            old_text: None,
            new_text: None,
            all: None,
        };
        assert!(apply_edits(SRC, &[edit], 2, true).is_ok());
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let edits = vec![
            op(EditOpKind::Replace, 2, Some(3), &["x"]),
            op(EditOpKind::Replace, 3, Some(4), &["y"]),
        ];
        let err = apply_edits(SRC, &edits, 2, true).unwrap_err();
        assert_eq!(err.code(), "E_OVERLAP");
    }

    #[test]
    fn delete_with_lines_is_invalid() {
        let mut edit = op(EditOpKind::Delete, 2, None, &["x"]);
        edit.lines = Some(vec!["x".into()]);
        let err = apply_edits(SRC, &[edit], 2, true).unwrap_err();
        assert_eq!(err.code(), "E_INVALID_PATCH");
    }

    fn text_op(old: &str, new: &str, all: Option<bool>) -> EditOpArgs {
        EditOpArgs {
            op: EditOpKind::ReplaceText,
            pos: json!(1),
            end: None,
            lines: None,
            old_text: Some(old.into()),
            new_text: Some(new.into()),
            all,
        }
    }

    #[test]
    fn replace_text_must_be_unique_unless_all_is_set() {
        assert_eq!(
            apply_edits(SRC, &[text_op("o", "0", None)], 2, true)
                .unwrap_err()
                .code(),
            "E_INVALID_PATCH"
        );
        // `all` turns the same non-unique oldText into a global rename.
        let applied = apply_edits(SRC, &[text_op("o", "0", Some(true))], 2, true).unwrap();
        assert!(!applied.new_lines.join("\n").contains('o'));
        let applied = apply_edits(SRC, &[text_op("three", "THREE", None)], 2, true).unwrap();
        assert!(applied.new_lines.contains(&"THREE".to_string()));
        // Nothing to rename is an error even with `all`.
        assert_eq!(
            apply_edits(SRC, &[text_op("absent", "x", Some(true))], 2, true)
                .unwrap_err()
                .code(),
            "E_INVALID_PATCH"
        );
    }

    #[test]
    fn replace_text_preserves_crlf() {
        let src = "a\r\nb\r\nc\r\n";
        let applied = apply_edits(src, &[text_op("b", "B", None)], 2, true).unwrap();
        assert!(applied.crlf);
        assert!(applied.trailing_newline);
    }

    #[test]
    fn bottom_up_application_keeps_line_numbers_frozen() {
        let applied = apply_edits(
            SRC,
            &[
                op(EditOpKind::Replace, 2, None, &["TWO", "2b"]),
                op(EditOpKind::Replace, 4, None, &["FOUR"]),
            ],
            2,
            true,
        )
        .unwrap();
        assert_eq!(
            applied.new_lines,
            vec!["one", "TWO", "2b", "three", "FOUR", "five"]
        );
    }

    #[test]
    fn crlf_source_round_trips() {
        let src = "alpha\r\nbeta\r\n";
        let applied =
            apply_edits(src, &[op(EditOpKind::Replace, 2, None, &["BETA"])], 2, true).unwrap();
        let text = reconstruct(&applied.new_lines, applied.crlf, applied.trailing_newline);
        assert_eq!(text, "alpha\r\nBETA\r\n");
    }

    #[test]
    fn embedded_cr_survives_an_edit() {
        let src = "a\rb\nc\n";
        let applied =
            apply_edits(src, &[op(EditOpKind::Replace, 2, None, &["C"])], 2, true).unwrap();
        assert_eq!(applied.new_lines[0], "a\rb");
        assert_eq!(
            reconstruct(&applied.new_lines, applied.crlf, applied.trailing_newline),
            "a\rb\nC\n"
        );
    }

    #[test]
    fn anchor_block_is_directly_feedable() {
        let applied = apply_edits(
            SRC,
            &[op(EditOpKind::Replace, 3, None, &["THREE"])],
            2,
            true,
        )
        .unwrap();
        let first = applied.anchor_table.lines().next().unwrap();
        assert!(first.contains('#'));
        assert!(first.contains("|"));
        let anchor = first.split('#').nth(1).unwrap().split('|').next().unwrap();
        assert!(anchor.bytes().all(|b| hashline::ALPHABET.contains(&b)));
    }

    #[test]
    fn empty_file_cannot_be_edited_by_line_ops() {
        let err =
            apply_edits("", &[op(EditOpKind::Replace, 1, None, &["x"])], 2, true).unwrap_err();
        assert_eq!(err.code(), "E_INVALID_PATCH");
    }

    #[test]
    fn dry_run_reports_diff_without_writing() {
        // Delegated to the tool; pure layer only proves the diff is computed.
        let applied =
            apply_edits(SRC, &[op(EditOpKind::Replace, 1, None, &["ONE"])], 2, true).unwrap();
        assert!(applied.diff.starts_with("--- a"));
    }
}
