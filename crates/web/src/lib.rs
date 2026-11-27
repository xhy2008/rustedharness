//! HTTP/WebSocket server and embedded web UI.
//!
//! Every browser tab owns one agent session. The WebSocket carries the same
//! `HarnessEvent` / `HarnessCommand` protocol as the TUI, so new frontends do
//! not need any server-specific event mapping. REST endpoints expose saved
//! sessions, non-secret configuration, the workspace tree (with git status) and
//! the directory browser; the interactive shell lives in [`terminal`], and the
//! settings surface in [`settings`].

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::info;

mod git;
mod settings;
mod terminal;

use rustedharness_core::agent::Agent;
use rustedharness_core::config::{Config, NewModel};
use rustedharness_core::error::HarnessError;
use rustedharness_core::paths::{WorkspacePathResolver, SKIP_DIRS};
use rustedharness_core::protocol::HarnessCommand;
use rustedharness_core::session::SessionStore;
use rustedharness_core::VERSION;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const TREE_LIMIT: usize = 500;
const FILE_LIMIT: usize = 256 * 1024;

struct AppState {
    config: Config,
    cwd: PathBuf,
    token: Option<String>,
    /// Where the server is reachable. Native dialogs are only offered when that
    /// is loopback: popping a file picker on a shared host is not a service a
    /// remote browser should be able to trigger.
    local: SocketAddr,
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
        local,
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
    if let Err(err) = webbrowser::open(url) {
        eprintln!("could not open a browser automatically: {err}");
    }
}

/// The same artwork the desktop bundle ships, so the browser tab and the native
/// window are recognisably the same application.
async fn favicon() -> impl IntoResponse {
    const ICO: &[u8] = include_bytes!("../../gui/icons/icon.ico");
    ([(header::CONTENT_TYPE, "image/x-icon")], ICO)
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/api/health", get(health))
        .route("/api/config", get(config_info))
        .route("/api/tree", get(workspace_tree))
        .route("/api/file", get(workspace_file))
        .route("/api/dirs", get(list_dirs))
        .route("/api/cwd/pick", axum::routing::post(pick_dir))
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
        .or_else(|| config.default_provider_id());
    let agent_model = agent
        .as_ref()
        .map(|model| model.reference())
        .or_else(|| (!config.model.trim().is_empty()).then(|| config.model.clone()));
    let compact_model = compact.as_ref().map(|model| model.reference()).or_else(|| {
        (!config.compact_model.trim().is_empty()).then(|| config.compact_model.clone())
    });
    let context_window = agent
        .as_ref()
        .map(|model| model.context_window)
        .filter(|value| *value > 0)
        .or((config.context_window > 0).then_some(config.context_window));
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
    let resolver = match WorkspacePathResolver::new(workspace_root(&state, &query.session)) {
        Ok(resolver) => resolver,
        Err(err) => return core_error(err),
    };
    let (entries, rel, dir) = match tree_entries(&resolver, &query.path) {
        Ok(triple) => triple,
        Err(err) => return core_error(err),
    };
    // `git status` walks the whole work tree, so it runs off the reactor.
    let probe = dir.clone();
    let statuses = tokio::task::spawn_blocking(move || git::statuses(&probe))
        .await
        .ok()
        .flatten();
    Json(tree_json(entries, &rel, &dir, statuses.as_ref())).into_response()
}

async fn workspace_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> Response {
    workspace_payload(&state, &headers, &query, file_payload)
}

/// Shared authorization, resolver setup and error mapping for the read-only
/// workspace endpoints.
fn workspace_payload(
    state: &AppState,
    headers: &HeaderMap,
    query: &PathQuery,
    build: fn(&WorkspacePathResolver, &str) -> std::result::Result<Value, HarnessError>,
) -> Response {
    if let Err(status) = authorized(state, headers) {
        return status.into_response();
    }
    let resolver = match WorkspacePathResolver::new(workspace_root(state, &query.session)) {
        Ok(resolver) => resolver,
        Err(err) => return core_error(err),
    };
    match build(&resolver, &query.path) {
        Ok(payload) => Json(payload).into_response(),
        Err(err) => core_error(err),
    }
}

#[derive(Debug, Deserialize)]
struct DirQuery {
    #[serde(default)]
    path: String,
}

/// Directories only, absolute paths: the fallback picker for browsers that
/// cannot ask the host for a real folder dialog (or when the host has none).
async fn list_dirs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<DirQuery>,
) -> Response {
    if let Err(status) = authorized(&state, &headers) {
        return status.into_response();
    }
    let requested = if query.path.trim().is_empty() {
        state.cwd.clone()
    } else {
        PathBuf::from(query.path.trim())
    };
    match dir_listing(&requested) {
        Ok(payload) => Json(payload).into_response(),
        Err(err) => core_error(err),
    }
}

fn dir_listing(dir: &std::path::Path) -> std::result::Result<Value, HarnessError> {
    let dir = dunce::canonicalize(dir).map_err(|err| {
        HarnessError::tool("E_INVALID_CWD", format!("cannot open directory: {err}"))
    })?;
    let mut entries = Vec::new();
    let read = fs::read_dir(&dir).map_err(|err| io_error("cannot read directory", err))?;
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        entries.push((name, entry.path()));
    }
    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(name, path)| json!({ "name": name, "path": path.to_string_lossy() }))
        .collect();
    Ok(json!({
        "path": dir.to_string_lossy(),
        "parent": dir.parent().map(|parent| parent.to_string_lossy().to_string()),
        "entries": entries,
        "home": std::env::var("HOME").ok(),
    }))
}

/// Ask the machine running the server for its folder dialog. Only loopback may
/// do this; anywhere else the browser gets `unsupported` and shows its own
/// picker instead.
async fn pick_dir(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(status) = authorized(&state, &headers) {
        return status.into_response();
    }
    if !state.local.ip().is_loopback() {
        return Json(json!({ "unsupported": true })).into_response();
    }
    let start = state.cwd.clone();
    let picked = tokio::task::spawn_blocking(move || native_directory_dialog(&start))
        .await
        .ok()
        .flatten();
    match picked {
        Some(Some(path)) => Json(json!({ "path": path.to_string_lossy() })).into_response(),
        Some(None) => Json(json!({ "cancelled": true })).into_response(),
        None => Json(json!({ "unsupported": true })).into_response(),
    }
}

/// `None` when the platform has no dialog we can drive, `Some(None)` when the
/// user cancelled.
fn native_directory_dialog(start: &std::path::Path) -> Option<Option<PathBuf>> {
    let start = start.to_string_lossy().to_string();
    let mut command = if cfg!(target_os = "macos") {
        let script = format!(
            "POSIX path of (choose folder with prompt \"选择工作目录\" default location POSIX file \"{start}\")"
        );
        let mut command = std::process::Command::new("osascript");
        command.arg("-e").arg(script);
        command
    } else if cfg!(windows) {
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
             $d.SelectedPath = '{start}'; \
             if ($d.ShowDialog() -eq 'OK') {{ Write-Output $d.SelectedPath }}"
        );
        let mut command = std::process::Command::new("powershell");
        command.args(["-NoProfile", "-STA", "-Command", &script]);
        command
    } else if which("zenity") {
        let mut command = std::process::Command::new("zenity");
        command.args([
            "--file-selection",
            "--directory",
            "--title=选择工作目录",
            &format!("--filename={start}/"),
        ]);
        command
    } else if which("kdialog") {
        let mut command = std::process::Command::new("kdialog");
        command.args(["--getexistingdirectory", &start, "--title", "选择工作目录"]);
        command
    } else {
        return None;
    };

    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Some(None);
                }
                let output = child.wait_with_output().ok()?;
                let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if text.is_empty() {
                    return Some(None);
                }
                return Some(Some(PathBuf::from(text)));
            }
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                return Some(None);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(120)),
            Err(_) => return Some(None),
        }
    }
}

fn which(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
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

struct TreeEntry {
    is_dir: bool,
    name: String,
    path: String,
    size: u64,
    abs: PathBuf,
}

fn tree_entries(
    resolver: &WorkspacePathResolver,
    rel: &str,
) -> std::result::Result<(Vec<TreeEntry>, String, PathBuf), HarnessError> {
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
    let read = fs::read_dir(&dir).map_err(|err| io_error("cannot read directory", err))?;
    for entry in read {
        if entries.len() >= TREE_LIMIT {
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
        let abs = entry.path();
        let size = if is_dir {
            0
        } else {
            entry.metadata().map(|meta| meta.len()).unwrap_or(0)
        };
        entries.push(TreeEntry {
            is_dir,
            name,
            path,
            size,
            abs,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok((entries, rel.to_string(), dir))
}

fn tree_json(
    entries: Vec<TreeEntry>,
    rel: &str,
    dir: &std::path::Path,
    statuses: Option<&BTreeMap<PathBuf, git::Status>>,
) -> Value {
    let truncated = entries.len() >= TREE_LIMIT;
    let root = statuses.and_then(|_| git::toplevel(dir));
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|entry| {
            let status = match (statuses, root.as_ref()) {
                (Some(statuses), Some(root)) => entry
                    .abs
                    .strip_prefix(root)
                    .ok()
                    .and_then(|path| git::find(statuses, path, entry.is_dir))
                    .map(|status| status.label()),
                _ => None,
            };
            json!({
                "name": entry.name,
                "path": entry.path,
                "dir": entry.is_dir,
                "size": entry.size,
                "git": status,
            })
        })
        .collect();
    json!({
        "path": if rel.is_empty() { "." } else { rel },
        "entries": entries,
        "truncated": truncated,
    })
}

/// Tree payload without git colours, for the tests that only exercise listing.
#[cfg(test)]
fn tree_payload(
    resolver: &WorkspacePathResolver,
    rel: &str,
) -> std::result::Result<Value, HarnessError> {
    let (entries, rel, dir) = tree_entries(resolver, rel)?;
    Ok(tree_json(entries, &rel, &dir, None))
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
    let sessions = store.list();
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

fn client_error(message: impl std::fmt::Display) -> String {
    json!({ "type": "error", "message": message.to_string() }).to_string()
}

fn client_notice(level: &str, message: impl std::fmt::Display) -> String {
    json!({ "type": "notice", "level": level, "message": message.to_string() }).to_string()
}

/// Out-of-band answers to the settings commands. They are not `HarnessEvent`s
/// because nothing outside this server produces them.
fn push(out: &mpsc::UnboundedSender<String>, value: Value) {
    let _ = out.send(value.to_string());
}

fn settings_event(agent: &Agent) -> Value {
    json!({ "type": "settings", "items": settings::describe_agent(agent) })
}

fn models_event(agent: &Agent) -> Value {
    json!({ "type": "models", "models": settings::models(agent) })
}

fn lsp_event(agent: &Agent) -> Value {
    let servers: Vec<Value> = agent
        .lsp_statuses()
        .into_iter()
        .map(|status| {
            json!({
                "language": status.language,
                "server": status.server,
                "version": status.version,
                "state": status.state,
                "detail": status.detail,
                "pid": status.pid,
                "restarts": status.restarts,
                "install_hint": status.install_hint,
                "runtime_deps": status.runtime_deps,
            })
        })
        .collect();
    json!({ "type": "lsp_servers", "servers": servers })
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
            let _ = socket.send(Message::Text(client_error(err).into())).await;
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
        // Owned by the connection: dropping it kills the shell, which is what
        // closing the tab should do.
        let mut terminal: Option<terminal::Terminal> = None;
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
                    let _ = cmd_out.send(client_error(format!("invalid command: {err}")));
                    continue;
                }
            };
            match command {
                HarnessCommand::UserMessage { text } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    if cmd_agent.busy() {
                        let _ = cmd_out.send(client_error("a turn is already running"));
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
                            let _ = cmd_out.send(client_notice("info", format!("model: {model}")));
                            push(&cmd_out, models_event(&cmd_agent));
                            push(&cmd_out, settings_event(&cmd_agent));
                        }
                        Err(err) => {
                            let _ = cmd_out.send(client_error(err));
                        }
                    }
                }
                HarnessCommand::SetCwd { path } => {
                    match cmd_agent.set_cwd(std::path::Path::new(&path)).await {
                        Ok(()) => {
                            if let Ok(mut cwds) = cmd_state.cwds.lock() {
                                cwds.insert(cmd_agent.session_id(), cmd_agent.cwd());
                            }
                            let _ = cmd_out.send(client_notice(
                                "info",
                                format!("workspace: {}", cmd_agent.cwd().display()),
                            ));
                            push(&cmd_out, settings_event(&cmd_agent));
                        }
                        Err(err) => {
                            let _ = cmd_out.send(client_error(err));
                        }
                    }
                }
                HarnessCommand::DownloadConsent { id, allow } => {
                    cmd_agent.grant_download_by_id(&id, allow);
                }
                HarnessCommand::DescribeSettings => push(&cmd_out, settings_event(&cmd_agent)),
                HarnessCommand::SetSetting { key, value } => {
                    match settings::apply(&cmd_agent, &key, &value) {
                        Ok(notice) => {
                            let _ = cmd_out.send(client_notice("info", notice));
                            push(&cmd_out, settings_event(&cmd_agent));
                            // The model rows and the web-search credential row are
                            // derived from the config, so a reload can move both.
                            push(&cmd_out, models_event(&cmd_agent));
                        }
                        Err(err) => {
                            let _ = cmd_out.send(client_error(err));
                        }
                    }
                }
                HarnessCommand::DescribeModels => push(&cmd_out, models_event(&cmd_agent)),
                HarnessCommand::SetCompactModel { model } => {
                    match cmd_agent.set_compact_model(&model) {
                        Ok(()) => {
                            let _ = cmd_out.send(client_notice(
                                "info",
                                if model.trim().is_empty() {
                                    "压缩复用主模型".to_string()
                                } else {
                                    format!("compact model: {model}")
                                },
                            ));
                            push(&cmd_out, models_event(&cmd_agent));
                            push(&cmd_out, settings_event(&cmd_agent));
                        }
                        Err(err) => {
                            let _ = cmd_out.send(client_error(err));
                        }
                    }
                }
                HarnessCommand::DiscoverModels { base_url, api_key } => {
                    let out = cmd_out.clone();
                    tokio::spawn(async move {
                        let value = match rustedharness_core::discovery::fetch_catalog(
                            base_url.trim(),
                            api_key.trim(),
                        )
                        .await
                        {
                            Ok(items) => json!({ "type": "model_catalog", "items": items }),
                            Err(err) => json!({
                                "type": "model_catalog",
                                "items": [],
                                "error": err.to_string(),
                            }),
                        };
                        push(&out, value);
                    });
                }
                HarnessCommand::AddModel {
                    base_url,
                    api_key,
                    model_id,
                    context_window,
                } => {
                    let new = NewModel {
                        base_url,
                        api_key,
                        model_id,
                        context_window,
                        max_tokens: None,
                    };
                    match cmd_agent.add_model(&new) {
                        Ok(reference) => {
                            let _ = cmd_out
                                .send(client_notice("info", format!("已添加模型 {reference}")));
                            push(&cmd_out, models_event(&cmd_agent));
                            push(&cmd_out, settings_event(&cmd_agent));
                        }
                        Err(err) => {
                            let _ = cmd_out.send(client_error(err));
                        }
                    }
                }
                HarnessCommand::DescribeLsp => push(&cmd_out, lsp_event(&cmd_agent)),
                HarnessCommand::Rollback { turn } => match cmd_agent.rollback_to_turn(turn) {
                    Ok(()) => {
                        // The transcript changed underneath the feed, so the
                        // browser gets a fresh snapshot instead of an edit list.
                        let _ = cmd_out.send(
                            json!({ "type": "snapshot", "snapshot": cmd_agent.snapshot() })
                                .to_string(),
                        );
                        let _ = cmd_out.send(client_notice("info", "已回滚到选中的输入"));
                    }
                    Err(err) => {
                        let _ = cmd_out.send(client_error(err));
                    }
                },
                HarnessCommand::InstallLsp { server, update } => {
                    let agent = cmd_agent.clone();
                    let out = cmd_out.clone();
                    tokio::spawn(async move {
                        match agent.install_lsp(&server, update).await {
                            Ok(path) => {
                                let _ = out.send(client_notice(
                                    "info",
                                    format!("{server} 已安装到 {}", path.display()),
                                ));
                            }
                            Err(err) => {
                                let _ = out.send(client_error(err));
                            }
                        }
                    });
                }
                HarnessCommand::OpenTerminal { cols, rows } => {
                    terminal = None;
                    let spec = rustedharness_core::shell::ShellSpec::detect(
                        &cmd_agent.config().shell.prefer,
                    );
                    match terminal::Terminal::open(
                        &cmd_agent.cwd(),
                        &spec,
                        cols,
                        rows,
                        cmd_out.clone(),
                    ) {
                        Ok(opened) => terminal = Some(opened),
                        Err(err) => {
                            let _ = cmd_out.send(client_error(err));
                        }
                    }
                }
                HarnessCommand::TerminalInput { data } => {
                    if let Some(terminal) = terminal.as_mut() {
                        terminal.write(&data);
                    }
                }
                HarnessCommand::TerminalResize { cols, rows } => {
                    if let Some(terminal) = terminal.as_mut() {
                        let size = terminal.resize(cols, rows);
                        push(&cmd_out, size);
                        push(&cmd_out, terminal.snapshot());
                    }
                }
                HarnessCommand::CloseTerminal => {
                    terminal = None;
                    let _ = cmd_out
                        .send(json!({ "type": "terminal_state", "state": "closed" }).to_string());
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
    fn directory_listing_only_walks_down() {
        let (dir, _resolver) = workspace();
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        let listing = dir_listing(dir.path()).unwrap();
        let names: Vec<&str> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        // `target/` is a legitimate workspace even though the file tree hides it.
        assert_eq!(names, vec!["src", "target"]);
        assert_eq!(
            listing["parent"],
            dir.path().parent().unwrap().to_string_lossy().as_ref()
        );
        // A file is not a directory and must be refused, not listed.
        assert!(dir_listing(&dir.path().join("a.txt")).is_err());
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
