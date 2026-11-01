//! Pi-style renderer: borderless transcript, filled user/tool cards,
//! single editor rule with embedded status, two-line footer.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{model_filtered, session_filtered, App, Overlay, PickerItem};
use crate::entries::EntryKind;
use crate::theme::*;

pub fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let rows = vec![
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ];
    let chunks = Layout::new(Direction::Vertical, rows).split(area);
    let body = chunks[0];
    let editor = chunks[1];
    let footer = chunks[2];

    draw_transcript(app, frame, body);
    app.mouse_area = body;
    draw_editor(app, frame, editor);
    draw_footer(app, frame, footer);
    // Inset overlays/transcript fills by one column: writing the last terminal
    // column triggers conhost's pending auto-wrap, which desynchronizes
    // subsequent incremental redraws (the "typing shifts rows" glitch that a
    // manual resize temporarily fixes).
    if app.consent.is_some() {
        draw_consent(app, frame, shrink(body));
    }
    draw_overlay(app, frame, shrink(body));
}

fn shrink(area: Rect) -> Rect {
    Rect {
        width: area.width.saturating_sub(1),
        ..area
    }
}

fn transcript_signature(app: &App) -> u64 {
    let mut hash =
        (if app.tools_open { 1u64 } else { 0 }) ^ if app.show_thinking { 2 } else { 0 };
    for entry in &app.entries {
        let flags = (entry.expanded as u64) << 62 | entry.ok.map(|o| o as u64 + 1).unwrap_or(0) << 60;
        let mix = ((entry.kind as u64) << 56)
            ^ ((entry.title.len() as u64) << 28)
            ^ entry.body.len() as u64;
        hash = hash.wrapping_mul(1000003) ^ flags ^ mix;
    }
    hash
}

fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let width = width.max(20);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut first_block = true;

    for entry in &app.entries {
        if entry.kind == EntryKind::Reasoning && !app.show_thinking {
            continue;
        }
        if !first_block {
            lines.push(Line::from(""));
        }
        first_block = false;

        match entry.kind {
            EntryKind::User => {
                let body_style = Style::default().fg(TEXT).bg(USER_BG);
                let mut prefix_done = false;
                for raw in entry.body.lines() {
                    if !prefix_done {
                        lines.extend(wrap_styled(
                            format!("❯ {raw}"),
                            body_style,
                            "  ",
                            width,
                        ));
                        prefix_done = true;
                    } else {
                        lines.extend(wrap_styled(raw.to_string(), body_style, "  ", width));
                    }
                }
                if entry.body.is_empty() {
                    lines.push(Line::from(Span::styled("❯", body_style)));
                }
            }
            EntryKind::Assistant => {
                for raw in entry.body.lines() {
                    let style = if raw.starts_with("```") {
                        Style::default().fg(MUTED).bg(TOOL_BG)
                    } else {
                        Style::default().fg(TEXT)
                    };
                    lines.extend(wrap_styled(raw.to_string(), style, "", width));
                }
            }
            EntryKind::Reasoning => {
                let style = Style::default().fg(THINKING).add_modifier(Modifier::ITALIC);
                lines.push(Line::from(Span::styled("▎ thinking", style)));
                let body = if entry.body.chars().count() > 600 {
                    let t: String = entry.body.chars().take(600).collect();
                    format!("{t}…")
                } else {
                    entry.body.clone()
                };
                for raw in body.lines() {
                    lines.extend(wrap_styled(format!("▎ {raw}"), style, "▎ ", width));
                }
            }
            EntryKind::Tool => {
                let status_color = match entry.ok {
                    Some(true) => GREEN,
                    Some(false) => RED,
                    None => YELLOW,
                };
                let card_bg = match entry.ok {
                    Some(true) => TOOL_OK_BG,
                    Some(false) => TOOL_ERR_BG,
                    None => TOOL_BG,
                };
                let head = format!("┌ {}", entry.title);
                lines.extend(wrap_styled(
                    head,
                    Style::default().fg(status_color).bg(card_bg),
                    "│ ",
                    width,
                ));
                if entry.visible_body(app.tools_open) && !entry.body.trim().is_empty() {
                    let body = if entry.body.chars().count() > 4_000 {
                        let t: String = entry.body.chars().take(4_000).collect();
                        format!("{t}…")
                    } else {
                        entry.body.clone()
                    };
                    for raw in body.lines() {
                        lines.extend(wrap_styled(
                            format!("│ {raw}"),
                            Style::default().fg(MUTED).bg(card_bg),
                            "│ ",
                            width,
                        ));
                    }
                    lines.push(Line::from(Span::styled(
                        "└",
                        Style::default().fg(DIM).bg(card_bg),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        "└ Ctrl+O expand",
                        Style::default().fg(DIM),
                    )));
                }
            }
            EntryKind::Notice => {
                let style = Style::default().fg(DIM);
                if entry.title.is_empty() {
                    for raw in entry.body.lines() {
                        lines.extend(wrap_styled(format!("· {raw}"), style, "  ", width));
                    }
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("· {}", entry.title),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )));
                    for raw in entry.body.lines() {
                        lines.extend(wrap_styled(format!("  {raw}"), style, "    ", width));
                    }
                }
            }
            EntryKind::Error => {
                let style = Style::default().fg(RED).add_modifier(Modifier::BOLD);
                for raw in entry.body.lines() {
                    lines.extend(wrap_styled(format!("! {raw}"), style, "  ", width));
                }
            }
            EntryKind::Compaction => {
                lines.push(Line::from(Span::styled(
                    format!("╌╌ {} ╌╌", entry.title),
                    Style::default().fg(ACCENT),
                )));
                let summary: String = entry.body.chars().take(300).collect();
                lines.push(Line::from(Span::styled(
                    format!("  {summary}"),
                    Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
                )));
            }
        }
    }

    if app.entries.iter().all(|e| e.kind == EntryKind::Notice) {
        lines.push(Line::from(Span::styled(
            "  Type a request, or / for commands. Wheel scrolls; drag selects and copies.",
            Style::default().fg(DIM),
        )));
    }
    lines
}

/// Width-aware wrap using display columns so CJK/emoji never overflow the
/// right edge (the classic "floating characters" bug when counting chars).
fn wrap_styled(text: String, style: Style, cont: &str, width: usize) -> Vec<Line<'static>> {
    let indent = display_width(cont);
    let budget = width.saturating_sub(indent).max(4);
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    let mut first = true;
    loop {
        if chars.peek().is_none() {
            if first {
                out.push(Line::from(Span::styled(String::new(), style)));
            }
            break;
        }
        let mut chunk = String::new();
        let mut used = 0usize;
        while let Some(&c) = chars.peek() {
            let cw = char_width(c);
            if used + cw > budget && !chunk.is_empty() {
                break;
            }
            chunk.push(c);
            used += cw;
            chars.next();
        }
        let rendered = if first { chunk } else { format!("{cont}{chunk}") };
        let rendered = if style.bg.is_some() {
            let pad = width.saturating_sub(display_width(&rendered));
            format!("{}{}", rendered, " ".repeat(pad))
        } else {
            rendered
        };
        out.push(Line::from(Span::styled(rendered, style)));
        first = false;
    }
    out
}

fn display_width(s: &str) -> usize {
    Span::raw(s.to_string()).width()
}

fn char_width(c: char) -> usize {
    Span::raw(String::from(c)).width()
}

fn draw_transcript(app: &mut App, frame: &mut Frame, area: Rect) {
    let width = area.width.saturating_sub(1) as usize;
    let height = area.height as usize;
    let sig = transcript_signature(app);
    {
        let mut cache = app.lines_cache.borrow_mut();
        if app.lines_sig != (width, sig) {
            *cache = transcript_lines(app, width);
            app.lines_sig = (width, sig);
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
    let paragraph = Paragraph::new(cache.as_slice()).scroll((app.scroll as u16, 0));
    frame.render_widget(paragraph, area);

    // Drag-selection highlight: restyle the cells under the selected rect
    // without touching their glyphs.
    if let Some(((c1, r1), (c2, r2))) = app.selection {
        let x1 = area.x + c1.min(c2);
        let x2 = (area.x + c1.max(c2)).min(area.right().saturating_sub(1));
        let y1 = area.y + r1.min(r2);
        let y2 = (area.y + r1.max(r2)).min(area.bottom().saturating_sub(1));
        let style = Style::default().fg(TEXT).bg(SELECTED_BG);
        let buffer = frame.buffer_mut();
        for y in y1..=y2 {
            for x in x1..=x2 {
                if let Some(cell) = buffer.cell_mut(ratatui::layout::Position::new(x, y)) {
                    cell.set_style(style);
                }
            }
        }
    }
}

fn draw_editor(app: &App, frame: &mut Frame, area: Rect) {
    let rows = Layout::new(
        Direction::Vertical,
        [Constraint::Length(1), Constraint::Min(1)],
    )
    .split(area);

    // Rule across the top, like Pi's editor border.
    let rule = Line::from(vec![Span::styled(
        "─".repeat(area.width.saturating_sub(1) as usize),
        Style::default().fg(if app.busy { ACCENT } else { BORDER_DIM }),
    )]);
    frame.render_widget(Paragraph::new(rule), rows[0]);

    // Single-line viewport around the cursor; the caret is drawn (no hardware
    // cursor) to avoid conhost block blink. No leading prompt glyph.
    // Reserve two columns: 1 for the caret glyph and 1 so we never write the
    // last column (conhost auto-wraps there and shifts every later row).
    let avail = (rows[1].width as usize).saturating_sub(2).max(1);
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
        if pre_w + post_w + cw > avail.saturating_sub(pre_w) {
            break;
        }
        post.push(c);
        post_w += cw;
    }

    let text_style = Style::default().fg(TEXT);
    let mut spans = vec![
        Span::styled(pre.into_iter().collect::<String>(), text_style),
        Span::styled(
            "▏",
            Style::default()
                .fg(if app.busy { YELLOW } else { ACCENT })
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !post.is_empty() {
        spans.push(Span::styled(post, text_style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rows[1]);
}

fn draw_footer(app: &App, frame: &mut Frame, area: Rect) {
    let ctx_pct = if app.context_limit > 0 {
        app.context_tokens as f64 * 100.0 / app.context_limit as f64
    } else {
        0.0
    };
    let ctx_str = format!(
        "ctx {:.1}%/{}",
        ctx_pct,
        crate::app::human_count(app.context_limit)
    );
    let ctx_color = if ctx_pct >= 90.0 {
        RED
    } else if ctx_pct >= 70.0 {
        YELLOW
    } else {
        GREEN
    };
    let sep = || Span::styled("  ·  ", Style::default().fg(DIM));
    let mut spans = Vec::new();
    if app.busy {
        spans.push(Span::styled(
            format!("{} working", crate::app::spinner(app.tick)),
            Style::default().fg(YELLOW),
        ));
        if !app.queue.is_empty() {
            spans.push(Span::styled(
                format!(" (+{})", app.queue.len()),
                Style::default().fg(DIM),
            ));
        }
        spans.push(sep());
    }
    spans.push(Span::styled(shorten(&app.cwd, 36), Style::default().fg(MUTED)));
    spans.push(sep());
    spans.push(Span::styled(app.agent_model.clone(), Style::default().fg(BLUE)));
    spans.push(sep());
    spans.push(Span::styled(
        format!("compact {}", app.compact_model),
        Style::default().fg(BLUE),
    ));
    spans.push(sep());
    spans.push(Span::styled(ctx_str, Style::default().fg(ctx_color)));
    spans.push(sep());
    spans.push(Span::styled(
        format!(
            "in {} · out {}",
            crate::app::human_count(app.input_tokens),
            crate::app::human_count(app.output_tokens)
        ),
        Style::default().fg(DIM),
    ));
    if let Some(cost) = app.cost_usd {
        spans.push(sep());
        spans.push(Span::styled(format_cost(cost), Style::default().fg(ACCENT)));
    }
    if let Some(reason) = &app.last_stop {
        if reason != "completed" {
            spans.push(sep());
            spans.push(Span::styled(reason.clone(), Style::default().fg(DIM)));
        }
    }
    frame.render_widget(
        Paragraph::new(truncate_line(
            Line::from(spans),
            area.width.saturating_sub(1),
        )),
        area,
    );
}

/// Trim a line's spans to a single row of at most `width` display columns, so a
/// long status bar cannot wrap onto a second row and push the editor out of
/// place on narrow terminals.
fn truncate_line(line: Line<'static>, width: u16) -> Line<'static> {
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
        let keep = max - used;
        let mut text = String::new();
        let mut tw = 0;
        for c in span.content.chars() {
            let cw = char_width(c);
            if tw + cw > keep.saturating_sub(1) {
                break;
            }
            text.push(c);
            tw += cw;
        }
        text.push('…');
        out.push(Span::styled(text, span.style));
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

fn shorten(path: &str, max: usize) -> String {
    if path.chars().count() <= max {
        return path.to_string();
    }
    let tail: String = path
        .chars()
        .rev()
        .take(max - 1)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

fn draw_consent(app: &App, frame: &mut Frame, body: Rect) {
    let Some(card) = &app.consent else { return };
    let height = 8.min(body.height.saturating_sub(1));
    let width = (body.width.saturating_sub(8)).min(80);
    let area = Rect {
        x: body.x + (body.width.saturating_sub(width)) / 2,
        y: body.y + body.height.saturating_sub(height + 1),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::from(Span::styled(
            format!("download {}", card.display_name),
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {} · v{}", card.url, card.version),
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            format!("  sha256 {}", card.sha256),
            Style::default().fg(DIM),
        )),
    ];
    if let Some(hint) = &card.runtime_hint {
        lines.push(Line::from(Span::styled(
            format!("  runtime: {hint}"),
            Style::default().fg(DIM),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  y allow · n/Esc decline",
        Style::default().fg(ACCENT),
    )));
    let paragraph = Paragraph::new(lines).block(
        Block::bordered()
            .border_style(Style::default().fg(BORDER))
            .title(" consent "),
    );
    frame.render_widget(paragraph, area);
}

fn draw_overlay(app: &App, frame: &mut Frame, body: Rect) {
    match &app.overlay {
        Overlay::None => {}
        Overlay::Slash { items, sel } => {
            draw_picker_items(frame, body, "commands", items, *sel, "")
        }
        Overlay::Model {
            items,
            sel,
            filter,
            target,
        } => {
            let shown: Vec<PickerItem> =
                model_filtered(items, filter).into_iter().cloned().collect();
            let title = match target {
                crate::app::ModelTarget::Agent => "models",
                crate::app::ModelTarget::Compact => "compact models",
            };
            draw_picker_items(frame, body, title, &shown, *sel, filter);
        }
        Overlay::Session { items, sel, filter } => {
            let shown: Vec<PickerItem> =
                session_filtered(items, filter).into_iter().cloned().collect();
            draw_picker_items(frame, body, "sessions", &shown, *sel, filter);
        }
        Overlay::FileBrowser { rel, entries, sel } => {
            let items: Vec<PickerItem> = entries
                .iter()
                .map(|(name, dir)| PickerItem {
                    label: if *dir { format!("📁 {name}") } else { name.clone() },
                    detail: String::new(),
                    value: name.clone(),
                })
                .collect();
            let title = format!("files · /{rel}");
            draw_picker_items(frame, body, &title, &items, *sel, "");
        }
        Overlay::FileView {
            path,
            lines,
            scroll,
            truncated,
        } => {
            let height = body.height.saturating_sub(1);
            let area = Rect {
                height,
                ..body
            };
            frame.render_widget(Clear, area);
            let rendered: Vec<Line> = lines
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    Line::from(vec![
                        Span::styled(
                            format!("{:>5} ", index + 1),
                            Style::default().fg(DIM),
                        ),
                        Span::styled(line.clone(), Style::default().fg(TEXT)),
                    ])
                })
                .collect();
            let flag = if *truncated { " (truncated)" } else { "" };
            let paragraph = Paragraph::new(rendered).scroll((*scroll as u16, 0)).block(
                Block::bordered()
                    .title(format!(" {path}{flag} · j/k scroll · ← tree · q close "))
                    .border_style(Style::default().fg(BORDER)),
            );
            frame.render_widget(paragraph, area);
        }
        Overlay::Settings { sel } => {
            let ctx_pct = if app.context_limit > 0 {
                (app.context_tokens as f64 * 100.0 / app.context_limit as f64) as usize
            } else {
                0
            };
            let rows = [
                PickerItem {
                    label: format!("agent model  ·  {}", app.agent_model),
                    detail: "select to switch the working model".to_string(),
                    value: String::new(),
                },
                PickerItem {
                    label: format!("compact model  ·  {}", app.compact_model),
                    detail: "select to switch the summary model".to_string(),
                    value: String::new(),
                },
                PickerItem {
                    label: format!("workspace  ·  {}", app.cwd),
                    detail: "Enter shows the /cwd hint".to_string(),
                    value: String::new(),
                },
                PickerItem {
                    label: format!(
                        "session  ·  {}…  ctx {ctx_pct}%",
                        app.session_id.chars().take(8).collect::<String>()
                    ),
                    detail: "Enter prints full session stats".to_string(),
                    value: String::new(),
                },
            ];
            draw_picker_items(frame, body, "settings", &rows, *sel, "");
        }
        Overlay::Hotkeys => {
            let lines = vec![
                Line::from(Span::styled(
                    "keys",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                kv("enter", "send · queue when busy"),
                kv("alt+enter", "queue follow-up"),
                kv("esc", "cancel turn · clear input · close overlay"),
                kv("ctrl+c", "clear editor · quit when empty"),
                kv("ctrl+d", "quit"),
                kv("ctrl+o", "collapse/expand tool output"),
                kv("ctrl+t", "show/hide thinking"),
                kv("ctrl+l", "model picker"),
                kv("ctrl+r", "session picker"),
                kv("ctrl+f", "file browser"),
                kv("ctrl+x", "copy last reply"),
                kv("ctrl+g", "follow transcript end"),
                kv("ctrl+m", "toggle mouse: off = terminal native select"),
                kv("drag select", "copy selected transcript text"),
                kv("pgup/pgdn · wheel", "scroll transcript"),
                Line::from(""),
                kv("/help", "command list"),
            ];
            draw_info_block(frame, body, lines);
        }
        Overlay::Todos => {
            let mut lines = vec![Line::from(Span::styled(
                "todos",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))];
            if app.todo.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  none",
                    Style::default().fg(DIM),
                )));
            }
            for item in &app.todo {
                let color = match item.status {
                    rustedharness_core::tools::todo::TodoStatus::Completed => GREEN,
                    rustedharness_core::tools::todo::TodoStatus::InProgress => YELLOW,
                    rustedharness_core::tools::todo::TodoStatus::Pending => MUTED,
                };
                lines.push(Line::from(Span::styled(
                    format!("  {} {}", item.status.symbol(), item.content),
                    Style::default().fg(color),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  any key to close",
                Style::default().fg(DIM),
            )));
            draw_info_block(frame, body, lines);
        }
        Overlay::Processes => {
            let mut lines = vec![Line::from(Span::styled(
                "processes",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))];
            if app.processes.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  none",
                    Style::default().fg(DIM),
                )));
            }
            for (pid, p) in &app.processes {
                let color = if p.status == "running" { YELLOW } else { GREEN };
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {pid} {} {}",
                        p.status,
                        p.exit_code
                            .map(|c| format!("exit {c}"))
                            .unwrap_or_default()
                    ),
                    Style::default().fg(color),
                )));
                for screen in p.screen.lines().take(3) {
                    lines.push(Line::from(Span::styled(
                        format!("    {}", clip(screen, 100)),
                        Style::default().fg(DIM),
                    )));
                }
            }
            lines.push(Line::from(Span::styled(
                "  any key to close",
                Style::default().fg(DIM),
            )));
            draw_info_block(frame, body, lines);
        }
        Overlay::Lsp => {
            let mut lines = vec![Line::from(Span::styled(
                "language servers",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))];
            if app.lsp.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  idle",
                    Style::default().fg(DIM),
                )));
            }
            for status in &app.lsp {
                let color = match status.state.as_str() {
                    "running" => GREEN,
                    "warming" | "downloading" => YELLOW,
                    "degraded" => RED,
                    _ => DIM,
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {:10} {:10} {}",
                        status.language,
                        status.state,
                        clip(status.detail.as_deref().unwrap_or(""), 44)
                    ),
                    Style::default().fg(color),
                )));
            }
            lines.push(Line::from(Span::styled(
                "  /checksum <id> · /install <id> · any key to close",
                Style::default().fg(DIM),
            )));
            draw_info_block(frame, body, lines);
        }
    }
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key:24}"), Style::default().fg(BLUE)),
        Span::styled(value.to_string(), Style::default().fg(MUTED)),
    ])
}

pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

fn draw_info_block(frame: &mut Frame, body: Rect, lines: Vec<Line<'static>>) {
    // Clamp to the body height: a box taller than `body` would spill past its
    // bottom edge into the editor and status rows (the "input printed onto the
    // footer" bug on short terminals).
    let height = (lines.len() as u16 + 2).min(body.height);
    let area = Rect {
        y: body.y + body.height.saturating_sub(height),
        height,
        ..body
    };
    frame.render_widget(Clear, area);
    let paragraph = Paragraph::new(lines).block(
        Block::bordered()
            .border_style(Style::default().fg(BORDER_DIM))
            .style(Style::default().bg(TOOL_BG)),
    );
    frame.render_widget(paragraph, area);
}

fn draw_picker_items(
    frame: &mut Frame,
    body: Rect,
    title: &str,
    items: &[PickerItem],
    sel: usize,
    filter: &str,
) {
    let height = (items.len().min(12) + 3) as u16;
    let height = height.min(body.height);
    let area = Rect {
        y: body.y + body.height.saturating_sub(height),
        height,
        ..body
    };
    frame.render_widget(Clear, area);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let head = if filter.is_empty() {
        format!(" {title}")
    } else {
        format!(" {title} · filter {filter}")
    };
    lines.push(Line::from(Span::styled(
        head,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches",
            Style::default().fg(DIM),
        )));
    }
    let window = (height as usize).saturating_sub(3).max(1);
    let start = sel.saturating_sub(window / 2);
    // Reserve room for the two side borders. Clipping each row to this width is
    // what stops a long "label + detail" line from soft-wrapping and stacking a
    // stray tail character (e.g. the "M" of "ctx 1.0M") down the column.
    let maxw = body.width.saturating_sub(2) as usize;
    for (index, item) in items.iter().enumerate().skip(start).take(window) {
        let selected = index == sel;
        let style = if selected {
            Style::default().fg(TEXT).bg(SELECTED_BG)
        } else {
            Style::default().fg(MUTED)
        };
        let marker = if selected { "❯" } else { " " };
        let mut spans = vec![Span::styled(
            format!("{marker} {}", clip(&item.label, 42)),
            style,
        )];
        if !item.detail.is_empty() {
            spans.push(Span::styled(
                format!("  {}", clip(&item.detail, 60)),
                Style::default().fg(DIM),
            ));
        }
        let mut line = Line::from(spans);
        // Hard-clip to the inner width; a single row must never wrap.
        if line.width() > maxw {
            line = clip_line(line, maxw);
        }
        lines.push(line);
    }
    if items.len() > window {
        lines.push(Line::from(Span::styled(
            format!("  {} of {} shown", window.min(items.len()), items.len()),
            Style::default().fg(DIM),
        )));
    }
    // Opaque background so nothing bleeds through from the transcript below.
    let paragraph = Paragraph::new(lines).block(
        Block::bordered()
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(BG_BASE)),
    );
    frame.render_widget(paragraph, area);
}

/// Trim a line's spans so its total display width fits `max`, dropping any
/// overflow instead of letting the Paragraph wrap it.
fn clip_line(line: Line<'static>, max: usize) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        let w = span.width();
        if used + w <= max {
            used += w;
            out.push(span);
            continue;
        }
        let keep = max - used;
        let trimmed: String = span.content.chars().take(keep).collect();
        if !trimmed.is_empty() {
            out.push(Span::styled(trimmed, span.style));
        }
        break;
    }
    Line::from(out)
}
