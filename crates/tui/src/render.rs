//! Pi-style renderer.
//!
//! Visual grammar, top to bottom:
//!
//! * a masthead block that names the session,
//! * a borderless transcript where user turns sit on a quiet band, prose is
//!   plain text, reasoning is a dim italic aside, and a tool call is one grey
//!   `[name] argument` line with a state glyph,
//! * a rounded editor box that lights up while a turn is running,
//! * one status line.
//!
//! Nothing is boxed except surfaces the user can interact with (editor,
//! pickers, cards), so the transcript reads as a document rather than a form.

use std::borrow::Cow;

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{
    human_count, model_filtered, ordered, selection_row_span, session_filtered, shorten_path, App,
    Overlay, PickerItem,
};
use crate::entries::{Entry, EntryKind};
use crate::theme::*;

/// Left padding so text never touches the terminal edge, and one spare column
/// on the right that absorbs East-Asian ambiguous glyphs which conhost draws
/// two cells wide while unicode-width counts one.
const PAD_LEFT: u16 = 1;
const PAD_RIGHT: u16 = 2;

pub fn draw(app: &mut App, frame: &mut Frame) {
    let area = {
        let mut a = frame.area();
        a.width = a.width.saturating_sub(1);
        a
    };
    let chunks = Layout::new(
        Direction::Vertical,
        [
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ],
    )
    .split(area);

    // A panel that opens, closes or changes type can be smaller than the last
    // one, so the rows it no longer covers may still hold a wide glyph's
    // trailing cell. The diff rewrites those cells; the only thing it cannot
    // fix is that half-glyph, which needs the whole row rewritten once.
    let overlay = app.overlay_key();
    if app.painted_overlay != overlay {
        app.painted_overlay = overlay;
        app.wide_rows_dirty = true;
    }

    let content = Rect {
        x: chunks[0].x + PAD_LEFT,
        y: chunks[0].y + 1,
        width: chunks[0].width.saturating_sub(PAD_LEFT + PAD_RIGHT),
        height: chunks[0].height.saturating_sub(1),
    };
    draw_transcript(app, frame, content);
    app.mouse_area = content;
    let caret = draw_editor(app, frame, chunks[1]);
    draw_footer(app, frame, chunks[2]);
    if app.consent.is_some() {
        draw_consent(app, frame, chunks[0]);
    }
    draw_overlay(app, frame, chunks[0]);

    // A modal owns the screen, so the composer must not keep the cursor.
    if app.overlay_key().is_none() && app.consent.is_none() {
        if let Some(caret) = caret {
            frame.set_cursor_position(caret);
        }
    }

    // Forcing every wide row every frame would rewrite the screen 8 times a
    // second, which also wipes the IME composition string mid-typing. Only do
    // it on the frames where the transcript was actually rebuilt.
    if std::mem::take(&mut app.wide_rows_dirty) {
        repaint_wide_rows(frame);
    }
}

/// Rewrite whole rows that contain a double-width glyph.
///
/// ratatui only re-emits the trailing column of a wide glyph when the cell it
/// replaced had a background colour or a visible modifier (see `BufferDiff`),
/// because "terminals handle wide characters well". conhost does not: a CJK
/// line replaced by shorter text can leave the right half of its last glyph on
/// screen forever. Rows without wide glyphs keep the cheap incremental diff.
fn repaint_wide_rows(frame: &mut Frame) {
    use ratatui::buffer::CellDiffOption;
    use ratatui::layout::Position;

    let area = frame.area();
    let buffer = frame.buffer_mut();
    for y in area.y..area.bottom() {
        let wide = (area.x..area.right()).any(|x| {
            buffer
                .cell(Position::new(x, y))
                .is_some_and(|cell| cell.symbol().chars().any(|c| char_width(c) > 1))
        });
        if wide {
            for x in area.x..area.right() {
                if let Some(cell) = buffer.cell_mut(Position::new(x, y)) {
                    cell.set_diff_option(CellDiffOption::AlwaysUpdate);
                }
            }
        }
    }
}

fn style(color: Color) -> Style {
    Style::default().fg(color)
}

// ---------------------------------------------------------------------------
// width helpers
// ---------------------------------------------------------------------------

fn display_width(text: &str) -> usize {
    Span::raw(text.to_string()).width()
}

fn char_width(c: char) -> usize {
    Span::raw(String::from(c)).width()
}

/// Truncate to `max` display columns including the trailing ellipsis.
pub fn clip(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    let ellipsis = max > 3;
    let limit = if ellipsis { max - 3 } else { max };
    let mut out = String::new();
    let mut width = 0;
    for c in text.chars() {
        let cw = char_width(c);
        if width + cw > limit {
            break;
        }
        out.push(c);
        width += cw;
    }
    if ellipsis {
        out.push_str("...");
    }
    out
}

/// Extend a line with `bg`-coloured spaces to exactly `width` columns so a
/// band or a selected row reaches the right edge in one uninterrupted stroke.
fn fill(line: Line<'static>, width: usize, bg: Color) -> Line<'static> {
    let used = line.width();
    if used >= width {
        return line;
    }
    let mut line = line;
    line.spans.push(Span::styled(
        " ".repeat(width - used),
        Style::default().bg(bg),
    ));
    line
}

/// East-Asian-"ambiguous" characters are one column to `unicode-width` and two
/// columns to a console with a CJK font. Either way the terminal renders it
/// differently from what the layout assumed, every following character on the
/// row shifts, and the tail of the line lands outside the cells the frame
/// buffer owns - where it stays forever. Tool output and model prose are full
/// of them (`→`, em dashes, ellipses, curly quotes), so they are folded to
/// ASCII before the layout ever sees them.
fn is_ambiguous(c: char) -> bool {
    use unicode_width::UnicodeWidthChar;
    c.width_cjk() != c.width()
}

/// Whether the terminal draws ambiguous-width glyphs in two cells.
///
/// There is no portable way to ask: the console decides, typically from its
/// font and code page, and two consoles on the same machine can disagree. So
/// the value is measured once at startup (see `lib::measure_ambiguous_width`)
/// and every layout decision follows it. `true` is the safe default: a console
/// that draws them wide will spill the tail of a row into cells the frame
/// buffer thinks are blank, and those cells are never repainted.
static AMBIGUOUS_IS_WIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_ambiguous_wide(wide: bool) {
    AMBIGUOUS_IS_WIDE.store(wide, std::sync::atomic::Ordering::Relaxed);
}

pub fn ambiguous_is_wide() -> bool {
    AMBIGUOUS_IS_WIDE.load(std::sync::atomic::Ordering::Relaxed)
}

fn narrow(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{2190}' => "<-",
        '\u{2192}' => "->",
        '\u{2194}' => "<->",
        '\u{2191}' => "^",
        '\u{2193}' => "v",
        '\u{2013}' | '\u{2014}' | '\u{2015}' => "-",
        '\u{2018}' | '\u{2019}' => "'",
        '\u{201c}' | '\u{201d}' => "\"",
        '\u{2026}' => "...",
        '\u{2022}' => "*",
        '\u{00b7}' => "-",
        '\u{00d7}' => "x",
        '\u{00f7}' => "/",
        '\u{00b1}' => "+/-",
        '\u{00a5}' => "CNY ",
        '\u{20ac}' => "EUR ",
        '\u{00a9}' => "(c)",
        '\u{00ae}' => "(r)",
        '\u{2122}' => "(TM)",
        '\u{00b0}' => "deg",
        '\u{00a7}' => "S",
        '\u{00b6}' => "P",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// text flow
// ---------------------------------------------------------------------------

/// Make text safe to put in a span.
///
/// Tool output is raw file text (`read`, `grep`, `edit`) and models emit
/// anything at all, so bodies routinely contain tabs and escape sequences.
/// A tab is measured as zero columns by unicode-width but the terminal jumps
/// to the next tab stop, and an escape sequence is executed by the terminal:
/// both push the rest of the row into cells the frame buffer believes are
/// blank, which is exactly how orphaned characters survive every repaint.
/// Expand tabs, keep newlines, drop every other control character.
fn sanitize(text: &str) -> Cow<'_, str> {
    let fold = ambiguous_is_wide();
    if !text
        .chars()
        .any(|c| (c.is_control() && c != '\n') || (fold && is_ambiguous(c)))
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            // Control characters, plus the invisible formatting characters a
            // model or a terminal log likes to smuggle in: zero-width joiners
            // and spaces, bidi overrides (which can reorder a whole row) and
            // variation selectors (which turn one cell into two).
            c if c.is_control() => {}
            '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{00ad}' => {}
            c if fold && is_ambiguous(c) => {
                if let Some(narrow) = narrow(c) {
                    out.push_str(narrow);
                }
            }
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Wrap styled fragments to `width` display columns.
///
/// `first` is the prefix of the opening row, `cont` the prefix of continuation
/// rows; both are emitted in their own style so a marker can differ from the
/// body (e.g. an accent marker in front of plain text). Widths are display
/// columns, which keeps CJK and emoji from overrunning the right edge.
fn flow(
    frags: Vec<Span<'static>>,
    first: (&str, Style),
    cont: (&str, Style),
    width: usize,
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::styled(first.0.to_string(), first.1)];
    let mut used = display_width(first.0);
    let mut filled = false;
    let mut last: Option<Style> = None;

    for frag in frags {
        for c in frag.content.chars() {
            // A newline inside a fragment (a pasted multi-line prompt, a tool
            // result) must become a real line break here: emitted into a span
            // it would move the terminal cursor behind the frame buffer's back.
            if c == '\n' {
                lines.push(Line::from(std::mem::take(&mut spans)));
                spans.push(Span::styled(cont.0.to_string(), cont.1));
                used = display_width(cont.0);
                last = None;
                filled = false;
                continue;
            }
            let cw = char_width(c);
            if filled && used + cw > width {
                lines.push(Line::from(std::mem::take(&mut spans)));
                spans.push(Span::styled(cont.0.to_string(), cont.1));
                used = display_width(cont.0);
                last = None;
            }
            if last == Some(frag.style) {
                if let Some(span) = spans.last_mut() {
                    span.content.to_mut().push(c);
                }
            } else {
                spans.push(Span::styled(c.to_string(), frag.style));
                last = Some(frag.style);
            }
            used += cw;
            filled = true;
        }
    }
    if filled || lines.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

// ---------------------------------------------------------------------------
// markdown
// ---------------------------------------------------------------------------

/// Render assistant prose. Parsing and table layout live in [`crate::markdown`],
/// which wraps `tui-markdown`; this only keeps the transcript interface.
fn prose_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    crate::markdown::to_lines(text, width)
}

/// Hash the transcript by content, not by length: a restored or re-streamed
/// entry can be replaced by text of the same size, and a length-only signature
/// would keep rendering the stale lines from the cache.
fn transcript_signature(app: &App) -> u64 {
    let mut hash = 0u64;
    for entry in &app.entries {
        let flags =
            (entry.expanded as u64) << 62 | entry.ok.map(|o| o as u64 + 1).unwrap_or(0) << 60;
        let body = xxhash_rust::xxh32::xxh32(entry.body.as_bytes(), 0) as u64;
        let title = xxhash_rust::xxh32::xxh32(entry.title.as_bytes(), 1) as u64;
        let arg = xxhash_rust::xxh32::xxh32(entry.arg.as_bytes(), 2) as u64;
        let mix = ((entry.kind as u64) << 56) ^ (title << 40) ^ (arg << 20) ^ body;
        hash = hash.wrapping_mul(1000003) ^ flags ^ mix;
    }
    hash
}

/// Build the transcript plus a parallel owner map: `owners[i]` is the entry
/// index that produced line `i`, which is what click-to-fold needs. Separator
/// rows own nothing (`usize::MAX`).
fn transcript_lines(app: &App, width: usize) -> (Vec<Line<'static>>, Vec<usize>) {
    let width = width.max(20);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut owners: Vec<usize> = Vec::new();
    for (index, entry) in app.entries.iter().enumerate() {
        // Vertical rhythm: a tool call belongs to the block above it - the
        // sentence that asked for it, the thinking that decided it, or the
        // previous call - so it never gets a blank row of its own. Everything
        // else keeps one line of air.
        if index > 0 && entry.kind != EntryKind::Tool {
            lines.push(Line::from(""));
            owners.push(usize::MAX);
        }
        let start = lines.len();
        lines.extend(entry_lines(entry, width));
        owners.extend(std::iter::repeat_n(index, lines.len() - start));
    }

    (lines, owners)
}

fn entry_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
    let title = sanitize(&entry.title);
    let arg = sanitize(&entry.arg);
    let body = sanitize(&entry.body);
    match entry.kind {
        EntryKind::User => {
            let band = |c: Color| style(c).bg(USER_BG);
            let frags = vec![Span::styled(body.to_string(), band(TEXT))];
            let lines = flow(frags, ("> ", band(ACCENT)), ("  ", band(TEXT)), width);
            lines.into_iter().map(|l| fill(l, width, USER_BG)).collect()
        }
        EntryKind::Assistant => prose_lines(&body, width),
        EntryKind::Reasoning => reasoning_lines(entry, &body, width),
        EntryKind::Tool => tool_lines(entry, title.as_ref(), arg.as_ref(), &body, width),
        EntryKind::Notice => {
            let mut lines = Vec::new();
            if title.is_empty() {
                for raw in body.lines() {
                    lines.extend(flow(
                        vec![Span::styled(raw.to_string(), style(DIM))],
                        ("* ", style(FAINT)),
                        ("  ", style(FAINT)),
                        width,
                    ));
                }
            } else {
                lines.push(Line::from(vec![
                    Span::styled("* ", style(FAINT)),
                    Span::styled(title.to_string(), style(MUTED)),
                ]));
                for raw in body.lines() {
                    lines.extend(flow(
                        vec![Span::styled(raw.to_string(), style(DIM))],
                        ("  ", style(FAINT)),
                        ("  ", style(FAINT)),
                        width,
                    ));
                }
            }
            lines
        }
        EntryKind::Error => {
            let base = style(RED);
            let mut lines = Vec::new();
            for raw in body.lines() {
                lines.extend(flow(
                    vec![Span::styled(raw.to_string(), base)],
                    ("! ", base),
                    ("  ", base),
                    width,
                ));
            }
            lines
        }
        EntryKind::Compaction => {
            let rule = format!("── {} ", title);
            let pad = width.saturating_sub(display_width(&rule)).min(width);
            let mut lines = vec![Line::from(vec![
                Span::styled(rule, style(FAINT)),
                Span::styled("─".repeat(pad), style(FAINT)),
            ])];
            let summary: String = body.chars().take(400).collect();
            for raw in summary.lines() {
                lines.extend(flow(
                    vec![Span::styled(raw.to_string(), style(DIM))],
                    ("  ", style(FAINT)),
                    ("  ", style(FAINT)),
                    width,
                ));
            }
            lines
        }
    }
}

fn tool_lines(
    entry: &Entry,
    title: &str,
    arg: &str,
    body: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let (glyph, color) = match entry.ok {
        Some(true) => ("+", GREEN),
        Some(false) => ("!", RED),
        None => ("~", YELLOW),
    };
    let mut header = vec![
        Span::styled(format!("{glyph} "), style(color)),
        Span::styled(format!("[{title}]"), style(MUTED)),
    ];
    if !arg.is_empty() {
        header.push(Span::styled(" ", style(FAINT)));
        header.push(Span::styled(arg.to_string(), style(DIM)));
    }
    // A tool argument can be a whole shell command or a long path; wrap it on
    // the same four-space indent as the output instead of letting it run off
    // the right edge.
    let mut lines = flow(header, ("", style(TEXT)), ("    ", style(FAINT)), width);

    let body = body.trim_end();
    if body.is_empty() {
        return lines;
    }
    let body_style = style(if entry.ok == Some(false) { RED } else { DIM });
    if entry.expanded {
        let shown = if body.chars().count() > 4_000 {
            format!("{}...", body.chars().take(4_000).collect::<String>())
        } else {
            body.to_string()
        };
        for raw in shown.lines() {
            lines.extend(flow(
                vec![Span::styled(raw.to_string(), body_style)],
                ("    ", body_style),
                ("    ", body_style),
                width,
            ));
        }
    } else {
        // Folded: one peek line under a stub, so the row still says what the
        // tool returned without spending the whole screen on it.
        let total = body.lines().count();
        let preview = body
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        let mut peek = vec![
            Span::styled("    └ ", style(FAINT)),
            Span::styled(
                clip(preview.trim_end(), width.saturating_sub(12)),
                body_style,
            ),
        ];
        if total > 1 {
            peek.push(Span::styled(format!("  +{}", total - 1), style(FAINT)));
        }
        lines.push(Line::from(peek));
    }
    lines
}

/// Thinking block: open while it streams, folded to a single line afterwards.
fn reasoning_lines(entry: &Entry, body: &str, width: usize) -> Vec<Line<'static>> {
    let base = style(DIM).add_modifier(Modifier::ITALIC);
    let marker = if entry.expanded { "v" } else { ">" };
    let mut head = vec![
        Span::styled(format!("{marker} "), style(FAINT)),
        Span::styled("thinking", style(DIM)),
    ];
    if !entry.expanded {
        let count = body.lines().count();
        head.push(Span::styled(
            format!("  {count} line{}", if count == 1 { "" } else { "s" }),
            style(FAINT),
        ));
        return vec![Line::from(head)];
    }

    let mut lines = vec![Line::from(head)];
    let body = if body.chars().count() > 700 {
        let head: String = body.chars().take(700).collect();
        format!("{head}...")
    } else {
        body.to_string()
    };
    for raw in body.lines() {
        lines.extend(flow(
            vec![Span::styled(raw.to_string(), base)],
            ("  ", style(FAINT)),
            ("  ", style(FAINT)),
            width,
        ));
    }
    lines
}

fn draw_transcript(app: &mut App, frame: &mut Frame, area: Rect) {
    let width = area.width as usize;
    let height = area.height as usize;
    let sig = transcript_signature(app);
    {
        let mut cache = app.lines_cache.borrow_mut();
        if app.lines_sig != (width, sig) {
            let (lines, owners) = transcript_lines(app, width);
            *cache = lines;
            *app.owner_cache.borrow_mut() = owners;
            app.lines_sig = (width, sig);
            // conhost keeps the right half of a wide glyph that we no longer
            // draw; rewriting those rows is the only way to clear it, and it
            // is only needed when the text actually changed.
            app.wide_rows_dirty = true;
        }
    }
    let cache = app.lines_cache.borrow();
    let max_scroll = cache.len().saturating_sub(height);
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll >= max_scroll {
            app.follow = true;
        }
    }
    // Scrolling swaps whole rows in and out, which can expose the trailing
    // cell of a wide glyph the previous frame left behind.
    if app.painted_scroll != app.scroll {
        app.painted_scroll = app.scroll;
        app.wide_rows_dirty = true;
    }
    let paragraph = Paragraph::new(cache.as_slice()).scroll((app.scroll as u16, 0));
    frame.render_widget(paragraph, area);

    // Drag-selection highlight. Linear, like a terminal: middle rows are
    // highlighted edge to edge, only the first and last row stop at the drag
    // column.
    if let Some((a, b)) = app.selection {
        let ((top_col, top), (bottom_col, bottom)) = ordered(a, b);
        let style = Style::default().fg(TEXT).bg(SELECTED_BG);
        let width = area.width as usize;
        let buffer = frame.buffer_mut();
        for row in top..=bottom {
            if row >= area.height as usize {
                break;
            }
            let Some((start, end)) = selection_row_span(row, top, top_col, bottom, bottom_col)
            else {
                continue;
            };
            let y = area.y + row as u16;
            for column in start..end.min(width) {
                if let Some(cell) =
                    buffer.cell_mut(ratatui::layout::Position::new(area.x + column as u16, y))
                {
                    cell.set_style(style);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// editor
// ---------------------------------------------------------------------------

/// Draw the composer and return the cell the caret belongs in.
///
/// The caret has to be the *terminal's* cursor, not a glyph: Windows consoles
/// draw the IME composition string and place the candidate window at the
/// console cursor position, so with a fake caret the pinyin shows up wherever
/// the last frame happened to leave the cursor - typically a few rows above
/// the box, over the transcript.
fn draw_editor(app: &App, frame: &mut Frame, area: Rect) -> Option<Position> {
    let accent = if app.busy { ACCENT } else { BORDER };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style(accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width < 4 {
        return None;
    }

    let prompt_style = style(ACCENT);
    // Reserve one column so the caret or the last glyph never lands in the
    // final cell, where conhost would latch a pending wrap.
    let avail = inner.width.saturating_sub(3) as usize;
    let cursor = app.cursor.min(app.input.len());
    let mut pre: Vec<char> = Vec::new();
    let mut pre_w = 0usize;
    for &c in app.input[..cursor].iter().rev() {
        let cw = char_width(c);
        if pre_w + cw > avail {
            break;
        }
        pre.insert(0, c);
        pre_w += cw;
    }
    let mut post = String::new();
    let mut post_w = 0usize;
    for &c in app.input[cursor..].iter() {
        let cw = char_width(c);
        if pre_w + post_w + cw > avail {
            break;
        }
        post.push(c);
        post_w += cw;
    }

    let text_style = style(TEXT);
    let mut spans = vec![Span::styled("> ", prompt_style)];
    if app.input.is_empty() {
        // The caret occupies the first cell after the prompt, so the hint has
        // to start one column later or the block cursor eats its first letter.
        spans.push(Span::styled(
            " Ask anything. / for commands",
            style(FAINT).add_modifier(Modifier::ITALIC),
        ));
    } else {
        spans.push(Span::styled(
            pre.into_iter().collect::<String>(),
            text_style,
        ));
        spans.push(Span::styled(post, text_style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);

    // "> " is two columns; the caret sits right after the visible prefix.
    let x = inner.x + 2 + pre_w.min(inner.width as usize) as u16;
    Some(Position::new(x, inner.y))
}

// ---------------------------------------------------------------------------
// footer
// ---------------------------------------------------------------------------

fn draw_footer(app: &App, frame: &mut Frame, area: Rect) {
    // One extra column of slack: the status line carries `¥`, which a CJK
    // console may draw two cells wide.
    let width = area.width.saturating_sub(2) as usize;
    frame.render_widget(Paragraph::new(footer_line(app, width)), area);
}

/// Build the status line, shedding the least important fields until it fits.
///
/// A wrapped status bar is worse than a shorter one: it pushes the editor off
/// its row, so the order of sacrifice is fixed (path length, cache hit,
/// compaction model, cost) and clipping is only the last resort.
fn footer_line(app: &App, width: usize) -> Line<'static> {
    let ctx_pct = if app.context_limit > 0 {
        app.context_tokens as f64 * 100.0 / app.context_limit as f64
    } else {
        0.0
    };
    let ctx_color = if ctx_pct >= 90.0 {
        RED
    } else if ctx_pct >= 70.0 {
        YELLOW
    } else {
        GREEN
    };
    let ctx_str = format!("ctx {:.1}%/{}", ctx_pct, human_count(app.context_limit));

    let build = |cwd: usize, compact: bool, cache: bool, cost: bool, tokens: bool| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if let Some((text, error)) = app.active_toast() {
            spans.push(Span::styled(
                text.to_string(),
                style(if error { RED } else { ACCENT }),
            ));
            spans.push(Span::styled("|", style(FAINT)));
        }
        if app.busy {
            spans.push(Span::styled(
                format!("working{}", crate::app::spinner(app.tick)),
                style(YELLOW),
            ));
            if !app.queue.is_empty() {
                spans.push(Span::styled(
                    format!("{} queued", app.queue.len()),
                    style(DIM),
                ));
            }
            spans.push(Span::styled("|", style(FAINT)));
        }
        spans.push(Span::styled(shorten_path(&app.cwd, cwd), style(MUTED)));
        spans.push(Span::styled("|", style(FAINT)));
        spans.push(Span::styled(app.agent_model.clone(), style(BLUE)));
        if compact {
            spans.push(Span::styled("|", style(FAINT)));
            spans.push(Span::styled(
                format!("compact {}", app.compact_model),
                style(DIM),
            ));
        }
        spans.push(Span::styled("|", style(FAINT)));
        spans.push(Span::styled(ctx_str.clone(), style(ctx_color)));
        if tokens {
            spans.push(Span::styled("|", style(FAINT)));
            spans.push(Span::styled(
                format!(
                    "in {}|out {}",
                    human_count(app.input_tokens),
                    human_count(app.output_tokens)
                ),
                style(DIM),
            ));
        }
        if cache {
            if let Some(pct) = app.cache_hit_pct {
                spans.push(Span::styled("|", style(FAINT)));
                spans.push(Span::styled(
                    format!("CacheHit{pct}%"),
                    style(if pct >= 80 { GREEN } else { YELLOW }),
                ));
            }
        }
        if cost {
            if let Some(total) = app.cost_usd {
                spans.push(Span::styled("|", style(FAINT)));
                spans.push(Span::styled(format_cost(total), style(ACCENT)));
            }
        }
        if let Some(reason) = &app.last_stop {
            if reason != "completed" {
                spans.push(Span::styled("|", style(FAINT)));
                spans.push(Span::styled(reason.clone(), style(DIM)));
            }
        }
        Line::from(spans)
    };

    let layouts = [
        (40usize, true, true, true, true),
        (24, true, true, true, true),
        (24, true, false, true, true),
        (24, true, false, false, true),
        (16, true, false, false, true),
        (16, false, false, false, true),
        (10, false, false, false, true),
        (8, false, false, false, false),
    ];
    let mut last = None;
    for (cwd, compact, cache, cost, tokens) in layouts {
        let line = build(cwd, compact, cache, cost, tokens);
        if line.width() <= width {
            return line;
        }
        last = Some(line);
    }
    clip_line(
        last.unwrap_or_else(|| build(8, false, false, false, false)),
        width as u16,
    )
}

/// Trim a line's spans to a single row of at most `width` display columns, so a
/// long status bar cannot wrap onto a second row and push the editor out of
/// place on narrow terminals.
fn clip_line(line: Line<'static>, width: u16) -> Line<'static> {
    let max = width as usize;
    if line.width() <= max {
        return line;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        let w = span.width();
        if used + w <= max {
            used += w;
            out.push(span);
            continue;
        }
        let budget = max - used;
        let ellipsis = budget > 3;
        let limit = if ellipsis { budget - 3 } else { budget };
        let mut text = String::new();
        let mut tw = 0;
        for c in span.content.chars() {
            let cw = char_width(c);
            if tw + cw > limit {
                break;
            }
            text.push(c);
            tw += cw;
        }
        if ellipsis {
            text.push_str("...");
        }
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
        break;
    }
    Line::from(out)
}

fn format_cost(cost: f64) -> String {
    if cost == 0.0 {
        "¥0".to_string()
    } else if cost < 0.01 {
        format!("¥{cost:.4}")
    } else {
        format!("¥{cost:.2}")
    }
}

// ---------------------------------------------------------------------------
// overlays
// ---------------------------------------------------------------------------

fn draw_consent(app: &App, frame: &mut Frame, body: Rect) {
    let Some(card) = &app.consent else { return };
    let text_width = (body.width.saturating_sub(14)).min(72) as usize;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(card.display_name.clone(), style(YELLOW)),
            Span::styled(format!("  v{}", card.version), style(FAINT)),
        ]),
        Line::from(""),
        Line::from(Span::styled(clip(&card.url, text_width), style(DIM))),
        Line::from(Span::styled(
            match &card.sha256 {
                Some(sha256) => format!("sha256 {}", clip(sha256, text_width)),
                None => "sha256 none published - installing unverified".to_string(),
            },
            style(FAINT),
        )),
    ];
    if let Some(hint) = &card.runtime_hint {
        lines.push(Line::from(Span::styled(
            format!("runtime {}", clip(hint, text_width)),
            style(FAINT),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("y", style(ACCENT)),
        Span::styled(" allow   ", style(DIM)),
        Span::styled("n", style(ACCENT)),
        Span::styled(" refuse   ", style(DIM)),
        Span::styled("esc", style(ACCENT)),
        Span::styled(" close", style(DIM)),
    ]));
    let height = (lines.len() as u16 + 2).min(body.height);
    let width = (body.width.saturating_sub(6)).min(84);
    let area = Rect {
        x: body.x + (body.width.saturating_sub(width)) / 2,
        y: body.y + body.height.saturating_sub(height + 1),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style(YELLOW))
        .style(Style::default().bg(BG_BASE))
        .title(Line::from(Span::styled(" download ", style(YELLOW))))
        .padding(ratatui::widgets::Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_overlay(app: &App, frame: &mut Frame, body: Rect) {
    match &app.overlay {
        Overlay::None => {}
        Overlay::Slash { items, sel } => draw_picker(frame, body, "commands", items, *sel, ""),
        Overlay::Model {
            items,
            sel,
            filter,
            target,
        } => {
            let shown: Vec<PickerItem> =
                model_filtered(items, filter).into_iter().cloned().collect();
            let title = match target {
                crate::app::ModelTarget::Agent => "model",
                crate::app::ModelTarget::Compact => "compact model",
            };
            draw_picker(frame, body, title, &shown, *sel, filter);
        }
        Overlay::Session { items, sel, filter } => {
            let shown: Vec<PickerItem> = session_filtered(items, filter)
                .into_iter()
                .cloned()
                .collect();
            draw_picker(frame, body, "sessions", &shown, *sel, filter);
        }
        Overlay::FileBrowser { rel, entries, sel } => {
            let items: Vec<PickerItem> = entries
                .iter()
                .map(|(name, dir)| PickerItem {
                    label: if *dir {
                        format!("{name}/")
                    } else {
                        name.clone()
                    },
                    detail: String::new(),
                    value: name.clone(),
                })
                .collect();
            let title = if rel.is_empty() {
                "files".to_string()
            } else {
                format!("files  {rel}")
            };
            draw_picker(frame, body, &title, &items, *sel, "");
        }
        Overlay::FileView {
            path,
            lines,
            scroll,
            truncated,
        } => {
            let height = body.height.saturating_sub(1).max(4);
            let area = Rect {
                x: body.x,
                y: body.bottom().saturating_sub(height),
                width: body.width,
                height,
            };
            let numbers = lines.len().to_string().len().max(3);
            let rendered: Vec<Line> = lines
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    Line::from(vec![
                        Span::styled(format!("{:>numbers$}  ", index + 1), style(FAINT)),
                        Span::styled(sanitize(line).into_owned(), style(MUTED)),
                    ])
                })
                .collect();
            let flag = if *truncated { " (truncated)" } else { "" };
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(style(BORDER))
                .style(Style::default().bg(BG_BASE))
                .title(Line::from(vec![
                    Span::styled(" ", style(DIM)),
                    Span::styled(path.clone(), style(MUTED)),
                    Span::styled(flag.to_string(), style(FAINT)),
                ]))
                .title(
                    Line::from(Span::styled(" j/k scroll  esc close ", style(FAINT)))
                        .right_aligned(),
                );
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(rendered)
                    .block(block)
                    .scroll((*scroll as u16, 0)),
                area,
            );
        }
        Overlay::Settings { sel } => {
            let ctx_pct = if app.context_limit > 0 {
                (app.context_tokens as f64 * 100.0 / app.context_limit as f64) as usize
            } else {
                0
            };
            let rows = [
                PickerItem {
                    label: "agent model".to_string(),
                    detail: app.agent_model.clone(),
                    value: String::new(),
                },
                PickerItem {
                    label: "compact model".to_string(),
                    detail: app.compact_model.clone(),
                    value: String::new(),
                },
                PickerItem {
                    label: "workspace".to_string(),
                    detail: app.cwd.clone(),
                    value: String::new(),
                },
                PickerItem {
                    label: "session".to_string(),
                    detail: format!(
                        "{}  ctx {ctx_pct}%",
                        app.session_id.chars().take(8).collect::<String>()
                    ),
                    value: String::new(),
                },
            ];
            draw_picker(frame, body, "settings", &rows, *sel, "");
        }
        Overlay::Hotkeys => {
            let lines = vec![
                panel_title("keys"),
                Line::from(""),
                kv("enter", "send, or queue while working"),
                kv("alt+enter", "queue follow-up"),
                kv("esc", "cancel turn, clear input, close overlay"),
                kv("ctrl+c", "clear editor, quit when empty"),
                kv("ctrl+d", "quit"),
                kv("ctrl+o", "expand/collapse all tool output"),
                kv("ctrl+t", "expand/collapse all thinking"),
                kv("ctrl+l", "model picker"),
                kv("ctrl+r", "session picker"),
                kv("ctrl+f", "file browser"),
                kv("ctrl+x", "copy last reply"),
                kv("ctrl+g", "follow end / repaint"),
                kv("ctrl+m", "toggle mouse capture (off = native select)"),
                kv("click", "fold/unfold a tool or thinking block"),
                kv("drag", "linear selection, copied on release"),
                kv("pgup/pgdn", "scroll transcript"),
                Line::from(""),
                kv("/", "command list"),
                Line::from(""),
                Line::from(vec![
                    Span::styled("ambiguous width ", style(FAINT)),
                    Span::styled(
                        if ambiguous_is_wide() {
                            "double - symbols folded to ascii"
                        } else {
                            "single - symbols kept as typed"
                        },
                        style(DIM),
                    ),
                    Span::styled("   RH_AMBIWIDTH", style(FAINT)),
                ]),
            ];
            draw_info_block(frame, body, lines);
        }
        Overlay::Todos => {
            let mut lines = vec![panel_title("todos")];
            if app.todo.is_empty() {
                lines.push(Line::from(Span::styled("none", style(FAINT))));
            }
            for item in &app.todo {
                let color = match item.status {
                    rustedharness_core::tools::todo::TodoStatus::Completed => GREEN,
                    rustedharness_core::tools::todo::TodoStatus::InProgress => YELLOW,
                    rustedharness_core::tools::todo::TodoStatus::Pending => MUTED,
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", item.status.symbol()), style(color)),
                    Span::styled(sanitize(&item.content).into_owned(), style(MUTED)),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("any key to close", style(FAINT))));
            draw_info_block(frame, body, lines);
        }
        Overlay::Processes => {
            let mut lines = vec![panel_title("processes")];
            if app.processes.is_empty() {
                lines.push(Line::from(Span::styled("none", style(FAINT))));
            }
            for (pid, p) in &app.processes {
                let color = if p.status == "running" { YELLOW } else { GREEN };
                lines.push(Line::from(vec![
                    Span::styled(format!("{pid}"), style(DIM)),
                    Span::styled(format!("  {}", p.status), style(color)),
                    Span::styled(
                        p.exit_code
                            .map(|c| format!("  exit {c}"))
                            .unwrap_or_default(),
                        style(FAINT),
                    ),
                ]));
                for screen in p.screen.lines().take(3) {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", clip(screen, 100)),
                        style(FAINT),
                    )));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("any key to close", style(FAINT))));
            draw_info_block(frame, body, lines);
        }
        Overlay::Lsp => {
            let mut lines = vec![panel_title("language servers")];
            if app.lsp.is_empty() {
                lines.push(Line::from(Span::styled("idle", style(FAINT))));
            }
            for status in &app.lsp {
                let color = match status.state.as_str() {
                    "running" => GREEN,
                    "warming" | "downloading" => YELLOW,
                    "degraded" => RED,
                    _ => DIM,
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{:14}", status.language), style(MUTED)),
                    Span::styled(format!("{:12}", status.state), style(color)),
                    Span::styled(
                        clip(&sanitize(status.detail.as_deref().unwrap_or("")), 44),
                        style(FAINT),
                    ),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "/checksum <id>   /install <id>   any key to close",
                style(FAINT),
            )));
            draw_info_block(frame, body, lines);
        }
    }
}

fn panel_title(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        style(TEXT).add_modifier(Modifier::BOLD),
    ))
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:14}"), style(BLUE)),
        Span::styled(value.to_string(), style(DIM)),
    ])
}

/// Bottom-anchored rounded panel with an opaque backdrop; returns its inner
/// area (borders and one column of padding already removed).
fn panel(frame: &mut Frame, body: Rect, height: u16, title: Option<Line<'static>>) -> Rect {
    let height = height.min(body.height).max(3);
    let area = Rect {
        y: body.y + body.height.saturating_sub(height + 1),
        height,
        ..body
    };
    frame.render_widget(Clear, area);
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style(BORDER))
        .style(Style::default().bg(BG_BASE))
        .padding(ratatui::widgets::Padding::new(1, 1, 0, 0));
    if let Some(title) = title {
        block = block.title(title);
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn draw_info_block(frame: &mut Frame, body: Rect, lines: Vec<Line<'static>>) {
    let height = lines.len() as u16 + 2;
    let inner = panel(frame, body, height, None);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_picker(
    frame: &mut Frame,
    body: Rect,
    title: &str,
    items: &[PickerItem],
    sel: usize,
    filter: &str,
) {
    let window = items.len().min(12);
    let height = window as u16 + if items.len() > window { 2 } else { 1 } + 1;
    let mut heading = vec![
        Span::styled(" ", style(FAINT)),
        Span::styled(title.to_string(), style(MUTED)),
    ];
    if !filter.is_empty() {
        heading.push(Span::styled(format!("  {filter}"), style(ACCENT)));
    }
    let title_line = Line::from(heading);
    let height = height.min(body.height);
    let area = Rect {
        y: body.y + body.height.saturating_sub(height + 1),
        height,
        ..body
    };
    frame.render_widget(Clear, area);

    let counter_line = Line::from(Span::styled(
        format!(" {} of {} ", window.min(items.len()), items.len()),
        style(FAINT),
    ))
    .right_aligned();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style(BORDER))
        .style(Style::default().bg(BG_BASE))
        .title(title_line)
        .title(counter_line)
        .padding(ratatui::widgets::Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let width = inner.width as usize;
    let start = sel.saturating_sub(inner.height as usize / 2);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if items.is_empty() {
        lines.push(Line::from(Span::styled("no matches", style(FAINT))));
    }
    let longest = items
        .iter()
        .map(|i| display_width(&i.label))
        .max()
        .unwrap_or(0);
    let label_col = (longest + 2).clamp(10, (width * 2 / 5).max(12));
    for (index, item) in items
        .iter()
        .enumerate()
        .skip(start)
        .take(inner.height as usize)
    {
        let selected = index == sel;
        let row_style = if selected {
            Style::default().fg(TEXT).bg(SELECTED_BG)
        } else {
            Style::default().fg(MUTED)
        };
        let mut spans = vec![
            Span::styled(
                if selected { "> " } else { "  " },
                if selected {
                    style(ACCENT).bg(SELECTED_BG)
                } else {
                    style(FAINT)
                },
            ),
            Span::styled(
                format!("{:<label_col$}", clip(&sanitize(&item.label), label_col)),
                row_style,
            ),
        ];
        if !item.detail.is_empty() {
            let room = width.saturating_sub(label_col + 3);
            if room > 4 {
                spans.push(Span::styled(
                    format!("  {}", clip(&sanitize(&item.detail), room)),
                    if selected {
                        style(MUTED).bg(SELECTED_BG)
                    } else {
                        style(FAINT)
                    },
                ));
            }
        }
        let line = Line::from(spans);
        lines.push(if selected {
            fill(line, width, SELECTED_BG)
        } else {
            line
        });
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
