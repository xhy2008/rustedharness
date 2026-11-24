//! The agent loop: streaming model calls, tool execution, todo chase,
//! compaction, cancellation, session persistence and token accounting.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::cache::CacheWatch;
use crate::compact::{Compactor, SummaryMode, SummaryRequest};
use crate::config::{model_display, CompactionSettings, Config, StripContent, ThinkingLevel};
use crate::error::{HarnessError, Result};
use crate::intel::CodeIndex;
use crate::lsp::LspHub;
use crate::mcp::McpManager;
use crate::paths::WorkspacePathResolver;
use crate::proc::ProcRegistry;
use crate::protocol::{HarnessEvent, SessionSnapshot};
use crate::provider::{Backend, ChatMessage, ChatRequest, Completion, Role, StreamEvent};
use crate::session::{
    canonical_tool_arguments, LoadedSession, SessionMeta, SessionStore, SessionWriter,
};
use crate::shell::ShellSpec;
use crate::token::UsageTracker;
use crate::tools::todo::TodoStore;
use crate::tools::{NoopTracker, ToolContext, ToolOutput, ToolRegistry};
use crate::web::WebTools;

pub struct Agent {
    config: Arc<Config>,
    backend: Arc<dyn Backend>,
    ctx: RwLock<ToolContext>,
    tools: RwLock<ToolRegistry>,
    messages: RwLock<Vec<ChatMessage>>,
    session: Mutex<SessionWriter>,
    todos: Arc<TodoStore>,
    usage: Arc<UsageTracker>,
    events: UnboundedSender<HarnessEvent>,
    agent_model: RwLock<String>,
    /// Actual model used for summaries: the configured compact model, or the
    /// agent model when no separate model is selected.
    compact_model: RwLock<String>,
    separate_compact: AtomicBool,
    compaction: RwLock<CompactionSettings>,
    cancel_token: Mutex<CancellationToken>,
    busy: AtomicBool,
    /// Prompt-cache accounting across turns; also feeds the cache-miss notice.
    cache_watch: Mutex<CacheWatch>,
    show_cache_miss_notices: AtomicBool,
}

struct ResumeData {
    meta: SessionMeta,
    path: PathBuf,
    loaded: LoadedSession,
}

impl Agent {
    pub async fn new(
        config: Config,
        cwd: PathBuf,
        events: UnboundedSender<HarnessEvent>,
    ) -> Result<Arc<Self>> {
        let backend = Self::backend_for(&config)?;
        Self::build(config, cwd, events, backend, None).await
    }

    /// Build with a custom backend (tests, custom providers).
    pub async fn with_backend(
        config: Config,
        cwd: PathBuf,
        events: UnboundedSender<HarnessEvent>,
        backend: Arc<dyn Backend>,
    ) -> Result<Arc<Self>> {
        Self::build(config, cwd, events, backend, None).await
    }

    pub async fn resume(
        mut config: Config,
        session_id: &str,
        cwd_override: Option<PathBuf>,
        events: UnboundedSender<HarnessEvent>,
    ) -> Result<Arc<Self>> {
        let store = SessionStore::new(&config.session);
        let loaded = store.load(session_id)?;
        let meta = loaded
            .meta
            .clone()
            .ok_or_else(|| HarnessError::tool("E_SESSION_CORRUPT", "missing metadata"))?;
        // A resumed session carries its model in the transcript. Use it when
        // config.json has none, rather than refusing to resume.
        if config.model.trim().is_empty() && !meta.agent_model.trim().is_empty() {
            config.model = meta.agent_model.clone();
        }
        // Append to the file the session was read from: it may live in a
        // per-project folder and carry pi's `<timestamp>_<id>` name.
        let path = loaded.path.clone();
        let cwd = cwd_override.unwrap_or_else(|| PathBuf::from(&meta.cwd));
        let backend = Self::backend_for(&config)?;
        Self::build(
            config,
            cwd,
            events,
            backend,
            Some(ResumeData { meta, path, loaded }),
        )
        .await
    }

    fn backend_for(config: &Config) -> Result<Arc<dyn Backend>> {
        match crate::provider::Router::from_config(config) {
            Ok(backend) => Ok(Arc::new(backend)),
            Err(err) if config.model.trim().is_empty() => Ok(Arc::new(
                crate::provider::UnconfiguredModel::new(err.to_string()),
            )),
            Err(err) => Err(err),
        }
    }

    async fn build(
        config: Config,
        cwd: PathBuf,
        events: UnboundedSender<HarnessEvent>,
        backend: Arc<dyn Backend>,
        resume: Option<ResumeData>,
    ) -> Result<Arc<Self>> {
        let cwd = dunce::canonicalize(&cwd).unwrap_or(cwd);
        if !cwd.is_dir() {
            return Err(HarnessError::Config(format!(
                "workspace `{}` is not a directory",
                cwd.display()
            )));
        }
        let compaction = CompactionSettings::from(&config.agent);
        let separate_compact = !config.compact_model.trim().is_empty();
        let show_cache_miss_notices = config.agent.show_cache_miss_notices;
        let config = Arc::new(config);
        // A pi session may not name a model at all (no `model_change`, no
        // assistant message yet). Use the transcript's model when config.json
        // has none, and let an unset compactModel follow the agent model.
        let agent_model = resume
            .as_ref()
            .map(|r| r.meta.agent_model.clone())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| config.model.clone());
        let compact_model = if separate_compact {
            config.compact_model.clone()
        } else {
            agent_model.clone()
        };
        let todos = Arc::new(TodoStore::new(
            resume
                .as_ref()
                .map(|r| r.loaded.todos.clone())
                .unwrap_or_default(),
        ));

        let (session, messages) = if let Some(resume) = resume.as_ref() {
            let writer =
                SessionWriter::resume(&resume.path, &resume.meta.id, &resume.meta.agent_model)?;
            (writer, resume.loaded.messages.clone())
        } else {
            let writer =
                SessionWriter::create(&config.session, &cwd, &agent_model, &compact_model)?;
            (writer, Vec::new())
        };
        let session_id = session.id().to_string();
        let ctx = Self::make_workspace_ctx(
            config.clone(),
            &cwd,
            events.clone(),
            backend.clone(),
            todos.clone(),
            &compact_model,
            session_id,
        )
        .await?;
        let tools = ToolRegistry::builtin(&ctx);
        let agent = Arc::new(Self {
            config,
            backend,
            ctx: RwLock::new(ctx),
            tools: RwLock::new(tools),
            messages: RwLock::new(messages),
            session: Mutex::new(session),
            todos,
            usage: {
                let tracker = UsageTracker::new();
                // A resumed session keeps the numbers the provider reported
                // before it was closed; without this the meter reads zero
                // while the transcript is clearly not empty.
                if let Some(loaded) = resume.as_ref().map(|r| &r.loaded) {
                    if let Some(usage) = loaded.usage {
                        tracker.restore(usage, loaded.cost_usd);
                    }
                }
                Arc::new(tracker)
            },
            events: events.clone(),
            agent_model: RwLock::new(agent_model),
            compact_model: RwLock::new(compact_model),
            separate_compact: AtomicBool::new(separate_compact),
            compaction: RwLock::new(compaction),
            cancel_token: Mutex::new(CancellationToken::new()),
            busy: AtomicBool::new(false),
            cache_watch: Mutex::new(CacheWatch::new()),
            show_cache_miss_notices: AtomicBool::new(show_cache_miss_notices),
        });
        agent.refresh_system();
        let (cwd_display, session_id) = {
            let ctx = agent.ctx.read().unwrap();
            (
                ctx.cwd.to_string_lossy().to_string(),
                ctx.session_id.clone(),
            )
        };
        let _ = events.send(HarnessEvent::SessionStarted {
            session_id,
            cwd: cwd_display,
            agent_model: agent.agent_model.read().unwrap().clone(),
            compact_model: agent.compact_model.read().unwrap().clone(),
        });
        if agent.agent_model.read().unwrap().trim().is_empty() {
            let _ = events.send(HarnessEvent::Notice {
                level: "warning".to_string(),
                message:
                    "no model configured — add a provider/model to models.json and set `model`"
                        .to_string(),
            });
        }
        Ok(agent)
    }

    async fn make_workspace_ctx(
        config: Arc<Config>,
        cwd: &Path,
        events: UnboundedSender<HarnessEvent>,
        backend: Arc<dyn Backend>,
        todos: Arc<TodoStore>,
        compact_model: &str,
        session_id: String,
    ) -> Result<ToolContext> {
        let resolver = Arc::new(WorkspacePathResolver::new(cwd)?);
        let spec = ShellSpec::detect(&config.shell.prefer);
        let proc = Arc::new(ProcRegistry::new(
            spec,
            config.shell.clone(),
            cwd.to_path_buf(),
            Some(events.clone()),
        ));
        let lsp = LspHub::new(cwd.to_path_buf(), &config.lsp, Some(events.clone()));
        let index: Arc<CodeIndex> = lsp.index().clone();
        let web = Arc::new(WebTools::new(&config)?);
        let mcp = McpManager::load(cwd, &config.mcp).await;
        Ok(ToolContext {
            cwd: cwd.to_path_buf(),
            resolver,
            config,
            proc,
            todo: todos,
            lsp,
            index,
            web,
            mcp,
            provider: backend,
            compact_model: compact_model.to_string(),
            events: Some(events),
            session_id,
            noop: Arc::new(NoopTracker::default()),
            cancel: CancellationToken::new(),
            call_id: None,
            tool_name: None,
        })
    }

    pub fn session_id(&self) -> String {
        self.ctx.read().unwrap().session_id.clone()
    }

    pub fn cwd(&self) -> PathBuf {
        self.ctx.read().unwrap().cwd.clone()
    }

    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn agent_model(&self) -> String {
        self.agent_model.read().unwrap().clone()
    }

    pub fn compact_model(&self) -> String {
        self.compact_model.read().unwrap().clone()
    }

    pub fn compact_is_separate(&self) -> bool {
        self.separate_compact.load(Ordering::Relaxed)
    }

    pub fn compaction_settings(&self) -> CompactionSettings {
        *self.compaction.read().unwrap()
    }

    pub fn set_compact_threshold(&self, threshold: f64) -> Result<()> {
        let threshold = threshold.clamp(0.1, 0.99);
        let mut settings = self.compaction_settings();
        settings.compact_threshold = threshold;
        self.config
            .save_setting(&["agent", "compactThreshold"], json!(threshold))?;
        *self.compaction.write().unwrap() = settings;
        Ok(())
    }

    pub fn set_keep_recent_turns(&self, turns: usize) -> Result<()> {
        let turns = turns.max(1);
        let mut settings = self.compaction_settings();
        settings.keep_recent_turns = turns;
        self.config
            .save_setting(&["agent", "keepRecentTurns"], json!(turns))?;
        *self.compaction.write().unwrap() = settings;
        Ok(())
    }

    pub fn set_compact_strip_context(&self, on: bool) -> Result<()> {
        let mut settings = self.compaction_settings();
        settings.compact_strip_context = on;
        self.config
            .save_setting(&["agent", "compactStripContext"], json!(on))?;
        *self.compaction.write().unwrap() = settings;
        Ok(())
    }

    pub fn set_send_recent_turns(&self, turns: usize) -> Result<()> {
        let turns = turns.max(1);
        let mut settings = self.compaction_settings();
        settings.send_recent_turns = turns;
        self.config
            .save_setting(&["agent", "sendRecentTurns"], json!(turns))?;
        *self.compaction.write().unwrap() = settings;
        Ok(())
    }

    pub fn set_compact_thinking(&self, level: ThinkingLevel) -> Result<()> {
        let mut settings = self.compaction_settings();
        settings.compact_thinking = level;
        self.config
            .save_setting(&["agent", "compactThinking"], json!(level))?;
        *self.compaction.write().unwrap() = settings;
        Ok(())
    }

    pub fn set_strip_content(&self, content: StripContent) -> Result<()> {
        let mut settings = self.compaction_settings();
        settings.strip_content = content;
        self.config
            .save_setting(&["agent", "stripContent"], json!(content))?;
        *self.compaction.write().unwrap() = settings;
        Ok(())
    }

    fn context_limit(&self) -> u64 {
        self.config.model_context_window(&self.agent_model())
    }

    fn output_limit(&self) -> u32 {
        self.config
            .model_max_tokens(&self.agent_model())
            .unwrap_or(self.config.agent.max_output_tokens)
    }

    pub fn messages(&self) -> Vec<ChatMessage> {
        self.messages.read().unwrap().clone()
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let ctx = self.ctx.read().unwrap();
        let messages = self.messages.read().unwrap().clone();
        let context_tokens = self.usage.context_tokens();
        SessionSnapshot {
            session_id: ctx.session_id.clone(),
            cwd: ctx.cwd.to_string_lossy().to_string(),
            agent_model: self.agent_model.read().unwrap().clone(),
            compact_model: self.compact_model.read().unwrap().clone(),
            messages,
            todos: self.todos.snapshot(),
            context_tokens,
            context_limit: self.context_limit(),
            usage: self.usage.last(),
            cost_usd: self.usage.total_cost_usd(),
        }
    }

    pub fn todo(&self) -> Arc<TodoStore> {
        self.todos.clone()
    }

    /// Frontend consent decision keyed by the download-consent card id.
    pub fn grant_download_by_id(&self, consent_id: &str, allow: bool) {
        if let Ok(ctx) = self.ctx.read() {
            ctx.lsp.grant_download_by_id(consent_id, allow);
        }
    }

    /// Frontend consent decision for a language-server download.
    pub fn grant_download(&self, server_id: &str, allow: bool) {
        if let Ok(ctx) = self.ctx.read() {
            ctx.lsp.grant_download(server_id, allow);
        }
    }

    /// Current supervisor state for the LSP panel.
    pub fn lsp_statuses(&self) -> Vec<crate::lsp::ServerStatus> {
        self.ctx
            .read()
            .map(|ctx| ctx.lsp.statuses())
            .unwrap_or_default()
    }

    /// Install or update a language server on the user's behalf (TUI action).
    pub async fn install_lsp(&self, server: &str, update: bool) -> Result<PathBuf> {
        crate::lsp::install_server(
            &self.config.lsp,
            &self.cwd(),
            server,
            update,
            Some(self.events.clone()),
        )
        .await
    }

    pub fn set_agent_model(&self, model: &str) -> Result<()> {
        let spec = self.config.resolve_model(model)?;
        let reference = spec.reference();
        *self.agent_model.write().unwrap() = reference.clone();
        let compact = if self.compact_is_separate() {
            self.compact_model.read().unwrap().clone()
        } else {
            *self.compact_model.write().unwrap() = reference.clone();
            if let Ok(mut ctx) = self.ctx.write() {
                ctx.compact_model = reference.clone();
            }
            reference.clone()
        };
        self.session.lock().unwrap().models(&reference, &compact)?;
        self.refresh_system();
        Ok(())
    }

    /// Switch the compaction model. An empty reference selects the main model;
    /// the choice is written to `config.json`.
    pub fn set_compact_model(&self, model: &str) -> Result<()> {
        let agent = self.agent_model.read().unwrap().clone();
        let (stored, actual) = if model.trim().is_empty() {
            (String::new(), agent.clone())
        } else {
            let reference = self.config.resolve_model(model)?.reference();
            (reference.clone(), reference)
        };
        self.config.save_setting(&["compactModel"], json!(stored))?;
        *self.compact_model.write().unwrap() = actual.clone();
        self.separate_compact
            .store(!stored.is_empty(), Ordering::Relaxed);
        self.session.lock().unwrap().models(&agent, &actual)?;
        if let Ok(mut ctx) = self.ctx.write() {
            ctx.compact_model = actual;
        }
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancel_token.lock().unwrap().cancel();
        // Interrupt cascade: all live process groups die with the turn.
        if let Ok(ctx) = self.ctx.read() {
            ctx.proc.cleanup_all();
        }
    }

    /// Rebuild the workspace-bound components (cwd selection from a frontend).
    pub async fn set_cwd(&self, path: &Path) -> Result<()> {
        if self.busy() {
            return Err(HarnessError::tool(
                "E_BUSY",
                "cannot change cwd while a turn is running",
            ));
        }
        let cwd = dunce::canonicalize(path)
            .map_err(|e| HarnessError::Config(format!("invalid cwd {}: {e}", path.display())))?;
        if !cwd.is_dir() {
            return Err(HarnessError::Config(format!(
                "`{}` is not a directory",
                cwd.display()
            )));
        }
        let old_ctx = self.ctx.read().unwrap().clone();
        let compact_model = self.compact_model.read().unwrap().clone();
        let ctx = Self::make_workspace_ctx(
            self.config.clone(),
            &cwd,
            self.events.clone(),
            self.backend.clone(),
            self.todos.clone(),
            &compact_model,
            old_ctx.session_id.clone(),
        )
        .await?;
        // Preserve the session identity; only the workspace moves.
        let tools = ToolRegistry::builtin(&ctx);
        *self.ctx.write().unwrap() = ctx;
        *self.tools.write().unwrap() = tools;
        old_ctx.lsp.shutdown_all().await;
        old_ctx.mcp.shutdown_all().await;
        old_ctx.proc.cleanup_all();
        self.refresh_system();
        let _ = self.events.send(HarnessEvent::Notice {
            level: "info".to_string(),
            message: format!("workspace changed to {}", cwd.display()),
        });
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.cancel();
        let ctx = self.ctx.read().unwrap().clone();
        ctx.lsp.shutdown_all().await;
        ctx.mcp.shutdown_all().await;
        ctx.proc.cleanup_all();
    }

    fn refresh_system(&self) {
        let model = self.agent_model.read().unwrap().clone();
        let prompt = system_prompt(model_display(&model));
        let mut messages = self.messages.write().unwrap();
        if messages
            .first()
            .map(|m| m.role == Role::System)
            .unwrap_or(false)
        {
            messages[0] = ChatMessage::system(prompt);
        } else {
            messages.insert(0, ChatMessage::system(prompt));
        }
    }

    fn rebuild_tools(&self) {
        let ctx = self.ctx.read().unwrap().clone();
        let registry = ToolRegistry::builtin(&ctx);
        *self.tools.write().unwrap() = registry;
    }

    fn push_user(&self, text: &str) {
        self.messages.write().unwrap().push(ChatMessage::user(text));
        let _ = self.session.lock().unwrap().user(text);
        let _ = self.events.send(HarnessEvent::UserMessage {
            text: text.to_string(),
        });
    }

    /// Run one user turn to completion. Frontends call this in their own task.
    pub async fn send(self: &Arc<Self>, text: impl Into<String>) -> Result<()> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(HarnessError::tool(
                "E_BUSY",
                "a turn is already running; cancel it first",
            ));
        }
        let result = self.run_turn(text.into()).await;
        self.busy.store(false, Ordering::SeqCst);
        result
    }

    async fn run_turn(self: &Arc<Self>, text: String) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        if self.agent_model.read().unwrap().trim().is_empty() {
            let message =
                "no model configured — add a provider/model to models.json and set `model`";
            let _ = self.events.send(HarnessEvent::Error {
                message: message.to_string(),
            });
            let _ = self.events.send(HarnessEvent::TurnComplete {
                turn: 0,
                stop_reason: "error".to_string(),
            });
            return Err(HarnessError::Config(message.to_string()));
        }
        let token = CancellationToken::new();
        *self.cancel_token.lock().unwrap() = token.clone();
        self.refresh_system();
        self.push_user(&text);
        let mcp = self.ctx.read().unwrap().mcp.clone();
        mcp.refresh().await;
        self.rebuild_tools();
        // The tool catalog is part of the cached prompt prefix. Build it once
        // per turn instead of regenerating it before every model call.
        let definitions = self.tools.read().unwrap().definitions();

        let max_turns = self.config.agent.max_turns.max(1);
        let chase_limit = self.config.agent.chase_limit;
        let mut turn = 0usize;
        let mut chases = 0u32;

        loop {
            if token.is_cancelled() {
                let _ = self.events.send(HarnessEvent::Notice {
                    level: "warning".to_string(),
                    message: "turn cancelled".to_string(),
                });
                let _ = self.events.send(HarnessEvent::TurnComplete {
                    turn: turn as u64,
                    stop_reason: "cancelled".to_string(),
                });
                return Ok(());
            }
            turn += 1;
            if turn > max_turns {
                let _ = self.events.send(HarnessEvent::Notice {
                    level: "warning".to_string(),
                    message: format!("reached max_turns ({max_turns})"),
                });
                let _ = self.events.send(HarnessEvent::TurnComplete {
                    turn: turn as u64,
                    stop_reason: "max_turns".to_string(),
                });
                return Ok(());
            }

            if let Err(err) = self.maybe_compact(&token, &definitions).await {
                let _ = self.events.send(HarnessEvent::Notice {
                    level: "warning".to_string(),
                    message: format!("compaction failed: {err}"),
                });
            }

            let messages = self.messages.read().unwrap().clone();
            let request = ChatRequest {
                model: self.agent_model.read().unwrap().clone(),
                messages,
                tools: definitions.clone(),
                temperature: self.config.agent.temperature.map(|value| value as f32),
                max_tokens: Some(self.output_limit()),
                stream: true,
                model_reasoning: self
                    .config
                    .resolve_model(&self.agent_model.read().unwrap().clone())
                    .map(|spec| spec.reasoning)
                    .unwrap_or(false),
                reasoning_effort: None,
                http_idle_timeout_ms: 0,
            };

            let (sink, mut stream_rx) = mpsc::unbounded_channel();
            let events = self.events.clone();
            let forward = tokio::spawn(async move {
                while let Some(event) = stream_rx.recv().await {
                    match event {
                        StreamEvent::Delta(text) => {
                            let _ = events.send(HarnessEvent::AssistantDelta { text });
                        }
                        StreamEvent::Reasoning(text) => {
                            let _ = events.send(HarnessEvent::AssistantReasoning { text });
                        }
                    }
                }
            });
            let completion = self
                .stream_with_retry(request, sink.clone(), token.clone(), turn)
                .await;
            drop(sink);
            let _ = forward.await;

            let completion = match completion {
                Ok(completion) => completion,
                Err(err) => {
                    if token.is_cancelled() {
                        let _ = self.events.send(HarnessEvent::TurnComplete {
                            turn: turn as u64,
                            stop_reason: "cancelled".to_string(),
                        });
                        return Ok(());
                    }
                    let _ = self.events.send(HarnessEvent::Error {
                        message: err.to_string(),
                    });
                    let _ = self.events.send(HarnessEvent::TurnComplete {
                        turn: turn as u64,
                        stop_reason: "error".to_string(),
                    });
                    return Err(err);
                }
            };

            self.report_usage(&completion);
            let content = completion.content.clone();
            let reasoning = completion.reasoning.clone();
            let calls = completion.tool_calls.clone();

            let message_calls: Vec<_> = calls
                .iter()
                .map(|call| {
                    let mut call = call.clone();
                    call.function.arguments = canonical_tool_arguments(&call.function.arguments);
                    call
                })
                .collect();
            let assistant = ChatMessage {
                role: Role::Assistant,
                content: (!content.is_empty()).then_some(content.clone()),
                reasoning_content: (!reasoning.is_empty()).then_some(reasoning.clone()),
                tool_calls: (!message_calls.is_empty()).then_some(message_calls),
                ..Default::default()
            };
            if assistant.content.is_some() || assistant.tool_calls.is_some() {
                self.messages.write().unwrap().push(assistant.clone());
                let _ = self.session.lock().unwrap().assistant(&assistant);
            }
            if !content.is_empty() || !reasoning.is_empty() {
                let _ = self.events.send(HarnessEvent::AssistantMessage {
                    content,
                    reasoning: (!reasoning.is_empty()).then_some(reasoning),
                });
            }

            if calls.is_empty() {
                if let Some(chase) = self.todos.chase_message() {
                    if chases < chase_limit {
                        chases += 1;
                        let pending: Vec<String> = self
                            .todos
                            .pending()
                            .into_iter()
                            .map(|t| format!("{} {}", t.id, t.content))
                            .collect();
                        let _ = self.events.send(HarnessEvent::Chase {
                            attempt: chases,
                            pending,
                        });
                        self.push_user(&chase);
                        continue;
                    }
                    let _ = self.events.send(HarnessEvent::Notice {
                        level: "warning".to_string(),
                        message: "todo chase limit reached; manual intervention required"
                            .to_string(),
                    });
                    let _ = self.events.send(HarnessEvent::TurnComplete {
                        turn: turn as u64,
                        stop_reason: "todo_incomplete".to_string(),
                    });
                    return Ok(());
                }
                let _ = self.events.send(HarnessEvent::TurnComplete {
                    turn: turn as u64,
                    stop_reason: "completed".to_string(),
                });
                return Ok(());
            }

            // Work resumed: the chase budget resets.
            chases = 0;
            let mut ctx = self.ctx.read().unwrap().clone();
            ctx.cancel = token.clone();
            let registry = self.tools.read().unwrap().clone();
            for call in &calls {
                if token.is_cancelled() {
                    break;
                }
                let parsed = serde_json::from_str::<Value>(&call.function.arguments);
                let event_args = match &parsed {
                    Ok(value) => value.clone(),
                    Err(_) => json!({ "invalid_json": call.function.arguments }),
                };
                let _ = self.events.send(HarnessEvent::ToolCall {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    arguments: event_args,
                });
                let output = match parsed {
                    Ok(Value::Object(map)) => {
                        let call_ctx = ctx.for_call(&call.id, &call.function.name);
                        registry
                            .run(&call.function.name, &call_ctx, Value::Object(map))
                            .await
                    }
                    Ok(_) => ToolOutput {
                        content: HarnessError::tool(
                            "E_INVALID_ARGUMENT",
                            "tool arguments must be a JSON object",
                        )
                        .to_model_json(),
                        is_error: true,
                        metadata: None,
                    },
                    Err(err) => ToolOutput {
                        content: HarnessError::tool(
                            "E_INVALID_ARGUMENT",
                            format!("arguments are not valid JSON: {err}"),
                        )
                        .to_model_json(),
                        is_error: true,
                        metadata: None,
                    },
                };
                let content = truncate_tool_output(&output.content);
                let _ = self.events.send(HarnessEvent::ToolResult {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    ok: !output.is_error,
                    content: content.clone(),
                    metadata: output.metadata.clone(),
                });
                let _ = self.session.lock().unwrap().tool_result(
                    &call.id,
                    &call.function.name,
                    !output.is_error,
                    &content,
                );
                if call.function.name == "todo" {
                    let items = self.todos.snapshot();
                    let _ = self.session.lock().unwrap().todos(&items);
                }
                let mut tool_message = ChatMessage::tool(call.id.clone(), content);
                tool_message.name = Some(call.function.name.clone());
                self.messages.write().unwrap().push(tool_message);
            }
            if token.is_cancelled() {
                let _ = self.events.send(HarnessEvent::TurnComplete {
                    turn: turn as u64,
                    stop_reason: "cancelled".to_string(),
                });
                return Ok(());
            }
        }
    }

    /// Ask the backend, retrying rate-limit failures at the configured cadence
    /// and limit (`agent.rateLimitRetries` / `agent.rateLimitIntervalSecs`).
    /// Quota/credit exhaustion is terminal: no amount of waiting refills it,
    /// so the error surfaces immediately with an explicit notice.
    async fn stream_with_retry(
        &self,
        request: ChatRequest,
        sink: mpsc::UnboundedSender<StreamEvent>,
        token: CancellationToken,
        turn: usize,
    ) -> Result<Completion> {
        let max_retries = self.config.agent.rate_limit_retries;
        let interval =
            std::time::Duration::from_secs(self.config.agent.rate_limit_interval_secs.max(1));
        let mut attempt = 0u32;
        loop {
            let result = self
                .backend
                .stream_chat(request.clone(), Some(sink.clone()), token.clone())
                .await;
            let err = match result {
                Ok(completion) => return Ok(completion),
                Err(err) => err,
            };
            let text = err.to_string();
            if token.is_cancelled() || !crate::provider::is_rate_limited(&text) {
                return Err(err);
            }
            if crate::provider::is_quota_exhausted(&text) {
                let _ = self.events.send(HarnessEvent::Notice {
                    level: "error".to_string(),
                    message: "account quota/credits exhausted — automatic retry disabled; \
                              top up or switch provider/model (/model)"
                        .to_string(),
                });
                return Err(err);
            }
            attempt += 1;
            if let Some(max) = max_retries {
                if attempt > max {
                    let _ = self.events.send(HarnessEvent::Notice {
                        level: "error".to_string(),
                        message: format!(
                            "rate limited (429) during turn {turn}; retry limit ({max}) reached"
                        ),
                    });
                    return Err(err);
                }
            }
            if attempt == 1 || attempt.is_multiple_of(5) {
                let limit = max_retries.map(|max| format!("/{max}")).unwrap_or_default();
                let _ = self.events.send(HarnessEvent::Notice {
                    level: "warning".to_string(),
                    message: format!(
                        "rate limited (429) during turn {turn}; retrying in {}s (attempt {attempt}{limit})",
                        interval.as_secs()
                    ),
                });
            }
            tokio::select! {
                _ = token.cancelled() => return Err(err),
                _ = tokio::time::sleep(interval) => {}
            }
        }
    }

    fn report_usage(&self, completion: &Completion) {
        let Some(usage) = completion.usage else {
            if self.usage.note_missing_usage() {
                let _ = self.events.send(HarnessEvent::Notice {
                    level: "warning".to_string(),
                    message:
                        "provider returned no token usage; counts are unavailable \
                              (enable streaming usage in the provider compat settings if supported)"
                            .to_string(),
                });
            }
            return;
        };
        self.note_cache_miss(&usage);
        self.usage.record(&usage);
        if let Ok(mut session) = self.session.lock() {
            let _ = session.usage(&usage, self.usage.total_cost_usd());
        }
        let _ = self.events.send(HarnessEvent::TokenUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            context_tokens: self.usage.context_tokens(),
            context_limit: self.context_limit(),
            cost_usd: self.usage.total_cost_usd(),
            cache_hit_pct: self.usage.cache_hit_pct(),
        });
    }

    /// Tell the user when a turn re-billed prompt tokens the previous turn had
    /// already cached. Only observable causes are named: an idle gap past the
    /// model's cache lifetime, or a model switch.
    fn note_cache_miss(&self, usage: &crate::provider::Usage) {
        let Some(miss) = self.cache_watch.lock().unwrap().observe(
            &self.agent_model.read().unwrap().clone(),
            usage,
            Instant::now(),
        ) else {
            return;
        };
        if !self.show_cache_miss_notices.load(Ordering::Relaxed) || !miss.is_significant() {
            return;
        }
        let ttl = self
            .config
            .model_cache_ttl_secs(&self.agent_model.read().unwrap().clone());
        let cause = if miss.model_changed {
            "after a model switch".to_string()
        } else if ttl.is_some_and(|secs| miss.idle.as_secs() >= secs) {
            format!("after {}m idle", miss.idle.as_secs() / 60)
        } else {
            String::new()
        };
        let _ = self.events.send(HarnessEvent::Notice {
            level: "cache".to_string(),
            message: format!(
                "cache miss{cause}: {:.1}k tokens re-billed{}",
                miss.missed_tokens as f64 / 1000.0,
                match ttl {
                    Some(secs) => format!(" (model cache lifetime about {}m)", secs / 60),
                    None => String::new(),
                }
            ),
        });
    }

    /// Toggle cache-miss notices at runtime.
    pub fn set_show_cache_miss_notices(&self, on: bool) -> Result<()> {
        self.config
            .save_setting(&["agent", "showCacheMissNotices"], json!(on))?;
        self.show_cache_miss_notices.store(on, Ordering::Relaxed);
        Ok(())
    }

    pub fn show_cache_miss_notices(&self) -> bool {
        self.show_cache_miss_notices.load(Ordering::Relaxed)
    }

    async fn maybe_compact(
        &self,
        token: &CancellationToken,
        tools: &[crate::provider::ToolDefinition],
    ) -> Result<()> {
        let settings = self.compaction_settings();
        let compactor = Compactor::new(settings, self.context_limit(), self.usage.clone());
        let messages = self.messages.read().unwrap().clone();
        if !compactor.needs_compaction() {
            return Ok(());
        }
        let Some(plan) = compactor.plan(&messages) else {
            return Ok(());
        };
        if token.is_cancelled() {
            return Ok(());
        }
        let compact_model = self.compact_model.read().unwrap().clone();
        let mode = if self.compact_is_separate() {
            SummaryMode::Separate
        } else {
            SummaryMode::Cached
        };
        let summary = compactor
            .summarize(
                self.backend.as_ref(),
                SummaryRequest {
                    model: &compact_model,
                    messages: &messages,
                    plan: &plan,
                    tools: tools.to_vec(),
                    mode,
                },
                token,
            )
            .await?;
        let new_messages = compactor.apply(&messages, &plan, &summary);
        *self.messages.write().unwrap() = new_messages;
        // The last API report described the pre-compaction request. Clearing
        // it prevents an immediate re-trigger from stale numbers; the next
        // model response reports the new context size. The prompt was
        // rewritten, so the next request is new content, not a cache miss.
        self.usage.reset_context();
        self.cache_watch.lock().unwrap().reset();
        let _ = self
            .session
            .lock()
            .unwrap()
            .compaction(&summary, plan.dropped);
        let _ = self.events.send(HarnessEvent::Compaction {
            summary,
            dropped_messages: plan.dropped,
        });
        let _ = self.events.send(HarnessEvent::TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            context_tokens: 0,
            context_limit: self.context_limit(),
            cost_usd: self.usage.total_cost_usd(),
            cache_hit_pct: None,
        });
        Ok(())
    }
}

fn truncate_tool_output(content: &str) -> String {
    const MAX_CHARS: usize = 256_000;
    if content.chars().count() <= MAX_CHARS {
        return content.to_string();
    }
    let mut out: String = content.chars().take(MAX_CHARS).collect();
    out.push_str("\n... [tool output truncated by harness]");
    out
}

/// System prompt shared by every frontend. Tool-specific details live in the
/// tool descriptions; this only carries cross-tool policy.
pub fn system_prompt(model: &str) -> String {
    format!(
        "You are RustedHarness, an autonomous AI coding agent. You work inside a \
         single workspace and use tools to inspect and modify it. Model: {model}.\n\n\
         Operating rules:\n\
         1. Paths: relative paths are anchored at the session cwd; never access \
         outside the workspace root.\n\
         2. Reading: `read` returns `line#anchor|content`. Line numbers select the \
         edit position; the anchor verifies that line's content. Pass it, e.g. \
         pos=\"12#AB\", to `edit`. An edit changes only that line's anchor; inserts \
         and deletes shift line numbers, so use the anchors returned by the edit.\n\
         3. Editing: prefer `edit` with line-numbered edits[] applied bottom-up. \
         `lines` is the new content only: strip the `line#anchor|` prefix from \
         read/grep output and never paste diff markers into it. A stale anchor rejects \
         the whole batch (E_STALE_ANCHOR) and returns the current anchor table; write \
         nothing in that case and re-read.\n\
         4. Verify changes (build/tests) before claiming success. Report failures \
         honestly and keep responses concise."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FunctionCall, ToolCall, Usage};
    use async_trait::async_trait;

    struct Fake {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Backend for Fake {
        async fn stream_chat(
            &self,
            _request: ChatRequest,
            sink: Option<UnboundedSender<StreamEvent>>,
            _cancel: CancellationToken,
        ) -> Result<Completion> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(Completion {
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "write".into(),
                            arguments: json!({ "path": "hello.txt", "content": "hi\n" })
                                .to_string(),
                        },
                    }],
                    usage: Some(Usage {
                        prompt_tokens: 100,
                        completion_tokens: 10,
                        total_tokens: 110,
                        cached_tokens: 0,
                        cost_usd: None,
                    }),
                    finish_reason: Some("tool_calls".into()),
                    ..Default::default()
                })
            } else {
                if let Some(sink) = &sink {
                    let _ = sink.send(StreamEvent::Delta("done".into()));
                }
                Ok(Completion {
                    content: "done".into(),
                    usage: Some(Usage {
                        prompt_tokens: 120,
                        completion_tokens: 2,
                        total_tokens: 122,
                        cached_tokens: 0,
                        cost_usd: None,
                    }),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                })
            }
        }
    }

    struct Recording {
        calls: std::sync::atomic::AtomicUsize,
        requests: std::sync::Mutex<Vec<ChatRequest>>,
    }

    #[async_trait]
    impl Backend for Recording {
        async fn stream_chat(
            &self,
            request: ChatRequest,
            _sink: Option<UnboundedSender<StreamEvent>>,
            _cancel: CancellationToken,
        ) -> Result<Completion> {
            self.requests.lock().unwrap().push(request);
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(Completion {
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "write".into(),
                            arguments: r#"{ "path" : "stable.txt", "content" : "ok\n" }"#
                                .to_string(),
                        },
                    }],
                    finish_reason: Some("tool_calls".into()),
                    ..Default::default()
                })
            } else {
                Ok(Completion {
                    content: "done".into(),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                })
            }
        }
    }

    fn test_config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.session.dir = Some(dir.join("sessions"));
        config.lsp.enabled = false;
        config.mcp.enabled = false;
        config.agent.max_turns = 5;
        config.context_window = 100_000;
        config.model = "test/model".into();
        config.compact_model = "test/model".into();
        config
    }

    struct RateLimited;

    #[async_trait]
    impl Backend for RateLimited {
        async fn stream_chat(
            &self,
            _request: ChatRequest,
            _sink: Option<UnboundedSender<StreamEvent>>,
            _cancel: CancellationToken,
        ) -> Result<Completion> {
            Err(HarnessError::tool(
                "E_RATE_LIMITED",
                "429 Too Many Requests",
            ))
        }
    }

    struct ShellFake {
        calls: std::sync::atomic::AtomicUsize,
        tool: &'static str,
    }

    #[async_trait]
    impl Backend for ShellFake {
        async fn stream_chat(
            &self,
            _request: ChatRequest,
            _sink: Option<UnboundedSender<StreamEvent>>,
            _cancel: CancellationToken,
        ) -> Result<Completion> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(Completion {
                    tool_calls: vec![ToolCall {
                        id: "sh1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: self.tool.to_string(),
                            arguments: json!({
                                "command": "printf 'stream-a\\n'; sleep 0.35; printf 'stream-b\\n'"
                            })
                            .to_string(),
                        },
                    }],
                    usage: Some(Usage {
                        prompt_tokens: 10,
                        completion_tokens: 5,
                        total_tokens: 15,
                        cached_tokens: 0,
                        cost_usd: None,
                    }),
                    finish_reason: Some("tool_calls".into()),
                    ..Default::default()
                })
            } else {
                Ok(Completion {
                    content: "done".into(),
                    usage: Some(Usage {
                        prompt_tokens: 20,
                        completion_tokens: 2,
                        total_tokens: 22,
                        cached_tokens: 0,
                        cost_usd: None,
                    }),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                })
            }
        }
    }

    /// Two turns whose prompts mostly miss the cache: the second reports the
    /// tokens the first had already sent.
    struct CacheMiss {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Backend for CacheMiss {
        async fn stream_chat(
            &self,
            _request: ChatRequest,
            _sink: Option<UnboundedSender<StreamEvent>>,
            _cancel: CancellationToken,
        ) -> Result<Completion> {
            let prompt_tokens = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                50_000
            } else {
                60_000
            };
            let cached_tokens = if prompt_tokens == 50_000 { 0 } else { 5_000 };
            Ok(Completion {
                content: "ok".into(),
                usage: Some(Usage {
                    prompt_tokens,
                    completion_tokens: 2,
                    total_tokens: prompt_tokens + 2,
                    cached_tokens,
                    cost_usd: None,
                }),
                finish_reason: Some("stop".into()),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn a_rebilled_prompt_emits_a_cache_notice() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend = Arc::new(CacheMiss {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let agent = Agent::with_backend(config, dir.path().to_path_buf(), tx, backend)
            .await
            .unwrap();
        agent.send("first").await.unwrap();
        agent.send("second").await.unwrap();
        agent.shutdown().await;

        let mut messages = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let HarnessEvent::Notice { level, message } = event {
                assert_eq!(level, "cache");
                messages.push(message);
            }
        }
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("45.0k tokens re-billed"),
            "{messages:?}"
        );
    }

    #[tokio::test]
    async fn request_prefix_is_stable_across_tool_rounds_and_turns() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let (tx, _rx) = mpsc::unbounded_channel();
        let backend = Arc::new(Recording {
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: std::sync::Mutex::new(Vec::new()),
        });
        let agent = Agent::with_backend(config, dir.path().to_path_buf(), tx, backend.clone())
            .await
            .unwrap();
        agent.send("write the file").await.unwrap();
        agent.send("again").await.unwrap();
        let requests = backend.requests.lock().unwrap().clone();
        agent.shutdown().await;

        assert_eq!(requests.len(), 3);
        let assistant = requests[1]
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .expect("assistant tool call");
        assert_eq!(
            assistant.tool_calls.as_ref().unwrap()[0].function.arguments,
            r#"{"content":"ok\n","path":"stable.txt"}"#,
            "live tool-call arguments use the same canonical shape as session replay"
        );
        for pair in requests.windows(2) {
            let first = &pair[0];
            let second = &pair[1];
            assert!(second.messages.len() > first.messages.len());
            assert_eq!(
                serde_json::to_string(&second.messages[..first.messages.len()]).unwrap(),
                serde_json::to_string(&first.messages).unwrap(),
                "each request must extend the previous request byte-for-byte"
            );
            assert_eq!(first.tools, second.tools);
            assert_eq!(first.model, second.model);
            assert_eq!(first.temperature, second.temperature);
            assert_eq!(first.max_tokens, second.max_tokens);
            assert_eq!(first.stream, second.stream);
        }
    }

    #[tokio::test]
    async fn shell_tool_streams_events_correlated_by_call_id() {
        if cfg!(windows) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let tool = crate::shell::ShellSpec::detect(&config.shell.prefer).tool_name();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend = Arc::new(ShellFake {
            calls: std::sync::atomic::AtomicUsize::new(0),
            tool,
        });
        let agent = Agent::with_backend(config, dir.path().to_path_buf(), tx, backend)
            .await
            .unwrap();
        agent.send("stream a command").await.unwrap();
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let live: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                HarnessEvent::ToolOutput { id, text, .. } if id == "sh1" => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(!live.is_empty(), "expected live tool output events");
        assert!(
            live.iter().any(|text| text.contains("stream-a")),
            "early output missing from live stream: {live:?}"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            HarnessEvent::ToolResult { id, ok: true, content, .. }
                if id == "sh1" && content.contains("stream-b")
        )));
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn rate_limit_retries_stop_at_the_configured_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.agent.rate_limit_retries = Some(0);
        config.agent.rate_limit_interval_secs = 1;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let agent =
            Agent::with_backend(config, dir.path().to_path_buf(), tx, Arc::new(RateLimited))
                .await
                .unwrap();
        let result = agent.send("hello").await;
        assert!(
            result.is_err(),
            "expected the retry limit to surface an error"
        );
        let mut notices = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let HarnessEvent::Notice { message, .. } = event {
                notices.push(message);
            }
        }
        assert!(
            notices
                .iter()
                .any(|m| m.contains("retry limit (0) reached")),
            "missing retry-limit notice: {notices:?}"
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn an_unconfigured_model_is_reported_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.session.dir = Some(dir.path().join("sessions"));
        config.lsp.enabled = false;
        config.mcp.enabled = false;
        let (tx, _rx) = mpsc::unbounded_channel();
        let backend = Arc::new(Fake {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let agent = Agent::with_backend(config, dir.path().to_path_buf(), tx, backend.clone())
            .await
            .unwrap();
        let err = agent.send("hello").await.unwrap_err().to_string();
        agent.shutdown().await;
        assert!(err.contains("no model configured"), "{err}");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn runs_tools_and_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend = Arc::new(Fake {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let agent = Agent::with_backend(config, dir.path().to_path_buf(), tx, backend)
            .await
            .unwrap();
        agent.send("create hello").await.unwrap();
        let written = std::fs::read_to_string(dir.path().join("hello.txt")).unwrap();
        assert_eq!(written, "hi\n");
        assert!(agent.messages().iter().any(|m| m.role == Role::Tool
            && m.content
                .as_deref()
                .is_some_and(|c| c.contains("hello.txt"))));
        assert!(agent.snapshot().messages.last().unwrap().content.as_deref() == Some("done"));

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, HarnessEvent::ToolResult { ok: true, .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            HarnessEvent::TurnComplete { stop_reason, .. } if stop_reason == "completed"
        )));
        let usage_events: Vec<(u64, u64, u64)> = events
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::TokenUsage {
                    input_tokens,
                    output_tokens,
                    context_tokens,
                    ..
                } => Some((*input_tokens, *output_tokens, *context_tokens)),
                _ => None,
            })
            .collect();
        assert_eq!(usage_events, vec![(100, 10, 110), (120, 2, 122)]);
        assert_eq!(agent.snapshot().context_tokens, 122);
        agent.shutdown().await;
    }

    #[test]
    fn prompt_contains_anchor_rules() {
        let prompt = system_prompt("test-model");
        assert!(!prompt.contains("<env>"));
        assert!(prompt.contains("line#anchor|content"));
        assert!(prompt.contains("E_STALE_ANCHOR"));
        assert!(prompt.contains("test-model"));
    }
}
