use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{HarnessError, Result};

/// Configuration uses the Pi-style split:
///
/// * `~/.rustedharness/config.json` — harness settings and the default model.
/// * `~/.rustedharness/models.json` — providers, endpoints and their models.
///
/// `Config` is the merged runtime view. `providers` is normally loaded from
/// `models.json`; keeping it on the struct also lets a single JSON file
/// describe everything, which is convenient for tests and `--config`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Default model, either `provider/model` or a model id unique across all
    /// providers. Empty means no model is configured.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Optional model for compaction summaries. Empty means the main model
    /// writes them, so the live prompt prefix can be reused from cache.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub compact_model: String,
    /// Fallback context window for models that do not declare one. Zero means
    /// the limit is unknown.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_window: u64,
    /// Provider selected for the current model. Empty means no provider.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub edit: EditConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub image: ImageConfig,
    #[serde(default)]
    pub lsp: LspConfig,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    /// Every tool is allowed by default; deny entries are the optional
    /// blacklist.
    #[serde(default)]
    pub deny_tools: Vec<String>,
    /// Directory the config was loaded from (set at load time, never serialized).
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

/// One provider entry in `models.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// API base URL, including `/v1` for OpenAI-compatible endpoints.
    pub base_url: String,
    /// Wire protocol: `openai-completions` (default) or `openai-responses`.
    pub api: String,
    /// Literal key, `$ENV`/`${ENV}` interpolation, or `!command` output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    pub compat: Compat,
    /// Models exposed by this provider. A model whose id already exists is
    /// replaced; any other entry is appended.
    pub models: Vec<ModelConfig>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: None,
            base_url: String::new(),
            api: "openai-completions".to_string(),
            api_key: None,
            headers: BTreeMap::new(),
            compat: Compat::default(),
            models: Vec::new(),
        }
    }
}

impl ProviderConfig {
    fn merge(&mut self, incoming: ProviderConfig) {
        if incoming.name.is_some() {
            self.name = incoming.name;
        }
        if !incoming.base_url.trim().is_empty() {
            self.base_url = incoming.base_url;
        }
        if !incoming.api.trim().is_empty() {
            self.api = incoming.api;
        }
        if incoming.api_key.is_some() {
            self.api_key = incoming.api_key;
        }
        self.headers.extend(incoming.headers);
        if incoming.compat != Compat::default() {
            self.compat = incoming.compat;
        }
        for model in incoming.models {
            if let Some(existing) = self.models.iter_mut().find(|m| m.id == model.id) {
                *existing = model;
            } else {
                self.models.push(model);
            }
        }
    }

    pub fn resolved_headers(&self) -> BTreeMap<String, String> {
        self.headers
            .iter()
            .map(|(key, value)| (key.clone(), resolve_config_value(value)))
            .collect()
    }
}

/// One model entry below a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelConfig {
    /// Model id sent to the API. May contain `/` (e.g. OpenRouter ids).
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub context_window: u64,
    /// Optional output cap. Falls back to `[agent].maxOutputTokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "is_false")]
    pub reasoning: bool,
}

/// `models.json` wrapper.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelsFile {
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// A fully resolved model reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSpec {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub max_tokens: Option<u32>,
    pub reasoning: bool,
}

impl ModelSpec {
    /// Canonical `provider/model` reference.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// Compatibility switches for OpenAI-compatible endpoints. Field names and
/// defaults follow Pi's `models.json` `compat` object.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Compat {
    /// `supportsStore`: send `"store": false` on Chat Completions requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    /// `supportsDeveloperRole`: use the developer role instead of system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    /// `supportsReasoningEffort`: provider accepts `reasoning_effort`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    /// `supportsUsageInStreaming`: request `stream_options.include_usage`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    /// `supportsFinishReason`: streamed responses include `finish_reason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_finish_reason: Option<bool>,
    /// `maxTokensField`: `max_tokens` or `max_completion_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    /// `requiresToolResultName`: include the tool name on tool results.
    #[serde(skip_serializing_if = "is_false")]
    pub requires_tool_result_name: bool,
    /// `requiresAssistantAfterToolResult`: bridge tool results and a following
    /// user message with a synthetic assistant message.
    #[serde(skip_serializing_if = "is_false")]
    pub requires_assistant_after_tool_result: bool,
    /// `requiresNonNullAssistantContent`: send `""` instead of `null` for
    /// tool-call-only assistant messages.
    #[serde(skip_serializing_if = "is_false")]
    pub requires_non_null_assistant_content: bool,
    /// `requiresToolsWithToolHistory`: send `"tools": []` when the transcript
    /// contains tool calls/results but the request has no current tools.
    #[serde(skip_serializing_if = "is_false")]
    pub requires_tools_with_tool_history: bool,
    /// `requiresReasoningContentOnAssistantMessages`: emit an empty
    /// `reasoning_content` on assistant messages when reasoning is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
}

/// Pi's resolved compatibility values: explicit `models.json` overrides first,
/// then URL/provider auto-detection. Not serialized; this is the view the
/// request builders use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCompat {
    pub supports_store: bool,
    pub supports_reasoning_effort: bool,
    pub supports_developer_role: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_finish_reason: bool,
    pub max_tokens_field: MaxTokensField,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_non_null_assistant_content: bool,
    pub requires_tools_with_tool_history: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
}

impl Compat {
    /// Resolve Pi's auto-detected defaults from `api`, provider id and base URL.
    pub fn resolve(&self, api: &str, provider: &str, base_url: &str) -> ResolvedCompat {
        let url = base_url.to_ascii_lowercase();
        let provider = provider.to_ascii_lowercase();
        let is_responses = api.eq_ignore_ascii_case("openai-responses");
        let is_zai = provider == "zai"
            || provider == "zai-coding-cn"
            || url.contains("api.z.ai")
            || url.contains("open.bigmodel.cn");
        let is_together = provider == "together"
            || url.contains("api.together.ai")
            || url.contains("api.together.xyz");
        let is_moonshot = provider == "moonshotai"
            || provider == "moonshotai-cn"
            || url.contains("api.moonshot.");
        let is_cloudflare_workers =
            provider == "cloudflare-workers-ai" || url.contains("api.cloudflare.com");
        let is_cloudflare_gateway =
            provider == "cloudflare-ai-gateway" || url.contains("gateway.ai.cloudflare.com");
        let is_nvidia = provider == "nvidia" || url.contains("integrate.api.nvidia.com");
        let is_ant_ling = provider == "ant-ling" || url.contains("api.ant-ling.com");
        let is_cerebras = provider == "cerebras" || url.contains("cerebras.ai");
        let is_grok = provider == "xai" || url.contains("api.x.ai");
        let is_deepseek = provider == "deepseek" || url.contains("deepseek.com");
        let is_non_standard = is_nvidia
            || is_cerebras
            || is_grok
            || is_together
            || url.contains("chutes.ai")
            || is_deepseek
            || is_zai
            || is_moonshot
            || provider == "opencode"
            || url.contains("opencode.ai")
            || is_cloudflare_workers
            || is_cloudflare_gateway
            || is_ant_ling;
        let use_max_tokens = url.contains("chutes.ai")
            || is_deepseek
            || is_moonshot
            || is_cloudflare_gateway
            || is_together
            || is_nvidia
            || is_ant_ling
            || is_zai;
        let is_openai = provider == "openai" || url.contains("api.openai.com");
        let detected_max_tokens = if use_max_tokens {
            MaxTokensField::MaxTokens
        } else {
            MaxTokensField::MaxCompletionTokens
        };
        let detected_developer = if is_responses { true } else { is_openai };
        ResolvedCompat {
            supports_store: self.supports_store.unwrap_or(!is_non_standard),
            supports_reasoning_effort: self
                .supports_reasoning_effort
                .unwrap_or(is_responses || !is_non_standard),
            supports_developer_role: self.supports_developer_role.unwrap_or(detected_developer),
            supports_usage_in_streaming: self.supports_usage_in_streaming.unwrap_or(true),
            supports_finish_reason: self.supports_finish_reason.unwrap_or(true),
            max_tokens_field: self.max_tokens_field.unwrap_or(detected_max_tokens),
            requires_tool_result_name: self.requires_tool_result_name,
            requires_assistant_after_tool_result: self.requires_assistant_after_tool_result,
            requires_non_null_assistant_content: self.requires_non_null_assistant_content,
            requires_tools_with_tool_history: self.requires_tools_with_tool_history,
            requires_reasoning_content_on_assistant_messages: self
                .requires_reasoning_content_on_assistant_messages
                .unwrap_or(is_deepseek),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentConfig {
    pub max_turns: usize,
    /// Fraction of the context window that triggers compaction (0.80 default).
    pub compact_threshold: f64,
    /// Recent user/assistant turns kept verbatim across a compaction.
    pub keep_recent_turns: usize,
    /// With a separate summary model, omit content from older turns. The
    /// main-model path ignores this: it must reuse the live prompt verbatim
    /// to hit the provider cache.
    pub compact_strip_context: bool,
    /// How many recent user turns are sent to the separate summary model in
    /// full. Older turns keep their prose but lose the content selected by
    /// `stripContent`.
    pub send_recent_turns: usize,
    /// What to remove from turns older than `sendRecentTurns`.
    pub strip_content: StripContent,
    /// Reasoning effort for the summary request. Applies to both compaction
    /// paths; `off` sends no effort at all.
    pub compact_thinking: ThinkingLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Max todo chase injections per stall event.
    pub chase_limit: u32,
    /// Output token cap when a model does not declare `maxTokens`.
    pub max_output_tokens: u32,
    /// Maximum retries after a 429/rate-limit response. The retry interval is
    /// fixed at 3 seconds; the settings panel offers 5, 15, 30, 50 and 100.
    pub rate_limit_retries: u32,
    /// Give up on a provider request after this many milliseconds without a
    /// single byte. `0` disables the budget: slow APIs may take as long as they
    /// need, and the turn is still cancellable with Esc/Ctrl+C.
    pub http_idle_timeout_ms: u64,
    /// Report significant prompt-cache misses in the transcript.
    pub show_cache_miss_notices: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 100,
            compact_threshold: 0.80,
            keep_recent_turns: 8,
            compact_strip_context: true,
            send_recent_turns: 16,
            strip_content: StripContent::ToolOutputAndThinking,
            compact_thinking: ThinkingLevel::Off,
            temperature: Some(0.2),
            chase_limit: 3,
            max_output_tokens: 8_192,
            rate_limit_retries: 5,
            http_idle_timeout_ms: 0,
            show_cache_miss_notices: true,
        }
    }
}

/// One provider/model pair to append to `models.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewModel {
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    /// `0` when unknown; the key is then left out.
    pub context_window: u64,
    pub max_tokens: Option<u32>,
}

/// Provider id derived from the endpoint host: `https://api.deepseek.com/v1`
/// becomes `deepseek`. Sanitised to `[a-z0-9-]` because it becomes a JSON key
/// and part of every model reference. An IP endpoint keeps the whole address
/// (and port), because two local servers would otherwise share one id.
pub fn provider_id_from_url(base_url: &str) -> String {
    let authority = base_url
        .trim()
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => (host, Some(port)),
        _ => (authority, None),
    };
    let host = host.to_ascii_lowercase();
    let is_ip = host.split('.').count() == 4
        && host
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    let label = if is_ip {
        let address = host.replace('.', "-");
        match port {
            Some(port) => format!("{address}-{port}"),
            None => address,
        }
    } else {
        let host = host
            .strip_prefix("api.")
            .or_else(|| host.strip_prefix("www."))
            .unwrap_or(&host);
        host.split('.').next().unwrap_or("").to_string()
    };
    let id: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let id = id.trim_matches('-').to_string();
    if id.is_empty() {
        "custom".to_string()
    } else {
        id
    }
}

/// Get (or create) the object under `key`, replacing a non-object value.
fn object_entry<'a>(
    map: &'a mut serde_json::Map<String, Value>,
    key: &str,
) -> &'a mut serde_json::Map<String, Value> {
    let slot = map.entry(key.to_string()).or_insert_with(|| json!({}));
    if !slot.is_object() {
        *slot = json!({});
    }
    slot.as_object_mut().expect("object guaranteed above")
}

/// Runtime-tunable compaction settings. The TUI edits these through
/// `/settings` without rebuilding the agent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    pub compact_threshold: f64,
    pub keep_recent_turns: usize,
    pub compact_strip_context: bool,
    pub send_recent_turns: usize,
    pub strip_content: StripContent,
    pub compact_thinking: ThinkingLevel,
}

impl From<&AgentConfig> for CompactionSettings {
    fn from(cfg: &AgentConfig) -> Self {
        Self {
            compact_threshold: cfg.compact_threshold,
            keep_recent_turns: cfg.keep_recent_turns,
            compact_strip_context: cfg.compact_strip_context,
            send_recent_turns: cfg.send_recent_turns,
            strip_content: cfg.strip_content,
            compact_thinking: cfg.compact_thinking,
        }
    }
}

/// What a separate summary model may omit from older turns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StripContent {
    #[serde(rename = "none")]
    Nothing,
    ToolOutput,
    #[default]
    ToolOutputAndThinking,
}

impl StripContent {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nothing => "none",
            Self::ToolOutput => "tool output",
            Self::ToolOutputAndThinking => "tool output and thinking",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Nothing => Self::ToolOutput,
            Self::ToolOutput => Self::ToolOutputAndThinking,
            Self::ToolOutputAndThinking => Self::Nothing,
        }
    }
}

/// Reasoning effort requested for a compaction summary. `Off` sends no
/// `reasoning_effort` field, so the provider's default applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingLevel {
    /// Every level, weakest first: the order a picker shows them in.
    pub const ALL: [Self; 6] = [
        Self::Off,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Wire value for `reasoning_effort`; `None` omits the field.
    pub fn effort(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::XHigh,
            Self::XHigh => Self::Max,
            Self::Max => Self::Off,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EditConfig {
    /// Hashline anchor width, 2..=4 characters.
    pub anchor_width: usize,
    /// Cap on edits per call; `None` means unlimited. Set to 1 to degrade to
    /// single-patch behaviour for weak models.
    pub max_ops_per_call: Option<usize>,
    pub enable_replace_text: bool,
    pub diagnostics_timeout_ms: u64,
}

impl Default for EditConfig {
    fn default() -> Self {
        Self {
            anchor_width: 2,
            max_ops_per_call: None,
            enable_replace_text: true,
            diagnostics_timeout_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ShellConfig {
    /// Which shell the agent runs commands in: `"git-bash"` for the bash that
    /// ships with Git for Windows, `"powershell"` (or anything else) for the
    /// platform's own shell. Ignored where bash is the only choice.
    pub prefer: String,
    pub timeout_ms: u64,
    pub activity_timeout_ms: u64,
    pub max_output_bytes: usize,
    pub pty_rows: u16,
    pub pty_cols: u16,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            prefer: "default".to_string(),
            timeout_ms: 30_000,
            activity_timeout_ms: 300_000,
            max_output_bytes: 1_048_576,
            pty_rows: 50,
            pty_cols: 200,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspConfig {
    pub enabled: bool,
    pub auto_download: bool,
    /// Downloads require consent; in non-interactive mode this must stay false.
    pub allow_downloads_noninteractive: bool,
    pub idle_timeout_secs: u64,
    pub max_restarts: u32,
    /// Command overrides per language, e.g. rust = "/opt/rust-analyzer".
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, String>,
    pub request_timeout_ms: u64,
    /// Optional download overrides, keyed by server id. URLs default to the
    /// registry's GitHub release; entries here pin a version or point at a
    /// private mirror.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub downloads: BTreeMap<String, LspDownload>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspDownload {
    /// Release tag to install. Unset means the newest release, which is what
    /// the built-in registry uses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// HTTPS archive/binary URL, replacing the release asset (a mirror).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// gz | zip | tar.gz | raw, inferred from the asset name when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
    /// Binary path inside the archive (relative).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Additional allowed hostnames (official domains).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
}

/// How `read` hands images to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImageConfig {
    /// Longest edge, in pixels, an attached image may have. A bigger image is
    /// downscaled to fit before it is uploaded instead of being refused.
    pub max_edge: u32,
}

impl Default for ImageConfig {
    fn default() -> Self {
        // Anthropic recommends 1568 px on the long edge; the other providers
        // accept it unharmed, and it keeps a screenshot's text legible.
        Self { max_edge: 1568 }
    }
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_download: true,
            allow_downloads_noninteractive: false,
            idle_timeout_secs: 600,
            max_restarts: 3,
            servers: BTreeMap::new(),
            request_timeout_ms: 5_000,
            downloads: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WebConfig {
    pub host: String,
    pub port: u16,
    /// Optional bearer token protecting the REST/WS endpoints.
    pub auth_token: Option<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8787,
            auth_token: None,
        }
    }
}

/// Which web search backend `web_search` uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchBackend {
    Brave,
    Tavily,
    #[serde(rename = "searxng")]
    SearXng,
    /// Keyless HTML search; an optional key switches to the official API.
    #[default]
    Bing,
    #[serde(rename = "duckduckgo")]
    DuckDuckGo,
}

impl SearchBackend {
    /// Order shown in `/settings`, keyless options included.
    pub const ALL: [Self; 5] = [
        Self::Brave,
        Self::Tavily,
        Self::SearXng,
        Self::Bing,
        Self::DuckDuckGo,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Brave => "brave",
            Self::Tavily => "tavily",
            Self::SearXng => "searxng",
            Self::Bing => "bing",
            Self::DuckDuckGo => "duckduckgo",
        }
    }

    /// What this backend needs before it can search; `None` when it needs no
    /// account at all.
    pub fn credential(self) -> Option<Credential> {
        let credential = |key, kind| Some(Credential { key, kind });
        match self {
            Self::Brave => credential("braveApiKey", CredentialKind::Key),
            Self::Tavily => credential("tavilyApiKey", CredentialKind::Key),
            Self::SearXng => credential("searxngUrl", CredentialKind::Url),
            Self::Bing => credential("bingApiKey", CredentialKind::OptionalKey),
            Self::DuckDuckGo => None,
        }
    }
}

/// What a backend needs before it can search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credential {
    /// `[search]` key that stores the value.
    pub key: &'static str,
    pub kind: CredentialKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// An API key; the backend cannot search without it.
    Key,
    /// An API key that only upgrades the backend to its official API.
    OptionalKey,
    /// The base URL of a self-hosted instance, not a secret.
    Url,
}

impl CredentialKind {
    /// Whether the backend is unusable until the value is set.
    pub fn required(self) -> bool {
        matches!(self, Self::Key | Self::Url)
    }
}

/// `web_search` settings: one backend plus whatever credential it needs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchConfig {
    pub backend: SearchBackend,
    /// Brave Search subscription token; `BRAVE_API_KEY` is used when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brave_api_key: Option<String>,
    /// Tavily API key; `TAVILY_API_KEY` is used when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tavily_api_key: Option<String>,
    /// Bing v7 subscription key. Unset means the website is scraped instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bing_api_key: Option<String>,
    /// Base URL of a SearXNG instance, e.g. `https://searx.example.com`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searxng_url: Option<String>,
}

/// Where a backend's credential comes from, for the settings panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// Stored in `config.json` under `[search]`.
    Config,
    /// Read from the backend's environment variable.
    Environment,
}

/// Trim a configured value, treating a blank string as unset.
fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

impl SearchConfig {
    /// The stored value and the environment variable behind it. `None` for a
    /// backend that needs no credential.
    fn slots(&self, backend: SearchBackend) -> Option<(&Option<String>, Option<&'static str>)> {
        match backend {
            SearchBackend::Brave => Some((&self.brave_api_key, Some("BRAVE_API_KEY"))),
            SearchBackend::Tavily => Some((&self.tavily_api_key, Some("TAVILY_API_KEY"))),
            SearchBackend::SearXng => Some((&self.searxng_url, Some("SEARXNG_URL"))),
            SearchBackend::Bing => Some((&self.bing_api_key, None)),
            SearchBackend::DuckDuckGo => None,
        }
    }

    /// The credential a backend searches with: the stored value, else the
    /// environment variable that has always worked when the file says nothing.
    pub fn credential(&self, backend: SearchBackend) -> Option<String> {
        let (stored, env) = self.slots(backend)?;
        stored.as_deref().and_then(non_empty).or_else(|| {
            let value = std::env::var(env?).ok()?;
            non_empty(&value)
        })
    }

    pub fn credential_source(&self, backend: SearchBackend) -> Option<CredentialSource> {
        let (stored, _) = self.slots(backend)?;
        if stored.as_deref().and_then(non_empty).is_some() {
            Some(CredentialSource::Config)
        } else if self.credential(backend).is_some() {
            Some(CredentialSource::Environment)
        } else {
            None
        }
    }

    /// Whether the selected backend can answer as configured.
    pub fn ready(&self) -> bool {
        let backend = self.backend;
        match backend.credential() {
            Some(spec) if spec.kind.required() => self.credential(backend).is_some(),
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    pub max_saved_sessions: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            dir: None,
            max_saved_sessions: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpConfig {
    pub enabled: bool,
    /// Path to mcp.json; defaults to `<cwd>/mcp.json` then the user config dir.
    pub config_path: Option<PathBuf>,
    pub deny: Vec<String>,
    pub call_timeout_ms: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            config_path: None,
            deny: Vec::new(),
            call_timeout_ms: 30_000,
        }
    }
}

/// Read a JSON or JSONC file as an object. A missing or damaged file becomes an
/// empty object, so the edit saves instead of failing.
fn load_json_object(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| parse_jsonc::<Value>(raw.strip_prefix('\u{feff}').unwrap_or(&raw)).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// Write `root` back as pretty JSON, creating its directory first.
fn save_json_object(path: &Path, root: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(root)?;
    text.push('\n');
    std::fs::write(path, text)?;
    Ok(())
}

impl Config {
    /// `~/.rustedharness` by default, or `RUSTEDHARNESS_DIR` when set.
    pub fn config_dir() -> PathBuf {
        if let Ok(value) = std::env::var("RUSTEDHARNESS_DIR") {
            let value = value.trim();
            if !value.is_empty() {
                return expand_tilde(Path::new(value));
            }
        }
        home_dir().join(".rustedharness")
    }

    /// Sessions, the code index and LSP downloads live beside the config.
    pub fn data_dir() -> PathBuf {
        Self::config_dir()
    }

    pub fn load() -> Result<Self> {
        Self::load_from_dir(&Self::config_dir())
    }

    /// Load a `config.json`, a `models.json`, or a directory containing both.
    pub fn load_from(path: &Path) -> Result<Self> {
        let path = expand_tilde(path);
        if path.is_dir() {
            return Self::load_from_dir(&path);
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let mut cfg = match extension.as_str() {
            "json" if file_name.eq_ignore_ascii_case("models.json") => {
                let mut cfg = Config::default();
                cfg.providers.clear();
                cfg.merge_models(Self::read_models(&path)?);
                cfg
            }
            "json" => Self::read_json(&path)?,
            _ => {
                return Err(HarnessError::Config(format!(
                    "unsupported config file `{}`; use .json",
                    path.display()
                )))
            }
        };
        // `--config /some/dir/config.json` should pick up the sibling
        // models.json automatically.
        if file_name.eq_ignore_ascii_case("config.json") {
            let sibling = path.with_file_name("models.json");
            if sibling.is_file() {
                cfg.merge_models(Self::read_models(&sibling)?);
            }
        }
        cfg.apply_env();
        cfg.normalize();
        cfg.source = Some(
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
        );
        Ok(cfg)
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let settings_path = dir.join("config.json");
        let models_path = dir.join("models.json");

        let mut cfg = if settings_path.is_file() {
            Self::read_json(&settings_path)?
        } else {
            Config::default()
        };
        if models_path.is_file() {
            // When `models.json` exists it is the complete provider list. Drop
            // the built-in provider that only exists to make a bare install
            // work, unless a settings file explicitly defined providers.
            if !settings_path.is_file() {
                cfg.providers.clear();
            }
            cfg.merge_models(Self::read_models(&models_path)?);
        }
        cfg.apply_env();
        cfg.normalize();
        cfg.source = Some(dir.to_path_buf());
        Ok(cfg)
    }

    fn read_text(path: &Path) -> Result<String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| HarnessError::Config(format!("cannot read {}: {e}", path.display())))?;
        // A UTF-8 BOM is common in Windows editors and is not valid JSON.
        Ok(raw
            .strip_prefix('\u{feff}')
            .map(str::to_string)
            .unwrap_or(raw))
    }

    fn read_json(path: &Path) -> Result<Self> {
        let raw = Self::read_text(path)?;
        parse_jsonc::<Config>(&raw)
            .map_err(|e| HarnessError::Config(format!("invalid config {}: {e}", path.display())))
    }

    fn read_models(path: &Path) -> Result<ModelsFile> {
        let raw = Self::read_text(path)?;
        parse_jsonc::<ModelsFile>(&raw).map_err(|e| {
            HarnessError::Config(format!("invalid models file {}: {e}", path.display()))
        })
    }

    /// Write `config.json` and `models.json` if they do not exist. Existing
    /// files are never overwritten, so local edits are safe.
    pub fn write_default_in(dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let settings = dir.join("config.json");
        let models = dir.join("models.json");
        if !settings.exists() {
            let (settings_text, models_text) = (default_config_json(), default_models_json());
            std::fs::write(&settings, settings_text)?;
            if !models.exists() {
                std::fs::write(&models, models_text)?;
            }
        } else if !models.exists() {
            std::fs::write(&models, default_models_json())?;
        }
        Ok(settings)
    }

    /// Serialize this configuration as the Pi-style pair of files.
    pub fn split_files(&self) -> (String, String) {
        let mut value = serde_json::to_value(self).expect("config is serializable");
        if let Some(object) = value.as_object_mut() {
            object.remove("providers");
            object.remove("provider");
        }
        let mut settings = serde_json::to_string_pretty(&value).expect("config is serializable");
        settings.push('\n');
        let file = ModelsFile {
            providers: self.providers.clone(),
        };
        let mut models = serde_json::to_string_pretty(&file).expect("models are serializable");
        models.push('\n');
        (settings, models)
    }

    fn apply_env(&mut self) {
        let provider_env = std::env::var("RUSTEDHARNESS_PROVIDER")
            .ok()
            .filter(|value| !value.trim().is_empty());
        if let Some(provider) = &provider_env {
            self.provider = provider.clone();
        }
        let model_env = std::env::var("RUSTEDHARNESS_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty());
        if let Some(model) = model_env {
            self.model = model;
        }
        if let Ok(model) = std::env::var("RUSTEDHARNESS_COMPACT_MODEL") {
            if !model.trim().is_empty() {
                self.compact_model = model;
            }
        }
    }

    fn normalize(&mut self) {
        self.edit.anchor_width = self.edit.anchor_width.clamp(2, 4);
        self.agent.rate_limit_retries = self.agent.rate_limit_retries.max(1);
        self.agent.compact_threshold = self.agent.compact_threshold.clamp(0.1, 0.99);
        self.agent.keep_recent_turns = self.agent.keep_recent_turns.max(1);
        self.agent.send_recent_turns = self.agent.send_recent_turns.max(1);
        // Never invent a provider or model. An unset model stays unset and the
        // frontends render it as `None` until the user configures one.
        if !self.model.trim().is_empty() {
            if let Ok(spec) = self.resolve_model(&self.model) {
                self.model = spec.reference();
                self.provider = spec.provider;
            }
        }
        if !self.compact_model.trim().is_empty() {
            if let Ok(spec) = self.resolve_model(&self.compact_model) {
                self.compact_model = spec.reference();
            }
        }
    }

    /// Model that writes compaction summaries. Empty `compactModel` means the
    /// main model; an explicit model is routed independently.
    pub fn compaction_model(&self) -> &str {
        if self.compact_model.trim().is_empty() {
            &self.model
        } else {
            &self.compact_model
        }
    }

    /// Persist one setting into `config.json`, leaving every other key
    /// (including ones this version does not model) untouched. `path` is a
    /// JSON object path, e.g. `["agent", "compactThreshold"]`.
    pub fn save_setting(&self, path: &[&str], value: Value) -> Result<()> {
        let (last, parents) = path
            .split_last()
            .ok_or_else(|| HarnessError::Config("empty settings path".to_string()))?;
        let path_on_disk = self.source_dir().join("config.json");
        let mut root = load_json_object(&path_on_disk);
        let mut slot = &mut root;
        for key in parents {
            if !slot.is_object() {
                *slot = Value::Object(Default::default());
            }
            let object = slot.as_object_mut().expect("object inserted above");
            slot = object
                .entry((*key).to_string())
                .or_insert_with(|| Value::Object(Default::default()));
        }
        if !slot.is_object() {
            *slot = Value::Object(Default::default());
        }
        slot.as_object_mut()
            .expect("object inserted above")
            .insert((*last).to_string(), value);
        save_json_object(&path_on_disk, &root)
    }

    /// Append one provider/model pair to `models.json`, returning the
    /// `provider/model` reference. Existing keys in the file are preserved
    /// byte-for-byte in meaning: the file is edited as JSON, not regenerated
    /// from this struct, so hand-written compat flags survive.
    pub fn add_model(&self, new: &NewModel) -> Result<String> {
        let base_url = new.base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(HarnessError::Config("base URL is empty".to_string()));
        }
        let model_id = new.model_id.trim();
        if model_id.is_empty() {
            return Err(HarnessError::Config("model id is empty".to_string()));
        }
        let provider_id = provider_id_from_url(&base_url);
        let path = self.source_dir().join("models.json");
        let mut root = load_json_object(&path);
        let provider = object_entry(
            object_entry(
                root.as_object_mut().expect("loaded as an object"),
                "providers",
            ),
            &provider_id,
        );
        provider.insert("baseUrl".to_string(), json!(base_url));
        // The wizard only speaks `openai-completions`, but a provider that is
        // already configured keeps the protocol its other models were written
        // for.
        provider
            .entry("api".to_string())
            .or_insert_with(|| json!("openai-completions"));
        let api_key = new.api_key.trim();
        if !api_key.is_empty() {
            provider.insert("apiKey".to_string(), json!(api_key));
        }
        let models = provider
            .entry("models".to_string())
            .or_insert_with(|| json!([]));
        if !models.is_array() {
            *models = json!([]);
        }
        let models = models.as_array_mut().expect("array inserted above");
        models.retain(|model| model.get("id").and_then(Value::as_str) != Some(model_id));
        let mut entry = json!({ "id": model_id });
        if new.context_window > 0 {
            entry["contextWindow"] = json!(new.context_window);
        }
        if let Some(max_tokens) = new.max_tokens.filter(|value| *value > 0) {
            entry["maxTokens"] = json!(max_tokens);
        }
        models.push(entry);

        save_json_object(&path, &root)?;
        Ok(format!("{provider_id}/{model_id}"))
    }

    fn merge_models(&mut self, file: ModelsFile) {
        for (id, incoming) in file.providers {
            match self.providers.get_mut(&id) {
                Some(existing) => existing.merge(incoming),
                None => {
                    self.providers.insert(id, incoming);
                }
            }
        }
    }

    pub fn default_provider_id(&self) -> Option<String> {
        if self.providers.contains_key(&self.provider) {
            return Some(self.provider.clone());
        }
        if let Some((head, _)) = self.model.split_once('/') {
            if self.providers.contains_key(head) {
                return Some(head.to_string());
            }
        }
        self.providers.keys().next().cloned()
    }

    /// Resolve `provider/model`, a bare unique model id, or a full model id that
    /// itself contains slashes (OpenRouter style).
    pub fn resolve_model(&self, reference: &str) -> Result<ModelSpec> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(HarnessError::Config("empty model reference".to_string()));
        }

        let exact: Vec<&String> = self
            .providers
            .iter()
            .filter(|(_, provider)| provider.models.iter().any(|m| m.id == reference))
            .map(|(id, _)| id)
            .collect();

        let (provider_id, model_id) = if let Some((head, rest)) = reference.split_once('/') {
            if self.providers.contains_key(head) {
                (head.to_string(), rest.to_string())
            } else if exact.len() == 1 {
                (exact[0].clone(), reference.to_string())
            } else {
                let Some(provider) = self.default_provider_id() else {
                    return Err(HarnessError::Config(format!(
                        "no provider configured for model `{reference}`"
                    )));
                };
                (provider, reference.to_string())
            }
        } else if exact.len() == 1 {
            (exact[0].clone(), reference.to_string())
        } else if exact.len() > 1 {
            let default = self.default_provider_id();
            let selected = default
                .filter(|id| exact.iter().any(|candidate| candidate.as_str() == id))
                .unwrap_or_else(|| exact[0].clone());
            (selected, reference.to_string())
        } else {
            let Some(provider) = self.default_provider_id() else {
                return Err(HarnessError::Config(format!(
                    "no provider configured for model `{reference}`"
                )));
            };
            (provider, reference.to_string())
        };

        let provider = self
            .providers
            .get(&provider_id)
            .ok_or_else(|| HarnessError::Config(format!("unknown provider `{provider_id}`")))?;
        let model = provider.models.iter().find(|m| m.id == model_id);
        let context_window = model
            .map(|m| m.context_window)
            .filter(|value| *value > 0)
            .unwrap_or(self.context_window);
        let max_tokens = model.and_then(|m| m.max_tokens).filter(|value| *value > 0);
        Ok(ModelSpec {
            provider: provider_id,
            id: model_id.clone(),
            name: model
                .and_then(|m| m.name.clone())
                .unwrap_or_else(|| model_id.clone()),
            context_window,
            max_tokens,
            reasoning: model.map(|m| m.reasoning).unwrap_or(false),
        })
    }

    /// All configured models, in provider id order.
    pub fn model_catalog(&self) -> Vec<ModelSpec> {
        let mut out = Vec::new();
        for (provider_id, provider) in &self.providers {
            for model in &provider.models {
                if model.id.trim().is_empty() {
                    continue;
                }
                out.push(ModelSpec {
                    provider: provider_id.clone(),
                    id: model.id.clone(),
                    name: model.name.clone().unwrap_or_else(|| model.id.clone()),
                    context_window: if model.context_window > 0 {
                        model.context_window
                    } else {
                        self.context_window
                    },
                    max_tokens: model.max_tokens.filter(|value| *value > 0),
                    reasoning: model.reasoning,
                });
            }
        }
        out
    }

    pub fn model_context_window(&self, reference: &str) -> u64 {
        self.resolve_model(reference)
            .map(|model| model.context_window)
            .unwrap_or(self.context_window)
    }

    pub fn model_max_tokens(&self, reference: &str) -> Option<u32> {
        self.resolve_model(reference)
            .ok()
            .and_then(|model| model.max_tokens)
    }

    pub fn set_agent_model(&mut self, reference: &str) -> Result<()> {
        let spec = self.resolve_model(reference)?;
        self.model = spec.reference();
        self.provider = spec.provider;
        Ok(())
    }

    pub fn resolve_api_key_for_provider(&self, provider_id: &str) -> String {
        let Some(provider) = self.providers.get(provider_id) else {
            return String::new();
        };
        if let Some(raw) = provider.api_key.as_deref() {
            if !raw.trim().is_empty() {
                return resolve_config_value(raw);
            }
        }
        String::new()
    }

    /// Redacted clone for `config show` and the web API.
    pub fn redacted(&self) -> Self {
        let mut cfg = self.clone();
        for provider in cfg.providers.values_mut() {
            if provider.api_key.is_some() {
                provider.api_key = Some("***".to_string());
            }
            for value in provider.headers.values_mut() {
                *value = "***".to_string();
            }
        }
        cfg
    }

    /// Directory of the loaded config file, or the default config directory.
    pub fn source_dir(&self) -> PathBuf {
        self.source.clone().unwrap_or_else(Self::config_dir)
    }

    /// Project instructions for the system prompt: the workspace `AGENTS.md`
    /// wins outright over `~/.rustedharness/AGENTS.md`, which is the fallback
    /// for instructions that apply to every workspace.
    pub fn instructions(&self, workspace: &Path) -> Option<(String, String)> {
        instructions_from(workspace, &Self::config_dir())
    }

    /// Default transcript directory when `[session].dir` is unset.
    pub fn sessions_dir() -> PathBuf {
        Self::data_dir().join("sessions")
    }
}

/// The two places project instructions may live, in priority order: a
/// workspace `AGENTS.md`, then the one in the config directory. Nothing else is
/// consulted - no walking up parent directories, no other file names - so it is
/// always obvious which file the model is following.
pub(crate) fn instructions_from(workspace: &Path, config_dir: &Path) -> Option<(String, String)> {
    for (label, path) in [
        ("AGENTS.md", workspace.join("AGENTS.md")),
        ("~/.rustedharness/AGENTS.md", config_dir.join("AGENTS.md")),
    ] {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !text.trim().is_empty() {
            return Some((label.to_string(), text));
        }
    }
    None
}

/// Parse JSON with comments and trailing commas, as accepted by Pi config files.
fn parse_jsonc<T: DeserializeOwned>(
    raw: &str,
) -> std::result::Result<T, jsonc_parser::errors::ParseError> {
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    };
    jsonc_parser::parse_to_serde_value(raw, &options)
}

/// Expand a leading `~` in paths supplied through `--config` or environment
/// variables. Only a bare `~` and `~/…` are expanded, with the same home
/// directory the rest of the config uses.
pub fn expand_tilde(path: &Path) -> PathBuf {
    match shellexpand::path::tilde_with_context(path, || Some(home_dir())) {
        Cow::Owned(expanded) => expanded,
        Cow::Borrowed(_) => path.to_path_buf(),
    }
}

fn home_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve a value from `models.json`: `$NAME`, `${NAME}`, `$$`, `$!`, a
/// literal, or a leading `!command`.
pub fn resolve_config_value(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(command) = raw.strip_prefix('!') {
        return run_command(command);
    }
    interpolate_env(raw)
}

pub(crate) fn interpolate_env(raw: &str) -> String {
    // `$!` is this config's escape for a literal `!`; shell expansion only
    // knows `$$`, so it is rewritten before the expansion runs.
    let escaped = raw.replace("$!", "!");
    // An unset variable expands to the empty string rather than staying
    // literal, which is what shell expansion does by default.
    shellexpand::env_with_context_no_errors(&escaped, |name| {
        Some(std::env::var(name).unwrap_or_default())
    })
    .into_owned()
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Human-facing model label; an empty reference means nothing is configured.
pub fn model_display(model: &str) -> &str {
    if model.trim().is_empty() {
        "None"
    } else {
        model
    }
}

fn run_command(command: &str) -> String {
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };
    cmd.output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Starter `config.json`. Providers live in `models.json`.
pub fn default_config_json() -> String {
    Config::default().split_files().0
}

/// Starter `models.json` with the built-in OpenAI provider.
pub fn default_models_json() -> String {
    Config::default().split_files().1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_model_until_configured() {
        let mut cfg: Config =
            serde_json::from_str(&default_config_json()).expect("template parses");
        cfg.normalize();
        assert_eq!(cfg.edit.anchor_width, 2);
        assert!(cfg.model.is_empty());
        assert!(cfg.compact_model.is_empty());
        assert!(cfg.providers.is_empty());
        assert_eq!(cfg.context_window, 0);
        assert_eq!(model_display(&cfg.model), "None");
        assert_eq!(cfg.agent.compact_threshold, 0.8);
        assert!(cfg.agent.compact_strip_context);
        assert_eq!(cfg.agent.send_recent_turns, 16);
        assert_eq!(cfg.agent.strip_content, StripContent::ToolOutputAndThinking);
    }

    #[test]
    fn separated_compaction_options_parse() {
        let cfg: Config = serde_json::from_str(
            r#"{"agent":{"keepRecentTurns":4,"sendRecentTurns":6,"stripContent":"toolOutput","compactThinking":"xhigh"}}"#,
        )
        .unwrap();
        let settings = CompactionSettings::from(&cfg.agent);
        assert_eq!(settings.keep_recent_turns, 4);
        assert_eq!(settings.send_recent_turns, 6);
        assert_eq!(settings.strip_content, StripContent::ToolOutput);
        assert_eq!(settings.compact_thinking, ThinkingLevel::XHigh);
    }

    /// The idle budget defaults to disabled so slow free APIs are never cut
    /// off; notices default to on because they explain a cost jump.
    #[test]
    fn network_defaults_match_the_slow_api_use_case() {
        let cfg = Config::default();
        assert_eq!(cfg.agent.http_idle_timeout_ms, 0);
        assert!(cfg.agent.show_cache_miss_notices);

        let cfg: Config = serde_json::from_str(
            r#"{"agent":{"httpIdleTimeoutMs":120000,"showCacheMissNotices":false}}"#,
        )
        .unwrap();
        assert_eq!(cfg.agent.http_idle_timeout_ms, 120_000);
        assert!(!cfg.agent.show_cache_miss_notices);
    }

    /// Every level the settings panel cycles through round-trips through JSON,
    /// and `off` is the default so no effort is requested unless asked for.
    #[test]
    fn thinking_levels_round_trip_and_cycle_in_order() {
        assert_eq!(ThinkingLevel::default(), ThinkingLevel::Off);
        assert_eq!(ThinkingLevel::default().effort(), None);
        let mut level = ThinkingLevel::Off;
        let mut labels = Vec::new();
        for _ in 0..6 {
            labels.push(level.label());
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(serde_json::from_str::<ThinkingLevel>(&json).unwrap(), level);
            level = level.next();
        }
        assert_eq!(labels, ["off", "low", "medium", "high", "xhigh", "max"]);
        assert_eq!(level, ThinkingLevel::Off, "the cycle wraps around");
        assert_eq!(ThinkingLevel::Max.effort(), Some("max"));
    }

    #[test]
    fn jsonc_comments_are_accepted() {
        let raw = r#"{
          // provider comment
          "providers": {
            "p": {
              "baseUrl": "https://example.com/v1/a//b",
              /* block comment */
              "models": [{ "id": "m", }],
            },
          },
        }"#;
        let file: ModelsFile = parse_jsonc(raw).unwrap();
        assert_eq!(file.providers["p"].models[0].id, "m");
        assert_eq!(file.providers["p"].base_url, "https://example.com/v1/a//b");
    }

    #[test]
    fn jsonc_error_keeps_the_original_line_number() {
        let err = parse_jsonc::<Value>("{\n  // comment\n  \"a\":\n}").unwrap_err();
        assert!(err.to_string().contains("line 4"), "{err}");
    }

    #[test]
    fn pi_style_compat_and_model_fields_parse() {
        let file: ModelsFile = serde_json::from_str(
            r#"{
              "providers": {
                "proxy": {
                  "baseUrl": "https://example.com/v1",
                  "api": "openai-completions",
                  "compat": {
                    "supportsDeveloperRole": false,
                    "supportsUsageInStreaming": true,
                    "maxTokensField": "max_completion_tokens",
                    "requiresToolResultName": true,
                    "requiresNonNullAssistantContent": true
                  },
                  "models": [
                    { "id": "m", "contextWindow": 64000, "maxTokens": 4096, "input": ["text", "image"] }
                  ]
                }
              }
            }"#,
        )
        .unwrap();
        let provider = &file.providers["proxy"];
        assert_eq!(provider.compat.supports_developer_role, Some(false));
        assert_eq!(provider.compat.supports_usage_in_streaming, Some(true));
        assert_eq!(
            provider.compat.max_tokens_field,
            Some(MaxTokensField::MaxCompletionTokens)
        );
        assert!(provider.compat.requires_tool_result_name);
        assert!(provider.compat.requires_non_null_assistant_content);
        assert_eq!(provider.models[0].context_window, 64_000);
        assert_eq!(provider.models[0].max_tokens, Some(4096));
    }

    #[test]
    fn default_models_template_is_empty() {
        let file: ModelsFile =
            serde_json::from_str(&default_models_json()).expect("models template parses");
        assert!(file.providers.is_empty());
    }

    #[test]
    fn multiple_providers_resolve_by_reference_and_bare_id() {
        let cfg: Config = serde_json::from_str(
            r#"{
              "model": "ollama/qwen2.5-coder:7b",
              "providers": {
                "openai": {
                  "baseUrl": "https://api.openai.com/v1",
                  "models": [{ "id": "gpt-4o", "contextWindow": 128000 }]
                },
                "ollama": {
                  "baseUrl": "http://127.0.0.1:11434/v1",
                  "apiKey": "ollama",
                  "models": [{ "id": "qwen2.5-coder:7b", "contextWindow": 32768 }]
                }
              }
            }"#,
        )
        .expect("config parses");
        let spec = cfg.resolve_model("ollama/qwen2.5-coder:7b").unwrap();
        assert_eq!(spec.provider, "ollama");
        assert_eq!(spec.context_window, 32_768);
        let bare = cfg.resolve_model("qwen2.5-coder:7b").unwrap();
        assert_eq!(bare.provider, "ollama");
        let default = cfg.resolve_model("gpt-4o").unwrap();
        assert_eq!(default.provider, "openai");
    }

    #[test]
    fn openrouter_style_model_ids_keep_their_slashes() {
        let cfg: Config = serde_json::from_str(
            r#"{
              "provider": "openrouter",
              "providers": {
                "openrouter": {
                  "baseUrl": "https://openrouter.ai/api/v1",
                  "models": [
                    { "id": "google/gemini-2.5-pro", "contextWindow": 1000000 },
                    { "id": "anthropic/claude-sonnet-4" }
                  ]
                }
              }
            }"#,
        )
        .expect("config parses");
        let spec = cfg.resolve_model("google/gemini-2.5-pro").unwrap();
        assert_eq!(spec.provider, "openrouter");
        assert_eq!(spec.id, "google/gemini-2.5-pro");
        let explicit = cfg
            .resolve_model("openrouter/anthropic/claude-sonnet-4")
            .unwrap();
        assert_eq!(explicit.provider, "openrouter");
        assert_eq!(explicit.id, "anthropic/claude-sonnet-4");
    }

    #[test]
    fn env_interpolation_and_escapes() {
        std::env::set_var("RUSTEDHARNESS_TEST_KEY", "secret-value");
        assert_eq!(
            resolve_config_value("$RUSTEDHARNESS_TEST_KEY"),
            "secret-value"
        );
        assert_eq!(
            resolve_config_value("${RUSTEDHARNESS_TEST_KEY}"),
            "secret-value"
        );
        assert_eq!(resolve_config_value("$$literal"), "$literal");
        assert_eq!(resolve_config_value("$!literal"), "!literal");
        // Unset variables expand to nothing, and an unterminated expression is
        // left as written instead of swallowing the rest of the value.
        assert_eq!(resolve_config_value("a-$RUSTEDHARNESS_UNSET-b"), "a--b");
        assert_eq!(
            resolve_config_value("a-${RUSTEDHARNESS_UNSET-b"),
            "a-${RUSTEDHARNESS_UNSET-b"
        );
        std::env::remove_var("RUSTEDHARNESS_TEST_KEY");
    }

    #[test]
    fn only_a_leading_tilde_is_expanded() {
        let home = home_dir();
        assert_eq!(expand_tilde(Path::new("~")), home);
        assert_eq!(
            expand_tilde(Path::new("~/config.json")),
            home.join("config.json")
        );
        assert_eq!(
            expand_tilde(Path::new("/tmp/~backup")),
            PathBuf::from("/tmp/~backup")
        );
    }

    /// Empty `compactModel` means the main model; a value routes summaries to
    /// that separate model.
    #[test]
    fn an_empty_compact_model_means_the_main_model() {
        let mut cfg: Config =
            serde_json::from_str(r#"{"model":"amd/big","compactModel":"amd/small"}"#).unwrap();
        cfg.normalize();
        assert_eq!(cfg.compaction_model(), cfg.compact_model);

        let mut cfg: Config = serde_json::from_str(r#"{"model":"amd/big"}"#).unwrap();
        cfg.normalize();
        assert!(cfg.compact_model.is_empty());
        assert_eq!(cfg.compaction_model(), cfg.model);
    }

    /// A nested setting writes one key and leaves the rest of the file alone.
    #[test]
    fn saving_a_setting_edits_only_that_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"model":"m","denyTools":["bash"],"note":"keep me","agent":{"maxTurns":7}}"#,
        )
        .unwrap();
        let cfg = Config {
            source: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        cfg.save_setting(&["agent", "compactThreshold"], serde_json::json!(0.7))
            .unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["agent"]["compactThreshold"], 0.7);
        assert_eq!(saved["agent"]["maxTurns"], 7);
        assert_eq!(saved["denyTools"][0], "bash");
        assert_eq!(saved["note"], "keep me");
    }

    #[test]
    fn rate_limit_settings_parse_and_normalize() {
        let cfg: Config = serde_json::from_str(r#"{"agent":{"rateLimitRetries":30}}"#).unwrap();
        assert_eq!(cfg.agent.rate_limit_retries, 30);
        assert_eq!(Config::default().agent.rate_limit_retries, 5);

        let mut cfg: Config = serde_json::from_str(r#"{"agent":{"rateLimitRetries":0}}"#).unwrap();
        cfg.normalize();
        assert_eq!(cfg.agent.rate_limit_retries, 1, "one retry is the floor");
    }

    /// The add-model flow edits `models.json` in place: unknown keys survive,
    /// the provider is created from the URL host, and re-adding the same model
    /// replaces its entry instead of duplicating it.
    #[test]
    fn adding_a_model_appends_to_models_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            r#"{
              // hand-written notes must survive
              "providers": {
                "openrouter": {
                  "baseUrl": "https://openrouter.ai/api/v1",
                  "compat": { "supportsReasoningEffort": true },
                  "models": [{ "id": "openai/gpt-5" }]
                }
              }
            }"#,
        )
        .unwrap();
        let cfg = Config {
            source: Some(dir.path().to_path_buf()),
            ..Config::default()
        };

        let reference = cfg
            .add_model(&NewModel {
                base_url: "https://api.deepseek.com/v1/".into(),
                api_key: "sk-test".into(),
                model_id: "deepseek-chat".into(),
                context_window: 128_000,
                max_tokens: Some(8_192),
            })
            .unwrap();
        assert_eq!(reference, "deepseek/deepseek-chat");

        let saved: Value =
            parse_jsonc(&std::fs::read_to_string(dir.path().join("models.json")).unwrap()).unwrap();
        let provider = &saved["providers"]["deepseek"];
        assert_eq!(provider["baseUrl"], "https://api.deepseek.com/v1");
        assert_eq!(provider["api"], "openai-completions");
        assert_eq!(provider["apiKey"], "sk-test");
        assert_eq!(provider["models"][0]["id"], "deepseek-chat");
        assert_eq!(provider["models"][0]["contextWindow"], 128_000);
        assert_eq!(provider["models"][0]["maxTokens"], 8_192);
        // Untouched provider and its compat flags are still there.
        assert_eq!(
            saved["providers"]["openrouter"]["compat"]["supportsReasoningEffort"],
            true
        );
        assert_eq!(
            saved["providers"]["openrouter"]["models"][0]["id"],
            "openai/gpt-5"
        );

        // Adding the same id again replaces it, and an unknown context window
        // writes no key at all.
        cfg.add_model(&NewModel {
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: String::new(),
            model_id: "deepseek-chat".into(),
            context_window: 0,
            max_tokens: None,
        })
        .unwrap();
        let saved: Value =
            parse_jsonc(&std::fs::read_to_string(dir.path().join("models.json")).unwrap()).unwrap();
        let models = saved["providers"]["deepseek"]["models"].as_array().unwrap();
        assert_eq!(models.len(), 1, "no duplicate entry");
        assert!(models[0].get("contextWindow").is_none());
        assert_eq!(
            saved["providers"]["deepseek"]["apiKey"], "sk-test",
            "an empty key does not erase the stored one"
        );
    }

    #[test]
    fn provider_ids_come_from_the_endpoint_host() {
        assert_eq!(
            provider_id_from_url("https://api.deepseek.com/v1"),
            "deepseek"
        );
        assert_eq!(
            provider_id_from_url("https://openrouter.ai/api/v1"),
            "openrouter"
        );
        assert_eq!(
            provider_id_from_url("http://127.0.0.1:11434/v1"),
            "127-0-0-1-11434"
        );
        assert_eq!(
            provider_id_from_url("http://localhost:8080/v1"),
            "localhost"
        );
        assert_eq!(provider_id_from_url(""), "custom");
    }

    #[test]
    fn split_files_merge_and_select_models() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
              "model": "ollama/qwen2.5-coder:7b",
              "compactModel": "ollama/qwen2.5-coder:7b"
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            r#"{
              "providers": {
                "ollama": {
                  "baseUrl": "http://127.0.0.1:11434/v1",
                  "apiKey": "ollama",
                  "models": [
                    { "id": "qwen2.5-coder:7b", "contextWindow": 32768 }
                  ]
                }
              }
            }"#,
        )
        .unwrap();

        let cfg = Config::load_from_dir(dir.path()).expect("split config loads");
        assert_eq!(cfg.model, "ollama/qwen2.5-coder:7b");
        assert_eq!(cfg.compact_model, "ollama/qwen2.5-coder:7b");
        let spec = cfg.resolve_model(&cfg.model).unwrap();
        assert_eq!(spec.provider, "ollama");
        assert_eq!(spec.context_window, 32_768);
        // `models.json` is authoritative: only providers declared there exist.
        assert!(cfg.providers.contains_key("ollama"));
        assert!(!cfg.providers.contains_key("openai"));
    }
}
