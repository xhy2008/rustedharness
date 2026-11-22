use std::sync::Arc;

use crate::config::{CompactionSettings, StripContent};
use crate::error::Result;
use crate::provider::{Backend, ChatMessage, ChatRequest, Role, ToolDefinition};
use crate::session::{COMPACTION_PREFIX, SUMMARY_SUFFIX};
use crate::token::UsageTracker;

pub const SUMMARY_SYSTEM: &str = concat!(
    "You compact an AI coding session. Produce a concise structured summary with exactly these five sections:\n",
    "## 目标 (Goal)\n## 已完成 (Done)\n## 关键约束 (Constraints)\n## 待办 (Pending)\n## 踩坑 (Pitfalls)\n",
    "Preserve file paths, commands, errors and decisions verbatim where they matter. No preamble."
);

const SUMMARY_REQUEST: &str = concat!(
    "Compact the conversation above into a handoff summary for a fresh context. ",
    "Use exactly these five sections:\n",
    "## 目标 (Goal)\n## 已完成 (Done)\n## 关键约束 (Constraints)\n## 待办 (Pending)\n## 踩坑 (Pitfalls)\n",
    "Preserve file paths, commands, errors and decisions verbatim where they matter. No preamble."
);

#[derive(Debug, Clone)]
pub struct CompactionPlan {
    pub keep_from: usize,
    pub dropped: usize,
}

/// How the summary request is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryMode {
    /// Same model, system prompt and tools as the live turn. Sending the old
    /// messages unchanged lets the provider serve the whole prefix from cache.
    Cached,
    /// A separate model starts cold, so the transcript may omit tool output
    /// and reasoning before it is sent.
    Separate,
}

pub struct Compactor {
    pub threshold: f64,
    pub keep_recent_turns: usize,
    pub send_recent_turns: usize,
    pub context_limit: u64,
    pub strip_context: bool,
    pub strip_content: StripContent,
    pub counter: Arc<UsageTracker>,
}

impl Compactor {
    pub fn new(
        settings: CompactionSettings,
        context_limit: u64,
        counter: Arc<UsageTracker>,
    ) -> Self {
        Self {
            threshold: settings.compact_threshold,
            keep_recent_turns: settings.keep_recent_turns,
            send_recent_turns: settings.send_recent_turns,
            context_limit,
            strip_context: settings.compact_strip_context,
            strip_content: settings.strip_content,
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
        Some(CompactionPlan { keep_from, dropped })
    }

    /// Summarise the old prefix. `Cached` reuses the live messages, system
    /// prompt and tools verbatim so the main model reads them from its prompt
    /// cache; `Separate` builds a smaller transcript for a cold summary model.
    pub async fn summarize(
        &self,
        backend: &dyn Backend,
        model: &str,
        messages: &[ChatMessage],
        plan: &CompactionPlan,
        tools: Vec<ToolDefinition>,
        mode: SummaryMode,
    ) -> Result<String> {
        let (messages, tools) = match mode {
            SummaryMode::Cached => {
                let mut prefix = messages[..plan.keep_from].to_vec();
                prefix.push(ChatMessage::user(SUMMARY_REQUEST));
                (prefix, tools)
            }
            SummaryMode::Separate => {
                let system_end = messages
                    .iter()
                    .take_while(|message| message.role == Role::System)
                    .count();
                let dropped = &messages[system_end..plan.keep_from];
                let elide_before = if self.strip_context {
                    recent_turn_start(dropped, self.send_recent_turns)
                } else {
                    0
                };
                let transcript = render_transcript(dropped, elide_before, self.strip_content);
                (
                    vec![
                        ChatMessage::system(SUMMARY_SYSTEM),
                        ChatMessage::user(format!(
                            "Summarise this session transcript.\n\n<transcript>\n{transcript}\n</transcript>"
                        )),
                    ],
                    Vec::new(),
                )
            }
        };
        let request = ChatRequest {
            model: model.to_string(),
            messages,
            tools,
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

/// Index of the first message in the most recent `turns` user turns.
fn recent_turn_start(messages: &[ChatMessage], turns: usize) -> usize {
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == Role::User)
        .map(|(index, _)| index)
        .collect();
    if user_indices.len() > turns {
        user_indices[user_indices.len() - turns]
    } else {
        0
    }
}

/// Render the dropped prefix for a separate summary model. Messages before
/// `elide_before` keep their prose, while `strip_content` decides whether their
/// tool output and reasoning are removed.
fn render_transcript(
    messages: &[ChatMessage],
    elide_before: usize,
    strip_content: StripContent,
) -> String {
    let mut out = String::new();
    for (index, message) in messages.iter().enumerate() {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let old = index < elide_before;
        let strip_tool = old && strip_content != StripContent::Nothing;
        let strip_thinking = old && strip_content == StripContent::ToolOutputAndThinking;
        if message.role == Role::Tool && strip_tool {
            let id = message.tool_call_id.as_deref().unwrap_or("unknown");
            out.push_str(&format!("[tool_result {id}] (output omitted)\n"));
            continue;
        }
        if let Some(reasoning) = &message.reasoning_content {
            if !strip_thinking && !reasoning.is_empty() {
                out.push_str(&format!("[reasoning] {reasoning}\n"));
            }
        }
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

    #[derive(Default)]
    struct Fake {
        seen: Mutex<Option<ChatRequest>>,
    }

    impl Fake {
        fn request(&self) -> ChatRequest {
            self.seen.lock().unwrap().take().expect("request")
        }
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
            *self.seen.lock().unwrap() = Some(request);
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
            send_recent_turns: 4,
            context_limit: 1_000,
            strip_context: true,
            strip_content: StripContent::ToolOutputAndThinking,
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
    async fn a_separate_model_gets_a_transcript() {
        let fake = Fake::default();
        let msgs = conv(6);
        let plan = compactor().plan(&msgs).unwrap();
        let summary = compactor()
            .summarize(
                &fake,
                "compact-model",
                &msgs,
                &plan,
                Vec::new(),
                SummaryMode::Separate,
            )
            .await
            .unwrap();
        let request = fake.request();
        assert_eq!(request.model, "compact-model");
        assert_eq!(request.messages[0].content.as_deref(), Some(SUMMARY_SYSTEM));
        assert!(request.tools.is_empty());
        let transcript = request.messages[1].content.as_deref().unwrap();
        assert!(transcript.contains("user 0"));
        assert!(!transcript.contains("user 5"));
        assert!(summary.contains("## 目标"));
    }

    #[tokio::test]
    async fn the_main_model_reuses_the_live_prefix_and_tools() {
        let fake = Fake::default();
        let msgs = conv(6);
        let plan = compactor().plan(&msgs).unwrap();
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        compactor()
            .summarize(
                &fake,
                "main-model",
                &msgs,
                &plan,
                tools.clone(),
                SummaryMode::Cached,
            )
            .await
            .unwrap();
        let request = fake.request();
        assert_eq!(request.model, "main-model");
        for (sent, original) in request.messages[..plan.keep_from]
            .iter()
            .zip(&msgs[..plan.keep_from])
        {
            assert_eq!(sent.role, original.role);
            assert_eq!(sent.content, original.content);
            assert_eq!(sent.reasoning_content, original.reasoning_content);
            assert_eq!(sent.tool_calls, original.tool_calls);
            assert_eq!(sent.tool_call_id, original.tool_call_id);
        }
        assert_eq!(
            request.messages.last().unwrap().content.as_deref(),
            Some(SUMMARY_REQUEST)
        );
        assert_eq!(request.tools, tools);
    }

    #[tokio::test]
    async fn a_separate_model_drops_old_tool_output_but_keeps_recent_turns() {
        let fake = Fake::default();
        let mut msgs = vec![ChatMessage::system("SYS")];
        msgs.push(ChatMessage::user("u0"));
        msgs.push(ChatMessage::assistant("a0"));
        msgs.push(ChatMessage::assistant("old answer"));
        if let Some(last) = msgs.last_mut() {
            last.reasoning_content = Some("hidden chain".into());
        }
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
        msgs.push(ChatMessage::tool("c1", "very long old tool output"));
        msgs.push(ChatMessage::user("u1"));
        msgs.push(ChatMessage::assistant("a1"));
        msgs.push(ChatMessage::user("u2"));
        msgs.push(ChatMessage::assistant("a2"));
        msgs.push(ChatMessage::assistant_tool_calls(
            None,
            vec![crate::provider::ToolCall {
                id: "c2".into(),
                kind: "function".into(),
                function: crate::provider::FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
        ));
        msgs.push(ChatMessage::tool("c2", "recent tool output"));
        msgs.push(ChatMessage::user("u3"));
        msgs.push(ChatMessage::assistant("a3"));
        msgs.push(ChatMessage::user("last"));
        msgs.push(ChatMessage::assistant("done"));
        let mut compact = compactor();
        compact.send_recent_turns = 2;
        let plan = compact.plan(&msgs).unwrap();
        compact
            .summarize(
                &fake,
                "compact-model",
                &msgs,
                &plan,
                Vec::new(),
                SummaryMode::Separate,
            )
            .await
            .unwrap();
        let request = fake.request();
        let transcript = request.messages[1].content.as_deref().unwrap();
        assert!(!transcript.contains("very long old tool output"));
        assert!(transcript.contains("output omitted"));
        assert!(!transcript.contains("hidden chain"));
        assert!(transcript.contains("recent tool output"));
        assert!(transcript.contains("[tool_call c2] read"));
        assert!(transcript.contains("[tool_call c1] read"));
    }

    #[test]
    fn strip_content_controls_what_older_turns_lose() {
        let mut answer = ChatMessage::assistant("answer");
        answer.reasoning_content = Some("hidden chain".into());
        let msgs = vec![
            answer,
            ChatMessage::tool("c1", "tool output"),
            ChatMessage::user("recent"),
            ChatMessage::assistant("done"),
        ];
        let nothing = render_transcript(&msgs, 2, StripContent::Nothing);
        assert!(nothing.contains("hidden chain"));
        assert!(nothing.contains("tool output"));

        let tools = render_transcript(&msgs, 2, StripContent::ToolOutput);
        assert!(tools.contains("hidden chain"));
        assert!(!tools.contains("tool output"));
        assert!(tools.contains("(output omitted)"));

        let both = render_transcript(&msgs, 2, StripContent::ToolOutputAndThinking);
        assert!(!both.contains("hidden chain"));
        assert!(!both.contains("tool output"));
        assert!(both.contains("(output omitted)"));
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
