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

/// One image attached to a message, base64 encoded exactly as providers and
/// pi's session format store it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImagePart {
    pub mime_type: String,
    /// Base64 payload without the `data:` prefix.
    pub data: String,
}

impl ImagePart {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.data)
    }

    pub fn from_data_url(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("data:")?;
        let (meta, data) = rest.split_once(',')?;
        let mime_type = meta.strip_suffix(";base64").unwrap_or(meta);
        Some(Self {
            mime_type: mime_type.to_string(),
            data: data.to_string(),
        })
    }

    /// Rough decoded size, for display only.
    pub fn bytes(&self) -> usize {
        self.data.len() * 3 / 4
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    /// Images carried with the message. Kept apart from `content` so the plain
    /// text path stays a `String`, and emitted as content parts on the wire.
    pub images: Vec<ImagePart>,
}

/// Content parts, in the OpenAI-compatible shape every provider we talk to
/// accepts for user messages.
#[derive(Serialize)]
#[serde(tag = "type")]
enum OutPart<'a> {
    #[serde(rename = "text")]
    Text { text: &'a str },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OutImageUrl },
}

#[derive(Serialize)]
struct OutImageUrl {
    url: String,
}

impl Serialize for ChatMessage {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("role", &self.role)?;
        // Only user messages are sent as a parts array: tool and assistant
        // content is a plain string in every gateway we target.
        if self.role == Role::User && !self.images.is_empty() {
            let mut parts: Vec<OutPart<'_>> = Vec::new();
            if let Some(text) = self.content.as_deref() {
                if !text.is_empty() {
                    parts.push(OutPart::Text { text });
                }
            }
            for image in &self.images {
                parts.push(OutPart::ImageUrl {
                    image_url: OutImageUrl {
                        url: image.data_url(),
                    },
                });
            }
            map.serialize_entry("content", &parts)?;
        } else if let Some(content) = &self.content {
            map.serialize_entry("content", content)?;
        }
        if let Some(reasoning) = &self.reasoning_content {
            map.serialize_entry("reasoning_content", reasoning)?;
        }
        if let Some(calls) = &self.tool_calls {
            map.serialize_entry("tool_calls", calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", id)?;
        }
        if let Some(name) = &self.name {
            map.serialize_entry("name", name)?;
        }
        map.end()
    }
}

#[derive(Deserialize)]
struct ChatMessageWire {
    role: Role,
    #[serde(default)]
    content: Option<WireContent>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WireContent {
    Text(String),
    Parts(Vec<WirePart>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum WirePart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: WireImageUrl },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct WireImageUrl {
    url: String,
}

impl<'de> Deserialize<'de> for ChatMessage {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ChatMessageWire::deserialize(deserializer)?;
        let mut content = None;
        let mut images = Vec::new();
        match wire.content {
            Some(WireContent::Text(text)) => content = Some(text),
            Some(WireContent::Parts(parts)) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        WirePart::Text { text: chunk } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(&chunk);
                        }
                        WirePart::ImageUrl { image_url } => {
                            if let Some(image) = ImagePart::from_data_url(&image_url.url) {
                                images.push(image);
                            }
                        }
                        WirePart::Unknown => {}
                    }
                }
                if !text.is_empty() {
                    content = Some(text);
                }
            }
            None => {}
        }
        Ok(Self {
            role: wire.role,
            content,
            reasoning_content: wire.reasoning_content,
            tool_calls: wire.tool_calls,
            tool_call_id: wire.tool_call_id,
            name: wire.name,
            images,
        })
    }
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

    pub fn user_with_images(content: impl Into<String>, images: Vec<ImagePart>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            images,
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

impl Usage {
    /// Cache hit ratio of this report, when the provider exposes cached prompt
    /// tokens. Percent (0-100) of prompt tokens served from cache.
    pub fn cache_hit_pct(&self) -> Option<u64> {
        if self.prompt_tokens == 0 || self.cached_tokens == 0 {
            return None;
        }
        Some(((self.cached_tokens * 100) / self.prompt_tokens).min(100))
    }

    /// Tokens occupying the context window after this request.
    pub fn context_tokens(&self) -> u64 {
        if self.total_tokens > 0 {
            self.total_tokens
        } else {
            self.prompt_tokens + self.completion_tokens
        }
    }
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

    fn body(&self, request: &ChatRequest) -> Value {
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
        if request.stream && self.compat.stream_usage {
            body.insert("stream_options".into(), json!({ "include_usage": true }));
        }
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if let Some(max_tokens) = request.max_tokens {
            match self.compat.max_tokens_field {
                MaxTokensField::MaxTokens => {
                    body.insert("max_tokens".into(), json!(max_tokens));
                }
                MaxTokensField::MaxCompletionTokens => {
                    body.insert("max_completion_tokens".into(), json!(max_tokens));
                }
                MaxTokensField::Omit => {}
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
            if self.compat.tool_choice {
                body.insert("tool_choice".into(), json!("auto"));
            }
            if self.compat.parallel_tool_calls {
                body.insert("parallel_tool_calls".into(), json!(true));
            }
        }
        Value::Object(body)
    }
}

/// Routes each request to the provider that owns the selected model, so the
/// agent and compaction models can live on different providers in one session.
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

impl WireUsage {
    fn into_usage(self) -> Usage {
        Usage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            cached_tokens: self
                .cached_tokens
                .or_else(|| {
                    self.prompt_tokens_details
                        .as_ref()
                        .and_then(|details| details.cached_tokens)
                })
                .unwrap_or(0),
            cost_usd: self.cost_usd,
        }
    }
}

/// A non-streaming Chat Completions response.
#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    choices: Vec<WireResponseChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireResponseChoice {
    #[serde(default)]
    message: WireResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireResponseMessage {
    #[serde(default)]
    content: Option<WireContent>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireResponseToolCall>>,
}

#[derive(Debug, Deserialize)]
struct WireResponseToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunction>,
}

impl WireResponse {
    fn into_completion(self) -> Completion {
        let mut completion = Completion::default();
        if let Some(usage) = self.usage {
            completion.usage = Some(usage.into_usage());
        }
        let mut call_index = 0usize;
        for choice in self.choices {
            if let Some(reason) = choice.finish_reason {
                completion.finish_reason = Some(reason);
            }
            if let Some(content) = choice.message.content {
                completion.content.push_str(&wire_content_text(content));
            }
            if let Some(reasoning) = choice.message.reasoning_content {
                completion.reasoning.push_str(&reasoning);
            }
            for call in choice.message.tool_calls.unwrap_or_default() {
                let WireResponseToolCall { id, function } = call;
                let Some(function) = function else {
                    continue;
                };
                let name = function.name.unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let id = id
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| format!("call_{call_index}"));
                call_index += 1;
                completion.tool_calls.push(ToolCall {
                    id,
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: function.arguments.unwrap_or_default(),
                    },
                });
            }
        }
        completion
    }
}

fn wire_content_text(content: WireContent) -> String {
    match content {
        WireContent::Text(text) => text,
        WireContent::Parts(parts) => parts
            .into_iter()
            .filter_map(|part| match part {
                WirePart::Text { text } => Some(text),
                WirePart::ImageUrl { .. } | WirePart::Unknown => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[async_trait::async_trait]
impl Backend for Provider {
    async fn stream_chat(
        &self,
        request: ChatRequest,
        sink: Option<UnboundedSender<StreamEvent>>,
        cancel: CancellationToken,
    ) -> Result<Completion> {
        self.send_once(&request, sink.as_ref(), &cancel).await
    }
}

impl Provider {
    async fn send_once(
        &self,
        request: &ChatRequest,
        sink: Option<&UnboundedSender<StreamEvent>>,
        cancel: &CancellationToken,
    ) -> Result<Completion> {
        let body = self.body(request);
        let chat = self.client.chat();
        if request.stream {
            // Guard the request itself with cancellation: the client awaits the
            // response headers before the first event, which is where a slow or
            // rate-limited provider stalls.
            let request_future = chat.create_stream_byot::<Value, WireChunk>(body);
            let created = tokio::select! {
                _ = cancel.cancelled() => return Err(HarnessError::Cancelled),
                result = request_future => result,
            };
            drop(chat);
            let stream = created.map_err(|err| self.provider_error(err))?;

            let mut completion = Completion::default();
            let mut calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
            let mut stream = stream;
            loop {
                let item = tokio::select! {
                    _ = cancel.cancelled() => return Err(HarnessError::Cancelled),
                    item = stream.next() => item,
                };
                let Some(item) = item else { break };
                let chunk = item.map_err(|err| self.provider_error(err))?;
                if let Some(usage) = chunk.usage {
                    completion.usage = Some(usage.into_usage());
                }
                for choice in chunk.choices {
                    if let Some(reason) = choice.finish_reason {
                        completion.finish_reason = Some(reason);
                    }
                    if let Some(text) = choice.delta.content {
                        if !text.is_empty() {
                            completion.content.push_str(&text);
                            if let Some(sink) = sink {
                                let _ = sink.send(StreamEvent::Delta(text));
                            }
                        }
                    }
                    if let Some(reasoning) = choice.delta.reasoning_content {
                        if !reasoning.is_empty() {
                            completion.reasoning.push_str(&reasoning);
                            if let Some(sink) = sink {
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

        let request_future = chat.create_byot::<Value, WireResponse>(body);
        let created = tokio::select! {
            _ = cancel.cancelled() => return Err(HarnessError::Cancelled),
            result = request_future => result,
        };
        drop(chat);
        let response = created.map_err(|err| self.provider_error(err))?;
        Ok(response.into_completion())
    }

    fn provider_error(&self, err: impl std::fmt::Display) -> HarnessError {
        HarnessError::Provider(format!("{}: {err}", self.name))
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
    fn optional_compat_fields_are_off_by_default() {
        let p = provider();
        let mut request = ChatRequest::new("m", vec![ChatMessage::user("x")]);
        request.stream = true;
        request.tools = vec![ToolDefinition {
            name: "read".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];
        let body = p.body(&request);
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn body_honours_compat_switches() {
        let mut p = provider();
        p.compat.max_tokens_field = MaxTokensField::MaxCompletionTokens;
        p.compat.system_role = SystemRole::Developer;
        p.compat.reasoning_content = true;
        p.compat.tool_choice = true;
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
        let body = p.body(&request);
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
        assert!(p.body(&request)["messages"][0]
            .get("reasoning_content")
            .is_none());
        p.compat.reasoning_content = true;
        assert_eq!(p.body(&request)["messages"][0]["reasoning_content"], "why");
    }

    /// Serve one HTTP request and return the bound base URL. The caller only
    /// needs the response, so the request body is drained but not parsed.
    async fn one_shot_json_server(response: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        format!("http://{address}/v1")
    }

    #[tokio::test]
    async fn non_streaming_completion_parses_json_responses() {
        let base_url = one_shot_json_server(
            r#"{
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "hello from json",
                        "tool_calls": [{
                            "id": "call_abc",
                            "type": "function",
                            "function": {"name": "read", "arguments": "{\"path\":\"a.txt\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 3,
                    "completion_tokens": 4,
                    "total_tokens": 7,
                    "prompt_tokens_details": {"cached_tokens": 2}
                }
            }"#,
        )
        .await;
        let provider = Provider::from_provider(
            &ProviderConfig {
                base_url,
                ..Default::default()
            },
            "test-key".into(),
        )
        .unwrap();

        let completion = provider
            .complete(ChatRequest::new("m", vec![ChatMessage::user("hi")]))
            .await
            .unwrap();

        assert_eq!(completion.content, "hello from json");
        assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].id, "call_abc");
        assert_eq!(completion.tool_calls[0].function.name, "read");
        let usage = completion.usage.expect("usage");
        assert_eq!(usage.total_tokens, 7);
        assert_eq!(usage.cached_tokens, 2);
    }
}
