//! Session storage, byte-compatible with pi.
//!
//! The on-disk layout is exactly the one documented for pi v3:
//!
//! ```text
//! <sessions>/<project-id>/<session-id>.jsonl
//! ```
//!
//! The first line is a `session` header; every following line is an entry with
//! `id` / `parentId` / `timestamp` forming a tree, where `type` decides what it
//! carries (`message`, `model_change`, `usage`, `compaction`, `custom`, ...).
//!
//! Two reasons to match it exactly instead of designing our own: a pi session
//! opens here as-is, and a session written here opens in pi.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, SessionConfig};
use crate::error::{HarnessError, Result};
use crate::provider::{ChatMessage, FunctionCall, ToolCall, Usage};
use crate::tools::todo::TodoItem;

/// File format version we write. pi migrates v1/v2 up to this on load.
const FORMAT_VERSION: u32 = 3;

/// Entry kinds that are ours, carried in pi `custom` entries: pi keeps them in
/// the file and ignores them in the model context.
const CUSTOM_TODOS: &str = "rustedharness.todos";
const CUSTOM_COMPACT_MODEL: &str = "rustedharness.compact_model";

#[derive(Debug, Clone, Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: String,
    pub agent_model: String,
    pub compact_model: String,
    pub created_at: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LoadedSession {
    pub meta: Option<SessionMeta>,
    pub messages: Vec<ChatMessage>,
    pub todos: Vec<TodoItem>,
    /// `(summary, messages replaced)` for every compaction on the active branch.
    pub compactions: Vec<(String, usize)>,
    /// A final line that did not parse (typically a torn write after kill -9).
    pub truncated_tail: bool,
    /// Last provider-reported usage, so a resumed session keeps its meters.
    pub usage: Option<Usage>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub path: String,
    pub cwd: String,
    pub agent_model: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: Option<String>,
    pub messages: usize,
    pub bytes: u64,
}

pub struct SessionWriter {
    id: String,
    path: PathBuf,
    /// Opened on the first entry. Until then nothing exists on disk: a plain
    /// launch-and-quit must leave no artifact behind.
    file: Option<File>,
    /// Entries written before the file exists, flushed in order.
    pending: Vec<Entry>,
    header: Header,
    dir: PathBuf,
    max_saved: usize,
    /// Id of the newest entry; the `parentId` of the next one.
    last_id: Option<String>,
    /// Ids of `message` entries in order, for `compaction.firstKeptEntryId`.
    message_ids: Vec<String>,
    provider: String,
    model: String,
    last_usage: PiUsage,
}

impl SessionWriter {
    pub fn create(
        cfg: &SessionConfig,
        cwd: &Path,
        agent_model: &str,
        compact_model: &str,
    ) -> Result<Self> {
        let dir = cfg.dir.clone().unwrap_or_else(Config::sessions_dir);
        let id = uuid::Uuid::new_v4().to_string();
        let path = dir.join(project_dir(cwd)).join(format!("{id}.jsonl"));
        let (provider, model) = split_model(agent_model);
        let mut writer = Self {
            id: id.clone(),
            path,
            file: None,
            pending: Vec::new(),
            header: Header {
                kind: "session".into(),
                version: FORMAT_VERSION,
                id,
                timestamp: now_iso(),
                cwd: cwd.to_string_lossy().to_string(),
                parent_session: None,
            },
            dir,
            max_saved: cfg.max_saved_sessions,
            last_id: None,
            message_ids: Vec::new(),
            provider,
            model,
            last_usage: PiUsage::default(),
        };
        // Record the models up front so `meta` is complete even for a session
        // that has no assistant message yet: pi reads the current model from
        // the latest `model_change` entry.
        writer.models(agent_model, compact_model)?;
        Ok(writer)
    }

    /// Reopen an existing session for appending.
    pub fn resume(path: &Path, id: &str, agent_model: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let (last_id, message_ids) = scan_chain(&raw);
        let (provider, model) = split_model(agent_model);
        Ok(Self {
            id: id.to_string(),
            path: path.to_path_buf(),
            file: None,
            pending: Vec::new(),
            header: Header {
                kind: "session".into(),
                version: FORMAT_VERSION,
                id: id.to_string(),
                timestamp: now_iso(),
                cwd: String::new(),
                parent_session: None,
            },
            dir: path
                .parent()
                .and_then(Path::parent)
                .unwrap_or(Path::new("."))
                .to_path_buf(),
            max_saved: 0,
            last_id,
            message_ids,
            provider,
            model,
            last_usage: PiUsage::default(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a user message. pi stores user content as a plain string.
    pub fn user(&mut self, text: &str) -> Result<()> {
        self.append(EntryKind::Message {
            message: AgentMessage::User {
                content: Content::Text(text.to_string()),
                timestamp: now_millis(),
            },
        })
    }

    pub fn assistant(&mut self, message: &ChatMessage) -> Result<()> {
        let mut content: Vec<Block> = Vec::new();
        if let Some(reasoning) = message.reasoning_content.as_deref() {
            if !reasoning.is_empty() {
                content.push(Block::Thinking {
                    thinking: reasoning.to_string(),
                });
            }
        }
        if let Some(text) = message.content.as_deref() {
            if !text.is_empty() {
                content.push(Block::Text {
                    text: text.to_string(),
                });
            }
        }
        for call in message.tool_calls.iter().flatten() {
            content.push(Block::ToolCall {
                id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| Value::Object(Default::default())),
            });
        }
        self.append(EntryKind::Message {
            message: AgentMessage::Assistant {
                content: Content::Blocks(content),
                api: self.provider.clone(),
                provider: self.provider.clone(),
                model: self.model.clone(),
                usage: self.last_usage,
                stop_reason: "stop".into(),
                timestamp: now_millis(),
            },
        })
    }

    pub fn tool_result(&mut self, id: &str, name: &str, ok: bool, content: &str) -> Result<()> {
        self.append(EntryKind::Message {
            message: AgentMessage::ToolResult {
                tool_call_id: id.to_string(),
                tool_name: name.to_string(),
                content: Content::Blocks(vec![Block::Text {
                    text: content.to_string(),
                }]),
                is_error: !ok,
                timestamp: now_millis(),
            },
        })
    }

    pub fn todos(&mut self, items: &[TodoItem]) -> Result<()> {
        self.append(EntryKind::Custom {
            custom_type: CUSTOM_TODOS.to_string(),
            data: Some(serde_json::to_value(items)?),
        })
    }

    pub fn models(&mut self, agent: &str, compact: &str) -> Result<()> {
        let (provider, model) = split_model(agent);
        self.provider = provider.clone();
        self.model = model.clone();
        self.append(EntryKind::ModelChange {
            provider,
            model_id: model,
        })?;
        self.append(EntryKind::Custom {
            custom_type: CUSTOM_COMPACT_MODEL.to_string(),
            data: Some(Value::String(compact.to_string())),
        })
    }

    /// Record provider-reported usage for the current model.
    ///
    /// pi keeps usage on the assistant message itself; ours is written before
    /// the report arrives, so it lands in a `usage` entry, which pi counts the
    /// same way.
    pub fn usage(&mut self, usage: &Usage, cost_usd: Option<f64>) -> Result<()> {
        let mut pi = PiUsage::from(*usage);
        if let Some(cost) = cost_usd {
            pi.cost.total = cost;
        }
        self.last_usage = pi;
        self.append(EntryKind::Usage {
            kind: "response".into(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            usage: pi,
        })
    }

    /// Record a compaction boundary. `dropped` is how many leading messages the
    /// summary replaced; pi rebuilds the context from `firstKeptEntryId`.
    pub fn compaction(&mut self, summary: &str, dropped: usize) -> Result<()> {
        let own_id = new_id();
        let first_kept = self
            .message_ids
            .get(dropped)
            .cloned()
            .unwrap_or_else(|| own_id.clone());
        let tokens_before = self.last_usage.total_tokens;
        self.append_with_id(
            own_id,
            EntryKind::Compaction {
                summary: summary.to_string(),
                first_kept_entry_id: first_kept,
                tokens_before,
            },
        )?;
        // Later compactions index into the retained messages only.
        let keep = dropped.min(self.message_ids.len());
        self.message_ids.drain(..keep);
        Ok(())
    }

    fn append(&mut self, kind: EntryKind) -> Result<()> {
        self.append_with_id(new_id(), kind)
    }

    fn append_with_id(&mut self, id: String, kind: EntryKind) -> Result<()> {
        let entry = Entry {
            id: id.clone(),
            parent_id: self.last_id.clone(),
            timestamp: now_iso(),
            kind,
        };
        if matches!(entry.kind, EntryKind::Message { .. }) {
            self.message_ids.push(id.clone());
        }
        self.last_id = Some(id);
        if self.file.is_none() {
            if !matches!(entry.kind, EntryKind::Message { .. }) {
                self.pending.push(entry);
                return Ok(());
            }
            self.open()?;
        }
        self.write(&entry)
    }

    fn open(&mut self) -> Result<()> {
        std::fs::create_dir_all(self.path.parent().unwrap_or(&self.dir))?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.file = Some(file);
        let header = self.header.clone();
        self.write_line(&header)?;
        for entry in std::mem::take(&mut self.pending) {
            self.write(&entry)?;
        }
        if self.max_saved > 0 {
            self.prune(self.max_saved);
        }
        Ok(())
    }

    fn write(&mut self, entry: &Entry) -> Result<()> {
        self.write_line(entry)
    }

    fn write_line<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        serde_json::to_writer(&mut *file, value)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    /// Keep only the newest `max_saved` sessions, counting every project folder.
    fn prune(&self, max_saved: usize) {
        let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        let Ok(projects) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for project in projects.flatten() {
            let Ok(entries) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().ends_with(".jsonl") {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                files.push((entry.path(), modified));
            }
        }
        if files.len() <= max_saved {
            return;
        }
        files.sort_by_key(|(_, modified)| *modified);
        let excess = files.len() - max_saved;
        for (path, _) in files.into_iter().take(excess) {
            if path != self.path {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(cfg: &SessionConfig) -> Self {
        Self {
            dir: cfg.dir.clone().unwrap_or_else(Config::sessions_dir),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        let mut out = Vec::new();
        for path in self.files() {
            let Ok(loaded) = load_file(&path) else {
                continue;
            };
            let Some(meta) = loaded.meta else { continue };
            let modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| chrono::DateTime::<chrono::Local>::from(t).to_rfc3339())
                .unwrap_or_else(|| meta.created_at.clone());
            out.push(SessionSummary {
                id: meta.id,
                path: path.to_string_lossy().to_string(),
                cwd: meta.cwd,
                agent_model: meta.agent_model,
                created_at: meta.created_at,
                updated_at: modified,
                title: meta.title,
                messages: loaded.messages.len(),
                bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            });
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    /// Load by id or unique id prefix.
    pub fn load(&self, id: &str) -> Result<LoadedSession> {
        let matches: Vec<PathBuf> = self
            .files()
            .into_iter()
            .filter(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem == id || stem.starts_with(id))
            })
            .collect();
        match matches.len() {
            0 => Err(HarnessError::tool(
                "E_SESSION_NOT_FOUND",
                format!("no session matching `{id}` in {}", self.dir.display()),
            )),
            1 => load_file(&matches[0]),
            _ => Err(HarnessError::tool(
                "E_SESSION_AMBIGUOUS",
                format!(
                    "`{id}` matches {} sessions; use a longer prefix",
                    matches.len()
                ),
            )),
        }
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let loaded = self.load(id)?;
        let Some(meta) = loaded.meta else {
            return Err(HarnessError::tool(
                "E_SESSION_NOT_FOUND",
                "missing metadata",
            ));
        };
        for path in self.files() {
            if path.file_stem().and_then(|s| s.to_str()) == Some(meta.id.as_str()) {
                std::fs::remove_file(path)?;
                return Ok(());
            }
        }
        Err(HarnessError::tool(
            "E_SESSION_NOT_FOUND",
            "session vanished while deleting",
        ))
    }

    /// Every session file: one project folder deep (`<dir>/<project>/<id>.jsonl`).
    fn files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(projects) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for project in projects.flatten() {
            let Ok(entries) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    out.push(path);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// on-disk types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    version: u32,
    id: String,
    timestamp: String,
    cwd: String,
    #[serde(
        rename = "parentSession",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    parent_session: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    #[serde(rename = "parentId")]
    parent_id: Option<String>,
    timestamp: String,
    #[serde(flatten)]
    kind: EntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum EntryKind {
    #[serde(rename = "message")]
    Message { message: AgentMessage },
    #[serde(rename = "model_change")]
    ModelChange {
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    #[serde(rename = "usage")]
    Usage {
        kind: String,
        provider: String,
        model: String,
        usage: PiUsage,
    },
    #[serde(rename = "compaction")]
    Compaction {
        summary: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
    },
    #[serde(rename = "custom")]
    Custom {
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    #[serde(rename = "custom_message")]
    CustomMessage {
        #[serde(rename = "customType")]
        custom_type: String,
        content: Content,
        #[serde(default)]
        display: bool,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
    },
    #[serde(rename = "context_edit")]
    ContextEdit {
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(default)]
        replacement: Option<Value>,
    },
    #[serde(rename = "label")]
    Label {
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(default)]
        label: Option<String>,
    },
    #[serde(rename = "session_info")]
    SessionInfo {
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(rename = "thinking_level_change")]
    ThinkingLevel {
        #[serde(rename = "thinkingLevel")]
        level: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role")]
enum AgentMessage {
    #[serde(rename = "user")]
    User { content: Content, timestamp: i64 },
    #[serde(rename = "assistant")]
    Assistant {
        content: Content,
        #[serde(default)]
        api: String,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        model: String,
        #[serde(default)]
        usage: PiUsage,
        #[serde(rename = "stopReason", default)]
        stop_reason: String,
        timestamp: i64,
    },
    #[serde(rename = "toolResult")]
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        content: Content,
        #[serde(rename = "isError", default)]
        is_error: bool,
        timestamp: i64,
    },
    /// pi's system messages carry prompt sections and tool declarations; we
    /// rebuild the prompt ourselves, so they are read and skipped.
    #[serde(rename = "system")]
    System {
        #[serde(default)]
        content: Content,
        timestamp: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

impl Default for Content {
    fn default() -> Self {
        Content::Text(String::new())
    }
}

impl Content {
    fn text(&self) -> String {
        match self {
            Content::Text(text) => text.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn thinking(&self) -> String {
        let Content::Blocks(blocks) = self else {
            return String::new();
        };
        blocks
            .iter()
            .filter_map(|block| match block {
                Block::Thinking { thinking } => Some(thinking.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool_calls(&self) -> Vec<ToolCall> {
        let Content::Blocks(blocks) = self else {
            return Vec::new();
        };
        blocks
            .iter()
            .filter_map(|block| match block {
                Block::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some(ToolCall {
                    id: id.clone(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: name.clone(),
                        arguments: arguments.to_string(),
                    },
                }),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum Block {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
    },
    #[serde(rename = "image")]
    Image {
        #[serde(default)]
        data: String,
        #[serde(rename = "mimeType", default)]
        mime_type: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
struct PiUsage {
    input: u64,
    output: u64,
    #[serde(rename = "cacheRead")]
    cache_read: u64,
    #[serde(rename = "cacheWrite")]
    cache_write: u64,
    #[serde(rename = "totalTokens")]
    total_tokens: u64,
    cost: PiCost,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
struct PiCost {
    input: f64,
    output: f64,
    #[serde(rename = "cacheRead")]
    cache_read: f64,
    #[serde(rename = "cacheWrite")]
    cache_write: f64,
    total: f64,
}

impl From<Usage> for PiUsage {
    fn from(usage: Usage) -> Self {
        Self {
            input: usage.prompt_tokens,
            output: usage.completion_tokens,
            cache_read: usage.cached_tokens,
            cache_write: 0,
            total_tokens: usage.context_tokens(),
            cost: PiCost {
                total: usage.cost_usd.unwrap_or(0.0),
                ..PiCost::default()
            },
        }
    }
}

impl From<PiUsage> for Usage {
    fn from(usage: PiUsage) -> Self {
        Usage {
            prompt_tokens: usage.input,
            completion_tokens: usage.output,
            total_tokens: usage.total_tokens,
            cached_tokens: usage.cache_read,
            cost_usd: (usage.cost.total > 0.0).then_some(usage.cost.total),
        }
    }
}

// ---------------------------------------------------------------------------
// loading
// ---------------------------------------------------------------------------

fn load_file(path: &Path) -> Result<LoadedSession> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut header: Option<Header> = None;
    let mut entries: Vec<Entry> = Vec::new();
    let mut truncated_tail = false;

    for line in reader.lines() {
        let Ok(line) = line else {
            truncated_tail = true;
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        if header.is_none() {
            match serde_json::from_str::<Header>(&line) {
                Ok(parsed) if parsed.kind == "session" => {
                    header = Some(parsed);
                    continue;
                }
                _ => {
                    return Err(HarnessError::tool(
                        "E_SESSION_CORRUPT",
                        format!("{} has no pi session header", path.display()),
                    ))
                }
            }
        }
        match serde_json::from_str::<Entry>(&line) {
            Ok(entry) => entries.push(entry),
            // A torn last line is expected after a hard kill; an unknown shape
            // is simply skipped.
            Err(_) => truncated_tail = true,
        }
    }

    let Some(header) = header else {
        return Err(HarnessError::tool(
            "E_SESSION_CORRUPT",
            format!("{} has no session header", path.display()),
        ));
    };

    Ok(LoadedSession {
        truncated_tail,
        ..replay(&header, &entries)
    })
}

/// Walk the active branch and rebuild what the model would see.
fn replay(header: &Header, entries: &[Entry]) -> LoadedSession {
    let chain = active_chain(entries);

    // `context_edit` applies to entries earlier in the branch, so edits are
    // collected first: the latest one for a target wins.
    let mut edits: HashMap<&str, Option<Value>> = HashMap::new();
    for entry in &chain {
        if let EntryKind::ContextEdit {
            target_id,
            replacement,
        } = &entry.kind
        {
            edits.insert(target_id.as_str(), replacement.clone());
        }
    }

    // The newest compaction replaces everything before `firstKeptEntryId`.
    let boundary = chain
        .iter()
        .rposition(|entry| matches!(entry.kind, EntryKind::Compaction { .. }));
    let mut compactions: Vec<(String, usize)> = Vec::new();
    let context: Vec<&Entry> = match boundary {
        Some(index) => {
            let EntryKind::Compaction {
                first_kept_entry_id,
                summary,
                ..
            } = &chain[index].kind
            else {
                unreachable!()
            };
            let keep_from = chain
                .iter()
                .position(|entry| &entry.id == first_kept_entry_id)
                .unwrap_or(index);
            let dropped = chain[..keep_from]
                .iter()
                .filter(|entry| matches!(entry.kind, EntryKind::Message { .. }))
                .count();
            compactions.push((summary.clone(), dropped));
            let mut window = Vec::with_capacity(chain.len() - keep_from);
            window.push(chain[index]);
            window.extend_from_slice(&chain[keep_from..index]);
            window.extend_from_slice(&chain[index + 1..]);
            window
        }
        None => chain.clone(),
    };

    let mut todos: Vec<TodoItem> = Vec::new();
    let mut usage: Option<Usage> = None;
    let mut cost_total = 0.0f64;
    let mut cost_seen = false;
    let mut agent_model: Option<String> = None;
    let mut compact_model: Option<String> = None;
    let mut title: Option<String> = None;

    // Metadata in file order: the last usage report or model change on the
    // branch is the one in force, wherever it sits relative to the window.
    for entry in &chain {
        match &entry.kind {
            EntryKind::ModelChange { provider, model_id } => {
                agent_model = Some(join_model(provider, model_id));
            }
            EntryKind::Custom {
                custom_type,
                data: Some(data),
            } => match custom_type.as_str() {
                CUSTOM_TODOS => {
                    if let Ok(items) = serde_json::from_value::<Vec<TodoItem>>(data.clone()) {
                        todos = items;
                    }
                }
                CUSTOM_COMPACT_MODEL => {
                    if let Some(name) = data.as_str() {
                        compact_model = Some(name.to_string());
                    }
                }
                _ => {}
            },
            EntryKind::SessionInfo { name } => title = name.clone(),
            EntryKind::Usage {
                usage: reported, ..
            } => merge_usage(&mut usage, &mut cost_total, &mut cost_seen, *reported),
            EntryKind::Message {
                message:
                    AgentMessage::Assistant {
                        usage: reported, ..
                    },
            } => merge_usage(&mut usage, &mut cost_total, &mut cost_seen, *reported),
            _ => {}
        }
    }

    // Messages in context order: the compaction summary comes first, then the
    // retained window, then everything written after it.
    let mut messages: Vec<ChatMessage> = Vec::new();
    for entry in &context {
        match &entry.kind {
            EntryKind::Compaction { summary, .. } => {
                messages.push(ChatMessage::user(format!(
                    "The earlier conversation was compacted. Summary://n//n{summary}"
                )));
            }
            EntryKind::BranchSummary { summary, .. } => {
                messages.push(ChatMessage::user(format!(
                    "A parallel branch was abandoned. Summary://n//n{summary}"
                )));
            }
            EntryKind::CustomMessage { content, .. } => {
                messages.push(ChatMessage::user(content.text()));
            }
            EntryKind::Message { message } => {
                if let Some(edit) = edits.get(entry.id.as_str()) {
                    match edit {
                        // `replacement: null` drops it from the context.
                        None => continue,
                        Some(value) => {
                            if let Some(chat) = edited_message(message, value) {
                                messages.push(chat);
                            }
                            continue;
                        }
                    }
                }
                if let Some(chat) = to_chat(message, &mut agent_model) {
                    messages.push(chat);
                }
            }
            _ => {}
        }
    }

    LoadedSession {
        meta: Some(SessionMeta {
            id: header.id.clone(),
            cwd: header.cwd.clone(),
            agent_model: agent_model.unwrap_or_default(),
            compact_model: compact_model.unwrap_or_default(),
            created_at: header.timestamp.clone(),
            title,
        }),
        messages,
        todos,
        compactions,
        truncated_tail: false,
        usage,
        cost_usd: cost_seen.then_some(cost_total),
    }
}

/// Fold one provider report into the running totals.
fn merge_usage(
    usage: &mut Option<Usage>,
    cost_total: &mut f64,
    cost_seen: &mut bool,
    reported: PiUsage,
) {
    if reported.total_tokens > 0 || reported.input > 0 {
        *usage = Some(Usage::from(reported));
    }
    if reported.cost.total > 0.0 {
        *cost_total += reported.cost.total;
        *cost_seen = true;
    }
}

/// Convert one stored message, collecting the model it names.
fn to_chat(message: &AgentMessage, agent_model: &mut Option<String>) -> Option<ChatMessage> {
    match message {
        AgentMessage::User { content, .. } => Some(ChatMessage::user(content.text())),
        AgentMessage::Assistant {
            content,
            provider,
            model,
            ..
        } => {
            if !provider.is_empty() && !model.is_empty() {
                agent_model.get_or_insert_with(|| join_model(provider, model));
            }
            let mut chat = ChatMessage::assistant(content.text());
            let thinking = content.thinking();
            if !thinking.is_empty() {
                chat.reasoning_content = Some(thinking);
            }
            let calls = content.tool_calls();
            if !calls.is_empty() {
                chat.tool_calls = Some(calls);
            }
            Some(chat)
        }
        AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            is_error,
            ..
        } => {
            let mut chat = ChatMessage::tool(tool_call_id, content.text());
            chat.name = Some(tool_name.clone());
            chat.content = Some(content.text());
            let _ = is_error;
            Some(chat)
        }
        AgentMessage::System { .. } => None,
    }
}

/// Apply a `context_edit` replacement: only the content changes, role and
/// metadata stay.
fn edited_message(message: &AgentMessage, replacement: &Value) -> Option<ChatMessage> {
    let content = replacement.get("content")?;
    let parsed: Content = serde_json::from_value(content.clone()).ok()?;
    match message {
        AgentMessage::User { .. } => Some(ChatMessage::user(parsed.text())),
        AgentMessage::Assistant { content, .. } => {
            let mut chat = ChatMessage::assistant(parsed.text());
            let calls = content.tool_calls();
            if !calls.is_empty() {
                chat.tool_calls = Some(calls);
            }
            Some(chat)
        }
        AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            ..
        } => {
            let mut chat = ChatMessage::tool(tool_call_id, parsed.text());
            chat.name = Some(tool_name.clone());
            chat.content = Some(parsed.text());
            Some(chat)
        }
        AgentMessage::System { .. } => None,
    }
}

/// Follow `parentId` from the newest entry back to the root.
fn active_chain(entries: &[Entry]) -> Vec<&Entry> {
    let by_id: HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let Some(mut cursor) = entries.last() else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    // Bounded so a corrupt file cannot spin forever.
    for _ in 0..=entries.len() {
        chain.push(cursor);
        let Some(parent) = cursor.parent_id.as_deref() else {
            break;
        };
        let Some(next) = by_id.get(parent) else {
            break;
        };
        cursor = next;
    }
    chain.reverse();
    chain
}

/// Last entry id plus the message ids in file order, for appending.
fn scan_chain(raw: &str) -> (Option<String>, Vec<String>) {
    let mut last = None;
    let mut message_ids = Vec::new();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str);
        if kind == Some("session") {
            continue;
        }
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            last = Some(id.to_string());
            if kind == Some("message") {
                message_ids.push(id.to_string());
            }
        }
    }
    (last, message_ids)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// pi's project folder: drop a leading separator, then turn `/`, `\` and `:`
/// into `-`.
fn project_dir(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    raw.trim_start_matches(['/', '\\'])
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect()
}

fn split_model(model: &str) -> (String, String) {
    match model.split_once('/') {
        Some((provider, name)) => (provider.to_string(), name.to_string()),
        None => (String::new(), model.to_string()),
    }
}

fn join_model(provider: &str, model: &str) -> String {
    if provider.is_empty() {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path) -> SessionConfig {
        SessionConfig {
            dir: Some(dir.to_path_buf()),
            max_saved_sessions: 5,
        }
    }

    #[test]
    fn writes_the_pi_v3_shape() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut writer =
            SessionWriter::create(&cfg, &cwd, "anthropic/claude-sonnet-4.5", "anthropic/haiku")
                .unwrap();
        writer.user("hello").unwrap();
        writer.assistant(&ChatMessage::assistant("hi")).unwrap();
        writer
            .tool_result("call_1", "read", true, "contents")
            .unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        let raw = std::fs::read_to_string(&path).unwrap();
        let mut lines = raw.lines();
        let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["version"], 3);
        assert!(header["id"]
            .as_str()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()));
        assert_eq!(header["cwd"], cwd.to_string_lossy().to_string());

        let model: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(model["type"], "model_change");
        assert_eq!(model["provider"], "anthropic");
        assert_eq!(model["modelId"], "claude-sonnet-4.5");
        assert!(model["parentId"].is_null());

        let compact: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(compact["type"], "custom");
        assert_eq!(compact["customType"], CUSTOM_COMPACT_MODEL);

        let first: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(first["type"], "message");
        assert_eq!(
            first["parentId"], compact["id"],
            "entries chain in file order"
        );
        assert!(first["id"].as_str().is_some_and(|id| id.len() == 8));
        assert_eq!(first["message"]["role"], "user");
        assert_eq!(first["message"]["content"], "hello");

        let second: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(second["parentId"], first["id"]);
        assert_eq!(second["message"]["role"], "assistant");
        assert_eq!(second["message"]["provider"], "anthropic");
        assert_eq!(second["message"]["model"], "claude-sonnet-4.5");

        assert_eq!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy(),
            project_dir(&cwd)
        );
    }

    #[test]
    fn roundtrips_messages_and_todos() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let mut writer =
            SessionWriter::create(&cfg, dir.path(), "openai/gpt-5", "openai/mini").unwrap();
        writer.user("hello").unwrap();
        let mut assistant = ChatMessage::assistant("hi there");
        assistant.reasoning_content = Some("thinking".into());
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: r#"{"path":"src/main.rs"}"#.into(),
            },
        }]);
        writer.assistant(&assistant).unwrap();
        writer
            .tool_result("call_1", "read", true, "file contents")
            .unwrap();
        writer
            .todos(&[TodoItem {
                id: "t1".into(),
                content: "do".into(),
                status: crate::tools::todo::TodoStatus::Pending,
            }])
            .unwrap();
        let id = writer.id().to_string();
        drop(writer);

        let loaded = SessionStore::new(&cfg).load(&id).unwrap();
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[0].content.as_deref(), Some("hello"));
        assert_eq!(
            loaded.messages[1].reasoning_content.as_deref(),
            Some("thinking")
        );
        let calls = loaded.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(loaded.messages[2].content.as_deref(), Some("file contents"));
        assert_eq!(loaded.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(loaded.todos.len(), 1);
        let meta = loaded.meta.unwrap();
        assert_eq!(meta.agent_model, "openai/gpt-5");
        assert_eq!(meta.compact_model, "openai/mini");
    }

    /// A session written by pi itself: thinking, text, a tool call, its result,
    /// a usage record and a model change.
    #[test]
    fn imports_a_pi_session() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let project = dir.path().join(project_dir(Path::new("/home/pi/project")));
        std::fs::create_dir_all(&project).unwrap();
        let id = "11111111-2222-3333-4444-555555555555";
        let raw = r#"{"type":"session","version":3,"id":"11111111-2222-3333-4444-555555555555","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/home/pi/project"}
{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"Hello","timestamp":1733234401000}}
{"type":"message","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"let me look"},{"type":"text","text":"Hi!"},{"type":"toolCall","id":"call_123","name":"bash","arguments":{"command":"ls"}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.001}},"stopReason":"toolUse","timestamp":1733234402000}}
{"type":"message","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":"2024-12-03T14:00:03.000Z","message":{"role":"toolResult","toolCallId":"call_123","toolName":"bash","content":[{"type":"text","text":"main.rs"}],"isError":false,"timestamp":1733234403000}}
{"type":"usage","id":"f6g7h8i9","parentId":"c3d4e5f6","timestamp":"2024-12-03T14:08:00.000Z","kind":"cache_warm","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":0,"output":0,"cacheRead":50000,"cacheWrite":0,"totalTokens":50000,"cost":{"input":0,"output":0,"cacheRead":0.015,"cacheWrite":0,"total":0.015}}}
{"type":"model_change","id":"d4e5f6g7","parentId":"f6g7h8i9","timestamp":"2024-12-03T14:05:00.000Z","provider":"openai","modelId":"gpt-4o"}
"#;
        std::fs::write(project.join(format!("{id}.jsonl")), raw).unwrap();

        let loaded = SessionStore::new(&cfg).load(&id).unwrap();
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[0].content.as_deref(), Some("Hello"));
        assert_eq!(loaded.messages[1].content.as_deref(), Some("Hi!"));
        assert_eq!(
            loaded.messages[1].reasoning_content.as_deref(),
            Some("let me look")
        );
        assert_eq!(
            loaded.messages[1].tool_calls.as_ref().unwrap()[0]
                .function
                .name,
            "bash"
        );
        assert_eq!(loaded.messages[2].content.as_deref(), Some("main.rs"));
        let usage = loaded.usage.expect("usage");
        assert_eq!(usage.context_tokens(), 50_000);
        assert!((loaded.cost_usd.unwrap() - 0.016).abs() < f64::EPSILON);
        let meta = loaded.meta.unwrap();
        assert_eq!(meta.cwd, "/home/pi/project");
        assert_eq!(meta.agent_model, "openai/gpt-4o", "latest change wins");
    }

    #[test]
    fn compaction_replaces_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let mut writer = SessionWriter::create(&cfg, dir.path(), "p/m", "p/c").unwrap();
        writer.user("first").unwrap();
        writer.assistant(&ChatMessage::assistant("one")).unwrap();
        writer.user("second").unwrap();
        writer.assistant(&ChatMessage::assistant("two")).unwrap();
        writer.compaction("both turns happened", 2).unwrap();
        writer.user("third").unwrap();
        let id = writer.id().to_string();
        drop(writer);

        let loaded = SessionStore::new(&cfg).load(&id).unwrap();
        let texts: Vec<String> = loaded
            .messages
            .iter()
            .filter_map(|m| m.content.clone())
            .collect();
        assert_eq!(texts.len(), 4, "summary + retained + new: {texts:?}");
        assert!(texts[0].contains("both turns happened"));
        assert_eq!(texts[1], "second");
        assert_eq!(texts[2], "two");
        assert_eq!(texts[3], "third");
    }

    #[test]
    fn usage_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let mut writer = SessionWriter::create(&cfg, dir.path(), "p/m", "p/c").unwrap();
        writer.user("hello").unwrap();
        let usage = Usage {
            prompt_tokens: 1_200,
            completion_tokens: 80,
            total_tokens: 1_280,
            cached_tokens: 1_100,
            cost_usd: Some(0.004),
        };
        writer.usage(&usage, Some(0.02)).unwrap();
        let id = writer.id().to_string();
        drop(writer);

        let loaded = SessionStore::new(&cfg).load(&id).unwrap();
        assert_eq!(loaded.usage.unwrap().prompt_tokens, 1_200);
        assert_eq!(loaded.cost_usd, Some(0.02));
        assert_eq!(loaded.messages.len(), 1, "usage rows are not messages");
    }

    #[test]
    fn project_dirs_follow_pi() {
        assert_eq!(project_dir(Path::new("/home/pi/proj")), "home-pi-proj");
        assert_eq!(project_dir(Path::new(r"C:\work\app")), "C--work-app");
    }
}
