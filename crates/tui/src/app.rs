//! Pi-style terminal frontend for RustedHarness.
//!
//! Layout (top to bottom, borderless like Pi):
//!   transcript | [overlay] | editor rule | status line | footer
//!
//! Feature parity with the web GUI: sessions (list/resume/new), full-screen
//! file browser and viewer, model picker, todos, LSP status, processes,
//! download consent, token/context stats, clipboard copy.

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
use rustedharness_core::agent::{Agent, RATE_LIMIT_RETRY_INTERVAL};
use rustedharness_core::config::{
    Config, CredentialKind, CredentialSource, NewModel, SearchBackend, SearchConfig,
};
use rustedharness_core::discovery::CatalogModel;
use rustedharness_core::lsp::ServerStatus;
use rustedharness_core::protocol::{HarnessEvent, SessionSnapshot};
use rustedharness_core::provider::{ChatMessage, Role};
use rustedharness_core::shell::{GIT_BASH, POWERSHELL};
use rustedharness_core::tools::todo::TodoItem;
use tokio::sync::mpsc::UnboundedSender;

use crate::entries::{Entry, EntryKind, ProcessCard};

/// Long-running action routed through the event loop so rendering never blocks.
pub enum OpResult {
    Installed {
        server: String,
        result: std::result::Result<PathBuf, String>,
    },
    /// A file finished highlighting on a worker thread.
    Highlighted {
        path: String,
        lines: Vec<Line<'static>>,
    },
    /// `GET {base}/models` answered for the add-model flow. The url is echoed so
    /// a reply cannot land in a different wizard.
    Catalog {
        url: String,
        result: std::result::Result<Vec<CatalogModel>, String>,
    },
}

/// Files above this size open without syntax highlighting. The highlighter is
/// linear and a viewer is a peek tool; a half-megabyte source would spend
/// seconds of CPU on colours nobody is going to read.
const HIGHLIGHT_LIMIT: usize = 96 * 1024;

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
    /// Dimmed and inert: the selected mode makes this option meaningless.
    pub disabled: bool,
}

/// One user input offered by `/rollback`.
#[derive(Clone)]
pub struct TurnRow {
    /// `#{n}` plus the first line of the input, as the row reads.
    pub label: String,
    /// The full input, shown before the rollback is confirmed.
    pub text: String,
    /// Messages after this input: exactly what a rollback deletes.
    pub after: usize,
    /// Position among the user inputs, handed back to the agent.
    pub turn: usize,
}

impl TurnRow {
    pub fn picker_row(&self) -> PickerItem {
        PickerItem {
            label: self.label.clone(),
            detail: if self.after == 0 {
                "nothing after it".to_string()
            } else {
                format!("{} after", self.after)
            },
            value: self.turn.to_string(),
            // The newest input has nothing after it, so rolling back to it
            // would not change anything.
            disabled: self.after == 0,
        }
    }
}

pub enum Overlay {
    None,
    Slash {
        items: Vec<PickerItem>,
        sel: usize,
        /// Text the chosen value completes. Empty while completing a command
        /// name; `/lsp install ` while completing a server id.
        prefix: String,
    },
    Model {
        items: Vec<PickerItem>,
        sel: usize,
        filter: String,
        target: ModelTarget,
        /// Opened from `/settings`; return to the compaction panel after a
        /// choice instead of closing every overlay.
        from_settings: bool,
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
        /// Syntax-highlighted file body, one entry per source line. Built once
        /// when the file is opened: highlighting 500 KB on every keypress would
        /// make scrolling stutter.
        lines: Vec<Line<'static>>,
        scroll: usize,
        truncated: bool,
    },
    Settings {
        page: SettingsPage,
        sel: usize,
    },
    Hotkeys,
    Todos,
    Processes,
    Lsp,
    /// Full-screen "add model" flow opened from the model picker.
    AddModel(AddModelState),
    /// Pick the user input the conversation is rolled back to.
    Rollback {
        rows: Vec<TurnRow>,
        sel: usize,
    },
    /// Second step of `/rollback`: nothing is cut until this is confirmed.
    RollbackConfirm {
        rows: Vec<TurnRow>,
        sel: usize,
    },
    /// Typing the credential of the selected search backend.
    WebSearchKey {
        backend: SearchBackend,
        text: String,
        cursor: usize,
    },
}

/// Which half of the add-model flow is on screen.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AddModelStep {
    /// Base URL and API key, then a fetch.
    Endpoint,
    /// Models the endpoint listed; Enter saves the selection.
    Models,
}

/// Progress of the add-model flow.
pub struct AddModelState {
    pub step: AddModelStep,
    pub url: String,
    pub key: String,
    /// Focused row on the endpoint step: 0 base url, 1 api key, 2 fetch.
    pub field: usize,
    /// Caret inside the focused text field.
    pub cursor: usize,
    pub items: Vec<CatalogModel>,
    pub sel: usize,
    /// Narrows the list, and doubles as a hand-typed model id.
    pub filter: String,
    pub busy: bool,
    pub error: Option<String>,
    /// Which model slot the wizard saves into.
    pub target: ModelTarget,
    /// Opened from `/settings`; return there once the model is saved.
    pub from_settings: bool,
}

impl AddModelState {
    pub fn new(target: ModelTarget, from_settings: bool) -> Self {
        Self {
            step: AddModelStep::Endpoint,
            url: String::new(),
            key: String::new(),
            field: 0,
            cursor: 0,
            items: Vec::new(),
            sel: 0,
            filter: String::new(),
            busy: false,
            error: None,
            target,
            from_settings,
        }
    }

    /// Run `edit` on the focused text field of the endpoint step. Returns
    /// false when no text field has focus.
    fn edit(&mut self, edit: impl FnOnce(&mut String, &mut usize)) -> bool {
        let text = match (self.step, self.field) {
            (AddModelStep::Endpoint, 0) => &mut self.url,
            (AddModelStep::Endpoint, 1) => &mut self.key,
            _ => return false,
        };
        edit(text, &mut self.cursor);
        true
    }

    /// Models matching the filter, or every model while it is empty.
    pub fn filtered(&self) -> Vec<&CatalogModel> {
        let needle = self.filter.trim().to_lowercase();
        self.items
            .iter()
            .filter(|model| needle.is_empty() || model.id.to_lowercase().contains(&needle))
            .collect()
    }
}

/// Which model slot the picker edits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModelTarget {
    Agent,
    Compact,
}

/// `/settings` pages. The root links to the compaction submenu.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    Root,
    Compaction,
    WebSearch,
}

/// A download in flight: the server, how far it got and what of.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub server: String,
    pub percent: Option<u8>,
    pub detail: Option<String>,
}

impl DownloadProgress {
    /// `"clangd 42% 12.3/29.4 MB"`, or the server alone before bytes arrive.
    fn text(&self) -> String {
        match (&self.detail, self.percent) {
            (Some(detail), _) if detail.contains('%') || detail.contains("MB") => {
                format!("{} {detail}", self.server)
            }
            (Some(detail), Some(percent)) => format!("{} {percent}% {detail}", self.server),
            (Some(detail), None) => format!("{} {detail}", self.server),
            (None, Some(percent)) => format!("{} {percent}%", self.server),
            (None, None) => self.server.clone(),
        }
    }
}

pub struct ConsentCard {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub url: String,
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
    pub compact_is_separate: bool,
    pub compaction: rustedharness_core::config::CompactionSettings,
    /// Mirror of `[search]` from the live agent config.
    pub search: SearchConfig,
    /// The download in flight, shown in the status line.
    pub download: Option<DownloadProgress>,
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
    /// Set when the transcript was rebuilt, so the renderer knows to rewrite
    /// the rows holding double-width glyphs (conhost only drops the trailing
    /// cell of a wide glyph if someone writes over it).
    pub wide_rows_dirty: bool,
    /// Scroll offset of the last painted frame; scrolling brings new rows on
    /// screen, which can expose the same wide-glyph residue.
    pub painted_scroll: usize,
    /// Overlay kind of the last painted frame.
    pub painted_overlay: Option<u8>,
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
    ("/cwd <dir>", "move the workspace root"),
    ("/new", "start a fresh session"),
    ("/resume", "pick a saved session"),
    ("/rollback", "cut the conversation back to an earlier input"),
    ("/session", "show session id, models and context usage"),
    (
        "/settings",
        "open settings (compaction model, thinking and limits)",
    ),
    ("/files", "browse the workspace file tree"),
    ("/view <path>", "open a file read-only"),
    ("/lsp", "language server status"),
    ("/lsp install <id>", "download the newest release"),
    ("/lsp update <id>", "re-install from the newest release"),
    ("/procs", "background processes"),
    ("/copy", "copy the last assistant reply"),
    ("/quit", "exit"),
];

/// Command matches for the typed prefix, e.g. `"/ls"` -> `/lsp ...`.
fn filtered_commands(text: &str) -> Vec<PickerItem> {
    if text.contains(' ') {
        return Vec::new();
    }
    let query = text.trim_start_matches('/').to_lowercase();
    COMMANDS
        .iter()
        .filter(|(cmd, _)| {
            let name = cmd.trim_start_matches('/').split(' ').next().unwrap_or("");
            name.starts_with(&query)
        })
        .map(|(cmd, help)| PickerItem {
            label: cmd.to_string(),
            detail: help.to_string(),
            value: cmd.to_string(),
            disabled: false,
        })
        .collect()
}

/// Where a server id belongs, plus the part typed so far: `"/lsp install ru"`
/// -> `("/lsp install ", "ru")`. `None` while the text is not a `/lsp` verb
/// that takes an id.
fn lsp_id_query(text: &str) -> Option<(&'static str, &str)> {
    let rest = text.strip_prefix("/lsp ")?;
    let (verb, partial) = match rest.split_once(' ') {
        Some((verb, partial)) => (verb, partial),
        None => (rest, ""),
    };
    let prefix = match verb {
        "install" => "/lsp install ",
        "update" => "/lsp update ",
        _ => return None,
    };
    Some((prefix, partial))
}

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
            compact_is_separate: false,
            compaction: rustedharness_core::config::CompactionSettings::from(
                &rustedharness_core::config::AgentConfig::default(),
            ),
            search: SearchConfig::default(),
            download: None,
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
            wide_rows_dirty: false,
            painted_scroll: 0,
            painted_overlay: None,
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
        self.compact_is_separate = self.agent.compact_is_separate();
        self.compaction = self.agent.compaction_settings();
        self.search = self.agent.config().search.clone();
        self.context_tokens = snapshot.context_tokens;
        self.context_limit = snapshot.context_limit;
        // A resumed session shows the numbers the provider last reported, so
        // the footer keeps saying what this session has spent instead of
        // restarting from zero. A rolled-back session is shorter, so the meters
        // are replaced rather than only set when a report is present.
        let usage = snapshot.usage.as_ref();
        self.input_tokens = usage.map_or(0, |usage| usage.prompt_tokens);
        self.output_tokens = usage.map_or(0, |usage| usage.completion_tokens);
        self.cache_hit_pct = usage.and_then(|usage| usage.cache_hit_pct());
        self.cost_usd = snapshot.cost_usd;
        self.todo = snapshot.todos;
        for message in snapshot.messages {
            self.push_message(message);
        }
    }

    /// Rebuild the transcript from the agent after its history changed
    /// underneath it (`/rollback`).
    pub fn reload_transcript(&mut self) {
        self.entries.clear();
        self.tool_slots.clear();
        self.streaming = None;
        self.reasoning_at = None;
        self.selection = None;
        self.scroll = 0;
        self.follow = true;
        self.populate(self.agent.snapshot());
        self.needs_repaint = true;
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

    /// `lsp` segment of the status line: the server that is up (or starting),
    /// its warm-up state, and how many others are running. `None` when no
    /// server is live, so a workspace without one adds no noise.
    pub fn lsp_summary(&self) -> Option<(String, bool)> {
        if let Some(download) = &self.download {
            return Some((format!("lsp {}", download.text()), true));
        }
        let mut live: Vec<&ServerStatus> = self
            .lsp
            .iter()
            .filter(|status| matches!(status.state.as_str(), "running" | "warming" | "degraded"))
            .collect();
        if live.is_empty() {
            return None;
        }
        // The row that needs attention first: a server that is down, then one
        // still loading, then the boring good case. `/lsp` has the full list.
        live.sort_by_key(|status| match status.state.as_str() {
            "degraded" => 0,
            "warming" => 1,
            _ => 2,
        });
        let head = live[0];
        let state = match head.state.as_str() {
            "warming" => " warm",
            "degraded" => " down",
            _ => "",
        };
        let extra = live.len() - 1;
        let more = if extra > 0 {
            format!(" +{extra}")
        } else {
            String::new()
        };
        // The language keeps the segment short; the panel (`/lsp`) shows the
        // server id, version and detail for the same row.
        Some((
            format!("lsp {}{state}{more}", head.language),
            head.state == "warming",
        ))
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        self.push(EntryKind::Notice, "", text);
    }

    pub fn on_op_result(&mut self, result: OpResult) {
        match result {
            OpResult::Installed { server, result } => match result {
                Ok(path) => self.notice(format!("{server} ready at {}", path.display())),
                Err(err) => self.push(EntryKind::Error, "", format!("{server} install: {err}")),
            },
            OpResult::Catalog { url, result } => {
                self.apply_catalog(&url, result);
            }
            OpResult::Highlighted { path, lines } => {
                // Only swap if that file is still on screen: the user may have
                // closed the viewer or opened another file in the meantime.
                if let Overlay::FileView {
                    path: current,
                    lines: current_lines,
                    ..
                } = &mut self.overlay
                {
                    if current == &path {
                        *current_lines = lines;
                        self.needs_repaint = true;
                    }
                }
            }
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
                // A failed download never sends a final DownloadStatus, so the
                // status line would keep showing the last percentage.
                if state != "downloading" {
                    let server =
                        rustedharness_core::lsp::spec_for_language(&language).map(|s| s.id);
                    if self
                        .download
                        .as_ref()
                        .is_some_and(|download| Some(download.server.as_str()) == server)
                    {
                        self.download = None;
                    }
                }
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
                runtime_hint,
                ..
            } => {
                self.consent = Some(ConsentCard {
                    id,
                    display_name,
                    version,
                    url,
                    runtime_hint,
                });
            }
            HarnessEvent::DownloadStatus {
                server_id,
                state,
                detail,
                percent,
            } => {
                // Progress belongs in the status line: a notice per chunk
                // would flood the transcript during a 50 MB download.
                if state == "downloading" {
                    self.download = Some(DownloadProgress {
                        server: server_id,
                        percent,
                        detail,
                    });
                    return;
                }
                self.download = None;
                let hint = match state.as_str() {
                    "update_available" => format!(" - /lsp update {server_id}"),
                    _ => String::new(),
                };
                self.notice(format!(
                    "lsp {server_id}: {state}{}{hint}",
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
                if level == "cache" {
                    // Worth keeping: it explains a jump in input cost.
                    self.push(EntryKind::Notice, "cache", message);
                } else if level == "warning" || level == "error" {
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
            Overlay::Rollback { rows, .. } => rows.len(),
            Overlay::Settings { .. } => self.settings_items().len(),
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
            | Overlay::Rollback { sel, .. }
            | Overlay::Settings { sel, .. } => sel,
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
                    // The todo list is the one panel worth a key of its own.
                    self.overlay = if matches!(self.overlay, Overlay::Todos) {
                        Overlay::None
                    } else {
                        Overlay::Todos
                    };
                    return Ok(());
                }
                KeyCode::Char('f') => {
                    self.open_file_browser("");
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
        if matches!(self.overlay, Overlay::AddModel(_)) {
            return self.add_model_key(key, op_tx).await;
        }
        if matches!(self.overlay, Overlay::WebSearchKey { .. }) {
            return self.web_search_key(key);
        }
        match key.code {
            KeyCode::Esc => self.overlay_escape(),
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
            KeyCode::Left if matches!(self.overlay, Overlay::Settings { .. }) => {
                self.adjust_setting(false);
            }
            KeyCode::Right if matches!(self.overlay, Overlay::Settings { .. }) => {
                self.adjust_setting(true);
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

    /// One step back from whatever is on screen: a confirmation returns to the
    /// list it confirmed, a file to the browser it came from, and anything else
    /// closes.
    fn overlay_escape(&mut self) {
        match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::FileView { path, .. } => {
                self.back_to_browser(&path);
                return;
            }
            Overlay::RollbackConfirm { rows, sel } => {
                self.overlay = Overlay::Rollback { rows, sel };
                return;
            }
            Overlay::Slash { .. } => self.slash_dismissed = true,
            _ => {}
        }
        self.refresh_slash();
    }

    pub fn overlay_active(&self) -> bool {
        !matches!(self.overlay, Overlay::None)
    }

    /// Identity of the current overlay: `None` when nothing is shown, else the
    /// overlay kind. Only the *kind* is part of the key - not the number of
    /// rows - because the row count changes on every keystroke while a picker
    /// filter narrows, and reacting to that would repaint the screen on every
    /// character typed.
    ///
    /// Opening, closing or switching overlays is what needs attention: the
    /// panel may be smaller than the previous one, and a terminal keeps the
    /// right half of a double-width glyph nobody writes over.
    pub fn overlay_key(&self) -> Option<u8> {
        match &self.overlay {
            Overlay::None => None,
            Overlay::Slash { .. } => Some(1),
            Overlay::Model { .. } => Some(2),
            Overlay::Session { .. } => Some(3),
            Overlay::FileBrowser { .. } => Some(4),
            Overlay::FileView { .. } => Some(5),
            Overlay::Settings { .. } => Some(6),
            Overlay::Hotkeys => Some(7),
            Overlay::Todos => Some(8),
            Overlay::Processes => Some(9),
            Overlay::Lsp => Some(10),
            Overlay::AddModel(_) => Some(11),
            Overlay::WebSearchKey { .. } => Some(12),
            Overlay::Rollback { .. } => Some(13),
            Overlay::RollbackConfirm { .. } => Some(14),
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

    pub(crate) fn refresh_slash(&mut self) {
        if let Overlay::Slash { .. } = self.overlay {
            self.overlay = Overlay::None;
        }
        if self.slash_dismissed {
            return;
        }
        let text: String = self.input.iter().collect();
        if !text.starts_with('/') {
            return;
        }
        // `/lsp install <id>` completes server ids; every other prefix
        // completes command names.
        let (items, prefix) = match lsp_id_query(&text) {
            Some((prefix, partial)) => (self.lsp_server_items(partial), prefix.to_string()),
            None => (filtered_commands(&text), String::new()),
        };
        if !items.is_empty() {
            self.overlay = Overlay::Slash {
                items,
                sel: 0,
                prefix,
            };
        }
    }

    /// Registry servers a `/lsp` verb accepts, with what is installed and
    /// whether the release can be downloaded at all.
    fn lsp_server_items(&self, partial: &str) -> Vec<PickerItem> {
        let needle = partial.to_ascii_lowercase();
        rustedharness_core::lsp::registry()
            .iter()
            .filter(|spec| spec.id.to_ascii_lowercase().starts_with(&needle))
            .map(|spec| {
                let installed = self
                    .lsp
                    .iter()
                    .find(|status| status.server == spec.id)
                    .and_then(|status| status.version.clone());
                let state = match installed {
                    Some(version) => format!("installed {version}"),
                    None if spec.asset.is_some() => "downloads from GitHub".to_string(),
                    None => "install manually".to_string(),
                };
                PickerItem {
                    label: spec.id.to_string(),
                    detail: format!("{} | {state}", spec.display),
                    value: spec.id.to_string(),
                    disabled: false,
                }
            })
            .collect()
    }

    async fn overlay_confirm(&mut self, op_tx: &UnboundedSender<OpResult>) -> Result<()> {
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::Slash { items, sel, prefix } => {
                if let Some(item) = items.get(sel) {
                    if !prefix.is_empty() {
                        // A server id was chosen: finish the line and keep the
                        // menu closed until the next keystroke.
                        self.input = format!("{prefix}{}", item.value).chars().collect();
                        self.cursor = self.input.len();
                        self.slash_dismissed = true;
                        return Ok(());
                    }
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
                from_settings,
            } => {
                if let Some(item) = model_filtered(&items, &filter).get(sel) {
                    let model = item.value.clone();
                    if model == ADD_MODEL_ROW {
                        self.overlay = Overlay::AddModel(AddModelState::new(target, from_settings));
                        return Ok(());
                    }
                    self.set_model(&model, target);
                    if from_settings {
                        self.overlay = Overlay::Settings {
                            page: SettingsPage::Compaction,
                            sel: 0,
                        };
                    }
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
                        self.open_file(&path, op_tx);
                    }
                }
            }
            Overlay::FileView { path, .. } => {
                self.open_file(&path, op_tx);
            }
            Overlay::Rollback { rows, sel } => {
                if rows.get(sel).is_some_and(|row| row.after > 0) {
                    self.overlay = Overlay::RollbackConfirm { rows, sel };
                } else {
                    self.notice("this input already ends the conversation");
                    self.overlay = Overlay::Rollback { rows, sel };
                }
            }
            Overlay::RollbackConfirm { rows, sel } => {
                let Some(row) = rows.get(sel) else {
                    return Ok(());
                };
                let (turn, after) = (row.turn, row.after);
                match self.agent.rollback_to_turn(turn) {
                    Ok(()) => {
                        self.reload_transcript();
                        self.notice(format!(
                            "rolled back to #{}: {after} messages dropped",
                            turn + 1
                        ));
                    }
                    Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                }
            }
            Overlay::Settings { page, sel } => {
                // `overlay_confirm` cleared the overlay before dispatching.
                // Keep the panel open for toggles and numeric adjustments, so
                // `adjust_setting` can still see which row is selected.
                self.overlay = Overlay::Settings { page, sel };
                match page {
                    SettingsPage::Root => {
                        let value = self
                            .settings_items_for(page)
                            .get(sel)
                            .map(|item| item.value.clone())
                            .unwrap_or_default();
                        match value.as_str() {
                            "compaction" => {
                                self.overlay = Overlay::Settings {
                                    page: SettingsPage::Compaction,
                                    sel: 0,
                                }
                            }
                            "search" => {
                                self.overlay = Overlay::Settings {
                                    page: SettingsPage::WebSearch,
                                    sel: 0,
                                }
                            }
                            "retry" => {
                                let next = next_retry_preset(self.agent.rate_limit_retries());
                                match self.agent.set_rate_limit_retries(next) {
                                    Ok(()) => self.notice(format!("429 retries: {next}, 3s apart")),
                                    Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                                }
                            }
                            "image" => {
                                let next = next_image_size(self.agent.config().image.max_edge);
                                match self.agent.set_image_max_edge(next) {
                                    Ok(()) => self
                                        .notice(format!("image size: {}", image_size_label(next))),
                                    Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                                }
                            }
                            "terminal" => {
                                let next = next_terminal_prefer(&self.agent.config().shell.prefer);
                                match self.agent.set_shell_prefer(next) {
                                    Ok(()) => self.notice(format!(
                                        "agent terminal: {}",
                                        terminal_label(next)
                                    )),
                                    Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                                }
                            }
                            "notices" => {
                                let on = !self.agent.show_cache_miss_notices();
                                match self.agent.set_show_cache_miss_notices(on) {
                                    Ok(()) => self.notice(format!(
                                        "cache miss notices: {}",
                                        if on { "on" } else { "off" }
                                    )),
                                    Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                                }
                            }
                            _ => {}
                        }
                    }
                    SettingsPage::WebSearch => {
                        let Some(item) = self.settings_items_for(page).get(sel).cloned() else {
                            return Ok(());
                        };
                        if item.disabled {
                            return Ok(());
                        }
                        if let Some(label) = item.value.strip_prefix("backend:") {
                            let Some(backend) = SearchBackend::ALL
                                .into_iter()
                                .find(|backend| backend.label() == label)
                            else {
                                return Ok(());
                            };
                            match self.agent.set_search_backend(backend) {
                                Ok(()) => {
                                    self.search = self.agent.config().search.clone();
                                    self.notice(format!("web search: {}", backend.label()));
                                    // The credential row depends on the backend.
                                    self.overlay = Overlay::Settings {
                                        page: SettingsPage::WebSearch,
                                        sel: 0,
                                    };
                                }
                                Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                            }
                        } else if item.value == "credential" {
                            let backend = self.search.backend;
                            self.overlay = Overlay::WebSearchKey {
                                backend,
                                text: String::new(),
                                cursor: 0,
                            };
                        }
                    }
                    SettingsPage::Compaction => {
                        let Some(item) = self.settings_items_for(page).get(sel).cloned() else {
                            return Ok(());
                        };
                        if item.disabled {
                            let message = match item.value.as_str() {
                            "strip" => "strip is disabled while compaction uses the main model: the full history is required for cache hits",
                            "send" | "content" => "enable strip old context first",
                            _ => "this option is disabled",
                        };
                            self.notice(message);
                            return Ok(());
                        }
                        match item.value.as_str() {
                            "model" => self.open_model_picker(ModelTarget::Compact, true),
                            "threshold" | "keep" | "send" => self.adjust_setting(true),
                            "strip" => {
                                let on = !self.compaction.compact_strip_context;
                                match self.agent.set_compact_strip_context(on) {
                                    Ok(()) => {
                                        self.compaction = self.agent.compaction_settings();
                                        self.notice(format!(
                                            "strip old context with a separate model: {}",
                                            if on { "on" } else { "off" }
                                        ));
                                    }
                                    Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                                }
                            }
                            "thinking" => {
                                let level = self.compaction.compact_thinking.next();
                                match self.agent.set_compact_thinking(level) {
                                    Ok(()) => {
                                        self.compaction = self.agent.compaction_settings();
                                        self.notice(format!(
                                            "compaction thinking: {}",
                                            level.label()
                                        ));
                                    }
                                    Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                                }
                            }
                            "content" => {
                                let content = self.compaction.strip_content.next();
                                match self.agent.set_strip_content(content) {
                                    Ok(()) => {
                                        self.compaction = self.agent.compaction_settings();
                                        self.notice(format!("strip content: {}", content.label()));
                                    }
                                    Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
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
            "/model" if arg.is_empty() => self.open_model_picker(ModelTarget::Agent, false),
            "/model" => self.set_model(&arg, ModelTarget::Agent),
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
            "/rollback" => self.open_rollback(),
            "/session" => {
                let detail = format!(
                    "session {}\n  cwd {}\n  model {} | compact {}\n  ctx {}/{} | in {} | out {}",
                    self.session_id,
                    self.cwd,
                    rustedharness_core::config::model_display(&self.agent_model),
                    rustedharness_core::config::model_display(&self.compact_model),
                    self.context_tokens,
                    self.context_limit,
                    self.input_tokens,
                    self.output_tokens
                );
                self.push(EntryKind::Notice, "", detail);
            }

            "/files" => self.open_file_browser(&arg),
            "/view" if arg.is_empty() => self.notice("usage: /view <path>"),
            "/view" => self.open_file(&arg, op_tx),
            "/settings" => {
                self.overlay = Overlay::Settings {
                    page: SettingsPage::Root,
                    sel: 0,
                }
            }
            "/procs" => self.overlay = Overlay::Processes,
            "/lsp" => self.lsp_command(&arg, op_tx),
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

    /// `/lsp` and its subcommands; everything language-server related lives
    /// here instead of in top-level commands.
    fn lsp_command(&mut self, arg: &str, op_tx: &UnboundedSender<OpResult>) {
        let (sub, server) = match arg.split_once(' ') {
            Some((sub, rest)) => (sub, rest.trim()),
            None => (arg, ""),
        };
        match sub {
            "" => {
                self.lsp = self.agent.lsp_statuses();
                self.overlay = Overlay::Lsp;
            }
            "install" | "update" => {
                if server.is_empty() {
                    self.notice(format!("usage: /lsp {sub} <server-id>"));
                    return;
                }
                let update = sub == "update";
                let server = server.to_string();
                let agent = self.agent.clone();
                let tx = op_tx.clone();
                let name = server.clone();
                tokio::spawn(async move {
                    let result = agent
                        .install_lsp(&name, update)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(OpResult::Installed {
                        server: name,
                        result,
                    });
                });
                self.notice(format!(
                    "{} {server}...",
                    if update { "updating" } else { "installing" }
                ));
            }
            other => self.push(
                EntryKind::Error,
                "",
                format!("unknown lsp subcommand: {other} - try /lsp, /lsp install or /lsp update"),
            ),
        }
    }

    fn set_model(&mut self, model: &str, target: ModelTarget) {
        let model = if target == ModelTarget::Compact && model.trim().eq_ignore_ascii_case("none") {
            ""
        } else {
            model
        };
        let result = match target {
            ModelTarget::Agent => self.agent.set_agent_model(model),
            ModelTarget::Compact => self.agent.set_compact_model(model),
        };
        match result {
            Ok(()) => {
                self.agent_model = self.agent.agent_model();
                self.compact_model = self.agent.compact_model();
                self.compact_is_separate = self.agent.compact_is_separate();
                let label = match target {
                    ModelTarget::Agent => "model",
                    ModelTarget::Compact => "compact model",
                };
                let value = match target {
                    ModelTarget::Agent => self.agent_model.clone(),
                    ModelTarget::Compact if !self.compact_is_separate => {
                        "None (main model)".to_string()
                    }
                    ModelTarget::Compact => self.compact_model.clone(),
                };
                self.notice(format!("{label}: {value}"));
            }
            Err(err) => self.push(EntryKind::Error, "", err.to_string()),
        }
    }

    /// Send the endpoint to `GET /models` on a worker task; the reply arrives
    /// as `OpResult::Catalog` so the UI never blocks on the network.
    fn fetch_catalog(&mut self, op_tx: &UnboundedSender<OpResult>) {
        let Overlay::AddModel(state) = &mut self.overlay else {
            return;
        };
        let url = state.url.trim().to_string();
        if url.is_empty() {
            state.error =
                Some("enter a base url first, e.g. https://api.openai.com/v1".to_string());
            return;
        }
        state.busy = true;
        state.error = None;
        let key = state.key.clone();
        let tx = op_tx.clone();
        tokio::spawn(async move {
            let result = rustedharness_core::discovery::fetch_catalog(&url, &key)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(OpResult::Catalog { url, result });
        });
    }

    /// Keys for the search-credential editor: one text field with save/cancel.
    fn web_search_key(&mut self, key: KeyEvent) -> Result<()> {
        let Overlay::WebSearchKey {
            backend,
            text,
            cursor,
        } = &mut self.overlay
        else {
            return Ok(());
        };
        let backend = *backend;
        let close = |overlay: &mut Overlay| {
            *overlay = Overlay::Settings {
                page: SettingsPage::WebSearch,
                sel: CREDENTIAL_ROW,
            };
        };
        match key.code {
            KeyCode::Esc => close(&mut self.overlay),
            KeyCode::Enter => {
                let value = text.trim().to_string();
                match self.agent.set_search_credential(backend, &value) {
                    Ok(()) => {
                        self.search = self.agent.config().search.clone();
                        close(&mut self.overlay);
                        self.notice(if value.is_empty() {
                            format!("{} credential cleared", backend.label())
                        } else {
                            format!("{} credential saved", backend.label())
                        });
                    }
                    Err(err) => self.push(EntryKind::Error, "", err.to_string()),
                }
            }
            KeyCode::Char(c) => insert_at(text, cursor, c),
            KeyCode::Backspace => delete_before(text, cursor),
            _ => {}
        }
        Ok(())
    }

    /// Fold a `GET /models` answer into the wizard. A url mismatch means the
    /// user already closed or restarted the flow, so the reply is dropped.
    fn apply_catalog(&mut self, url: &str, result: std::result::Result<Vec<CatalogModel>, String>) {
        let Overlay::AddModel(state) = &mut self.overlay else {
            return;
        };
        if state.url.trim() != url {
            return;
        }
        state.busy = false;
        match result {
            Ok(items) => {
                let empty = items.is_empty();
                state.items = items;
                state.sel = 0;
                state.filter.clear();
                state.step = AddModelStep::Models;
                if empty {
                    state.error =
                        Some("the endpoint listed no models; type the model id".to_string());
                }
            }
            Err(err) => state.error = Some(err),
        }
    }

    /// Keys for the add-model wizard. Text fields own every printable key, so
    /// this runs before the generic overlay handling.
    async fn add_model_key(
        &mut self,
        key: KeyEvent,
        op_tx: &UnboundedSender<OpResult>,
    ) -> Result<()> {
        let Overlay::AddModel(state) = &mut self.overlay else {
            return Ok(());
        };
        // While a fetch is in flight only Esc is accepted; Enter would start a
        // second request against the same endpoint.
        if state.busy && !matches!(key.code, KeyCode::Esc) {
            return Ok(());
        }
        match key.code {
            KeyCode::Esc => match state.step {
                AddModelStep::Endpoint => self.overlay = Overlay::None,
                AddModelStep::Models => {
                    state.step = AddModelStep::Endpoint;
                    state.filter.clear();
                    state.error = None;
                    state.field = 2;
                }
            },
            KeyCode::Tab | KeyCode::Up | KeyCode::Down if state.step == AddModelStep::Endpoint => {
                state.field = if key.code == KeyCode::Up {
                    state.field.saturating_sub(1)
                } else {
                    (state.field + 1).min(2)
                };
                state.cursor = match state.field {
                    0 => state.url.chars().count(),
                    1 => state.key.chars().count(),
                    _ => 0,
                };
            }
            KeyCode::Up => {
                state.sel = state.sel.saturating_sub(1);
            }
            KeyCode::Down => {
                state.sel = (state.sel + 1).min(state.filtered().len().saturating_sub(1));
            }
            KeyCode::Backspace => {
                if !state.edit(delete_before) && state.step == AddModelStep::Models {
                    state.filter.pop();
                    state.sel = 0;
                }
            }
            KeyCode::Char(c) => {
                if state.edit(|text, cursor| insert_at(text, cursor, c)) {
                    state.error = None;
                } else if state.step == AddModelStep::Models {
                    state.filter.push(c);
                    state.sel = 0;
                }
            }
            KeyCode::Enter => match state.step {
                AddModelStep::Endpoint => self.fetch_catalog(op_tx),
                AddModelStep::Models => self.save_added_model(),
            },
            _ => {}
        }
        Ok(())
    }

    /// Write the chosen model into `models.json`, reload the config and select
    /// it in the slot the picker was editing.
    fn save_added_model(&mut self) {
        let Overlay::AddModel(state) = &self.overlay else {
            return;
        };
        let chosen = state
            .filtered()
            .get(state.sel)
            .map(|model| (*model).clone());
        let manual = state.filter.trim().to_string();
        let model_id = chosen
            .as_ref()
            .map(|model| model.id.clone())
            .unwrap_or(manual);
        if model_id.is_empty() {
            self.add_model_error("pick a model or type its id".to_string());
            return;
        }
        let new = NewModel {
            base_url: state.url.clone(),
            api_key: state.key.clone(),
            model_id,
            context_window: chosen.as_ref().map_or(0, |model| model.context_window),
            max_tokens: chosen.as_ref().and_then(|model| model.max_tokens),
        };
        let target = state.target;
        let from_settings = state.from_settings;
        let reference = match self.agent.add_model(&new) {
            Ok(reference) => reference,
            Err(err) => {
                self.add_model_error(err.to_string());
                return;
            }
        };
        // Pick up the file we just wrote so the picker and `/session` see it.
        if let Ok(config) = Config::load_from_dir(&self.config.source_dir()) {
            self.config = config;
        }
        self.set_model(&reference, target);
        self.overlay = if from_settings {
            Overlay::Settings {
                page: SettingsPage::Compaction,
                sel: 0,
            }
        } else {
            Overlay::None
        };
        self.notice(format!("added {reference} to models.json"));
    }

    /// Show a failure inside the wizard, where the user is still looking.
    fn add_model_error(&mut self, message: String) {
        if let Overlay::AddModel(state) = &mut self.overlay {
            state.error = Some(message);
        }
    }

    fn open_model_picker(&mut self, target: ModelTarget, from_settings: bool) {
        let mut items: Vec<PickerItem> = self
            .config
            .model_catalog()
            .into_iter()
            .map(|spec| PickerItem {
                label: spec.reference(),
                detail: format!("{} | ctx {}", spec.name, human_count(spec.context_window)),
                value: spec.reference(),
                disabled: false,
            })
            .collect();
        if target == ModelTarget::Compact {
            items.insert(
                0,
                PickerItem {
                    label: "None (main model)".to_string(),
                    detail: "use the main model and its cached prompt".to_string(),
                    value: String::new(),
                    disabled: false,
                },
            );
        }
        // Always offered, including on a bare install: this is how a provider
        // gets into models.json in the first place.
        items.push(PickerItem {
            label: "Add model...".to_string(),
            detail: "save a new provider and model to models.json".to_string(),
            value: ADD_MODEL_ROW.to_string(),
            disabled: false,
        });
        let current = match target {
            ModelTarget::Agent => &self.agent_model,
            ModelTarget::Compact if self.compact_is_separate => &self.compact_model,
            ModelTarget::Compact => "",
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
            from_settings,
        };
    }

    pub(crate) fn settings_items(&self) -> Vec<PickerItem> {
        let page = match &self.overlay {
            Overlay::Settings { page, .. } => *page,
            _ => SettingsPage::Root,
        };
        self.settings_items_for(page)
    }

    /// Whether the model that writes summaries declares `reasoning`; a level
    /// set for a non-reasoning model is never sent.
    fn compaction_model_reasons(&self) -> bool {
        let model = if self.compact_is_separate {
            &self.compact_model
        } else {
            &self.agent_model
        };
        self.config
            .resolve_model(model)
            .map(|spec| spec.reasoning)
            .unwrap_or(false)
    }

    fn settings_items_for(&self, page: SettingsPage) -> Vec<PickerItem> {
        match page {
            SettingsPage::Root => {
                let notices = PickerItem {
                    label: "cache miss notices".to_string(),
                    detail: if self.agent.show_cache_miss_notices() {
                        "on - warn when a turn re-bills cached prompt tokens".to_string()
                    } else {
                        "off".to_string()
                    },
                    value: "notices".to_string(),
                    disabled: false,
                };
                let retry = PickerItem {
                    label: "429 retry".to_string(),
                    detail: format!(
                        "{} retries, {}s apart",
                        self.agent.rate_limit_retries(),
                        RATE_LIMIT_RETRY_INTERVAL.as_secs()
                    ),
                    value: "retry".to_string(),
                    disabled: false,
                };
                let search = PickerItem {
                    label: "web search".to_string(),
                    detail: format!(
                        "{} ({})",
                        self.search.backend.label(),
                        if self.search.ready() {
                            "ready"
                        } else {
                            "needs a credential"
                        }
                    ),
                    value: "search".to_string(),
                    disabled: false,
                };
                let compaction = PickerItem {
                    label: "compaction".to_string(),
                    detail: format!(
                        "model {} | {} thinking | {:.0}% threshold | keep {}",
                        if self.compact_is_separate {
                            rustedharness_core::config::model_display(&self.compact_model)
                                .to_string()
                        } else {
                            "None (main model)".to_string()
                        },
                        self.compaction.compact_thinking.label(),
                        self.compaction.compact_threshold * 100.0,
                        self.compaction.keep_recent_turns
                    ),
                    value: "compaction".to_string(),
                    disabled: false,
                };
                let image = PickerItem {
                    label: "image size".to_string(),
                    detail: format!(
                        "{} on the longest edge - bigger images are shrunk",
                        image_size_label(self.agent.config().image.max_edge)
                    ),
                    value: "image".to_string(),
                    disabled: false,
                };
                let mut items = vec![notices, retry];
                // Windows installed with Git for Windows is the only place with
                // a choice of shell, so the row is absent everywhere else.
                if rustedharness_core::shell::git_bash_available() {
                    items.push(PickerItem {
                        label: "terminal".to_string(),
                        detail: terminal_label(&self.agent.config().shell.prefer).to_string(),
                        value: "terminal".to_string(),
                        disabled: false,
                    });
                }
                items.extend([search, compaction, image]);
                items
            }
            SettingsPage::WebSearch => {
                let search = &self.search;
                let mut items: Vec<PickerItem> = SearchBackend::ALL
                    .into_iter()
                    .map(|backend| {
                        let status = search_status(search, backend);
                        PickerItem {
                            label: backend.label().to_string(),
                            detail: if backend == search.backend {
                                format!("selected | {status}")
                            } else {
                                status.to_string()
                            },
                            value: format!("backend:{}", backend.label()),
                            disabled: false,
                        }
                    })
                    .collect();
                // Only the selected backend's credential is editable: storing a
                // key for a backend that is not in use would be a mystery.
                let backend = search.backend;
                items.push(match backend.credential() {
                    None => PickerItem {
                        label: "credential".to_string(),
                        detail: format!("{} needs no credential", backend.label()),
                        value: "credential".to_string(),
                        disabled: true,
                    },
                    Some(spec) => PickerItem {
                        label: if spec.kind == CredentialKind::Url {
                            "instance url"
                        } else {
                            "api key"
                        }
                        .to_string(),
                        detail: credential_state(search, backend).to_string(),
                        value: "credential".to_string(),
                        disabled: false,
                    },
                });
                items
            }
            SettingsPage::Compaction => vec![
                PickerItem {
                    label: "model".to_string(),
                    detail: if self.compact_is_separate {
                        format!(
                            "{} (separate)",
                            rustedharness_core::config::model_display(&self.compact_model)
                        )
                    } else {
                        "None - reuse the main model's cached prompt".to_string()
                    },
                    value: "model".to_string(),
                    disabled: false,
                },
                PickerItem {
                    label: "thinking".to_string(),
                    detail: if self.compaction_model_reasons() {
                        format!(
                            "{} - reasoning effort for the summary request",
                            self.compaction.compact_thinking.label()
                        )
                    } else {
                        format!(
                            "{} - the compaction model does not declare reasoning",
                            self.compaction.compact_thinking.label()
                        )
                    },
                    value: "thinking".to_string(),
                    disabled: false,
                },
                PickerItem {
                    label: "threshold".to_string(),
                    detail: format!(
                        "{:.0}% of the context window",
                        self.compaction.compact_threshold * 100.0
                    ),
                    value: "threshold".to_string(),
                    disabled: false,
                },
                PickerItem {
                    label: "keep recent turns".to_string(),
                    detail: format!(
                        "{} turns kept after compaction",
                        self.compaction.keep_recent_turns
                    ),
                    value: "keep".to_string(),
                    disabled: false,
                },
                PickerItem {
                    label: "strip old context".to_string(),
                    detail: if self.compact_is_separate {
                        if self.compaction.compact_strip_context {
                            "on - older turns may be trimmed".to_string()
                        } else {
                            "off - send the complete transcript".to_string()
                        }
                    } else {
                        "disabled - the main model needs the complete history".to_string()
                    },
                    value: "strip".to_string(),
                    disabled: !self.compact_is_separate,
                },
                PickerItem {
                    label: "send recent turns".to_string(),
                    detail: format!(
                        "{} full transcript turns sent to the compactor",
                        self.compaction.send_recent_turns
                    ),
                    value: "send".to_string(),
                    disabled: !self.compact_is_separate || !self.compaction.compact_strip_context,
                },
                PickerItem {
                    label: "strip content".to_string(),
                    detail: self.compaction.strip_content.label().to_string(),
                    value: "content".to_string(),
                    disabled: !self.compact_is_separate || !self.compaction.compact_strip_context,
                },
            ],
        }
    }

    /// Long description of the selected setting, shown under the submenu.
    pub(crate) fn settings_description(&self) -> String {
        let page = match &self.overlay {
            Overlay::Settings { page, .. } => *page,
            _ => SettingsPage::Root,
        };
        if page == SettingsPage::Root {
            let sel = match &self.overlay {
                Overlay::Settings { sel, .. } => *sel,
                _ => 0,
            };
            let value = self
                .settings_items_for(page)
                .get(sel)
                .map(|item| item.value.clone())
                .unwrap_or_default();
            return match value.as_str() {
                "notices" => "Cache miss notices appear in the transcript when a turn re-bills prompt tokens the previous turn had already cached. Only significant misses are reported, and only when the provider reports cached tokens at all.".to_string(),
                "retry" => format!(
                    "How many times a turn retries after a 429/rate-limit response before giving up. Enter cycles {}. The wait between attempts is fixed at {} seconds; quota or credit exhaustion is never retried.",
                    retry_presets_text(),
                    RATE_LIMIT_RETRY_INTERVAL.as_secs()
                ),
                "image" => "How many pixels the longest edge of an attached image may have. A bigger image is downscaled to fit before it is uploaded, so a screenshot never gets refused for being too big; nothing is cropped and the aspect ratio is kept.".to_string(),
                "terminal" => "Which shell the agent's shell tool spawns. Git Bash is the bash that ships with Git for Windows, so commands, quoting and paths read like a unix shell; PowerShell is the native Windows shell. Switching applies at once and stops the background processes the previous shell started.".to_string(),
                "search" => "Which backend the web_search tool uses. Enter opens the web search page: brave, tavily, SearXNG, bing or duckduckgo. The page asks for an api key (or a SearXNG instance url) for the backend that needs one.".to_string(),
                _ => "Compaction keeps long sessions inside the context window. Enter opens the compaction options.".to_string(),
            };
        }
        let sel = match &self.overlay {
            Overlay::Settings { sel, .. } => *sel,
            _ => 0,
        };
        let value = self
            .settings_items_for(page)
            .get(sel)
            .map(|item| item.value.clone())
            .unwrap_or_default();
        if page == SettingsPage::WebSearch {
            return match value.as_str() {
                "credential" => "The credential this backend needs. Enter opens a text box; an empty value clears the stored one. Keys are kept in config.json in plain text, and BRAVE_API_KEY/TAVILY_API_KEY/SEARXNG_URL are used when the file says nothing.".to_string(),
                other if other.starts_with("backend:") => "Enter selects this backend for every web_search call. brave and tavily need an api key, searxng needs the base url of an instance with the JSON API enabled, bing scrapes the public results page until an api key upgrades it to the official API, and duckduckgo needs no account.".to_string(),
                _ => String::new(),
            };
        }
        match value.as_str() {
            "model" => "None reuses the main model. The summary request then sends the same system prompt, tools and old messages as the live turn, so the provider can serve the whole prefix from prompt cache. A separate model starts with a cold cache; it is useful only when its lower price outweighs the uncached input.".to_string(),
            "thinking" => "Reasoning effort requested for the summary request, for both the main model and a separate compaction model. Enter cycles off, low, medium, high, xhigh and max. Off sends no effort field, so the provider default applies. The value is only sent when the compaction model declares reasoning and its provider supports reasoning effort.".to_string(),
            "threshold" => "Compaction starts when the provider-reported context reaches this share of the model's context window. Use Left/Right to change it by 5%. Lower values compact earlier and keep more headroom; higher values spend more context before summarising.".to_string(),
            "keep" => "After compaction this many recent user turns stay verbatim below the summary. They are the model's immediate working context. Use Left/Right to change the count.".to_string(),
            "strip" if self.compact_is_separate => "Turn trimming on for the separate model. When enabled, the two options below decide how many recent turns are sent in full and what is removed from older turns. When disabled, the complete transcript is sent.".to_string(),
            "strip" => "Disabled while the compaction model is None. Main-model compaction must send the complete live history so the provider prompt cache hits; removing old tool output or thinking would invalidate that prefix.".to_string(),
            "send" if self.compact_is_separate && self.compaction.compact_strip_context => "How many recent user turns of the summarised transcript are sent to the separate model in full, including tool output and thinking. Older turns keep their user/assistant prose but lose the content selected by strip content. Use Left/Right to change the count.".to_string(),
            "send" => "Disabled until strip old context is on. While trimming is off, the summariser receives the complete transcript.".to_string(),
            "content" if self.compact_is_separate && self.compaction.compact_strip_context => "What to remove from turns older than send recent turns: none, tool output, or tool output and thinking. Enter cycles the value. Recent turns and user/assistant prose are always retained.".to_string(),
            "content" => "Disabled until strip old context is on. While trimming is off, the summariser receives the complete transcript.".to_string(),
            _ => String::new(),
        }
    }

    fn adjust_setting(&mut self, up: bool) {
        let page = match &self.overlay {
            Overlay::Settings { page, .. } => *page,
            _ => return,
        };
        if page != SettingsPage::Compaction {
            return;
        }
        let sel = match &self.overlay {
            Overlay::Settings { sel, .. } => *sel,
            _ => return,
        };
        let Some(item) = self.settings_items().get(sel).cloned() else {
            return;
        };
        if item.disabled {
            return;
        }
        let result = match item.value.as_str() {
            "threshold" => {
                let next = self.compaction.compact_threshold + if up { 0.05 } else { -0.05 };
                self.agent.set_compact_threshold(next)
            }
            "keep" => {
                let next = if up {
                    self.compaction.keep_recent_turns + 1
                } else {
                    self.compaction.keep_recent_turns.saturating_sub(1).max(1)
                };
                self.agent.set_keep_recent_turns(next)
            }
            "send" => {
                let next = if up {
                    self.compaction.send_recent_turns + 1
                } else {
                    self.compaction.send_recent_turns.saturating_sub(1).max(1)
                };
                self.agent.set_send_recent_turns(next)
            }
            _ => return,
        };
        match result {
            Ok(()) => {
                self.compaction = self.agent.compaction_settings();
                self.notice(format!(
                    "compaction: {:.0}% threshold, keep {} turns",
                    self.compaction.compact_threshold * 100.0,
                    self.compaction.keep_recent_turns
                ));
            }
            Err(err) => self.push(EntryKind::Error, "", err.to_string()),
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
                disabled: false,
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

    /// Return to the directory listing that contains `rel`, with the cursor on
    /// it. Esc uses this, and so does anything that cannot show the file.
    fn back_to_browser(&mut self, rel: &str) {
        let (dir, name) = match rel.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), Some(name.to_string())),
            None => (String::new(), Some(rel.to_string())),
        };
        self.open_file_browser_at(&dir, name.as_deref());
    }

    /// List the user inputs, newest first: the last one is the usual target.
    fn open_rollback(&mut self) {
        if self.busy {
            self.notice("cancel the running turn before rolling back");
            return;
        }
        let rows: Vec<TurnRow> = self
            .agent
            .user_turns()
            .into_iter()
            .rev()
            .map(|turn| TurnRow {
                label: format!("#{} {}", turn.index + 1, first_line(&turn.text)),
                text: turn.text,
                after: turn.after,
                turn: turn.index,
            })
            .collect();
        if rows.is_empty() {
            self.notice("nothing to roll back to: no user input in this session");
            return;
        }
        self.overlay = Overlay::Rollback { rows, sel: 0 };
    }

    pub(crate) fn open_file_browser(&mut self, rel: &str) {
        self.open_file_browser_at(rel, None);
    }

    /// Open the list at `rel` with the cursor on `focus`, so leaving a file
    /// lands where the user was instead of at the top of the directory.
    fn open_file_browser_at(&mut self, rel: &str, focus: Option<&str>) {
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
        let focus = focus
            .and_then(|name| entries.iter().position(|(entry, _)| entry == name))
            .unwrap_or(if rel.is_empty() { 0 } else { 1 });
        self.overlay = Overlay::FileBrowser {
            sel: focus,
            rel,
            entries,
        };
    }

    fn open_file(&mut self, rel: &str, op_tx: &UnboundedSender<OpResult>) {
        let path = self
            .agent
            .cwd()
            .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        match std::fs::read(&path) {
            Ok(bytes) => {
                if bytes.iter().take(8_192).any(|b| *b == 0) {
                    self.notice(format!("{rel}: binary file, not shown"));
                    self.back_to_browser(rel);
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
                // Show the file immediately, in plain text; highlighting runs
                // on a worker thread because syntect needs about a second for
                // a thousand lines in a debug build, and a frozen UI on `/view`
                // reads as a hang.
                self.overlay = Overlay::FileView {
                    path: rel.to_string(),
                    lines: crate::markdown::plain(&text),
                    scroll: 0,
                    truncated,
                };
                if text.len() <= HIGHLIGHT_LIMIT {
                    let tx = op_tx.clone();
                    let path = rel.to_string();
                    let language = crate::markdown::language_for(rel);
                    tokio::task::spawn_blocking(move || {
                        let lines = crate::markdown::highlight(&text, language);
                        let _ = tx.send(OpResult::Highlighted { path, lines });
                    });
                }
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
    rustedharness_core::paths::SKIP_DIRS.contains(&name)
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

/// Picker row that opens the add-model wizard. The value cannot collide with a
/// real model reference because a reference always contains a `/`.
const ADD_MODEL_ROW: &str = "+";

/// The shell the agent will spawn, as the settings row reads it.
/// Longest-edge presets, in pixels: a thumbnail, a legible screenshot, the
/// recommendation most providers publish, and a full-resolution one.
const IMAGE_EDGE_PRESETS: [u32; 4] = [512, 1024, 1568, 2048];

fn image_size_label(pixels: u32) -> String {
    format!("{pixels} px")
}

fn next_image_size(max_edge: u32) -> u32 {
    let position = IMAGE_EDGE_PRESETS
        .iter()
        .position(|preset| *preset >= max_edge)
        .unwrap_or(0);
    IMAGE_EDGE_PRESETS[(position + 1) % IMAGE_EDGE_PRESETS.len()]
}

fn terminal_label(prefer: &str) -> &'static str {
    if prefer.eq_ignore_ascii_case(GIT_BASH) {
        "git bash"
    } else {
        "powershell"
    }
}

/// The other shell, as it is stored in `shell.prefer`.
fn next_terminal_prefer(prefer: &str) -> &'static str {
    if prefer.eq_ignore_ascii_case(GIT_BASH) {
        POWERSHELL
    } else {
        GIT_BASH
    }
}

/// First line of a multi-line input, for one-line picker rows.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// Row of the credential editor on the web-search page: always the last one,
/// below the five backends.
const CREDENTIAL_ROW: usize = SearchBackend::ALL.len();

/// One line saying whether `backend` can search with the current config.
fn search_status(search: &SearchConfig, backend: SearchBackend) -> &'static str {
    match search.credential_source(backend) {
        Some(CredentialSource::Config) => "credential saved",
        Some(CredentialSource::Environment) => "credential from the environment",
        None => match backend.credential().map(|spec| spec.kind) {
            None => "no account needed",
            Some(CredentialKind::Url) => "needs an instance url",
            Some(_) => "needs an api key",
        },
    }
}

/// What the credential row shows for the selected backend.
fn credential_state(search: &SearchConfig, backend: SearchBackend) -> &'static str {
    match search.credential_source(backend) {
        Some(CredentialSource::Config) => "saved",
        Some(CredentialSource::Environment) => "from the environment",
        None if backend
            .credential()
            .is_some_and(|spec| !spec.kind.required()) =>
        {
            "not set - the public website is scraped instead"
        }
        None => "not set - press enter to type it",
    }
}

/// Insert one character at the caret of a single-line field.
fn insert_at(text: &mut String, cursor: &mut usize, c: char) {
    let byte = text
        .char_indices()
        .nth(*cursor)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.insert(byte, c);
    *cursor += 1;
}

/// Delete the character before the caret.
fn delete_before(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let start = text
        .char_indices()
        .nth(*cursor - 1)
        .map(|(index, _)| index)
        .unwrap_or(0);
    text.remove(start);
    *cursor -= 1;
}

/// Retry counts offered by `/settings`. A config value that is not one of them
/// moves up to the next preset, wrapping at the top.
const RETRY_PRESETS: [u32; 5] = [5, 15, 30, 50, 100];

/// `5, 15, 30, 50, 100`, for the settings help text.
fn retry_presets_text() -> String {
    RETRY_PRESETS
        .iter()
        .map(|preset| preset.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn next_retry_preset(current: u32) -> u32 {
    RETRY_PRESETS
        .into_iter()
        .find(|preset| *preset > current)
        .unwrap_or(RETRY_PRESETS[0])
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustedharness_core::config::ThinkingLevel;

    /// Everything language-server related is a `/lsp` subcommand.
    #[test]
    fn lsp_commands_live_under_lsp() {
        for (command, _) in COMMANDS {
            assert_ne!(*command, "/install", "use /lsp install");
        }
        for sub in ["/lsp install <id>", "/lsp update <id>"] {
            assert!(COMMANDS.iter().any(|(command, _)| *command == sub), "{sub}");
        }
    }

    fn status(language: &str, server: &str, state: &str) -> rustedharness_core::lsp::ServerStatus {
        rustedharness_core::lsp::ServerStatus {
            language: language.into(),
            server: server.into(),
            version: None,
            state: state.into(),
            detail: None,
            pid: None,
            restarts: 0,
            install_hint: String::new(),
            runtime_deps: Vec::new(),
        }
    }

    #[test]
    fn the_status_line_summarises_live_servers() {
        let mut app = crate::ui_tests::demo_app();

        app.lsp.clear();
        assert!(app.lsp_summary().is_none(), "no servers, no segment");

        app.lsp = vec![status("rust", "rust-analyzer", "running")];
        assert_eq!(app.lsp_summary(), Some(("lsp rust".to_string(), false)));

        // Warming is the transient state worth colouring.
        app.lsp = vec![status("lua", "lua-language-server", "warming")];
        assert_eq!(app.lsp_summary(), Some(("lsp lua warm".to_string(), true)));

        // A server that is down is what needs attention, plus the count of the
        // ones hidden behind it.
        app.lsp = vec![
            status("rust", "rust-analyzer", "running"),
            status("lua", "lua-language-server", "warming"),
            status("c", "clangd", "degraded"),
        ];
        assert_eq!(
            app.lsp_summary(),
            Some(("lsp c down +2".to_string(), false))
        );

        // Absent servers are not "running" and stay out of the status line.
        app.lsp = vec![status("go", "gopls", "absent")];
        assert!(app.lsp_summary().is_none());
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn the_removed_control_keys_do_nothing() {
        let mut app = crate::ui_tests::demo_app();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for key in [ctrl('l'), ctrl('r'), ctrl('x'), ctrl('g')] {
            runtime.block_on(app.handle_key(key, &tx)).unwrap();
            assert!(
                matches!(app.overlay, Overlay::None),
                "{:?} still opens an overlay",
                key.code
            );
        }
        // ctrl+f is the one that stayed.
        runtime.block_on(app.handle_key(ctrl('f'), &tx)).unwrap();
        assert!(matches!(app.overlay, Overlay::FileBrowser { .. }));
    }

    /// Esc leaves the viewer for the list it came from, and only the second Esc
    /// leaves the list.
    #[test]
    fn esc_walks_back_out_of_the_file_viewer() {
        let mut app = crate::ui_tests::demo_app();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        app.overlay = Overlay::FileView {
            path: "Cargo.toml".into(),
            lines: vec![Line::from("x")],
            scroll: 0,
            truncated: false,
        };
        runtime
            .block_on(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx))
            .unwrap();
        let Overlay::FileBrowser { rel, entries, sel } = &app.overlay else {
            panic!("esc from the viewer must land in the browser");
        };
        assert_eq!(rel, "", "the file sits in the workspace root");
        assert_eq!(
            entries.get(*sel).map(|(name, _)| name.as_str()),
            Some("Cargo.toml"),
            "and the cursor is on it"
        );

        runtime
            .block_on(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx))
            .unwrap();
        assert!(matches!(app.overlay, Overlay::None), "second esc leaves");
    }

    /// A download in flight shows on the status line instead of writing a
    /// notice per chunk, and it disappears when the install ends - including
    /// when it fails without a final status.
    #[test]
    fn a_download_shows_its_progress_on_the_status_line() {
        let mut app = crate::ui_tests::demo_app();
        app.lsp.clear();
        let entries = app.entries.len();
        app.on_harness_event(HarnessEvent::DownloadStatus {
            server_id: "clangd".into(),
            state: "downloading".into(),
            detail: Some("42% 12.3/29.4 MB".into()),
            percent: Some(42),
        });
        assert_eq!(
            app.lsp_summary(),
            Some(("lsp clangd 42% 12.3/29.4 MB".to_string(), true))
        );
        assert_eq!(
            app.entries.len(),
            entries,
            "progress is not a transcript entry"
        );

        app.on_harness_event(HarnessEvent::DownloadStatus {
            server_id: "clangd".into(),
            state: "installed".into(),
            detail: None,
            percent: None,
        });
        assert!(app.download.is_none(), "installed clears the progress");
        assert_eq!(app.lsp_summary(), None);

        // A failure reports through LspStatus only; the bar must go away.
        app.on_harness_event(HarnessEvent::DownloadStatus {
            server_id: "clangd".into(),
            state: "downloading".into(),
            detail: Some("7% 2.0/29.4 MB".into()),
            percent: Some(7),
        });
        app.on_harness_event(HarnessEvent::LspStatus {
            language: "c".into(),
            state: "absent".into(),
            detail: Some("request failed: operation timed out".into()),
        });
        assert!(app.download.is_none(), "a failed download clears it too");
    }

    #[test]
    fn lsp_id_query_splits_verb_and_partial() {
        assert_eq!(
            lsp_id_query("/lsp install ru"),
            Some(("/lsp install ", "ru"))
        );
        assert_eq!(lsp_id_query("/lsp update "), Some(("/lsp update ", "")));
        assert_eq!(lsp_id_query("/lsp install"), Some(("/lsp install ", "")));
        for text in ["/lsp", "/lsp bogus x", "/model ru", "/install ru"] {
            assert_eq!(lsp_id_query(text), None, "{text}");
        }
    }

    /// Typing after the verb offers server ids instead of command names.
    #[test]
    fn server_ids_complete_after_the_verb() {
        let mut app = crate::ui_tests::demo_app();
        app.input = "/lsp install rust".chars().collect();
        app.cursor = app.input.len();
        app.refresh_slash();
        let Overlay::Slash { items, prefix, .. } = &app.overlay else {
            panic!("expected the completion menu");
        };
        assert_eq!(prefix, "/lsp install ");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "rust-analyzer");
    }

    #[test]
    fn the_verb_alone_lists_every_server() {
        let mut app = crate::ui_tests::demo_app();
        app.input = "/lsp update".chars().collect();
        app.cursor = app.input.len();
        app.refresh_slash();
        let Overlay::Slash { items, prefix, .. } = &app.overlay else {
            panic!("expected the completion menu");
        };
        assert_eq!(prefix, "/lsp update ");
        assert_eq!(items.len(), rustedharness_core::lsp::registry().len());
    }

    #[test]
    fn confirming_an_id_fills_the_command_line() {
        let mut app = crate::ui_tests::demo_app();
        app.input = "/lsp install lua".chars().collect();
        app.cursor = app.input.len();
        app.refresh_slash();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(app.overlay_confirm(&tx))
            .unwrap();
        assert_eq!(
            app.input.iter().collect::<String>(),
            "/lsp install lua-language-server"
        );
        assert!(matches!(app.overlay, Overlay::None));
        assert!(
            app.slash_dismissed,
            "the menu stays closed until the next key"
        );
    }

    #[test]
    fn ids_do_not_leak_into_other_commands() {
        let mut app = crate::ui_tests::demo_app();
        app.input = "/model ru".chars().collect();
        app.cursor = app.input.len();
        app.refresh_slash();
        assert!(
            matches!(app.overlay, Overlay::None),
            "no id completion for /model"
        );
    }

    #[test]
    fn the_menu_carries_no_redundant_entries() {
        for (command, _) in COMMANDS {
            assert_ne!(*command, "/model <name>", "the picker covers this");
            assert_ne!(*command, "/clear", "/clear was removed");
            assert_ne!(*command, "/todo", "ctrl+t shows the todo list");
            assert_ne!(
                *command, "/compact-model",
                "the settings panel owns the compaction model"
            );
        }
        // The settings panel is back and drives compaction.
        assert!(COMMANDS.iter().any(|(command, _)| *command == "/settings"));
    }

    /// ctrl+t owns the todo panel; `/settings` opens a root menu whose
    /// compaction submenu edits the live agent and persists the change.
    #[test]
    fn ctrl_t_toggles_todos_and_settings_edits_compaction() {
        let mut app = crate::ui_tests::demo_app();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(app.handle_key(ctrl('t'), &tx)).unwrap();
        assert!(matches!(app.overlay, Overlay::Todos));
        runtime.block_on(app.handle_key(ctrl('t'), &tx)).unwrap();
        assert!(matches!(app.overlay, Overlay::None), "ctrl+t collapses");

        // The root page lists notices, 429 retries, web search, then compaction
        // (plus a terminal row on Windows), so rows are found by name.
        app.overlay = Overlay::Settings {
            page: SettingsPage::Root,
            sel: crate::ui_tests::settings_row(&app, "compaction"),
        };
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert!(matches!(
            app.overlay,
            Overlay::Settings {
                page: SettingsPage::Compaction,
                ..
            }
        ));
        app.overlay = Overlay::Settings {
            page: SettingsPage::Compaction,
            sel: 2,
        };
        let threshold = app.compaction.compact_threshold;
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert!(app.compaction.compact_threshold > threshold);
        assert!(matches!(
            app.overlay,
            Overlay::Settings {
                page: SettingsPage::Compaction,
                ..
            }
        ));
        assert_eq!(app.compaction, app.agent.compaction_settings());

        app.set_model("None", ModelTarget::Compact);
        assert!(!app.compact_is_separate);
        assert_eq!(app.compact_model, app.agent_model);
        assert_eq!(app.agent.compact_model(), app.agent_model);
        app.overlay = Overlay::Settings {
            page: SettingsPage::Compaction,
            sel: 4,
        };
        for index in 4..=6 {
            assert!(app.settings_items()[index].disabled, "item {index}");
        }
        assert!(app.settings_description().contains("complete live history"));
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert!(
            app.compaction.compact_strip_context,
            "the disabled item did nothing"
        );

        app.compact_is_separate = true;
        app.compaction.compact_strip_context = false;
        app.overlay = Overlay::Settings {
            page: SettingsPage::Compaction,
            sel: 5,
        };
        assert!(!app.settings_items()[4].disabled);
        assert!(app.settings_items()[5].disabled);
        assert!(app.settings_items()[6].disabled);
        let send = app.compaction.send_recent_turns;
        app.adjust_setting(true);
        assert_eq!(
            app.compaction.send_recent_turns, send,
            "left/right cannot change a disabled child"
        );

        app.compaction.compact_strip_context = true;
        app.overlay = Overlay::Settings {
            page: SettingsPage::Compaction,
            sel: 6,
        };
        let content = app.compaction.strip_content;
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert_ne!(app.compaction.strip_content, content);
    }

    /// `thinking` is a compaction option like `strip content`: Enter cycles the
    /// level, the agent keeps it, and both compaction paths read it.
    #[test]
    fn settings_cycle_the_compaction_thinking_level() {
        let mut app = crate::ui_tests::demo_app();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let index = app
            .settings_items_for(SettingsPage::Compaction)
            .iter()
            .position(|item| item.value == "thinking")
            .expect("thinking row");
        app.overlay = Overlay::Settings {
            page: SettingsPage::Compaction,
            sel: index,
        };
        assert!(
            !app.settings_items()[index].disabled,
            "both paths honour it"
        );

        for expected in [
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
            ThinkingLevel::Off,
        ] {
            runtime.block_on(app.overlay_confirm(&tx)).unwrap();
            assert_eq!(app.compaction.compact_thinking, expected);
            assert_eq!(app.agent.compaction_settings().compact_thinking, expected);
            assert!(matches!(
                app.overlay,
                Overlay::Settings {
                    page: SettingsPage::Compaction,
                    ..
                }
            ));
        }
        assert!(app.settings_description().contains("off, low, medium"));
    }

    /// The 429 retry row cycles the presets the user asked for and persists the
    /// count; the wait between attempts stays a constant.
    #[test]
    fn settings_cycle_the_429_retry_count() {
        let mut app = crate::ui_tests::demo_app();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let retry = crate::ui_tests::settings_row(&app, "retry");
        app.overlay = Overlay::Settings {
            page: SettingsPage::Root,
            sel: retry,
        };

        let start = app.agent.rate_limit_retries();
        let mut seen = vec![start];
        for _ in 0..RETRY_PRESETS.len() {
            runtime.block_on(app.overlay_confirm(&tx)).unwrap();
            seen.push(app.agent.rate_limit_retries());
        }
        assert_eq!(
            seen,
            vec![start, 15, 30, 50, 100, 5],
            "5, 15, 30, 50, 100 and back to 5"
        );
        assert!(app.settings_items()[retry].detail.contains("3s apart"));
        assert!(app.settings_description().contains("fixed at 3 seconds"));
    }

    /// The terminal row exists exactly where the agent has a choice of shell,
    /// and it cycles between the two values `ShellSpec::detect` understands.
    #[test]
    fn the_terminal_row_cycles_between_the_two_shells() {
        assert_eq!(next_terminal_prefer(POWERSHELL), GIT_BASH);
        assert_eq!(next_terminal_prefer(GIT_BASH), POWERSHELL);
        assert_eq!(next_terminal_prefer("default"), GIT_BASH);
        assert_eq!(terminal_label(GIT_BASH), "git bash");
        assert_eq!(terminal_label(POWERSHELL), "powershell");
        assert_eq!(terminal_label("default"), "powershell");

        let app = crate::ui_tests::demo_app();
        let row = app
            .settings_items()
            .into_iter()
            .find(|item| item.value == "terminal");
        assert_eq!(
            row.is_some(),
            rustedharness_core::shell::git_bash_available(),
            "the row is shown only where a second shell exists"
        );
        if let Some(row) = row {
            assert_eq!(row.label, "terminal");
            assert_eq!(row.detail, "powershell", "the default preference");
        }
    }

    /// Choosing the other shell writes `shell.prefer` and rebuilds the agent's
    /// shell tool. Only Windows has the row, so this runs for real there.
    #[test]
    fn the_terminal_row_switches_the_agent_shell() {
        if !rustedharness_core::shell::git_bash_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut app = crate::ui_tests::demo_app_in(dir.path());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        app.overlay = Overlay::Settings {
            page: SettingsPage::Root,
            sel: crate::ui_tests::settings_row(&app, "terminal"),
        };

        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert_eq!(app.agent.config().shell.prefer, GIT_BASH);
        let row = crate::ui_tests::settings_row(&app, "terminal");
        assert_eq!(app.settings_items()[row].detail, "git bash");

        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert_eq!(app.agent.config().shell.prefer, POWERSHELL);
        assert_eq!(
            app.settings_items()[crate::ui_tests::settings_row(&app, "terminal")].detail,
            "powershell"
        );
    }

    /// The image row is always present and walks the pixel ladder, writing the
    /// chosen ceiling to `config.json` so the next `read` uses it.
    #[test]
    fn the_image_size_row_cycles_through_the_presets() {
        assert_eq!(image_size_label(1568), "1568 px");
        assert_eq!(next_image_size(1568), 2048);
        assert_eq!(next_image_size(2048), 512);
        assert_eq!(next_image_size(512), 1024);
        // A value from the config file that is not a preset rounds up first.
        assert_eq!(next_image_size(1200), 2048);

        let dir = tempfile::tempdir().unwrap();
        let mut app = crate::ui_tests::demo_app_in(dir.path());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let row = crate::ui_tests::settings_row(&app, "image");
        assert!(app.settings_items()[row].detail.contains("1568 px"));
        app.overlay = Overlay::Settings {
            page: SettingsPage::Root,
            sel: row,
        };

        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert_eq!(app.agent.config().image.max_edge, 2048);
        let row = crate::ui_tests::settings_row(&app, "image");
        assert!(app.settings_items()[row].detail.contains("2048 px"));
        let stored = std::fs::read_to_string(dir.path().join("config.json")).unwrap();
        assert!(stored.contains("2048"), "{stored}");
    }

    /// The web-search page selects a backend and offers a credential box only
    /// for the backend that is in use.
    #[test]
    fn settings_pick_the_web_search_backend_and_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = crate::ui_tests::demo_app_in(dir.path());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let search = crate::ui_tests::settings_row(&app, "search");
        app.overlay = Overlay::Settings {
            page: SettingsPage::Root,
            sel: search,
        };
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert!(matches!(
            app.overlay,
            Overlay::Settings {
                page: SettingsPage::WebSearch,
                sel: 0
            }
        ));

        let labels: Vec<String> = app
            .settings_items()
            .iter()
            .map(|item| item.label.clone())
            .collect();
        assert_eq!(
            labels,
            [
                "brave",
                "tavily",
                "searxng",
                "bing",
                "duckduckgo",
                "api key"
            ],
            "the five backends, then the bing key box (the default backend)"
        );

        // Brave is the first row: selecting it switches the backend and asks
        // for a key.
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert_eq!(app.search.backend, SearchBackend::Brave);
        assert_eq!(app.agent.search_backend(), SearchBackend::Brave);
        let items = app.settings_items();
        assert_eq!(items[5].label, "api key");
        assert!(items[5].detail.contains("not set"), "{}", items[5].detail);

        // Enter on the credential row opens the text box; the key is stored and
        // the page comes back.
        app.overlay = Overlay::Settings {
            page: SettingsPage::WebSearch,
            sel: 5,
        };
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert!(matches!(
            app.overlay,
            Overlay::WebSearchKey {
                backend: SearchBackend::Brave,
                ..
            }
        ));
        for c in "brave-key".chars() {
            app.web_search_key(KeyEvent::from(KeyCode::Char(c)))
                .unwrap();
        }
        app.web_search_key(KeyEvent::from(KeyCode::Enter)).unwrap();
        assert!(matches!(
            app.overlay,
            Overlay::Settings {
                page: SettingsPage::WebSearch,
                ..
            }
        ));
        assert_eq!(
            app.search.credential(SearchBackend::Brave).as_deref(),
            Some("brave-key")
        );
        assert!(app.search.ready());
        assert!(app.settings_items()[5].detail.contains("saved"));

        // DuckDuckGo needs nothing, so the credential row is inert.
        app.overlay = Overlay::Settings {
            page: SettingsPage::WebSearch,
            sel: 4,
        };
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert_eq!(app.search.backend, SearchBackend::DuckDuckGo);
        assert!(app.settings_items()[5].disabled);
        assert!(app.settings_items()[5].detail.contains("no credential"));

        // SearXNG asks for an instance url rather than a key.
        app.overlay = Overlay::Settings {
            page: SettingsPage::WebSearch,
            sel: 2,
        };
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert_eq!(app.search.backend, SearchBackend::SearXng);
        assert_eq!(app.settings_items()[5].label, "instance url");
        assert!(!app.search.ready(), "no instance url yet");

        // Everything landed in config.json.
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("config.json")).unwrap())
                .unwrap();
        assert_eq!(saved["search"]["backend"], "searxng");
        assert_eq!(saved["search"]["braveApiKey"], "brave-key");
    }

    /// The root settings page toggles cache-miss notices and persists it.
    #[test]
    fn settings_toggle_cache_miss_notices() {
        let mut app = crate::ui_tests::demo_app();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let before = app.agent.show_cache_miss_notices();
        app.overlay = Overlay::Settings {
            page: SettingsPage::Root,
            sel: 0,
        };
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert_ne!(app.agent.show_cache_miss_notices(), before);
        assert!(matches!(
            app.overlay,
            Overlay::Settings {
                page: SettingsPage::Root,
                ..
            }
        ));
        assert!(app.settings_description().contains("Cache miss notices"));
    }

    /// `/model` ends with an "Add model..." row that opens the full-screen
    /// wizard; saving writes models.json and selects the new model.
    #[test]
    fn the_add_model_wizard_saves_a_listed_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = crate::ui_tests::demo_app_in(dir.path());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        app.open_model_picker(ModelTarget::Agent, false);
        if let Overlay::Model { items, sel, .. } = &mut app.overlay {
            assert_eq!(items.last().unwrap().value, ADD_MODEL_ROW);
            *sel = items.len() - 1;
        } else {
            panic!("model picker did not open");
        }
        runtime.block_on(app.overlay_confirm(&tx)).unwrap();
        assert!(matches!(
            &app.overlay,
            Overlay::AddModel(AddModelState {
                step: AddModelStep::Endpoint,
                field: 0,
                ..
            })
        ));

        for c in "https://api.example.com/v1".chars() {
            runtime
                .block_on(app.add_model_key(KeyEvent::from(KeyCode::Char(c)), &tx))
                .unwrap();
        }
        runtime
            .block_on(app.add_model_key(KeyEvent::from(KeyCode::Tab), &tx))
            .unwrap();
        for c in "sk-test".chars() {
            runtime
                .block_on(app.add_model_key(KeyEvent::from(KeyCode::Char(c)), &tx))
                .unwrap();
        }

        // The fetch itself is a network call; feed its answer in directly.
        app.apply_catalog(
            "https://api.example.com/v1",
            Ok(vec![
                CatalogModel {
                    id: "m1".into(),
                    name: Some("Model One".into()),
                    context_window: 128_000,
                    max_tokens: Some(8_192),
                },
                CatalogModel {
                    id: "m2".into(),
                    name: None,
                    context_window: 0,
                    max_tokens: None,
                },
            ]),
        );
        assert!(matches!(
            &app.overlay,
            Overlay::AddModel(AddModelState {
                step: AddModelStep::Models,
                ..
            })
        ));

        runtime
            .block_on(app.add_model_key(KeyEvent::from(KeyCode::Enter), &tx))
            .unwrap();
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.agent_model, "example/m1");

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("models.json")).unwrap())
                .unwrap();
        let provider = &saved["providers"]["example"];
        assert_eq!(provider["baseUrl"], "https://api.example.com/v1");
        assert_eq!(provider["apiKey"], "sk-test");
        assert_eq!(provider["models"][0]["id"], "m1");
        assert_eq!(provider["models"][0]["contextWindow"], 128_000);
        assert_eq!(provider["models"][0]["maxTokens"], 8_192);
        // The picker sees the new provider now that the config was reloaded.
        assert!(app
            .config
            .model_catalog()
            .iter()
            .any(|spec| spec.reference() == "example/m1"));
    }

    /// Endpoints without a `/models` list still work: the typed text is used as
    /// the model id, and Esc walks back step by step.
    #[test]
    fn the_add_model_wizard_accepts_a_typed_id_and_steps_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = crate::ui_tests::demo_app_in(dir.path());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        app.overlay = Overlay::AddModel(AddModelState::new(ModelTarget::Compact, false));
        if let Overlay::AddModel(state) = &mut app.overlay {
            state.url = "http://127.0.0.1:11434/v1".into();
        }
        app.apply_catalog("http://127.0.0.1:11434/v1", Ok(Vec::new()));
        assert!(matches!(
            &app.overlay,
            Overlay::AddModel(AddModelState {
                step: AddModelStep::Models,
                error: Some(_),
                ..
            })
        ));

        // Esc goes back to the endpoint instead of throwing the flow away.
        runtime
            .block_on(app.add_model_key(KeyEvent::from(KeyCode::Esc), &tx))
            .unwrap();
        assert!(matches!(
            &app.overlay,
            Overlay::AddModel(AddModelState {
                step: AddModelStep::Endpoint,
                ..
            })
        ));
        app.apply_catalog("http://127.0.0.1:11434/v1", Ok(Vec::new()));

        for c in "qwen2.5-coder:7b".chars() {
            runtime
                .block_on(app.add_model_key(KeyEvent::from(KeyCode::Char(c)), &tx))
                .unwrap();
        }
        runtime
            .block_on(app.add_model_key(KeyEvent::from(KeyCode::Enter), &tx))
            .unwrap();
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.compact_model, "127-0-0-1-11434/qwen2.5-coder:7b");
        assert!(app.compact_is_separate);

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("models.json")).unwrap())
                .unwrap();
        let models = saved["providers"]["127-0-0-1-11434"]["models"]
            .as_array()
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "qwen2.5-coder:7b");
        assert!(models[0].get("contextWindow").is_none(), "unknown context");

        // Esc from the endpoint step closes the wizard.
        app.overlay = Overlay::AddModel(AddModelState::new(ModelTarget::Agent, false));
        runtime
            .block_on(app.add_model_key(KeyEvent::from(KeyCode::Esc), &tx))
            .unwrap();
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn slash_completion_sees_the_lsp_subcommands() {
        // The picker filters on the first token, so every `/lsp ...` entry
        // appears once the user types `/lsp`.
        let shown: Vec<&str> = COMMANDS
            .iter()
            .map(|(command, _)| *command)
            .filter(|command| {
                command
                    .trim_start_matches('/')
                    .split(' ')
                    .next()
                    .unwrap_or("")
                    .starts_with("lsp")
            })
            .collect();
        assert_eq!(shown.len(), 3, "{shown:?}");
    }
}
