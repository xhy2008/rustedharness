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
//! Two threads belong to a terminal: one reads the pty and forwards each read as
//! one frame (`read` returns what the child wrote, so a prompt is a frame of its
//! own and never waits for a deadline), and one writes keys into it. Writes go
//! through a queue rather than the reactor, because `write` on a pty blocks once
//! the child stops draining it and a paste must not stall the whole session.

use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::json;
use tokio::sync::mpsc::Sender;

use rustedharness_core::shell::{saw_terminal_query, ShellSpec};

const READ_BYTES: usize = 16 * 1024;
/// How many key batches may wait for the pty before typing applies
/// back-pressure. Small on purpose: this queue only exists to keep the reactor
/// out of a blocking write.
const KEY_QUEUE: usize = 32;

pub struct Terminal {
    /// Distinguishes this shell from the one it replaced in the same connection.
    id: u64,
    /// `MasterPty` is `Send` but not `Sync`, and `write` holds `&self` across
    /// an await, so the handle lives behind a lock.
    master: Mutex<Box<dyn MasterPty + Send>>,
    keys: Sender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    rows: Arc<AtomicU16>,
    alive: Arc<AtomicBool>,
}

/// Distinguishes two terminals of one connection: a restarted shell must not
/// mix its first frames into the screen of the shell it replaced.
fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Clamp both axes into a range a pty accepts.
fn size(rows: u16, cols: u16) -> (NonZeroU16, NonZeroU16) {
    (
        NonZeroU16::new(rows.clamp(4, 200)).expect("clamped above zero"),
        NonZeroU16::new(cols.clamp(20, 500)).expect("clamped above zero"),
    )
}

impl Terminal {
    pub async fn open(
        cwd: &Path,
        spec: &ShellSpec,
        cols: u16,
        rows: u16,
        out: Sender<String>,
    ) -> Result<Self, String> {
        let (rows, cols) = size(rows, cols);
        let (rows, cols) = (rows.get(), cols.get());
        let id = next_id();
        let pair = native_pty_system()
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
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| format!("cannot start {}: {err}", spec.program.display()))?;
        drop(pair.slave);

        let killer = child.clone_killer();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| format!("cannot read the pty: {err}"))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|err| format!("cannot write the pty: {err}"))?;

        let (keys, mut queued) = tokio::sync::mpsc::channel::<Vec<u8>>(KEY_QUEUE);
        std::thread::spawn(move || {
            while let Some(bytes) = queued.blocking_recv() {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });

        // Announce the shell before its first frame can arrive, so the page
        // knows which terminal the bytes belong to.
        let _ = out
            .send(
                json!({
                    "type": "terminal_state",
                    "state": "running",
                    "id": id,
                    "shell": spec.display_name,
                    "cwd": cwd.to_string_lossy(),
                    "cols": cols,
                    "rows": rows,
                })
                .to_string(),
            )
            .await;

        let alive = Arc::new(AtomicBool::new(true));
        let rows = Arc::new(AtomicU16::new(rows));
        let reply_rows = rows.clone();
        let reply_keys = keys.clone();
        let reader_out = out.clone();
        let reader_alive = alive.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; READ_BYTES];
            let mut carry = [0u8; 3];
            loop {
                let read = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(_) => break,
                };
                let chunk = &buf[..read];
                if saw_terminal_query(&mut carry, chunk) {
                    // ConPTY blocks the child until this is answered. It goes
                    // through the same queue as typed keys so replies and keys
                    // cannot interleave inside one write.
                    let reply = format!("\x1b[{};1R", reply_rows.load(Ordering::Relaxed));
                    let _ = reply_keys.blocking_send(reply.into_bytes());
                }
                if send_bytes(&reader_out, id, chunk).is_err() {
                    break;
                }
            }
            if reader_alive.swap(false, Ordering::SeqCst) {
                let _ = reader_out.blocking_send(
                    json!({ "type": "terminal_state", "state": "closed", "id": id }).to_string(),
                );
            }
        });

        Ok(Self {
            id,
            master: Mutex::new(pair.master),
            keys,
            killer,
            rows,
            alive,
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Write keys straight to the pty: xterm.js sends the same escape sequences
    /// a terminal emulator would.
    pub async fn write(&self, data: &str) {
        let _ = self.keys.send(data.as_bytes().to_vec()).await;
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Option<serde_json::Value> {
        let (rows, cols) = size(rows, cols);
        let master = self.master.lock().ok()?;
        master
            .resize(PtySize {
                rows: rows.get(),
                cols: cols.get(),
                pixel_width: 0,
                pixel_height: 0,
            })
            .ok()?;
        self.rows.store(rows.get(), Ordering::Relaxed);
        Some(json!({ "type": "terminal_size", "cols": cols.get(), "rows": rows.get() }))
    }
}

/// Base64, because a pty is a byte stream and JSON is not: a chunk can end in
/// the middle of a UTF-8 sequence.
fn send_bytes(out: &Sender<String>, id: u64, bytes: &[u8]) -> Result<(), ()> {
    let payload = BASE64.encode(bytes);
    out.blocking_send(
        json!({ "type": "terminal_output", "id": id, "data": payload, "bytes": bytes.len() })
            .to_string(),
    )
    .map_err(|_| ())
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.killer.kill();
    }
}
