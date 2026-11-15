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
        /// `None` when the release publishes no digest and the download would
        /// therefore be installed unverified.
        sha256: Option<String>,
        runtime_hint: Option<String>,
    },
    DownloadStatus {
        server_id: String,
        state: String,
        detail: Option<String>,
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
    UserMessage { text: String },
    Cancel,
    SetAgentModel { model: String },
    SetCwd { path: String },
    DownloadConsent { id: String, allow: bool },
    Shutdown,
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
