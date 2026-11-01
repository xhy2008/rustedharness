//! HTTP/WebSocket server and embedded web UI.
//!
//! Every browser tab owns one agent session. The WebSocket carries the same
//! `HarnessEvent` / `HarnessCommand` protocol as the TUI, so new frontends do
//! not need any server-specific event mapping. REST endpoints expose saved
//! sessions and non-secret configuration.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::info;

use rustedharness_core::agent::Agent;
use rustedharness_core::config::Config;
use rustedharness_core::error::HarnessError;
use rustedharness_core::paths::WorkspacePathResolver;
use rustedharness_core::protocol::HarnessCommand;
use rustedharness_core::session::SessionStore;
use rustedharness_core::VERSION;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const TREE_LIMIT: usize = 500;
const FILE_LIMIT: usize = 256 * 1024;
const SKIP_DIRS: &[&str] = &["target", "node_modules", "dist", "build"];

struct AppState {
    config: Config,
    cwd: PathBuf,
    token: Option<String>,
    /// Per-session workspace roots so the file browser follows `/cwd`.
    cwds: Mutex<HashMap<String, PathBuf>>,
}

pub async fn serve(
    config: Config,
    cwd: PathBuf,
    host: String,
    port: u16,
    token: Option<String>,
    open: bool,
) -> anyhow::Result<()> {
    let token_required = token.is_some();
    let (local, handle) = spawn(config, cwd, host, port, token).await?;
    let url = display_url(local);
    println!("RustedHarness {VERSION} web UI: {url}");
    if token_required {
        println!("Authorization: bearer token required (enter it in the sidebar)");
    } else if !local.ip().is_loopback() {
        eprintln!(
            "warning: serving on {} without an access token; use --token before exposing this port",
            local.ip()
        );
    }
    if open {
        open_browser(&url);
    }
    info!(%local, "web server listening");
    handle.await?;
    Ok(())
}

/// Bind and start the server on a background task. Returns the bound address
/// and the task handle; used by `serve` and by the desktop shell, which picks
/// an ephemeral port.
pub async fn spawn(
    config: Config,
    cwd: PathBuf,
    host: String,
    port: u16,
    token: Option<String>,
) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    let local = listener.local_addr()?;
    let app = router(Arc::new(AppState {
        config,
        cwd,
        token,
        cwds: Mutex::new(HashMap::new()),
    }));
    let handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("web server stopped: {err}");
        }
    });
    Ok((local, handle))
}

fn display_url(addr: SocketAddr) -> String {
    let host = match addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_string(),
        ip => ip.to_string(),
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    format!("http://{host}:{}/", addr.port())
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(target_os = "windows")]
    let command = "cmd";
    #[cfg(all(unix, not(target_os = "macos")))]
    let command = "xdg-open";

    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new(command);
    #[cfg(target_os = "windows")]
    cmd.arg("/C").arg("start").arg("").arg(url);
    #[cfg(not(target_os = "windows"))]
    let mut cmd = std::process::Command::new(command);
    #[cfg(not(target_os = "windows"))]
    cmd.arg(url);

    if let Err(err) = cmd.spawn() {
        eprintln!("could not open a browser automatically: {err}");
    }
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/health", get(health))
        .route("/api/config", get(config_info))
        .route("/api/tree", get(workspace_tree))
        .route("/api/file", get(workspace_file))
        .route("/api/sessions", get(list_sessions))
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/ws", get(ws_handler))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": VERSION }))
}

async fn config_info(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(status) = authorized(&state, &headers) {
        return status.into_response();
    }
    let config = &state.config;
    let agent = config.resolve_model(&config.model).ok();
    let compact = config.resolve_model(&config.compact_model).ok();
    let provider = agent
        .as_ref()
        .map(|model| model.provider.clone())
        .unwrap_or_else(|| config.default_provider_id());
    let agent_model = agent
        .as_ref()
        .map(|model| model.reference())
        .unwrap_or_else(|| config.model.clone());
    let compact_model = compact
        .as_ref()
        .map(|model| model.reference())
        .unwrap_or_else(|| config.compact_model.clone());
    let context_window = agent
        .as_ref()
        .map(|model| model.context_window)
        .unwrap_or(config.context_window);
    Json(json!({
        "version": VERSION,
        "cwd": state.cwd.to_string_lossy(),
        "provider": provider,
        "agent_model": agent_model,
        "compact_model": compact_model,
        "context_window": context_window,
        "models": config.model_catalog(),
        "providers": config.providers.iter().map(|(id, provider)| json!({
            "id": id,
            "name": provider.name,
            "api": provider.api,
            "base_url": provider.base_url,
            "models": provider.models.iter().map(|model| &model.id).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "lsp_enabled": config.lsp.enabled,
        "mcp_enabled": config.mcp.enabled,
        "auth_required": state.token.is_some(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    #[serde(default)]
    session: String,
    #[serde(default)]
    path: String,
}

/// The browser can address a workspace per session, because `/cwd` moves the
/// agent (and its tree) without moving the server process.
fn workspace_root(state: &AppState, session: &str) -> PathBuf {
    if session.is_empty() {
        return state.cwd.clone();
    }
    state
        .cwds
        .lock()
        .ok()
        .and_then(|cwds| cwds.get(session).cloned())
        .unwrap_or_else(|| state.cwd.clone())
}

async fn workspace_tree(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> Response {
    if let Err(status) = authorized(&state, &headers) {
        return status.into_response();
    }
    let root = workspace_root(&state, &query.session);
    let resolver = match WorkspacePathResolver::new(&root) {
        Ok(resolver) => resolver,
        Err(err) => return core_error(err),
    };
    match tree_payload(&resolver, &query.path) {
        Ok(payload) => Json(payload).into_response(),
        Err(err) => core_error(err),
    }
}

async fn workspace_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> Response {
    if let Err(status) = authorized(&state, &headers) {
        return status.into_response();
    }
    let root = workspace_root(&state, &query.session);
    let resolver = match WorkspacePathResolver::new(&root) {
        Ok(resolver) => resolver,
        Err(err) => return core_error(err),
    };
    match file_payload(&resolver, &query.path) {
        Ok(payload) => Json(payload).into_response(),
        Err(err) => core_error(err),
    }
}

fn core_error(err: HarnessError) -> Response {
    let status = match err.code() {
        "E_NOT_FOUND" => StatusCode::NOT_FOUND,
        "E_PATH_ESCAPE" | "E_INVALID_CWD" => StatusCode::BAD_REQUEST,
        "E_IO" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(json!({ "error": err.to_string(), "code": err.code() })),
    )
        .into_response()
}

fn tree_payload(
    resolver: &WorkspacePathResolver,
    rel: &str,
) -> std::result::Result<Value, HarnessError> {
    let rel = rel.trim().trim_matches('/');
    let rel = if rel == "." { "" } else { rel };
    let dir = resolver.resolve_existing(rel)?;
    let metadata = fs::metadata(&dir).map_err(|err| io_error("cannot stat directory", err))?;
    if !metadata.is_dir() {
        return Err(HarnessError::tool(
            "E_NOT_DIR",
            format!("{} is not a directory", resolver.display_rel(&dir)),
        ));
    }

    let mut entries = Vec::new();
    let mut truncated = false;
    let read = fs::read_dir(&dir).map_err(|err| io_error("cannot read directory", err))?;
    for entry in read {
        if entries.len() >= TREE_LIMIT {
            truncated = true;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let file_type = match entry.file_type() {
            Ok(kind) => kind,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        let is_dir = file_type.is_dir();
        if name.starts_with('.') {
            continue;
        }
        if is_dir && SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let size = if is_dir {
            0
        } else {
            entry.metadata().map(|meta| meta.len()).unwrap_or(0)
        };
        entries.push((is_dir, name, path, size));
    }
    entries.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });

    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(dir, name, path, size)| {
            json!({
                "name": name,
                "path": path,
                "dir": dir,
                "size": size,
            })
        })
        .collect();
    Ok(json!({
        "path": if rel.is_empty() { "." } else { rel },
        "entries": entries,
        "truncated": truncated,
    }))
}

fn file_payload(
    resolver: &WorkspacePathResolver,
    rel: &str,
) -> std::result::Result<Value, HarnessError> {
    let path = resolver.resolve_existing(rel)?;
    let metadata = fs::metadata(&path).map_err(|err| io_error("cannot stat file", err))?;
    if !metadata.is_file() {
        return Err(HarnessError::tool(
            "E_NOT_FILE",
            format!("{} is not a regular file", resolver.display_rel(&path)),
        ));
    }
    let total = metadata.len();
    let mut file = fs::File::open(&path).map_err(|err| io_error("cannot open file", err))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(FILE_LIMIT as u64)
        .read_to_end(&mut bytes)
        .map_err(|err| io_error("cannot read file", err))?;

    let truncated = total > FILE_LIMIT as u64;
    let nul = bytes.iter().take(8192).any(|byte| *byte == 0);
    let (binary, content) = if nul {
        (true, String::new())
    } else {
        match std::str::from_utf8(&bytes) {
            Ok(text) => (false, text.to_string()),
            Err(err) if truncated && err.error_len().is_none() => {
                // The read cap sliced a multi-byte character in half.
                let valid = std::str::from_utf8(&bytes[..err.valid_up_to()]).unwrap_or("");
                (false, valid.to_string())
            }
            Err(_) => (true, String::new()),
        }
    };
    let language = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| resolver.display_rel(&path));
    Ok(json!({
        "path": resolver.display_rel(&path),
        "name": name,
        "size": total,
        "content": content,
        "binary": binary,
        "truncated": truncated,
        "language": language,
    }))
}

fn io_error(context: &str, err: std::io::Error) -> HarnessError {
    HarnessError::tool("E_IO", format!("{context}: {err}"))
}

async fn list_sessions(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(status) = authorized(&state, &headers) {
        return status.into_response();
    }
    let store = SessionStore::new(&state.config.session);
    let mut sessions = store.list();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Json(json!({ "sessions": sessions, "dir": store.dir().to_string_lossy() })).into_response()
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(status) = authorized(&state, &headers) {
        return status.into_response();
    }
    let store = SessionStore::new(&state.config.session);
    match store.load(&id) {
        Ok(loaded) => Json(json!({
            "meta": loaded.meta,
            "messages": loaded.messages,
            "todos": loaded.todos,
            "compactions": loaded.compactions,
            "truncated_tail": loaded.truncated_tail,
        }))
        .into_response(),
        Err(err) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": err.to_string(), "code": err.code() })),
        )
            .into_response(),
    }
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(status) = authorized(&state, &headers) {
        return status.into_response();
    }
    let store = SessionStore::new(&state.config.session);
    match store.delete(&id) {
        Ok(()) => Json(json!({ "deleted": id })).into_response(),
        Err(err) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": err.to_string(), "code": err.code() })),
        )
            .into_response(),
    }
}

fn authorized(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let Some(token) = &state.token else {
        return Ok(());
    };
    let given = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    if given == Some(token.as_str()) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut allowed = authorized(&state, &headers).is_ok();
    if !allowed {
        if let Some(token) = query.get("token") {
            allowed = state.token.as_deref() == Some(token.as_str());
        }
    }
    if !allowed {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let resume = query
        .get("resume")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    ws.on_upgrade(move |socket| handle_socket(socket, state, resume))
        .into_response()
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, resume: Option<String>) {
    let (tx, mut events) = mpsc::unbounded_channel();
    let config = state.config.clone();
    let cwd = state.cwd.clone();
    let agent = match resume {
        Some(id) => Agent::resume(config, &id, Some(cwd), tx).await,
        None => Agent::new(config, cwd, tx).await,
    };
    let agent = match agent {
        Ok(agent) => agent,
        Err(err) => {
            let mut socket = socket;
            let _ = socket
                .send(Message::Text(
                    json!({ "type": "error", "message": err.to_string() })
                        .to_string()
                        .into(),
                ))
                .await;
            let _ = socket.close().await;
            return;
        }
    };

    let (mut socket_tx, mut socket_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let _ = out_tx.send(json!({ "type": "snapshot", "snapshot": agent.snapshot() }).to_string());
    if let Ok(mut cwds) = state.cwds.lock() {
        cwds.insert(agent.session_id(), agent.cwd());
    }

    let mut send_task = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if socket_tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let event_out = out_tx.clone();
    let mut event_task = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let text = serde_json::to_string(&event).unwrap_or_else(|_| {
                json!({ "type": "error", "message": "event serialization failed" }).to_string()
            });
            if event_out.send(text).is_err() {
                break;
            }
        }
    });

    let cmd_agent = agent.clone();
    let cmd_state = state.clone();
    let cmd_out = out_tx.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(message) = socket_rx.next().await {
            let message = match message {
                Ok(message) => message,
                Err(_) => break,
            };
            let text = match message {
                Message::Text(text) => text.as_str().to_string(),
                Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            let command: HarnessCommand = match serde_json::from_str(&text) {
                Ok(command) => command,
                Err(err) => {
                    let _ = cmd_out.send(
                        json!({ "type": "error", "message": format!("invalid command: {err}") })
                            .to_string(),
                    );
                    continue;
                }
            };
            match command {
                HarnessCommand::UserMessage { text } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    if cmd_agent.busy() {
                        let _ = cmd_out.send(
                            json!({ "type": "error", "message": "a turn is already running" })
                                .to_string(),
                        );
                        continue;
                    }
                    let agent = cmd_agent.clone();
                    tokio::spawn(async move {
                        let _ = agent.send(text).await;
                    });
                }
                HarnessCommand::Cancel => cmd_agent.cancel(),
                HarnessCommand::SetAgentModel { model } => {
                    match cmd_agent.set_agent_model(&model) {
                        Ok(()) => {
                            let _ = cmd_out.send(
                                json!({ "type": "notice", "level": "info", "message": format!("model: {model}") })
                                    .to_string(),
                            );
                        }
                        Err(err) => {
                            let _ = cmd_out.send(
                                json!({ "type": "error", "message": err.to_string() }).to_string(),
                            );
                        }
                    }
                }
                HarnessCommand::SetCwd { path } => {
                    match cmd_agent.set_cwd(std::path::Path::new(&path)).await {
                        Ok(()) => {
                            if let Ok(mut cwds) = cmd_state.cwds.lock() {
                                cwds.insert(cmd_agent.session_id(), cmd_agent.cwd());
                            }
                            let _ = cmd_out.send(
                                json!({
                                    "type": "notice",
                                    "level": "info",
                                    "message": format!("workspace: {}", cmd_agent.cwd().display())
                                })
                                .to_string(),
                            );
                        }
                        Err(err) => {
                            let _ = cmd_out.send(
                                json!({ "type": "error", "message": err.to_string() }).to_string(),
                            );
                        }
                    }
                }
                HarnessCommand::DownloadConsent { id, allow } => {
                    cmd_agent.grant_download_by_id(&id, allow);
                }
                HarnessCommand::Shutdown => break,
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => {}
        _ = &mut event_task => {}
        _ = &mut recv_task => {}
    }
    send_task.abort();
    event_task.abort();
    recv_task.abort();
    agent.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, WorkspacePathResolver) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        fs::write(dir.path().join(".hidden"), "secret\n").unwrap();
        let resolver = WorkspacePathResolver::new(dir.path()).unwrap();
        (dir, resolver)
    }

    #[test]
    fn tree_lists_directories_first_and_skips_noise() {
        let (_dir, resolver) = workspace();
        let payload = tree_payload(&resolver, "").unwrap();
        let entries = payload["entries"].as_array().unwrap();
        let names: Vec<&str> = entries
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["src", "a.txt"]);
        assert_eq!(entries[0]["dir"], true);
        assert_eq!(entries[1]["dir"], false);

        let nested = tree_payload(&resolver, "src").unwrap();
        assert_eq!(nested["entries"][0]["path"], "src/main.rs");
    }

    #[test]
    fn file_payload_reads_text_and_flags_binary() {
        let (_dir, resolver) = workspace();
        let text = file_payload(&resolver, "a.txt").unwrap();
        assert_eq!(text["content"], "hello\n");
        assert_eq!(text["binary"], false);
        assert_eq!(text["language"], "txt");

        let bin = resolver.cwd().join("bin.dat");
        fs::write(&bin, [0u8, 1, 2, 3]).unwrap();
        let payload = file_payload(&resolver, "bin.dat").unwrap();
        assert_eq!(payload["binary"], true);
        assert_eq!(payload["content"], "");
    }

    #[test]
    fn file_payload_truncates_large_files_on_a_char_boundary() {
        let (dir, resolver) = workspace();
        let mut content = "a".repeat(FILE_LIMIT - 1);
        content.push('é');
        fs::write(dir.path().join("large.txt"), content).unwrap();
        let payload = file_payload(&resolver, "large.txt").unwrap();
        assert_eq!(payload["truncated"], true);
        let text = payload["content"].as_str().unwrap();
        assert_eq!(text.len(), FILE_LIMIT - 1);
        assert!(text.ends_with('a'));
    }

    #[test]
    fn file_and_tree_block_escapes_and_wrong_kinds() {
        let (_dir, resolver) = workspace();
        assert_eq!(
            file_payload(&resolver, "../outside.txt")
                .unwrap_err()
                .code(),
            "E_PATH_ESCAPE"
        );
        assert_eq!(
            tree_payload(&resolver, "..").unwrap_err().code(),
            "E_PATH_ESCAPE"
        );
        assert_eq!(
            file_payload(&resolver, "src").unwrap_err().code(),
            "E_NOT_FILE"
        );
        assert_eq!(
            tree_payload(&resolver, "a.txt").unwrap_err().code(),
            "E_NOT_DIR"
        );
    }
}
