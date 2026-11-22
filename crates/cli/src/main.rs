//! `rustedharness` command line entry point.
//!
//! Subcommands are deliberately thin: all session/agent logic lives in
//! `rustedharness-core`, the terminal UI in `rustedharness-tui`, and the web
//! server in `rustedharness-web`.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rustedharness_core::agent::Agent;
use rustedharness_core::config::model_display;
use rustedharness_core::lsp::{registry, LspHub};
use rustedharness_core::protocol::HarnessEvent;
use rustedharness_core::session::SessionStore;
use rustedharness_core::{Config, VERSION};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    name = "rustedharness",
    version = VERSION,
    about = "RustedHarness — an autonomous coding agent with anchored edits"
)]
struct Cli {
    /// Config file or directory (defaults to `~/.rustedharness`).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Workspace directory (defaults to the current directory).
    #[arg(long, global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Override the agent model.
    #[arg(long, global = true, value_name = "MODEL")]
    model: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one prompt (or a line-oriented REPL) without the TUI.
    Run(RunArgs),
    /// Serve the web UI and REST/WebSocket API.
    Serve(ServeArgs),
    /// Open the full-screen terminal UI (default when no subcommand is given).
    Tui(TuiArgs),
    /// Inspect and install language servers.
    Lsp(LspArgs),
    /// List, show or delete saved sessions.
    Sessions(SessionsArgs),
    /// Configuration helpers.
    Config(ConfigArgs),
}

#[derive(clap::Args)]
struct RunArgs {
    /// Prompt words. With no prompt and a TTY, starts a small REPL.
    prompt: Vec<String>,
    /// Resume a session by id or unique prefix.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
    /// Emit one JSON event per line.
    #[arg(long)]
    json: bool,
    /// Approve language-server downloads without asking.
    #[arg(long)]
    yes: bool,
}

#[derive(clap::Args)]
struct ServeArgs {
    /// Bind address (defaults to config web.host).
    #[arg(long, value_name = "HOST")]
    host: Option<String>,
    /// Port (defaults to config web.port).
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,
    /// Bearer token protecting the API (defaults to config web.auth_token).
    #[arg(long, value_name = "TOKEN", env = "RUSTEDHARNESS_TOKEN")]
    token: Option<String>,
    /// Open the UI in the default browser.
    #[arg(long)]
    open: bool,
}

#[derive(clap::Args)]
struct TuiArgs {
    /// Resume a session by id or unique prefix.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
}

#[derive(clap::Args)]
struct LspArgs {
    #[command(subcommand)]
    command: LspCommand,
}

#[derive(Subcommand)]
enum LspCommand {
    /// Show the built-in server registry and availability.
    List,
    /// Install the newest release of a server.
    Install {
        /// Server id, e.g. rust-analyzer.
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(lsp_server_ids()))]
        server: String,
    },
    /// Re-install a server from its newest release.
    Update {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(lsp_server_ids()))]
        server: String,
    },
}

/// Registry ids, so clap validates the argument and lists what exists.
fn lsp_server_ids() -> Vec<&'static str> {
    rustedharness_core::lsp::registry()
        .iter()
        .map(|spec| spec.id)
        .collect()
}

#[derive(clap::Args)]
struct SessionsArgs {
    #[command(subcommand)]
    command: SessionsCommand,
}

#[derive(Subcommand)]
enum SessionsCommand {
    /// List saved sessions, most recently updated first.
    List,
    /// Print the messages of one session.
    Show { id: String },
    /// Delete one session.
    Delete {
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Write config.json and models.json if they do not exist.
    Init,
    /// Print the config directory and file paths.
    Path,
    /// Print the effective config (credentials redacted).
    Show,
    /// List configured providers and model references.
    Models,
    /// Open a config file in $VISUAL/$EDITOR (or the platform default).
    Edit {
        #[arg(value_enum, default_value_t = ConfigFile::Config)]
        file: ConfigFile,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ConfigFile {
    /// config.json (model selection and harness settings).
    Config,
    /// models.json (providers and models).
    Models,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config(&cli)?;
    let cwd = cli
        .cwd
        .clone()
        .unwrap_or(std::env::current_dir().context("cannot read current directory")?);

    match cli.command {
        None | Some(Command::Tui(_)) => {
            let resume = match &cli.command {
                Some(Command::Tui(args)) => args.resume.clone(),
                _ => None,
            };
            if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "the TUI needs an interactive terminal; use `rustedharness run \"...\"` for headless use"
                );
            }
            rustedharness_tui::run(config, cwd, resume).await
        }
        Some(Command::Run(args)) => run_headless(config, cwd, args).await,
        Some(Command::Serve(args)) => {
            let host = args.host.unwrap_or_else(|| config.web.host.clone());
            let port = args.port.unwrap_or(config.web.port);
            let token = args.token.or_else(|| config.web.auth_token.clone());
            rustedharness_web::serve(config, cwd, host, port, token, args.open).await
        }
        Some(Command::Lsp(args)) => lsp_command(config, cwd, args).await,
        Some(Command::Sessions(args)) => sessions_command(config, args),
        Some(Command::Config(args)) => config_command(config, args),
    }
}

fn load_config(cli: &Cli) -> Result<Config> {
    let mut config = match &cli.config {
        Some(path) => Config::load_from(path)
            .with_context(|| format!("cannot load config {}", path.display()))?,
        None => Config::load().context("cannot load configuration")?,
    };
    if let Some(model) = &cli.model {
        config
            .set_agent_model(model)
            .with_context(|| format!("unknown model `{model}`"))?;
    }
    Ok(config)
}

async fn run_headless(config: Config, cwd: PathBuf, args: RunArgs) -> Result<()> {
    if args.resume.is_none() && config.model.trim().is_empty() {
        anyhow::bail!(
            "no model configured; add a provider/model to models.json and set `model` in config.json"
        );
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = match &args.resume {
        Some(id) => Agent::resume(config, id, Some(cwd), tx).await?,
        None => Agent::new(config, cwd, tx).await?,
    };

    let piped = !std::io::stdin().is_terminal();
    let prompt = args.prompt.join(" ");
    let oneshot = if !prompt.trim().is_empty() {
        Some(prompt)
    } else if piped {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        (!input.trim().is_empty()).then_some(input)
    } else {
        None
    };

    let result = if let Some(prompt) = oneshot {
        let stop = run_prompt(&agent, &mut rx, prompt, args.json, args.yes).await?;
        if stop == "error" {
            anyhow::bail!("turn failed");
        }
        Ok(())
    } else {
        repl(&agent, &mut rx, args.json, args.yes).await
    };

    agent.shutdown().await;
    result
}

async fn repl(
    agent: &Arc<Agent>,
    rx: &mut mpsc::UnboundedReceiver<HarnessEvent>,
    json: bool,
    yes: bool,
) -> Result<()> {
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        if !json {
            print!("\x1b[36m›\x1b[0m ");
            std::io::stdout().flush().ok();
        }
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        match text {
            "/quit" | "/exit" | "exit" | "quit" => break,
            "/cancel" => {
                agent.cancel();
                continue;
            }
            "/help" => {
                eprintln!(
                    "commands: /cancel, /model <name>, /cwd <dir>, /quit\n\
                     anything else is sent as a user message"
                );
                continue;
            }
            _ => {}
        }
        if let Some(model) = text.strip_prefix("/model ") {
            match agent.set_agent_model(model.trim()) {
                Ok(()) => eprintln!("model: {}", agent.agent_model()),
                Err(err) => eprintln!("error: {err}"),
            }
            continue;
        }
        if let Some(path) = text.strip_prefix("/cwd ") {
            match agent.set_cwd(Path::new(path.trim())).await {
                Ok(()) => eprintln!("cwd: {}", agent.cwd().display()),
                Err(err) => eprintln!("error: {err}"),
            }
            continue;
        }
        run_prompt(agent, rx, text.to_string(), json, yes).await?;
    }
    Ok(())
}

/// Send one user prompt and print events until the turn completes. The send
/// task is joined before returning, so a failed turn is reported exactly once.
async fn run_prompt(
    agent: &Arc<Agent>,
    rx: &mut mpsc::UnboundedReceiver<HarnessEvent>,
    prompt: String,
    json: bool,
    yes: bool,
) -> Result<String> {
    let sender = agent.clone();
    let send = tokio::spawn(async move { sender.send(prompt).await });
    let stop = pump(agent, rx, json, yes).await?;
    match send.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("error: {err}"),
        Err(err) => eprintln!("agent task failed: {err}"),
    }
    Ok(stop)
}

/// Print events until the current turn completes. Returns the stop reason.
async fn pump(
    agent: &Arc<Agent>,
    rx: &mut mpsc::UnboundedReceiver<HarnessEvent>,
    json: bool,
    yes: bool,
) -> Result<String> {
    let mut printed_delta = false;
    let mut printed_reasoning = false;
    loop {
        let Some(event) = rx.recv().await else {
            return Ok("channel_closed".to_string());
        };
        if json {
            println!("{}", serde_json::to_string(&event)?);
            if let HarnessEvent::DownloadConsent { server_id, .. } = &event {
                if yes {
                    agent.grant_download(server_id, true);
                }
            }
            if let HarnessEvent::TurnComplete { stop_reason, .. } = &event {
                return Ok(stop_reason.clone());
            }
            continue;
        }

        match event {
            HarnessEvent::SessionStarted {
                session_id,
                cwd,
                agent_model,
                ..
            } => {
                eprintln!(
                    "session {session_id} · {} · {}",
                    model_display(&agent_model),
                    cwd
                );
            }
            HarnessEvent::UserMessage { .. } => {}
            HarnessEvent::AssistantDelta { text } => {
                printed_delta = true;
                print!("{text}");
                std::io::stdout().flush().ok();
            }
            HarnessEvent::AssistantReasoning { text } => {
                if !printed_reasoning {
                    eprint!("\x1b[2m");
                    printed_reasoning = true;
                }
                eprint!("{text}");
            }
            HarnessEvent::AssistantMessage { .. } => {
                if printed_reasoning {
                    eprint!("\x1b[0m");
                    printed_reasoning = false;
                }
                if printed_delta {
                    println!();
                    printed_delta = false;
                }
            }
            HarnessEvent::ToolCall {
                name, arguments, ..
            } => {
                eprintln!(
                    "\x1b[33m⚙ {name}\x1b[0m {}",
                    truncate(&arguments.to_string(), 160)
                );
            }
            HarnessEvent::ToolOutput { .. } => {
                // The one-shot CLI prints the final tool result only; live
                // chunks are rendered by the TUI and web UI.
            }
            HarnessEvent::ToolResult {
                name, ok, content, ..
            } => {
                let marker = if ok { "✔" } else { "✘" };
                let color = if ok { "32" } else { "31" };
                let preview = truncate(content.trim(), 600);
                eprintln!("\x1b[{color}m{marker} {name}\x1b[0m {preview}");
            }
            HarnessEvent::TodoUpdated { items } => {
                if !items.is_empty() {
                    eprintln!("todos:");
                    for item in items {
                        eprintln!("  {} {} ({})", item.status.symbol(), item.content, item.id);
                    }
                }
            }
            HarnessEvent::Notice { level, message } => {
                eprintln!("\x1b[2m[{level}] {message}\x1b[0m");
            }
            HarnessEvent::Error { message } => {
                eprintln!("\x1b[31merror:\x1b[0m {message}");
            }
            HarnessEvent::Compaction {
                summary,
                dropped_messages,
            } => {
                eprintln!(
                    "\x1b[2m[compacted {dropped_messages} messages]\x1b[0m {}",
                    truncate(summary.trim(), 400)
                );
            }
            HarnessEvent::Chase { attempt, pending } => {
                eprintln!(
                    "\x1b[2m[todo chase {attempt}; {} pending]\x1b[0m",
                    pending.len()
                );
            }
            HarnessEvent::DownloadConsent {
                server_id,
                display_name,
                version,
                url,
                runtime_hint,
                ..
            } => {
                if yes {
                    eprintln!("allowing download of {display_name} {version}");
                    agent.grant_download(&server_id, true);
                    continue;
                }
                eprintln!("\n{display_name} {version} is not installed.\n  {url}");
                if let Some(hint) = runtime_hint {
                    eprintln!("  runtime: {hint}");
                }
                eprint!("Download it? [y/N] ");
                std::io::stderr().flush().ok();
                let mut answer = String::new();
                let allow = std::io::stdin()
                    .read_line(&mut answer)
                    .map(|_| matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
                    .unwrap_or(false);
                agent.grant_download(&server_id, allow);
            }
            HarnessEvent::DownloadStatus {
                server_id,
                state,
                detail,
                ..
            } => {
                let hint = match state.as_str() {
                    "update_available" => format!(" - run: rustedharness lsp update {server_id}"),
                    _ => String::new(),
                };
                eprintln!(
                    "\x1b[2m[lsp download {server_id}: {state}]\x1b[0m{}{hint}",
                    detail.map(|d| format!(" {d}")).unwrap_or_default()
                );
            }
            HarnessEvent::LspStatus {
                language,
                state,
                detail,
            } => {
                if state == "degraded" || state == "absent" {
                    eprintln!(
                        "\x1b[2m[lsp {language}: {state}]\x1b[0m{}",
                        detail.map(|d| format!(" {d}")).unwrap_or_default()
                    );
                }
            }
            HarnessEvent::ProcessExited {
                pid,
                status,
                exit_code,
            } => {
                eprintln!(
                    "\x1b[2m[process {pid} {status}{}]\x1b[0m",
                    exit_code.map(|c| format!(", exit {c}")).unwrap_or_default()
                );
            }
            HarnessEvent::ProcessSnapshot { .. } | HarnessEvent::TokenUsage { .. } => {}
            HarnessEvent::TurnComplete { stop_reason, turn } => {
                if printed_reasoning {
                    eprint!("\x1b[0m");
                }
                if stop_reason != "completed" {
                    eprintln!("\x1b[2m[turn {turn} {stop_reason}]\x1b[0m");
                }
                return Ok(stop_reason);
            }
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

async fn lsp_command(config: Config, cwd: PathBuf, args: LspArgs) -> Result<()> {
    match args.command {
        LspCommand::List => {
            let hub = LspHub::new(cwd, &config.lsp, None);
            let statuses = hub.statuses();
            println!(
                "{:<14} {:<20} {:<12} {:<10} INSTALL",
                "LANGUAGE", "SERVER", "INSTALLED", "STATE"
            );
            for status in statuses {
                println!(
                    "{:<14} {:<20} {:<12} {:<10} {}",
                    status.language,
                    status.server,
                    status.version.as_deref().unwrap_or("-"),
                    status.state,
                    status.install_hint
                );
            }
            println!(
                "{} servers in the built-in registry. Downloads take the newest GitHub release.",
                registry().len()
            );
            hub.shutdown_all().await;
            Ok(())
        }
        LspCommand::Install { server } => lsp_install(config, cwd, &server, false).await,
        LspCommand::Update { server } => lsp_install(config, cwd, &server, true).await,
    }
}

async fn lsp_install(config: Config, cwd: PathBuf, server: &str, update: bool) -> Result<()> {
    let lsp = config.lsp;
    // Progress goes to stderr so the caller's stdout stays clean. Without a
    // sink a 50 MB download prints nothing at all and reads as a hang.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = tokio::spawn(async move {
        // A line per 10% is enough on a fast link, but a throttled mirror can
        // take minutes per 10%, so print at least every few seconds as well.
        let mut last_step: Option<u8> = None;
        let mut last_print = std::time::Instant::now();
        while let Some(event) = rx.recv().await {
            let HarnessEvent::DownloadStatus {
                server_id,
                state,
                detail,
                percent,
            } = event
            else {
                continue;
            };
            if state == "downloading" && percent.is_some() {
                let step = percent.map(|percent| percent / 10);
                if step == last_step && last_print.elapsed() < std::time::Duration::from_secs(5) {
                    continue;
                }
                last_step = step;
                last_print = std::time::Instant::now();
            }
            eprintln!(
                "\x1b[2m[lsp {server_id}: {state}]\x1b[0m{}",
                detail
                    .map(|detail| format!(" {detail}"))
                    .unwrap_or_default()
            );
        }
    });
    let result =
        rustedharness_core::lsp::install_server(&lsp, &cwd, server, update, Some(tx)).await;
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), sink).await;
    let path = result?;
    println!("installed {server}: {}", path.display());
    Ok(())
}

fn sessions_command(config: Config, args: SessionsArgs) -> Result<()> {
    let store = SessionStore::new(&config.session);
    match args.command {
        SessionsCommand::List => {
            let sessions = store.list();
            if sessions.is_empty() {
                println!("no saved sessions in {}", store.dir().display());
                return Ok(());
            }
            println!(
                "{:<24} {:<12} {:<6} {:<12} CWD",
                "ID", "CREATED", "MSGS", "MODEL"
            );
            for s in sessions {
                println!(
                    "{:<24} {:<12} {:<6} {:<12} {}",
                    s.id,
                    s.created_at.chars().take(10).collect::<String>(),
                    s.messages,
                    model_display(&s.agent_model)
                        .chars()
                        .take(12)
                        .collect::<String>(),
                    s.cwd
                );
            }
            Ok(())
        }
        SessionsCommand::Show { id } => {
            let loaded = store.load(&id)?;
            if let Some(meta) = &loaded.meta {
                let compact = if meta.compact_model.trim().is_empty() {
                    String::new()
                } else {
                    format!(" → {}", model_display(&meta.compact_model))
                };
                println!(
                    "session {} · {} · {}{}",
                    meta.id,
                    meta.cwd,
                    model_display(&meta.agent_model),
                    compact
                );
            }
            if loaded.truncated_tail {
                println!("(ignored a torn final record)");
            }
            for message in loaded.messages {
                let role = match message.role {
                    rustedharness_core::provider::Role::System => "system",
                    rustedharness_core::provider::Role::User => "user",
                    rustedharness_core::provider::Role::Assistant => "assistant",
                    rustedharness_core::provider::Role::Tool => "tool",
                };
                if let Some(text) = &message.content {
                    println!("\n── {role} ──\n{}", text.trim());
                }
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        println!(
                            "\n── assistant → {}({}) ──",
                            call.function.name, call.function.arguments
                        );
                    }
                }
            }
            Ok(())
        }
        SessionsCommand::Delete { id, yes } => {
            let loaded = store.load(&id)?;
            let real = loaded.meta.map(|m| m.id).unwrap_or(id);
            if !yes {
                eprint!("delete session {real}? [y/N] ");
                std::io::stderr().flush().ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("cancelled");
                    return Ok(());
                }
            }
            store.delete(&real)?;
            println!("deleted {real}");
            Ok(())
        }
    }
}

fn config_command(config: Config, args: ConfigArgs) -> Result<()> {
    let source_dir = config.source_dir();
    match args.command {
        ConfigCommand::Init => {
            let path = Config::write_default_in(&source_dir)?;
            println!("config: {}", path.display());
            println!("models: {}", source_dir.join("models.json").display());
            println!("no model configured — add a provider/model to models.json and set `model`");
            Ok(())
        }
        ConfigCommand::Path => {
            println!("dir:    {}", source_dir.display());
            println!("config: {}", source_dir.join("config.json").display());
            println!("models: {}", source_dir.join("models.json").display());
            Ok(())
        }
        ConfigCommand::Show => {
            let (settings, models) = config.redacted().split_files();
            println!("# {}", source_dir.join("config.json").display());
            print!("{settings}");
            println!("# {}", source_dir.join("models.json").display());
            print!("{models}");
            Ok(())
        }
        ConfigCommand::Models => {
            let default = config.resolve_model(&config.model).ok();
            let compact = config.resolve_model(&config.compact_model).ok();
            println!(
                "default: {}  (compact: {})",
                default
                    .as_ref()
                    .map(|model| model.reference())
                    .unwrap_or_else(|| "None".to_string()),
                compact
                    .as_ref()
                    .map(|model| model.reference())
                    .unwrap_or_else(|| "None".to_string())
            );
            println!("     {:<40} {:>9}  NAME", "MODEL", "CONTEXT");
            let catalog = config.model_catalog();
            if catalog.is_empty() {
                println!("no models configured — add a provider/model to models.json");
            }
            for model in catalog {
                let marker = if default
                    .as_ref()
                    .is_some_and(|default| model.reference() == default.reference())
                {
                    "*"
                } else if compact
                    .as_ref()
                    .is_some_and(|compact| model.reference() == compact.reference())
                {
                    "c"
                } else {
                    " "
                };
                println!(
                    "{marker:<4} {:<40} {:>9}  {}",
                    model.reference(),
                    if model.context_window == 0 {
                        "None".to_string()
                    } else {
                        model.context_window.to_string()
                    },
                    model.name
                );
            }
            Ok(())
        }
        ConfigCommand::Edit { file } => {
            let path = match file {
                ConfigFile::Config => source_dir.join("config.json"),
                ConfigFile::Models => source_dir.join("models.json"),
            };
            if !path.exists() {
                Config::write_default_in(&source_dir)?;
            }
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| {
                    if cfg!(windows) {
                        "notepad".to_string()
                    } else if cfg!(target_os = "macos") {
                        "open".to_string()
                    } else {
                        "xdg-open".to_string()
                    }
                });
            let mut command = std::process::Command::new(&editor);
            if editor == "open" && cfg!(target_os = "macos") {
                command.arg("-e");
            }
            let status = command
                .arg(&path)
                .status()
                .with_context(|| format!("cannot open editor `{editor}`"))?;
            if !status.success() {
                anyhow::bail!("editor `{editor}` exited with {status}");
            }
            Ok(())
        }
    }
}
