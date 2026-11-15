//! Pi-style terminal frontend for RustedHarness.
//!
//! Layout (top to bottom, borderless like Pi):
//!   transcript | [overlay] | editor rule | status line | footer
//!
//! Feature parity with the web GUI: sessions (list/resume/new), file tree and
//! viewer, model picker, settings, todos, LSP status, processes, download
//! consent, token/context stats, clipboard copy.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::Rect;
use ratatui::text::Line;
use rustedharness_core::agent::Agent;
use rustedharness_core::config::Config;
use rustedharness_core::lsp::{ChecksumInfo, ServerStatus};
use rustedharness_core::protocol::{HarnessEvent, SessionSnapshot};
use rustedharness_core::provider::{ChatMessage, Role};
use rustedharness_core::tools::todo::TodoItem;
use tokio::sync::mpsc::UnboundedSender;

use crate::entries::{Entry, EntryKind, ProcessCard};

/// Long-running action routed through the event loop so rendering never blocks.
pub enum OpResult {
    Checksum {
        server: String,
        result: std::result::Result<ChecksumInfo, String>,
    },
    Installed {
        server: String,
        result: std::result::Result<PathBuf, String>,
    },
}

/// Request to replace the current agent; the loop owns the event channel, so
/// it performs the swap when this is set.
pub struct PendingSession {
    pub resume: Option<String>,
    pub cwd: PathBuf,
}

#[derive(Clone)]
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub value: String,
}

pub enum Overlay {
    None,
    Slash {
        items: Vec<PickerItem>,
        sel: usize,
    },
    Model {
        items: Vec<PickerItem>,
        sel: usize,
        filter: String,
        target: ModelTarget,
    },
    Session {
        items: Vec<PickerItem>,
        sel: usize,
        filter: String,
    },
    FileBrowser {
        rel: String,
        entries: Vec<(String, bool)>,
        sel: usize,
    },
    FileView {
        path: String,
        lines: Vec<String>,
        scroll: usize,
        truncated: bool,
    },
    Settings {
        sel: usize,
    },
    Hotkeys,
    Todos,
    Processes,
    Lsp,
}

/// Which model slot the picker edits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModelTarget {
    Agent,
    Compact,
}

/// Selectable rows in the settings overlay.
const SETTINGS_ROWS: &[&str] = &["agent model", "compact model", "workspace", "session"];

pub struct ConsentCard {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub url: String,
    /// `None` when the release publishes no digest: the download is offered
    /// anyway and installed unverified.
    pub sha256: Option<String>,
    pub runtime_hint: Option<String>,
}

pub struct App {
    pub agent: std::sync::Arc<Agent>,
    pub config: Config,
    pub entries: Vec<Entry>,
    pub todo: Vec<TodoItem>,
    pub lsp: Vec<ServerStatus>,
    pub processes: BTreeMap<u32, ProcessCard>,
    pub consent: Option<ConsentCard>,

    pub input: Vec<char>,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_pos: usize,

    pub busy: bool,
    pub queue: VecDeque<String>,
    pub session_id: String,
    pub cwd: String,
    pub agent_model: String,
    pub compact_model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub context_tokens: u64,
    pub context_limit: u64,
    pub cost_usd: Option<f64>,
    pub cache_hit_pct: Option<u64>,
    pub last_stop: Option<String>,

    pub scroll: usize,
    pub follow: bool,
    pub overlay: Overlay,
    pub mouse_enabled: bool,
    pub selection: Option<((u16, u16), (u16, u16))>,
    pub mouse_area: Rect,
    pub lines_cache: RefCell<Vec<Line<'static>>>,
    /// Entry index owning each cached transcript line, for click-to-fold.
    pub owner_cache: RefCell<Vec<usize>>,
    pub lines_sig: (usize, u64),
    pub slash_dismissed: bool,
    pub streaming: Option<usize>,
    pub reasoning_at: Option<usize>,
    /// Entry index of every tool call, keyed by call id. Tool results arrive
    /// out of order (a turn can issue several calls in parallel) and may even
    /// be replayed from a session, so the id is the only reliable link back to
    /// the row that asked for the work.
    pub tool_slots: HashMap<String, usize>,
    pub tick: u64,
    /// Transient status message (copy confirmations), shown in the footer for
    /// a couple of seconds instead of pushing a transcript entry.
    pub toast: Option<(String, u64, bool)>,
    /// Set when the whole screen must be repainted from scratch.
    pub needs_repaint: bool,
    pub quit: bool,
    pub pending: Option<PendingSession>,
}

/// Animated dots, all three frames the same width so the status line never
/// jitters. Braille and dingbat spinners are not in every terminal font.
const SPINNER: [&str; 3] = [".  ", ".. ", "..."];

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show this command list"),
    ("/hotkeys", "keyboard shortcut reference"),
    ("/model", "open the model picker"),
    ("/model <name>", "switch the agent model directly"),
    ("/compact-model", "switch the compaction model"),
    ("/settings", "model, compact model, workspace, session"),
    ("/cwd <dir>", "move the workspace root"),
    ("/new", "start a fresh session"),
    ("/resume", "pick a saved session"),
    ("/session", "show session id, models and context usage"),
    ("/clear", "clear the transcript view"),
    ("/files", "browse the workspace file tree"),
    ("/view <path>", "open a file read-only"),
    ("/todo", "show the todo list"),
    ("/lsp", "language server status"),
    (
        "/checksum <id>",
        "inspect the verified language server digest",
    ),
    ("/install <id>", "install a language server"),
    ("/procs", "background processes"),
    ("/copy", "copy the last assistant reply"),
    ("/quit", "exit"),
];

impl App {
    pub fn new(agent: std::sync::Arc<Agent>, config: Config) -> Self {
        let mut app = Self {
            agent,
            config,
            entries: Vec::new(),
            todo: Vec::new(),
            lsp: Vec::new(),
            processes: BTreeMap::new(),
            consent: None,
            input: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: 0,
            busy: false,
            queue: VecDeque::new(),
            session_id: String::new(),
            cwd: String::new(),
            agent_model: String::new(),
            compact_model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            context_tokens: 0,
            context_limit: 0,
            cost_usd: None,
            cache_hit_pct: None,
            last_stop: None,
            scroll: 0,
            follow: true,
            overlay: Overlay::None,
            mouse_enabled: true,
            selection: None,
            mouse_area: Rect::ZERO,
            lines_cache: RefCell::new(Vec::new()),
            owner_cache: RefCell::new(Vec::new()),
            lines_sig: (0, 0),
            slash_dismissed: false,
            streaming: None,
            reasoning_at: None,
            tool_slots: HashMap::new(),
            tick: 0,
            toast: None,
            needs_repaint: false,
            quit: false,
            pending: None,
        };
        app.populate(app.agent.snapshot());
        app
    }

    /// Replace the running agent (new or resumed session) and clear every
    /// piece of state that belongs to the previous one.
    pub fn switch_agent(&mut self, agent: std::sync::Arc<Agent>) {
        let snapshot = agent.snapshot();
        self.agent = agent;
        self.entries.clear();
        self.todo.clear();
        self.lsp.clear();
        self.processes.clear();
        self.consent = None;
        self.queue.clear();
        self.streaming = None;
        self.reasoning_at = None;
        self.tool_slots.clear();
        self.scroll = 0;
        self.follow = true;
        self.busy = false;
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.cost_usd = None;
        self.cache_hit_pct = None;
        self.last_stop = None;
        self.overlay = Overlay::None;
        self.selection = None;
        self.slash_dismissed = false;
        self.populate(snapshot);
        self.selection = None;
        self.needs_repaint = true;
    }

    pub fn populate(&mut self, snapshot: SessionSnapshot) {
        self.tool_slots.clear();
        self.session_id = snapshot.session_id;
        self.cwd = snapshot.cwd;
        self.agent_model = snapshot.agent_model;
        self.compact_model = snapshot.compact_model;
        self.context_tokens = snapshot.context_tokens;
        self.context_limit = snapshot.context_limit;
        self.todo = snapshot.todos;
        for message in snapshot.messages {
            self.push_message(message);
        }
    }

    pub fn push_message(&mut self, message: ChatMessage) {
        match message.role {
            Role::System => {}
            Role::User => {
                if let Some(text) = message.content {
                    self.push(EntryKind::User, "", text);
                }
            }
            Role::Assistant => {
                if let Some(reasoning) = message.reasoning_content {
                    if !reasoning.trim().is_empty() {
                        self.push(EntryKind::Reasoning, "thinking", reasoning);
                    }
                }
                if let Some(text) = message.content {
                    if !text.trim().is_empty() {
                        self.push(EntryKind::Assistant, "", text);
                    }
                }
                if let Some(calls) = message.tool_calls {
                    for call in calls {
                        let args = call.function.arguments.clone();
                        let entry = Entry::new(EntryKind::Tool, call.function.name.as_str(), "")
                            .with_arg(tool_summary(&call.function.name, &args));
                        self.push_entry(entry);
                        self.tool_slots
                            .insert(call.id.clone(), self.entries.len() - 1);
                    }
                }
            }
            Role::Tool => {
                let ChatMessage {
                    content,
                    tool_call_id,
                    name,
                    ..
                } = message;
                if let Some(text) = content {
                    let body = text.trim().to_string();
                    let id = tool_call_id.unwrap_or_default();
                    // The session stores the result as its own message; the
                    // transcript shows it under the call that produced it, so a
                    // restored conversation looks exactly like a live one.
                    if !self.attach_tool_result(&id, true, body.clone()) {
                        let title = name.unwrap_or_else(|| "tool".to_string());
                        let mut entry = Entry::new(EntryKind::Tool, title, body);
                        entry.ok = Some(true);
                        self.push_entry(entry);
                    }
                }
            }
        }
    }

    /// Bind a tool's output to the row of the call that produced it.
    ///
    /// Returns false when no call with that id is in the transcript, in which
    /// case the caller has to decide where the output goes. The slot is kept
    /// after a match so a duplicated result overwrites instead of appending.
    fn attach_tool_result(&mut self, id: &str, ok: bool, body: String) -> bool {
        let Some(index) = self.tool_slots.get(id).copied() else {
            return false;
        };
        if let Some(entry) = self.entries.get_mut(index) {
            entry.ok = Some(ok);
            entry.body = body;
            entry.expanded = false;
            return true;
        }
        false
    }

    /// A reasoning block opens while it streams and folds itself away as soon
    /// as the model moves on, so finished turns keep only one dim line.
    fn end_reasoning(&mut self) {
        if let Some(index) = self.reasoning_at.take() {
            if let Some(entry) = self.entries.get_mut(index) {
                entry.expanded = false;
            }
        }
    }

    /// Ctrl+O / Ctrl+T: if anything of `kind` is open, close them all;
    /// otherwise open them all.
    fn toggle_all(&mut self, kind: EntryKind) {
        let open = !self
            .entries
            .iter()
            .any(|entry| entry.kind == kind && entry.expanded);
        for entry in self.entries.iter_mut().filter(|e| e.kind == kind) {
            entry.expanded = open;
        }
    }

    /// Click on a transcript row: fold or unfold the entry that owns it.
    pub(crate) fn toggle_row(&mut self, row: u16) {
        let index = {
            let owners = self.owner_cache.borrow();
            owners.get(self.scroll + row as usize).copied()
        };
        let Some(index) = index else { return };
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if entry.foldable() {
            entry.expanded = !entry.expanded;
        }
    }

    pub fn push(&mut self, kind: EntryKind, title: impl Into<String>, body: impl Into<String>) {
        self.push_entry(Entry::new(kind, title, body));
    }

    fn push_entry(&mut self, entry: Entry) {
        self.entries.push(entry);
        if self.follow {
            self.scroll = usize::MAX;
        }
    }

    /// Show a transient footer message (copy confirmations, clipboard errors).
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), self.tick, false));
    }

    pub fn toast_error(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), self.tick, true));
    }

    /// The toast still being inside its display window, if any.
    pub fn active_toast(&self) -> Option<(&str, bool)> {
        const TICKS: u64 = 16;
        match &self.toast {
            Some((text, started, error)) if self.tick.saturating_sub(*started) < TICKS => {
                Some((text.as_str(), *error))
            }
            _ => None,
        }
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        self.push(EntryKind::Notice, "", text);
    }

    pub fn on_op_result(&mut self, result: OpResult) {
        match result {
            OpResult::Checksum { server, result } => match result {
                Ok(info) => self.notice(format!(
                    "{server} {} | {} | sha256 {} ({})",
                    info.version,
                    info.url,
                    info.sha256.as_deref().unwrap_or("none published"),
                    info.source
                )),
                Err(err) => self.push(EntryKind::Error, "", format!("{server} checksum: {err}")),
            },
            OpResult::Installed { server, result } => match result {
                Ok(path) => self.notice(format!("{server} ready at {}", path.display())),
                Err(err) => self.push(EntryKind::Error, "", format!("{server} install: {err}")),
            },
        }
    }

    pub fn on_harness_event(&mut self, event: HarnessEvent) {
        match event {
            HarnessEvent::SessionStarted {
                session_id,
                cwd,
                agent_model,
                compact_model,
            } => {
                self.session_id = session_id;
                self.cwd = cwd;
                self.agent_model = agent_model;
                self.compact_model = compact_model;
            }
            HarnessEvent::UserMessage { text } => {
                let duplicate = self
                    .entries
                    .last()
                    .map(|e| e.kind == EntryKind::User && e.body == text)
                    .unwrap_or(false);
                if !duplicate {
                    self.push(EntryKind::User, "", text);
                }
            }
            HarnessEvent::AssistantDelta { text } => {
                if let Some(index) = self.streaming {
                    if let Some(entry) = self.entries.get_mut(index) {
                        entry.body.push_str(&text);
                        return;
                    }
                }
                self.push(EntryKind::Assistant, "", text);
                self.streaming = Some(self.entries.len() - 1);
                self.end_reasoning();
            }
            HarnessEvent::AssistantReasoning { text } => {
                if let Some(index) = self.reasoning_at {
                    if let Some(entry) = self.entries.get_mut(index) {
                        entry.body.push_str(&text);
                        return;
                    }
                }
                let mut entry = Entry::new(EntryKind::Reasoning, "thinking", text);
                entry.expanded = true;
                self.push_entry(entry);
                self.reasoning_at = Some(self.entries.len() - 1);
            }
            HarnessEvent::AssistantMessage { content, reasoning } => {
                let reasoning_missing = self.reasoning_at.is_none()
                    && !reasoning.as_deref().unwrap_or("").trim().is_empty();
                if reasoning_missing {
                    self.push(EntryKind::Reasoning, "thinking", reasoning.unwrap());
                }
                if self.streaming.is_none() && !content.trim().is_empty() {
                    self.push(EntryKind::Assistant, "", content);
                }
                self.streaming = None;
                self.end_reasoning();
            }
            HarnessEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                let args = arguments.to_string();
                let mut entry = Entry::new(EntryKind::Tool, name.as_str(), "")
                    .with_arg(tool_summary(&name, &args));
                entry.expanded = true;
                self.push_entry(entry);
                self.tool_slots.insert(id, self.entries.len() - 1);
                self.streaming = None;
                self.end_reasoning();
            }
            HarnessEvent::ToolOutput { id, text, .. } => {
                if let Some(index) = self.tool_slots.get(&id).copied() {
                    if let Some(entry) = self.entries.get_mut(index) {
                        entry.body = tail_lines(&text, 20);
                    }
                }
            }
            HarnessEvent::ToolResult {
                id,
                name,
                ok,
                content,
                ..
            } => {
                let mut body = content.trim().to_string();
                if body.chars().count() > 4_000 {
                    body = format!("{}...", body.chars().take(4_000).collect::<String>());
                }
                if !self.attach_tool_result(&id, ok, body.clone()) {
                    // A result whose call is not in this transcript (an older
                    // session that was compacted, a dropped event). Keep the
                    // output visible under the tool's own name instead of
                    // inventing a "tool result" row.
                    let mut entry = Entry::new(EntryKind::Tool, name.as_str(), body);
                    entry.ok = Some(ok);
                    self.push_entry(entry);
                }
            }
            HarnessEvent::TodoUpdated { items } => self.todo = items,
            HarnessEvent::LspStatus {
                language,
                state,
                detail,
            } => {
                if let Some(status) = self.lsp.iter_mut().find(|s| s.language == language) {
                    status.state = state.clone();
                    status.detail = detail.clone();
                } else if let Some(spec) = rustedharness_core::lsp::spec_for_language(&language) {
                    self.lsp.push(ServerStatus {
                        language,
                        server: spec.id.to_string(),
                        version: None,
                        state,
                        detail,
                        pid: None,
                        restarts: 0,
                        install_hint: spec.install_hint.to_string(),
                        runtime_deps: spec.runtime_deps.iter().map(|s| s.to_string()).collect(),
                    });
                }
                self.lsp.sort_by(|a, b| a.language.cmp(&b.language));
            }
            HarnessEvent::ProcessSnapshot {
                pid,
                status,
                screen,
                exit_code,
            } => {
                self.processes.insert(
                    pid,
                    ProcessCard {
                        status,
                        exit_code,
                        screen,
                    },
                );
            }
            HarnessEvent::ProcessExited {
                pid,
                status,
                exit_code,
            } => {
                let screen = self
                    .processes
                    .get(&pid)
                    .map(|p| p.screen.clone())
                    .unwrap_or_default();
                self.processes.insert(
                    pid,
                    ProcessCard {
                        status,
                        exit_code,
                        screen,
                    },
                );
            }
            HarnessEvent::DownloadConsent {
                id,
                display_name,
                version,
                url,
                sha256,
                runtime_hint,
                ..
            } => {
                self.consent = Some(ConsentCard {
                    id,
                    display_name,
                    version,
                    url,
                    sha256,
                    runtime_hint,
                });
            }
            HarnessEvent::DownloadStatus {
                server_id,
                state,
                detail,
            } => {
                self.notice(format!(
                    "lsp {server_id}: {state}{}",
                    detail.map(|d| format!(" — {d}")).unwrap_or_default()
                ));
            }
            HarnessEvent::TokenUsage {
                input_tokens,
                output_tokens,
                context_tokens,
                context_limit,
                cost_usd,
                cache_hit_pct,
            } => {
                self.input_tokens = input_tokens;
                self.output_tokens = output_tokens;
                self.context_tokens = context_tokens;
                self.context_limit = context_limit;
                self.cost_usd = cost_usd;
                self.cache_hit_pct = cache_hit_pct;
            }
            HarnessEvent::Compaction {
                summary,
                dropped_messages,
            } => {
                self.push(
                    EntryKind::Compaction,
                    format!("compacted {dropped_messages} messages"),
                    summary,
                );
            }
            HarnessEvent::Chase { attempt, pending } => {
                self.notice(format!(
                    "todo chase {attempt}: {} item(s) open",
                    pending.len()
                ));
            }
            HarnessEvent::Notice { level, message } => {
                if level == "warning" || level == "error" {
                    self.notice(format!("{level}: {message}"));
                } else {
                    self.notice(message);
                }
            }
            HarnessEvent::Error { message } => {
                self.push(EntryKind::Error, "", message);
            }
            HarnessEvent::TurnComplete { stop_reason, .. } => {
                self.busy = false;
                self.last_stop = Some(stop_reason);
                self.streaming = None;
                self.end_reasoning();
                if let Some(next) = self.queue.pop_front() {
                    let agent = self.agent.clone();
                    self.busy = true;
                    self.push(EntryKind::User, "", next.clone());
                    tokio::spawn(async move {
                        let _ = agent.send(next).await;
                    });
                    if !self.queue.is_empty() {
                        self.notice(format!("{} queued", self.queue.len()));
                    }
                }
            }
        }
    }

    pub async fn handle_event(&mut self, event: Event, op_tx: &UnboundedSender<OpResult>) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // Informational overlays swallow exactly one key: close them
                // and let useful keys fall through to normal handling.
                if matches!(
                    self.overlay,
                    Overlay::Hotkeys | Overlay::Todos | Overlay::Processes | Overlay::Lsp
                ) {
                    self.overlay = Overlay::None;
                    if matches!(key.code, KeyCode::Esc) {
                        return;
                    }
                }
                let _ = self.handle_key(key, op_tx).await;
            }
            Event::Mouse(mouse) => {
                let in_body = mouse.column >= self.mouse_area.x
                    && mouse.column < self.mouse_area.right()
                    && mouse.row >= self.mouse_area.y
                    && mouse.row < self.mouse_area.bottom();
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.follow = false;
                        self.scroll = self.scroll.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll += 3;
                    }
                    MouseEventKind::Down(MouseButton::Left)
                        if in_body && !self.overlay_active() =>
                    {
                        let point = (
                            mouse.column - self.mouse_area.x,
                            mouse.row - self.mouse_area.y,
                        );
                        self.selection = Some((point, point));
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some((mut anchor, _)) = self.selection {
                            let col = mouse.column.clamp(
                                self.mouse_area.x,
                                self.mouse_area.right().saturating_sub(1),
                            );
                            let row = mouse.row.clamp(
                                self.mouse_area.y,
                                self.mouse_area.bottom().saturating_sub(1),
                            );
                            let point = (col - self.mouse_area.x, row - self.mouse_area.y);
                            // Dragging past an edge scrolls the transcript, like
                            // a native terminal; the anchor rides along so the
                            // selection keeps covering the same text.
                            let last = self.mouse_area.height.saturating_sub(1);
                            if point.1 == 0 && self.scroll > 0 {
                                self.follow = false;
                                self.scroll -= 1;
                                anchor.1 = anchor.1.saturating_add(1);
                            } else if point.1 == last && self.mouse_area.height > 1 {
                                self.follow = false;
                                self.scroll += 1;
                                anchor.1 = anchor.1.saturating_sub(1);
                            }
                            self.selection = Some((anchor, point));
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        self.finish_selection();
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                for ch in text.chars() {
                    if !ch.is_control() {
                        self.input.insert(self.cursor, ch);
                        self.cursor += 1;
                    }
                }
                self.refresh_slash();
            }
            Event::Resize(_, _) => self.follow = true,
            _ => {}
        }
    }

    fn move_sel(&mut self, down: bool) {
        let len = match &self.overlay {
            Overlay::Slash { items, .. } => items.len(),
            Overlay::Model { items, filter, .. } => model_filtered(items, filter).len(),
            Overlay::Session { items, filter, .. } => session_filtered(items, filter).len(),
            Overlay::FileBrowser { entries, .. } => entries.len(),
            Overlay::Settings { .. } => SETTINGS_ROWS.len(),
            _ => 0,
        };
        if len == 0 {
            return;
        }
        let sel = match &mut self.overlay {
            Overlay::Slash { sel, .. }
            | Overlay::Model { sel, .. }
            | Overlay::Session { sel, .. }
            | Overlay::FileBrowser { sel, .. }
            | Overlay::Settings { sel } => sel,
            _ => return,
        };
        *sel = if down {
            (*sel + 1).min(len - 1)
        } else {
            sel.saturating_sub(1)
        };
    }

    pub async fn handle_key(
        &mut self,
        key: KeyEvent,
        op_tx: &UnboundedSender<OpResult>,
    ) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if self.consent.is_some() {
            let allow = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
            if matches!(
                key.code,
                KeyCode::Char('y')
                    | KeyCode::Char('Y')
                    | KeyCode::Char('n')
                    | KeyCode::Char('N')
                    | KeyCode::Esc
            ) {
                let card = self.consent.take().unwrap();
                self.agent.grant_download_by_id(&card.id, allow);
                self.notice(if allow {
                    "download approved"
                } else {
                    "download declined"
                });
            }
            return Ok(());
        }

        if ctrl {
            match key.code {
                KeyCode::Char('c') => {
                    if self.input.is_empty() && !self.overlay_active() {
                        self.quit = true;
                    } else {
                        self.input.clear();
                        self.cursor = 0;
                        self.overlay = Overlay::None;
                    }
                    return Ok(());
                }
                KeyCode::Char('d') => {
                    self.quit = true;
                    return Ok(());
                }
                KeyCode::Char('o') => {
                    self.toggle_all(EntryKind::Tool);
                    return Ok(());
                }
                KeyCode::Char('t') => {
                    self.toggle_all(EntryKind::Reasoning);
                    return Ok(());
                }
                KeyCode::Char('l') => {
                    self.open_model_picker(ModelTarget::Agent);
                    return Ok(());
                }
                KeyCode::Char('r') => {
                    self.open_session_picker();
                    return Ok(());
                }
                KeyCode::Char('f') => {
                    self.open_file_browser("");
                    return Ok(());
                }
                KeyCode::Char('x') => {
                    self.copy_last_assistant();
                    return Ok(());
                }
                KeyCode::Char('g') => {
                    // Also doubles as "fix my screen": a full repaint clears
                    // anything the terminal left behind after an external
                    // scroll or a mode switch.
                    self.follow = true;
                    self.needs_repaint = true;
                    return Ok(());
                }
                KeyCode::Char('m') => {
                    self.mouse_enabled = !self.mouse_enabled;
                    let mut out = std::io::stdout();
                    let _ = if self.mouse_enabled {
                        execute!(out, crossterm::event::EnableMouseCapture)
                    } else {
                        execute!(out, crossterm::event::DisableMouseCapture)
                    };
                    self.notice(if self.mouse_enabled {
                        "mouse on - wheel scrolls, drag selects linearly and copies on release (ctrl+m hands selection back to the terminal)"
                    } else {
                        "mouse off — terminal handles selection natively again (Ctrl+M re-enables wheel + drag-copy)"
                    });
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }

        if self.overlay_active() {
            return self.handle_overlay_key(key, op_tx).await;
        }

        match key.code {
            KeyCode::Enter => {
                let text: String = self.input.iter().collect();
                self.input.clear();
                self.cursor = 0;
                self.slash_dismissed = false;
                let text = text.trim().to_string();
                if text.is_empty() {
                    return Ok(());
                }
                if text.starts_with('/') {
                    return self.run_command(&text, op_tx).await;
                }
                self.history.push(text.clone());
                self.history_pos = self.history.len();
                if self.busy {
                    self.queue.push_back(text.clone());
                    self.notice(if alt {
                        format!("follow-up queued ({})", self.queue.len())
                    } else {
                        format!("queued ({})", self.queue.len())
                    });
                    return Ok(());
                }
                self.push(EntryKind::User, "", text.clone());
                self.busy = true;
                self.follow = true;
                let agent = self.agent.clone();
                tokio::spawn(async move {
                    let _ = agent.send(text).await;
                });
                Ok(())
            }
            KeyCode::Esc => {
                if self.busy {
                    self.agent.cancel();
                    self.notice("cancelling...");
                } else {
                    self.input.clear();
                    self.cursor = 0;
                }
                Ok(())
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                    self.slash_dismissed = false;
                    self.refresh_slash();
                }
                Ok(())
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                    self.refresh_slash();
                }
                Ok(())
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Ok(())
            }
            KeyCode::Right => {
                if self.cursor < self.input.len() {
                    self.cursor += 1;
                }
                Ok(())
            }
            KeyCode::Home => {
                self.cursor = 0;
                Ok(())
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                Ok(())
            }
            KeyCode::Up => {
                if !self.history.is_empty() {
                    self.history_pos = self.history_pos.saturating_sub(1);
                    self.input = self.history[self.history_pos].chars().collect();
                    self.cursor = self.input.len();
                    self.refresh_slash();
                }
                Ok(())
            }
            KeyCode::Down => {
                if self.history_pos + 1 < self.history.len() {
                    self.history_pos += 1;
                    self.input = self.history[self.history_pos].chars().collect();
                } else {
                    self.history_pos = self.history.len();
                    self.input.clear();
                }
                self.cursor = self.input.len();
                self.refresh_slash();
                Ok(())
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(14);
                Ok(())
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(14);
                Ok(())
            }
            KeyCode::Char(ch) => {
                self.input.insert(self.cursor, ch);
                self.cursor += 1;
                self.slash_dismissed = false;
                self.refresh_slash();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn handle_overlay_key(
        &mut self,
        key: KeyEvent,
        op_tx: &UnboundedSender<OpResult>,
    ) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                let was_slash = matches!(self.overlay, Overlay::Slash { .. });
                self.overlay = Overlay::None;
                if was_slash {
                    self.slash_dismissed = true;
                } else {
                    self.refresh_slash();
                }
            }
            KeyCode::Up => self.move_sel(false),
            KeyCode::Down => self.move_sel(true),
            KeyCode::PageUp => {
                for _ in 0..8 {
                    self.move_sel(false);
                }
            }
            KeyCode::PageDown => {
                for _ in 0..8 {
                    self.move_sel(true);
                }
            }
            KeyCode::Left if matches!(self.overlay, Overlay::FileView { .. }) => {
                let rel = self.file_view_dir();
                self.overlay = Overlay::None;
                self.open_file_browser(&rel);
            }
            KeyCode::Enter => return self.overlay_confirm(op_tx).await,
            KeyCode::Char(c) => match &mut self.overlay {
                Overlay::Slash { .. } => {
                    self.input.insert(self.cursor, c);
                    self.cursor += 1;
                    self.slash_dismissed = false;
                    self.refresh_slash();
                }
                Overlay::Model { filter, sel, .. } | Overlay::Session { filter, sel, .. } => {
                    filter.push(c);
                    *sel = 0;
                }
                Overlay::FileView { scroll, lines, .. } => match c {
                    'j' => *scroll = scroll.saturating_add(1),
                    'k' => *scroll = scroll.saturating_sub(1),
                    'g' => *scroll = 0,
                    'G' => *scroll = lines.len(),
                    _ => {}
                },
                _ => {}
            },
            KeyCode::Backspace if matches!(self.overlay, Overlay::Slash { .. }) => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                }
                self.slash_dismissed = false;
                self.refresh_slash();
            }
            _ => {}
        }
        Ok(())
    }

    fn file_view_dir(&self) -> String {
        if let Overlay::FileView { path, .. } = &self.overlay {
            match path.rfind('/') {
                Some(index) => path[..index].to_string(),
                None => String::new(),
            }
        } else {
            String::new()
        }
    }

    pub fn overlay_active(&self) -> bool {
        !matches!(self.overlay, Overlay::None)
    }

    /// Identity of the current overlay for repaint-coalescing: `None` when no
    /// overlay is shown, else a small (kind, size) tag. When this changes we
    /// force a full repaint, because an overlay box of a different height or
    /// type can leave stale border glyphs outside its new (often shorter)
    /// area on terminals that do not erase them during an incremental redraw.
    pub fn overlay_key(&self) -> Option<(u8, usize)> {
        match &self.overlay {
            Overlay::None => None,
            Overlay::Slash { items, .. } => Some((1, items.len())),
            Overlay::Model { items, .. } => Some((2, items.len())),
            Overlay::Session { items, .. } => Some((3, items.len())),
            Overlay::FileBrowser { entries, .. } => Some((4, entries.len())),
            Overlay::FileView { lines, .. } => Some((5, lines.len())),
            Overlay::Settings { .. } => Some((6, SETTINGS_ROWS.len())),
            Overlay::Hotkeys => Some((7, 1)),
            Overlay::Todos => Some((8, self.todo.len())),
            Overlay::Processes => Some((9, self.processes.len())),
            Overlay::Lsp => Some((10, self.lsp.len())),
        }
    }

    /// Release of a left-drag: copy the selected transcript region.
    ///
    /// The selection is linear, like every terminal: rows between the anchor
    /// and the cursor are taken whole, and only the first and last row are cut
    /// at the column boundary.
    fn finish_selection(&mut self) {
        let Some((a, b)) = self.selection else {
            return;
        };
        if a == b {
            self.selection = None;
            self.toggle_row(a.1);
            return;
        }
        let Some(text) = self.selection_text() else {
            return;
        };
        let lines = text.lines().count();
        match copy_to_clipboard(&text) {
            Ok(()) => {
                self.selection = None;
                self.toast(format!("copied {lines} line{}", plural(lines)));
            }
            Err(err) => self.toast_error(format!("clipboard: {err}")),
        }
    }

    /// Plain text under the current selection, one entry per screen row, with
    /// the visual padding trimmed. `None` when the selection is empty.
    pub(crate) fn selection_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        if a == b {
            return None;
        }
        let ((top_col, top), (bottom_col, bottom)) = ordered(a, b);
        let cache = self.lines_cache.borrow();
        let mut picked: Vec<String> = Vec::new();
        for row in top..=bottom {
            let index = self.scroll + row;
            let Some(line) = cache.get(index) else {
                continue;
            };
            let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let Some((start, end)) = selection_row_span(row, top, top_col, bottom, bottom_col)
            else {
                continue;
            };
            picked.push(slice_columns(&plain, start, end).trim_end().to_string());
        }
        let text = picked.join("\n");
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn copy_last_assistant(&mut self) {
        let text = self
            .entries
            .iter()
            .rev()
            .find(|e| e.kind == EntryKind::Assistant)
            .map(|e| e.body.clone());
        match text {
            Some(text) => match copy_to_clipboard(&text) {
                Ok(()) => self.toast("copied last reply"),
                Err(err) => self.toast_error(format!("clipboard: {err}")),
            },
            None => self.notice("no assistant reply to copy yet"),
        }
    }

    fn refresh_slash(&mut self) {
        if let Overlay::Slash { .. } = self.overlay {
            self.overlay = Overlay::None;
        }
        if self.slash_dismissed {
            return;
        }
        let text: String = self.input.iter().collect();
        if text.starts_with('/') && !text.contains(' ') {
            let query = text[1..].to_lowercase();
            let items: Vec<PickerItem> = COMMANDS
                .iter()
                .filter(|(cmd, _)| {
                    let name = cmd.trim_start_matches('/').split(' ').next().unwrap_or("");
                    name.starts_with(&query)
                })
                .map(|(cmd, help)| PickerItem {
                    label: cmd.to_string(),
                    detail: help.to_string(),
                    value: cmd.to_string(),
                })
                .collect();
            if !items.is_empty() {
                self.overlay = Overlay::Slash { items, sel: 0 };
            }
        }
    }

    async fn overlay_confirm(&mut self, op_tx: &UnboundedSender<OpResult>) -> Result<()> {
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::Slash { items, sel } => {
                if let Some(item) = items.get(sel) {
                    let cmd = item.value.clone();
                    if let Some(cut) = cmd.find('<') {
                        self.input = cmd[..cut].trim_end().chars().collect();
                        self.cursor = self.input.len();
                        self.refresh_slash();
                    } else {
                        self.input.clear();
                        self.cursor = 0;
                        return self.run_command(&cmd, op_tx).await;
                    }
                }
            }
            Overlay::Model {
                items,
                sel,
                filter,
                target,
            } => {
                if let Some(item) = model_filtered(&items, &filter).get(sel) {
                    let model = item.value.clone();
                    self.set_model(&model, target);
                }
            }
            Overlay::Session { items, sel, filter } => {
                if let Some(item) = session_filtered(&items, &filter).get(sel) {
                    let id = item.value.clone();
                    let cwd = self.agent.cwd();
                    self.pending = Some(PendingSession {
                        resume: Some(id),
                        cwd,
                    });
                }
            }
            Overlay::FileBrowser { entries, sel, rel } => {
                if let Some((name, is_dir)) = entries.get(sel).cloned() {
                    let path = if rel.is_empty() || name == ".." {
                        if name == ".." {
                            match rel.rfind('/') {
                                Some(index) => rel[..index].to_string(),
                                None => String::new(),
                            }
                        } else {
                            name.clone()
                        }
                    } else {
                        format!("{rel}/{name}")
                    };
                    if name == ".." || is_dir {
                        self.open_file_browser(&path);
                    } else {
                        self.open_file(&path);
                    }
                }
            }
            Overlay::FileView { path, .. } => {
                self.open_file(&path);
            }
            Overlay::Settings { sel } => match sel {
                0 => self.open_model_picker(ModelTarget::Agent),
                1 => self.open_model_picker(ModelTarget::Compact),
                2 => self.notice(format!(
                    "workspace is {} — change it with /cwd <dir>",
                    self.cwd
                )),
                _ => {
                    let cost = self
                        .cost_usd
                        .map(|c| format!("{c:.4}"))
                        .unwrap_or_else(|| "n/a".into());
                    self.push(
                        EntryKind::Notice,
                        "",
                        format!(
                            "session {}\n  cwd {}\n  model {} | compact {}\n  ctx {}/{} | in {} | out {} | cost {}",
                            self.session_id,
                            self.cwd,
                            self.agent_model,
                            self.compact_model,
                            self.context_tokens,
                            self.context_limit,
                            self.input_tokens,
                            self.output_tokens,
                            cost
                        ),
                    );
                }
            },
            other => self.overlay = other,
        }
        Ok(())
    }

    async fn run_command(&mut self, text: &str, op_tx: &UnboundedSender<OpResult>) -> Result<()> {
        let mut parts = text.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim().to_string();
        match cmd {
            "/help" => {
                let list: Vec<String> = COMMANDS
                    .iter()
                    .map(|(c, h)| format!("{c:24} {h}"))
                    .collect();
                self.push(EntryKind::Notice, "commands", list.join("\n"));
            }
            "/hotkeys" => self.overlay = Overlay::Hotkeys,
            "/model" if arg.is_empty() => self.open_model_picker(ModelTarget::Agent),
            "/model" => self.set_model(&arg, ModelTarget::Agent),
            "/compact-model" if arg.is_empty() => self.open_model_picker(ModelTarget::Compact),
            "/compact-model" => self.set_model(&arg, ModelTarget::Compact),
            "/settings" => self.overlay = Overlay::Settings { sel: 0 },
            "/cwd" if arg.is_empty() => self.notice(format!("cwd: {}", self.cwd)),
            "/cwd" => match self.agent.set_cwd(Path::new(&arg)).await {
                Ok(()) => {
                    self.cwd = self.agent.cwd().to_string_lossy().to_string();
                    self.notice(format!("workspace: {}", self.cwd));
                }
                Err(err) => self.push(EntryKind::Error, "", err.to_string()),
            },
            "/new" => {
                let cwd = self.agent.cwd();
                self.pending = Some(PendingSession { resume: None, cwd });
            }
            "/resume" => self.open_session_picker(),
            "/session" => {
                let detail = format!(
                    "session {}\n  cwd {}\n  model {} | compact {}\n  ctx {}/{} | in {} | out {}",
                    self.session_id,
                    self.cwd,
                    self.agent_model,
                    self.compact_model,
                    self.context_tokens,
                    self.context_limit,
                    self.input_tokens,
                    self.output_tokens
                );
                self.push(EntryKind::Notice, "", detail);
            }
            "/clear" => {
                self.entries.clear();
                self.scroll = 0;
                self.follow = true;
            }
            "/files" => self.open_file_browser(&arg),
            "/view" if arg.is_empty() => self.notice("usage: /view <path>"),
            "/view" => self.open_file(&arg),
            "/todo" => self.overlay = Overlay::Todos,
            "/procs" => self.overlay = Overlay::Processes,
            "/lsp" => {
                self.lsp = self.agent.lsp_statuses();
                self.overlay = Overlay::Lsp;
            }
            "/checksum" if arg.is_empty() => self.notice("usage: /checksum <server-id>"),
            "/checksum" => {
                let agent = self.agent.clone();
                let server = arg.clone();
                let tx = op_tx.clone();
                tokio::spawn(async move {
                    let result = agent.lsp_checksum(&server).await.map_err(|e| e.to_string());
                    let _ = tx.send(OpResult::Checksum { server, result });
                });
                self.notice(format!("checksumming {arg}..."));
            }
            "/install" if arg.is_empty() => self.notice("usage: /install <server-id>"),
            "/install" => {
                let agent = self.agent.clone();
                let server = arg.clone();
                let tx = op_tx.clone();
                tokio::spawn(async move {
                    let result = agent
                        .install_lsp(&server, None, false)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(OpResult::Installed { server, result });
                });
                self.notice(format!("installing {arg}..."));
            }
            "/copy" => self.copy_last_assistant(),
            "/quit" | "/exit" => self.quit = true,
            other => self.push(
                EntryKind::Error,
                "",
                format!("unknown command: {other} — /help lists commands"),
            ),
        }
        Ok(())
    }

    fn set_model(&mut self, model: &str, target: ModelTarget) {
        let result = match target {
            ModelTarget::Agent => self.agent.set_agent_model(model),
            ModelTarget::Compact => self.agent.set_compact_model(model),
        };
        match result {
            Ok(()) => {
                self.agent_model = self.agent.agent_model();
                self.compact_model = self.agent.compact_model();
                let label = match target {
                    ModelTarget::Agent => "model",
                    ModelTarget::Compact => "compact model",
                };
                let value = match target {
                    ModelTarget::Agent => self.agent_model.clone(),
                    ModelTarget::Compact => self.compact_model.clone(),
                };
                self.notice(format!("{label}: {value}"));
            }
            Err(err) => self.push(EntryKind::Error, "", err.to_string()),
        }
    }

    fn open_model_picker(&mut self, target: ModelTarget) {
        let items: Vec<PickerItem> = self
            .config
            .model_catalog()
            .into_iter()
            .map(|spec| PickerItem {
                label: spec.reference(),
                detail: format!("{} | ctx {}", spec.name, human_count(spec.context_window)),
                value: spec.reference(),
            })
            .collect();
        if items.is_empty() {
            self.notice("no models configured — add providers to models.json");
        } else {
            let current = match target {
                ModelTarget::Agent => &self.agent_model,
                ModelTarget::Compact => &self.compact_model,
            };
            let sel = items
                .iter()
                .position(|item| item.value == *current)
                .unwrap_or(0);
            self.overlay = Overlay::Model {
                items,
                sel,
                filter: String::new(),
                target,
            };
        }
    }

    fn open_session_picker(&mut self) {
        let store = rustedharness_core::session::SessionStore::new(&self.config.session);
        let items: Vec<PickerItem> = store
            .list()
            .iter()
            .map(|s| PickerItem {
                label: s
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("{}...", s.id.chars().take(8).collect::<String>())),
                detail: format!(
                    "{} msgs | {} | {}",
                    s.messages,
                    shorten_path(&s.cwd, 28),
                    s.updated_at
                ),
                value: s.id.clone(),
            })
            .collect();
        if items.is_empty() {
            self.notice("no saved sessions");
        } else {
            self.overlay = Overlay::Session {
                items,
                sel: 0,
                filter: String::new(),
            };
        }
    }

    fn open_file_browser(&mut self, rel: &str) {
        let rel = rel.trim_matches('/').to_string();
        let root = self.agent.cwd();
        let dir = if rel.is_empty() {
            root.clone()
        } else {
            root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
        };
        let mut entries: Vec<(String, bool)> = Vec::new();
        if !rel.is_empty() {
            entries.push(("..".to_string(), true));
        }
        if let Ok(read) = std::fs::read_dir(&dir) {
            let mut items: Vec<(String, bool)> = Vec::new();
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir && is_skip_dir(&name) {
                    continue;
                }
                items.push((if is_dir { format!("{name}/") } else { name }, is_dir));
            }
            items.sort_by(|a, b| match (a.1, b.1) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.0.cmp(&b.0),
            });
            entries.extend(items);
        } else {
            self.notice(format!("cannot open directory {rel}"));
            self.overlay = Overlay::None;
            return;
        }
        self.overlay = Overlay::FileBrowser {
            sel: if rel.is_empty() { 0 } else { 1 },
            rel,
            entries,
        };
    }

    fn open_file(&mut self, rel: &str) {
        let path = self
            .agent
            .cwd()
            .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        match std::fs::read(&path) {
            Ok(bytes) => {
                if bytes.iter().take(8_192).any(|b| *b == 0) {
                    self.notice(format!("{rel}: binary file"));
                    self.overlay = Overlay::None;
                    return;
                }
                let limit = 512 * 1024;
                let truncated = bytes.len() > limit;
                let slice = if truncated {
                    &bytes[..limit]
                } else {
                    &bytes[..]
                };
                let text = String::from_utf8_lossy(slice).to_string();
                let lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
                self.overlay = Overlay::FileView {
                    path: rel.to_string(),
                    lines,
                    scroll: 0,
                    truncated,
                };
            }
            Err(err) => {
                self.overlay = Overlay::None;
                self.push(EntryKind::Error, "", format!("cannot read {rel}: {err}"));
            }
        }
    }
}

pub fn model_filtered<'a>(items: &'a [PickerItem], filter: &str) -> Vec<&'a PickerItem> {
    if filter.is_empty() {
        return items.iter().collect();
    }
    let needle = filter.to_lowercase();
    items
        .iter()
        .filter(|item| {
            item.label.to_lowercase().contains(&needle)
                || item.detail.to_lowercase().contains(&needle)
        })
        .collect()
}

pub fn session_filtered<'a>(items: &'a [PickerItem], filter: &str) -> Vec<&'a PickerItem> {
    model_filtered(items, filter)
}

fn is_skip_dir(name: &str) -> bool {
    matches!(name, "target" | "node_modules" | "dist" | "build")
}

pub(crate) fn shorten_path(path: &str, max: usize) -> String {
    if path.chars().count() <= max {
        return path.to_string();
    }
    let tail: String = path
        .chars()
        .rev()
        .take(max - 3)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail}")
}

/// Human-facing one-liner for a tool call: the primary argument instead of the
/// raw JSON blob, e.g. `[bash] cargo test --workspace` rather than
/// `{"command":"cargo test --workspace","timeout_ms":120000}`.
fn tool_summary(name: &str, raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return one_line(raw, 96);
    };
    let text = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let summary = match name {
        "grep" | "glob" => match (text("pattern"), text("path")) {
            (Some(pattern), Some(path)) => format!("{pattern}  in {path}"),
            (Some(pattern), None) => pattern,
            _ => first_string(&value, raw),
        },
        "edit" => {
            let count = value
                .get("edits")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len);
            match (text("path"), count) {
                (Some(path), Some(n)) => format!("{path}  ({n} edit{})", plural(n)),
                (Some(path), None) => path,
                (None, _) => first_string(&value, raw),
            }
        }
        "lsp" => match (text("op"), text("path")) {
            (Some(op), Some(path)) => format!("{op}  {path}"),
            (_, path) => path.unwrap_or_else(|| first_string(&value, raw)),
        },
        "todo" => value
            .get("items")
            .and_then(serde_json::Value::as_array)
            .map(|items| format!("{} item{}", items.len(), plural(items.len())))
            .unwrap_or_else(|| "list".to_string()),
        "await" => value
            .get("pid")
            .map(|pid| format!("pid {pid}"))
            .unwrap_or_default(),
        "web_search" => text("query").unwrap_or_else(|| first_string(&value, raw)),
        _ => text("command")
            .or_else(|| text("path"))
            .unwrap_or_else(|| first_string(&value, raw)),
    };
    one_line(&summary, 96)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn first_string(value: &serde_json::Value, fallback: &str) -> String {
    value
        .as_object()
        .and_then(|map| {
            map.values()
                .find_map(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| fallback.to_string())
}

/// Collapse a multi-line value into one row of at most `max` characters.
fn one_line(value: &str, max: usize) -> String {
    let flat: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!(
            "{}...",
            flat.chars().take(max.saturating_sub(3)).collect::<String>()
        )
    }
}

fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

pub fn human_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

pub fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick as usize / 4) % SPINNER.len()]
}

/// Order two selection corners top-left to bottom-right, comparing columns
/// only when both are on the same row (that is what makes the selection
/// linear rather than rectangular).
pub fn ordered(a: (u16, u16), b: (u16, u16)) -> ((usize, usize), (usize, usize)) {
    let first = (a.0 as usize, a.1 as usize);
    let second = (b.0 as usize, b.1 as usize);
    if (second.1, second.0) < (first.1, first.0) {
        (second, first)
    } else {
        (first, second)
    }
}

/// Half-open column range selected on `row`, or `None` when the row is outside
/// the selection. `(col, row)` tuples, not `(row, col)`.
pub fn selection_row_span(
    row: usize,
    top: usize,
    top_col: usize,
    bottom: usize,
    bottom_col: usize,
) -> Option<(usize, usize)> {
    if row < top || row > bottom {
        return None;
    }
    let start = if row == top { top_col } else { 0 };
    let end = if row == bottom {
        bottom_col + 1
    } else {
        usize::MAX
    };
    if end <= start {
        None
    } else {
        Some((start, end))
    }
}

/// Slice a rendered row by display columns so double-width characters are
/// never cut in half and the result matches what the user sees highlighted.
pub fn slice_columns(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut column = 0usize;
    for c in text.chars() {
        let width = ratatui::text::Span::raw(String::from(c)).width();
        if column >= end {
            break;
        }
        if column >= start {
            out.push(c);
        }
        column += width;
    }
    out
}

/// Copy through the platform clipboard API instead of piping bytes into
/// `clip.exe`, which decodes stdin with the ANSI code page and turns every
/// non-ASCII character into mojibake on a Chinese Windows.
fn copy_to_clipboard(text: &str) -> std::result::Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|err| format!("clipboard unavailable: {err}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|err| format!("copy failed: {err}"))
}
