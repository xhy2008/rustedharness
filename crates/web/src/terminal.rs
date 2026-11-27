//! An interactive shell for the right-hand panel.
//!
//! The pty is owned by the WebSocket connection, so the shell lives exactly as
//! long as the tab. Output is forwarded as raw bytes and rendered by xterm.js:
//! it is the terminal emulator, so the browser gets the same screen a native
//! terminal would show - colours, wrapping, scrollback, mouse reporting and
//! alternate-screen programs. A server-side emulator can only ever be a second,
//! worse renderer, and it cannot give the user a scrollback or let a full-screen
//! program track the mouse.
//!
//! Everything here is blocking and runs on the pty reader thread; only JSON
//! strings travel on the async channel.

use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

use rustedharness_core::shell::{saw_terminal_query, ShellSpec};

/// Bytes accumulated before a burst is flushed to the browser, so a command
/// that writes thousands of small chunks does not become thousands of
/// WebSocket frames. A read that fills the buffer less than completely means
/// the child went quiet, and that is flushed immediately: it is where prompts
/// and command results arrive.
const FLUSH_BYTES: usize = 16 * 1024;
const READ_BYTES: usize = 16 * 1024;

pub struct Terminal {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    alive: Arc<AtomicBool>,
}

/// Clamp both axes into a range a pty accepts.
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
        cmd.env("COLORTERM", "truecolor");
        cmd.env("COLUMNS", cols.to_string());
        cmd.env("LINES", rows.to_string());
        // The pty is a terminal, so tools should colour their output for it.
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
        let reader_alive = alive.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; READ_BYTES];
            let mut burst: Vec<u8> = Vec::new();
            let mut carry: Vec<u8> = Vec::new();
            loop {
                let read = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(_) => break,
                };
                if saw_terminal_query(&mut carry, &buf[..read]) {
                    if let Ok(mut writer) = reader_writer.lock() {
                        let _ = writer.write_all(format!("\x1b[{rows};1R").as_bytes());
                        let _ = writer.flush();
                    }
                }
                burst.extend_from_slice(&buf[..read]);
                if burst.len() >= FLUSH_BYTES || read < buf.len() {
                    if send_bytes(&reader_out, &burst).is_err() {
                        break;
                    }
                    burst.clear();
                }
            }
            if !burst.is_empty() {
                let _ = send_bytes(&reader_out, &burst);
            }
            if reader_alive.swap(false, Ordering::SeqCst) {
                let _ = reader_out
                    .send(json!({ "type": "terminal_state", "state": "closed" }).to_string());
            }
        });

        Ok(Self {
            master: pair.master,
            writer,
            killer,
            alive,
        })
    }

    /// Write keys straight to the pty: xterm.js sends the same escape sequences
    /// a terminal emulator would.
    pub fn write(&mut self, data: &str) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(data.as_bytes());
            let _ = writer.flush();
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Option<serde_json::Value> {
        let (rows, cols) = size(rows, cols);
        self.master
            .resize(PtySize {
                rows: rows.get(),
                cols: cols.get(),
                pixel_width: 0,
                pixel_height: 0,
            })
            .ok()?;
        Some(json!({ "type": "terminal_size", "cols": cols.get(), "rows": rows.get() }))
    }
}

/// Base64, because a pty is a byte stream and JSON is not: a chunk can end in
/// the middle of a UTF-8 sequence.
fn send_bytes(out: &UnboundedSender<String>, bytes: &[u8]) -> Result<(), ()> {
    let payload = BASE64.encode(bytes);
    out.send(
        json!({ "type": "terminal_output", "data": payload, "bytes": bytes.len() }).to_string(),
    )
    .map_err(|_| ())
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.killer.kill();
    }
}
