//! Markdown rendering for assistant prose.
//!
//! Parsing and layout come from `tui-markdown` (pulldown-cmark underneath),
//! including GFM tables, lists, task lists, quotes and fenced code. This module
//! only maps its style sheet onto our palette and wraps the result to the
//! transcript width - a terminal is not a browser, but markdown is what the
//! models actually write, so it should read like markdown.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tui_markdown::{AlertKind, Options, StyleSheet};

use crate::theme::{ACCENT, BLUE, CODE_BG, DIM, FAINT, RED, TEXT, YELLOW};

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
    let width = width.max(20);
    let options = Options::new(Skin).table_width(width.min(u16::MAX as usize) as u16);
    let text = tui_markdown::from_str_with_options(markdown, &options);

    let mut out: Vec<Line<'static>> = Vec::new();
    for line in text.lines {
        // The crate keeps element styles on the line and character styles on
        // the span; both have to survive, and `Text` borrows the markdown while
        // the transcript cache is `'static`.
        let in_code_block = line.style.bg == Some(CODE_BG);
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

        let mut wrapped = wrap_words(spans, width);
        if in_code_block {
            // Code blocks read as one surface: paint the band to the full
            // width instead of letting it end with the longest line.
            for line in &mut wrapped {
                let used = line.width();
                if used < width {
                    line.spans.push(Span::styled(
                        " ".repeat(width - used),
                        Style::default().bg(CODE_BG),
                    ));
                }
            }
        }
        out.extend(wrapped);
    }

    while out.last().is_some_and(|line| line.width() == 0) {
        out.pop();
    }
    out
}

/// Wrap styled spans to `width`, breaking at spaces when there is one and
/// between characters otherwise (CJK has no spaces, and a long URL must still
/// fit).
fn wrap_words(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in spans {
        chars.extend(span.content.chars().map(|c| (c, span.style)));
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut used = 0usize;
    let mut last_space: Option<usize> = None;

    for (c, style) in chars {
        let char_width = Span::raw(c.to_string()).width();
        if used + char_width > width && !current.is_empty() {
            match last_space {
                // Break after the last space instead of in the middle of a
                // word; the space itself is dropped.
                Some(index) => {
                    let rest = current.split_off(index + 1);
                    let line = std::mem::replace(&mut current, rest);
                    lines.push(render_chars(line));
                    used = current
                        .iter()
                        .map(|(c, _)| Span::raw(c.to_string()).width())
                        .sum();
                    last_space = current.iter().rposition(|(c, _)| *c == ' ');
                }
                None => {
                    lines.push(render_chars(std::mem::take(&mut current)));
                    used = 0;
                }
            }
        }
        if c == ' ' {
            last_space = Some(current.len());
        }
        current.push((c, style));
        used += char_width;
    }
    if !current.is_empty() {
        lines.push(render_chars(current));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Re-merge per-character styles into spans.
fn render_chars(chars: Vec<(char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (c, style) in chars {
        match spans.last_mut() {
            Some(span) if span.style == style => span.content.to_mut().push(c),
            _ => spans.push(Span::styled(c.to_string(), style)),
        }
    }
    Line::from(spans)
}
