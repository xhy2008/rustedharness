use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Events emitted by the harness and consumed by every frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    SessionStarted {
        session_id: String,
        cwd: String,
        agent_model: String,
        compact_model: String,
    },
    UserMessage {
        text: String,
    },
    AssistantDelta {
        text: String,
    },
    AssistantReasoning {
        text: String,
    },
    AssistantMessage {
        content: String,
        reasoning: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        id: String,
        name: String,
        ok: bool,
        content: String,
        metadata: Option<Value>,
    },
    /// Live output while a tool is still running. `text` is the latest
    /// sanitized screen tail and replaces the previous live text.
    ToolOutput {
        id: String,
        name: String,
        text: String,
    },
    TodoUpdated {
        items: Vec<crate::tools::todo::TodoItem>,
    },
    LspStatus {
        language: String,
        state: String,
        detail: Option<String>,
    },
    ProcessSnapshot {
        pid: u32,
        status: String,
        screen: String,
        exit_code: Option<i32>,
    },
    ProcessExited {
        pid: u32,
        status: String,
        exit_code: Option<i32>,
    },
    DownloadConsent {
        id: String,
        server_id: String,
        display_name: String,
        version: String,
        url: String,
        runtime_hint: Option<String>,
    },
    DownloadStatus {
        server_id: String,
        state: String,
        detail: Option<String>,
        /// Bytes downloaded so far, in percent, while `state` is `downloading`.
        #[serde(default)]
        percent: Option<u8>,
    },
    TokenUsage {
        input_tokens: u64,
        output_tokens: u64,
        context_tokens: u64,
        context_limit: u64,
        /// Accumulated cost reported by providers inside `usage`, if any.
        #[serde(default)]
        cost_usd: Option<f64>,
        /// Prompt-cache hit ratio of the latest report, percent 0-100, when
        /// the provider exposes cached token counts.
        #[serde(default)]
        cache_hit_pct: Option<u64>,
    },
    Compaction {
        summary: String,
        dropped_messages: usize,
    },
    Chase {
        attempt: u32,
        pending: Vec<String>,
    },
    Notice {
        level: String,
        message: String,
    },
    Error {
        message: String,
    },
    TurnComplete {
        turn: u64,
        stop_reason: String,
    },
}

/// Commands accepted by a running harness session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessCommand {
    UserMessage {
        text: String,
    },
    Cancel,
    SetAgentModel {
        model: String,
    },
    SetCwd {
        path: String,
    },
    DownloadConsent {
        id: String,
        allow: bool,
    },
    Shutdown,

    // The TUI edits settings by calling `Agent` setters in-process; a browser
    // only holds a socket, so the same operations travel here and the server
    // answers with a state event (`settings`, `models`, `model_catalog`,
    // `turns`). Each command is idempotent and every answer carries the whole
    // current surface, so no request ids or deltas are needed.
    /// Ask for the editable settings surface.
    DescribeSettings,
    /// Write one setting: `key` is what `DescribeSettings` returned.
    SetSetting {
        key: String,
        value: Value,
    },
    /// Ask for the model catalog and the two current choices.
    DescribeModels,
    /// Select the compaction model; an empty string reuses the main model.
    SetCompactModel {
        model: String,
    },
    /// List the models an OpenAI-compatible endpoint serves.
    DiscoverModels {
        base_url: String,
        api_key: String,
    },
    /// Append an endpoint/model pair to `models.json`.
    AddModel {
        base_url: String,
        api_key: String,
        model_id: String,
        #[serde(default)]
        context_window: u64,
    },
    /// Cut the transcript back to its `turn`-th user input.
    Rollback {
        turn: usize,
    },
    /// Install or update a language server by id.
    InstallLsp {
        server: String,
        update: bool,
    },
    /// Full language-server status, including the ids `InstallLsp` takes; the
    /// live `lsp_status` events carry only the language.
    DescribeLsp,

    // An interactive shell in the workspace, for the right panel. Output is a
    // rendered screen snapshot (`terminal_output`), not raw bytes: the browser
    // then needs no ANSI parser and a slow tab cannot accumulate a backlog.
    OpenTerminal {
        cols: u16,
        rows: u16,
    },
    TerminalInput {
        data: String,
    },
    TerminalResize {
        cols: u16,
        rows: u16,
    },
    CloseTerminal,
}

/// Snapshot sent to a newly attached frontend so it can render immediately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub cwd: String,
    pub agent_model: String,
    pub compact_model: String,
    pub messages: Vec<crate::provider::ChatMessage>,
    pub todos: Vec<crate::tools::todo::TodoItem>,
    pub context_tokens: u64,
    pub context_limit: u64,
    /// Last provider-reported usage, when the session carries one. Keeps the
    /// token counters populated across a resume.
    #[serde(default)]
    pub usage: Option<crate::provider::Usage>,
    /// Running cost total for the whole session.
    #[serde(default)]
    pub cost_usd: Option<f64>,
}
