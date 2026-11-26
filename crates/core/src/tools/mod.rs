//! Tool registry, shared context and built-in tools.

pub mod edit;
pub mod fs_tools;
pub mod lsp_tool;
pub mod search;
pub mod shell_tool;
pub mod todo;
pub mod web_tool;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::{HarnessError, Result};
use crate::intel::CodeIndex;
use crate::lsp::LspHub;
use crate::mcp::McpManager;
use crate::paths::WorkspacePathResolver;
use crate::proc::ProcRegistry;
use crate::protocol::HarnessEvent;
use crate::provider::{Backend, ToolDefinition};
use crate::web::WebTools;

use self::todo::TodoStore;

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub metadata: Option<Value>,
    /// Images the tool wants the model to see. They travel as a user message
    /// after the batch of tool results: providers only accept images from the
    /// user role, and a tool message inside a batch cannot be followed by one.
    pub images: Vec<crate::provider::ImagePart>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn with_images(mut self, images: Vec<crate::provider::ImagePart>) -> Self {
        self.images = images;
        self
    }
}

#[derive(Default)]
pub struct NoopTracker {
    entries: Mutex<std::collections::HashMap<String, (String, u32)>>,
}

impl NoopTracker {
    /// Record an identical no-op patch; returns the consecutive count.
    pub fn record(&self, key: &str, fingerprint: &str) -> u32 {
        let mut map = self.entries.lock().unwrap();
        let entry = map
            .entry(key.to_string())
            .or_insert_with(|| (String::new(), 0));
        if entry.0 == fingerprint {
            entry.1 += 1;
        } else {
            entry.0 = fingerprint.to_string();
            entry.1 = 1;
        }
        entry.1
    }

    pub fn reset(&self, key: &str) {
        self.entries.lock().unwrap().remove(key);
    }
}

/// Everything a tool may touch. Path access must go through `resolver` (R1–R4).
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub resolver: Arc<WorkspacePathResolver>,
    pub config: Arc<Config>,
    pub proc: Arc<ProcRegistry>,
    pub todo: Arc<TodoStore>,
    pub lsp: Arc<LspHub>,
    pub index: Arc<CodeIndex>,
    pub web: Arc<WebTools>,
    pub mcp: Arc<McpManager>,
    pub provider: Arc<dyn Backend>,
    pub compact_model: String,
    pub events: Option<UnboundedSender<HarnessEvent>>,
    pub session_id: String,
    pub noop: Arc<NoopTracker>,
    pub cancel: CancellationToken,
    /// Tool-call id for the invocation currently executing. Set by the agent
    /// before dispatch so long-running tools can stream output to the exact
    /// card in every frontend.
    pub call_id: Option<String>,
    pub tool_name: Option<String>,
}

impl ToolContext {
    pub fn emit(&self, event: HarnessEvent) {
        if let Some(events) = &self.events {
            let _ = events.send(event);
        }
    }

    /// Clone this context for one tool invocation so streaming output can be
    /// correlated with the `tool_call` event.
    pub fn for_call(&self, id: &str, name: &str) -> Self {
        let mut ctx = self.clone();
        ctx.call_id = Some(id.to_string());
        ctx.tool_name = Some(name.to_string());
        ctx
    }

    /// Stream a live screen update for the current tool call. No-op outside a
    /// correlated invocation.
    pub fn emit_tool_output(&self, text: impl Into<String>) {
        let (Some(id), Some(name)) = (&self.call_id, &self.tool_name) else {
            return;
        };
        self.emit(HarnessEvent::ToolOutput {
            id: id.clone(),
            name: name.clone(),
            text: text.into(),
        });
    }

    pub fn rel(&self, path: &std::path::Path) -> String {
        self.resolver.display_rel(path)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn schema(&self) -> Value;

    /// Whether a long result is a log to be searched rather than the value the
    /// model asked for. Those are written to a file the model can grep; every
    /// other tool's output is truncated instead, because a truncated answer is
    /// still an answer while a truncated file read is not.
    fn spills_long_output(&self) -> bool {
        false
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput>;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    deny: Vec<String>,
}

impl ToolRegistry {
    /// Build the built-in catalog. Platform-specific shell tool naming is
    /// decided here, never at call sites.
    pub fn builtin(ctx: &ToolContext) -> Self {
        let mut tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
        let mut insert = |tool: Arc<dyn Tool>| {
            tools.insert(tool.name().to_string(), tool);
        };
        insert(Arc::new(fs_tools::ReadTool));
        insert(Arc::new(fs_tools::WriteTool));
        insert(Arc::new(edit::EditTool));
        insert(Arc::new(search::GrepTool));
        insert(Arc::new(search::GlobTool));
        insert(Arc::new(shell_tool::ShellTool::new(
            ctx.proc.spec().clone(),
        )));
        insert(Arc::new(shell_tool::AwaitTool));
        insert(Arc::new(lsp_tool::LspTool::new(ctx.cwd.clone())));
        insert(Arc::new(web_tool::WebSearchTool));
        insert(Arc::new(todo::TodoTool));
        // MCP tools are injected as `mcp__server__tool`; a built-in with the
        // same (sanitised) name always wins.
        for tool in ctx.mcp.tools() {
            let name = tool.name().to_string();
            tools.entry(name).or_insert(tool);
        }
        Self {
            tools,
            deny: ctx.config.deny_tools.clone(),
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn spills_long_output(&self, name: &str) -> bool {
        self.get(name).is_some_and(|tool| tool.spills_long_output())
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .filter(|t| !self.deny.iter().any(|d| d == t.name()))
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description(),
                parameters: t.schema(),
            })
            .collect()
    }

    pub async fn run(&self, name: &str, ctx: &ToolContext, args: Value) -> ToolOutput {
        if self.deny.iter().any(|d| d == name) {
            return ToolOutput {
                content: HarnessError::tool("E_TOOL_DENIED", format!("tool `{name}` is denied"))
                    .to_model_json(),
                is_error: true,
                ..Default::default()
            };
        }
        let Some(tool) = self.get(name) else {
            return ToolOutput {
                content: HarnessError::tool(
                    "E_UNKNOWN_TOOL",
                    format!(
                        "unknown tool `{name}`; available: {}",
                        self.names().join(", ")
                    ),
                )
                .to_model_json(),
                is_error: true,
                ..Default::default()
            };
        };
        match tool.run(ctx, args).await {
            Ok(output) => output,
            Err(err) => ToolOutput {
                content: err.to_model_json(),
                is_error: true,
                metadata: Some(serde_json::json!({ "code": err.code() })),
                images: Vec::new(),
            },
        }
    }
}

/// Invalidate the name index for a written file and append LSP diagnostics to
/// the tool result. Returns the diagnostics text for use in `metadata`.
pub async fn append_diagnostics(
    ctx: &ToolContext,
    path: &std::path::Path,
    content: &mut String,
) -> Value {
    ctx.index.invalidate(path);
    match ctx
        .lsp
        .diagnostics_for(path, ctx.config.edit.diagnostics_timeout_ms)
        .await
    {
        Some(text) if !text.trim().is_empty() => {
            content.push_str("\n--- Diagnostics ---\n");
            content.push_str(&text);
            Value::String(text)
        }
        _ => {
            content.push_str("\n--- Diagnostics ---\nno diagnostics");
            Value::Null
        }
    }
}

/// Convert a schemars schema into the JSON Schema accepted by function tools.
pub fn schema_of<T: schemars::JsonSchema>() -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null);
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
        object.insert("additionalProperties".into(), Value::Bool(false));
    }
    schema
}

pub fn parse_args<T: serde::de::DeserializeOwned + schemars::JsonSchema>(args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|e| {
        HarnessError::tool(
            "E_INVALID_ARGUMENT",
            format!("invalid arguments: {e} (schema: {})", schema_of::<T>()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_tracker_counts_consecutive_identical_patches() {
        let t = NoopTracker::default();
        assert_eq!(t.record("f", "a"), 1);
        assert_eq!(t.record("f", "a"), 2);
        assert_eq!(t.record("f", "a"), 3);
        assert_eq!(t.record("f", "b"), 1);
        t.reset("f");
        assert_eq!(t.record("f", "b"), 1);
    }

    #[test]
    fn schema_has_no_meta_noise() {
        #[derive(schemars::JsonSchema)]
        struct Args {
            #[allow(dead_code)] // read through the generated schema
            path: String,
        }
        let schema = schema_of::<Args>();
        assert!(schema.get("$schema").is_none());
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["path"]["type"], "string");
    }
}
