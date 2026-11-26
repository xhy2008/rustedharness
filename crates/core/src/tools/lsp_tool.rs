//! Single `lsp` tool covering all ten code-intelligence operations.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{HarnessError, Result};
use crate::lsp::{LspOp, LspQuery};
use crate::tools::{parse_args, schema_of, Tool, ToolContext, ToolOutput};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct LspArgs {
    /// Operation to perform.
    pub op: LspOp,
    /// Server id for `install`/`restart`, e.g. rust-analyzer. `lsp` with
    /// `op="install"` needs the user to approve the download; the call waits
    /// for that answer.
    #[serde(default)]
    pub server: Option<String>,
    /// Path relative to the workspace root (or absolute inside it). Required
    /// for every op except `install` and `restart`.
    #[serde(default)]
    pub path: Option<String>,
    /// 1-based line. Required for position-based operations.
    #[serde(default)]
    pub line: Option<u32>,
    /// 1-based column (Unicode scalar values).
    #[serde(default)]
    pub column: Option<u32>,
    /// New name for `rename`, or a query for `workspace_symbols`.
    #[serde(default)]
    pub symbol: Option<String>,
    /// `rename` only: write the workspace edit (default false = preview).
    #[serde(default)]
    pub apply: Option<bool>,
    /// Per-request timeout in milliseconds (default 5000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Maximum number of results (default 50, max 200).
    #[serde(default)]
    pub limit: Option<usize>,
}

pub struct LspTool {
    cwd: std::path::PathBuf,
}

impl LspTool {
    pub fn new(cwd: std::path::PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> String {
        "Code intelligence via a language server (precise) or the built-in \
         name-based index (fallback). Ops: definition, type_definition, \
         implementation, references, hover, document_symbols, \
         workspace_symbols, call_hierarchy, rename, diagnostics. Results are \
         annotated with match=\"lsp-precise\" or match=\"name-based\". Rename \
         is a dry-run preview unless apply=true. After edits, diagnostics are \
         injected automatically by the edit/write tools. Two more ops manage \
         servers and take `server` instead of `path`: install downloads the \
         newest release of a registry server (the user is asked to approve it \
         and the call waits for the answer; it fails with \
         E_LSP_CONSENT_DECLINED if refused), restart replaces a running server \
         and waits for it to be ready."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<LspArgs>()
    }

    /// Diagnostics for a whole file, or a symbol tree, can run to pages. Unlike
    /// a `read` window, the model cannot ask for "the rest" of a diagnostic
    /// list, so the result is spilt to a file it can grep rather than cut.
    fn spills_long_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: LspArgs = parse_args(args)?;
        if args.op.is_maintenance() {
            let server = args.server.as_deref().unwrap_or("").trim();
            if server.is_empty() {
                return Err(HarnessError::tool(
                    "E_INVALID_ARGUMENT",
                    format!(
                        "`{}` requires `server` (the language server id, e.g. rust-analyzer)",
                        args.op.as_str()
                    ),
                ));
            }
            let answer = ctx
                .lsp
                .answer(LspQuery {
                    op: args.op,
                    server: Some(server.to_string()),
                    path: std::path::PathBuf::new(),
                    line: 1,
                    column: 1,
                    symbol: None,
                    apply: false,
                    timeout_ms: 0,
                    limit: 1,
                })
                .await?;
            let metadata = json!({
                "op": args.op.as_str(),
                "match": answer.match_kind,
                "server": server,
                "raw": answer.raw,
            });
            return Ok(ToolOutput::ok(answer.text).with_metadata(metadata));
        }
        let Some(requested) = args.path.as_deref() else {
            return Err(HarnessError::tool(
                "E_INVALID_ARGUMENT",
                format!("`{}` requires `path`", args.op.as_str()),
            ));
        };
        let path = ctx.resolver.resolve_existing(requested)?;
        if path.is_dir() {
            return Err(HarnessError::tool(
                "E_INVALID_ARGUMENT",
                "lsp requires a file path, not a directory",
            ));
        }
        let needs_position = matches!(
            args.op,
            LspOp::Definition
                | LspOp::TypeDefinition
                | LspOp::Implementation
                | LspOp::References
                | LspOp::Hover
                | LspOp::CallHierarchy
                | LspOp::Rename
        );
        if needs_position && args.line.is_none() {
            return Err(HarnessError::tool(
                "E_INVALID_ARGUMENT",
                format!("`{}` requires `line`", args.op.as_str()),
            ));
        }
        if args.op == LspOp::Rename && args.symbol.as_deref().unwrap_or("").trim().is_empty() {
            return Err(HarnessError::tool(
                "E_INVALID_ARGUMENT",
                "`rename` requires `symbol` (the new name)",
            ));
        }

        let query = LspQuery {
            op: args.op,
            server: None,
            path: path.clone(),
            line: args.line.unwrap_or(1),
            column: args.column.unwrap_or(1),
            symbol: args.symbol,
            apply: args.apply.unwrap_or(false),
            timeout_ms: args.timeout_ms.unwrap_or(0),
            limit: args.limit.unwrap_or(50),
        };
        let answer = ctx.lsp.answer(query).await?;

        // A successful rename keeps the L0 index honest.
        for file in &answer.applied {
            let changed = self.cwd.join(&file.path);
            ctx.index.invalidate(&changed);
        }

        let mut text = answer.text.clone();
        if let Some(hint) = &answer.warmup_hint {
            text.push_str("\n\n");
            text.push_str(hint);
        }
        let metadata = json!({
            "op": args.op.as_str(),
            "match": answer.match_kind,
            "warmup_hint": answer.warmup_hint,
            "applied": answer.applied.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
            "raw": answer.raw,
        });
        Ok(ToolOutput::ok(text).with_metadata(metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model only learns the ops from the schema, so the two management
    /// ops and their `server` argument have to be in it.
    #[test]
    fn the_schema_lists_server_management() {
        let schema = LspTool::new(std::env::temp_dir()).schema().to_string();
        for needle in ["install", "restart", "server"] {
            assert!(schema.contains(needle), "missing {needle} in {schema}");
        }
    }
}
