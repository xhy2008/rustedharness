//! Transcript entry model shared by the event reducer and the renderer.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    Notice,
    Error,
    Compaction,
}

pub struct Entry {
    pub kind: EntryKind,
    /// One-line identity of the entry (tool name, notice text).
    pub title: String,
    /// Wrapped body. Tool bodies stay collapsed unless expanded.
    pub body: String,
    /// Per-tool expand state; also read by the global Ctrl+O toggle.
    pub expanded: bool,
    /// Tool completion status: None = running, Some(true/false).
    pub ok: Option<bool>,
}

impl Entry {
    pub fn new(kind: EntryKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            body: body.into(),
            expanded: false,
            ok: None,
        }
    }

    pub fn visible_body(&self, tools_open: bool) -> bool {
        match self.kind {
            EntryKind::Tool => self.expanded && tools_open,
            _ => true,
        }
    }
}

pub struct ProcessCard {
    pub status: String,
    pub exit_code: Option<i32>,
    pub screen: String,
}
