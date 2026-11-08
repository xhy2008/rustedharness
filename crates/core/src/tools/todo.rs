use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::tools::{parse_args, schema_of, Tool, ToolContext, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    pub fn symbol(&self) -> &'static str {
        match self {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Completed => "[x]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Default)]
pub struct TodoStore {
    items: Arc<Mutex<Vec<TodoItem>>>,
}

impl TodoStore {
    pub fn new(items: Vec<TodoItem>) -> Self {
        Self {
            items: Arc::new(Mutex::new(items)),
        }
    }

    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.items.lock().unwrap().clone()
    }

    pub fn replace(&self, items: Vec<TodoItem>) -> Vec<TodoItem> {
        let mut current = self.items.lock().unwrap();
        *current = items;
        current.clone()
    }

    /// Merge by id; items without an id are appended. Existing ids not present
    /// in the update keep their state.
    pub fn merge(&self, mut items: Vec<TodoItem>) -> Vec<TodoItem> {
        let mut current = self.items.lock().unwrap();
        for item in items.drain(..) {
            if item.id.is_empty() {
                current.push(item);
            } else if let Some(existing) = current.iter_mut().find(|i| i.id == item.id) {
                *existing = item;
            } else {
                current.push(item);
            }
        }
        current.clone()
    }

    pub fn pending(&self) -> Vec<TodoItem> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.status != TodoStatus::Completed)
            .cloned()
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.items.lock().unwrap().is_empty()
    }

    /// The user-role message injected when the agent stalls with open todos.
    pub fn chase_message(&self) -> Option<String> {
        let pending = self.pending();
        if pending.is_empty() {
            return None;
        }
        let list = pending
            .iter()
            .map(|i| format!("- {} {} (id: {})", i.status.symbol(), i.content, i.id))
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!(
            "任务尚未全部完成，请继续执行：\n{list}\n\n完成一项后立即更新 todo 状态。全部完成后给出最终答复。"
        ))
    }

    pub fn render(&self) -> String {
        let items = self.snapshot();
        if items.is_empty() {
            return "(no todos)".to_string();
        }
        items
            .iter()
            .map(|i| format!("{} {} (id: {})", i.status.symbol(), i.content, i.id))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Generate a stable, human-readable id for a new todo item.
pub fn auto_id(index: usize) -> String {
    format!("t{index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: id.into(),
            content: content.into(),
            status,
        }
    }

    #[test]
    fn replace_and_merge() {
        let store = TodoStore::new(vec![item("t1", "a", TodoStatus::Pending)]);
        let after = store.replace(vec![
            item("t1", "a", TodoStatus::InProgress),
            item("t2", "b", TodoStatus::Pending),
        ]);
        assert_eq!(after.len(), 2);
        let after = store.merge(vec![item("t2", "b done", TodoStatus::Completed)]);
        assert_eq!(after.len(), 2);
        assert_eq!(after[1].content, "b done");
        assert_eq!(after[1].status, TodoStatus::Completed);
    }

    #[test]
    fn query_lists_pending_and_chase() {
        let store = TodoStore::new(vec![
            item("t1", "done", TodoStatus::Completed),
            item("t2", "open", TodoStatus::Pending),
        ]);
        assert_eq!(store.pending().len(), 1);
        let msg = store.chase_message().unwrap();
        assert!(msg.contains("任务尚未全部完成"));
        assert!(msg.contains("open"));
        assert!(!msg.contains("done (id"));
    }

    #[test]
    fn no_chase_when_empty_or_complete() {
        assert!(TodoStore::new(vec![]).chase_message().is_none());
        assert!(TodoStore::new(vec![item("t1", "a", TodoStatus::Completed)])
            .chase_message()
            .is_none());
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoArg {
    pub id: Option<String>,
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoArgs {
    /// Omit to query the current list.
    pub items: Option<Vec<TodoArg>>,
    /// Merge by id instead of replacing the whole list.
    pub merge: Option<bool>,
}

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> String {
        "Maintain the session task list. Omit items to query. With items, replaces \
         the list; merge=true updates/inserts by id. Every call returns the full list. \
         When a turn ends with pending/in_progress items, the harness injects a chase \
         message (at most 3 times per stall event)."
            .to_string()
    }

    fn schema(&self) -> Value {
        schema_of::<TodoArgs>()
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: TodoArgs = parse_args(args)?;
        let items_snapshot = match args.items {
            None => ctx.todo.snapshot(),
            Some(items) => {
                let next_id_base = ctx
                    .todo
                    .snapshot()
                    .iter()
                    .filter_map(|i| i.id.strip_prefix('t').and_then(|n| n.parse::<usize>().ok()))
                    .max()
                    .unwrap_or(0)
                    + 1;
                let mut auto = next_id_base;
                let mut converted = Vec::with_capacity(items.len());
                for item in items {
                    if item.content.trim().is_empty() {
                        return Err(crate::error::HarnessError::tool(
                            "E_INVALID_ARGUMENT",
                            "todo content must not be empty",
                        ));
                    }
                    let id = item.id.filter(|i| !i.trim().is_empty()).unwrap_or_else(|| {
                        let id = auto_id(auto);
                        auto += 1;
                        id
                    });
                    converted.push(TodoItem {
                        id,
                        content: item.content,
                        status: item.status,
                    });
                }
                if args.merge.unwrap_or(false) {
                    ctx.todo.merge(converted)
                } else {
                    ctx.todo.replace(converted)
                }
            }
        };
        ctx.emit(crate::protocol::HarnessEvent::TodoUpdated {
            items: items_snapshot.clone(),
        });
        let pending = items_snapshot
            .iter()
            .filter(|i| i.status != TodoStatus::Completed)
            .count();
        let body = format!(
            "todo list ({} items, {} unfinished):\n{}",
            items_snapshot.len(),
            pending,
            ctx.todo.render()
        );
        Ok(ToolOutput::ok(body).with_metadata(serde_json::json!({
            "items": items_snapshot,
            "pending": pending,
        })))
    }
}
