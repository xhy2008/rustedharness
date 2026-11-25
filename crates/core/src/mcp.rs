//! MCP client: Claude/Cursor-compatible `mcp.json`, stdio child processes and
//! Streamable HTTP, dynamic `mcp__<server>__<tool>` tool injection.
//!
//! Server crashes are isolated: only that server's tools become unavailable,
//! and tool calls return a structured `E_MCP_UNAVAILABLE` error.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool as RmcpTool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ServiceError, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::{interpolate_env, McpConfig};
use crate::error::{HarnessError, Result};
use crate::provider::ToolDefinition;
use crate::tools::{Tool, ToolContext, ToolOutput};

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

pub struct McpServer {
    pub name: String,
    client: AsyncMutex<RunningService<RoleClient, ()>>,
    tools: AsyncMutex<Option<Vec<McpToolDef>>>,
    timeout: Duration,
}

impl McpServer {
    async fn connect(
        name: &str,
        cfg: &ServerConfig,
        default_timeout: Duration,
    ) -> Result<Arc<Self>> {
        let timeout =
            Duration::from_millis(cfg.timeout_ms.unwrap_or(default_timeout.as_millis() as u64));
        let client = if let Some(command) = &cfg.command {
            let mut cmd = Command::new(command);
            cmd.args(&cfg.args)
                .kill_on_drop(true)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            for (key, value) in &cfg.env {
                cmd.env(key, interpolate_env(value));
            }
            let (transport, stderr) = TokioChildProcess::builder(cmd)
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| {
                    HarnessError::tool(
                        "E_MCP_SPAWN",
                        format!("server `{name}` failed to start `{command}`: {e}"),
                    )
                })?;
            if let Some(stderr) = stderr {
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        tracing::debug!("[mcp] {line}");
                    }
                });
            }
            tokio::time::timeout(timeout, ().serve(transport))
                .await
                .map_err(|_| {
                    HarnessError::tool("E_MCP_TIMEOUT", format!("{name}: initialize timed out"))
                })?
                .map_err(|e| HarnessError::tool("E_MCP_CONNECT", format!("{name}: {e}")))?
        } else if let Some(url) = &cfg.url {
            let parsed = url::Url::parse(url)
                .map_err(|e| HarnessError::tool("E_MCP_HTTP", format!("bad URL: {e}")))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(HarnessError::tool(
                    "E_MCP_HTTP",
                    "only http:// and https:// MCP endpoints are supported",
                ));
            }
            let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
            let mut custom_headers = HashMap::new();
            for (key, value) in &cfg.headers {
                let header = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|e| HarnessError::tool("E_MCP_HTTP", format!("header {key}: {e}")))?;
                let value = interpolate_env(value);
                if header == reqwest::header::AUTHORIZATION {
                    let token = value.strip_prefix("Bearer ").unwrap_or(&value);
                    transport_config = transport_config.auth_header(token.to_string());
                } else {
                    let value = reqwest::header::HeaderValue::from_str(&value).map_err(|e| {
                        HarnessError::tool("E_MCP_HTTP", format!("header {key}: {e}"))
                    })?;
                    custom_headers.insert(header, value);
                }
            }
            transport_config = transport_config.custom_headers(custom_headers);
            let transport = StreamableHttpClientTransport::from_config(transport_config);
            tokio::time::timeout(timeout, ().serve(transport))
                .await
                .map_err(|_| {
                    HarnessError::tool("E_MCP_TIMEOUT", format!("{name}: initialize timed out"))
                })?
                .map_err(|e| HarnessError::tool("E_MCP_CONNECT", format!("{name}: {e}")))?
        } else {
            return Err(HarnessError::tool(
                "E_MCP_CONFIG",
                format!("server `{name}` needs either `command` or `url`"),
            ));
        };

        Ok(Arc::new(Self {
            name: name.to_string(),
            client: AsyncMutex::new(client),
            tools: AsyncMutex::new(None),
            timeout,
        }))
    }

    pub fn alive(&self) -> bool {
        self.client
            .try_lock()
            .map(|client| !client.is_closed())
            .unwrap_or(true)
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        let client = self.client.lock().await;
        let tools = tokio::time::timeout(self.timeout, client.list_all_tools())
            .await
            .map_err(|_| {
                HarnessError::tool(
                    "E_MCP_TIMEOUT",
                    format!("{}: tools/list timed out", self.name),
                )
            })?
            .map_err(|e| service_error(&self.name, e))?;
        let tools: Vec<McpToolDef> = tools
            .into_iter()
            .take(MAX_TOOLS_PER_SERVER)
            .map(tool_def)
            .collect();
        *self.tools.lock().await = Some(tools.clone());
        Ok(tools)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpCallResult> {
        let params = CallToolRequestParams::new(name.to_string())
            .with_arguments(arguments.as_object().cloned().unwrap_or_default());
        let client = self.client.lock().await;
        let result = tokio::time::timeout(self.timeout, client.call_tool(params))
            .await
            .map_err(|_| {
                HarnessError::tool(
                    "E_MCP_TIMEOUT",
                    format!("{}: tools/call timed out", self.name),
                )
            })?
            .map_err(|e| service_error(&self.name, e))?;
        Ok(McpCallResult {
            content: flatten_call_result(&result),
            is_error: result.is_error.unwrap_or(false),
            raw: serde_json::to_value(&result).unwrap_or(Value::Null),
        })
    }

    pub async fn shutdown(&self) {
        let mut client = self.client.lock().await;
        let _ = client.close_with_timeout(Duration::from_secs(2)).await;
    }
}

fn service_error(name: &str, error: ServiceError) -> HarnessError {
    let code = if matches!(error, ServiceError::TransportClosed) {
        "E_MCP_UNAVAILABLE"
    } else {
        "E_MCP_ERROR"
    };
    HarnessError::tool(code, format!("{name}: {error}"))
}

fn tool_def(tool: RmcpTool) -> McpToolDef {
    McpToolDef {
        name: tool.name.to_string(),
        description: tool.description.map(|d| d.to_string()).unwrap_or_default(),
        schema: Value::Object((*tool.input_schema).clone()),
    }
}

fn flatten_call_result(result: &CallToolResult) -> String {
    let mut texts = Vec::new();
    for item in &result.content {
        match item {
            ContentBlock::Text(text) => texts.push(text.text.clone()),
            ContentBlock::Image(_) => texts.push("[image content omitted]".to_string()),
            ContentBlock::Audio(_) => texts.push("[audio content omitted]".to_string()),
            ContentBlock::Resource(resource) => match &resource.resource {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    texts.push(text.clone())
                }
                rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => {
                    texts.push(uri.clone())
                }
                _ => texts.push("[resource omitted]".to_string()),
            },
            ContentBlock::ResourceLink(link) => texts.push(link.uri.clone()),
            _ => texts.push(serde_json::to_string(item).unwrap_or_default()),
        }
    }
    if texts.is_empty() {
        texts.push(
            result
                .structured_content
                .as_ref()
                .map(|value| value.to_string())
                .unwrap_or_else(|| Value::Null.to_string()),
        );
    }
    texts.join("\n")
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
            images: Vec::new(),
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
        assert_eq!(interpolate_env("a-${RH_MCP_TEST}-b"), "a-value-b");
        assert_eq!(interpolate_env("${RH_MCP_MISSING}"), "");
    }

    #[test]
    fn call_result_content_is_flattened() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" }
            ],
            "isError": false
        }))
        .unwrap();
        assert_eq!(flatten_call_result(&result), "first\nsecond");
    }
}
