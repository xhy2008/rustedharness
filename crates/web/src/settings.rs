//! The editable settings surface, described to the browser.
//!
//! The TUI renders settings from its own rows because it owns the `Agent` in
//! process. A browser only holds a socket, so every value is served as data and
//! the frontend renders whatever arrives: one generic list, with controls the
//! server names (`toggle`, `choice`, `text`, `action`). Adding a setting is an
//! arm here, not new markup.

use serde::Serialize;
use serde_json::{json, Value};

use rustedharness_core::agent::{Agent, RATE_LIMIT_RETRY_INTERVAL};
use rustedharness_core::config::{
    CompactionSettings, Config, CredentialKind, CredentialSource, SearchBackend, SearchConfig,
    StripContent, ThinkingLevel,
};
use rustedharness_core::error::HarnessError;
use rustedharness_core::shell;

/// Longest-edge presets, matching the TUI's `/settings` page.
const IMAGE_PRESETS: [u32; 4] = [512, 1024, 1568, 2048];
/// Retry counts offered by `/settings`; any other configured value becomes a
/// preset of its own so the browser never hides the truth.
const RETRY_PRESETS: [u32; 5] = [5, 15, 30, 50, 100];

fn percent(value: f64) -> u32 {
    (value * 100.0).round() as u32
}

#[derive(Debug, Clone, Serialize)]
pub struct Option_ {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// One editable setting, as the browser renders it.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub key: &'static str,
    /// `session`, `general`, `compaction` or `search`; the UI groups by this.
    pub group: &'static str,
    pub label: String,
    /// The current value, pre-formatted for display.
    pub display: String,
    pub hint: String,
    /// `toggle`, `choice`, `text` or `action`.
    pub control: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<Option_>,
    /// For `action` rows: which dialog to open (`models_agent`,
    /// `models_compact`, `cwd`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<&'static str>,
    #[serde(skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub secret: bool,
    /// Present for `toggle` rows only, so the browser never has to parse the
    /// Chinese display string back into a boolean.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Item {
    fn new(
        key: &'static str,
        group: &'static str,
        label: &str,
        display: impl Into<String>,
        hint: &str,
        control: &'static str,
    ) -> Self {
        Self {
            key,
            group,
            label: label.to_string(),
            display: display.into(),
            hint: hint.to_string(),
            control,
            options: Vec::new(),
            action: None,
            disabled: false,
            secret: false,
            checked: None,
        }
    }

    fn toggle(key: &'static str, group: &'static str, label: &str, on: bool, hint: &str) -> Self {
        Self {
            checked: Some(on),
            ..Self::new(
                key,
                group,
                label,
                if on { "开" } else { "关" },
                hint,
                "toggle",
            )
        }
    }

    fn action(
        key: &'static str,
        group: &'static str,
        label: &str,
        display: impl Into<String>,
        hint: &str,
        action: &'static str,
    ) -> Self {
        Self {
            action: Some(action),
            ..Self::new(key, group, label, display, hint, "action")
        }
    }

    /// A row whose value is one of `options`. Presets that do not include the
    /// configured value get it appended, so a hand-edited `config.json` is
    /// always visible rather than silently snapping to the nearest preset.
    fn choice(
        key: &'static str,
        group: &'static str,
        label: &str,
        hint: &str,
        current: &str,
        options: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let mut values: Vec<(String, String)> = Vec::new();
        for option in options {
            if !values.iter().any(|(value, _)| *value == option.0) {
                values.push(option);
            }
        }
        if !values.iter().any(|(value, _)| value == current) {
            values.push((current.to_string(), current.to_string()));
        }
        let options = values
            .into_iter()
            .map(|(value, label)| Option_ {
                selected: value == current,
                value,
                label,
            })
            .collect();
        Self {
            options,
            ..Self::new(key, group, label, current, hint, "choice")
        }
    }

    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Values that the setters keep out of the live `Config`: the agent mirrors
/// them in atomics and only writes the file.
#[derive(Debug, Clone)]
pub struct Live {
    pub cache_miss_notices: bool,
    pub rate_limit_retries: u32,
    pub compaction: CompactionSettings,
    pub cwd: String,
    /// `set_agent_model` / `set_compact_model` only write the session, so the
    /// live choice is not in `config.json` and must come from the agent.
    pub agent_model: String,
    pub compact_model: String,
    pub separate_compact: bool,
}

impl Live {
    pub fn from_agent(agent: &Agent) -> Self {
        Self {
            cache_miss_notices: agent.show_cache_miss_notices(),
            rate_limit_retries: agent.rate_limit_retries(),
            compaction: agent.compaction_settings(),
            cwd: agent.cwd().to_string_lossy().to_string(),
            agent_model: agent.agent_model(),
            compact_model: agent.compact_model(),
            separate_compact: agent.compact_is_separate(),
        }
    }
}

/// The whole surface, in the order the TUI lists it.
pub fn describe(config: &Config, live: &Live) -> Vec<Item> {
    let compaction = live.compaction;
    let separate = live.separate_compact;
    let summary_model = if separate {
        live.compact_model.clone()
    } else {
        live.agent_model.clone()
    };
    let summary_reasons = config
        .resolve_model(&summary_model)
        .map(|spec| spec.reasoning)
        .unwrap_or(false);
    let search = &config.search;

    let mut items = vec![
        Item::action(
            "agent_model",
            "session",
            "Agent 模型",
            model_display(&live.agent_model),
            "当前回合使用的模型；切换后下一次请求生效。",
            "models_agent",
        ),
        Item::action(
            "compact_model",
            "session",
            "压缩模型",
            if separate {
                model_display(&live.compact_model)
            } else {
                "None（复用主模型）".to_string()
            },
            "None 时摘要请求复用主模型的完整前缀，从而命中 prompt 缓存。",
            "models_compact",
        ),
        Item::action(
            "cwd",
            "session",
            "工作目录",
            live.cwd.clone(),
            "所有相对路径、文件树与查看器都锚定到这里。",
            "cwd",
        ),
        Item::toggle(
            "notices",
            "general",
            "缓存未命中提示",
            live.cache_miss_notices,
            "某一回合重新计费了上一回合已缓存的 prompt token 时给出提示。",
        ),
        Item::choice(
            "retry",
            "general",
            "429 重试次数",
            &format!(
                "重试间隔固定为 {} 秒。",
                RATE_LIMIT_RETRY_INTERVAL.as_secs()
            ),
            &live.rate_limit_retries.to_string(),
            RETRY_PRESETS
                .iter()
                .map(|preset| (preset.to_string(), format!("{preset} 次"))),
        ),
    ];

    // Windows installed with Git for Windows is the only place with a choice of
    // shell, so the row is absent everywhere else.
    if shell::git_bash_available() {
        let prefer = config.shell.prefer.as_str();
        items.push(Item::choice(
            "terminal",
            "general",
            "终端",
            "切换会立即生效，并终止上一个 shell 启动的后台进程。",
            &terminal_label(prefer),
            [
                (shell::GIT_BASH.to_string(), "Git Bash".to_string()),
                (shell::POWERSHELL.to_string(), "PowerShell".to_string()),
            ],
        ));
    }

    items.push(Item::choice(
        "image",
        "general",
        "图片最长边",
        "超过该像素值的图片会等比缩小后上传；不裁剪。",
        &config.image.max_edge.to_string(),
        IMAGE_PRESETS
            .iter()
            .map(|preset| (preset.to_string(), format!("{preset} px"))),
    ));

    items.extend([
        Item::action(
            "compaction.model",
            "compaction",
            "压缩模型",
            if separate {
                format!("{}（独立）", model_display(&live.compact_model))
            } else {
                "None（复用主模型）".to_string()
            },
            "独立模型从冷缓存开始；只有价格优势大于未命中缓存的输入成本时才划算。",
            "models_compact",
        ),
        Item::choice(
            "compaction.thinking",
            "compaction",
            "思考等级",
            "摘要请求的推理强度；模型未声明 reasoning 时不发送。",
            compaction.compact_thinking.label(),
            ThinkingLevel::ALL
                .into_iter()
                .map(|level| (level.label().to_string(), level.label().to_string())),
        )
        .disabled(!summary_reasons),
        Item::choice(
            "compaction.threshold",
            "compaction",
            "压缩阈值",
            "上下文占用达到该比例时开始压缩；越低越早压缩、余量越大。",
            &percent(compaction.compact_threshold).to_string(),
            (50..=90)
                .step_by(5)
                .map(|pct| (pct.to_string(), format!("{pct}%"))),
        ),
        Item::choice(
            "compaction.keep",
            "compaction",
            "保留最近轮次",
            "压缩后这些最近的用户轮次保持原文，作为模型的即时工作上下文。",
            &compaction.keep_recent_turns.to_string(),
            (1..=8).map(|turns| (turns.to_string(), format!("{turns} 轮"))),
        ),
        Item::toggle(
            "compaction.strip",
            "compaction",
            "裁剪旧上下文",
            compaction.compact_strip_context,
            if separate {
                "只影响独立压缩模型：关闭时发送完整转录。"
            } else {
                "主模型压缩必须发送完整历史才能命中缓存，因此该选项被禁用。"
            },
        )
        .disabled(!separate),
        Item::choice(
            "compaction.send",
            "compaction",
            "完整发送最近轮次",
            "这些轮次连同工具输出与思考完整发送给压缩模型，更早的轮次只保留叙述。",
            &compaction.send_recent_turns.to_string(),
            (1..=8).map(|turns| (turns.to_string(), format!("{turns} 轮"))),
        )
        .disabled(!separate || !compaction.compact_strip_context),
        Item::choice(
            "compaction.content",
            "compaction",
            "裁剪内容",
            "从更早轮次中移除什么：工具输出，或工具输出与思考。",
            compaction.strip_content.label(),
            [
                (
                    StripContent::Nothing.label().to_string(),
                    StripContent::Nothing.label().to_string(),
                ),
                (
                    StripContent::ToolOutput.label().to_string(),
                    StripContent::ToolOutput.label().to_string(),
                ),
                (
                    StripContent::ToolOutputAndThinking.label().to_string(),
                    StripContent::ToolOutputAndThinking.label().to_string(),
                ),
            ],
        )
        .disabled(!separate || !compaction.compact_strip_context),
    ]);

    items.push(Item::choice(
        "search.backend",
        "search",
        "搜索后端",
        "web_search 只使用选中的后端，不静默回退。",
        search.backend.label(),
        SearchBackend::ALL.into_iter().map(|backend| {
            (
                backend.label().to_string(),
                format!("{} · {}", backend.label(), search_status(search, backend)),
            )
        }),
    ));

    let backend = search.backend;
    let mut credential = Item::new(
        "search.credential",
        "search",
        credential_label(backend),
        credential_state(search, backend),
        "凭据以明文保存在 config.json；留空表示清除并回退到环境变量。",
        "text",
    );
    credential.secret = matches!(
        backend.credential().map(|spec| spec.kind),
        Some(CredentialKind::Key | CredentialKind::OptionalKey)
    );
    credential.disabled = backend.credential().is_none();
    items.push(credential);

    items
}

pub fn describe_agent(agent: &Agent) -> Vec<Item> {
    describe(&agent.config(), &Live::from_agent(agent))
}

fn model_display(reference: &str) -> String {
    if reference.trim().is_empty() {
        "None".to_string()
    } else {
        reference.to_string()
    }
}

fn terminal_label(prefer: &str) -> String {
    if prefer.eq_ignore_ascii_case(shell::GIT_BASH) {
        "Git Bash".to_string()
    } else {
        "PowerShell".to_string()
    }
}

fn credential_label(backend: SearchBackend) -> &'static str {
    match backend.credential().map(|spec| spec.kind) {
        Some(CredentialKind::Url) => "实例地址",
        Some(_) => "API Key",
        None => "凭据",
    }
}

/// One line saying whether `backend` can search with the current config.
fn search_status(search: &SearchConfig, backend: SearchBackend) -> &'static str {
    match search.credential_source(backend) {
        Some(CredentialSource::Config) => "已保存凭据",
        Some(CredentialSource::Environment) => "凭据来自环境变量",
        None => match backend.credential().map(|spec| spec.kind) {
            None => "无需账号",
            Some(CredentialKind::Url) => "需要实例地址",
            Some(_) => "需要 API Key",
        },
    }
}

/// What the credential row shows for the selected backend.
fn credential_state(search: &SearchConfig, backend: SearchBackend) -> &'static str {
    match search.credential_source(backend) {
        Some(CredentialSource::Config) => "已保存",
        Some(CredentialSource::Environment) => "来自环境变量",
        None if backend
            .credential()
            .is_some_and(|spec| !spec.kind.required()) =>
        {
            "未设置 · 将抓取公开网页"
        }
        None => "未设置 · 填写后保存",
    }
}

fn string_value(value: &Value, key: &str) -> Result<String, HarnessError> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| HarnessError::Config(format!("setting {key} expects a string")))
}

fn bool_value(value: &Value, key: &str) -> Result<bool, HarnessError> {
    value
        .as_bool()
        .ok_or_else(|| HarnessError::Config(format!("setting {key} expects true or false")))
}

fn number_value(value: &Value, key: &str) -> Result<u64, HarnessError> {
    value
        .as_u64()
        .ok_or_else(|| HarnessError::Config(format!("setting {key} expects a number")))
}

/// Write one setting and return the notice the browser shows.
pub fn apply(agent: &Agent, key: &str, value: &Value) -> Result<String, HarnessError> {
    match key {
        "notices" => {
            let on = bool_value(value, key)?;
            agent.set_show_cache_miss_notices(on)?;
            Ok(format!(
                "缓存未命中提示已{}",
                if on { "开启" } else { "关闭" }
            ))
        }
        "retry" => {
            let retries = number_value(value, key)? as u32;
            agent.set_rate_limit_retries(retries)?;
            Ok(format!("429 重试已设为 {retries} 次"))
        }
        "terminal" => {
            let prefer = string_value(value, key)?;
            agent.set_shell_prefer(&prefer)?;
            Ok(format!("终端已切换为 {prefer}"))
        }
        "image" => {
            let pixels = number_value(value, key)? as u32;
            agent.set_image_max_edge(pixels)?;
            Ok(format!("图片最长边已设为 {pixels} px"))
        }
        "compaction.model" => {
            let model = string_value(value, key)?;
            agent.set_compact_model(&model)?;
            Ok(if model.trim().is_empty() {
                "压缩复用主模型".to_string()
            } else {
                format!("压缩模型已设为 {model}")
            })
        }
        "compaction.thinking" => {
            let label = string_value(value, key)?;
            let level = ThinkingLevel::ALL
                .into_iter()
                .find(|level| level.label() == label)
                .ok_or_else(|| HarnessError::Config(format!("unknown thinking level {label}")))?;
            agent.set_compact_thinking(level)?;
            Ok(format!("压缩思考等级已设为 {}", level.label()))
        }
        "compaction.threshold" => {
            let pct = number_value(value, key)? as f64;
            agent.set_compact_threshold(pct / 100.0)?;
            Ok(format!("压缩阈值已设为 {pct}%"))
        }
        "compaction.keep" => {
            let turns = number_value(value, key)? as usize;
            agent.set_keep_recent_turns(turns)?;
            Ok(format!("压缩后保留 {turns} 轮"))
        }
        "compaction.strip" => {
            let on = bool_value(value, key)?;
            agent.set_compact_strip_context(on)?;
            Ok(format!(
                "旧上下文裁剪已{}",
                if on { "开启" } else { "关闭" }
            ))
        }
        "compaction.send" => {
            let turns = number_value(value, key)? as usize;
            agent.set_send_recent_turns(turns)?;
            Ok(format!("完整发送最近 {turns} 轮"))
        }
        "compaction.content" => {
            let label = string_value(value, key)?;
            let content = [
                StripContent::Nothing,
                StripContent::ToolOutput,
                StripContent::ToolOutputAndThinking,
            ]
            .into_iter()
            .find(|content| content.label() == label)
            .ok_or_else(|| HarnessError::Config(format!("unknown strip content {label}")))?;
            agent.set_strip_content(content)?;
            Ok(format!("裁剪内容已设为 {}", content.label()))
        }
        "search.backend" => {
            let label = string_value(value, key)?;
            let backend = SearchBackend::ALL
                .into_iter()
                .find(|backend| backend.label() == label)
                .ok_or_else(|| HarnessError::Config(format!("unknown search backend {label}")))?;
            agent.set_search_backend(backend)?;
            Ok(format!("搜索后端已设为 {}", backend.label()))
        }
        "search.credential" => {
            let text = string_value(value, key)?;
            let backend = agent.search_backend();
            agent.set_search_credential(backend, &text)?;
            Ok(if text.trim().is_empty() {
                format!("已清除 {} 的凭据", backend.label())
            } else {
                format!("已保存 {} 的凭据", backend.label())
            })
        }
        other => Err(HarnessError::Config(format!("unknown setting {other}"))),
    }
}

/// The model catalog plus the two current choices, for the picker.
pub fn models(agent: &Agent) -> Value {
    let config = agent.config();
    let catalog: Vec<Value> = config
        .model_catalog()
        .into_iter()
        .map(|spec| {
            json!({
                "reference": spec.reference(),
                "id": spec.id,
                "name": spec.name,
                "provider": spec.provider,
                "context_window": spec.context_window,
                "reasoning": spec.reasoning,
            })
        })
        .collect();
    json!({
        "agent": agent.agent_model(),
        "compact": agent.compact_model(),
        "compact_is_separate": agent.compact_is_separate(),
        "catalog": catalog,
    })
}

/// Every user input a rollback can target, oldest first.
pub fn turns(agent: &Agent) -> Value {
    let turns: Vec<Value> = agent
        .user_turns()
        .into_iter()
        .map(|turn| {
            json!({
                "index": turn.index,
                "after": turn.after,
                "text": turn.text,
            })
        })
        .collect();
    json!({ "turns": turns })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustedharness_core::provider::{Backend, ChatRequest, Completion, StreamEvent};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct Stub;

    #[async_trait::async_trait]
    impl Backend for Stub {
        async fn stream_chat(
            &self,
            _request: ChatRequest,
            _sink: Option<mpsc::UnboundedSender<StreamEvent>>,
            _cancel: CancellationToken,
        ) -> rustedharness_core::error::Result<Completion> {
            Ok(Completion::default())
        }
    }

    /// An agent whose writes land in `scratch`, so the setters can persist.
    pub(crate) fn agent_in(scratch: &std::path::Path) -> Arc<Agent> {
        let config = Config {
            model: "anthropic/claude-sonnet-4.5".into(),
            compact_model: "anthropic/claude-haiku-4.5".into(),
            session: rustedharness_core::config::SessionConfig {
                dir: Some(scratch.join("sessions")),
                max_saved_sessions: 1,
            },
            source: Some(scratch.to_path_buf()),
            ..Config::default()
        };
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let (tx, _rx) = mpsc::unbounded_channel();
        runtime
            .block_on(Agent::with_backend(
                config,
                std::env::current_dir().expect("cwd"),
                tx,
                Arc::new(Stub),
            ))
            .expect("agent")
    }

    fn config() -> Config {
        Config {
            model: "anthropic/claude-sonnet-4.5".into(),
            compact_model: "anthropic/claude-haiku-4.5".into(),
            ..Config::default()
        }
    }

    fn live(config: &Config) -> Live {
        Live {
            cache_miss_notices: true,
            rate_limit_retries: 15,
            compaction: CompactionSettings::from(&config.agent),
            cwd: "/workspace".to_string(),
            agent_model: config.model.clone(),
            compact_model: config.compact_model.clone(),
            separate_compact: !config.compact_model.trim().is_empty(),
        }
    }

    fn find<'a>(items: &'a [Item], key: &str) -> &'a Item {
        items
            .iter()
            .find(|item| item.key == key)
            .unwrap_or_else(|| panic!("missing setting {key}"))
    }

    #[test]
    fn every_group_is_described_once() {
        let config = config();
        let items = describe(&config, &live(&config));
        let keys: Vec<&str> = items.iter().map(|item| item.key).collect();
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(keys.len(), unique.len(), "duplicate key in {keys:?}");
        for key in [
            "agent_model",
            "compact_model",
            "cwd",
            "notices",
            "retry",
            "image",
            "compaction.model",
            "compaction.thinking",
            "compaction.threshold",
            "compaction.keep",
            "compaction.strip",
            "compaction.send",
            "compaction.content",
            "search.backend",
            "search.credential",
        ] {
            assert!(keys.contains(&key), "{key} missing from {keys:?}");
        }
        assert_eq!(
            find(&items, "image").display,
            config.image.max_edge.to_string()
        );
    }

    #[test]
    fn trimming_is_disabled_while_compaction_reuses_the_main_model() {
        let config = config();
        let mut live = live(&config);
        live.separate_compact = false;
        live.compact_model = String::new();
        let items = describe(&config, &live);
        assert!(find(&items, "compaction.strip").disabled);
        assert!(find(&items, "compaction.send").disabled);
        assert!(find(&items, "compaction.content").disabled);

        live.separate_compact = true;
        live.compact_model = "anthropic/claude-haiku-4.5".into();
        live.compaction.compact_strip_context = false;
        let items = describe(&config, &live);
        assert!(!find(&items, "compaction.strip").disabled);
        assert!(find(&items, "compaction.send").disabled);
    }

    #[test]
    fn a_toggle_serialises_without_a_value_list() {
        let item = Item::toggle("notices", "general", "缓存未命中提示", true, "hint");
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["control"], "toggle");
        assert_eq!(value["checked"], true);
        assert!(value.get("options").is_none());
        assert!(value.get("action").is_none());
        assert!(value.get("disabled").is_none());
    }

    #[test]
    fn apply_writes_every_knob_and_describe_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_in(dir.path());
        let cases: Vec<(&str, Value)> = vec![
            ("notices", json!(false)),
            ("retry", json!(30)),
            ("image", json!(1024)),
            ("compaction.threshold", json!(65)),
            ("compaction.keep", json!(4)),
            ("compaction.send", json!(3)),
            ("compaction.strip", json!(false)),
            ("compaction.thinking", json!("medium")),
            ("compaction.content", json!("none")),
            ("search.backend", json!("tavily")),
            ("search.credential", json!("tvly-test")),
        ];
        for (key, value) in cases {
            apply(&agent, key, &value).unwrap_or_else(|err| panic!("{key}: {err}"));
        }

        let items = describe_agent(&agent);
        assert_eq!(find(&items, "notices").checked, Some(false));
        assert_eq!(find(&items, "retry").display, "30");
        assert_eq!(find(&items, "image").display, "1024");
        assert_eq!(find(&items, "compaction.threshold").display, "65");
        assert_eq!(find(&items, "compaction.keep").display, "4");
        assert_eq!(find(&items, "compaction.send").display, "3");
        assert_eq!(find(&items, "compaction.strip").checked, Some(false));
        assert_eq!(find(&items, "compaction.thinking").display, "medium");
        assert_eq!(find(&items, "compaction.content").display, "none");
        assert_eq!(find(&items, "search.backend").display, "tavily");
        assert_eq!(find(&items, "search.credential").display, "已保存");

        // The file is the contract: a fresh load must see the same values.
        let reloaded = Config::load_from_dir(dir.path()).unwrap();
        assert!(!reloaded.agent.show_cache_miss_notices);
        assert_eq!(reloaded.agent.rate_limit_retries, 30);
        assert_eq!(reloaded.image.max_edge, 1024);
        assert_eq!(reloaded.search.backend, SearchBackend::Tavily);
        assert_eq!(reloaded.search.tavily_api_key.as_deref(), Some("tvly-test"));
        assert_eq!(reloaded.agent.compact_threshold, 0.65);
        assert_eq!(reloaded.agent.keep_recent_turns, 4);
        assert_eq!(reloaded.agent.strip_content, StripContent::Nothing);
    }

    #[test]
    fn apply_rejects_unknown_keys_and_wrong_types() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_in(dir.path());
        assert!(apply(&agent, "nope", &json!(1)).is_err());
        assert!(apply(&agent, "notices", &json!("yes")).is_err());
        assert!(apply(&agent, "search.backend", &json!("altavista")).is_err());
    }

    #[test]
    fn a_value_outside_the_presets_is_still_offered_and_selected() {
        let config = config();
        let mut live = live(&config);
        live.rate_limit_retries = 7;
        live.compaction.compact_threshold = 0.63;
        let items = describe(&config, &live);

        let retry = find(&items, "retry");
        assert_eq!(retry.display, "7");
        assert!(retry
            .options
            .iter()
            .any(|option| option.value == "7" && option.selected));

        let threshold = find(&items, "compaction.threshold");
        assert_eq!(threshold.display, "63");
        let selected: Vec<&str> = threshold
            .options
            .iter()
            .filter(|option| option.selected)
            .map(|option| option.value.as_str())
            .collect();
        assert_eq!(selected, vec!["63"]);
        let values: Vec<&str> = threshold
            .options
            .iter()
            .map(|option| option.value.as_str())
            .collect();
        let mut unique = values.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(values.len(), unique.len(), "duplicate threshold presets");
    }
}
