//! Full-screen terminal UI for RustedHarness.
//!
//! The layout follows Pi's minimal terminal style: a borderless transcript,
//! filled user/tool cards, a single editor rule with an embedded status line,
//! and a compact footer with workspace, model and context statistics. All
//! state still comes from `HarnessEvent`, so the TUI and web UI stay
//! equivalent.

mod app;
mod entries;
mod render;
mod theme;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use rustedharness_core::agent::Agent;
use rustedharness_core::config::Config;
use rustedharness_core::protocol::HarnessEvent;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use app::{App, OpResult};

/// Entry point used by the CLI. Takes ownership of the terminal only after the
/// agent has been created, so configuration errors print normally.
pub async fn run(config: Config, cwd: PathBuf, resume: Option<String>) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = match resume {
        Some(id) => Agent::resume(config.clone(), &id, Some(cwd), tx).await?,
        None => Agent::new(config.clone(), cwd, tx).await?,
    };

    let mut terminal = ratatui::init();
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, EnableMouseCapture);
    let stop = Arc::new(AtomicBool::new(false));
    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    let (op_tx, mut op_rx) = mpsc::unbounded_channel();
    spawn_key_reader(key_tx, stop.clone());

    let mut app = App::new(agent, config);
    let result =
        event_loop(&mut terminal, &mut app, &mut key_rx, &mut op_rx, &mut rx, &op_tx).await;

    stop.store(true, Ordering::Relaxed);
    app.agent.shutdown().await;
    let _ = execute!(stdout, DisableMouseCapture);
    ratatui::restore();
    result
}

fn spawn_key_reader(tx: UnboundedSender<Event>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(25)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    key_rx: &mut UnboundedReceiver<Event>,
    op_rx: &mut UnboundedReceiver<OpResult>,
    harness_rx: &mut UnboundedReceiver<HarnessEvent>,
    op_tx: &UnboundedSender<OpResult>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    let mut last_overlay = app.overlay_key();
    loop {
        // Force a full repaint when the overlay changes (opens, closes, or
        // switches to a different size/type). Incremental diffs can leave stale
        // border glyphs in the blank transcript area on terminals that don't
        // erase untouched cells (notably the Windows console).
        let overlay = app.overlay_key();
        if overlay != last_overlay {
            app.lines_sig = (usize::MAX, 0);
            terminal.clear()?;
        }
        last_overlay = overlay;
        terminal.draw(|frame| render::draw(app, frame))?;
        if let Some(pending) = app.pending.take() {
            let label = pending
                .resume
                .as_ref()
                .map(|id| id.chars().take(8).collect::<String>());
            app.agent.shutdown().await;
            let (tx, new_rx) = mpsc::unbounded_channel();
            let agent = match pending.resume {
                Some(id) => Agent::resume(app.config.clone(), &id, Some(pending.cwd), tx).await?,
                None => Agent::new(app.config.clone(), pending.cwd, tx).await?,
            };
            *harness_rx = new_rx;
            app.agent = agent;
            app.entries.clear();
            app.scroll = 0;
            app.follow = true;
            app.busy = false;
            app.queue.clear();
            app.live_tool = None;
            app.streaming = None;
            app.reasoning_at = None;
            app.processes.clear();
            app.consent = None;
            app.populate(app.agent.snapshot());
            app.notice(match label {
                Some(short) => format!("resumed session {short}"),
                None => "new session".to_string(),
            });
            continue;
        }
        tokio::select! {
            biased;
            Some(event) = key_rx.recv() => app.handle_event(event, op_tx).await,
            Some(result) = op_rx.recv() => app.on_op_result(result),
            Some(event) = harness_rx.recv() => app.on_harness_event(event),
            _ = ticker.tick() => app.tick += 1,
        }
        // Coalesce everything already queued so one frame carries all keys,
        // streamed deltas and tool results; without this, fast input feels
        // like 8fps stutter because each event forced its own draw.
        for _ in 0..512 {
            let key = key_rx.try_recv().ok();
            let op = op_rx.try_recv().ok();
            let harness = harness_rx.try_recv().ok();
            if key.is_none() && op.is_none() && harness.is_none() {
                break;
            }
            if let Some(event) = key {
                app.handle_event(event, op_tx).await;
            }
            if let Some(result) = op {
                app.on_op_result(result);
            }
            if let Some(event) = harness {
                app.on_harness_event(event);
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}
