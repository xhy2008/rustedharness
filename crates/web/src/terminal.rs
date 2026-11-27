//! An interactive shell for the right-hand panel.
//!
//! The pty is owned by the WebSocket connection, so the shell lives exactly as
//! long as the tab. Output is pushed as a rendered screen snapshot rather than
//! raw bytes: the browser needs no ANSI parser, and a slow tab cannot build up
//! a backlog of escape sequences it would only replay to reach the same screen.
//!
//! Everything here is blocking and runs on the pty reader thread; only JSON
//! strings travel on the async channel.

use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use vt100::Parser;

use rustedharness_core::shell::{answer_terminal_queries, ShellSpec};

/// Minimum gap between screen snapshots. A command writing in a tight loop
/// would otherwise send one snapshot per 8 KiB read.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(24);

pub struct Terminal {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    screen: Arc<Mutex<Parser>>,
    alive: Arc<AtomicBool>,
}

/// Clamp both axes into a range a pty and the parser accept.
fn size(rows: u16, cols: u16) -> (NonZeroU16, NonZeroU16) {
    (
        NonZeroU16::new(rows.clamp(4, 200)).expect("clamped above zero"),
        NonZeroU16::new(cols.clamp(20, 500)).expect("clamped above zero"),
    )
}

impl Terminal {
    pub fn open(
        cwd: &Path,
        spec: &ShellSpec,
        cols: u16,
        rows: u16,
        out: UnboundedSender<String>,
    ) -> Result<Self, String> {
        let cols = cols.clamp(20, 500);
        let rows = rows.clamp(4, 200);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| format!("cannot allocate a pty: {err}"))?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLUMNS", cols.to_string());
        cmd.env("LINES", rows.to_string());
        cmd.env("CLICOLOR_FORCE", "1");
        let _child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| format!("cannot start {}: {err}", spec.program.display()))?;
        drop(pair.slave);

        let killer = _child.clone_killer();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| format!("cannot read the pty: {err}"))?;
        // The writer is shared: the connection types keys with it, and the
        // reader thread uses it to answer ConPTY's startup cursor query (which
        // otherwise blocks all child output on Windows).
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(|err| format!("cannot write the pty: {err}"))?,
        ));
        let reader_writer = writer.clone();
        let (screen_rows, screen_cols) = size(rows, cols);
        let screen = Arc::new(Mutex::new(Parser::new(screen_rows, screen_cols, 0)));
        let alive = Arc::new(AtomicBool::new(true));

        let _ = out.send(
            json!({
                "type": "terminal_state",
                "state": "running",
                "shell": spec.display_name,
                "cwd": cwd.to_string_lossy(),
                "cols": cols,
                "rows": rows,
            })
            .to_string(),
        );

        let reader_out = out.clone();
        let reader_screen = screen.clone();
        let reader_alive = alive.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut pending: Vec<u8> = Vec::new();
            let mut last = Instant::now() - SNAPSHOT_INTERVAL;
            loop {
                let read = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(_) => break,
                };
                pending.extend_from_slice(&buf[..read]);
                if let Ok(mut writer) = reader_writer.lock() {
                    answer_terminal_queries(&mut pending, writer.as_mut(), rows);
                }
                if let Ok(mut parser) = reader_screen.lock() {
                    parser.process(&buf[..read]);
                }
                // Coalesce bursts, but never drop the last chunk of a quiet
                // moment: that is exactly where a prompt or a command's result
                // arrives, and the browser would keep showing the previous
                // screen until more output appeared.
                let elapsed = last.elapsed();
                if elapsed < SNAPSHOT_INTERVAL {
                    std::thread::sleep(SNAPSHOT_INTERVAL - elapsed);
                }
                last = Instant::now();
                if send_snapshot(&reader_out, &reader_screen).is_err() {
                    break;
                }
            }
            let _ = send_snapshot(&reader_out, &reader_screen);
            if reader_alive.swap(false, Ordering::SeqCst) {
                let _ = reader_out
                    .send(json!({ "type": "terminal_state", "state": "closed" }).to_string());
            }
        });

        Ok(Self {
            master: pair.master,
            writer,
            killer,
            screen,
            alive,
        })
    }

    /// Write keys straight to the pty: the browser sends the same escape
    /// sequences a terminal emulator would.
    pub fn write(&mut self, data: &str) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(data.as_bytes());
            let _ = writer.flush();
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Value {
        let cols = cols.clamp(20, 500);
        let rows = rows.clamp(4, 200);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut parser) = self.screen.lock() {
            let (rows, cols) = size(rows, cols);
            parser.set_size(rows, cols);
        }
        json!({ "type": "terminal_size", "cols": cols, "rows": rows })
    }

    pub fn snapshot(&self) -> Value {
        screen_json(&self.screen)
    }
}

fn screen_json(screen: &Arc<Mutex<Parser>>) -> Value {
    let parser = match screen.lock() {
        Ok(parser) => parser,
        Err(_) => return json!({ "type": "terminal_output", "screen": "" }),
    };
    let view = parser.screen();
    let (rows, cols) = view.size();
    let (row, col) = view.cursor_position();
    json!({
        "type": "terminal_output",
        "screen": view.contents(),
        "rows": rows,
        "cols": cols,
        "cursor_row": row,
        "cursor_col": col,
        "cursor_hidden": view.hide_cursor(),
        "alternate": view.alternate_screen(),
    })
}

fn send_snapshot(out: &UnboundedSender<String>, screen: &Arc<Mutex<Parser>>) -> Result<(), ()> {
    out.send(screen_json(screen).to_string()).map_err(|_| ())
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.killer.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_snapshot_is_valid_json() {
        let (rows, cols) = size(6, 40);
        let screen = Arc::new(Mutex::new(Parser::new(rows, cols, 0)));
        screen.lock().unwrap().process(b"hello\r\nworld");
        let value = screen_json(&screen);
        assert_eq!(value["type"], "terminal_output");
        assert!(value["screen"].as_str().unwrap().contains("hello"));
        assert_eq!(value["rows"], 6);
        assert_eq!(value["cols"], 40);
    }
}
