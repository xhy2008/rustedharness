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
    /// Replacement/inserted lines. Omit for delete.
    pub lines: Option<Vec<String>>,
    /// Unique substring for op=replace_text.
    #[serde(rename = "oldText")]
    pub old_text: Option<String>,
    #[serde(rename = "newText")]
    pub new_text: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EditArgs {
    pub path: String,
    pub edits: Vec<EditOpArgs>,
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
        "Apply one or more line-numbered patches to an existing file. Each edit is \
         {op: replace|insert_before|insert_after|delete|replace_text, pos: line or \
         \"line#anchor\", end?, lines?, oldText?, newText?}. Line numbers choose the \
         position; an anchor verifies that line's current content. All edits are \
         validated against one pre-edit snapshot and applied bottom-up; any stale anchor rejects \
         the whole batch with E_STALE_ANCHOR and the current anchor table. replace_text \
         uses oldText/newText and must be the ONLY edit in a call; mixing it with any \
         other edit (or several replace_text ops) fails with E_INVALID_PATCH. `lines[]` is \
         the new content only. Returns a unified diff, fresh anchors, and LSP diagnostics \
         when available. dry_run=true writes nothing."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<EditArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: EditArgs = parse_args(args)?;
        if args.edits.is_empty() {
            return Err(HarnessError::tool("E_INVALID_PATCH", "edits[] is empty"));
        }
        if let Some(max) = ctx.config.edit.max_ops_per_call {
            if args.edits.len() > max {
                return Err(HarnessError::tool(
                    "E_INVALID_PATCH",
                    format!(
                        "{} edits supplied but edit.max_ops_per_call={max}; send at most {max} \
                         edit(s) per call",
                        args.edits.len()
                    ),
                ));
            }
        }
        let path = ctx.resolver.resolve_existing(&args.path)?;
        if path.is_dir() {
            return Err(HarnessError::tool(
                "E_IS_DIR",
                format!("{} is a directory", ctx.rel(&path)),
            ));
        }
        let raw = std::fs::read(&path)?;
        if crate::tools::fs_tools::is_binary(&raw) {
            return Err(HarnessError::tool(
                "E_BINARY",
                format!("refusing to edit binary file {}", ctx.rel(&path)),
            ));
        }
        // `is_binary` already rejects invalid UTF-8; keep this explicit so the
        // edit path never turns undecodable bytes into replacement characters.
        let original = std::str::from_utf8(&raw)
            .map_err(|_| {
                HarnessError::tool(
                    "E_BINARY",
                    format!("refusing to edit non-UTF-8 file {}", ctx.rel(&path)),
                )
            })?
            .to_string();
        let width = ctx.config.edit.anchor_width;
        let applied = apply_edits(
            &original,
            &args.edits,
            width,
            ctx.config.edit.enable_replace_text,
        )?;

        if !applied.changed {
            let fingerprint = format!(
                "{:x}",
                xxhash_rust::xxh32::xxh32(
                    format!(
                        "{}:{}",
                        ctx.rel(&path),
                        serde_json::to_string(&args.edits).unwrap_or_default()
                    )
                    .as_bytes(),
                    0
                )
            );
            let count = ctx.noop.record(&ctx.rel(&path), &fingerprint);
            if count >= 3 {
                return Err(HarnessError::tool(
                    "E_NOOP_LOOP",
                    format!(
                        "the same no-op edit was applied {count} times in a row; stop repeating it \
                         and inspect the current file with `read`"
                    ),
                ));
            }
            return Ok(ToolOutput::ok(format!(
                "no changes (dry_run={})\n{}\n--- Anchors {}# ---\n{}",
                args.dry_run.unwrap_or(false),
                applied.diff,
                applied.anchor_start,
                applied.old_anchor_table
            ))
            .with_metadata(json!({ "changed": false, "noop_count": count })));
        }
        ctx.noop.reset(&ctx.rel(&path));

        if args.dry_run.unwrap_or(false) {
            return Ok(ToolOutput::ok(format!(
                "dry_run: no files were written\n{}\n--- Anchors {}-{} ---\n{}",
                applied.diff, applied.anchor_start, applied.anchor_end, applied.anchor_table
            ))
            .with_metadata(json!({
                "changed": true,
                "dry_run": true,
                "path": ctx.rel(&path),
            })));
        }

        // 3-way guard: if the file changed between read and write, refuse rather
        // than clobber an external edit.
        let current = std::fs::read(&path)?;
        if current != raw {
            return Err(HarnessError::tool(
                "E_STALE_SNAPSHOT",
                format!(
                    "{} changed on disk after the snapshot was read; re-read it and retry",
                    ctx.rel(&path)
                ),
            ));
        }

        let new_text = reconstruct(&applied.new_lines, applied.crlf, applied.trailing_newline);
        atomic_write(&path, new_text.as_bytes(), true)?;

        let mut content = format!(
            "{}\n--- Anchors {}-{} ---\n{}",
            applied.diff, applied.anchor_start, applied.anchor_end, applied.anchor_table
        );
        let diagnostics = append_diagnostics(ctx, &path, &mut content).await;
        Ok(ToolOutput::ok(content).with_metadata(json!({
            "changed": true,
            "dry_run": false,
            "path": ctx.rel(&path),
            "lines_added": applied.lines_added,
            "lines_removed": applied.lines_removed,
            "diagnostics": diagnostics,
        })))
    }
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
        if hits != 1 {
            return Err(HarnessError::tool_details(
                "E_INVALID_PATCH",
                format!("oldText must match exactly once; found {hits} matches"),
                json!({ "matches": hits }),
            ));
        }
        let new_text = original.replacen(old, new, 1);
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

    #[test]
    fn replace_text_must_be_unique() {
        let edit = EditOpArgs {
            op: EditOpKind::ReplaceText,
            pos: json!(1),
            end: None,
            lines: None,
            old_text: Some("o".into()),
            new_text: Some("0".into()),
        };
        let err = apply_edits(SRC, &[edit], 2, true).unwrap_err();
        assert_eq!(err.code(), "E_INVALID_PATCH");
        let edit = EditOpArgs {
            op: EditOpKind::ReplaceText,
            pos: json!(1),
            end: None,
            lines: None,
            old_text: Some("three".into()),
            new_text: Some("THREE".into()),
        };
        let applied = apply_edits(SRC, &[edit], 2, true).unwrap();
        assert!(applied.new_lines.contains(&"THREE".to_string()));
    }

    #[test]
    fn replace_text_preserves_crlf() {
        let src = "a\r\nb\r\nc\r\n";
        let edit = EditOpArgs {
            op: EditOpKind::ReplaceText,
            pos: json!(1),
            end: None,
            lines: None,
            old_text: Some("b".into()),
            new_text: Some("B".into()),
        };
        let applied = apply_edits(src, &[edit], 2, true).unwrap();
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
