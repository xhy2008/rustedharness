//! Session persistence: append-only JSONL transcripts that survive crashes
//! (including `kill -9`), plus resume support for every frontend.
//!
//! Every record is one JSON object on its own line. A truncated final line is
//! ignored on load, so a hard kill loses at most the in-flight record.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::SessionConfig;
use crate::error::{HarnessError, Result};
use crate::provider::ChatMessage;
use crate::tools::todo::TodoItem;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: String,
    pub agent_model: String,
    pub compact_model: String,
    pub created_at: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRecord {
    Meta {
        meta: SessionMeta,
    },
    Message {
        message: ChatMessage,
    },
    ToolResult {
        id: String,
        name: String,
        ok: bool,
        content: String,
    },
    Todo {
        items: Vec<TodoItem>,
    },
    Process {
        pid: u32,
        command: String,
        status: String,
        exit_code: Option<i32>,
    },
    Models {
        agent: String,
        compact: String,
    },
    Compaction {
        summary: String,
        dropped_messages: usize,
    },
    Notice {
        level: String,
        message: String,
    },
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

#[derive(Debug, Clone, Default)]
pub struct LoadedSession {
    pub meta: Option<SessionMeta>,
    pub messages: Vec<ChatMessage>,
    pub todos: Vec<TodoItem>,
    pub compactions: Vec<(String, usize)>,
    pub last_process: Option<(u32, String)>,
    pub records: usize,
    /// A final line that did not parse (typically a torn write after kill -9).
    pub truncated_tail: bool,
}

pub struct SessionWriter {
    id: String,
    path: PathBuf,
    file: File,
    cwd: String,
    agent_model: String,
    compact_model: String,
    created_at: String,
    messages: usize,
}

impl SessionWriter {
    pub fn create(
        cfg: &SessionConfig,
        cwd: &Path,
        agent_model: &str,
        compact_model: &str,
    ) -> Result<Self> {
        let dir = cfg.dir.clone().unwrap_or_else(|| {
            crate::config::Config::session_dir(&crate::config::Config::default())
        });
        std::fs::create_dir_all(&dir)?;
        let now = chrono::Local::now();
        let created_at = now.to_rfc3339();
        let id = format!(
            "{}-{}",
            now.format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        );
        let path = dir.join(format!("{id}.jsonl"));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let meta = SessionMeta {
            id: id.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            agent_model: agent_model.to_string(),
            compact_model: compact_model.to_string(),
            created_at: created_at.clone(),
            title: None,
        };
        let mut writer = Self {
            id,
            path,
            file,
            cwd: cwd.to_string_lossy().to_string(),
            agent_model: agent_model.to_string(),
            compact_model: compact_model.to_string(),
            created_at,
            messages: 0,
        };
        writer.append(&SessionRecord::Meta { meta })?;
        writer.prune(cfg.max_saved_sessions);
        Ok(writer)
    }

    /// Re-open an existing transcript for appending (session resume).
    pub fn resume(path: &Path, meta: SessionMeta, message_count: usize) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            id: meta.id,
            path: path.to_path_buf(),
            file,
            cwd: meta.cwd,
            agent_model: meta.agent_model,
            compact_model: meta.compact_model,
            created_at: meta.created_at,
            messages: message_count,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Number of `Message` records written in this transcript. Used to detect
    /// abandoned empty sessions (started but never prompted).
    pub fn messages(&self) -> usize {
        self.messages
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, record: &SessionRecord) -> Result<()> {
        let line = serde_json::to_string(record)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        // Flush per record: a crash must lose at most the record being written.
        self.file.flush()?;
        if matches!(record, SessionRecord::Message { .. }) {
            self.messages += 1;
        }
        Ok(())
    }

    pub fn user(&mut self, text: &str) -> Result<()> {
        self.append(&SessionRecord::Message {
            message: ChatMessage::user(text),
        })
    }

    pub fn assistant(&mut self, message: &ChatMessage) -> Result<()> {
        self.append(&SessionRecord::Message {
            message: message.clone(),
        })
    }

    pub fn tool_result(&mut self, id: &str, name: &str, ok: bool, content: &str) -> Result<()> {
        self.append(&SessionRecord::ToolResult {
            id: id.to_string(),
            name: name.to_string(),
            ok,
            content: content.to_string(),
        })
    }

    pub fn todos(&mut self, items: &[TodoItem]) -> Result<()> {
        self.append(&SessionRecord::Todo {
            items: items.to_vec(),
        })
    }

    pub fn process(
        &mut self,
        pid: u32,
        command: &str,
        status: &str,
        exit_code: Option<i32>,
    ) -> Result<()> {
        self.append(&SessionRecord::Process {
            pid,
            command: command.to_string(),
            status: status.to_string(),
            exit_code,
        })
    }

    pub fn models(&mut self, agent: &str, compact: &str) -> Result<()> {
        self.agent_model = agent.to_string();
        self.compact_model = compact.to_string();
        self.append(&SessionRecord::Models {
            agent: agent.to_string(),
            compact: compact.to_string(),
        })
    }

    pub fn compaction(&mut self, summary: &str, dropped: usize) -> Result<()> {
        self.append(&SessionRecord::Compaction {
            summary: summary.to_string(),
            dropped_messages: dropped,
        })
    }

    pub fn notice(&mut self, level: &str, message: &str) -> Result<()> {
        self.append(&SessionRecord::Notice {
            level: level.to_string(),
            message: message.to_string(),
        })
    }

    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            path: self.path.to_string_lossy().to_string(),
            cwd: self.cwd.clone(),
            agent_model: self.agent_model.clone(),
            created_at: self.created_at.clone(),
            updated_at: chrono::Local::now().to_rfc3339(),
            title: None,
            messages: self.messages,
            bytes: std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
        }
    }

    fn prune(&self, max: usize) {
        if max == 0 {
            return;
        }
        let dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    return None;
                }
                let modified = e.metadata().ok()?.modified().ok()?;
                Some((modified, path))
            })
            .collect();
        files.sort_by_key(|a| std::cmp::Reverse(a.0));
        let mut kept = 1usize; // the transcript being written right now
        for (_, path) in files {
            if path == self.path {
                continue;
            }
            // Sessions with no messages (every launch used to leave one behind)
            // are garbage regardless of the save quota — sweep them always.
            if kept < max && file_has_messages(&path) {
                kept += 1;
                continue;
            }
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Cheap probe: does this transcript contain at least one message record?
/// The serde tag is the first key of every line, so a substring scan is exact.
fn file_has_messages(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    BufReader::new(file)
        .lines()
        .any(|line| line.map(|l| l.contains(r#""kind":"message""#)).unwrap_or(false))
}

pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(cfg: &SessionConfig) -> Self {
        let dir = cfg.dir.clone().unwrap_or_else(|| {
            crate::config::Config::session_dir(&crate::config::Config::default())
        });
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(loaded) = load_file(&path) else {
                continue;
            };
            let Some(meta) = loaded.meta else { continue };
            // Sessions are created when an agent starts so they can be resumed
            // immediately; hide ones that never received a message.
            if loaded.messages.is_empty() {
                continue;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
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
                bytes: entry.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    /// Load by id or unique id prefix. Rejects anything outside the session dir.
    pub fn load(&self, id: &str) -> Result<LoadedSession> {
        let mut matches = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name == format!("{id}.jsonl") || name.starts_with(id) {
                    matches.push(entry.path());
                }
            }
        }
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
        let path = self.dir.join(format!("{}.jsonl", meta.id));
        if !path.starts_with(&self.dir) {
            return Err(HarnessError::tool("E_PATH_ESCAPE", "session path escapes"));
        }
        std::fs::remove_file(path)?;
        Ok(())
    }
}

fn load_file(path: &Path) -> Result<LoadedSession> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut loaded = LoadedSession::default();
    let mut lines = reader.lines().peekable();
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionRecord>(&line) {
            Ok(record) => {
                loaded.records += 1;
                match record {
                    SessionRecord::Meta { meta } => loaded.meta = Some(meta),
                    SessionRecord::Message { message } => loaded.messages.push(message),
                    SessionRecord::ToolResult { id, content, .. } => {
                        loaded.messages.push(ChatMessage::tool(id, content));
                    }
                    SessionRecord::Todo { items } => loaded.todos = items,
                    SessionRecord::Process { pid, status, .. } => {
                        loaded.last_process = Some((pid, status))
                    }
                    SessionRecord::Models { agent, compact } => {
                        // The header Meta holds the model chosen at creation;
                        // every `/model` switch appends a later Models record.
                        // Replay the latest one so resuming returns to the
                        // model in use when the session was last closed.
                        if let Some(meta) = &mut loaded.meta {
                            if !agent.is_empty() {
                                meta.agent_model = agent;
                            }
                            if !compact.is_empty() {
                                meta.compact_model = compact;
                            }
                        }
                    }
                    SessionRecord::Compaction {
                        summary,
                        dropped_messages,
                    } => {
                        // Replay the same replacement the live agent performed:
                        // drop the summarised prefix and insert the summary in
                        // its place, so a resumed transcript stays within the
                        // context window and keeps a valid message sequence.
                        let drop = dropped_messages.min(loaded.messages.len());
                        loaded.messages.drain(..drop);
                        loaded.messages.insert(
                            0,
                            ChatMessage::user(format!(
                                "<session_summary>\n{summary}\n</session_summary>\n\
                                 The summary above replaces earlier turns. Continue the task; use tools to verify any detail you are unsure about."
                            )),
                        );
                        loaded.compactions.push((summary, dropped_messages));
                    }
                    SessionRecord::Notice { .. } => {}
                }
            }
            Err(_) => {
                // Only tolerate a torn *final* line (hard kill during append).
                if lines.peek().is_some() {
                    tracing::warn!("skipping malformed session record in {}", path.display());
                } else {
                    loaded.truncated_tail = true;
                }
            }
        }
    }
    if loaded.meta.is_none() {
        return Err(HarnessError::tool(
            "E_SESSION_CORRUPT",
            format!("{} has no metadata record", path.display()),
        ));
    }
    Ok(loaded)
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
    fn append_and_reload_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let mut writer = SessionWriter::create(&cfg, dir.path(), "agent", "compact").unwrap();
        writer.user("hello").unwrap();
        writer
            .assistant(&ChatMessage::assistant("hi there"))
            .unwrap();
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

        let store = SessionStore::new(&cfg);
        let loaded = store.load(&id).unwrap();
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(loaded.todos.len(), 1);
        assert!(loaded.compactions.is_empty());
        assert!(!loaded.truncated_tail);
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn models_switch_is_replayed_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let mut writer = SessionWriter::create(&cfg, dir.path(), "orig-model", "orig-compact").unwrap();
        writer.user("hello").unwrap();
        // A mid-session `/model` switch appends a later Models record.
        writer.models("new-model", "new-compact").unwrap();
        let id = writer.id().to_string();

        let loaded = SessionStore::new(&cfg).load(&id).unwrap();
        let meta = loaded.meta.expect("meta");
        assert_eq!(meta.agent_model, "new-model");
        assert_eq!(meta.compact_model, "new-compact");
    }

    #[test]
    fn compaction_is_replayed_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let mut writer = SessionWriter::create(&cfg, dir.path(), "a", "c").unwrap();
        writer.user("old user").unwrap();
        writer
            .assistant(&ChatMessage::assistant("old answer"))
            .unwrap();
        writer.user("kept user").unwrap();
        writer.compaction("the summary", 2).unwrap();
        writer.assistant(&ChatMessage::assistant("after")).unwrap();
        let id = writer.id().to_string();

        let loaded = SessionStore::new(&cfg).load(&id).unwrap();
        assert_eq!(loaded.messages.len(), 3);
        assert!(loaded.messages[0]
            .content
            .as_deref()
            .unwrap()
            .contains("the summary"));
        assert_eq!(loaded.messages[1].content.as_deref(), Some("kept user"));
        assert_eq!(loaded.messages[2].content.as_deref(), Some("after"));
        assert_eq!(loaded.compactions, vec![("the summary".to_string(), 2)]);
    }

    #[test]
    fn torn_tail_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let mut writer = SessionWriter::create(&cfg, dir.path(), "a", "c").unwrap();
        writer.user("kept").unwrap();
        let id = writer.id().to_string();
        let path = writer.path().to_path_buf();
        drop(writer);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"kind\":\"message\",\"message\":{\"role\":\"user\"")
            .unwrap();
        drop(file);
        let loaded = SessionStore::new(&cfg).load(&id).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert!(loaded.truncated_tail);
    }

    #[test]
    fn pruning_keeps_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SessionConfig {
            dir: Some(dir.path().to_path_buf()),
            max_saved_sessions: 2,
        };
        for _ in 0..4 {
            let mut writer = SessionWriter::create(&cfg, dir.path(), "a", "c").unwrap();
            writer.user("x").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(SessionStore::new(&cfg).list().len(), 2);
    }

    #[test]
    fn pruning_sweeps_empty_sessions_regardless_of_quota() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let mut real = SessionWriter::create(&cfg, dir.path(), "a", "c").unwrap();
        real.user("kept").unwrap();
        let real_path = real.path().to_path_buf();
        let empty = SessionWriter::create(&cfg, dir.path(), "a", "c").unwrap();
        let empty_path = empty.path().to_path_buf();
        drop(real);
        drop(empty);
        let _ = SessionWriter::create(&cfg, dir.path(), "a", "c").unwrap();
        assert!(real_path.exists());
        assert!(!empty_path.exists());
    }

    #[test]
    fn missing_session_is_structured() {
        let dir = tempfile::tempdir().unwrap();
        let err = SessionStore::new(&config(dir.path()))
            .load("nope")
            .unwrap_err();
        assert_eq!(err.code(), "E_SESSION_NOT_FOUND");
    }
}
