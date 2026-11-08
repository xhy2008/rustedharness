use async_trait::async_trait;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::Result;
use crate::tools::{parse_args, schema_of, Tool, ToolContext, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellArgs {
    pub command: String,
    pub run_in_background: Option<bool>,
    /// Foreground timeout in ms (default 30000).
    pub timeout_ms: Option<u64>,
    /// "text" (default, final screen lines) or "raw" (base64 of the pty bytes).
    pub capture: Option<String>,
}

pub struct ShellTool {
    spec: crate::shell::ShellSpec,
    cwd: std::path::PathBuf,
}

impl ShellTool {
    pub fn new(spec: crate::shell::ShellSpec, cwd: std::path::PathBuf) -> Self {
        Self { spec, cwd }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        self.spec.tool_name
    }

    fn description(&self) -> String {
        self.spec.description(&self.cwd)
    }

    fn schema(&self) -> Value {
        schema_of::<ShellArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: ShellArgs = parse_args(args)?;
        if args.command.trim().is_empty() {
            return Err(crate::error::HarnessError::tool(
                "E_INVALID_ARGUMENT",
                "command must not be empty",
            ));
        }
        if args.run_in_background.unwrap_or(false) {
            let handle = ctx.proc.spawn_background(&args.command)?;
            let body = format!(
                "started in background\npid: {}\nstatus: {}\ncommand: {}\n\
                 Use await(pid={}) to wait or snapshot its final screen.",
                handle.pid, handle.status, handle.command, handle.pid
            );
            return Ok(ToolOutput::ok(body).with_metadata(json!({
                "pid": handle.pid,
                "status": handle.status,
                "background": true,
            })));
        }
        let live = |screen: &str| ctx.emit_tool_output(screen.to_string());
        let outcome = ctx
            .proc
            .run_foreground_with(&args.command, args.timeout_ms, Some(&live))
            .await?;
        let mut body = format!(
            "exit_code: {}\nstatus: {}\nelapsed_ms: {}\n--- output ---\n{}",
            outcome
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "null".into()),
            outcome.status,
            outcome.elapsed_ms,
            if outcome.output.is_empty() {
                "(no output)"
            } else {
                &outcome.output
            }
        );
        if outcome.truncated {
            body.push_str("\n[output truncated by shell.max_output_bytes]");
        }
        body.push_str(&format!(
            "\n--- stats ---\nraw_bytes={} screen_lines={} ansi_sequences={} inline_rewrites={}",
            outcome.stats.raw_bytes,
            outcome.stats.screen_lines,
            outcome.stats.ansi_sequences,
            outcome.stats.inline_rewrites
        ));
        let mut metadata = json!({
            "pid": outcome.pid,
            "status": outcome.status,
            "exit_code": outcome.exit_code,
            "elapsed_ms": outcome.elapsed_ms,
            "truncated": outcome.truncated,
            "stats": outcome.stats,
        });
        if args.capture.as_deref() == Some("raw") {
            if let Some(raw) = ctx.proc.raw_bytes(outcome.pid) {
                metadata["raw_base64"] =
                    json!(base64::engine::general_purpose::STANDARD.encode(&raw));
                body.push_str("\n[raw bytes attached in metadata.raw_base64]");
            }
        }
        Ok(ToolOutput::ok(body).with_metadata(metadata))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AwaitArgs {
    pub pid: u32,
    pub timeout_ms: Option<u64>,
}

pub struct AwaitTool;

#[async_trait]
impl Tool for AwaitTool {
    fn name(&self) -> &str {
        "await"
    }

    fn description(&self) -> String {
        "Wait for a background process started by the shell tool. Always returns the \
         sanitized screen and stats; on timeout the process is left alive and status is \
         \"running\". Unknown pids report the live pid list."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<AwaitArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: AwaitArgs = parse_args(args)?;
        let outcome = ctx.proc.await_process(args.pid, args.timeout_ms).await?;
        let body = format!(
            "pid: {}\nstatus: {}\nexit_code: {}\nelapsed_ms: {}\n--- output ---\n{}",
            outcome.pid,
            outcome.status,
            outcome
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "null".into()),
            outcome.elapsed_ms,
            if outcome.output.is_empty() {
                "(no output yet)"
            } else {
                &outcome.output
            }
        );
        Ok(ToolOutput::ok(body).with_metadata(json!({
            "pid": outcome.pid,
            "status": outcome.status,
            "exit_code": outcome.exit_code,
            "elapsed_ms": outcome.elapsed_ms,
            "stats": outcome.stats,
        })))
    }
}
