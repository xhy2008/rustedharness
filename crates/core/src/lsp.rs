//! LSP first-class integration: stdio JSON-RPC client, per-language supervisor,
//! server registry, verified auto-download and transparent L0 fallback.
//!
//! The client speaks the subset of LSP 3.16 the harness needs (definition,
//! type definition, implementation, references, hover, document symbols,
//! workspace symbols, call hierarchy, rename and diagnostics). Servers are
//! spawned once per language and workspace, restarted with backoff at most
//! `lsp.max_restarts` times and then reported as `degraded`; the `lsp` tool
//! transparently answers from the name-based index in that case.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex};

use crate::config::{Config, LspConfig, LspDownload};
use crate::error::{HarnessError, Result};
use crate::intel::CodeIndex;
use crate::protocol::HarnessEvent;
use crate::VERSION;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveKind {
    Raw,
    Gz,
    Zip,
    TarGz,
}

impl ArchiveKind {
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "gz" | "gzip" => ArchiveKind::Gz,
            "zip" => ArchiveKind::Zip,
            "tar.gz" | "tgz" => ArchiveKind::TarGz,
            _ => ArchiveKind::Raw,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ArchiveKind::Raw => "raw",
            ArchiveKind::Gz => "gz",
            ArchiveKind::Zip => "zip",
            ArchiveKind::TarGz => "tar.gz",
        }
    }
}

/// One language server description. `download_url` may contain `{version}`
/// and `{target}` placeholders. Downloads are verified against the digest the
/// vendor publishes on the GitHub release; `lsp.downloads.<id>.sha256` is an
/// optional override, not a requirement.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub language: &'static str,
    pub id: &'static str,
    pub display: &'static str,
    pub extensions: &'static [&'static str],
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub version: &'static str,
    pub install_hint: &'static str,
    pub runtime_deps: &'static [&'static str],
    pub download_url: Option<&'static str>,
    pub archive: ArchiveKind,
    pub binary: &'static str,
    pub official_domains: &'static [&'static str],
}

macro_rules! spec {
    ($language:expr, $id:expr, $display:expr, $ext:expr, $cmd:expr, $args:expr,
     $ver:expr, $hint:expr, $deps:expr, $url:expr, $archive:expr, $bin:expr, $domains:expr) => {
        ServerSpec {
            language: $language,
            id: $id,
            display: $display,
            extensions: $ext,
            command: $cmd,
            args: $args,
            version: $ver,
            install_hint: $hint,
            runtime_deps: $deps,
            download_url: $url,
            archive: $archive,
            binary: $bin,
            official_domains: $domains,
        }
    };
}

pub fn registry() -> &'static [ServerSpec] {
    static REGISTRY: OnceLock<Vec<ServerSpec>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        vec![
            spec!(
                "rust",
                "rust-analyzer",
                "rust-analyzer",
                &["rs"],
                "rust-analyzer",
                &[],
                "2026-10-19",
                "rustup component add rust-analyzer",
                &[],
                Some("https://github.com/rust-lang/rust-analyzer/releases/download/{version}/rust-analyzer-{target}.gz"),
                ArchiveKind::Gz,
                "rust-analyzer",
                &["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"]
            ),
            spec!(
                "c",
                "clangd",
                "clangd",
                &["c", "h"],
                "clangd",
                &[],
                "23.1.0",
                "install clangd from your system package manager",
                &[],
                Some("https://github.com/clangd/clangd/releases/download/{version}/clangd-linux-{version}.zip"),
                ArchiveKind::Zip,
                "clangd",
                &["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"]
            ),
            spec!(
                "cpp",
                "clangd-cpp",
                "clangd",
                &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
                "clangd",
                &[],
                "23.1.0",
                "install clangd from your system package manager",
                &[],
                None,
                ArchiveKind::Raw,
                "clangd",
                &["github.com"]
            ),
            spec!(
                "go",
                "gopls",
                "gopls",
                &["go"],
                "gopls",
                &[],
                "v0.16.2",
                "go install golang.org/x/tools/gopls@latest",
                &["Go toolchain"],
                None,
                ArchiveKind::Raw,
                "gopls",
                &["go.dev"]
            ),
            spec!(
                "typescript",
                "typescript-language-server",
                "typescript-language-server",
                &["ts", "tsx", "mts", "cts"],
                "typescript-language-server",
                &["--stdio"],
                "4.3.3",
                "npm i -g typescript typescript-language-server",
                &["Node.js"],
                None,
                ArchiveKind::Raw,
                "typescript-language-server",
                &["npmjs.com"]
            ),
            spec!(
                "javascript",
                "typescript-language-server-js",
                "typescript-language-server",
                &["js", "jsx", "mjs", "cjs"],
                "typescript-language-server",
                &["--stdio"],
                "4.3.3",
                "npm i -g typescript typescript-language-server",
                &["Node.js"],
                None,
                ArchiveKind::Raw,
                "typescript-language-server",
                &["npmjs.com"]
            ),
            spec!(
                "python",
                "pyright",
                "pyright",
                &["py", "pyi"],
                "pyright-langserver",
                &["--stdio"],
                "1.1.383",
                "npm i -g pyright",
                &["Node.js"],
                None,
                ArchiveKind::Raw,
                "pyright-langserver",
                &["npmjs.com"]
            ),
            spec!(
                "java",
                "jdtls",
                "Eclipse JDT language server",
                &["java"],
                "jdtls",
                &[],
                "1.39.0",
                "download jdtls from download.eclipse.org/jdtls/milestones/",
                &["JDK 17+"],
                None,
                ArchiveKind::Raw,
                "jdtls",
                &["download.eclipse.org"]
            ),
            spec!(
                "kotlin",
                "kotlin-lsp",
                "Kotlin language server",
                &["kt", "kts"],
                "kotlin-lsp",
                &[],
                "2.0.0",
                "download kotlin-lsp from github.com/Kotlin/kotlin-lsp/releases",
                &["JDK 21+"],
                None,
                ArchiveKind::Raw,
                "kotlin-lsp",
                &["github.com"]
            ),
            spec!(
                "csharp",
                "csharp-ls",
                "csharp-ls",
                &["cs"],
                "csharp-ls",
                &[],
                "0.16.0",
                "dotnet tool install --global csharp-ls",
                &[".NET SDK"],
                None,
                ArchiveKind::Raw,
                "csharp-ls",
                &["nuget.org"]
            ),
            spec!(
                "ruby",
                "ruby-lsp",
                "Ruby LSP",
                &["rb", "rake", "gemspec"],
                "ruby-lsp",
                &[],
                "0.18.0",
                "gem install ruby-lsp",
                &["Ruby"],
                None,
                ArchiveKind::Raw,
                "ruby-lsp",
                &["github.com"]
            ),
            spec!(
                "php",
                "phpactor",
                "phpactor",
                &["php"],
                "phpactor",
                &["language-server"],
                "0.25.0",
                "download phpactor from github.com/phpactor/language-server",
                &["PHP"],
                None,
                ArchiveKind::Raw,
                "phpactor",
                &["github.com"]
            ),
            spec!(
                "swift",
                "sourcekit-lsp",
                "sourcekit-lsp",
                &["swift"],
                "sourcekit-lsp",
                &[],
                "bundled",
                "install the Swift toolchain from swift.org/download",
                &["Swift toolchain"],
                None,
                ArchiveKind::Raw,
                "sourcekit-lsp",
                &["swift.org"]
            ),
            spec!(
                "scala",
                "metals",
                "Metals",
                &["scala", "sc"],
                "metals",
                &[],
                "1.5.0",
                "install Metals through coursier",
                &["JDK"],
                None,
                ArchiveKind::Raw,
                "metals",
                &["scalameta.org"]
            ),
            spec!(
                "lua",
                "lua-language-server",
                "Lua Language Server",
                &["lua"],
                "lua-language-server",
                &[],
                "3.19.2",
                "download from github.com/LuaLS/lua-language-server/releases",
                &[],
                Some("https://github.com/LuaLS/lua-language-server/releases/download/{version}/lua-language-server-{version}-{target}.tar.gz"),
                ArchiveKind::TarGz,
                "lua-language-server",
                &["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"]
            ),
            spec!(
                "elixir",
                "elixir-ls",
                "ElixirLS",
                &["ex", "exs"],
                "language_server.sh",
                &[],
                "0.26.0",
                "download ElixirLS from github.com/elixir-lsp/elixir-ls/releases",
                &["Elixir", "Erlang"],
                None,
                ArchiveKind::Raw,
                "language_server.sh",
                &["github.com"]
            ),
        ]
    })
}

pub fn spec_for_language(language: &str) -> Option<&'static ServerSpec> {
    registry().iter().find(|s| s.language == language)
}

pub fn spec_for_id(id: &str) -> Option<&'static ServerSpec> {
    registry().iter().find(|s| s.id == id)
}

pub fn language_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    registry()
        .iter()
        .find(|s| s.extensions.iter().any(|e| *e == ext))
        .map(|s| s.language)
}

fn target_triple() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "x86_64-pc-windows-msvc"
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64"),
    )))]
    {
        "unknown"
    }
}

/// Asset-name platform token. Most projects use Rust target triples, but
/// LuaLS uses its own `linux-x64`-style names.
fn asset_target(spec: &ServerSpec) -> String {
    if spec.id == "lua-language-server" {
        lua_asset_target().to_string()
    } else {
        target_triple().to_string()
    }
}

fn lua_asset_target() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-arm64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "win32-x64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86")) {
        "win32-ia32"
    } else {
        target_triple()
    }
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) && !name.ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn find_in_path(command: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(command);
    if candidate.components().count() > 1 {
        return candidate.is_file().then_some(candidate);
    }
    let path = std::env::var_os("PATH")?;
    let names: Vec<OsString> = if cfg!(windows) {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut names = vec![OsString::from(command)];
        for ext in pathext.split(';').filter(|e| !e.is_empty()) {
            names.push(OsString::from(format!("{command}{ext}")));
        }
        names
    } else {
        vec![OsString::from(command)]
    };
    for dir in std::env::split_paths(&path) {
        for name in &names {
            let full = dir.join(name);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

fn cache_root() -> PathBuf {
    Config::data_dir().join("lsp")
}

/// Local cache for vendor-published digests, so repeat installs and
/// `lsp checksum` do not depend on the GitHub API quota.
fn digest_cache_path(server: &str, version: &str) -> PathBuf {
    cache_root()
        .join("checksums")
        .join(server)
        .join(format!("{version}.sha256"))
}

fn read_digest_cache(path: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let hash = lines.next()?.trim();
    if !valid_sha256(hash) {
        return None;
    }
    let source = lines.next().unwrap_or("cached").trim().to_string();
    Some((hash.to_ascii_lowercase(), source))
}

fn write_digest_cache(path: &Path, hash: &str, source: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{hash}\n{source}\n"));
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Enforce the download transport rules shared by installation and checksum
/// discovery: HTTPS plus an official-host suffix match.
fn validate_download_url(url: &str, domains: &[String]) -> Result<url::Url> {
    let parsed = url::Url::parse(url)
        .map_err(|e| HarnessError::tool("E_LSP_DOWNLOAD", format!("bad URL: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(HarnessError::tool(
            "E_LSP_DOWNLOAD",
            "only HTTPS downloads are allowed",
        ));
    }
    let host_ok = parsed
        .host_str()
        .map(|host| {
            domains
                .iter()
                .any(|domain| host == domain || host.ends_with(&format!(".{domain}")))
        })
        .unwrap_or(false);
    if !host_ok {
        return Err(HarnessError::tool(
            "E_LSP_DOWNLOAD",
            format!(
                "host `{}` is not in the official domain whitelist ({}); add it to lsp.downloads.* domains",
                parsed.host_str().unwrap_or("?"),
                domains.join(", ")
            ),
        ));
    }
    Ok(parsed)
}

fn http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("rustedharness/{}", VERSION))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 4 {
                attempt.error("too many redirects")
            } else if attempt.url().scheme() != "https" {
                attempt.error("redirect downgraded to non-HTTPS")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .ok()
}

async fn fetch_text(url: &str, limit: usize) -> Option<String> {
    let client = http_client()?;
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let text = response.text().await.ok()?;
    (text.len() <= limit).then_some(text)
}

/// Parse a published checksums file (`<hash>  <name>`, `*<name>`, or the BSD
/// `SHA256 (name) = <hash>` form) and return the entry for `asset_name`.
fn parse_checksum_file(text: &str, asset_name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((label, value)) = line.split_once('=') {
            if label.contains(&format!("({asset_name})")) {
                let hash = value.trim();
                if valid_sha256(hash) {
                    return Some(hash.to_ascii_lowercase());
                }
            }
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next().unwrap_or("");
        if !valid_sha256(hash) {
            continue;
        }
        let matches_asset = parts.any(|name| {
            let name = name.trim_start_matches('*');
            name == asset_name || name.rsplit('/').next() == Some(asset_name)
        });
        if matches_asset {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Outcome of asking GitHub's release API for a published digest.
enum DigestLookup {
    Found {
        sha256: String,
        source: String,
    },
    /// The release answered, but publishes no digest we can use.
    NoDigest,
    /// The API could not be reached or refused us (rate limit, auth, network).
    /// This is not evidence that the vendor omitted a digest.
    Unavailable(String),
}

/// Return the `digest` metadata GitHub records for the named release asset.
fn asset_digest(assets: &[Value], asset_name: &str) -> Option<String> {
    assets.iter().find_map(|entry| {
        if entry.get("name").and_then(Value::as_str) != Some(asset_name) {
            return None;
        }
        let digest = entry.get("digest").and_then(Value::as_str)?;
        let hash = digest.strip_prefix("sha256:")?;
        valid_sha256(hash).then(|| hash.to_ascii_lowercase())
    })
}

/// Release assets that are likely to hold checksums for the other artifacts.
fn checksum_assets(assets: &[Value]) -> Vec<(String, String)> {
    assets
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name").and_then(Value::as_str)?;
            let lower = name.to_ascii_lowercase();
            if !(lower.contains("sha256") || lower.contains("checksum")) || lower.ends_with(".sig")
            {
                return None;
            }
            let url = entry.get("browser_download_url").and_then(Value::as_str)?;
            Some((name.to_string(), url.to_string()))
        })
        .collect()
}

/// Well-known checksums files published next to the release artifacts. Tried
/// when the GitHub API is unavailable (for example when its rate limit is hit).
async fn release_checksum_file_digest(
    artifact_url: &str,
    asset_name: &str,
) -> Option<(String, String)> {
    let base = artifact_url.rsplit_once('/')?.0;
    for name in [
        "SHA256SUMS",
        "SHA256SUMS.txt",
        "sha256sums.txt",
        "checksums.txt",
        "sha256.txt",
    ] {
        let Some(text) = fetch_text(&format!("{base}/{name}"), 1024 * 1024).await else {
            continue;
        };
        if let Some(hash) = parse_checksum_file(&text, asset_name) {
            return Some((hash, format!("release checksums ({name})")));
        }
    }
    None
}

async fn sidecar_digest(artifact_url: &str) -> Option<String> {
    let text = fetch_text(&format!("{artifact_url}.sha256"), 8 * 1024).await?;
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|token| token.len() == 64)
        .map(|token| token.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Public query types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LspOp {
    Definition,
    TypeDefinition,
    Implementation,
    References,
    Hover,
    DocumentSymbols,
    WorkspaceSymbols,
    CallHierarchy,
    Rename,
    Diagnostics,
}

impl LspOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            LspOp::Definition => "definition",
            LspOp::TypeDefinition => "type_definition",
            LspOp::Implementation => "implementation",
            LspOp::References => "references",
            LspOp::Hover => "hover",
            LspOp::DocumentSymbols => "document_symbols",
            LspOp::WorkspaceSymbols => "workspace_symbols",
            LspOp::CallHierarchy => "call_hierarchy",
            LspOp::Rename => "rename",
            LspOp::Diagnostics => "diagnostics",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LspQuery {
    pub op: LspOp,
    /// Absolute, workspace-checked path.
    pub path: PathBuf,
    /// 1-based line (0 when omitted).
    pub line: u32,
    /// 1-based column; columns count Unicode scalar values.
    pub column: u32,
    pub symbol: Option<String>,
    pub apply: bool,
    pub timeout_ms: u64,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppliedFile {
    pub path: String,
    pub diff: String,
}

#[derive(Debug, Clone)]
pub struct LspAnswer {
    /// `lsp-precise` or `name-based`.
    pub match_kind: &'static str,
    pub text: String,
    pub warmup_hint: Option<String>,
    pub raw: Value,
    pub applied: Vec<AppliedFile>,
}

impl LspAnswer {
    pub fn name_based(text: impl Into<String>, raw: Value) -> Self {
        Self {
            match_kind: "name-based",
            text: text.into(),
            warmup_hint: None,
            raw,
            applied: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub language: String,
    pub server: String,
    pub state: String,
    pub detail: Option<String>,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub install_hint: String,
    pub runtime_deps: Vec<String>,
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

struct HubState {
    state: String,
    detail: Option<String>,
    restarts: u32,
}

struct DownloadTarget {
    version: String,
    url: String,
    archive: ArchiveKind,
    binary: String,
    domains: Vec<String>,
    display: String,
}

struct DownloadManifest {
    version: String,
    url: String,
    sha: String,
    archive: ArchiveKind,
    binary: String,
    domains: Vec<String>,
    display: String,
}

/// A checksum obtained for a registry entry without installing it. The
/// `source` field makes the trust path explicit to the user.
#[derive(Debug, Clone, Serialize)]
pub struct ChecksumInfo {
    pub server_id: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub source: String,
}

pub struct LspHub {
    cwd: PathBuf,
    config: LspConfig,
    events: Option<UnboundedSender<HarnessEvent>>,
    clients: AsyncMutex<HashMap<String, Arc<LspClient>>>,
    states: Mutex<HashMap<String, HubState>>,
    consent: Mutex<HashMap<String, bool>>,
    asked: Mutex<HashSet<String>>,
    pending_consent: Mutex<HashMap<String, String>>,
    index: Arc<CodeIndex>,
}

impl LspHub {
    pub fn new(
        cwd: PathBuf,
        config: &LspConfig,
        events: Option<UnboundedSender<HarnessEvent>>,
    ) -> Arc<Self> {
        let index = CodeIndex::new(&cwd);
        let hub = Arc::new(Self {
            cwd,
            config: config.clone(),
            events,
            clients: AsyncMutex::new(HashMap::new()),
            states: Mutex::new(HashMap::new()),
            consent: Mutex::new(HashMap::new()),
            asked: Mutex::new(HashSet::new()),
            pending_consent: Mutex::new(HashMap::new()),
            index,
        });
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&hub);
            let idle = config.idle_timeout_secs.max(30);
            handle.spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(30));
                loop {
                    ticker.tick().await;
                    let Some(hub) = weak.upgrade() else { break };
                    hub.reap_idle(Duration::from_secs(idle)).await;
                }
            });
        }
        hub
    }

    pub fn config(&self) -> &LspConfig {
        &self.config
    }

    pub fn index(&self) -> &Arc<CodeIndex> {
        &self.index
    }

    pub fn emit(&self, event: HarnessEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    fn set_state(&self, language: &str, state: &str, detail: Option<String>) {
        {
            let mut states = self.states.lock().unwrap();
            let entry = states
                .entry(language.to_string())
                .or_insert_with(|| HubState {
                    state: "absent".to_string(),
                    detail: None,
                    restarts: 0,
                });
            entry.state = state.to_string();
            entry.detail = detail.clone();
        }
        self.emit(HarnessEvent::LspStatus {
            language: language.to_string(),
            state: state.to_string(),
            detail,
        });
    }

    fn bump_restart(&self, language: &str) -> u32 {
        let mut states = self.states.lock().unwrap();
        let entry = states
            .entry(language.to_string())
            .or_insert_with(|| HubState {
                state: "absent".to_string(),
                detail: None,
                restarts: 0,
            });
        entry.restarts += 1;
        entry.restarts
    }

    pub fn statuses(&self) -> Vec<ServerStatus> {
        let states = self.states.lock().unwrap();
        registry()
            .iter()
            .map(|spec| {
                let entry = states.get(spec.language);
                ServerStatus {
                    language: spec.language.to_string(),
                    server: spec.id.to_string(),
                    state: entry.map(|e| e.state.clone()).unwrap_or_else(|| {
                        if self.resolve_command(spec).is_some() {
                            "available".to_string()
                        } else {
                            "absent".to_string()
                        }
                    }),
                    detail: entry.and_then(|e| e.detail.clone()),
                    pid: None,
                    restarts: entry.map(|e| e.restarts).unwrap_or(0),
                    install_hint: spec.install_hint.to_string(),
                    runtime_deps: spec.runtime_deps.iter().map(|d| d.to_string()).collect(),
                }
            })
            .collect()
    }

    /// Frontend acknowledgement for a `download_consent` card.
    pub fn grant_download(&self, server_id: &str, allow: bool) {
        self.consent
            .lock()
            .unwrap()
            .insert(server_id.to_string(), allow);
        if allow {
            self.asked.lock().unwrap().remove(server_id);
        }
        self.emit(HarnessEvent::DownloadStatus {
            server_id: server_id.to_string(),
            state: if allow { "consented" } else { "declined" }.to_string(),
            detail: None,
        });
    }

    /// Consent keyed by the opaque id shown in the `download_consent` card.
    pub fn grant_download_by_id(&self, consent_id: &str, allow: bool) {
        let server_id = self
            .pending_consent
            .lock()
            .unwrap()
            .iter()
            .find(|(_, id)| id.as_str() == consent_id)
            .map(|(server, _)| server.clone());
        if let Some(server_id) = server_id {
            self.grant_download(&server_id, allow);
        }
    }

    /// Install a language server into the cache without starting it. Used by
    /// `rustedharness lsp install`; the caller is responsible for consent and
    /// the published SHA-256 is still enforced.
    pub async fn install(&self, server_id: &str) -> Result<PathBuf> {
        let spec = spec_for_id(server_id).ok_or_else(|| {
            HarnessError::tool(
                "E_UNKNOWN_LSP_SERVER",
                format!("unknown language server `{server_id}`"),
            )
        })?;
        if let Some(command) = self.resolve_command(spec) {
            return Ok(command);
        }
        self.download_server(spec).await
    }

    /// Discover the vendor-published SHA-256 for a registry server without
    /// installing it. Installation performs the same discovery automatically;
    /// this entry point only lets a user inspect the digest or pin it.
    pub async fn checksum(&self, server_id: &str) -> Result<ChecksumInfo> {
        let spec = spec_for_id(server_id).ok_or_else(|| {
            HarnessError::tool(
                "E_UNKNOWN_LSP_SERVER",
                format!("unknown language server `{server_id}`"),
            )
        })?;
        let Some(target) = self.download_target(spec) else {
            return Err(HarnessError::tool(
                "E_LSP_NO_DOWNLOAD",
                format!(
                    "`{}` has no built-in download URL; install it manually: {}",
                    spec.id, spec.install_hint
                ),
            ));
        };
        validate_download_url(&target.url, &target.domains)?;
        let (sha256, source) = self
            .published_digest(spec, &target.version, &target.url)
            .await?;
        Ok(ChecksumInfo {
            server_id: spec.id.to_string(),
            version: target.version,
            url: target.url,
            sha256,
            source,
        })
    }

    /// Resolve the digest for an artifact. Pins and previously discovered
    /// digests come first; otherwise `discover_digest` searches the vendor's
    /// published checksums.
    async fn published_digest(
        &self,
        spec: &ServerSpec,
        version: &str,
        artifact_url: &str,
    ) -> Result<(String, String)> {
        if let Some(pinned) = self
            .config
            .downloads
            .get(spec.id)
            .and_then(|entry| entry.sha256.clone())
        {
            return Ok((pinned, "pinned in lsp.downloads".to_string()));
        }
        let cache_path = digest_cache_path(spec.id, version);
        if let Some(cached) = read_digest_cache(&cache_path) {
            return Ok(cached);
        }
        let found = self.discover_digest(spec, artifact_url).await?;
        write_digest_cache(&cache_path, &found.0, &found.1);
        Ok(found)
    }

    async fn discover_digest(
        &self,
        spec: &ServerSpec,
        artifact_url: &str,
    ) -> Result<(String, String)> {
        // Token-free sources first: plain HTTPS GETs against the release
        // artifacts, then the GitHub API (optional token, raises its quota).
        if let Some(sha256) = sidecar_digest(artifact_url).await {
            return Ok((sha256, "published .sha256 sidecar".to_string()));
        }
        let asset_name = artifact_url.rsplit('/').next().unwrap_or(artifact_url);
        if let Some(found) = release_checksum_file_digest(artifact_url, asset_name).await {
            return Ok(found);
        }
        match self.github_release_digest(artifact_url).await? {
            DigestLookup::Found { sha256, source } => return Ok((sha256, source)),
            DigestLookup::NoDigest => {}
            DigestLookup::Unavailable(reason) => {
                return Err(HarnessError::tool(
                    "E_LSP_CHECKSUM_LOOKUP_FAILED",
                    format!(
                        concat!(
                            "could not read the GitHub release digest for `{}` ({}):\n  {}\n",
                            "No download was attempted. Retry later, set GITHUB_TOKEN to raise ",
                            "the API quota, or pin lsp.downloads.{}.sha256 to override."
                        ),
                        spec.id, reason, artifact_url, spec.id
                    ),
                ))
            }
        }
        Err(HarnessError::tool(
            "E_LSP_CHECKSUM_UNAVAILABLE",
            format!(
                concat!(
                    "`{}` publishes no SHA-256 for:\n  {}\n",
                    "Refusing to install an unverified binary. ",
                    "Install it manually: {}"
                ),
                spec.id, artifact_url, spec.install_hint
            ),
        ))
    }

    async fn github_release_digest(&self, artifact_url: &str) -> Result<DigestLookup> {
        let parsed = url::Url::parse(artifact_url)
            .map_err(|e| HarnessError::tool("E_LSP_DOWNLOAD", format!("bad URL: {e}")))?;
        if parsed.host_str() != Some("github.com") {
            return Ok(DigestLookup::NoDigest);
        }
        let segments: Vec<&str> = parsed
            .path_segments()
            .map(|segments| segments.collect())
            .unwrap_or_default();
        // /{owner}/{repo}/releases/download/{tag}/{asset...}
        if segments.len() < 6
            || segments.get(2) != Some(&"releases")
            || segments.get(3) != Some(&"download")
        {
            return Ok(DigestLookup::NoDigest);
        }
        let owner = segments[0];
        let repo = segments[1];
        let tag = segments[4];
        let asset = urlencoding::decode(&segments[5..].join("/"))
            .map(|value| value.into_owned())
            .unwrap_or_else(|_| segments[5..].join("/"));
        let api = format!(
            "https://api.github.com/repos/{owner}/{repo}/releases/tags/{}",
            urlencoding::encode(tag)
        );
        let client = reqwest::Client::builder()
            .user_agent(format!("rustedharness/{}", VERSION))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| HarnessError::tool("E_LSP_DOWNLOAD", e.to_string()))?;
        let mut request = client
            .get(&api)
            .header("Accept", "application/vnd.github+json");
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            if !token.is_empty() {
                request = request.bearer_auth(token);
            }
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => return Ok(DigestLookup::Unavailable(err.to_string())),
        };
        let status = response.status();
        if !status.is_success() {
            return Ok(DigestLookup::Unavailable(format!("HTTP {status}")));
        }
        let value: Value = match response.json().await {
            Ok(value) => value,
            Err(err) => return Ok(DigestLookup::Unavailable(err.to_string())),
        };
        let Some(assets) = value.get("assets").and_then(Value::as_array) else {
            return Ok(DigestLookup::Unavailable(
                "release metadata has no assets".to_string(),
            ));
        };
        let asset_name = asset.rsplit('/').next().unwrap_or(asset.as_str());
        if let Some(hash) = asset_digest(assets, asset_name) {
            return Ok(DigestLookup::Found {
                sha256: hash,
                source: "GitHub release asset digest".to_string(),
            });
        }
        // Some releases publish a checksums file instead of per-asset digests.
        for (name, url) in checksum_assets(assets) {
            let Some(text) = fetch_text(&url, 1024 * 1024).await else {
                continue;
            };
            if let Some(hash) = parse_checksum_file(&text, asset_name) {
                return Ok(DigestLookup::Found {
                    sha256: hash,
                    source: format!("GitHub release checksums ({name})"),
                });
            }
        }
        Ok(DigestLookup::NoDigest)
    }

    /// Re-download a pinned server, replacing the cached binary.
    pub async fn update(&self, server_id: &str) -> Result<PathBuf> {
        let spec = spec_for_id(server_id).ok_or_else(|| {
            HarnessError::tool(
                "E_UNKNOWN_LSP_SERVER",
                format!("unknown language server `{server_id}`"),
            )
        })?;
        let version = self
            .config
            .downloads
            .get(spec.id)
            .and_then(|d| d.version.clone())
            .unwrap_or_else(|| spec.version.to_string());
        let cached = self.cached_binary(spec, &version);
        if cached.exists() {
            std::fs::remove_file(&cached)?;
        }
        let _ = std::fs::remove_file(digest_cache_path(spec.id, &version));
        self.download_server(spec).await
    }

    fn cached_binary(&self, spec: &ServerSpec, version: &str) -> PathBuf {
        cache_root()
            .join(spec.id)
            .join(version)
            .join(exe_name(spec.binary))
    }

    /// Resolution chain: explicit config override -> PATH -> local cache.
    fn resolve_command(&self, spec: &ServerSpec) -> Option<PathBuf> {
        if let Some(over) = self.config.servers.get(spec.language) {
            let p = PathBuf::from(over);
            if p.is_file() {
                return Some(p);
            }
        }
        if let Some(over) = self.config.servers.get(spec.id) {
            let p = PathBuf::from(over);
            if p.is_file() {
                return Some(p);
            }
        }
        if let Some(p) = find_in_path(spec.command) {
            return Some(p);
        }
        let version = self
            .config
            .downloads
            .get(spec.id)
            .and_then(|d| d.version.clone())
            .unwrap_or_else(|| spec.version.to_string());
        let cached = self.cached_binary(spec, &version);
        cached.is_file().then_some(cached)
    }

    fn download_target(&self, spec: &ServerSpec) -> Option<DownloadTarget> {
        let over: Option<&LspDownload> = self.config.downloads.get(spec.id);
        let version = over
            .and_then(|o| o.version.clone())
            .unwrap_or_else(|| spec.version.to_string());
        let url = over.and_then(|o| o.url.clone()).or_else(|| {
            spec.download_url.map(|u| {
                u.replace("{version}", &version)
                    .replace("{target}", &asset_target(spec))
            })
        })?;
        let archive = over
            .and_then(|o| o.archive.as_deref())
            .map(ArchiveKind::parse)
            .unwrap_or(spec.archive);
        let binary = over
            .and_then(|o| o.binary.clone())
            .unwrap_or_else(|| exe_name(spec.binary));
        let mut domains: Vec<String> = spec
            .official_domains
            .iter()
            .map(|d| d.to_string())
            .collect();
        if let Some(over) = over {
            domains.extend(over.domains.iter().cloned());
        }
        Some(DownloadTarget {
            version,
            url,
            archive,
            binary,
            domains,
            display: spec.display.to_string(),
        })
    }

    /// Build the download manifest. The digest normally comes from the
    /// vendor's GitHub release metadata; a pinned `lsp.downloads.<id>.sha256`
    /// only overrides it.
    async fn download_manifest(&self, spec: &ServerSpec) -> Result<DownloadManifest> {
        let target = self.download_target(spec).ok_or_else(|| {
            HarnessError::tool(
                "E_LSP_NO_DOWNLOAD",
                format!(
                    "`{}` has no built-in download URL; install it manually: {}",
                    spec.id, spec.install_hint
                ),
            )
        })?;
        validate_download_url(&target.url, &target.domains)?;
        let sha = self
            .published_digest(spec, &target.version, &target.url)
            .await?
            .0;
        Ok(DownloadManifest {
            version: target.version,
            url: target.url,
            sha,
            archive: target.archive,
            binary: target.binary,
            domains: target.domains,
            display: target.display,
        })
    }

    async fn ensure_client(&self, language: &str) -> Option<Arc<LspClient>> {
        if !self.config.enabled {
            return None;
        }
        let spec = spec_for_language(language)?;
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(language) {
            if client.alive() {
                return Some(client.clone());
            }
            clients.remove(language);
        }
        let Some(command) = self.resolve_command(spec) else {
            // Attempt verified auto-download.
            if self.config.auto_download {
                match self.download_server(spec).await {
                    Ok(path) => match self.spawn_and_init(spec, &path).await {
                        Ok(client) => {
                            clients.insert(language.to_string(), client.clone());
                            self.set_state(language, "running", None);
                            return Some(client);
                        }
                        Err(err) => {
                            self.set_state(language, "degraded", Some(err.to_string()));
                            return None;
                        }
                    },
                    Err(err) => {
                        self.set_state(language, "absent", Some(err.to_string()));
                    }
                }
            } else {
                self.set_state(
                    language,
                    "absent",
                    Some(format!("server not found; {}", spec.install_hint)),
                );
            }
            return None;
        };

        let mut attempt = 0u32;
        loop {
            if attempt > self.config.max_restarts {
                self.set_state(
                    language,
                    "degraded",
                    Some(format!(
                        "server crashed {} times; using name-based fallback",
                        self.config.max_restarts
                    )),
                );
                return None;
            }
            match self.spawn_and_init(spec, &command).await {
                Ok(client) => {
                    clients.insert(language.to_string(), client.clone());
                    self.set_state(language, "running", None);
                    return Some(client);
                }
                Err(err) => {
                    attempt += 1;
                    let restarts = self.bump_restart(language);
                    self.set_state(language, "degraded", Some(err.to_string()));
                    if restarts > self.config.max_restarts {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(250 * u64::from(restarts))).await;
                }
            }
        }
    }

    async fn spawn_and_init(&self, spec: &ServerSpec, command: &Path) -> Result<Arc<LspClient>> {
        let client = LspClient::spawn(spec, command, &self.cwd).await?;
        let root = uri_from_path(&self.cwd);
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "rustedharness", "version": VERSION },
            "rootUri": root,
            "workspaceFolders": [{ "uri": root, "name": self.cwd.file_name().and_then(|n| n.to_str()).unwrap_or("workspace") }],
            "capabilities": client_capabilities(),
            "initializationOptions": {},
        });
        let init_timeout = Duration::from_millis(self.config.request_timeout_ms.max(1_000));
        let result = client
            .request_with_timeout("initialize", params, init_timeout)
            .await
            .map_err(|e| {
                HarnessError::tool("E_LSP_INIT", format!("{} initialize failed: {e}", spec.id))
            })?;
        client.set_capabilities(result.get("capabilities").cloned().unwrap_or(Value::Null));
        client
            .notify("initialized", json!({}))
            .await
            .map_err(|e| HarnessError::tool("E_LSP_INIT", e.to_string()))?;
        Ok(client)
    }

    /// Fetch a verified archive and extract it into the cache. The SHA-256 is
    /// discovered from the vendor's GitHub release metadata (optionally
    /// overridden by a pin); HTTPS, an allow-listed host and (in interactive
    /// mode) explicit consent are still required.
    async fn download_server(&self, spec: &ServerSpec) -> Result<PathBuf> {
        let DownloadManifest {
            version,
            url,
            sha,
            archive,
            binary,
            domains,
            display,
        } = self.download_manifest(spec).await?;
        if !valid_sha256(&sha) {
            return Err(HarnessError::tool(
                "E_LSP_CHECKSUM_INVALID",
                "sha256 must be 64 hex characters",
            ));
        }

        let consented = *self.consent.lock().unwrap().get(spec.id).unwrap_or(&false);
        if !consented {
            if self.config.allow_downloads_noninteractive {
                // Explicit opt-in for headless environments.
            } else {
                let first = self.asked.lock().unwrap().insert(spec.id.to_string());
                let id = {
                    let mut pending = self.pending_consent.lock().unwrap();
                    pending
                        .entry(spec.id.to_string())
                        .or_insert_with(|| uuid::Uuid::new_v4().to_string())
                        .clone()
                };
                if first {
                    self.emit(HarnessEvent::DownloadConsent {
                        id,
                        server_id: spec.id.to_string(),
                        display_name: display.clone(),
                        version: version.clone(),
                        url: url.clone(),
                        sha256: sha.clone(),
                        runtime_hint: (!spec.runtime_deps.is_empty())
                            .then(|| spec.runtime_deps.join(", ")),
                    });
                }
                self.set_state(
                    spec.language,
                    "absent",
                    Some("waiting for download consent".to_string()),
                );
                return Err(HarnessError::tool(
                    "E_LSP_CONSENT_REQUIRED",
                    format!(
                        "download of `{}` requires consent; use the consent card or `rustedharness lsp install {}`",
                        spec.id, spec.id
                    ),
                ));
            }
        }

        self.set_state(spec.language, "downloading", Some(url.clone()));
        self.emit(HarnessEvent::DownloadStatus {
            server_id: spec.id.to_string(),
            state: "downloading".to_string(),
            detail: Some(format!("{display} {version}")),
        });

        let client = reqwest::Client::builder()
            .user_agent(format!("rustedharness/{}", VERSION))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() > 8 {
                    attempt.error("too many redirects")
                } else if attempt.url().scheme() != "https" {
                    attempt.error("redirect downgraded to non-HTTPS")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| HarnessError::tool("E_LSP_DOWNLOAD", e.to_string()))?;
        let response =
            client.get(&url).send().await.map_err(|e| {
                HarnessError::tool("E_LSP_DOWNLOAD", format!("request failed: {e}"))
            })?;
        if !response.status().is_success() {
            return Err(HarnessError::tool(
                "E_LSP_DOWNLOAD",
                format!("HTTP {} for {url}", response.status()),
            ));
        }
        if let Some(final_url) = response.url().host_str() {
            let redirect_ok = domains
                .iter()
                .any(|d| final_url == d || final_url.ends_with(&format!(".{d}")))
                || final_url.ends_with("githubusercontent.com");
            if !redirect_ok {
                return Err(HarnessError::tool(
                    "E_LSP_DOWNLOAD",
                    format!("redirected to non-whitelisted host `{final_url}`"),
                ));
            }
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| HarnessError::tool("E_LSP_DOWNLOAD", format!("read failed: {e}")))?;
        if bytes.len() > 256 * 1024 * 1024 {
            return Err(HarnessError::tool(
                "E_LSP_DOWNLOAD",
                "download exceeds 256 MiB",
            ));
        }
        let digest = hex::encode(Sha256::digest(&bytes));
        if !digest.eq_ignore_ascii_case(&sha) {
            self.emit(HarnessEvent::DownloadStatus {
                server_id: spec.id.to_string(),
                state: "checksum_failed".to_string(),
                detail: Some(format!("expected {sha}, got {digest}")),
            });
            return Err(HarnessError::tool(
                "E_LSP_CHECKSUM_MISMATCH",
                format!(
                    "SHA-256 mismatch for {}: expected {sha}, got {digest}",
                    spec.id
                ),
            ));
        }

        let target = cache_root().join(spec.id).join(&version);
        std::fs::create_dir_all(&target)?;
        let binary_path = extract_archive(&bytes, archive, &target, &binary)?;
        make_executable(&binary_path)?;
        self.emit(HarnessEvent::DownloadStatus {
            server_id: spec.id.to_string(),
            state: "installed".to_string(),
            detail: Some(binary_path.display().to_string()),
        });
        self.set_state(
            spec.language,
            "absent",
            Some("installed; starting".to_string()),
        );
        Ok(binary_path)
    }

    async fn client_for_path(&self, path: &Path) -> Option<(Arc<LspClient>, bool)> {
        let language = language_for_path(path)?;
        let client = self.ensure_client(language).await?;
        let warmup = client.is_warming();
        Some((client, warmup))
    }

    async fn reap_idle(&self, idle: Duration) {
        let mut clients = self.clients.lock().await;
        let now = Instant::now();
        let mut remove = Vec::new();
        for (lang, client) in clients.iter() {
            if now.duration_since(client.last_used()) > idle {
                remove.push(lang.clone());
            }
        }
        for lang in remove {
            if let Some(client) = clients.remove(&lang) {
                client.shutdown().await;
                if let Some(spec) = spec_for_language(&lang) {
                    self.set_state(spec.language, "absent", Some("idle timeout".to_string()));
                }
            }
        }
    }

    pub async fn shutdown_all(&self) {
        let mut clients = self.clients.lock().await;
        for (_, client) in clients.drain() {
            client.shutdown().await;
        }
    }

    /// Wait for diagnostics of a single file (used after edit/write). Returns
    /// only error-severity items; `None` means clean (or no server).
    pub async fn diagnostics_for(&self, path: &Path, timeout_ms: u64) -> Option<String> {
        if !self.config.enabled {
            return None;
        }
        let language = language_for_path(path)?;
        let client = self.ensure_client(language).await?;
        let diags = client.file_diagnostics(path, language, timeout_ms).await;
        let items = diags?;
        let errors: Vec<String> = items
            .iter()
            .filter(|d| d.get("severity").and_then(Value::as_u64).unwrap_or(1) == 1)
            .map(format_diagnostic)
            .collect();
        if errors.is_empty() {
            None
        } else {
            Some(errors.join("\n"))
        }
    }

    /// Route an `lsp` tool call: precise server first, name index as fallback.
    pub async fn answer(&self, query: LspQuery) -> Result<LspAnswer> {
        let limit = query.limit.clamp(1, 200);
        let language = language_for_path(&query.path);
        let mut warmup_hint = None;
        if let Some(language) = language {
            if self.config.enabled {
                if let Some((client, warming)) = self.client_for_path(&query.path).await {
                    if warming && query.op != LspOp::Diagnostics {
                        warmup_hint = Some(format!(
                            "{} is still warming up; showing name-based results",
                            spec_for_language(language)
                                .map(|s| s.id)
                                .unwrap_or("server")
                        ));
                    } else {
                        let mut last_error = None;
                        for attempt in 0..3u64 {
                            match self.execute_l1(&client, &query, limit).await {
                                Ok(answer) => return Ok(answer),
                                Err(err) if is_transient_lsp_error(&err) && attempt < 2 => {
                                    tokio::time::sleep(Duration::from_millis(250 * (attempt + 1)))
                                        .await;
                                }
                                Err(err) => {
                                    last_error = Some(err);
                                    break;
                                }
                            }
                        }
                        if let Some(err) = last_error {
                            tracing::debug!(
                                "LSP {} failed: {}; falling back to L0",
                                query.op.as_str(),
                                err
                            );
                            warmup_hint =
                                Some(format!("LSP request failed ({}); name-based fallback", err));
                        }
                    }
                }
            }
        }
        let mut answer = self.execute_l0(&query, limit).await?;
        answer.warmup_hint = warmup_hint;
        Ok(answer)
    }

    async fn execute_l0(&self, query: &LspQuery, limit: usize) -> Result<LspAnswer> {
        let index = self.index.clone();
        let path = query.path.clone();
        let op = query.op;
        let line = query.line;
        let column = query.column;
        let symbol = query.symbol.clone();
        // The index scans the workspace in the background; never block the
        // async runtime for large trees.
        let result = tokio::task::spawn_blocking(move || {
            index.query(op, &path, line, column, symbol.as_deref(), limit)
        })
        .await
        .map_err(|e| HarnessError::tool("E_LSP_L0", e.to_string()))??;
        Ok(LspAnswer::name_based(result.0, result.1))
    }

    async fn execute_l1(
        &self,
        client: &LspClient,
        query: &LspQuery,
        limit: usize,
    ) -> Result<LspAnswer> {
        let timeout = Duration::from_millis(
            if query.timeout_ms == 0 {
                self.config.request_timeout_ms
            } else {
                query.timeout_ms
            }
            .max(500),
        );
        let position = self.prepare_position(&query.path, query.line, query.column)?;
        let uri = uri_from_path(&query.path);
        let language = language_for_path(&query.path).unwrap_or("plaintext");
        // Diagnostics subscribe before syncing so a fast publish cannot be
        // missed; other operations only need the document to be current.
        if query.op != LspOp::Diagnostics {
            client.sync_file(&query.path, language).await?;
        }
        client.touch();

        let (text, raw) = match query.op {
            LspOp::Diagnostics => {
                let items = client
                    .file_diagnostics(&query.path, language, timeout.as_millis() as u64)
                    .await
                    .unwrap_or_default();
                let errors: Vec<String> = items
                    .iter()
                    .filter(|d| d.get("severity").and_then(Value::as_u64).unwrap_or(1) == 1)
                    .map(format_diagnostic)
                    .collect();
                let text = if errors.is_empty() {
                    "no diagnostics".to_string()
                } else {
                    errors.join("\n")
                };
                (text, json!(items))
            }
            LspOp::Definition => {
                let result = client
                    .request_with_timeout(
                        "textDocument/definition",
                        json!({ "textDocument": { "uri": uri }, "position": position }),
                        timeout,
                    )
                    .await?;
                (format_locations(&result, &self.cwd, limit), result.clone())
            }
            LspOp::TypeDefinition => {
                let result = client
                    .request_with_timeout(
                        "textDocument/typeDefinition",
                        json!({ "textDocument": { "uri": uri }, "position": position }),
                        timeout,
                    )
                    .await?;
                (format_locations(&result, &self.cwd, limit), result.clone())
            }
            LspOp::Implementation => {
                let result = client
                    .request_with_timeout(
                        "textDocument/implementation",
                        json!({ "textDocument": { "uri": uri }, "position": position }),
                        timeout,
                    )
                    .await?;
                (format_locations(&result, &self.cwd, limit), result.clone())
            }
            LspOp::References => {
                let result = client
                    .request_with_timeout(
                        "textDocument/references",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": position,
                            "context": { "includeDeclaration": true }
                        }),
                        timeout,
                    )
                    .await?;
                (format_locations(&result, &self.cwd, limit), result.clone())
            }
            LspOp::Hover => {
                let result = client
                    .request_with_timeout(
                        "textDocument/hover",
                        json!({ "textDocument": { "uri": uri }, "position": position }),
                        timeout,
                    )
                    .await?;
                let text = format_hover(&result);
                (text, result)
            }
            LspOp::DocumentSymbols => {
                let result = client
                    .request_with_timeout(
                        "textDocument/documentSymbol",
                        json!({ "textDocument": { "uri": uri } }),
                        timeout,
                    )
                    .await?;
                (format_document_symbols(&result, limit), result.clone())
            }
            LspOp::WorkspaceSymbols => {
                let symbol = query.symbol.clone().unwrap_or_default();
                let result = client
                    .request_with_timeout("workspace/symbol", json!({ "query": symbol }), timeout)
                    .await?;
                (
                    format_workspace_symbols(&result, &self.cwd, limit),
                    result.clone(),
                )
            }
            LspOp::CallHierarchy => {
                let prepared = client
                    .request_with_timeout(
                        "textDocument/prepareCallHierarchy",
                        json!({ "textDocument": { "uri": uri }, "position": position }),
                        timeout,
                    )
                    .await?;
                let items = match &prepared {
                    Value::Array(items) => items.clone(),
                    _ => Vec::new(),
                };
                if items.is_empty() {
                    return Err(HarnessError::tool(
                        "E_LSP_NO_RESULT",
                        "no call hierarchy item at position",
                    ));
                }
                let item = items[0].clone();
                let incoming = client
                    .request_with_timeout(
                        "callHierarchy/incomingCalls",
                        json!({ "item": item }),
                        timeout,
                    )
                    .await
                    .unwrap_or(Value::Array(Vec::new()));
                let outgoing = client
                    .request_with_timeout(
                        "callHierarchy/outgoingCalls",
                        json!({ "item": item }),
                        timeout,
                    )
                    .await
                    .unwrap_or(Value::Array(Vec::new()));
                let text = format_call_hierarchy(&item, &incoming, &outgoing, &self.cwd, limit);
                (text, json!({ "incoming": incoming, "outgoing": outgoing }))
            }
            LspOp::Rename => {
                let new_name = query.symbol.clone().ok_or_else(|| {
                    HarnessError::tool(
                        "E_INVALID_ARGUMENT",
                        "rename requires `symbol` as the new name",
                    )
                })?;
                let edit = client
                    .request_with_timeout(
                        "textDocument/rename",
                        json!({ "textDocument": { "uri": uri }, "position": position, "newName": new_name }),
                        timeout,
                    )
                    .await?;
                if query.apply {
                    let applied = apply_workspace_edit(&edit, &self.cwd)?;
                    let text = if applied.is_empty() {
                        "no changes".to_string()
                    } else {
                        applied
                            .iter()
                            .map(|f| format!("{}:\n{}", f.path, f.diff))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    (
                        text,
                        json!({ "applied": true, "files": applied.iter().map(|f| f.path.clone()).collect::<Vec<_>>() }),
                    )
                } else {
                    let preview = format_workspace_edit_preview(&edit, &self.cwd);
                    (
                        format!("rename preview (dry-run; pass apply=true to write):\n{preview}"),
                        edit.clone(),
                    )
                }
            }
        };

        Ok(LspAnswer {
            match_kind: "lsp-precise",
            text,
            warmup_hint: None,
            raw,
            applied: Vec::new(),
        })
    }

    fn prepare_position(&self, path: &Path, line: u32, column: u32) -> Result<Value> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let line0 = line.saturating_sub(1);
        let col0 = column.saturating_sub(1);
        let prefix = text
            .lines()
            .nth(line0 as usize)
            .map(|l| {
                l.chars()
                    .take(col0 as usize)
                    .map(|c| c.len_utf16())
                    .sum::<usize>()
            })
            .unwrap_or(0);
        Ok(json!({ "line": line0, "character": prefix }))
    }
}

/// Pin a SHA-256 supplied for a single install. The digest is validated and
/// used for this call only; it is never written back to `config.json`.
pub fn pin_sha256(config: &mut LspConfig, server: &str, sha256: &str) -> Result<()> {
    let sha = sha256.trim().to_ascii_lowercase();
    if !valid_sha256(&sha) {
        return Err(HarnessError::Config(
            "sha256 must be exactly 64 hexadecimal characters".to_string(),
        ));
    }
    config
        .downloads
        .entry(server.to_string())
        .or_default()
        .sha256 = Some(sha);
    Ok(())
}

/// One-shot install/update used by the CLI and the TUI. A management hub never
/// stays alive: it is shut down before the result is returned.
pub async fn install_server(
    config: &LspConfig,
    cwd: &Path,
    server: &str,
    update: bool,
    events: Option<UnboundedSender<HarnessEvent>>,
) -> Result<PathBuf> {
    let hub = LspHub::new(cwd.to_path_buf(), &enabled(config), events);
    hub.grant_download(server, true);
    let result = if update {
        hub.update(server).await
    } else {
        hub.install(server).await
    };
    hub.shutdown_all().await;
    result
}

/// One-shot checksum discovery used by the CLI and the TUI.
pub async fn checksum_server(config: &LspConfig, cwd: &Path, server: &str) -> Result<ChecksumInfo> {
    let hub = LspHub::new(cwd.to_path_buf(), &enabled(config), None);
    let result = hub.checksum(server).await;
    hub.shutdown_all().await;
    result
}

fn enabled(config: &LspConfig) -> LspConfig {
    let mut config = config.clone();
    config.enabled = true;
    config
}

fn client_capabilities() -> Value {
    json!({
        "workspace": {
            "workspaceFolders": true,
            "configuration": true,
            "symbol": {},
            "applyEdit": false
        },
        "textDocument": {
            "synchronization": { "dynamicRegistration": false, "didSave": false },
            "definition": { "linkSupport": true },
            "typeDefinition": { "linkSupport": true },
            "implementation": { "linkSupport": true },
            "references": {},
            "hover": { "contentFormat": ["markdown", "plaintext"] },
            "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
            "rename": { "prepareSupport": false },
            "publishDiagnostics": { "relatedInformation": false, "versionSupport": true },
            "callHierarchy": {}
        },
        "window": { "workDoneProgress": true },
        "general": { "positionEncodings": ["utf-16"] }
    })
}

// ---------------------------------------------------------------------------
// JSON-RPC client
// ---------------------------------------------------------------------------

type RpcResult = std::result::Result<Value, String>;
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<RpcResult>>>>;

pub struct LspClient {
    pub server_id: String,
    pub pid: u32,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    child: std::sync::Mutex<Child>,
    next_id: AtomicI64,
    pending: PendingMap,
    docs: Mutex<HashMap<PathBuf, DocState>>,
    diagnostics: Arc<Mutex<HashMap<PathBuf, DiagEntry>>>,
    diag_seq: Arc<watch::Sender<u64>>,
    capabilities: Mutex<Value>,
    shutdown: AtomicBool,
    spawned_at: Instant,
    ready_at: Instant,
    last_used: Mutex<Instant>,
}

struct DocState {
    version: i64,
    digest: u32,
}

#[derive(Clone)]
struct DiagEntry {
    seq: u64,
    items: Vec<Value>,
}

impl LspClient {
    pub async fn spawn(spec: &ServerSpec, command: &Path, cwd: &Path) -> Result<Arc<Self>> {
        let mut cmd = Command::new(command);
        cmd.args(spec.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| {
            HarnessError::tool("E_LSP_SPAWN", format!("{}: {e}", command.display()))
        })?;
        let pid = child.id().unwrap_or(0);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::tool("E_LSP_SPAWN", "server stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::tool("E_LSP_SPAWN", "server stdout unavailable"))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("[lsp] {line}");
                }
            });
        }

        let stdin = Arc::new(AsyncMutex::new(stdin));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics = Arc::new(Mutex::new(HashMap::new()));
        let (diag_tx, _) = watch::channel(0u64);
        let diag_seq = Arc::new(diag_tx);
        let now = Instant::now();

        let client = Arc::new(Self {
            server_id: spec.id.to_string(),
            pid,
            stdin: stdin.clone(),
            child: std::sync::Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending: pending.clone(),
            docs: Mutex::new(HashMap::new()),
            diagnostics: diagnostics.clone(),
            diag_seq: diag_seq.clone(),
            capabilities: Mutex::new(Value::Null),
            shutdown: AtomicBool::new(false),
            spawned_at: now,
            ready_at: now + Duration::from_secs(2),
            last_used: Mutex::new(now),
        });

        // Reader loop.
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(bytes)) = read_frame(&mut reader).await {
                let value: Value = match serde_json::from_slice(&bytes) {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::debug!("[lsp] bad message: {err}");
                        continue;
                    }
                };
                handle_message(&value, &stdin, &pending, &diagnostics, &diag_seq).await;
            }
            // Fail all pending requests so callers do not wait for the timeout.
            let mut pending = pending.lock().unwrap();
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err("server exited".to_string()));
            }
        });

        Ok(client)
    }

    pub fn is_warming(&self) -> bool {
        Instant::now() < self.ready_at
    }

    pub fn spawned_at(&self) -> Instant {
        self.spawned_at
    }

    pub fn capabilities(&self) -> Value {
        self.capabilities.lock().unwrap().clone()
    }

    pub fn set_capabilities(&self, caps: Value) {
        *self.capabilities.lock().unwrap() = caps;
    }

    pub fn alive(&self) -> bool {
        let mut child = self.child.lock().unwrap();
        matches!(child.try_wait(), Ok(None))
    }

    pub fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    pub fn last_used(&self) -> Instant {
        *self.last_used.lock().unwrap()
    }

    async fn send_value(&self, value: &Value) -> Result<()> {
        write_message(&self.stdin, value).await
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_value(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(HarnessError::tool(
                "E_LSP_UNAVAILABLE",
                "server is shut down",
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let payload = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(err) = self.send_value(&payload).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(err);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => {
                self.touch();
                Ok(result)
            }
            Ok(Ok(Err(err))) => Err(HarnessError::tool("E_LSP_ERROR", err)),
            Ok(Err(_)) => Err(HarnessError::tool("E_LSP_ERROR", "response channel closed")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                let _ = self.notify("$/cancelRequest", json!({ "id": id })).await;
                Err(HarnessError::tool(
                    "E_LSP_TIMEOUT",
                    format!("{method} timed out after {} ms", timeout.as_millis()),
                ))
            }
        }
    }

    /// Opens or updates `path` in the language server. Returns `true` when a
    /// `didOpen`/`didChange` notification was sent (i.e. the content changed).
    pub async fn sync_file(&self, path: &Path, language: &str) -> Result<bool> {
        let text = std::fs::read_to_string(path)?;
        if text.len() > 4 * 1024 * 1024 {
            return Err(HarnessError::tool(
                "E_LSP_FILE_TOO_LARGE",
                "files over 4 MiB are not synced to the language server",
            ));
        }
        let digest = xxhash_rust::xxh32::xxh32(text.as_bytes(), 0);
        let uri = uri_from_path(path);
        enum Action {
            None,
            Change(i64, String),
            Open(i64, String),
        }
        let action = {
            let mut docs = self.docs.lock().unwrap();
            match docs.get_mut(path) {
                Some(doc) if doc.digest == digest => Action::None,
                Some(doc) => {
                    doc.version += 1;
                    doc.digest = digest;
                    Action::Change(doc.version, text)
                }
                None => {
                    docs.insert(path.to_path_buf(), DocState { version: 1, digest });
                    Action::Open(1, text)
                }
            }
        };
        match action {
            Action::None => Ok(false),
            Action::Open(version, text) => {
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language,
                            "version": version,
                            "text": text
                        }
                    }),
                )
                .await?;
                Ok(true)
            }
            Action::Change(version, text) => {
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await?;
                Ok(true)
            }
        }
    }

    /// Wait up to `timeout_ms` for `publishDiagnostics` after syncing the file.
    /// When the file is unchanged, a cached report is returned immediately
    /// instead of waiting for a notification the server will not send.
    /// Returns `None` when there is no report.
    pub async fn file_diagnostics(
        &self,
        path: &Path,
        language: &str,
        timeout_ms: u64,
    ) -> Option<Vec<Value>> {
        let mut rx = self.diag_seq.subscribe();
        let before = *rx.borrow();
        let synced = match self.sync_file(path, language).await {
            Ok(synced) => synced,
            Err(_) => return None,
        };
        if !synced {
            if let Some(entry) = self.diagnostics.lock().unwrap().get(path) {
                return Some(entry.items.clone());
            }
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(50));
        // rust-analyzer publishes an empty report as soon as the file is opened
        // and the real report after analysis/flycheck. Keep collecting until
        // reports have been quiet for a moment so the later one is not lost.
        let settle = Duration::from_millis(1_200).min(Duration::from_millis(timeout_ms.max(50)));
        let mut latest: Option<Vec<Value>> = None;
        let mut quiet_at: Option<Instant> = None;
        let mut seen = before;
        loop {
            if let Some(entry) = self.diagnostics.lock().unwrap().get(path) {
                if entry.seq > seen {
                    seen = entry.seq;
                    latest = Some(entry.items.clone());
                    quiet_at = Some(Instant::now() + settle);
                }
            }
            let now = Instant::now();
            if let Some(quiet_at) = quiet_at {
                if now >= quiet_at {
                    return latest;
                }
            }
            if now >= deadline {
                return latest;
            }
            let wake = quiet_at.map(|q| q.min(deadline)).unwrap_or(deadline);
            let remaining = wake.saturating_duration_since(now);
            let _ = tokio::time::timeout(remaining, rx.changed()).await;
        }
    }

    pub async fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self
            .request_with_timeout("shutdown", Value::Null, Duration::from_millis(500))
            .await;
        let _ = self.notify("exit", Value::Null).await;
        #[cfg(unix)]
        {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        {
            let mut child = self.child.lock().unwrap();
            let _ = child.start_kill();
            let _ = child.try_wait();
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if !self.shutdown.load(Ordering::SeqCst) && self.pid > 0 {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(self.pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
    }
}

async fn write_message(stdin: &Arc<AsyncMutex<ChildStdin>>, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut guard = stdin.lock().await;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    guard.write_all(header.as_bytes()).await?;
    guard.write_all(&body).await?;
    guard.flush().await?;
    Ok(())
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok();
        }
    }
    let Some(len) = content_length else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing Content-Length",
        ));
    };
    if len > 32 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

async fn handle_message(
    value: &Value,
    stdin: &Arc<AsyncMutex<ChildStdin>>,
    pending: &PendingMap,
    diagnostics: &Arc<Mutex<HashMap<PathBuf, DiagEntry>>>,
    diag_seq: &Arc<watch::Sender<u64>>,
) {
    let id = value.get("id").cloned();
    let method = value.get("method").and_then(Value::as_str);
    match (id, method) {
        (Some(id), Some(method)) => {
            // Server-to-client request.
            let result = match method {
                "workspace/configuration" => {
                    let count = value
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0);
                    Value::Array(vec![Value::Null; count])
                }
                "workspace/applyEdit" => {
                    json!({ "applied": false, "failureReason": "not supported" })
                }
                _ => Value::Null,
            };
            let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let _ = write_message(stdin, &response).await;
        }
        (Some(id), None) => {
            let Some(id) = id.as_i64() else { return };
            let Some(tx) = pending.lock().unwrap().remove(&id) else {
                return;
            };
            if let Some(error) = value.get("error") {
                let message = format!(
                    "{} (code {})",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("error"),
                    error.get("code").and_then(Value::as_i64).unwrap_or(0)
                );
                let _ = tx.send(Err(message));
            } else {
                let _ = tx.send(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
            }
        }
        (None, Some("textDocument/publishDiagnostics")) => {
            let Some(params) = value.get("params") else {
                return;
            };
            let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                return;
            };
            let Ok(url) = url::Url::parse(uri) else {
                return;
            };
            let Ok(path) = url.to_file_path() else { return };
            let items = params
                .get("diagnostics")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let seq = {
                let current = *diag_seq.borrow();
                let next = current + 1;
                let _ = diag_seq.send(next);
                next
            };
            diagnostics
                .lock()
                .unwrap()
                .insert(path, DiagEntry { seq, items });
        }
        _ => {}
    }
}

fn uri_from_path(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

// ---------------------------------------------------------------------------
// Result formatting
// ---------------------------------------------------------------------------

fn location_parts(value: &Value) -> Option<(&str, u64, u64)> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))?
        .as_str()?;
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"))?;
    let line = range.pointer("/start/line")?.as_u64()?;
    let col = range.pointer("/start/character")?.as_u64()?;
    Some((uri, line, col))
}

fn relative_display(cwd: &Path, uri: &str) -> String {
    match url::Url::parse(uri)
        .ok()
        .and_then(|u| u.to_file_path().ok())
    {
        Some(path) => {
            let rel = path.strip_prefix(cwd).unwrap_or(&path);
            rel.to_string_lossy().replace('\\', "/")
        }
        None => uri.to_string(),
    }
}

fn format_locations(value: &Value, cwd: &Path, limit: usize) -> String {
    let items: Vec<Value> = match value {
        Value::Array(items) => items.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    };
    if items.is_empty() {
        return "no matches".to_string();
    }
    let mut lines = Vec::new();
    for item in items.iter().take(limit) {
        let (uri, line, col) = if let Some(parts) = location_parts(item) {
            parts
        } else if let (Some(uri), Some(range)) = (
            item.get("targetUri").and_then(Value::as_str),
            item.get("targetSelectionRange"),
        ) {
            (
                uri,
                range
                    .pointer("/start/line")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                range
                    .pointer("/start/character")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        } else {
            continue;
        };
        let path = relative_display(cwd, uri);
        let preview = item
            .get("name")
            .and_then(Value::as_str)
            .map(|n| format!("  {n}"))
            .unwrap_or_default();
        lines.push(format!("{}:{}:{}{}", path, line + 1, col + 1, preview));
    }
    if lines.is_empty() {
        "no matches".to_string()
    } else {
        if items.len() > limit {
            lines.push(format!("... ({} of {} results)", limit, items.len()));
        }
        lines.join("\n")
    }
}

fn format_hover(value: &Value) -> String {
    let contents = value.get("contents").cloned().unwrap_or(Value::Null);
    fn render(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Object(map) => {
                if let Some(value) = map.get("value").and_then(Value::as_str) {
                    value.to_string()
                } else if let Some(language) = map.get("language").and_then(Value::as_str) {
                    format!(
                        "```{language}\n{}\n```",
                        map.get("value").and_then(Value::as_str).unwrap_or("")
                    )
                } else {
                    v.to_string()
                }
            }
            Value::Array(items) => {
                if let Some(first) = items.first() {
                    if first.get("language").is_some() {
                        format!(
                            "```{}\n{}\n```",
                            first.get("language").and_then(Value::as_str).unwrap_or(""),
                            first.get("value").and_then(Value::as_str).unwrap_or("")
                        )
                    } else {
                        format!(" {}", render(first))
                    }
                } else {
                    String::new()
                }
            }
            _ => v.to_string(),
        }
    }
    let text = render(&contents);
    let text = text.trim();
    if text.is_empty() {
        "no hover information".to_string()
    } else {
        text.to_string()
    }
}

fn format_document_symbols(value: &Value, limit: usize) -> String {
    fn walk(items: &[Value], depth: usize, out: &mut Vec<String>, limit: usize) {
        for item in items {
            if out.len() >= limit {
                return;
            }
            let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
            let kind = item
                .get("kind")
                .and_then(Value::as_u64)
                .map(symbol_kind)
                .unwrap_or("symbol");
            let (line, col) = item
                .get("selectionRange")
                .or_else(|| item.get("range"))
                .and_then(|r| {
                    Some((
                        r.pointer("/start/line")?.as_u64()?,
                        r.pointer("/start/character")?.as_u64()?,
                    ))
                })
                .unwrap_or((0, 0));
            out.push(format!(
                "{}{} {:<10} {}:{}",
                "  ".repeat(depth),
                kind,
                name,
                line + 1,
                col + 1
            ));
            if let Some(children) = item.get("children").and_then(Value::as_array) {
                walk(children, depth + 1, out, limit);
            }
        }
    }
    let items = match value {
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    };
    if items.is_empty() {
        return "no symbols".to_string();
    }
    let mut out = Vec::new();
    walk(&items, 0, &mut out, limit);
    out.join("\n")
}

fn format_workspace_symbols(value: &Value, cwd: &Path, limit: usize) -> String {
    let items = match value {
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    };
    if items.is_empty() {
        return "no symbols".to_string();
    }
    let mut out = Vec::new();
    for item in items.iter().take(limit) {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = item
            .get("kind")
            .and_then(Value::as_u64)
            .map(symbol_kind)
            .unwrap_or("symbol");
        if let Some(location) = item.get("location") {
            if let Some((uri, line, col)) = location_parts(location) {
                out.push(format!(
                    "{}:{}:{} {} {}",
                    relative_display(cwd, uri),
                    line + 1,
                    col + 1,
                    kind,
                    name
                ));
                continue;
            }
        }
        out.push(format!("{kind} {name}"));
    }
    out.join("\n")
}

fn format_call_hierarchy(
    item: &Value,
    incoming: &Value,
    outgoing: &Value,
    cwd: &Path,
    limit: usize,
) -> String {
    let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
    let mut out = vec![format!("call hierarchy for {name}")];
    out.push("incoming:".to_string());
    let add = |value: &Value, label: &str, out: &mut Vec<String>| {
        if let Value::Array(items) = value {
            if items.is_empty() {
                out.push("  none".to_string());
            }
            for call in items.iter().take(limit) {
                let callee = call.get("from").or_else(|| call.get("to"));
                let name = callee
                    .and_then(|i| i.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let locations = call
                    .get("fromRanges")
                    .or_else(|| call.get("toRanges"))
                    .and_then(Value::as_array)
                    .and_then(|a| a.first());
                let pos = locations
                    .and_then(|r| {
                        Some((
                            r.pointer("/start/line")?.as_u64()?,
                            r.pointer("/start/character")?.as_u64()?,
                        ))
                    })
                    .map(|(l, c)| format!(":{}:{}", l + 1, c + 1))
                    .unwrap_or_default();
                let _ = label;
                out.push(format!("  {name}{pos}"));
                let _ = cwd;
            }
        }
    };
    add(incoming, "incoming", &mut out);
    out.push("outgoing:".to_string());
    add(outgoing, "outgoing", &mut out);
    out.join("\n")
}

fn symbol_kind(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum-member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type-parameter",
        _ => "symbol",
    }
}

/// Errors the server reports while it is still loading the workspace. Retrying
/// these is correct: the request itself is valid, only its timing was early.
fn is_transient_lsp_error(err: &HarnessError) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    message.contains("content modified")
        || message.contains("server cancelled")
        || message.contains("request cancelled")
        || message.contains("(code -32800)")
        || message.contains("(code -32801)")
        || message.contains("(code -32802)")
}

fn format_diagnostic(diag: &Value) -> String {
    let range = diag.get("range");
    let line = range
        .and_then(|r| r.pointer("/start/line"))
        .and_then(Value::as_u64)
        .map(|l| l + 1)
        .unwrap_or(0);
    let col = range
        .and_then(|r| r.pointer("/start/character"))
        .and_then(Value::as_u64)
        .map(|c| c + 1)
        .unwrap_or(0);
    let severity = match diag.get("severity").and_then(Value::as_u64) {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "info",
        Some(4) => "hint",
        _ => "error",
    };
    let message = diag.get("message").and_then(Value::as_str).unwrap_or("");
    let code = diag
        .get("code")
        .map(|c| match c {
            Value::String(s) => format!("[{s}] "),
            other => format!("[{other}] "),
        })
        .unwrap_or_default();
    format!("{severity} at {line}:{col}: {code}{message}")
}

// ---------------------------------------------------------------------------
// Workspace edits
// ---------------------------------------------------------------------------

struct TextEditSpec {
    start_line: usize,
    start_char: usize,
    end_line: usize,
    end_char: usize,
    new_text: String,
}

fn workspace_edits(value: &Value) -> Vec<(PathBuf, Vec<TextEditSpec>)> {
    let mut per_file: HashMap<String, Vec<Value>> = HashMap::new();
    if let Some(changes) = value.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            if let Some(list) = edits.as_array() {
                per_file
                    .entry(uri.clone())
                    .or_default()
                    .extend(list.clone());
            }
        }
    }
    if let Some(changes) = value.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            if let Some(uri) = change.pointer("/textDocument/uri").and_then(Value::as_str) {
                if let Some(edits) = change.get("edits").and_then(Value::as_array) {
                    per_file
                        .entry(uri.to_string())
                        .or_default()
                        .extend(edits.clone());
                }
            }
        }
    }
    let mut result = Vec::new();
    for (uri, edits) in per_file {
        let Ok(url) = url::Url::parse(&uri) else {
            continue;
        };
        let Ok(path) = url.to_file_path() else {
            continue;
        };
        let mut specs = Vec::new();
        for edit in edits {
            let Some(range) = edit.get("range") else {
                continue;
            };
            let Some(new_text) = edit.get("newText").and_then(Value::as_str) else {
                continue;
            };
            let start_line = range
                .pointer("/start/line")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let start_char = range
                .pointer("/start/character")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let end_line = range
                .pointer("/end/line")
                .and_then(Value::as_u64)
                .unwrap_or(start_line as u64) as usize;
            let end_char = range
                .pointer("/end/character")
                .and_then(Value::as_u64)
                .unwrap_or(start_char as u64) as usize;
            specs.push(TextEditSpec {
                start_line,
                start_char,
                end_line,
                end_char,
                new_text: new_text.to_string(),
            });
        }
        if !specs.is_empty() {
            result.push((path, specs));
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn format_workspace_edit_preview(value: &Value, cwd: &Path) -> String {
    let files = workspace_edits(value);
    if files.is_empty() {
        return "no files changed".to_string();
    }
    let mut out = Vec::new();
    for (path, edits) in &files {
        let rel = path
            .strip_prefix(cwd)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let lines: Vec<String> = edits
            .iter()
            .map(|e| {
                format!(
                    "  {}:{} replace with {:?}",
                    e.start_line + 1,
                    e.start_char + 1,
                    e.new_text
                )
            })
            .collect();
        out.push(format!(
            "{} ({} edits)\n{}",
            rel,
            edits.len(),
            lines.join("\n")
        ));
    }
    out.join("\n")
}

fn apply_workspace_edit(value: &Value, cwd: &Path) -> Result<Vec<AppliedFile>> {
    let files = workspace_edits(value);
    let mut applied = Vec::new();
    for (path, mut edits) in files {
        if !path.starts_with(cwd) {
            return Err(HarnessError::tool(
                "E_PATH_ESCAPE",
                format!(
                    "rename would modify a file outside the workspace: {}",
                    path.display()
                ),
            ));
        }
        let original = std::fs::read_to_string(&path).map_err(|e| {
            HarnessError::tool(
                "E_LSP_RENAME",
                format!("cannot read {}: {e}", path.display()),
            )
        })?;
        edits.sort_by_key(|a| std::cmp::Reverse((a.start_line, a.start_char)));
        let mut text = original.clone();
        for edit in edits {
            let start = offset_for_position(&text, edit.start_line, edit.start_char);
            let end = offset_for_position(&text, edit.end_line, edit.end_char);
            if start > end || end > text.len() {
                return Err(HarnessError::tool(
                    "E_LSP_RENAME",
                    format!("invalid edit range in {}", path.display()),
                ));
            }
            text.replace_range(start..end, &edit.new_text);
        }
        if text == original {
            continue;
        }
        crate::tools::fs_tools::atomic_write(&path, text.as_bytes(), true)?;
        let diff = similar::TextDiff::from_lines(&original, &text)
            .unified_diff()
            .header("before", "after")
            .to_string();
        applied.push(AppliedFile {
            path: path
                .strip_prefix(cwd)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/"),
            diff,
        });
    }
    Ok(applied)
}

/// Convert an LSP UTF-16 (line, character) position to a byte offset.
fn offset_for_position(text: &str, line: usize, character: usize) -> usize {
    let mut current_line = 0;
    let mut line_start = 0;
    for (idx, ch) in text.char_indices() {
        if current_line == line {
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = idx + 1;
        }
    }
    if current_line < line {
        return text.len();
    }
    let mut utf16 = 0;
    for (idx, ch) in text[line_start..].char_indices() {
        if utf16 >= character || ch == '\n' {
            return line_start + idx;
        }
        utf16 += ch.len_utf16();
    }
    text.len()
}

// ---------------------------------------------------------------------------
// Download helpers
// ---------------------------------------------------------------------------

fn extract_archive(
    bytes: &[u8],
    kind: ArchiveKind,
    target: &Path,
    binary: &str,
) -> Result<PathBuf> {
    let binary_name = Path::new(binary)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(binary);
    match kind {
        ArchiveKind::Raw => {
            let path = target.join(exe_name(binary_name));
            std::fs::write(&path, bytes)?;
            Ok(path)
        }
        ArchiveKind::Gz => {
            let mut decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
            let path = target.join(exe_name(binary_name));
            let mut out = std::fs::File::create(&path)?;
            std::io::copy(&mut decoder, &mut out)?;
            Ok(path)
        }
        ArchiveKind::Zip => {
            let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
                .map_err(|e| HarnessError::tool("E_LSP_ARCHIVE", format!("bad zip: {e}")))?;
            for i in 0..archive.len() {
                let mut file = archive
                    .by_index(i)
                    .map_err(|e| HarnessError::tool("E_LSP_ARCHIVE", e.to_string()))?;
                let Some(enclosed) = file.enclosed_name() else {
                    continue;
                };
                let out_path = target.join(&enclosed);
                if file.is_dir() {
                    std::fs::create_dir_all(&out_path)?;
                    continue;
                }
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut out = std::fs::File::create(&out_path)?;
                std::io::copy(&mut file, &mut out)?;
            }
            Ok(find_extracted_binary(target, binary_name)?)
        }
        ArchiveKind::TarGz => {
            let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
            let mut archive = tar::Archive::new(decoder);
            for entry in archive
                .entries()
                .map_err(|e| HarnessError::tool("E_LSP_ARCHIVE", e.to_string()))?
            {
                let mut entry =
                    entry.map_err(|e| HarnessError::tool("E_LSP_ARCHIVE", e.to_string()))?;
                let path = entry
                    .path()
                    .map_err(|e| HarnessError::tool("E_LSP_ARCHIVE", e.to_string()))?
                    .to_path_buf();
                if path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    continue;
                }
                let out_path = target.join(&path);
                if entry.header().entry_type().is_dir() {
                    std::fs::create_dir_all(&out_path)?;
                    continue;
                }
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut out = std::fs::File::create(&out_path)?;
                std::io::copy(&mut entry, &mut out)?;
            }
            Ok(find_extracted_binary(target, binary_name)?)
        }
    }
}

fn find_extracted_binary(root: &Path, name: &str) -> Result<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut fallback = None;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if file_name == name || file_name == exe_name(name) {
                    // Prefer the shallowest candidate.
                    if fallback
                        .as_ref()
                        .map(|p: &PathBuf| p.components().count() > path.components().count())
                        .unwrap_or(true)
                    {
                        fallback = Some(path);
                    }
                }
            }
        }
    }
    fallback.ok_or_else(|| {
        HarnessError::tool(
            "E_LSP_ARCHIVE",
            format!("binary `{name}` not found in archive"),
        )
    })
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn registry_covers_common_languages() {
        for lang in [
            "rust",
            "go",
            "python",
            "typescript",
            "javascript",
            "java",
            "c",
            "cpp",
            "lua",
        ] {
            assert!(spec_for_language(lang).is_some(), "missing {lang}");
        }
        assert_eq!(language_for_path(Path::new("a/b.rs")), Some("rust"));
        assert_eq!(language_for_path(Path::new("a/b.unknown")), None);
    }

    #[test]
    fn utf16_positions_convert_to_offsets() {
        let text = "a😀b\nsecond\n";
        // LSP positions count UTF-16 units: 'a'=1, '😀'=2, 'b'=1.
        assert_eq!(offset_for_position(text, 0, 3), 5); // before 'b'
        assert_eq!(offset_for_position(text, 0, 4), 6); // after 'b'
        assert_eq!(offset_for_position(text, 1, 0), 7); // after the newline
        assert_eq!(offset_for_position(text, 99, 0), text.len());
    }

    #[test]
    fn workspace_edit_preview_and_apply() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn old() {}\n").unwrap();
        let uri = uri_from_path(&file);
        let edit = json!({
            "changes": {
                uri.clone(): [{
                    "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } },
                    "newText": "new"
                }]
            }
        });
        let preview = format_workspace_edit_preview(&edit, dir.path());
        assert!(preview.contains("a.rs"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn old() {}\n");
        let applied = apply_workspace_edit(&edit, dir.path()).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn new() {}\n");
    }

    #[test]
    fn workspace_edit_refuses_escape() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let file = other.path().join("outside.rs");
        std::fs::write(&file, "fn old() {}\n").unwrap();
        let edit = json!({
            "changes": {
                uri_from_path(&file): [{
                    "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } },
                    "newText": "new"
                }]
            }
        });
        assert!(apply_workspace_edit(&edit, dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn old() {}\n");
    }

    #[test]
    fn zip_extraction_picks_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("nested/bin/server", options).unwrap();
            zip.write_all(b"#!/bin/sh\n").unwrap();
            zip.finish().unwrap();
        }
        let out = extract_archive(&buf, ArchiveKind::Zip, dir.path(), "server").unwrap();
        assert!(out.ends_with("nested/bin/server"));
        assert_eq!(std::fs::read_to_string(out).unwrap(), "#!/bin/sh\n");
    }

    #[test]
    fn diagnostic_formatting() {
        let diag = json!({
            "range": { "start": { "line": 4, "character": 2 }, "end": { "line": 4, "character": 5 } },
            "severity": 1,
            "code": "E0308",
            "message": "mismatched types"
        });
        assert_eq!(
            format_diagnostic(&diag),
            "error at 5:3: [E0308] mismatched types"
        );
    }

    #[test]
    fn transient_lsp_errors_are_retried() {
        assert!(is_transient_lsp_error(&HarnessError::tool(
            "E_LSP_ERROR",
            "content modified (code -32801)"
        )));
        assert!(is_transient_lsp_error(&HarnessError::tool(
            "E_LSP_ERROR",
            "server cancelled (code -32802)"
        )));
        assert!(!is_transient_lsp_error(&HarnessError::tool(
            "E_LSP_ERROR",
            "invalid params (code -32602)"
        )));
    }

    #[test]
    fn sha256_validation_is_strict() {
        assert!(valid_sha256(&"a".repeat(64)));
        assert!(valid_sha256(&"0123456789abcdef".repeat(4)));
        assert!(!valid_sha256(&"a".repeat(63)));
        assert!(!valid_sha256(&"a".repeat(65)));
        assert!(!valid_sha256(&"g".repeat(64)));
        assert!(!valid_sha256(""));
    }

    #[test]
    fn discovered_digests_roundtrip_through_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rust-analyzer").join("2026-10-19.sha256");
        let hash = "0123456789abcdef".repeat(4);
        write_digest_cache(&path, &hash, "GitHub release asset digest");
        assert_eq!(
            read_digest_cache(&path),
            Some((hash, "GitHub release asset digest".to_string()))
        );
        std::fs::write(&path, "not-a-hash\ncached\n").unwrap();
        assert!(read_digest_cache(&path).is_none());
    }

    /// A rate-limited or unreachable API must never be described as "the
    /// vendor publishes no digest": the two paths return different errors.
    #[tokio::test]
    async fn artifact_urls_outside_github_releases_answer_without_a_digest() {
        let dir = tempfile::tempdir().unwrap();
        let hub = LspHub::new(dir.path().to_path_buf(), &LspConfig::default(), None);
        for url in [
            "https://example.com/tool.tar.gz",
            "https://github.com/owner/repo/archive/v1.tar.gz",
        ] {
            assert!(matches!(
                hub.github_release_digest(url).await.unwrap(),
                DigestLookup::NoDigest
            ));
        }
        assert!(hub.github_release_digest("not a url").await.is_err());
    }

    #[test]
    fn release_assets_expose_digests_and_checksum_files() {
        let hash = "0123456789abcdef".repeat(4);
        let assets = vec![
            json!({ "name": "server.zip", "digest": format!("sha256:{hash}"),
                    "browser_download_url": "https://github.com/o/r/releases/download/t/server.zip" }),
            json!({ "name": "SHA256SUMS",
                    "browser_download_url": "https://github.com/o/r/releases/download/t/SHA256SUMS" }),
            json!({ "name": "server.zip.sig",
                    "browser_download_url": "https://github.com/o/r/releases/download/t/server.zip.sig" }),
        ];
        assert_eq!(
            asset_digest(&assets, "server.zip").as_deref(),
            Some(hash.as_str())
        );
        assert!(asset_digest(&assets, "other.zip").is_none());
        let checksums = checksum_assets(&assets);
        assert_eq!(checksums.len(), 1);
        assert_eq!(checksums[0].0, "SHA256SUMS");
    }

    #[test]
    fn checksum_files_are_parsed_in_common_formats() {
        let hash = "0123456789abcdef".repeat(4);
        let gnu = format!(
            "{hash}  lua-language-server-3.19.2-linux-x64.tar.gz
{hash}  other.zip
"
        );
        assert_eq!(
            parse_checksum_file(&gnu, "lua-language-server-3.19.2-linux-x64.tar.gz").as_deref(),
            Some(hash.as_str())
        );
        assert!(parse_checksum_file(&gnu, "missing.zip").is_none());
        let star = format!(
            "{hash} *rust-analyzer-linux.gz
"
        );
        assert_eq!(
            parse_checksum_file(&star, "rust-analyzer-linux.gz").as_deref(),
            Some(hash.as_str())
        );
        let bsd = format!(
            "SHA256 (clangd-linux-23.1.0.zip) = {hash}
"
        );
        assert_eq!(
            parse_checksum_file(&bsd, "clangd-linux-23.1.0.zip").as_deref(),
            Some(hash.as_str())
        );
        assert!(parse_checksum_file(
            "# comment
not a checksum",
            "x"
        )
        .is_none());
    }

    #[test]
    fn download_urls_require_https_and_official_domains() {
        let domains = vec![
            "github.com".to_string(),
            "objects.githubusercontent.com".to_string(),
        ];
        assert!(validate_download_url("https://github.com/o/r/releases/x", &domains).is_ok());
        assert!(
            validate_download_url("https://objects.githubusercontent.com/blob", &domains).is_ok()
        );
        assert!(validate_download_url("http://github.com/o/r", &domains).is_err());
        assert!(validate_download_url("https://evilgithub.com/o/r", &domains).is_err());
        assert!(validate_download_url("https://github.com.evil.test/o/r", &domains).is_err());
        assert!(validate_download_url("https://example.com/o/r", &domains).is_err());
        assert!(validate_download_url("not a url", &domains).is_err());
    }

    #[test]
    fn registry_download_templates_resolve_to_whitelisted_https_urls() {
        for spec in registry() {
            let Some(template) = spec.download_url else {
                continue;
            };
            let url = template
                .replace("{version}", spec.version)
                .replace("{target}", &asset_target(spec));
            assert!(
                !url.contains('{'),
                "{} left a placeholder in {url}",
                spec.id
            );
            let domains: Vec<String> = spec
                .official_domains
                .iter()
                .map(|d| d.to_string())
                .collect();
            assert!(
                validate_download_url(&url, &domains).is_ok(),
                "{} produced {url}",
                spec.id
            );
        }
    }

    #[test]
    fn lua_assets_use_release_platform_names() {
        let lua = spec_for_id("lua-language-server").expect("lua spec");
        let target = asset_target(lua);
        assert_eq!(target, lua_asset_target());
        let url = lua
            .download_url
            .expect("lua download")
            .replace("{version}", lua.version)
            .replace("{target}", &target);
        assert!(url.ends_with(&format!(
            "lua-language-server-{}-{target}.tar.gz",
            lua.version
        )));
    }

    #[test]
    fn format_locations_uses_relative_paths() {
        let cwd = std::env::temp_dir()
            .join("rustedharness-lsp-test")
            .join("w");
        let file = cwd.join("src").join("lib.rs");
        let uri = url::Url::from_file_path(&file).unwrap().to_string();
        let value = json!([{ "uri": uri, "range": { "start": { "line": 2, "character": 4 }, "end": { "line": 2, "character": 8 } } }]);
        assert_eq!(format_locations(&value, &cwd, 10), "src/lib.rs:3:5");
    }
}
