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
    /// One-line identity of the entry (tool name, notice heading).
    pub title: String,
    /// Tool argument summary rendered next to the tool name.
    pub arg: String,
    /// Wrapped body. Tool bodies stay collapsed unless expanded.
    pub body: String,
    /// Body visibility for foldable entries (tool output, thinking). Clicking
    /// the row or Ctrl+O / Ctrl+T flips it.
    pub expanded: bool,
    /// Tool completion status: None = running, Some(true/false).
    pub ok: Option<bool>,
}

impl Entry {
    pub fn new(kind: EntryKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            arg: String::new(),
            body: body.into(),
            expanded: false,
            ok: None,
        }
    }

    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.arg = arg.into();
        self
    }

    /// Whether this entry can be folded at all.
    pub fn foldable(&self) -> bool {
        matches!(self.kind, EntryKind::Tool | EntryKind::Reasoning) && !self.body.trim().is_empty()
    }
}

pub struct ProcessCard {
    pub status: String,
    pub exit_code: Option<i32>,
    pub screen: String,
}
