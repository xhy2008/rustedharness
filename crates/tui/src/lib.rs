//! Full-screen terminal UI for RustedHarness.
//!
//! The layout follows Pi's minimal terminal style: a borderless transcript,
//! filled user/tool cards, a single editor rule with an embedded status line,
//! and a compact footer with workspace, model and context statistics. All
//! state still comes from `HarnessEvent`, so the TUI and web UI stay
//! equivalent.

mod app;
mod entries;
mod markdown;

/// Highlighting entry point for the `highlight` benchmark.
#[doc(hidden)]
pub use markdown::highlight;
mod render;
mod theme;
#[cfg(test)]
mod ui_tests;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::{self, MoveTo};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use crossterm::style::Print;
use crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, DisableLineWrap, EnableLineWrap,
    EndSynchronizedUpdate,
};
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
    // Autowrap is the single worst enemy of a full-screen TUI: one row that
    // renders a cell wider than we measured (ambiguous-width glyph, CJK
    // punctuation) makes the terminal scroll, after which every diff lands on
    // the wrong row and the screen fills with orphaned characters. Turn it off
    // for the whole session and restore it on the way out.
    let _ = execute!(stdout, EnableMouseCapture, DisableLineWrap);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), EnableLineWrap);
        previous_hook(info);
    }));
    // Decide once, before the first frame, how this terminal draws
    // ambiguous-width glyphs; every layout decision follows it.
    let ambiguous_wide = ambiguous_setting();
    render::set_ambiguous_wide(ambiguous_wide);
    tracing::debug!(ambiguous_wide, "terminal ambiguous-width measurement");

    let stop = Arc::new(AtomicBool::new(false));
    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    let (op_tx, mut op_rx) = mpsc::unbounded_channel();
    spawn_key_reader(key_tx, stop.clone());

    // ratatui only sizes its buffers; it never erases the screen. The first
    // frame diffs against an empty buffer and therefore writes only the cells
    // we actually paint - anything already on screen (typically the leftover
    // alternate-screen content of a previous run) survives in every blank cell
    // and no later frame will ever touch it. That is the fixed-column row of
    // orphaned characters users kept seeing. Erase once, on the way in.
    terminal.clear()?;

    let mut app = App::new(agent, config);
    if app.agent_model.trim().is_empty() {
        app.notice("no model configured — add a provider/model to models.json");
    }
    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut key_rx,
        &mut op_rx,
        &mut rx,
        &op_tx,
    )
    .await;

    stop.store(true, Ordering::Relaxed);
    graceful_shutdown(&app.agent).await;
    let _ = execute!(stdout, DisableMouseCapture, EnableLineWrap);
    ratatui::restore();
    result
}

/// How this terminal renders ambiguous-width glyphs.
///
/// Order of precedence: an explicit `RH_AMBIWIDTH` (single|double|auto), then
/// measurement, then the conservative answer (wide, i.e. fold to ASCII).
fn ambiguous_setting() -> bool {
    let configured = std::env::var("RH_AMBIWIDTH")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase());
    match configured.as_deref() {
        Some("single" | "narrow" | "1" | "false" | "off") => false,
        Some("double" | "wide" | "2" | "true" | "on") => true,
        _ => measure_ambiguous_width().unwrap_or(true),
    }
}

/// Ask the terminal how many cells it gives an ambiguous-width glyph.
///
/// No escape sequence and no registry key says "draw `→` in one cell" - the
/// console picks, normally from its font and code page, and it can differ
/// between two consoles on the same box. So park the cursor in a known column,
/// print one ambiguous glyph and read the cursor back: one cell means the
/// console treats them as ordinary characters and the transcript can keep
/// them, two cells means they have to be folded to ASCII.
///
/// Runs inside the alternate screen before the first frame, so the probe
/// never touches scrollback; `None` means the terminal would not answer and
/// the caller should assume the wide (safe) behaviour.
fn measure_ambiguous_width() -> Option<bool> {
    const GLYPH: &str = "\u{2192}";
    let mut stdout = std::io::stdout();
    execute!(stdout, Clear(ClearType::All), MoveTo(0, 0), Print(GLYPH)).ok()?;
    std::io::Write::flush(&mut stdout).ok()?;
    let (column, _) = cursor::position().ok()?;
    Some(column >= 2)
}

/// Draw one frame.
///
/// Everything - the optional clear included - happens inside a synchronized
/// update so the terminal applies it as one atomic repaint. Without that the
/// erase lands one write before the new content, and every full repaint reads
/// as a black flash.
///
/// `force_clear` is only for the cases where the terminal's cells can no
/// longer be trusted to match the frame buffer: resize, overlay change or a
/// session switch.
fn paint(terminal: &mut ratatui::DefaultTerminal, app: &mut App, force_clear: bool) -> Result<()> {
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, BeginSynchronizedUpdate);
    let mut outcome: Result<()> = Ok(());
    if force_clear || std::mem::take(&mut app.needs_repaint) {
        app.lines_sig = (usize::MAX, 0);
        app.wide_rows_dirty = true;
        outcome = terminal.clear().map_err(anyhow::Error::from);
    }
    if outcome.is_ok() {
        outcome = terminal
            .draw(|frame| render::draw(app, frame))
            .map(|_| ())
            .map_err(anyhow::Error::from);
    }
    // Always close the block: leaving it open would freeze the screen until
    // the next write finally flushes it.
    let _ = execute!(stdout, EndSynchronizedUpdate);
    outcome
}

/// Shut the agent down but never longer than a couple of seconds: a wedged
/// LSP/MCP child must not be able to keep the TUI (and thus the terminal in
/// raw/alt-screen mode) alive after the user has asked to quit.
async fn graceful_shutdown(agent: &Arc<Agent>) {
    let _ = tokio::time::timeout(Duration::from_secs(2), agent.shutdown()).await;
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

    // Resize anti-flicker: dragging the terminal window fires a storm of
    // Resize events, and repainting at every intermediate size stacks a
    // full-buffer clear+redraw on top of conhost's own reflow — visible
    // strobing. Suppress draws while resizes keep arriving and do a single
    // forced repaint once they have been quiet for 150 ms.
    const RESIZE_SETTLE: Duration = Duration::from_millis(150);
    let mut resize_at: Option<Instant> = None;
    let mut resize_dirty = false;
    // Only paint when something changed. An unconditional 8 fps repaint would
    // redraw the same cells and, worse, re-home the terminal cursor every
    // frame - which is exactly where conhost anchors the IME composition
    // string and the candidate window while the user is typing Chinese.
    let mut dirty = true;
    loop {
        let settling = resize_at.is_some_and(|t| t.elapsed() < RESIZE_SETTLE);
        // ratatui resets the draw buffer every frame, so a panel that shrinks
        // or closes is erased by the ordinary diff - no screen clear needed.
        // Only a resize has to erase: the terminal reflowed on its own and
        // every row may hold something we never drew.
        if resize_dirty || app.needs_repaint {
            dirty = true;
        }
        if dirty && !settling {
            paint(terminal, app, resize_dirty)?;
            resize_dirty = false;
            dirty = false;
        }
        if let Some(pending) = app.pending.take() {
            let label = pending
                .resume
                .as_ref()
                .map(|id| id.chars().take(8).collect::<String>());
            graceful_shutdown(&app.agent).await;
            let (tx, new_rx) = mpsc::unbounded_channel();
            let agent = match pending.resume {
                Some(id) => Agent::resume(app.config.clone(), &id, Some(pending.cwd), tx).await?,
                None => Agent::new(app.config.clone(), pending.cwd, tx).await?,
            };
            *harness_rx = new_rx;
            app.switch_agent(agent);
            app.notice(match label {
                Some(short) => format!("resumed session {short}"),
                None => "new session".to_string(),
            });
            dirty = true;
            continue;
        }
        let toast_shown = app.active_toast().is_some();
        tokio::select! {
            biased;
            Some(event) = key_rx.recv() => {
                if matches!(event, Event::Resize(..)) {
                    resize_at = Some(Instant::now());
                    resize_dirty = true;
                }
                app.handle_event(event, op_tx).await;
                dirty = true;
            }
            Some(result) = op_rx.recv() => {
                app.on_op_result(result);
                dirty = true;
            }
            Some(event) = harness_rx.recv() => {
                app.on_harness_event(event);
                dirty = true;
            }
            _ = ticker.tick() => {
                app.tick += 1;
                // The spinner and the toast countdown are the only things that
                // move on a timer; the last repaint also has to happen on the
                // tick the toast disappears.
                if app.busy || toast_shown || app.active_toast().is_some() {
                    dirty = true;
                }
            }
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
            dirty = true;
            if let Some(event) = key {
                if matches!(event, Event::Resize(..)) {
                    resize_at = Some(Instant::now());
                    resize_dirty = true;
                }
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
