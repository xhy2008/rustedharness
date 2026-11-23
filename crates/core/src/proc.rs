use std::collections::HashMap;
use std::io::Read;
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::ShellConfig;
use crate::error::{HarnessError, Result};
use crate::protocol::HarnessEvent;
use crate::shell::ShellSpec;

const MAX_RAW_BYTES: usize = 1_048_576;

/// ConPTY answers cursor-shape/position negotiation by writing a DSR query
/// (`ESC[6n`) on the output pipe and waiting for a reply on the input pipe.
/// Nothing else in this process speaks to the pty, so respond here; without
/// this Windows shells never emit any output.
fn answer_terminal_queries(pending: &mut Vec<u8>, writer: &mut dyn std::io::Write, rows: u16) {
    if pending.windows(4).any(|window| window == b"\x1b[6n") {
        let reply = format!("\x1b[{rows};1R");
        let _ = writer.write_all(reply.as_bytes());
        let _ = writer.flush();
        pending.clear();
        return;
    }
    // Keep a small tail so a query split across reads is still matched.
    if pending.len() > 3 {
        let tail = pending.split_off(pending.len() - 3);
        *pending = tail;
    }
}
const MAX_SCREEN_LINES: usize = 10_000;
/// Maximum size of one live tool-output update. The screen tail is sent as a
/// replacement snapshot, so the frontend never has to replay raw pty bytes.
const LIVE_OUTPUT_BYTES: usize = 16 * 1024;
/// Minimum interval between live tool-output updates.
const LIVE_OUTPUT_INTERVAL: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcStatus {
    Running,
    Exited,
    Timeout,
    Killed,
    Failed,
}

impl ProcStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProcStatus::Running => "running",
            ProcStatus::Exited => "exited",
            ProcStatus::Timeout => "timeout",
            ProcStatus::Killed => "killed",
            ProcStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CaptureStats {
    pub raw_bytes: usize,
    pub screen_bytes: usize,
    pub screen_lines: usize,
    pub ansi_sequences: u64,
    pub inline_rewrites: u64,
    pub activity_events: u64,
    pub truncated: bool,
}

/// Terminal screen backed by `vt100`. It keeps the last `MAX_SCREEN_LINES`
/// rows as one tall screen, so scrollback text stays available without a
/// separate history buffer. The PTY width matches the child's `COLUMNS`, which
/// makes progress bars and wrapped output behave like a real terminal.
pub struct ScreenEmulator {
    parser: vt100::Parser,
    pub ansi_sequences: u64,
    pub inline_rewrites: u64,
    pub activity_events: u64,
    total_bytes: usize,
}

impl Default for ScreenEmulator {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenEmulator {
    pub fn new() -> Self {
        Self::with_cols(80)
    }

    pub fn with_cols(cols: u16) -> Self {
        let rows = NonZeroU16::new(MAX_SCREEN_LINES as u16).expect("MAX_SCREEN_LINES > 0");
        let cols = NonZeroU16::new(cols.max(1)).expect("cols > 0");
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            ansi_sequences: 0,
            inline_rewrites: 0,
            activity_events: 0,
            total_bytes: 0,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.total_bytes += bytes.len();
        self.ansi_sequences += bytes.iter().filter(|byte| **byte == 0x1b).count() as u64;
        self.inline_rewrites += bytes.iter().filter(|byte| **byte == b'\r').count() as u64;
        self.activity_events += bytes.len() as u64;
        self.parser.process(bytes);
    }

    pub fn snapshot(&mut self) -> String {
        self.parser
            .screen()
            .contents()
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }

    pub fn tail(&mut self, n: usize) -> String {
        let text = self.snapshot();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }

    /// Latest screen content bounded by `max_bytes`, used for live tool output.
    /// Lines are taken from the bottom so progress-heavy commands show their
    /// most recent state without resending the whole scrollback.
    pub fn tail_bytes(&self, max_bytes: usize) -> String {
        let text = self
            .parser
            .screen()
            .contents()
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        let mut chunks: Vec<&str> = Vec::new();
        let mut used = 0usize;
        let mut started = false;
        for line in text.lines().rev() {
            if !started {
                if line.is_empty() {
                    continue;
                }
                started = true;
            }
            let sep = usize::from(!chunks.is_empty());
            let extra = sep + line.len();
            if used + extra > max_bytes {
                let remaining = max_bytes.saturating_sub(used + sep);
                if remaining == 0 {
                    break;
                }
                let mut cut = line.len() - remaining;
                while cut < line.len() && !line.is_char_boundary(cut) {
                    cut += 1;
                }
                chunks.push(&line[cut..]);
                break;
            }
            chunks.push(line);
            used += extra;
        }
        chunks.reverse();
        chunks.join("\n")
    }

    pub fn stats(&self) -> CaptureStats {
        let snapshot = self.parser.screen().contents();
        CaptureStats {
            raw_bytes: self.total_bytes,
            screen_bytes: snapshot.len(),
            screen_lines: snapshot.lines().count(),
            ansi_sequences: self.ansi_sequences,
            inline_rewrites: self.inline_rewrites,
            activity_events: self.activity_events,
            ..Default::default()
        }
    }
}

struct ProcEntry {
    pid: u32,
    started: Instant,
    screen: Arc<Mutex<ScreenEmulator>>,
    raw: Arc<Mutex<Vec<u8>>>,
    status: Arc<Mutex<ProcStatus>>,
    exit_code: Arc<Mutex<Option<i32>>>,
    changed_at: Arc<Mutex<Instant>>,
    finished_at: Arc<Mutex<Option<Instant>>>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    _master: Mutex<Option<Box<dyn MasterPty + Send>>>,
}

impl ProcEntry {
    fn touch(&self) {
        if let Ok(mut t) = self.changed_at.lock() {
            *t = Instant::now();
        }
    }

    fn is_running(&self) -> bool {
        *self.status.lock().unwrap() == ProcStatus::Running
    }

    fn snapshot_text(&self) -> String {
        self.screen.lock().unwrap().snapshot()
    }

    fn live_tail(&self, limit: usize) -> String {
        self.screen.lock().unwrap().tail_bytes(limit)
    }

    fn stats(&self) -> CaptureStats {
        let mut stats = self.screen.lock().unwrap().stats();
        let raw = self.raw.lock().unwrap();
        stats.raw_bytes = raw.len();
        stats.truncated = raw.len() >= MAX_RAW_BYTES;
        stats
    }

    fn output_limited(&self, limit: usize) -> (String, bool) {
        let text = self.snapshot_text();
        if text.len() <= limit {
            (text, false)
        } else {
            let mut cut = limit;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            (
                format!(
                    "[output truncated to {limit} bytes; showing the first part]\n{}",
                    &text[..cut]
                ),
                true,
            )
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessOutcome {
    pub pid: u32,
    pub status: String,
    pub exit_code: Option<i32>,
    pub output: String,
    pub truncated: bool,
    pub elapsed_ms: u64,
    pub stats: CaptureStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundHandle {
    pub pid: u32,
    pub status: String,
    pub command: String,
    pub started_ms_ago: u64,
}

pub struct ProcRegistry {
    entries: Mutex<HashMap<u32, Arc<ProcEntry>>>,
    spec: ShellSpec,
    config: ShellConfig,
    cwd: std::path::PathBuf,
    events: Option<UnboundedSender<HarnessEvent>>,
}

impl ProcRegistry {
    pub fn new(
        spec: ShellSpec,
        config: ShellConfig,
        cwd: std::path::PathBuf,
        events: Option<UnboundedSender<HarnessEvent>>,
    ) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            spec,
            config,
            cwd,
            events,
        }
    }

    pub fn spec(&self) -> &ShellSpec {
        &self.spec
    }

    pub fn live_pids(&self) -> Vec<u32> {
        let entries = self.entries.lock().unwrap();
        let mut pids: Vec<u32> = entries
            .values()
            .filter(|e| e.is_running())
            .map(|e| e.pid)
            .collect();
        pids.sort_unstable();
        pids
    }

    pub fn spawn_background(&self, command: &str) -> Result<BackgroundHandle> {
        if let Some(message) = self.spec.bashism(command) {
            return Err(HarnessError::tool("E_BASHISM", message));
        }
        let entry = self.spawn_entry(command)?;
        Ok(BackgroundHandle {
            pid: entry.pid,
            status: ProcStatus::Running.as_str().to_string(),
            command: command.to_string(),
            started_ms_ago: entry.started.elapsed().as_millis() as u64,
        })
    }

    pub async fn run_foreground(
        &self,
        command: &str,
        timeout_ms: Option<u64>,
    ) -> Result<ProcessOutcome> {
        self.run_foreground_with(command, timeout_ms, None).await
    }

    /// Run in the foreground and deliver sanitized screen updates while the
    /// process is alive. Updates are throttled and bounded; the final
    /// `ProcessOutcome` remains authoritative.
    pub async fn run_foreground_with(
        &self,
        command: &str,
        timeout_ms: Option<u64>,
        live: Option<&(dyn Fn(&str) + Send + Sync)>,
    ) -> Result<ProcessOutcome> {
        if let Some(message) = self.spec.bashism(command) {
            return Err(HarnessError::tool("E_BASHISM", message));
        }
        let entry = self.spawn_entry(command)?;
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(self.config.timeout_ms));
        let activity = Duration::from_millis(self.config.activity_timeout_ms.max(1));
        self.wait_entry(&entry, timeout, Some(activity), true, live)
            .await
    }

    pub async fn await_process(&self, pid: u32, timeout_ms: Option<u64>) -> Result<ProcessOutcome> {
        let entry = {
            let entries = self.entries.lock().unwrap();
            entries.get(&pid).cloned()
        };
        let Some(entry) = entry else {
            let live = self.live_pids();
            return Err(HarnessError::tool_details(
                "E_UNKNOWN_PID",
                format!("no process registered for pid {pid}"),
                serde_json::json!({ "live_pids": live }),
            ));
        };
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000));
        self.wait_entry(&entry, timeout, None, false, None).await
    }

    async fn wait_entry(
        &self,
        entry: &Arc<ProcEntry>,
        timeout: Duration,
        activity_timeout: Option<Duration>,
        kill_on_timeout: bool,
        live: Option<&(dyn Fn(&str) + Send + Sync)>,
    ) -> Result<ProcessOutcome> {
        let start = Instant::now();
        let mut timed_out = false;
        let mut last_live = String::new();
        let mut last_live_at = Instant::now()
            .checked_sub(LIVE_OUTPUT_INTERVAL)
            .unwrap_or_else(Instant::now);
        loop {
            if !entry.is_running() {
                break;
            }
            if start.elapsed() >= timeout {
                timed_out = true;
                break;
            }
            if let Some(activity_timeout) = activity_timeout {
                let last = *entry.changed_at.lock().unwrap();
                if last.elapsed() >= activity_timeout {
                    timed_out = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
            if let Some(live) = live {
                if last_live_at.elapsed() >= LIVE_OUTPUT_INTERVAL {
                    let text = entry.live_tail(LIVE_OUTPUT_BYTES);
                    if text != last_live {
                        live(&text);
                        last_live = text;
                        last_live_at = Instant::now();
                    }
                }
            }
        }
        if timed_out {
            if kill_on_timeout {
                *entry.status.lock().unwrap() = ProcStatus::Timeout;
                self.terminate(entry, false);
                let grace_deadline = Instant::now() + Duration::from_secs(2);
                while entry.is_running() && Instant::now() < grace_deadline {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                if entry.is_running() {
                    self.terminate(entry, true);
                    let hard_deadline = Instant::now() + Duration::from_secs(1);
                    while entry.is_running() && Instant::now() < hard_deadline {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                entry.touch();
            }
        } else {
            // Let the reader drain the last bytes.
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let status = *entry.status.lock().unwrap();
        let exit_code = *entry.exit_code.lock().unwrap();
        let (output, truncated) = entry.output_limited(self.config.max_output_bytes);
        let elapsed_ms = entry
            .finished_at
            .lock()
            .unwrap()
            .unwrap_or_else(Instant::now)
            .duration_since(entry.started)
            .as_millis() as u64;
        let outcome = ProcessOutcome {
            pid: entry.pid,
            // A wait timeout only means this `await` gave up; the process is
            // still alive and callers rely on status "running".
            status: status.as_str().to_string(),
            exit_code,
            output,
            truncated,
            elapsed_ms,
            stats: entry.stats(),
        };
        if let Some(events) = &self.events {
            let _ = events.send(HarnessEvent::ProcessSnapshot {
                pid: outcome.pid,
                status: outcome.status.clone(),
                screen: outcome.output.clone(),
                exit_code: outcome.exit_code,
            });
        }
        Ok(outcome)
    }

    fn spawn_entry(&self, command: &str) -> Result<Arc<ProcEntry>> {
        let args = self.spec.args(command);
        let mut cmd = CommandBuilder::new(&self.spec.program);
        cmd.args(&args);
        cmd.cwd(self.spec_cwd());
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLUMNS", self.config.pty_cols.to_string());
        cmd.env("LINES", self.config.pty_rows.to_string());
        cmd.env("CLICOLOR_FORCE", "1");
        // Keep tools honest about being non-interactive even though they have a
        // tty (otherwise they may wait for input).
        cmd.env("CI", "1");

        let screen = Arc::new(Mutex::new(ScreenEmulator::with_cols(self.config.pty_cols)));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new(ProcStatus::Running));
        let exit_code = Arc::new(Mutex::new(None));
        let changed_at = Arc::new(Mutex::new(Instant::now()));
        let finished_at = Arc::new(Mutex::new(None));

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: self.config.pty_rows,
                cols: self.config.pty_cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| {
                HarnessError::tool(
                    "E_PTY",
                    format!(
                        "cannot allocate a pty for {}: {err}",
                        self.spec.program.display()
                    ),
                )
            })?;

        let mut child = match pair.slave.spawn_command(cmd) {
            Ok(child) => child,
            Err(err) => {
                return Err(HarnessError::tool(
                    "E_SPAWN",
                    format!("failed to spawn {}: {err}", self.spec.program.display()),
                ));
            }
        };
        drop(pair.slave);
        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader().ok();
        // ConPTY (PSUEDOCONSOLE_INHERIT_CURSOR) starts by emitting a DSR
        // cursor-position query on the output pipe and blocks all child
        // output until something answers on the input pipe. Without a
        // writer the shell never produces output at all.
        let writer = pair.master.take_writer().ok();
        let master: Box<dyn MasterPty + Send> = pair.master;

        if let Some(mut reader) = reader {
            let screen = screen.clone();
            let raw = raw.clone();
            let changed_at = changed_at.clone();
            let pty_rows = self.config.pty_rows;
            std::thread::spawn(move || {
                let mut writer = writer;
                let mut buf = [0u8; 8192];
                let mut pending: Vec<u8> = Vec::new();
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut raw) = raw.lock() {
                                if raw.len() < MAX_RAW_BYTES {
                                    let remaining = MAX_RAW_BYTES - raw.len();
                                    raw.extend_from_slice(&buf[..n.min(remaining)]);
                                }
                            }
                            if let Ok(mut screen) = screen.lock() {
                                screen.feed(&buf[..n]);
                            }
                            if let Ok(mut t) = changed_at.lock() {
                                *t = Instant::now();
                            }
                            if let Some(writer) = writer.as_mut() {
                                pending.extend_from_slice(&buf[..n]);
                                answer_terminal_queries(&mut pending, writer, pty_rows);
                                if pending.len() > 256 {
                                    pending.clear();
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        let entry = Arc::new(ProcEntry {
            pid,
            started: Instant::now(),
            screen,
            raw,
            status: status.clone(),
            exit_code: exit_code.clone(),
            changed_at,
            finished_at: finished_at.clone(),
            killer: Mutex::new(Some(killer)),
            _master: Mutex::new(Some(master)),
        });
        self.entries.lock().unwrap().insert(pid, entry.clone());

        let status_wait = status.clone();
        let exit_wait = exit_code.clone();
        let finished_wait = finished_at.clone();
        let events = self.events.clone();
        let pid_wait = pid;
        std::thread::spawn(move || {
            let wait = child.wait();
            let code = wait.as_ref().ok().map(|s| s.exit_code() as i32);
            let mut status = status_wait.lock().unwrap();
            if *status == ProcStatus::Running {
                *status = match &wait {
                    Ok(_) => ProcStatus::Exited,
                    Err(_) => ProcStatus::Failed,
                };
            }
            drop(status);
            *exit_wait.lock().unwrap() = code;
            *finished_wait.lock().unwrap() = Some(Instant::now());
            if let Some(events) = events {
                let _ = events.send(HarnessEvent::ProcessExited {
                    pid: pid_wait,
                    status: "exited".into(),
                    exit_code: code,
                });
            }
        });
        Ok(entry)
    }

    fn spec_cwd(&self) -> std::path::PathBuf {
        self.cwd.clone()
    }

    fn terminate(&self, entry: &Arc<ProcEntry>, force: bool) {
        if let Some(killer) = entry.killer.lock().unwrap().as_mut() {
            let _ = killer.kill();
        }
        #[cfg(unix)]
        {
            use nix::sys::signal::{killpg, Signal};
            use nix::unistd::Pid;
            let sig = if force {
                Signal::SIGKILL
            } else {
                Signal::SIGTERM
            };
            let pid = Pid::from_raw(entry.pid as i32);
            let _ = killpg(pid, sig);
            let _ = nix::sys::signal::kill(pid, sig);
        }
        #[cfg(windows)]
        {
            let mut cmd = std::process::Command::new("taskkill");
            cmd.args(["/PID", &entry.pid.to_string(), "/T"]);
            if force {
                cmd.arg("/F");
            }
            let _ = cmd.output();
        }
        if !force {
            let entry = entry.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                if entry.is_running() {
                    #[cfg(unix)]
                    {
                        use nix::sys::signal::{killpg, Signal};
                        use nix::unistd::Pid;
                        let _ = killpg(Pid::from_raw(entry.pid as i32), Signal::SIGKILL);
                    }
                }
            });
        }
    }

    pub fn kill(&self, pid: u32) -> Result<()> {
        let entry = {
            let entries = self.entries.lock().unwrap();
            entries.get(&pid).cloned()
        };
        let Some(entry) = entry else {
            return Err(HarnessError::tool(
                "E_UNKNOWN_PID",
                format!("no process registered for pid {pid}"),
            ));
        };
        *entry.status.lock().unwrap() = ProcStatus::Killed;
        self.terminate(&entry, false);
        Ok(())
    }

    /// Session teardown: cascade-kill every live process group.
    pub fn cleanup_all(&self) {
        let entries: Vec<Arc<ProcEntry>> = {
            let entries = self.entries.lock().unwrap();
            entries.values().cloned().collect()
        };
        for entry in entries {
            if entry.is_running() {
                *entry.status.lock().unwrap() = ProcStatus::Killed;
                self.terminate(&entry, true);
            }
        }
        self.entries.lock().unwrap().clear();
    }

    pub fn raw_bytes(&self, pid: u32) -> Option<Vec<u8>> {
        let entries = self.entries.lock().unwrap();
        entries.get(&pid).map(|e| e.raw.lock().unwrap().clone())
    }
}

impl Drop for ProcRegistry {
    fn drop(&mut self) {
        self.cleanup_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_of(input: &str) -> String {
        let mut emu = ScreenEmulator::new();
        emu.feed(input.as_bytes());
        emu.snapshot()
    }

    /// Regression: ConPTY's PSUEDOCONSOLE_INHERIT_CURSOR emits a DSR query
    /// (`ESC[6n`) and stalls all child output until it is answered. Without
    /// the writer reply this test hangs until the timeout with an empty
    /// screen (see `stats.raw_bytes == 4` in the original bug report).
    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_foreground_captures_output_on_conpty() {
        use crate::config::ShellConfig;
        use crate::shell::ShellSpec;
        let spec = ShellSpec::detect("default");
        let config = ShellConfig {
            timeout_ms: 20_000,
            ..Default::default()
        };
        let registry = ProcRegistry::new(spec, config, std::env::current_dir().unwrap(), None);
        let outcome = registry
            .run_foreground("echo rusted-conpty-probe", Some(20_000))
            .await
            .expect("foreground run");
        assert_eq!(outcome.status, "exited", "output: {:?}", outcome.output);
        assert_eq!(outcome.exit_code, Some(0));
        assert!(
            outcome.output.contains("rusted-conpty-probe"),
            "conpty swallowed the command output (raw_bytes={})",
            outcome.stats.raw_bytes
        );
        // An elevated Chinese PowerShell sets its window title to
        // "管理员: C:\...powershell.exe" via OSC; that multibyte text must be
        // swallowed by the emulator, never leaked into captured output.
        assert!(
            !outcome.output.contains('管') && !outcome.output.contains("管理员"),
            "window-title OSC leaked into output: {:?}",
            outcome.output
        );
    }

    #[test]
    fn osc_multibyte_title_is_swallowed_not_printed() {
        // Reproduces the elevated-PowerShell leak: OSC sets a title containing
        // CJK, then real output follows on the same cursor position.
        let mut emu = ScreenEmulator::new();
        let seq = "\u{1b}]0;\u{7ba1}\u{7406}\u{5458}: C:\\Windows\\powershell.exe\u{7}rusted-ok";
        emu.feed(seq.as_bytes());
        let text = emu.snapshot();
        assert_eq!(text.trim(), "rusted-ok", "got: {text:?}");
    }

    /// The wrapper must propagate a native command's real exit code, not
    /// flatten it to powershell's own 0/1.
    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_propagates_native_exit_code() {
        use crate::config::ShellConfig;
        use crate::shell::ShellSpec;
        let spec = ShellSpec::detect("default");
        let config = ShellConfig {
            timeout_ms: 20_000,
            ..Default::default()
        };
        let registry = ProcRegistry::new(spec, config, std::env::current_dir().unwrap(), None);
        let outcome = registry
            .run_foreground("cmd /c exit 42", Some(20_000))
            .await
            .expect("foreground run");
        assert_eq!(
            outcome.exit_code,
            Some(42),
            "native exit code not propagated: {:?}",
            outcome.output
        );
    }

    #[test]
    fn carriage_return_collapses_progress_to_one_line() {
        let mut emu = ScreenEmulator::new();
        emu.feed(b"  0%|          | 0/5000");
        for i in (500..=5000).step_by(500) {
            emu.feed(format!("\r {:3}%|#####| {i}/5000", i / 50).as_bytes());
        }
        emu.feed(b"\r100%|##########| 5000/5000\n");
        let text = emu.snapshot();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("100%"));
        assert!(!text.contains("  0%|"));
        assert!(emu.inline_rewrites >= 10);
    }

    #[test]
    fn ansi_colors_are_removed() {
        let text = screen_of("\x1b[31mred\x1b[0m plain \x1b[1;32mgreen\x1b[0m");
        assert_eq!(text, "red plain green");
    }

    #[test]
    fn erase_line_and_cursor_up_animation_converges() {
        let text = screen_of("step 1\r\nstep 2\r\n\x1b[1A\x1b[2K\rstep 2 done\r\n");
        assert_eq!(text, "step 1\nstep 2 done");
    }

    #[test]
    fn osc_title_is_ignored() {
        let text = screen_of("\x1b]0;cargo build\x07building\n");
        assert_eq!(text, "building");
    }

    #[test]
    fn utf8_split_across_chunks_is_reassembled() {
        let bytes = "进度 50%\n".as_bytes();
        let mut emu = ScreenEmulator::new();
        for chunk in bytes.chunks(2) {
            emu.feed(chunk);
        }
        assert_eq!(emu.snapshot(), "进度 50%");
    }

    #[test]
    fn clear_screen_resets_output() {
        assert_eq!(screen_of("old\nnoise\n\x1b[2J\x1b[Hnew\n"), "new");
    }

    #[test]
    fn no_ansi_residue_survives() {
        let text = screen_of("\x1b[38;5;196mhello\x1b[0m\x1b[?25l\x1b[?25h");
        assert!(!text.contains('\x1b'));
        assert_eq!(text, "hello");
    }

    #[test]
    fn live_tail_is_bounded_and_utf8_safe() {
        let mut emu = ScreenEmulator::new();
        for i in 0..100 {
            emu.feed(format!("line {i}\n").as_bytes());
        }
        let text = emu.tail_bytes(64);
        assert!(
            text.len() <= 64,
            "tail must respect byte cap: {}",
            text.len()
        );
        assert!(text.ends_with("line 99"));

        let mut wide = ScreenEmulator::new();
        wide.feed("汉字进度".repeat(20).as_bytes());
        let tail = wide.tail_bytes(10);
        assert!(tail.len() <= 10);
        assert!(tail
            .chars()
            .all(|c| c == '汉' || c == '字' || c == '进' || c == '度'));
    }

    #[tokio::test]
    async fn foreground_streams_live_screen_updates() {
        if cfg!(windows) {
            return;
        }
        let spec = ShellSpec::detect("default");
        let reg = ProcRegistry::new(
            spec,
            ShellConfig::default(),
            std::env::current_dir().unwrap(),
            None,
        );
        let updates = Arc::new(Mutex::new(Vec::new()));
        let sink = updates.clone();
        let live: &(dyn Fn(&str) + Send + Sync) = &move |text: &str| {
            sink.lock().unwrap().push(text.to_string());
        };
        let out = reg
            .run_foreground_with(
                "printf 'live-start\\n'; sleep 0.35; printf 'live-end\\n'",
                None,
                Some(live),
            )
            .await
            .unwrap();
        assert!(out.output.contains("live-start"));
        assert!(out.output.contains("live-end"));
        let updates = updates.lock().unwrap();
        assert!(!updates.is_empty(), "expected at least one live update");
        assert!(
            updates.iter().any(|text| text.contains("live-start")),
            "live updates did not contain early output: {updates:?}"
        );
    }

    #[tokio::test]
    async fn foreground_capture_returns_final_line_and_exit_code() {
        if cfg!(windows) {
            return;
        }
        let spec = ShellSpec::detect("default");
        let cfg = ShellConfig {
            timeout_ms: 5_000,
            activity_timeout_ms: 5_000,
            ..Default::default()
        };
        let reg = ProcRegistry::new(spec, cfg, std::env::current_dir().unwrap(), None);
        let out = reg
            .run_foreground("printf '10%%\\r20%%\\r100%%\\n'; echo done", None)
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.output.contains("100%"));
        assert!(out.output.contains("done"));
        assert!(!out.output.contains("20%"));
    }

    #[tokio::test]
    async fn background_await_reports_exit_code_and_survives_timeout() {
        if cfg!(windows) {
            return;
        }
        let spec = ShellSpec::detect("default");
        let reg = ProcRegistry::new(
            spec,
            ShellConfig::default(),
            std::env::current_dir().unwrap(),
            None,
        );
        let handle = reg.spawn_background("sleep 0.2; echo finished").unwrap();
        let mid = reg.await_process(handle.pid, Some(20)).await.unwrap();
        assert_eq!(mid.status, "running");
        let done = reg.await_process(handle.pid, Some(3_000)).await.unwrap();
        assert_eq!(done.status, "exited");
        assert_eq!(done.exit_code, Some(0));
        assert!(done.output.contains("finished"));
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        if cfg!(windows) {
            return;
        }
        let spec = ShellSpec::detect("default");
        let reg = ProcRegistry::new(
            spec,
            ShellConfig::default(),
            std::env::current_dir().unwrap(),
            None,
        );
        let out = reg.run_foreground("sleep 30", Some(120)).await.unwrap();
        assert_eq!(out.status, "timeout");
        assert!(reg.live_pids().is_empty(), "process should be gone");
    }
}
