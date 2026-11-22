use std::sync::Arc;

use crate::config::AgentConfig;
use crate::error::Result;
use crate::provider::{Backend, ChatMessage, ChatRequest, Role};
use crate::session::{COMPACTION_PREFIX, SUMMARY_SUFFIX};
use crate::token::UsageTracker;

pub const SUMMARY_SYSTEM: &str = "You compact an AI coding session. Produce a concise structured summary with exactly these five sections:\n\
## 目标 (Goal)\n## 已完成 (Done)\n## 关键约束 (Constraints)\n## 待办 (Pending)\n## 踩坑 (Pitfalls)\n\
Preserve file paths, commands, errors and decisions verbatim where they matter. No preamble.";

#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// Messages sent to the compact model for summarisation (old prefix).
    pub keep_from: usize,
    pub dropped: usize,
    pub transcript: String,
}

pub struct Compactor {
    pub threshold: f64,
    pub keep_recent_turns: usize,
    pub context_limit: u64,
    pub counter: Arc<UsageTracker>,
}

impl Compactor {
    pub fn from_config(cfg: &AgentConfig, context_limit: u64, counter: Arc<UsageTracker>) -> Self {
        Self {
            threshold: cfg.compact_threshold,
            keep_recent_turns: cfg.keep_recent_turns,
            context_limit,
            counter,
        }
    }

    pub fn trigger_tokens(&self) -> u64 {
        ((self.context_limit as f64) * self.threshold) as u64
    }

    /// Compaction is driven only by the most recent provider-reported context
    /// size. Before the first report there is no number to act on, so the
    /// harness keeps going until the API tells it otherwise.
    pub fn needs_compaction(&self) -> bool {
        self.counter.context_tokens() >= self.trigger_tokens()
    }

    /// Build a plan that always keeps the system prompt and the last N user
    /// turns verbatim. Returns `None` when there is nothing safe to compact.
    pub fn plan(&self, messages: &[ChatMessage]) -> Option<CompactionPlan> {
        let system_end = messages
            .iter()
            .take_while(|m| m.role == Role::System)
            .count();
        let user_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .map(|(i, _)| i)
            .collect();
        if user_indices.len() <= 1 {
            return None;
        }
        let keep_turns = self.keep_recent_turns.max(1).min(user_indices.len() - 1);
        let mut keep_from = user_indices[user_indices.len() - keep_turns];
        // Never start the kept suffix with orphaned tool results.
        while keep_from > system_end && messages[keep_from].role == Role::Tool {
            keep_from -= 1;
        }
        if keep_from <= system_end {
            return None;
        }
        let dropped = keep_from - system_end;
        if dropped == 0 {
            return None;
        }
        let transcript = render_transcript(&messages[system_end..keep_from]);
        Some(CompactionPlan {
            keep_from,
            dropped,
            transcript,
        })
    }

    /// Summarise the old prefix with the compact model. The agent model never
    /// sees the raw transcript of the dropped prefix.
    pub async fn summarize(
        &self,
        backend: &dyn Backend,
        compact_model: &str,
        plan: &CompactionPlan,
    ) -> Result<String> {
        let request = ChatRequest {
            model: compact_model.to_string(),
            messages: vec![
                ChatMessage::system(SUMMARY_SYSTEM),
                ChatMessage::user(format!(
                    "Summarise this session transcript.\n\n<transcript>\n{}\n</transcript>",
                    plan.transcript
                )),
            ],
            tools: Vec::new(),
            temperature: Some(0.0),
            max_tokens: Some(1_500),
            stream: false,
            model_reasoning: false,
        };
        let completion = backend.complete(request).await?;
        let mut summary = completion.content.trim().to_string();
        if summary.is_empty() {
            summary = "(empty summary)".to_string();
        }
        Ok(summary)
    }

    /// Apply a summary to the message list, preserving system messages and the
    /// recent suffix.
    pub fn apply(
        &self,
        messages: &[ChatMessage],
        plan: &CompactionPlan,
        summary: &str,
    ) -> Vec<ChatMessage> {
        let system_end = messages
            .iter()
            .take_while(|m| m.role == Role::System)
            .count();
        let mut out = messages[..system_end].to_vec();
        out.push(ChatMessage::user(format!(
            "{COMPACTION_PREFIX}{summary}{SUMMARY_SUFFIX}"
        )));
        out.extend_from_slice(&messages[plan.keep_from..]);
        out
    }
}

fn render_transcript(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        if let Some(content) = &message.content {
            out.push_str(&format!("[{role}] {content}\n"));
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                out.push_str(&format!(
                    "[tool_call {}] {} {}\n",
                    call.id, call.function.name, call.function.arguments
                ));
            }
        }
        if let Some(id) = &message.tool_call_id {
            out.push_str(&format!("[tool_result {}]\n", id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Completion;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio_util::sync::CancellationToken;

    fn conv(turns: usize) -> Vec<ChatMessage> {
        let mut msgs = vec![ChatMessage::system("SYS")];
        for i in 0..turns {
            msgs.push(ChatMessage::user(format!("user {i}")));
            msgs.push(ChatMessage::assistant(format!("assistant {i}")));
        }
        msgs
    }

    struct Fake {
        seen_model: Mutex<String>,
    }

    #[async_trait]
    impl Backend for Fake {
        async fn stream_chat(
            &self,
            request: ChatRequest,
            _sink: Option<UnboundedSender<crate::provider::StreamEvent>>,
            _cancel: CancellationToken,
        ) -> Result<Completion> {
            assert!(!request.stream, "summaries use the non-streaming path");
            *self.seen_model.lock().unwrap() = request.model;
            Ok(Completion {
                content: "## 目标\nship it\n## 已完成\nx\n## 关键约束\ny\n## 待办\nz\n## 踩坑\nw"
                    .into(),
                ..Default::default()
            })
        }
    }

    fn compactor() -> Compactor {
        Compactor {
            threshold: 0.8,
            keep_recent_turns: 2,
            context_limit: 1_000,
            counter: Arc::new(UsageTracker::new()),
        }
    }

    #[test]
    fn keeps_system_and_recent_turns() {
        let msgs = conv(5);
        let plan = compactor().plan(&msgs).expect("plan");
        assert_eq!(plan.keep_from, 7);
        assert_eq!(msgs[plan.keep_from].content.as_deref(), Some("user 3"));
        assert_eq!(plan.dropped, 6);
        let summary = "S";
        let out = compactor().apply(&msgs, &plan, summary);
        assert_eq!(out[0].role, Role::System);
        assert_eq!(
            out[1].content.as_deref(),
            Some(format!("{COMPACTION_PREFIX}S{SUMMARY_SUFFIX}").as_str())
        );
        assert_eq!(out.last().unwrap().content.as_deref(), Some("assistant 4"));
    }

    #[test]
    fn refuses_to_compact_single_turn() {
        assert!(compactor().plan(&conv(1)).is_none());
        assert!(compactor().plan(&[ChatMessage::system("s")]).is_none());
    }

    #[test]
    fn tool_result_suffix_is_not_orphaned() {
        let mut msgs = vec![ChatMessage::system("S")];
        for i in 0..3 {
            msgs.push(ChatMessage::user(format!("u{i}")));
            msgs.push(ChatMessage::assistant(format!("a{i}")));
        }
        msgs.push(ChatMessage::user("last"));
        msgs.push(ChatMessage::assistant_tool_calls(
            None,
            vec![crate::provider::ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: crate::provider::FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
        ));
        msgs.push(ChatMessage::tool("c1", "result"));
        let plan = compactor().plan(&msgs).expect("plan");
        assert!(msgs[plan.keep_from].role != Role::Tool);
    }

    #[tokio::test]
    async fn summary_uses_compact_model() {
        let fake = Fake {
            seen_model: Mutex::new(String::new()),
        };
        let msgs = conv(6);
        let plan = compactor().plan(&msgs).unwrap();
        let summary = compactor()
            .summarize(&fake, "compact-model", &plan)
            .await
            .unwrap();
        assert_eq!(*fake.seen_model.lock().unwrap(), "compact-model");
        assert!(summary.contains("## 目标"));
        assert!(plan.transcript.contains("user 0"));
        assert!(!plan.transcript.contains("user 5"));
    }

    #[test]
    fn threshold_triggers_at_80_percent_of_reported_usage() {
        let c = compactor();
        assert_eq!(c.trigger_tokens(), 800);
        assert!(!c.needs_compaction());
        c.counter.record(&crate::provider::Usage {
            prompt_tokens: 700,
            completion_tokens: 99,
            total_tokens: 799,
            cached_tokens: 0,
            cost_usd: None,
        });
        assert!(!c.needs_compaction());
        c.counter.record(&crate::provider::Usage {
            prompt_tokens: 700,
            completion_tokens: 100,
            total_tokens: 800,
            cached_tokens: 0,
            cost_usd: None,
        });
        assert!(c.needs_compaction());
        c.counter.reset_context();
        assert!(!c.needs_compaction());
    }
}
