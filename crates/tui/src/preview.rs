//! Offline UI preview: builds an `App` full of representative entries and
//! dumps the rendered cell grid so the layout can be inspected (and turned
//! into an image) without a live provider or a real terminal.
//!
//! Run with `cargo test -p rustedharness-tui -- --ignored preview`. The dump
//! lands in `target/ui-preview.json`.

use std::sync::Arc;

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use rustedharness_core::config::Config;
use rustedharness_core::protocol::HarnessEvent;
use rustedharness_core::provider::{Backend, ChatRequest, Completion, StreamEvent};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::app::{App, Overlay, PickerItem};
use crate::entries::{Entry, EntryKind};

struct Stub;

#[async_trait::async_trait]
impl Backend for Stub {
    async fn stream_chat(
        &self,
        _request: ChatRequest,
        _sink: Option<UnboundedSender<StreamEvent>>,
        _cancel: CancellationToken,
    ) -> rustedharness_core::error::Result<Completion> {
        Ok(Completion::default())
    }
}

fn demo_app() -> App {
    let config = Config {
        model: "anthropic/claude-sonnet-4.5".into(),
        compact_model: "anthropic/claude-haiku-4.5".into(),
        session: rustedharness_core::config::SessionConfig {
            dir: Some(std::env::temp_dir().join("rustedharness-ui-preview")),
            max_saved_sessions: 1,
        },
        ..Config::default()
    };

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = rt
        .block_on(rustedharness_core::agent::Agent::with_backend(
            config.clone(),
            std::env::current_dir().expect("cwd"),
            tx,
            Arc::new(Stub),
        ))
        .expect("agent");

    let mut app = App::new(agent, config);
    app.cwd = "E:/rustedharness".into();
    app.context_tokens = 24_800;
    app.context_limit = 200_000;
    app.input_tokens = 31_240;
    app.output_tokens = 4_180;
    app.cache_hit_pct = Some(92);
    app.cost_usd = Some(0.0431);
    app.agent_model = "claude-sonnet-4.5".into();
    app.compact_model = "claude-haiku-4.5".into();
    app.todo = Vec::new();
    app.entries.clear();

    app.push(
        EntryKind::User,
        "",
        "the transcript looks cramped. can you make the tool calls quieter and give the \n\
         composer a real frame?\n\n\t- tabs in a paste must not shift the row\n\t- nor may a \\x1b[31mraw escape\\x1b[0m",
    );
    let mut thinking = Entry::new(
        EntryKind::Reasoning,
        "thinking",
        "The band behind the user message is doing too much work, and every tool call draws a \
         three-line box. I should keep one accent colour and let whitespace separate blocks.\n\
         Finished blocks can fold to a single line so a long session stays readable.",
    );
    thinking.expanded = true;
    app.entries.push(thinking);
    app.push(
        EntryKind::Assistant,
        "",
        "Two changes, both in `render.rs`:\n\n\
         - tool calls collapse to a single `[name] argument` row\n\
         - the composer becomes a rounded box that lights up while a turn runs\n\n\
         ```rust\n\
         let block = Block::bordered()\n\
             .border_type(BorderType::Rounded)\n\
             .border_style(style(accent));\n\
         ```\n\n\
         Status glyphs keep their colour, everything else drops to grey. Press **ctrl+o** to \
         expand output.",
    );
    let mut tool = Entry::new(EntryKind::Tool, "bash", "").with_arg("cargo test --workspace");
    tool.ok = Some(true);
    tool.expanded = false;
    tool.body = "running 142 tests\n...\ntest result: ok. 142 passed; 0 failed".into();
    app.entries.push(tool);

    let mut tool = Entry::new(EntryKind::Tool, "read", "").with_arg("crates/tui/src/render.rs");
    tool.ok = Some(true);
    tool.expanded = true;
    tool.body = "  1  //! Pi-style renderer.\n  2  //!\n  3  //! Visual grammar, top to bottom:\n  4  //! * a masthead block that names the session,".into();
    app.entries.push(tool);

    let mut tool = Entry::new(EntryKind::Tool, "grep", "").with_arg("fn draw  in crates/tui");
    tool.ok = Some(false);
    tool.expanded = true;
    tool.body = "error: no matches found in crates/tui".into();
    app.entries.push(tool);

    let tool =
        Entry::new(EntryKind::Tool, "edit", "").with_arg("crates/tui/src/theme.rs  (2 edits)");
    app.entries.push(tool);

    let folded = Entry::new(
        EntryKind::Reasoning,
        "thinking",
        "Which status glyphs survive in Consolas? Box-drawing and the geometric shape block are \
         safe; dingbats are not.\nThey also need to be single width, otherwise the header shifts \
         by a column.",
    );
    app.entries.push(folded);
    app.push(
        EntryKind::Assistant,
        "",
        "Both folds are in place: thinking collapses itself when the turn moves on, and every \
         tool row carries its own expand state.",
    );
    app.push(
        EntryKind::Notice,
        "",
        "lsp rust-analyzer: ready, cache warm",
    );
    app.push(
        EntryKind::Compaction,
        "compacted 18 messages",
        "Earlier turns covered the session-file lifecycle and the LSP download flow.",
    );
    app.push(
        EntryKind::Error,
        "",
        "turn aborted: provider returned 429 after 5 attempts",
    );

    app.input = "also align the model name to the right of the status line"
        .chars()
        .collect();
    app.cursor = app.input.len();
    app.busy = false;
    app.follow = false;
    app.scroll = 0;
    app
}

fn dump(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| crate::render::draw(app, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for y in 0..height {
        let mut cells: Vec<serde_json::Value> = Vec::new();
        for x in 0..width {
            let cell = &buffer[(x, y)];
            let fg = match cell.fg {
                ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                ratatui::style::Color::Reset => "#d8d9de".to_string(),
                other => format!("{other:?}"),
            };
            let bg = match cell.bg {
                ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                _ => "#141518".to_string(),
            };
            cells.push(serde_json::json!([cell.symbol(), fg, bg]));
        }
        rows.push(serde_json::Value::Array(cells));
    }
    serde_json::json!({ "width": width, "height": height, "rows": rows }).to_string()
}

#[test]
#[ignore = "writes target/ui-preview.json; run on demand"]
fn preview() {
    let mut app = demo_app();
    let json = dump(&mut app, 108, 44);
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-preview.json");
    std::fs::write(&path, json).expect("write preview");

    let mut app = demo_app();
    let json = dump(&mut app, 64, 24);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/ui-preview-narrow.json");
    std::fs::write(&path, json).expect("write preview");
    println!("wrote previews");
}

#[test]
#[ignore = "writes target/ui-preview-picker.json; run on demand"]
fn preview_picker() {
    let mut app = demo_app();
    app.overlay = Overlay::Slash {
        items: vec![
            PickerItem {
                label: "/help".into(),
                detail: "show this command list".into(),
                value: "/help".into(),
            },
            PickerItem {
                label: "/model".into(),
                detail: "open the model picker".into(),
                value: "/model".into(),
            },
            PickerItem {
                label: "/compact-model".into(),
                detail: "switch the compaction model".into(),
                value: "/compact-model".into(),
            },
            PickerItem {
                label: "/settings".into(),
                detail: "model, compact model, workspace, session".into(),
                value: "/settings".into(),
            },
            PickerItem {
                label: "/resume".into(),
                detail: "pick a saved session".into(),
                value: "/resume".into(),
            },
        ],
        sel: 2,
    };
    app.input = "/".chars().collect();
    app.cursor = 1;
    let json = dump(&mut app, 108, 26);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/ui-preview-picker.json");
    std::fs::write(&path, json).expect("write preview");
    println!("wrote {}", path.display());
}

#[test]
#[ignore = "writes target/ui-preview-panel.json; run on demand"]
fn preview_panel() {
    let mut app = demo_app();
    app.overlay = Overlay::Hotkeys;
    parent_dump(&mut app, "ui-preview-panel.json", 100, 34);

    let mut app = demo_app();
    app.consent = Some(crate::app::ConsentCard {
        id: "rust".into(),
        display_name: "rust-analyzer".into(),
        version: "2026-11-12".into(),
        url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-11-12/rust-analyzer-x86_64-pc-windows-msvc.zip".into(),
        sha256: None,
        runtime_hint: Some("extract to ~/.rustedharness/lsp".into()),
    });
    parent_dump(&mut app, "ui-preview-consent.json", 100, 30);
}

/// The click-to-fold path end to end: render (which fills the owner map),
/// click a tool row, then click a separator row that owns nothing.
#[test]
fn clicking_a_row_folds_the_entry_that_owns_it() {
    let mut app = demo_app();
    dump(&mut app, 100, 40);

    let (row, index) = {
        let owners = app.owner_cache.borrow();
        let lines = app.lines_cache.borrow();
        let row = lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .starts_with("● [bash]")
            })
            .expect("bash row");
        (row, owners[row])
    };
    assert_ne!(index, usize::MAX, "transcript rows must map to an entry");
    assert!(!app.entries[index].expanded, "tools start folded");

    app.toggle_row(row as u16);
    assert!(app.entries[index].expanded, "click unfolds the tool output");
    app.toggle_row(row as u16);
    assert!(!app.entries[index].expanded, "second click folds it again");

    // A blank separator row owns nothing and must be a no-op.
    let blank = app
        .lines_cache
        .borrow()
        .iter()
        .position(|line| line.spans.is_empty())
        .expect("separator row");
    let before = app.entries.iter().map(|e| e.expanded).collect::<Vec<_>>();
    app.toggle_row(blank as u16);
    let after = app.entries.iter().map(|e| e.expanded).collect::<Vec<_>>();
    assert_eq!(before, after);
}

/// A reasoning block opens while it streams and folds as soon as the model
/// starts writing the answer; a tool opens while it runs and folds on result.
#[test]
fn streaming_blocks_open_themselves_and_fold_on_completion() {
    let mut app = demo_app();
    app.entries.clear();

    app.on_harness_event(HarnessEvent::AssistantReasoning {
        text: "look at the reducer".into(),
    });
    assert_eq!(app.entries.len(), 1);
    assert!(app.entries[0].expanded, "thinking streams open");
    app.on_harness_event(HarnessEvent::AssistantReasoning {
        text: " and the renderer".into(),
    });
    assert_eq!(app.entries.len(), 1, "chunks append to one entry");
    assert_eq!(app.entries[0].body, "look at the reducer and the renderer");

    app.on_harness_event(HarnessEvent::AssistantDelta {
        text: "Done.".into(),
    });
    assert!(
        !app.entries[0].expanded,
        "thinking folds when content starts"
    );

    app.on_harness_event(HarnessEvent::ToolCall {
        id: "t1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({ "command": "cargo test" }),
    });
    let tool = app.entries.len() - 1;
    assert!(
        app.entries[tool].expanded,
        "a running tool shows its output"
    );
    assert_eq!(app.entries[tool].arg, "cargo test");

    app.on_harness_event(HarnessEvent::ToolResult {
        id: "t1".into(),
        name: "bash".into(),
        ok: true,
        content: "ok. 12 passed".into(),
        metadata: None,
    });
    assert!(!app.entries[tool].expanded, "a finished tool folds");
}

#[test]
#[ignore = "writes target/ui-preview-selection.json; run on demand"]
fn preview_selection() {
    let mut app = demo_app();
    app.selection = Some(((10, 2), (30, 6)));
    parent_dump(&mut app, "ui-preview-selection.json", 108, 26);
}

#[test]
#[ignore = "writes target/ui-preview-idle.json; run on demand"]
fn preview_idle() {
    let mut app = demo_app();
    app.entries.clear();
    app.input.clear();
    app.cursor = 0;
    app.busy = true;
    app.queue.push_back("and drop the old card borders".into());
    parent_dump(&mut app, "ui-preview-idle.json", 100, 16);
}

fn parent_dump(app: &mut crate::app::App, name: &str, width: u16, height: u16) {
    let json = dump(app, width, height);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(name);
    std::fs::write(&path, json).expect("write preview");
    println!("wrote {}", path.display());
}

#[test]
fn selection_is_linear_not_rectangular() {
    let mut app = demo_app();
    dump(&mut app, 100, 40);

    let rows: Vec<String> = app
        .lines_cache
        .borrow()
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    // The thinking block: one header row plus wrapped body rows, all non-empty.
    let top = rows
        .iter()
        .position(|row| row.starts_with("v thinking"))
        .expect("expanded thinking header");
    let bottom = top + 2;
    assert!(rows[bottom].trim_end().chars().count() > 6);

    // Drag from column 6 of the first row to column 4 three rows down.
    app.selection = Some(((6, top as u16), (4, bottom as u16)));
    let text = app.selection_text().expect("selection text");
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines.len(), bottom - top + 1, "one output line per screen row");
    assert_eq!(
        lines[0],
        crate::app::slice_columns(&rows[top], 6, usize::MAX).trim_end(),
        "first row cut at the anchor"
    );
    for row in top + 1..bottom {
        assert_eq!(
            lines[row - top],
            rows[row].trim_end(),
            "middle rows are taken whole"
        );
    }
    assert_eq!(
        lines[bottom - top],
        crate::app::slice_columns(&rows[bottom], 0, 5).trim_end(),
        "last row stops at the cursor"
    );
    // A rectangular selection would clip every row to the drag columns; whole
    // middle rows are what makes it linear.
    assert!(
        lines.iter().skip(1).any(|line| line.chars().count() > 5),
        "middle rows must not be clipped to the drag columns"
    );
}

#[test]
fn slice_columns_never_splits_a_wide_character() {
    use crate::app::slice_columns;
    // 回归 = columns 0-1 and 2-3, then a/b/c at 4/5/6.
    assert_eq!(slice_columns("回归abc", 0, 2), "回");
    assert_eq!(slice_columns("回归abc", 2, 4), "归");
    assert_eq!(slice_columns("回归abc", 1, 4), "归");
    assert_eq!(slice_columns("回归abc", 0, 7), "回归abc");
    assert_eq!(slice_columns("回归abc", 4, 7), "abc");
    assert_eq!(slice_columns("回归abc", 6, 99), "c");
    assert_eq!(slice_columns("abc", 1, 2), "b");
    assert_eq!(slice_columns("", 0, 5), "");
}

/// Smoke test for the clipboard path: the platform API must round-trip
/// non-ASCII text, which the old `clip.exe` pipe could not do on a Chinese
/// Windows. Overwrites the clipboard, so it only runs on demand.
#[test]
#[ignore = "overwrites the clipboard; run on demand"]
fn clipboard_round_trips_unicode() {
    let text = "回归全部通过 — 边界用例 ✨";
    let mut clipboard = arboard::Clipboard::new().expect("clipboard");
    clipboard.set_text(text.to_string()).expect("set text");
    assert_eq!(clipboard.get_text().expect("get text"), text);
}

/// Tool output is raw file text: tabs and escape sequences must never reach a
/// span, or the terminal advances to a tab stop the frame buffer knows nothing
/// about and the row silently desyncs (the "orphaned character" bug).
#[test]
fn tool_output_cannot_desync_the_screen() {
    let mut app = demo_app();
    app.entries.clear();
    let mut entry = Entry::new(EntryKind::Tool, "read", "").with_arg("src/main.rs");
    entry.ok = Some(true);
    entry.expanded = true;
    entry.body = "\tfn main() {\n\t\tprintln!(\"\u{1b}[31mred\u{1b}[0m\");\r\n\t}\n".into();
    app.entries.push(entry);
    app.push(EntryKind::User, "", "tabs\tin\tchat\r\nlines");

    let width = 80u16;
    dump(&mut app, width, 20);
    let limit = (width - 3) as usize; // left pad + right gutter
    let lines = app.lines_cache.borrow().clone();
    assert!(!lines.is_empty());
    for line in &lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains('\t'), "unexpanded tab: {text:?}");
        assert!(
            !text.chars().any(|c| c.is_control()),
            "control character reached a span: {text:?}"
        );
        assert!(line.width() <= limit, "row wider than the area: {text:?}");
    }
}
