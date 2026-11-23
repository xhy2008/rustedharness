//! OpenAI Responses API backend (`"api": "openai-responses"`).
//!
//! The agent speaks one internal message format; this module translates it to
//! the Responses API (`input` items, `instructions`, flat function tools) and
//! folds the SSE event stream back into the same [`Completion`] the Chat
//! Completions backend produces.
//!
//! Transport, SSE framing and event decoding use `async-openai`'s Responses
//! client in BYOT mode: the payload/event stay raw JSON so compatible gateways
//! that lag behind the OpenAI schema keep working.

use std::collections::BTreeMap;

use async_openai::{config::OpenAIConfig, Client};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::config::{Compat, ProviderConfig, ResolvedCompat};
use crate::error::{HarnessError, Result};
use crate::provider::{
    openai_client, Backend, ChatRequest, Completion, FunctionCall, Role, StreamEvent, ToolCall,
    Usage,
};

#[derive(Clone)]
pub struct ResponsesProvider {
    client: Client<OpenAIConfig>,
    compat: Compat,
    base_url: String,
    name: String,
}

impl ResponsesProvider {
    pub fn from_provider(provider: &ProviderConfig, api_key: String) -> Result<Self> {
        if provider.base_url.trim().is_empty() {
            return Err(HarnessError::Config(
                "provider baseUrl is empty; add it to models.json".to_string(),
            ));
        }
        if !provider.api.eq_ignore_ascii_case("openai-responses") {
            return Err(HarnessError::Config(format!(
                "unsupported provider api `{}`; expected `openai-responses`",
                provider.api
            )));
        }
        let base = provider.base_url.trim_end_matches('/');
        let client_base = base.strip_suffix("/responses").unwrap_or(base);
        Ok(Self {
            client: openai_client(provider, api_key, client_base)?,
            compat: provider.compat.clone(),
            base_url: provider.base_url.clone(),
            name: "provider".to_string(),
        })
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    fn resolved(&self) -> ResolvedCompat {
        self.compat
            .resolve("openai-responses", &self.name, &self.base_url)
    }

    fn body(&self, request: &ChatRequest) -> Value {
        let compat = self.resolved();
        let mut instructions: Option<String> = None;
        let mut input: Vec<Value> = Vec::with_capacity(request.messages.len());

        for (index, message) in request.messages.iter().enumerate() {
            match message.role {
                Role::System => {
                    let content = message.content.clone().unwrap_or_default();
                    if index == 0 && !content.is_empty() {
                        instructions = Some(content);
                    } else if !content.is_empty() {
                        let role = if compat.supports_developer_role {
                            "developer"
                        } else {
                            "system"
                        };
                        input.push(message_item(role, "input_text", &content));
                    }
                }
                Role::User => {
                    if let Some(content) = &message.content {
                        input.push(message_item("user", "input_text", content));
                    }
                }
                Role::Assistant => {
                    if let Some(content) = &message.content {
                        if !content.is_empty() {
                            input.push(message_item("assistant", "output_text", content));
                        }
                    }
                    if let Some(calls) = &message.tool_calls {
                        for call in calls {
                            input.push(json!({
                                "type": "function_call",
                                "call_id": call.id,
                                "name": call.function.name,
                                "arguments": call.function.arguments,
                            }));
                        }
                    }
                }
                Role::Tool => {
                    if let Some(call_id) = &message.tool_call_id {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": message.content.clone().unwrap_or_default(),
                        }));
                    } else if let Some(content) = &message.content {
                        // A tool result without an id cannot be replayed as a
                        // function_call_output; keep the text visible instead
                        // of silently dropping it.
                        input.push(message_item("user", "input_text", content));
                    }
                }
            }
        }

        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert("input".into(), Value::Array(input));
        body.insert("stream".into(), json!(request.stream));
        // Stateless by default: do not ask the server to retain the response.
        body.insert("store".into(), json!(false));
        if let Some(instructions) = instructions {
            body.insert("instructions".into(), json!(instructions));
        }
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if let Some(max_tokens) = request.max_tokens {
            body.insert("max_output_tokens".into(), json!(max_tokens));
        }
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        Value::Object(body)
    }

    async fn stream_response(
        &self,
        stream: async_openai::types::stream::StreamResponse<Value>,
        sink: Option<UnboundedSender<StreamEvent>>,
        cancel: CancellationToken,
    ) -> Result<Completion> {
        let mut acc = ResponsesAccumulator::default();
        let mut stream = stream;
        loop {
            let item = tokio::select! {
                _ = cancel.cancelled() => return Err(HarnessError::Cancelled),
                item = stream.next() => item,
            };
            let Some(item) = item else { break };
            let event = item
                .map_err(|e| HarnessError::Provider(format!("{}: stream error: {e}", self.name)))?;
            acc.apply(&event, &sink);
        }
        acc.finish(&self.name)
    }
}

fn message_item(role: &str, content_type: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": content_type, "text": text }],
    })
}

#[async_trait::async_trait]
impl Backend for ResponsesProvider {
    async fn stream_chat(
        &self,
        request: ChatRequest,
        sink: Option<UnboundedSender<StreamEvent>>,
        cancel: CancellationToken,
    ) -> Result<Completion> {
        let body = self.body(&request);
        let responses = self.client.responses();
        if request.stream {
            let stream = tokio::select! {
                _ = cancel.cancelled() => return Err(HarnessError::Cancelled),
                stream = responses.create_stream_byot::<Value, Value>(body) => stream,
            }
            .map_err(|e| HarnessError::Provider(format!("{}: {e}", self.name)))?;
            return self.stream_response(stream, sink, cancel).await;
        }

        let value: Value = tokio::select! {
            _ = cancel.cancelled() => return Err(HarnessError::Cancelled),
            value = responses.create_byot::<Value, Value>(body) => value,
        }
        .map_err(|e| HarnessError::Provider(format!("{}: {e}", self.name)))?;
        let mut acc = ResponsesAccumulator::default();
        acc.absorb_response(&value);
        acc.finish(&self.name)
    }
}

#[derive(Debug, Default, Clone)]
struct FunctionCallAccum {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct ResponsesAccumulator {
    content: String,
    reasoning: String,
    calls: BTreeMap<u32, FunctionCallAccum>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    error: Option<String>,
}

impl ResponsesAccumulator {
    fn apply(&mut self, event: &Value, sink: &Option<UnboundedSender<StreamEvent>>) {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.content.push_str(delta);
                        if let Some(sink) = sink {
                            let _ = sink.send(StreamEvent::Delta(delta.to_string()));
                        }
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.reasoning.push_str(delta);
                        if let Some(sink) = sink {
                            let _ = sink.send(StreamEvent::Reasoning(delta.to_string()));
                        }
                    }
                }
            }
            "response.output_text.done" => {
                if self.content.is_empty() {
                    if let Some(text) = event.get("text").and_then(Value::as_str) {
                        self.content = text.to_string();
                    }
                }
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                if self.reasoning.is_empty() {
                    if let Some(text) = event.get("text").and_then(Value::as_str) {
                        self.reasoning = text.to_string();
                    }
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                self.absorb_item(
                    output_index(event),
                    event.get("item").unwrap_or(&Value::Null),
                    kind == "response.output_item.done",
                );
            }
            "response.function_call_arguments.delta" => {
                let entry = self.calls.entry(output_index(event)).or_default();
                if entry.item_id.is_empty() {
                    if let Some(id) = event.get("item_id").and_then(Value::as_str) {
                        entry.item_id = id.to_string();
                    }
                }
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    entry.arguments.push_str(delta);
                }
            }
            "response.function_call_arguments.done" => {
                let entry = self.calls.entry(output_index(event)).or_default();
                if entry.item_id.is_empty() {
                    if let Some(id) = event.get("item_id").and_then(Value::as_str) {
                        entry.item_id = id.to_string();
                    }
                }
                if let Some(name) = event.get("name").and_then(Value::as_str) {
                    entry.name = name.to_string();
                }
                if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
                    entry.arguments = arguments.to_string();
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(response) = event.get("response") {
                    self.absorb_response(response);
                }
            }
            "error" => {
                let message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                self.error = Some(message.to_string());
            }
            _ => {}
        }
    }

    fn absorb_item(&mut self, index: u32, item: &Value, done: bool) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
        let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
        // Terminal events repeat items already seen through deltas. Reuse the
        // existing slot when the ids match even if the index shifted.
        let existing = self.calls.iter().find_map(|(slot, call)| {
            ((!call_id.is_empty() && call.call_id == call_id)
                || (!item_id.is_empty() && call.item_id == item_id))
                .then_some(*slot)
        });
        let entry = self.calls.entry(existing.unwrap_or(index)).or_default();
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            entry.item_id = id.to_string();
        }
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
            entry.call_id = call_id.to_string();
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            entry.name = name.to_string();
        }
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
            if done || entry.arguments.is_empty() {
                entry.arguments = arguments.to_string();
            }
        }
    }

    /// Fold a complete `response` object (non-stream body or a terminal event)
    /// into the accumulator. The `output` array is authoritative for tool calls
    /// and is used as a fallback for text.
    fn absorb_response(&mut self, response: &Value) {
        if let Some(error) = response.get("error").filter(|value| !value.is_null()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("response failed");
            self.error = Some(message.to_string());
        }
        if let Some(usage) = response.get("usage").filter(|value| !value.is_null()) {
            self.usage = Some(Usage {
                prompt_tokens: usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                completion_tokens: usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                total_tokens: usage
                    .get("total_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cached_tokens: usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .or_else(|| usage.get("cached_tokens"))
                    .or_else(|| usage.get("prompt_cache_hit_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cost_usd: usage
                    .get("cost")
                    .or_else(|| usage.get("total_cost"))
                    .or_else(|| usage.get("cost_usd"))
                    .and_then(Value::as_f64),
            });
        }
        match response.get("status").and_then(Value::as_str) {
            Some("completed") => self.finish_reason = Some("stop".to_string()),
            Some("incomplete") => self.finish_reason = Some("length".to_string()),
            _ => {}
        }
        let Some(items) = response.get("output").and_then(Value::as_array) else {
            return;
        };
        for (index, item) in items.iter().enumerate() {
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    self.absorb_item(index as u32, item, true);
                }
                Some("message")
                    if self.content.is_empty()
                        && item.get("role").and_then(Value::as_str) == Some("assistant") =>
                {
                    let text = item
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter(|part| {
                                    part.get("type").and_then(Value::as_str) == Some("output_text")
                                })
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<String>()
                        })
                        .unwrap_or_default();
                    if !text.is_empty() {
                        self.content = text;
                    }
                }
                _ => {}
            }
        }
    }

    fn finish(self, provider_name: &str) -> Result<Completion> {
        if let Some(error) = self.error {
            return Err(HarnessError::Provider(format!("{provider_name}: {error}")));
        }
        let tool_calls = self
            .calls
            .into_iter()
            .filter(|(_, call)| !call.name.is_empty())
            .enumerate()
            .map(|(position, (index, call))| ToolCall {
                id: if !call.call_id.is_empty() {
                    call.call_id
                } else if !call.item_id.is_empty() {
                    call.item_id
                } else {
                    format!("call_{index}_{position}")
                },
                kind: "function".to_string(),
                function: FunctionCall {
                    name: call.name,
                    arguments: call.arguments,
                },
            })
            .collect::<Vec<_>>();
        let finish_reason = self.finish_reason.or_else(|| {
            if tool_calls.is_empty() {
                None
            } else {
                Some("tool_calls".to_string())
            }
        });
        Ok(Completion {
            content: self.content,
            reasoning: self.reasoning,
            tool_calls,
            usage: self.usage,
            finish_reason,
        })
    }
}

fn output_index(event: &Value) -> u32 {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolDefinition;

    fn provider() -> ResponsesProvider {
        let config = ProviderConfig {
            base_url: "https://api.openai.com/v1".into(),
            api: "openai-responses".into(),
            ..Default::default()
        };
        ResponsesProvider::from_provider(&config, "test-key".into()).unwrap()
    }

    #[test]
    fn endpoint_appends_responses_once() {
        let config = ProviderConfig {
            base_url: "https://example.com/v1/".into(),
            api: "openai-responses".into(),
            ..Default::default()
        };
        use async_openai::config::Config as _;
        let provider = ResponsesProvider::from_provider(&config, String::new()).unwrap();
        assert_eq!(
            provider.client.config().api_base(),
            "https://example.com/v1"
        );

        let config = ProviderConfig {
            base_url: "https://example.com/v1/responses".into(),
            api: "openai-responses".into(),
            ..Default::default()
        };
        let provider = ResponsesProvider::from_provider(&config, String::new()).unwrap();
        assert_eq!(
            provider.client.config().api_base(),
            "https://example.com/v1"
        );
    }

    #[test]
    fn body_translates_messages_and_tools() {
        let provider = provider();
        let request = ChatRequest {
            model: "gpt-5".into(),
            messages: vec![
                crate::provider::ChatMessage::system("sys"),
                crate::provider::ChatMessage::user("hi"),
                crate::provider::ChatMessage::assistant_tool_calls(
                    Some("thinking".into()),
                    vec![ToolCall {
                        id: "call_1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "read".into(),
                            arguments: "{\"path\":\"a\"}".into(),
                        },
                    }],
                ),
                crate::provider::ChatMessage::tool("call_1", "file body"),
            ],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "read a file".into(),
                parameters: json!({ "type": "object" }),
            }],
            temperature: Some(0.2),
            max_tokens: Some(100),
            stream: true,
            model_reasoning: false,
        };

        let body = provider.body(&request);
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], 100);
        assert_eq!(body["temperature"].as_f64().unwrap(), 0.2f32 as f64);
        assert!(body.get("messages").is_none());

        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hi");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["name"], "read");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "file body");

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read");
        assert!(body["tools"][0].get("function").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn streamed_events_build_a_completion() {
        let mut accumulator = ResponsesAccumulator::default();
        let events = vec![
            json!({ "type": "response.output_item.added", "output_index": 3,
                    "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1",
                              "name": "read", "arguments": "" } }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 3,
                    "item_id": "fc_1", "delta": "{\"path\":" }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 3,
                    "item_id": "fc_1", "delta": "\"a\"}" }),
            json!({ "type": "response.reasoning_summary_text.delta",
                    "item_id": "rs_1", "output_index": 1, "delta": "because" }),
            json!({ "type": "response.output_text.delta",
                    "item_id": "msg_1", "output_index": 2, "delta": "hello " }),
            json!({ "type": "response.output_text.delta",
                    "item_id": "msg_1", "output_index": 2, "delta": "world" }),
            json!({ "type": "response.completed", "response": {
                "status": "completed",
                "usage": { "input_tokens": 10, "output_tokens": 5, "total_tokens": 15 },
                "output": [
                    { "type": "function_call", "id": "fc_1", "call_id": "call_1",
                      "name": "read", "arguments": "{\"path\":\"a\"}" },
                    { "type": "message", "role": "assistant",
                      "content": [{ "type": "output_text", "text": "hello world" }] }
                ]
            }}),
        ];
        for event in events {
            accumulator.apply(&event, &None);
        }
        let completion = accumulator.finish("openai").unwrap();
        assert_eq!(completion.content, "hello world");
        assert_eq!(completion.reasoning, "because");
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].id, "call_1");
        assert_eq!(completion.tool_calls[0].function.name, "read");
        assert_eq!(
            completion.tool_calls[0].function.arguments,
            "{\"path\":\"a\"}"
        );
        assert_eq!(
            completion.usage,
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
                cost_usd: None,
            })
        );
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn non_stream_response_parses() {
        let value = json!({
            "status": "completed",
            "usage": { "input_tokens": 7, "output_tokens": 3, "total_tokens": 10 },
            "output": [
                { "type": "reasoning", "id": "rs_1" },
                { "type": "message", "role": "assistant",
                  "content": [{ "type": "output_text", "text": "done" }] },
                { "type": "function_call", "id": "fc_1", "call_id": "call_9",
                  "name": "write", "arguments": "{\"path\":\"b\"}" }
            ]
        });
        let mut accumulator = ResponsesAccumulator::default();
        accumulator.absorb_response(&value);
        let completion = accumulator.finish("openai").unwrap();
        assert_eq!(completion.content, "done");
        assert_eq!(completion.tool_calls[0].id, "call_9");
        assert_eq!(completion.tool_calls[0].function.name, "write");
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
        assert_eq!(completion.usage.unwrap().total_tokens, 10);
    }

    #[test]
    fn failed_response_is_an_error() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator.absorb_response(&json!({
            "status": "failed",
            "error": { "code": "server_error", "message": "boom" }
        }));
        let err = accumulator.finish("openai").unwrap_err().to_string();
        assert!(err.contains("boom"), "{err}");
    }
}
