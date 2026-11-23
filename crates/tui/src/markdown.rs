//! Markdown rendering for assistant prose.
//!
//! Parsing and layout come from `tui-markdown` (pulldown-cmark underneath),
//! including GFM tables, lists, task lists, quotes and fenced code. This module
//! only maps its style sheet onto our palette and wraps the result to the
//! transcript width - a terminal is not a browser, but markdown is what the
//! models actually write, so it should read like markdown.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tui_markdown::{AlertKind, BuiltinCodeTheme, Options, StyleSheet};

use crate::render::sanitize;
use crate::theme::{ACCENT, BLUE, CODE_BG, DIM, FAINT, MUTED, RED, TEXT, YELLOW};

/// Our palette as a `tui-markdown` style sheet.
#[derive(Clone, Copy, Default)]
struct Skin;

impl StyleSheet for Skin {
    fn heading(&self, level: u8) -> Style {
        let base = Style::default().fg(TEXT).add_modifier(Modifier::BOLD);
        if level <= 1 {
            base.fg(ACCENT)
        } else {
            base
        }
    }

    /// Inline code and fenced code both land here; a tinted background is what
    /// separates them from prose without drawing boxes.
    fn code(&self) -> Style {
        Style::default().fg(TEXT).bg(CODE_BG)
    }

    fn link(&self) -> Style {
        Style::default().fg(BLUE).add_modifier(Modifier::UNDERLINED)
    }

    fn blockquote(&self) -> Style {
        Style::default().fg(DIM).add_modifier(Modifier::ITALIC)
    }

    fn html(&self) -> Style {
        Style::default().fg(FAINT)
    }

    fn table_header(&self) -> Style {
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
    }

    fn table_cell(&self) -> Style {
        Style::default().fg(TEXT)
    }

    fn table_border(&self) -> Style {
        Style::default().fg(FAINT)
    }

    fn list_marker(&self) -> Style {
        Style::default().fg(ACCENT)
    }

    fn alert(&self, kind: AlertKind) -> Style {
        let color = match kind {
            AlertKind::Note | AlertKind::Tip | AlertKind::Important => BLUE,
            AlertKind::Warning => YELLOW,
            AlertKind::Caution => RED,
        };
        Style::default().fg(color)
    }

    /// No `##` in front of headings: the transcript is already dense, and the
    /// weight change carries the hierarchy.
    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }

    /// The tinted background delimits a code block; fence lines would only add
    /// two rows of noise per block.
    fn code_block_fence(&self) -> &str {
        ""
    }
}

/// Render markdown to transcript lines that fit `width` display columns.
pub fn to_lines(markdown: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = render(markdown, Some(width.max(20)));
    // Models like to trail a turn with blank lines; the transcript expresses
    // that with block spacing instead of empty rows at the very end.
    while lines.last().is_some_and(|line| line.width() == 0) {
        lines.pop();
    }
    lines
}

/// Highlight one file for the viewer.
///
/// The content goes through the same pipeline as prose - wrapped in a fenced
/// block naming the syntax - so a file and an embedded snippet of the same
/// language cannot drift apart in colour. The fence is five tildes so a file
/// that itself contains ``` fences cannot terminate it early.
pub fn highlight(source: &str, language: &str) -> Vec<Line<'static>> {
    // The final newline belongs to the file, not to an extra blank line: drop
    // it so the viewer's row count matches `str::lines`.
    let source = sanitize(source);
    let body = source.strip_suffix('\n').unwrap_or(&source);
    let fenced = format!("~~~~~{language}\n{body}\n~~~~~");
    render(&fenced, None)
}

/// Un-highlighted rows for a file: what the viewer shows while the highlighter
/// is still running, and what it keeps for files too large to bother with.
pub fn plain(source: &str) -> Vec<Line<'static>> {
    let source = sanitize(source);
    source
        .lines()
        .map(|line| Line::from(Span::styled(line.to_string(), Style::default().fg(MUTED))))
        .collect()
}

/// Syntax token for a path, empty when the language is not recognised (the
/// renderer then falls back to the plain code style).
pub fn language_for(path: &str) -> &'static str {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "rs" => "rust",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "markdown",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "sh" | "bash" | "zsh" => "bash",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" | "cxx" => "c++",
        "go" => "go",
        "java" => "java",
        "rb" => "ruby",
        "php" => "php",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "sql" => "sql",
        "lua" => "lua",
        "diff" | "patch" => "diff",
        _ => "",
    }
}

fn render(markdown: &str, width: Option<usize>) -> Vec<Line<'static>> {
    let mut options = Options::new(Skin)
        // A Base16 theme that shares our palette's tone: muted blues, greens
        // and purples. The highlighter colours tokens but does not paint a slab
        // behind the block, so code sits on the same surface as the prose.
        .code_theme(BuiltinCodeTheme::Base16OceanDark);
    if let Some(width) = width {
        options = options.table_width(width.min(u16::MAX as usize) as u16);
    }
    let text = tui_markdown::from_str_with_options(markdown, &options);

    let mut out: Vec<Line<'static>> = Vec::new();
    for line in text.lines {
        // The crate keeps element styles on the line and character styles on
        // the span; both have to survive, and `Text` borrows the markdown while
        // the transcript cache is `'static`.
        let spans: Vec<Span<'static>> = line
            .spans
            .into_iter()
            .map(|span| {
                let mut style = span.style.patch(line.style);
                if style.fg.is_none() {
                    style = style.fg(TEXT);
                }
                Span::styled(span.content.into_owned(), style)
            })
            .collect();

        match width {
            // The viewer scrolls instead of reflowing and numbers its own
            // lines, so its rows must stay one-to-one with the file.
            None => out.push(Line::from(spans)),
            Some(width) => out.extend(wrap_words(spans, width)),
        }
    }
    out
}

/// Wrap styled spans to `width`, breaking at spaces when there is one and
/// between characters otherwise (CJK has no spaces, and a long URL must still
/// fit). The implementation is shared with the transcript's tool/output flow.
fn wrap_words(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    crate::render::wrap_spans(spans, width, ("", Style::default()), ("", Style::default()))
}
