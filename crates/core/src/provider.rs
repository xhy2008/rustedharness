use std::collections::BTreeMap;
use std::sync::Arc;

use async_openai::{config::OpenAIConfig, Client};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::config::{Compat, Config, MaxTokensField, ProviderConfig, SystemRole};
use crate::error::{HarnessError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn assistant_tool_calls(content: Option<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: Some(calls),
            ..Default::default()
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    /// Approximate textual footprint, used by the token estimator.
    pub fn text_len(&self) -> usize {
        let mut n = self.content.as_deref().map(str::len).unwrap_or(0);
        n += self.reasoning_content.as_deref().map(str::len).unwrap_or(0);
        if let Some(calls) = &self.tool_calls {
            for c in calls {
                n += c.function.name.len() + c.function.arguments.len();
            }
        }
        n
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            stream: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Prompt tokens served from the provider's prompt cache, as reported in
    /// `usage` (OpenAI `prompt_tokens_details.cached_tokens`, DeepSeek
    /// `prompt_cache_hit_tokens`, Anthropic-style `cache_read_input_tokens`).
    #[serde(default)]
    pub cached_tokens: u64,
    /// Cost reported by the provider inside `usage` (some gateways return
    /// `cost`/`total_cost` alongside the token counts).
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    Reasoning(String),
}

/// Pluggable backend so the agent loop can be tested without a network.
#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    async fn stream_chat(
        &self,
        request: ChatRequest,
        sink: Option<UnboundedSender<StreamEvent>>,
        cancel: CancellationToken,
    ) -> Result<Completion>;

    async fn complete(&self, request: ChatRequest) -> Result<Completion> {
        let request = ChatRequest {
            stream: false,
            ..request
        };
        self.stream_chat(request, None, CancellationToken::new())
            .await
    }
}

#[derive(Clone)]
pub struct Provider {
    client: Client<OpenAIConfig>,
    compat: Compat,
    name: String,
}

impl Provider {
    pub fn from_config(config: &Config) -> Result<Self> {
        let spec = config.resolve_model(&config.model)?;
        let provider = config
            .providers
            .get(&spec.provider)
            .ok_or_else(|| HarnessError::Config(format!("unknown provider `{}`", spec.provider)))?;
        Self::from_provider(
            provider,
            config.resolve_api_key_for_provider(&spec.provider),
        )
        .map(|provider| provider.with_name(spec.provider))
    }

    pub fn from_provider(provider: &ProviderConfig, api_key: String) -> Result<Self> {
        if provider.base_url.trim().is_empty() {
            return Err(HarnessError::Config(
                "provider baseUrl is empty; add it to models.json".to_string(),
            ));
        }
        let api = provider.api.trim().to_ascii_lowercase();
        if !api.is_empty() && api != "openai-completions" && api != "openai" {
            return Err(HarnessError::Config(format!(
                "unsupported provider api `{}`; the completions backend expects `openai-completions`",
                provider.api
            )));
        }
        let base_url = provider.base_url.trim_end_matches('/').to_string();
        let mut oa = OpenAIConfig::new()
            .with_api_base(base_url)
            .with_api_key(api_key);
        for (key, value) in provider.resolved_headers() {
            let header = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|e| HarnessError::Config(format!("invalid header name {key}: {e}")))?;
            oa = oa
                .with_header(header, value)
                .map_err(|e| HarnessError::Config(format!("invalid header {key}: {e}")))?;
        }
        let client = Client::with_config(oa);
        Ok(Self {
            client,
            compat: provider.compat.clone(),
            name: "provider".to_string(),
        })
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn compat(&self) -> &Compat {
        &self.compat
    }

    fn body(&self, request: &ChatRequest, degraded_without: Option<&str>) -> Value {
        let mut messages = Vec::with_capacity(request.messages.len());
        for (i, message) in request.messages.iter().enumerate() {
            let mut out = serde_json::Map::new();
            let role = if i == 0 && message.role == Role::System {
                match self.compat.system_role {
                    SystemRole::System => "system",
                    SystemRole::Developer => "developer",
                }
            } else {
                match message.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                }
            };
            out.insert("role".into(), json!(role));
            if let Some(content) = &message.content {
                out.insert("content".into(), json!(content));
            } else if message.role == Role::Assistant && message.tool_calls.is_some() {
                out.insert("content".into(), Value::Null);
            }
            if self.compat.reasoning_content {
                if let Some(reasoning) = &message.reasoning_content {
                    out.insert("reasoning_content".into(), json!(reasoning));
                }
            }
            if let Some(calls) = &message.tool_calls {
                let calls: Vec<Value> = calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.function.name,
                                "arguments": c.function.arguments,
                            }
                        })
                    })
                    .collect();
                out.insert("tool_calls".into(), Value::Array(calls));
            }
            if let Some(id) = &message.tool_call_id {
                out.insert("tool_call_id".into(), json!(id));
            }
            if let Some(name) = &message.name {
                out.insert("name".into(), json!(name));
            }
            messages.push(Value::Object(out));
        }

        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert("messages".into(), Value::Array(messages));
        body.insert("stream".into(), json!(request.stream));
        if request.stream && self.compat.stream_usage && degraded_without != Some("stream_usage") {
            body.insert("stream_options".into(), json!({ "include_usage": true }));
        }
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if let Some(max_tokens) = request.max_tokens {
            match self.compat.max_tokens_field {
                MaxTokensField::MaxTokens if degraded_without != Some("max_tokens") => {
                    body.insert("max_tokens".into(), json!(max_tokens));
                }
                MaxTokensField::MaxCompletionTokens
                    if degraded_without != Some("max_completion_tokens") =>
                {
                    body.insert("max_completion_tokens".into(), json!(max_tokens));
                }
                _ => {}
            }
        }
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
            if self.compat.tool_choice && degraded_without != Some("tool_choice") {
                body.insert("tool_choice".into(), json!("auto"));
            }
            if self.compat.parallel_tool_calls && degraded_without != Some("parallel_tool_calls") {
                body.insert("parallel_tool_calls".into(), json!(true));
            }
        }
        Value::Object(body)
    }

    /// Drop compatibility knobs that a gateway rejected, then retry once each.
    fn degradation_candidates(&self, request: &ChatRequest) -> Vec<Option<&'static str>> {
        let mut out = vec![None];
        if self.compat.parallel_tool_calls {
            out.push(Some("parallel_tool_calls"));
        }
        if self.compat.tool_choice {
            out.push(Some("tool_choice"));
        }
        if self.compat.stream_usage {
            out.push(Some("stream_usage"));
        }
        if request.max_tokens.is_some() {
            out.push(Some("max_tokens"));
            out.push(Some("max_completion_tokens"));
        }
        out
    }
}

/// Routes each request to the provider that owns the selected model. This is
/// what makes `models.agent` and `models.compact` able to live on different
/// providers in the same session.
#[derive(Clone)]
pub struct Router {
    config: Config,
    providers: BTreeMap<String, Arc<dyn Backend>>,
    /// Providers that could not be constructed, with the reason. They are kept
    /// out of the way so one unsupported entry does not break the session.
    unusable: BTreeMap<String, String>,
    default_provider: String,
}

impl Router {
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut providers: BTreeMap<String, Arc<dyn Backend>> = BTreeMap::new();
        let mut unusable = BTreeMap::new();
        for (id, provider) in &config.providers {
            // An empty baseUrl means the provider is intentionally disabled.
            if provider.base_url.trim().is_empty() {
                unusable.insert(
                    id.clone(),
                    format!("provider `{id}` has no baseUrl in models.json"),
                );
                continue;
            }
            let api_key = config.resolve_api_key_for_provider(id);
            let backend = match provider.api.trim().to_ascii_lowercase().as_str() {
                "" | "openai" | "openai-completions" => {
                    Provider::from_provider(provider, api_key)
                        .map(|backend| Arc::new(backend.with_name(id.clone())) as Arc<dyn Backend>)
                }
                "openai-responses" | "responses" => {
                    crate::responses::ResponsesProvider::from_provider(provider, api_key).map(
                        |backend| Arc::new(backend.with_name(id.clone())) as Arc<dyn Backend>,
                    )
                }
                other => Err(HarnessError::Config(format!(
                    "provider `{id}` uses unsupported api `{other}`; use `openai-completions` or `openai-responses`"
                ))),
            };
            match backend {
                Ok(backend) => {
                    providers.insert(id.clone(), backend);
                }
                Err(err) => {
                    unusable.insert(id.clone(), err.to_string());
                }
            }
        }
        if providers.is_empty() {
            let detail = unusable
                .values()
                .next()
                .cloned()
                .unwrap_or_else(|| "no providers configured".to_string());
            return Err(HarnessError::Config(format!(
                "no usable model provider: {detail}"
            )));
        }
        let default_provider = config
            .resolve_model(&config.model)
            .map(|model| model.provider)
            .unwrap_or_else(|_| config.default_provider_id());
        if !providers.contains_key(&default_provider) {
            let reason = unusable
                .get(&default_provider)
                .cloned()
                .unwrap_or_else(|| format!("provider `{default_provider}` is not configured"));
            return Err(HarnessError::Config(format!(
                "cannot use model `{}`: {reason}",
                config.model
            )));
        }
        Ok(Self {
            config: config.clone(),
            providers,
            unusable,
            default_provider,
        })
    }

    fn resolve(&self, reference: &str) -> Result<(String, String)> {
        if let Ok(spec) = self.config.resolve_model(reference) {
            return Ok((spec.provider, spec.id));
        }
        Ok((self.default_provider.clone(), reference.to_string()))
    }
}

#[async_trait::async_trait]
impl Backend for Router {
    async fn stream_chat(
        &self,
        mut request: ChatRequest,
        sink: Option<UnboundedSender<StreamEvent>>,
        cancel: CancellationToken,
    ) -> Result<Completion> {
        let (provider_id, model_id) = self.resolve(&request.model)?;
        let provider = self.providers.get(&provider_id).ok_or_else(|| {
            let reason = self
                .unusable
                .get(&provider_id)
                .cloned()
                .unwrap_or_else(|| format!("provider `{provider_id}` is not configured"));
            HarnessError::Config(format!("cannot use model `{}`: {reason}", request.model))
        })?;
        request.model = model_id;
        provider.stream_chat(request, sink, cancel).await
    }
}

#[derive(Debug, Deserialize)]
struct WireChunk {
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    #[serde(default)]
    delta: WireDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Debug, Deserialize, Clone)]
struct WireToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunction>,
}

#[derive(Debug, Deserialize, Clone)]
struct WireFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireUsageDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default, alias = "cost", alias = "total_cost")]
    cost_usd: Option<f64>,
    #[serde(default)]
    prompt_tokens_details: Option<WireUsageDetails>,
    #[serde(
        default,
        alias = "cached_tokens",
        alias = "prompt_cache_hit_tokens",
        alias = "cache_read_input_tokens"
    )]
    cached_tokens: Option<u64>,
}

#[async_trait::async_trait]
impl Backend for Provider {
    async fn stream_chat(
        &self,
        request: ChatRequest,
        sink: Option<UnboundedSender<StreamEvent>>,
        cancel: CancellationToken,
    ) -> Result<Completion> {
        let candidates = self.degradation_candidates(&request);
        let mut last_error = String::new();

        for degraded in candidates {
            let body = self.body(&request, degraded);
            // Guard the request itself with cancellation: `create_stream_byot`
            // awaits the response headers, which is where a slow or rate-limited
            // provider stalls — without this the cancel token cannot interrupt
            // until the first chunk arrives.
            let chat = self.client.chat();
            let request_future = chat.create_stream_byot::<Value, WireChunk>(body);
            let created = tokio::select! {
                _ = cancel.cancelled() => return Err(HarnessError::Cancelled),
                result = request_future => result,
            };
            drop(chat);
            let stream = match created {
                Ok(stream) => stream,
                Err(err) => {
                    let text = err.to_string();
                    if is_compat_rejection(&text) {
                        last_error = text;
                        continue;
                    }
                    return Err(HarnessError::Provider(format!("{}: {text}", self.name)));
                }
            };

            let mut completion = Completion::default();
            let mut calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
            let mut stream = stream;
            loop {
                let item = tokio::select! {
                    _ = cancel.cancelled() => return Err(HarnessError::Cancelled),
                    item = stream.next() => item,
                };
                let Some(item) = item else { break };
                let chunk = match item {
                    Ok(chunk) => chunk,
                    Err(err) => return Err(HarnessError::Provider(err.to_string())),
                };
                if let Some(usage) = chunk.usage {
                    completion.usage = Some(Usage {
                        prompt_tokens: usage.prompt_tokens,
                        completion_tokens: usage.completion_tokens,
                        total_tokens: usage.total_tokens,
                        cached_tokens: usage
                            .cached_tokens
                            .or_else(|| {
                                usage
                                    .prompt_tokens_details
                                    .as_ref()
                                    .and_then(|d| d.cached_tokens)
                            })
                            .unwrap_or(0),
                        cost_usd: usage.cost_usd,
                    });
                }
                for choice in chunk.choices {
                    if let Some(reason) = choice.finish_reason {
                        completion.finish_reason = Some(reason);
                    }
                    if let Some(text) = choice.delta.content {
                        if !text.is_empty() {
                            completion.content.push_str(&text);
                            if let Some(sink) = &sink {
                                let _ = sink.send(StreamEvent::Delta(text));
                            }
                        }
                    }
                    if let Some(reasoning) = choice.delta.reasoning_content {
                        if !reasoning.is_empty() {
                            completion.reasoning.push_str(&reasoning);
                            if let Some(sink) = &sink {
                                let _ = sink.send(StreamEvent::Reasoning(reasoning));
                            }
                        }
                    }
                    if let Some(tool_calls) = choice.delta.tool_calls {
                        for call in tool_calls {
                            let entry = calls.entry(call.index).or_default();
                            if let Some(id) = call.id {
                                if !id.is_empty() {
                                    entry.0 = id;
                                }
                            }
                            if let Some(function) = call.function {
                                if let Some(name) = function.name {
                                    if !name.is_empty() {
                                        entry.1.push_str(&name);
                                    }
                                }
                                if let Some(args) = function.arguments {
                                    entry.2.push_str(&args);
                                }
                            }
                        }
                    }
                }
            }
            completion.tool_calls = calls
                .into_values()
                .filter(|(_, name, _)| !name.is_empty())
                .enumerate()
                .map(|(i, (id, name, arguments))| ToolCall {
                    id: if id.is_empty() {
                        format!("call_{i}")
                    } else {
                        id
                    },
                    kind: "function".to_string(),
                    function: FunctionCall { name, arguments },
                })
                .collect();
            return Ok(completion);
        }

        Err(HarnessError::Provider(format!(
            "{}: all compatibility fallbacks failed: {last_error}",
            self.name
        )))
    }
}

/// True when a provider error is a retryable throughput limit (HTTP 429 /
/// rate limit / throttling) rather than an account-state failure.
pub fn is_rate_limited(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("too many requests")
        || m.contains("rate limit")
        || m.contains("ratelimit")
        || m.contains("throttl")
        || m.contains("status code: 429")
        || m.contains("status code 429")
        || m.contains("status: 429")
        || m.contains("429 too many")
        || m.contains("resource_exhausted")
        || m.contains("quota exceeded per minute")
}

/// True when the account itself is out of money/credits: retrying can never
/// succeed, so callers must stop instead of spinning on 429s.
pub fn is_quota_exhausted(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("insufficient_quota")
        || m.contains("insufficient quota")
        || m.contains("insufficient balance")
        || m.contains("out of funds")
        || m.contains("out of credit")
        || m.contains("billing")
        || m.contains("arrears")
        || m.contains("payment required")
        || m.contains("add funds")
        || m.contains("top up")
        || m.contains("购买")
        || m.contains("余额")
        || m.contains("欠费")
}

pub(crate) fn is_compat_rejection(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("max_tokens")
        || m.contains("max_completion_tokens")
        || m.contains("tool_choice")
        || m.contains("parallel_tool_calls")
        || m.contains("stream_options")
        || m.contains("400")
        || m.contains("422")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProviderConfig};

    #[test]
    fn wire_usage_accepts_cache_token_aliases() {
        let openai: WireUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,
                "prompt_tokens_details":{"cached_tokens":96}}"#,
        )
        .unwrap();
        assert_eq!(openai.cached_tokens, None);
        assert_eq!(
            openai
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            Some(96)
        );
        let deepseek: WireUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":5,
                "prompt_cache_hit_tokens":88}"#,
        )
        .unwrap();
        assert_eq!(deepseek.cached_tokens, Some(88));
        let anthropic: WireUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":5,
                "cache_read_input_tokens":40}"#,
        )
        .unwrap();
        assert_eq!(anthropic.cached_tokens, Some(40));
    }

    #[test]
    fn rate_limit_and_quota_are_distinguished() {
        assert!(is_rate_limited("HTTP status code 429 Too Many Requests"));
        assert!(is_rate_limited("api error: rate limit exceeded for key"));
        assert!(is_rate_limited("RESOURCE_EXHAUSTED: capped"));
        assert!(is_rate_limited("429: Too Many Requests"));
        // Insufficient credits is ALSO a 429 but must never be retried.
        assert!(is_quota_exhausted(
            "{\"error\":{\"code\":\"insufficient_quota\",\"message\":\"You exceeded your current quota\"}}"
        ));
        assert!(is_quota_exhausted("余额不足，请充值"));
        assert!(!is_quota_exhausted("Too Many Requests"));
        assert!(!is_rate_limited("500 internal server error"));
    }

    fn provider() -> Provider {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "mock".into(),
            ProviderConfig {
                base_url: "http://127.0.0.1:1/v1".into(),
                ..Default::default()
            },
        );
        cfg.provider = "mock".into();
        cfg.model = "mock/m".into();
        Provider::from_config(&cfg).unwrap()
    }

    #[test]
    fn router_selects_the_provider_that_owns_each_model() {
        let cfg: Config = serde_json::from_str(
            r#"{
              "model": "local/small",
              "providers": {
                "local": {
                  "baseUrl": "http://127.0.0.1:11434/v1",
                  "models": [{ "id": "small" }]
                },
                "remote": {
                  "baseUrl": "https://example.com/v1",
                  "models": [{ "id": "large" }]
                }
              }
            }"#,
        )
        .unwrap();
        let router = Router::from_config(&cfg).unwrap();
        assert_eq!(
            router.resolve("local/small").unwrap(),
            ("local".to_string(), "small".to_string())
        );
        assert_eq!(
            router.resolve("remote/large").unwrap(),
            ("remote".to_string(), "large".to_string())
        );
        assert_eq!(
            router.resolve("large").unwrap(),
            ("remote".to_string(), "large".to_string())
        );
    }

    #[test]
    fn router_ignores_unusable_providers_until_they_are_selected() {
        let cfg: Config = serde_json::from_str(
            r#"{
              "model": "local/small",
              "providers": {
                "local": {
                  "baseUrl": "http://127.0.0.1:11434/v1",
                  "models": [{ "id": "small" }]
                },
                "claude": {
                  "baseUrl": "https://api.anthropic.com/v1",
                  "api": "anthropic-messages",
                  "models": [{ "id": "claude-sonnet-4" }]
                },
                "disabled": {
                  "models": [{ "id": "ghost" }]
                }
              }
            }"#,
        )
        .unwrap();
        // The unsupported and disabled entries do not prevent startup.
        let router = Router::from_config(&cfg).unwrap();
        assert!(router.providers.contains_key("local"));
        assert!(!router.providers.contains_key("claude"));
        assert!(router.unusable["claude"].contains("anthropic-messages"));
        assert!(router.unusable["disabled"].contains("baseUrl"));
    }

    #[test]
    fn router_builds_the_responses_backend() {
        let cfg: Config = serde_json::from_str(
            r#"{
              "model": "gpt/gpt-5",
              "providers": {
                "gpt": {
                  "baseUrl": "https://api.openai.com/v1",
                  "api": "openai-responses",
                  "models": [{ "id": "gpt-5" }]
                }
              }
            }"#,
        )
        .unwrap();
        let router = Router::from_config(&cfg).unwrap();
        assert!(router.providers.contains_key("gpt"));
        assert!(router.unusable.is_empty());
    }

    #[test]
    fn router_fails_when_the_selected_provider_is_unusable() {
        let cfg: Config = serde_json::from_str(
            r#"{
              "model": "claude/claude-sonnet-4",
              "providers": {
                "local": {
                  "baseUrl": "http://127.0.0.1:11434/v1",
                  "models": [{ "id": "small" }]
                },
                "claude": {
                  "baseUrl": "https://api.anthropic.com/v1",
                  "api": "anthropic-messages",
                  "models": [{ "id": "claude-sonnet-4" }]
                }
              }
            }"#,
        )
        .unwrap();
        let err = match Router::from_config(&cfg) {
            Ok(_) => panic!("expected the unusable provider to be rejected"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("claude"), "{err}");
        assert!(err.contains("anthropic-messages"), "{err}");
    }

    #[test]
    fn body_honours_compat_switches() {
        let mut p = provider();
        p.compat.max_tokens_field = MaxTokensField::MaxCompletionTokens;
        p.compat.system_role = SystemRole::Developer;
        p.compat.reasoning_content = true;
        let mut request = ChatRequest::new(
            "m",
            vec![ChatMessage::system("sys"), ChatMessage::user("hi")],
        );
        request.max_tokens = Some(10);
        request.tools = vec![ToolDefinition {
            name: "read".into(),
            description: "d".into(),
            parameters: json!({"type":"object"}),
        }];
        let body = p.body(&request, None);
        assert_eq!(body["messages"][0]["role"], "developer");
        assert_eq!(body["max_completion_tokens"], 10);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn reasoning_content_is_round_tripped_only_when_enabled() {
        let mut p = provider();
        let mut assistant = ChatMessage::assistant("x");
        assistant.reasoning_content = Some("why".into());
        let request = ChatRequest::new("m", vec![assistant]);
        assert_eq!(
            p.body(&request, None)["messages"][0]["reasoning_content"],
            "why"
        );
        p.compat.reasoning_content = false;
        assert!(p.body(&request, None)["messages"][0]
            .get("reasoning_content")
            .is_none());
    }

    #[test]
    fn degraded_body_removes_fields() {
        let p = provider();
        let mut request = ChatRequest::new("m", vec![ChatMessage::user("x")]);
        request.max_tokens = Some(5);
        request.tools = vec![ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            parameters: json!({}),
        }];
        let body = p.body(&request, Some("tool_choice"));
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["parallel_tool_calls"], true);
        let body = p.body(&request, Some("max_tokens"));
        assert!(body.get("max_tokens").is_none());
    }
}
