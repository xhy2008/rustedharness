//! MCP client: Claude/Cursor-compatible `mcp.json`, stdio child processes and
//! Streamable HTTP, dynamic `mcp__<server>__<tool>` tool injection.
//!
//! Server crashes are isolated: only that server's tools become unavailable,
//! and tool calls return a structured `E_MCP_UNAVAILABLE` error.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use crate::config::McpConfig;
use crate::error::{HarnessError, Result};
use crate::provider::ToolDefinition;
use crate::tools::{Tool, ToolContext, ToolOutput};

const PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_TOOLS_PER_SERVER: usize = 512;

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Debug, Clone)]
pub struct McpCallResult {
    pub content: String,
    pub is_error: bool,
    pub raw: Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ServerConfig {
    command: Option<String>,
    args: Vec<String>,
    env: HashMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    disabled: bool,
    timeout_ms: Option<u64>,
}

enum Kind {
    Stdio {
        stdin: Arc<AsyncMutex<ChildStdin>>,
        child: Mutex<Child>,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: reqwest::header::HeaderMap,
        session: AsyncMutex<Option<String>>,
    },
}

pub struct McpServer {
    pub name: String,
    kind: Kind,
    alive: Arc<AtomicBool>,
    tools: Arc<AsyncMutex<Option<Vec<McpToolDef>>>>,
    next_id: AtomicI64,
    pending: PendingMap,
    timeout: Duration,
}

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<std::result::Result<Value, String>>>>>;

impl McpServer {
    async fn connect(
        name: &str,
        cfg: &ServerConfig,
        default_timeout: Duration,
    ) -> Result<Arc<Self>> {
        let timeout =
            Duration::from_millis(cfg.timeout_ms.unwrap_or(default_timeout.as_millis() as u64));
        let mut stdio_stdout = None;
        let kind = if let Some(command) = &cfg.command {
            let mut cmd = Command::new(command);
            cmd.args(&cfg.args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            for (key, value) in &cfg.env {
                cmd.env(key, expand_env(value));
            }
            let mut child = cmd.spawn().map_err(|e| {
                HarnessError::tool(
                    "E_MCP_SPAWN",
                    format!("server `{name}` failed to start `{command}`: {e}"),
                )
            })?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| HarnessError::tool("E_MCP_SPAWN", "stdin unavailable"))?;
            stdio_stdout = Some(
                child
                    .stdout
                    .take()
                    .ok_or_else(|| HarnessError::tool("E_MCP_SPAWN", "stdout unavailable"))?,
            );
            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        tracing::debug!("[mcp] {line}");
                    }
                });
            }
            Kind::Stdio {
                stdin: Arc::new(AsyncMutex::new(stdin)),
                child: Mutex::new(child),
            }
        } else if let Some(url) = &cfg.url {
            let parsed = url::Url::parse(url)
                .map_err(|e| HarnessError::tool("E_MCP_HTTP", format!("bad URL: {e}")))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(HarnessError::tool(
                    "E_MCP_HTTP",
                    "only http:// and https:// MCP endpoints are supported",
                ));
            }
            let mut headers = reqwest::header::HeaderMap::new();
            for (key, value) in &cfg.headers {
                let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|e| HarnessError::tool("E_MCP_HTTP", format!("header {key}: {e}")))?;
                let value = reqwest::header::HeaderValue::from_str(&expand_env(value))
                    .map_err(|e| HarnessError::tool("E_MCP_HTTP", format!("header {key}: {e}")))?;
                headers.insert(name, value);
            }
            let client = reqwest::Client::builder()
                .timeout(timeout + Duration::from_secs(5))
                .build()
                .map_err(|e| HarnessError::tool("E_MCP_HTTP", e.to_string()))?;
            Kind::Http {
                client,
                url: url.clone(),
                headers,
                session: AsyncMutex::new(None),
            }
        } else {
            return Err(HarnessError::tool(
                "E_MCP_CONFIG",
                format!("server `{name}` needs either `command` or `url`"),
            ));
        };

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let server = Arc::new(Self {
            name: name.to_string(),
            kind,
            alive: alive.clone(),
            tools: Arc::new(AsyncMutex::new(None)),
            next_id: AtomicI64::new(1),
            pending: pending.clone(),
            timeout,
        });

        if let (Kind::Stdio { stdin, .. }, Some(stdout)) = (&server.kind, stdio_stdout) {
            let stdin = stdin.clone();
            let pending_reader = pending.clone();
            let alive_reader = alive.clone();
            let tools_reader = server.tools.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                        continue;
                    };
                    if let Some(method) = value.get("method").and_then(Value::as_str) {
                        if let Some(id) = value.get("id") {
                            let result = match method {
                                "roots/list" => json!({ "roots": [] }),
                                "ping" => json!({}),
                                _ => Value::Null,
                            };
                            let response = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": result
                            });
                            let mut guard = stdin.lock().await;
                            let body = serde_json::to_vec(&response).unwrap_or_default();
                            let _ = guard.write_all(&body).await;
                            let _ = guard.write_all(b"\n").await;
                            let _ = guard.flush().await;
                        } else if method == "notifications/tools/list_changed" {
                            *tools_reader.lock().await = None;
                        }
                        continue;
                    }
                    if let Some(id) = value.get("id").and_then(Value::as_i64) {
                        if let Some(tx) = pending_reader.lock().unwrap().remove(&id) {
                            if let Some(error) = value.get("error") {
                                let message = error
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("MCP error")
                                    .to_string();
                                let _ = tx.send(Err(message));
                            } else {
                                let _ = tx
                                    .send(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
                            }
                        }
                    }
                }
                alive_reader.store(false, Ordering::SeqCst);
                let mut pending = pending_reader.lock().unwrap();
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err("server process exited".to_string()));
                }
            });
        }

        // Handshake. A failure here marks the server offline but does not
        // prevent other servers from loading.
        let init = server
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "rustedharness", "version": crate::VERSION }
                }),
            )
            .await
            .map_err(|e| {
                HarnessError::tool(
                    "E_MCP_INIT",
                    format!("server `{name}` initialize failed: {e}"),
                )
            })?;
        let _ = init;
        server
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(server)
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match &self.kind {
            Kind::Stdio { stdin, .. } => {
                if !self.alive() {
                    return Err(HarnessError::tool(
                        "E_MCP_UNAVAILABLE",
                        format!("MCP server `{}` is offline", self.name),
                    ));
                }
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = oneshot::channel();
                self.pending.lock().unwrap().insert(id, tx);
                let payload = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params
                });
                let body = serde_json::to_vec(&payload)?;
                {
                    let mut guard = stdin.lock().await;
                    guard.write_all(&body).await?;
                    guard.write_all(b"\n").await?;
                    guard.flush().await?;
                }
                match tokio::time::timeout(self.timeout, rx).await {
                    Ok(Ok(Ok(value))) => Ok(value),
                    Ok(Ok(Err(message))) => Err(HarnessError::tool(
                        "E_MCP_ERROR",
                        format!("{}: {message}", self.name),
                    )),
                    Ok(Err(_)) => Err(HarnessError::tool(
                        "E_MCP_UNAVAILABLE",
                        format!("MCP server `{}` closed", self.name),
                    )),
                    Err(_) => {
                        self.pending.lock().unwrap().remove(&id);
                        Err(HarnessError::tool(
                            "E_MCP_TIMEOUT",
                            format!("{}: {method} timed out", self.name),
                        ))
                    }
                }
            }
            Kind::Http {
                client,
                url,
                headers,
                session,
            } => {
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                let payload = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params
                });
                let mut request = client
                    .post(url)
                    .header("Accept", "application/json, text/event-stream")
                    .header("Content-Type", "application/json")
                    .header("MCP-Protocol-Version", PROTOCOL_VERSION)
                    .headers(headers.clone())
                    .json(&payload);
                if let Some(session) = session.lock().await.clone() {
                    request = request.header("Mcp-Session-Id", session);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| HarnessError::tool("E_MCP_HTTP", format!("{}: {e}", self.name)))?;
                if let Some(value) = response.headers().get("mcp-session-id") {
                    if let Ok(value) = value.to_str() {
                        *session.lock().await = Some(value.to_string());
                    }
                }
                let status = response.status();
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let body = response
                    .text()
                    .await
                    .map_err(|e| HarnessError::tool("E_MCP_HTTP", e.to_string()))?;
                if !status.is_success() {
                    return Err(HarnessError::tool(
                        "E_MCP_HTTP",
                        format!("{}: HTTP {status}", self.name),
                    ));
                }
                let value = if content_type.contains("text/event-stream") {
                    parse_sse(&body, id)?
                } else {
                    serde_json::from_str::<Value>(&body)
                        .map_err(|e| HarnessError::tool("E_MCP_HTTP", format!("bad JSON: {e}")))?
                };
                if let Some(error) = value.get("error") {
                    return Err(HarnessError::tool(
                        "E_MCP_ERROR",
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("MCP error")
                            .to_string(),
                    ));
                }
                Ok(value.get("result").cloned().unwrap_or(Value::Null))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        match &self.kind {
            Kind::Stdio { stdin, .. } => {
                if !self.alive() {
                    return Ok(());
                }
                let payload = json!({ "jsonrpc": "2.0", "method": method, "params": params });
                let body = serde_json::to_vec(&payload)?;
                let mut guard = stdin.lock().await;
                guard.write_all(&body).await?;
                guard.write_all(b"\n").await?;
                guard.flush().await?;
                Ok(())
            }
            Kind::Http {
                client,
                url,
                headers,
                session,
            } => {
                let payload = json!({ "jsonrpc": "2.0", "method": method, "params": params });
                let mut request = client
                    .post(url)
                    .header("Accept", "application/json, text/event-stream")
                    .header("Content-Type", "application/json")
                    .header("MCP-Protocol-Version", PROTOCOL_VERSION)
                    .headers(headers.clone())
                    .json(&payload);
                if let Some(session) = session.lock().await.clone() {
                    request = request.header("Mcp-Session-Id", session);
                }
                let _ = request.send().await;
                Ok(())
            }
        }
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        if let Some(cached) = self.tools.lock().await.clone() {
            return Ok(cached);
        }
        if !self.alive() {
            return Err(HarnessError::tool(
                "E_MCP_UNAVAILABLE",
                format!("MCP server `{}` is offline", self.name),
            ));
        }
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let params = cursor
                .clone()
                .map(|c| json!({ "cursor": c }))
                .unwrap_or_else(|| json!({}));
            let result = self.request("tools/list", params).await?;
            if let Some(items) = result.get("tools").and_then(Value::as_array) {
                for item in items {
                    let Some(name) = item.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let description = item
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let schema = item
                        .get("inputSchema")
                        .cloned()
                        .filter(|v| v.is_object())
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                    tools.push(McpToolDef {
                        name: name.to_string(),
                        description,
                        schema,
                    });
                    if tools.len() >= MAX_TOOLS_PER_SERVER {
                        break;
                    }
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            if cursor.is_none() || tools.len() >= MAX_TOOLS_PER_SERVER {
                break;
            }
        }
        *self.tools.lock().await = Some(tools.clone());
        Ok(tools)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpCallResult> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut texts = Vec::new();
        if let Some(items) = result.get("content").and_then(Value::as_array) {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => texts.push(
                        item.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                    Some("resource") => {
                        if let Some(text) = item.pointer("/resource/text").and_then(Value::as_str) {
                            texts.push(text.to_string());
                        } else {
                            texts.push(
                                item.pointer("/resource/uri")
                                    .and_then(Value::as_str)
                                    .unwrap_or("[resource]")
                                    .to_string(),
                            );
                        }
                    }
                    Some("image") => texts.push("[image content omitted]".to_string()),
                    _ => texts.push(item.to_string()),
                }
            }
        }
        if texts.is_empty() {
            texts.push(
                result
                    .get("structuredContent")
                    .cloned()
                    .unwrap_or(Value::Null)
                    .to_string(),
            );
        }
        Ok(McpCallResult {
            content: texts.join("\n"),
            is_error,
            raw: result,
        })
    }

    pub async fn shutdown(&self) {
        if let Kind::Stdio { child, .. } = &self.kind {
            let mut guard = child.lock().unwrap();
            let _ = guard.start_kill();
        }
        self.alive.store(false, Ordering::SeqCst);
    }
}

fn parse_sse(body: &str, id: i64) -> Result<Value> {
    let mut fallback = None;
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            return Ok(value);
        }
        if fallback.is_none() {
            fallback = Some(value);
        }
    }
    fallback.ok_or_else(|| HarnessError::tool("E_MCP_HTTP", "empty SSE response"))
}

fn expand_env(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                name.push(c);
            }
            out.push_str(&std::env::var(&name).unwrap_or_default());
        } else {
            out.push(c);
        }
    }
    out
}

pub(crate) fn exposed_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize(server), sanitize(tool))
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn denied(deny: &[String], server: &str, tool: &str, exposed: &str) -> bool {
    deny.iter().any(|d| {
        d == exposed
            || d == server
            || d == tool
            || d == &format!("{server}:{tool}")
            || d == &format!("{server}__{tool}")
    })
}

pub struct McpManager {
    servers: Vec<Arc<McpServer>>,
    deny: Vec<String>,
}

impl McpManager {
    /// Load every configured server. Individual failures are logged and the
    /// remaining servers stay available.
    pub async fn load(cwd: &std::path::Path, cfg: &McpConfig) -> Arc<Self> {
        if !cfg.enabled {
            return Arc::new(Self {
                servers: Vec::new(),
                deny: cfg.deny.clone(),
            });
        }
        let path = cfg
            .config_path
            .clone()
            .or_else(|| {
                let local = cwd.join("mcp.json");
                local.is_file().then_some(local)
            })
            .or_else(|| {
                let user = crate::config::Config::config_dir().join("mcp.json");
                user.is_file().then_some(user)
            });
        let Some(path) = path else {
            return Arc::new(Self {
                servers: Vec::new(),
                deny: cfg.deny.clone(),
            });
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!("cannot read {}: {err}", path.display());
                return Arc::new(Self {
                    servers: Vec::new(),
                    deny: cfg.deny.clone(),
                });
            }
        };
        let value: Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!("invalid {}: {err}", path.display());
                return Arc::new(Self {
                    servers: Vec::new(),
                    deny: cfg.deny.clone(),
                });
            }
        };
        let entries = value
            .get("mcpServers")
            .or_else(|| value.get("servers"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let timeout = Duration::from_millis(cfg.call_timeout_ms.max(1_000));
        let mut servers = Vec::new();
        for (name, entry) in entries {
            let parsed: ServerConfig = match serde_json::from_value(entry) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!("mcp server `{name}` is invalid: {err}");
                    continue;
                }
            };
            if parsed.disabled {
                continue;
            }
            match McpServer::connect(&name, &parsed, timeout).await {
                Ok(server) => {
                    if let Err(err) = server.list_tools().await {
                        tracing::warn!("mcp server `{name}` tools/list failed: {err}");
                    }
                    servers.push(server);
                }
                Err(err) => tracing::warn!("mcp server `{name}` unavailable: {err}"),
            }
        }
        Arc::new(Self {
            servers,
            deny: cfg.deny.clone(),
        })
    }

    /// Refresh `tools/list` for live servers (called before building prompts).
    pub async fn refresh(&self) {
        for server in &self.servers {
            if server.alive() && server.tools.lock().await.is_none() {
                let _ = server.list_tools().await;
            }
        }
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut out = Vec::new();
        for server in &self.servers {
            if !server.alive() {
                continue;
            }
            let cached = match server.tools.try_lock() {
                Ok(guard) => guard.clone(),
                Err(_) => None,
            };
            let Some(defs) = cached else { continue };
            for def in defs {
                let exposed = exposed_name(&server.name, &def.name);
                if denied(&self.deny, &server.name, &def.name, &exposed) {
                    continue;
                }
                out.push(Arc::new(McpTool {
                    exposed,
                    remote: def.name.clone(),
                    server: server.clone(),
                    description: def.description,
                    schema: def.schema,
                }) as Arc<dyn Tool>);
            }
        }
        out
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools()
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description(),
                parameters: tool.schema(),
            })
            .collect()
    }

    pub async fn shutdown_all(&self) {
        for server in &self.servers {
            server.shutdown().await;
        }
    }
}

pub struct McpTool {
    exposed: String,
    remote: String,
    server: Arc<McpServer>,
    description: String,
    schema: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed
    }

    fn description(&self) -> String {
        format!(
            "[MCP server `{}`] {}",
            self.server.name,
            self.description.trim()
        )
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn run(&self, _ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let result = self.server.call_tool(&self.remote, args).await?;
        Ok(ToolOutput {
            content: result.content,
            is_error: result.is_error,
            metadata: Some(json!({
                "mcp": {
                    "server": self.server.name,
                    "tool": self.remote,
                    "raw": result.raw
                }
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_openai_safe_and_stable() {
        assert_eq!(
            exposed_name("my server", "do/thing"),
            "mcp__my_server__do_thing"
        );
        assert_eq!(exposed_name("s", "t"), "mcp__s__t");
    }

    #[test]
    fn deny_matches_all_shapes() {
        let deny = vec!["mcp__s__t".to_string()];
        assert!(denied(&deny, "s", "t", "mcp__s__t"));
        assert!(!denied(&deny, "s", "u", "mcp__s__u"));
        let deny = vec!["s:t".to_string()];
        assert!(denied(&deny, "s", "t", "mcp__s__t"));
    }

    #[test]
    fn env_expansion() {
        std::env::set_var("RH_MCP_TEST", "value");
        assert_eq!(expand_env("a-${RH_MCP_TEST}-b"), "a-value-b");
        assert_eq!(expand_env("${RH_MCP_MISSING}"), "");
    }

    #[test]
    fn sse_response_matches_request_id() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n";
        let value = parse_sse(body, 2).unwrap();
        assert_eq!(value["id"], 2);
    }

    #[test]
    fn call_result_content_is_flattened() {
        let result = json!({
            "content": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" }
            ],
            "isError": false
        });
        let text = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text, "first\nsecond");
    }
}
