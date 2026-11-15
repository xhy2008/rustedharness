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

pub(crate) fn demo_app() -> App {
    // Everything this harness writes while a preview runs - sessions, and any
    // setting a test toggles - has to land in the temp directory, never in the
    // real one.
    let scratch = std::env::temp_dir().join("rustedharness-ui-preview");
    let config = Config {
        model: "anthropic/claude-sonnet-4.5".into(),
        compact_model: "anthropic/claude-haiku-4.5".into(),
        session: rustedharness_core::config::SessionConfig {
            dir: Some(scratch.join("sessions")),
            max_saved_sessions: 1,
        },
        source: Some(scratch),
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
    app.lsp = vec![
        rustedharness_core::lsp::ServerStatus {
            language: "rust".into(),
            server: "rust-analyzer".into(),
            version: Some("2026-11-12".into()),
            state: "running".into(),
            detail: None,
            pid: Some(4242),
            restarts: 0,
            install_hint: "rustup component add rust-analyzer".into(),
            runtime_deps: Vec::new(),
        },
        rustedharness_core::lsp::ServerStatus {
            language: "lua".into(),
            server: "lua-language-server".into(),
            version: Some("3.19.2".into()),
            state: "warming".into(),
            detail: None,
            pid: Some(4243),
            restarts: 0,
            install_hint: "download from github.com/LuaLS/lua-language-server/releases".into(),
            runtime_deps: Vec::new(),
        },
    ];

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
         tool row carries its own expand state.\n\
         \n\
         | check | result | note |\n\
         | --- | :---: | ---: |\n\
         | folds on finish | yes | 3 tests |\n\
         | click to toggle | yes | owner map |\n\
         | scroll costs | 0 bytes | no repaint |",
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
    let backend = terminal.backend();
    let cursor = backend.cursor_position();
    serde_json::json!({
        "width": width,
        "height": height,
        "rows": rows,
        "cursor": { "x": cursor.x, "y": cursor.y, "visible": backend.cursor_visible() },
    })
    .to_string()
}

#[test]
#[ignore = "writes target/ui-preview.json; run on demand"]
fn preview() {
    let mut app = demo_app();
    let json = dump(&mut app, 108, 52);
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
        prefix: String::new(),
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
/// Text of one rendered row, for asserting on what the screen really shows.
fn row_text(json: &str, row: usize) -> String {
    let value: serde_json::Value = serde_json::from_str(json).expect("preview json");
    value["rows"]
        .as_array()
        .and_then(|rows| rows.get(row))
        .and_then(|cells| cells.as_array())
        .map(|cells| {
            cells
                .iter()
                .map(|cell| cell[0].as_str().unwrap_or(" "))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_status_line_reports_the_language_server() {
    let mut app = demo_app();
    let json = dump(&mut app, 120, 20);
    let footer = row_text(&json, 19);
    assert!(footer.contains("lsp lua warm +1"), "{footer}");

    // Without a live server the segment disappears instead of showing "none".
    let mut app = demo_app();
    app.lsp.clear();
    let json = dump(&mut app, 120, 20);
    let footer = row_text(&json, 19);
    assert!(!footer.contains("lsp "), "{footer}");
}

/// A narrow terminal must shed the token counts before the language server,
/// because that is the only field that changes on its own.
#[test]
fn the_language_server_survives_narrow_status_lines() {
    let mut app = demo_app();
    let json = dump(&mut app, 100, 20);
    let footer = row_text(&json, 19);
    assert!(footer.contains("lsp lua warm +1"), "{footer}");
    assert!(!footer.contains("in 31.2k"), "{footer}");
}

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
                    .starts_with("+ [bash]")
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

/// The file browser takes the whole screen instead of a 12-row popup.
#[test]
#[ignore = "writes target/ui-preview-files.json; run on demand"]
fn preview_files_fullscreen() {
    let mut app = demo_app();
    app.open_file_browser("");
    parent_dump(&mut app, "ui-preview-files.json", 100, 24);
}

/// With compaction on the main model there is no second column to show.
#[test]
fn the_compact_column_disappears_when_it_is_the_same_model() {
    let mut app = demo_app();
    app.agent_model = "claude-sonnet-4.5".into();
    app.compact_model = "claude-haiku-4.5".into();
    let json = dump(&mut app, 140, 20);
    assert!(row_text(&json, 19).contains("compact claude-haiku-4.5"));

    let mut app = demo_app();
    app.agent_model = "claude-sonnet-4.5".into();
    app.compact_model = "claude-sonnet-4.5".into();
    let json = dump(&mut app, 140, 20);
    assert!(
        !row_text(&json, 19).contains("compact"),
        "{}",
        row_text(&json, 19)
    );
}

/// A download in flight: the status line, not the transcript.
#[test]
#[ignore = "writes target/ui-preview-download.json; run on demand"]
fn preview_download() {
    let mut app = demo_app();
    app.download = Some(crate::app::DownloadProgress {
        server: "clangd".into(),
        percent: Some(42),
        detail: Some("42% 12.3/29.4 MB".into()),
    });
    parent_dump(&mut app, "ui-preview-download.json", 100, 20);
}

/// The settings panel: one toggle for the compaction model.
#[test]
#[ignore = "writes target/ui-preview-settings.json; run on demand"]
fn preview_settings() {
    let mut app = demo_app();
    app.overlay = Overlay::Settings { sel: 0 };
    parent_dump(&mut app, "ui-preview-settings.json", 100, 20);

    let mut app = demo_app();
    app.another_model_for_compact = false;
    app.compact_model = app.agent_model.clone();
    app.overlay = Overlay::Settings { sel: 0 };
    parent_dump(&mut app, "ui-preview-settings-off.json", 100, 20);
}

/// The `/lsp` panel: installed version, live state, and the subcommands that
/// replaced `/install` and `/checksum`.
#[test]
#[ignore = "writes target/ui-preview-lsp.json; run on demand"]
fn preview_lsp_panel() {
    let mut app = demo_app();
    app.lsp = vec![
        rustedharness_core::lsp::ServerStatus {
            language: "rust".into(),
            server: "rust-analyzer".into(),
            version: Some("2026-11-12".into()),
            state: "running".into(),
            detail: None,
            pid: Some(4242),
            restarts: 0,
            install_hint: "rustup component add rust-analyzer".into(),
            runtime_deps: Vec::new(),
        },
        rustedharness_core::lsp::ServerStatus {
            language: "lua".into(),
            server: "lua-language-server".into(),
            version: Some("3.19.2".into()),
            state: "available".into(),
            detail: Some("newer release available".into()),
            pid: None,
            restarts: 0,
            install_hint: "download from github.com/LuaLS/lua-language-server/releases".into(),
            runtime_deps: Vec::new(),
        },
    ];
    app.overlay = Overlay::Lsp;
    parent_dump(&mut app, "ui-preview-lsp.json", 100, 20);
}

/// Completing a server id after `/lsp install`: the menu switches from command
/// names to registry ids and says what each one needs.
#[test]
#[ignore = "writes target/ui-preview-lsp-ids.json; run on demand"]
fn preview_lsp_id_completion() {
    let mut app = demo_app();
    app.input = "/lsp install ".chars().collect();
    app.cursor = app.input.len();
    app.refresh_slash();
    parent_dump(&mut app, "ui-preview-lsp-ids.json", 100, 24);
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

    assert_eq!(
        lines.len(),
        bottom - top + 1,
        "one output line per screen row"
    );
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

/// Scrolling must stay an ordinary diff. Forcing a full repaint per wheel step
/// makes the screen flash, because the erase is flushed before the new frame.
#[test]
fn scrolling_does_not_force_a_full_repaint() {
    use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    let mut app = demo_app();
    app.mouse_area = Rect::new(0, 0, 80, 20);
    app.scroll = 10;
    app.follow = false;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    for (kind, expected) in [
        (MouseEventKind::ScrollUp, 7),
        (MouseEventKind::ScrollDown, 10),
    ] {
        runtime.block_on(app.handle_event(
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
            &tx,
        ));
        assert_eq!(app.scroll, expected);
        assert!(
            !app.needs_repaint,
            "scrolling must not ask for a full-screen clear"
        );
    }
}

/// East-Asian-Ambiguous characters are drawn *two cells wide* by Windows
/// consoles with a CJK font, while unicode-width counts them as one. Our
/// layout then places the rest of the row a column too far left, the tail
/// spills into cells the frame buffer thinks are blank, and those cells are
/// never repainted again — the orphaned-character bug. Only ASCII, box drawing
/// and real wide characters are safe in the UI.
#[test]
fn transcript_uses_only_single_width_glyphs() {
    fn width_safe(c: char) -> bool {
        match c as u32 {
            // Box drawing is Ambiguous by Unicode, but every Windows console
            // renders it single width (the editor frame depends on it).
            0x2500..=0x257F => true,
            // Geometric shapes, arrows, general punctuation and the ambiguous
            // Latin-1 symbols are the ones that blow up.
            0x25A0..=0x25FF | 0x2190..=0x21FF | 0x2000..=0x206F => false,
            0x00B7 | 0x00D7 => false,
            _ => true,
        }
    }

    let mut app = demo_app();
    dump(&mut app, 100, 40);
    for line in app.lines_cache.borrow().iter() {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        for c in text.chars() {
            assert!(
                width_safe(c),
                "ambiguous-width glyph {c:?} (U+{:04X}) in {text:?}",
                c as u32
            );
        }
    }
}

/// Serialise the tests that change the ambiguous-width mode: it is process
/// wide by design (the layout needs it everywhere), so two of them running in
/// parallel would each see the other's setting.
fn with_ambiguous_width(wide: bool) -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    crate::render::set_ambiguous_wide(wide);
    guard
}

/// Content (not just our own UI strings) can carry East-Asian-ambiguous
/// characters: the model writes `->` as `→`, tool output has em dashes and
/// ellipses. In a CJK console those are two cells wide while every layout
/// library counts one, so the tail of the line lands one cell outside the
/// frame buffer and stays on screen. They are folded to ASCII on the way in.
#[test]
fn ambiguous_width_content_is_folded_before_layout() {
    let _guard = with_ambiguous_width(true);
    let mut app = demo_app();
    app.entries.clear();
    let mut tool = Entry::new(EntryKind::Tool, "edit", "").with_arg(
        "rename `norm` → `length_squared` preview (name-based only; install a language \
         server to apply a precise workspace edit):",
    );
    tool.ok = Some(true);
    tool.body = "preview — 3 hunks… “quoted” ×2".into();
    app.entries.push(tool);

    let width = 100u16;
    dump(&mut app, width, 20);
    let limit = (width - 3) as usize;
    let mut seen_arrow = false;
    for line in app.lines_cache.borrow().iter() {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        for bad in ['→', '←', '—', '–', '…', '“', '”', '×', '¥'] {
            assert!(
                !text.contains(bad),
                "ambiguous {bad:?} survived in {text:?}"
            );
        }
        if text.contains("->") {
            seen_arrow = true;
        }
        assert!(line.width() <= limit, "row too wide: {text:?}");
    }
    assert!(
        seen_arrow,
        "the arrow should be folded to ASCII, not dropped"
    );
}

/// The mirror image: a console that draws ambiguous glyphs in a single cell
/// needs no folding at all. Forcing ASCII there would be a pointless loss of
/// fidelity (`->` instead of `→`), so the measured mode is what decides.
#[test]
fn ambiguous_glyphs_survive_when_the_console_draws_them_narrow() {
    let _guard = with_ambiguous_width(false);
    let mut app = demo_app();
    app.entries.clear();
    app.push(
        EntryKind::Assistant,
        "",
        "rename `norm` \u{2192} `length_squared` \u{2014} done",
    );

    dump(&mut app, 90, 12);
    let text: String = app
        .lines_cache
        .borrow()
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains('\u{2192}'),
        "arrow should be kept, not folded: {text:?}"
    );
    assert!(
        text.contains('\u{2014}'),
        "em dash should be kept, not folded: {text:?}"
    );
    assert!(
        !text.contains("->"),
        "nothing should have been folded: {text:?}"
    );

    // Back to the default so a later test in this thread starts from the safe
    // assumption even if the guard is dropped by a panic.
    crate::render::set_ambiguous_wide(true);
}

/// A turn can issue several tool calls at once and their results come back in
/// whatever order the tools finish. Each result must land on the row of *its*
/// call: a result that becomes a transcript entry of its own ("tool result")
/// breaks the transcript into unreadable fragments.
#[test]
fn tool_results_attach_to_their_call_in_any_order() {
    let mut app = demo_app();
    app.entries.clear();

    for (id, name, path) in [
        ("c1", "read", "sandbox_test/dump.txt"),
        ("c2", "grep", "alpha"),
        ("c3", "lsp", "rename"),
    ] {
        app.on_harness_event(HarnessEvent::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({ "path": path, "pattern": path }),
        });
    }
    assert_eq!(app.entries.len(), 3);

    app.on_harness_event(HarnessEvent::ToolResult {
        id: "c3".into(),
        name: "lsp".into(),
        ok: true,
        content: "rename preview: 3 files".into(),
        metadata: None,
    });
    app.on_harness_event(HarnessEvent::ToolResult {
        id: "c1".into(),
        name: "read".into(),
        ok: false,
        content: "offset 99 is beyond the end of the file".into(),
        metadata: None,
    });
    app.on_harness_event(HarnessEvent::ToolResult {
        id: "c2".into(),
        name: "grep".into(),
        ok: true,
        content: "dump.txt:1:alpha beta".into(),
        metadata: None,
    });

    assert_eq!(app.entries.len(), 3, "results must not add rows");
    assert!(
        app.entries
            .iter()
            .all(|entry| entry.kind == EntryKind::Tool),
        "only the calls themselves are in the transcript"
    );
    assert_eq!(app.entries[0].ok, Some(false), "read failed");
    assert_eq!(
        app.entries[0].body,
        "offset 99 is beyond the end of the file"
    );
    assert_eq!(app.entries[1].ok, Some(true), "grep succeeded");
    assert_eq!(app.entries[1].body, "dump.txt:1:alpha beta");
    assert_eq!(app.entries[2].ok, Some(true), "lsp succeeded");
    assert_eq!(app.entries[2].body, "rename preview: 3 files");
    assert!(
        app.entries.iter().all(|entry| !entry.expanded),
        "a finished tool folds its output"
    );
}

/// Restoring a session replays the stored messages, where a tool result is a
/// message of its own. It has to be folded back into the call, so a restored
/// conversation looks exactly like a live one.
#[test]
fn restored_session_shows_results_under_their_call() {
    use rustedharness_core::provider::{ChatMessage, FunctionCall, Role, ToolCall};

    let mut app = demo_app();
    app.entries.clear();

    let call = ChatMessage {
        role: Role::Assistant,
        tool_calls: Some(vec![ToolCall {
            id: "call_7".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: r#"{"path":"src/main.rs"}"#.into(),
            },
        }]),
        ..ChatMessage::default()
    };
    let result = ChatMessage {
        role: Role::Tool,
        content: Some("  1  fn main() {}".into()),
        tool_call_id: Some("call_7".into()),
        name: Some("read".into()),
        ..ChatMessage::default()
    };
    app.push_message(call);
    app.push_message(result);

    assert_eq!(app.entries.len(), 1, "the result joins its call");
    let entry = &app.entries[0];
    assert_eq!(entry.title, "read");
    assert_eq!(entry.body, "1  fn main() {}");
    assert_eq!(entry.ok, Some(true));
    assert!(
        app.entries.iter().all(|e| e.title != "tool result"),
        "no free-floating result rows"
    );
}

/// A tool call belongs to the text above it: no blank row between the sentence
/// that asked for the work and the call itself, and none between calls.
#[test]
fn tool_rows_sit_tight_under_the_text_that_asked_for_them() {
    let mut app = demo_app();
    app.entries.clear();
    app.push(EntryKind::User, "", "check the dump");
    app.push(EntryKind::Assistant, "", "Reading it now.");
    for (id, path) in [("a", "dump.txt"), ("b", "demo.rs")] {
        app.on_harness_event(HarnessEvent::ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": path }),
        });
    }
    app.push(EntryKind::Assistant, "", "Both parsed.");

    dump(&mut app, 80, 30);
    let rows: Vec<String> = app
        .lines_cache
        .borrow()
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let text_row = rows
        .iter()
        .position(|row| row.contains("Reading it now."))
        .expect("prose row");
    assert!(
        rows[text_row + 1].starts_with("~ [read]"),
        "the call must start on the very next row: {:?}",
        &rows[text_row..=text_row + 2]
    );
    let tool_rows: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains("[read]"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(tool_rows.len(), 2);
    // Second call follows the first call's peek line without an empty row.
    assert!(
        !rows[tool_rows[1] - 1].trim().is_empty() || rows[tool_rows[1] - 1].contains('└'),
        "calls stay in one block: {:?}",
        &rows[tool_rows[0]..=tool_rows[1]]
    );
}

/// The caret must be the terminal's own cursor, sitting inside the composer.
/// A console draws the IME composition string and anchors the candidate window
/// at that cursor; a caret painted as a glyph (what we had) leaves the pinyin
/// floating wherever the last frame happened to leave the cursor.
#[test]
fn the_caret_is_a_real_terminal_cursor_inside_the_composer() {
    let mut app = demo_app();
    app.input = "你好 wor".chars().collect();
    app.cursor = app.input.len();

    let dump: serde_json::Value = serde_json::from_str(&dump(&mut app, 80, 12)).expect("json");
    let cursor = &dump["cursor"];
    assert_eq!(cursor["visible"], true, "the cursor has to be visible");

    let (x, y) = (
        cursor["x"].as_u64().expect("x") as usize,
        cursor["y"].as_u64().expect("y") as usize,
    );
    let cells = dump["rows"][y].as_array().expect("row");
    let symbol = |index: usize| cells[index][0].as_str().unwrap_or_default().to_string();
    assert_eq!(symbol(1), ">", "caret row is not the composer");
    // "你好 wor" is two wide glyphs (each followed by a continuation cell), a
    // space and three latin letters, so the insertion point follows the `r`.
    assert_eq!(symbol(x - 1), "r", "caret is not at the insertion point");
    assert_eq!(
        symbol(x).trim(),
        "",
        "the cell under the caret must be empty"
    );
}

/// Opening, closing or shrinking a panel must not need a screen clear: ratatui
/// resets the draw buffer each frame, so every cell the panel used to cover is
/// diffed back to blank. Clearing the screen instead makes the whole window
/// flash on every keystroke.
#[test]
fn closing_a_panel_leaves_no_ghost_rows() {
    let mut app = demo_app();
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).expect("terminal");

    app.overlay = Overlay::Hotkeys;
    terminal
        .draw(|frame| crate::render::draw(&mut app, frame))
        .expect("draw");
    let with_panel = count_rows_with(&terminal, '╭');
    assert_eq!(with_panel, 2, "panel border plus the composer");

    app.overlay = Overlay::None;
    terminal
        .draw(|frame| crate::render::draw(&mut app, frame))
        .expect("draw");
    assert_eq!(
        count_rows_with(&terminal, '╭'),
        1,
        "the closed panel must not leave its border behind"
    );

    fn count_rows_with(terminal: &Terminal<TestBackend>, glyph: char) -> usize {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .filter(|y| (0..buffer.area.width).any(|x| buffer[(x, *y)].symbol().contains(glyph)))
            .count()
    }
}

/// The panel identity must ignore how many rows the filter currently matches,
/// otherwise every character typed into a picker reads as "the overlay
/// changed" and triggers a repaint pass over the whole frame.
#[test]
fn narrowing_a_picker_filter_keeps_the_overlay_identity() {
    let mut app = demo_app();
    let item = |label: &str| PickerItem {
        label: label.into(),
        detail: String::new(),
        value: label.into(),
    };
    app.overlay = Overlay::Slash {
        prefix: String::new(),
        items: vec![item("/help"), item("/model"), item("/sessions")],
        sel: 0,
    };
    let wide = app.overlay_key();
    app.overlay = Overlay::Slash {
        prefix: String::new(),
        items: vec![item("/model")],
        sel: 0,
    };
    assert_eq!(
        wide,
        app.overlay_key(),
        "typing must not change the identity"
    );
    app.overlay = Overlay::None;
    assert_ne!(wide, app.overlay_key(), "closing is a change");
}

/// The models write tables constantly. Parsing and column layout come from
/// `tui-markdown`; what this asserts is that the pipe source is gone and the
/// grid fits the pane.
#[test]
fn markdown_tables_render_as_a_grid() {
    let mut app = demo_app();
    app.entries.clear();
    app.push(
        EntryKind::Assistant,
        "",
        "| check | result |\n| --- | :---: |\n| folds | yes |\n| scroll | 0 bytes |",
    );

    let width = 60u16;
    dump(&mut app, width, 16);
    let rows: Vec<String> = app
        .lines_cache
        .borrow()
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();

    assert!(
        rows.iter()
            .any(|row| row.contains('┌') && row.contains('┬')),
        "no table border: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("│ check")),
        "header cell missing: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("│ 0 bytes │")),
        "body cell missing: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("| ---")),
        "raw markdown source leaked through: {rows:?}"
    );
    for row in &rows {
        assert!(
            ratatui::text::Line::raw(row.clone()).width() <= width as usize,
            "table row overflows the pane: {row:?}"
        );
    }
}

/// A wide table has to shrink to the pane instead of running off the right
/// edge - the whole point of handing the width to the renderer.
#[test]
fn a_table_too_wide_for_the_pane_is_wrapped() {
    let mut app = demo_app();
    app.entries.clear();
    app.push(
        EntryKind::Assistant,
        "",
        "| a much longer heading | another long heading | third |\n\
         | --- | --- | --- |\n\
         | a cell with a good deal of prose in it | and another one | x |",
    );

    let width = 44u16;
    dump(&mut app, width, 20);
    for line in app.lines_cache.borrow().iter() {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line.width() <= width as usize, "row overflows: {text:?}");
    }
}

/// The viewer keeps its own line numbers, so the highlighter has to preserve
/// the file's line count exactly - no trimming, no reflowing.
#[test]
fn highlighting_keeps_one_row_per_source_line() {
    let source = "fn main() {\n    let x = 1; // note\n\n    println!(\"{x}\");\n}\n";
    let lines = crate::markdown::highlight(source, "rust");
    assert_eq!(lines.len(), 5, "one row per source line: {lines:?}");
    assert!(
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style.fg.is_some()),
        "no token was coloured"
    );
    assert!(
        crate::markdown::language_for("src/main.rs") == "rust"
            && crate::markdown::language_for("Cargo.toml") == "toml"
            && crate::markdown::language_for("notes.zzz").is_empty(),
        "extension mapping is wrong"
    );
}

#[test]
#[ignore = "writes target/ui-preview-file.json; run on demand"]
fn preview_file_view() {
    let mut app = demo_app();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/theme.rs");
    let source = std::fs::read_to_string(path).expect("theme.rs");
    let lines = crate::markdown::highlight(&source, "rust");
    app.overlay = Overlay::FileView {
        path: "crates/tui/src/theme.rs".into(),
        lines,
        scroll: 6,
        truncated: false,
    };
    parent_dump(&mut app, "ui-preview-file.json", 100, 30);
}

/// Opening a file must not stall the UI: the viewer highlights once, when the
/// file is opened, so this is the budget that matters.
#[test]
#[ignore = "timing; run on demand"]
fn highlight_budget() {
    let unit = "pub const TEXT: Color = Color::Rgb(0xe4, 0xe5, 0xea); // reading colour\n";
    for count in [100usize, 400, 1_600] {
        let source = unit.repeat(count);
        let started = std::time::Instant::now();
        let lines = crate::markdown::highlight(&source, "rust");
        println!(
            "{count} lines -> {} rows in {:?}",
            lines.len(),
            started.elapsed()
        );
    }
}

/// Highlighting is asynchronous, so the result can arrive after the user closed
/// the viewer or opened a different file. It must never paint into a viewer it
/// does not belong to.
#[test]
fn a_finished_highlight_only_lands_on_its_own_file() {
    use ratatui::text::Line;

    let mut app = demo_app();
    app.overlay = Overlay::FileView {
        path: "a.rs".into(),
        lines: vec![],
        scroll: 0,
        truncated: false,
    };

    app.on_op_result(crate::app::OpResult::Highlighted {
        path: "b.rs".into(),
        lines: vec![Line::from("wrong")],
    });
    match &app.overlay {
        Overlay::FileView { lines, .. } => {
            assert!(lines.is_empty(), "a stale result must be dropped")
        }
        _ => panic!("the viewer closed"),
    }

    app.on_op_result(crate::app::OpResult::Highlighted {
        path: "a.rs".into(),
        lines: vec![Line::from("coloured")],
    });
    match &app.overlay {
        Overlay::FileView { lines, .. } => {
            assert_eq!(lines.len(), 1, "the matching result is swapped in")
        }
        _ => panic!("the viewer closed"),
    }
}

/// Resuming a session used to blank the footer: the meters are driven by
/// provider usage reports, and a restored transcript has none of its own. The
/// session file now carries the last report, so the numbers stay put.
#[test]
fn resuming_restores_the_token_and_context_meters() {
    use rustedharness_core::protocol::SessionSnapshot;
    use rustedharness_core::provider::Usage;

    let mut app = demo_app();
    app.populate(SessionSnapshot {
        session_id: "s1".into(),
        cwd: "/tmp".into(),
        agent_model: "model".into(),
        compact_model: "compact".into(),
        messages: Vec::new(),
        todos: Vec::new(),
        context_tokens: 12_345,
        context_limit: 200_000,
        usage: Some(Usage {
            prompt_tokens: 1_200,
            completion_tokens: 80,
            total_tokens: 12_345,
            cached_tokens: 1_100,
            cost_usd: Some(0.004),
        }),
        cost_usd: Some(0.42),
    });

    assert_eq!(app.context_tokens, 12_345, "context meter");
    assert_eq!(app.context_limit, 200_000);
    assert_eq!(app.input_tokens, 1_200, "prompt tokens");
    assert_eq!(app.output_tokens, 80, "completion tokens");
    assert_eq!(app.cache_hit_pct, Some(91), "cache ratio");
    assert_eq!(app.cost_usd, Some(0.42), "cost is the session total");
}
