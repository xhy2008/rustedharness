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
    let mtime = "claude-sonnet-4.5";

    app.push(EntryKind::Welcome, "0.1.0", "E:/rustedharness");
    app.entries.last_mut().unwrap().arg = mtime.to_string();

    app.push(
        EntryKind::User,
        "",
        "the transcript looks cramped. can you make the tool calls quieter and give the \
         composer a real frame?",
    );
    app.push(
        EntryKind::Reasoning,
        "thinking",
        "The band behind the user message is doing too much work, and every tool call draws a \
         three-line box. I should keep one accent colour and let whitespace separate blocks.",
    );
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

#[test]
#[ignore = "writes target/ui-preview-idle.json; run on demand"]
fn preview_idle() {
    let mut app = demo_app();
    app.entries.truncate(1);
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
