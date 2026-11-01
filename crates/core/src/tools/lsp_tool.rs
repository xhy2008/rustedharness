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
    /// Path relative to the workspace root (or absolute inside it).
    pub path: String,
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
         injected automatically by the edit/write tools."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<LspArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: LspArgs = parse_args(args)?;
        let path = ctx.resolver.resolve_existing(&args.path)?;
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
