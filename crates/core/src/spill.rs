//! Long tool output goes to a log file instead of into the conversation.
//!
//! A build log or a directory-wide grep can be a megabyte of text, and almost
//! all of it is noise the model will never read while it costs tokens on every
//! subsequent request. Past [`MAX_INLINE_BYTES`] the tool result is therefore
//! replaced by a pointer to a file under [`dir`], which the model reaches with
//! `grep` and `read(ranges)` - the same tools it would use on any other file.
//! The directory lives in the system temp dir; `WorkspacePathResolver` grants
//! the agent access to it even though it sits outside the workspace.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::Result;

/// Tool output up to this size is returned inline. Larger results are written
/// to a log file: a `read` window (100 kB) and a typical `grep` still fit, so
/// ordinary tool use never turns into file juggling.
pub const MAX_INLINE_BYTES: usize = 128 * 1024;

/// Log files older than this are deleted whenever a new one is written, so the
/// temp directory does not accumulate one directory listing per failed build.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Where spilled output goes: `<temp>/rustedharness-logs`.
pub fn dir() -> PathBuf {
    std::env::temp_dir().join("rustedharness-logs")
}

/// Write `text` to a fresh log file named after the tool call and return its
/// path. Old logs are pruned first so a long session cannot grow unbounded.
pub fn save(tool: &str, call_id: &str, text: &str) -> Result<PathBuf> {
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    prune(&dir);
    let path = dir.join(format!(
        "{}-{}-{}.log",
        epoch_millis(),
        sanitize(tool),
        sanitize(call_id)
    ));
    std::fs::write(&path, text)?;
    Ok(path)
}

/// What the conversation keeps from one tool result: the text itself while it
/// is small enough to be worth the tokens, otherwise a pointer to a log file
/// that the model can grep. Nothing is silently cut in half - a truncated build
/// log hides the error at its end, which is the only part that matters.
pub fn inline_or_spill(tool: &str, call_id: &str, content: &str) -> String {
    if content.len() <= MAX_INLINE_BYTES {
        return content.to_string();
    }
    match save(tool, call_id, content) {
        Ok(path) => notice(&path, content.len()),
        Err(err) => format!(
            "[tool output was {} bytes and could not be saved: {err}]\n{}",
            content.len(),
            content.chars().take(4000).collect::<String>()
        ),
    }
}

/// The text the model sees instead of the output it was spared.
pub fn notice(path: &std::path::Path, bytes: usize) -> String {
    format!(
        "[tool output was {bytes} bytes, too large to return; the full text is in {}]\n\
         grep or read that file to find what you need.",
        path.display()
    )
}

fn prune(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if now.duration_since(modified).is_ok_and(|age| age > MAX_AGE) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

/// Keep the file name readable but inert: only `[A-Za-z0-9._-]`, and short.
fn sanitize(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(48);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spilled_output_is_readable_from_the_notice() {
        let body = "x".repeat(MAX_INLINE_BYTES + 1);
        let path = save("bash", "call_1", &body).unwrap();
        assert!(path.starts_with(dir()));
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), body.len());
        let notice = notice(&path, body.len());
        assert!(notice.contains(&path.display().to_string()), "{notice}");
        assert!(notice.contains("grep"), "{notice}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_names_stay_readable() {
        assert_eq!(sanitize("bash"), "bash");
        assert_eq!(sanitize("call/../x y"), "call_.._x_y");
        assert!(sanitize(&"a".repeat(200)).len() <= 48);
    }
}
