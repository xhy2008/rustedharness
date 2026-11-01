use std::io::Write;
use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{HarnessError, Result};
use crate::hashline;
use crate::tools::{append_diagnostics, parse_args, schema_of, Tool, ToolContext, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// File, directory, or http(s) URL.
    pub path: String,
    /// 1-based first line (files only).
    pub offset: Option<usize>,
    /// Maximum number of lines.
    pub limit: Option<usize>,
    /// Emit original content without line/anchor prefixes.
    pub raw: Option<bool>,
    /// Extraction instruction for URL content (uses the compact model).
    pub prompt: Option<String>,
    /// Byte cap for the returned window (default 100000).
    pub max_bytes: Option<usize>,
    /// Use the compact model for URL extraction (default true).
    pub use_compact_model: Option<bool>,
    /// URL cache TTL in seconds (default 900).
    pub cache_ttl: Option<u64>,
}

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> String {
        "Read a file, list a directory, or fetch a URL. Files are returned as \
         `line#anchor|content` windows; feed those anchors (e.g. \"12#AB\") to `edit`. \
         Use raw=true for literal content. URLs are converted to markdown, cached, and \
         optionally summarised by the compact model; external content is untrusted."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<ReadArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: ReadArgs = parse_args(args)?;
        if args.path.starts_with("http://") || args.path.starts_with("https://") {
            let ttl = args.cache_ttl.unwrap_or(900);
            let result = ctx
                .web
                .read_url(
                    &args.path,
                    args.prompt.as_deref(),
                    args.use_compact_model.unwrap_or(true),
                    ctx.provider.as_ref(),
                    &ctx.compact_model,
                    ttl,
                )
                .await?;
            return Ok(ToolOutput::ok(result.content).with_metadata(json!({
                "kind": "url",
                "url": result.url,
                "cache_hit": result.cache_hit,
                "truncated": result.truncated,
            })));
        }

        let path = ctx.resolver.resolve(&args.path)?;
        if path.is_dir() {
            return list_dir(ctx, &path, args.limit);
        }
        if !path.exists() {
            return Err(HarnessError::tool(
                "E_NOT_FOUND",
                format!("file not found: {}", ctx.rel(&path)),
            ));
        }
        let bytes = std::fs::read(&path)?;
        if is_binary(&bytes) {
            return Err(HarnessError::tool(
                "E_BINARY",
                format!(
                    "refusing to read binary file {} ({} bytes); use a shell tool for hex/strings",
                    ctx.rel(&path),
                    bytes.len()
                ),
            ));
        }
        let text = String::from_utf8_lossy(&bytes).to_string();
        read_text(ctx, &path, &text, &args)
    }
}

fn read_text(ctx: &ToolContext, path: &Path, text: &str, args: &ReadArgs) -> Result<ToolOutput> {
    let raw = args.raw.unwrap_or(false);
    let max_bytes = args.max_bytes.unwrap_or(100_000);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = args.offset.unwrap_or(1).max(1);
    let limit = args.limit.unwrap_or(usize::MAX);
    if total > 0 && start > total {
        return Err(HarnessError::tool(
            "E_OUT_OF_RANGE",
            format!("offset {start} is beyond the end of the file ({total} lines)"),
        ));
    }
    let end = start.saturating_add(limit).saturating_sub(1).min(total);

    let mut out = String::new();
    let mut used = 0usize;
    let mut shown_end = start.saturating_sub(1);
    let anchors = if raw {
        Vec::new()
    } else {
        hashline::anchors(&lines, ctx.config.edit.anchor_width)
    };
    for n in start..=end {
        if total == 0 {
            break;
        }
        let content = lines[n - 1];
        let rendered = if raw {
            content.to_string()
        } else {
            hashline::format_line(n, &anchors[n - 1], content)
        };
        let cost = rendered.len() + 1;
        if used + cost > max_bytes && shown_end >= start {
            break;
        }
        out.push_str(&rendered);
        out.push('\n');
        used += cost;
        shown_end = n;
    }
    let truncated = shown_end < end;
    let mut body = String::new();
    if !raw {
        body.push_str(&format!(
            "{}: {} lines{}",
            ctx.rel(path),
            total,
            if total == 0 { " (empty)" } else { "" }
        ));
        if start > 1 || shown_end < total {
            let mut note = format!("; showing {start}-{shown_end}");
            if truncated {
                note.push_str(" (byte limit reached)");
            } else if shown_end < total {
                note.push_str(" (more lines below)");
            }
            body.push_str(&note);
        }
        body.push('\n');
    }
    body.push_str(&out);

    Ok(ToolOutput::ok(body).with_metadata(json!({
        "kind": "file",
        "path": ctx.rel(path),
        "total_lines": total,
        "start": start,
        "end": shown_end,
        "truncated": truncated,
        "bytes": text.len(),
    })))
}

fn list_dir(ctx: &ToolContext, path: &Path, limit: Option<usize>) -> Result<ToolOutput> {
    let limit = limit.unwrap_or(500);
    let mut entries: Vec<(String, bool, u64)> = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mut name = entry.file_name().to_string_lossy().to_string();
        if is_dir {
            name.push('/');
        }
        entries.push((name, is_dir, size));
    }
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let total = entries.len();
    let mut body = format!("{}: {} entries\n", ctx.rel(path), total);
    for (name, is_dir, size) in entries.iter().take(limit) {
        if *is_dir {
            body.push_str(&format!("{name}\n"));
        } else {
            body.push_str(&format!("{name}  ({size} bytes)\n"));
        }
    }
    if total > limit {
        body.push_str(&format!(
            "... {} more entries (limit {limit})\n",
            total - limit
        ));
    }
    Ok(ToolOutput::ok(body).with_metadata(json!({
        "kind": "directory",
        "path": ctx.rel(path),
        "entries": total,
        "truncated": total > limit,
    })))
}

pub fn is_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(8_000)];
    if sample.contains(&0) {
        return true;
    }
    std::str::from_utf8(sample).is_err() && std::str::from_utf8(bytes).is_err()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
    /// Replace the whole file atomically instead of appending.
    pub overwrite: Option<bool>,
}

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> String {
        "Create a file, or append to an existing file. Existing files are never \
         silently overwritten: without `overwrite=true` the content is appended and the \
         exact message \"文件已存在，内容已追加\" plus the starting line is returned. \
         Set overwrite=true for an atomic replacement."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<WriteArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: WriteArgs = parse_args(args)?;
        let path = ctx.resolver.resolve(&args.path)?;
        if path.is_dir() {
            return Err(HarnessError::tool(
                "E_IS_DIR",
                format!("{} is a directory", ctx.rel(&path)),
            ));
        }
        let overwrite = args.overwrite.unwrap_or(false);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if path.exists() && !overwrite {
            let existing = std::fs::read_to_string(&path).map_err(|e| {
                HarnessError::tool(
                    "E_IO",
                    format!("cannot read existing file {}: {e}", ctx.rel(&path)),
                )
            })?;
            let already_lines = existing.lines().count();
            let start_line = already_lines + 1;
            let needs_newline = !existing.is_empty() && !existing.ends_with('\n');
            let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
            let mut appended = Vec::new();
            if needs_newline {
                appended.push(b'\n');
            }
            appended.extend_from_slice(args.content.as_bytes());
            file.write_all(&appended)?;
            file.flush()?;
            let new_lines = args.content.lines().count();
            let mut content = format!(
                "文件已存在，内容已追加\nstarting line: {start_line}\nappended lines: {start_line}-{}\nbytes appended: {}",
                start_line + new_lines.saturating_sub(1),
                appended.len()
            );
            let diagnostics = append_diagnostics(ctx, &path, &mut content).await;
            return Ok(ToolOutput::ok(content).with_metadata(json!({
                "action": "appended",
                "path": ctx.rel(&path),
                "start_line": start_line,
                "end_line": start_line + new_lines.saturating_sub(1),
                "bytes": appended.len(),
                "diagnostics": diagnostics,
            })));
        }

        let existed = path.exists();
        atomic_write(&path, args.content.as_bytes(), existed)?;
        let lines = args.content.lines().count();
        let mut content = format!(
            "{} {}\nlines: 1-{}\nbytes: {}",
            if existed { "overwrote" } else { "created" },
            ctx.rel(&path),
            lines,
            args.content.len()
        );
        let diagnostics = append_diagnostics(ctx, &path, &mut content).await;
        Ok(ToolOutput::ok(content).with_metadata(json!({
            "action": if existed { "overwritten" } else { "created" },
            "path": ctx.rel(&path),
            "start_line": 1,
            "end_line": lines,
            "bytes": args.content.len(),
            "diagnostics": diagnostics,
        })))
    }
}

/// Write via a sibling temp file and rename, preserving permissions of an
/// existing target (symlink targets are followed by the resolver, so the
/// rename never replaces the symlink itself).
pub fn atomic_write(path: &Path, bytes: &[u8], existed: bool) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let permissions = if existed {
        std::fs::metadata(path).ok().map(|m| m.permissions())
    } else {
        None
    };
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    if let Some(permissions) = permissions {
        let _ = temp.as_file().set_permissions(permissions);
    }
    temp.persist(path).map_err(|e| {
        HarnessError::tool(
            "E_IO",
            format!("atomic write failed for {}: {}", path.display(), e.error),
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_detection() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary("hello 世界".as_bytes()));
        assert!(!is_binary(b""));
    }
}
