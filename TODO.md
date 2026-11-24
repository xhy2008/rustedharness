# TODO / 交接文档（2026-11-08）

## 已完成：压缩模型与 /settings compaction 子菜单（2026-11-22）

1. **移除 `anotherModelForCompact`**。`compactModel` 为空（默认）即使用主模型；
   显式设置则走独立模型。`/settings` 的 compaction 子菜单可选择 `None (main model)`、
   独立模型、`compactThreshold`、`keepRecentTurns`、`compactStripContext`、
   `sendRecentTurns` 和 `stripContent`，改动立即生效并写回 `config.json`。自动压缩恒为
   开启，不再提供开关或配置项。
2. **主模型压缩必须命中缓存**：`SummaryMode::Cached` 发送与 live 相同的 system prompt、
   tool catalog 和旧消息前缀，只追加摘要指令；不再使用独立的 summary system prompt。
   `model` 为 None 时 `strip old context` 变灰且不可修改，并显示必须发送完整历史的原因。
3. **独立模型压缩可裁剪**：`compactStripContext` 为 on 时，`sendRecentTurns` 指定摘要
   transcript 中发送给独立模型的完整用户轮数；更早的轮次保留用户/助手正文，但按
   `stripContent` 删除
   `none` / `toolOutput` / `toolOutputAndThinking`。压缩后保留轮数仍由
   `keepRecentTurns` 独立控制。独立模型的 tools 为空。
4. **设置持久化**：`Config::save_setting` 支持嵌套路径，例如
   `["agent", "compactThreshold"]`，只改目标键并保留其他键。
5. **详细说明**：compaction 子菜单在列表下方显示当前选项的多行说明；`model` 为 None 时
   `strip old context` 灰显，`compactStripContext` 关闭时 `sendRecentTurns` 与
   `stripContent` 灰显。

## 已完成：Pi 可选字段自动处理（2026-11-22）

1. **删除 `reasoningContent` / `toolChoice` / `parallelToolCalls` 配置开关**。
   reasoning 非空时自动回放；空 reasoning 在 `requiresReasoningContentOnAssistantMessages`
   和 reasoning 模型下自动补空；没有 content/tool_calls 的 assistant 跳过。
   `tool_choice`、`parallel_tool_calls` 不再由 `models.json` 控制，默认请求不发送
   （Pi 只从调用方 request option 发送）。
2. **可选 compat 按 Pi 自动检测**：`supportsStore`、`supportsDeveloperRole`、
   `supportsUsageInStreaming`、`supportsFinishReason`、`maxTokensField`、
   `requiresToolResultName`、`requiresAssistantAfterToolResult`、
   `requiresNonNullAssistantContent`、`requiresToolsWithToolHistory`、
   `requiresReasoningContentOnAssistantMessages`。默认值、URL 检测和偏移逻辑与 Pi
   对应实现一致。
3. **仍不映射的高级字段**：`thinkingFormat`、`chatTemplate*`、`openRouterRouting`、
   `vercelGatewayRouting`、`zaiToolStream`、`cacheControlFormat`、
   `promptCacheKeyMode`、session-affinity headers、`thinkingTokenBudget*`、
   strict/grammar tools、mid-conversation tool/system 标志。它们被忽略，不猜测发送。

## 已完成：移除默认模型硬编码（2026-11-22）

1. **无内置 provider/model**。`Config::default()` 不再注入 OpenAI provider、
   `openai/gpt-4o` 或 `gpt-4o-mini`；`normalize()` 不再自动补模型或挑第一个
   模型。`config init` 只写空 provider 列表，并提示用户配置。
2. **未配置模型显示 None**。TUI 状态栏、`/session`、`config models`、Web 状态
   都显示 `None`；模型为空时 TUI 提示 `add a provider/model to models.json`，
   headless `run` 在启动前直接报错并给同样的提示。
3. **未知 context 不再猜 128000**。`contextWindow` 未配置时保持未知，TUI 显示
   `ctx None`；只有模型显式声明时才显示百分比。
4. **Agent 可无模型启动**。使用 `UnconfiguredModel` 后端承载空配置，首轮请求
   前返回配置错误；resume 时若 config 没有模型，则沿用会话里的模型。

## 已完成：对齐 Pi models.json（2026-11-22）

1. **compat 字段名/位置对齐**。`models.json.providers.*.compat` 现在解析 Pi 的
   `supportsDeveloperRole`、`supportsUsageInStreaming`、`requiresToolResultName`、
   `requiresNonNullAssistantContent`、`requiresToolsWithToolHistory`、
   `requiresReasoningContentOnAssistantMessages`、`maxTokensField`。删除了我们的
   `systemRole`/`streamUsage` 写法；`reasoningContent`、`toolChoice`、
   `parallelToolCalls` 也已删除，改为 Pi 的自动行为。
2. **JSONC**。`models.json`/`config.json` 现在接受 `//`、`/* ... */` 注释和尾随逗号。
3. **已知未映射的 Pi 字段**（会被忽略，但不会导致加载失败）：provider 的
   `oauth`/`authHeader`/`modelOverrides`；model 的 `api`/`baseUrl`/`input`/
   `inputLimits`/`cost`/`promptCache`/`samplingParams*`/`thinkingLevelMap`/`headers`/
   `compat`；以及上节列出的高级 compat 标志。provider/model 基本选择与
   OpenAI-compatible 请求可互通。

## 已完成：移除 provider degradation retry（2026-11-22）

1. **Chat Completions 和 Responses 都只发一次请求**。删除
   `degradation_candidates`、`degraded_without`、`is_compat_rejection` 以及
   `all compatibility fallbacks failed` 路径；网关拒绝字段时直接返回 provider 错误。
2. **可选兼容字段对齐 Pi 字段名**。`maxTokensField`、`supportsDeveloperRole`、
   `supportsUsageInStreaming`、`requiresToolResultName`、
   `requiresNonNullAssistantContent`、`requiresToolsWithToolHistory`、
   `requiresReasoningContentOnAssistantMessages` 按 Pi 的位置解析；默认值后来
   由“Pi 可选字段自动处理”改为 Pi 的自动检测。`reasoningContent`、
   `toolChoice`、`parallelToolCalls` 已删除。
3. **测试与文档**。新增默认字段省略测试；更新 README、`docs/architecture.md`，删除
   fallback 描述。

## 已完成：Hashline 改为逐行哈希 + 编辑数据完整性（2026-11-22）

1. **锚点只哈希本行内容**。删除 `(prev, current, next)` 上下文哈希，改为
   `xxh32(normalize(line))`；行号负责定位，锚点只验证该行内容仍是模型看到的那一行。
   相同内容的不同行共享锚点，哈希碰撞按设计接受，不再为了唯一性扩大宽度。
   编辑一行只失效该行锚点，插入/删除后返回位移范围的 fresh anchors。
2. **行内 CR 不再丢失**。`apply_edits` 不再对所有行执行 `replace('\r', "")`；
   `str::lines()` 已经处理 CRLF，行内 CR 原样保留并参与往返重建。
3. **严格 UTF-8 / binary 检测**。`is_binary` 对完整文件做 `from_utf8` 校验，任意非法
   字节都拒绝；`read`/`edit`/`grep` 不再用 `from_utf8_lossy` 把坏字节变成 `U+FFFD`。
4. **插入边界冲突**。`insert_after N` 与 `insert_before N + 1` 是同一个插入 slot，
   同一 slot 多次插入现在返回 `E_OVERLAP`，不再由排序决定先后。
5. **移除 `looks_decorated` 卫生检查**。`lines[]` 不再检测 `read` 前缀或 unified-diff
   的 `+/-` 标记；主路径是行号锚点，`replace_text` 只作为不熟悉锚点编辑的 fallback。
   `read`/`grep`/`edit` 的工具说明和 `lines` schema 改为强调：只把 `line#anchor`
   放进 `pos`/`end`，`lines[]` 只放新内容。
6. **提示词减负 / 缓存前缀**。删除 shell 工具描述和 system prompt 里的 `<env>`
   块；system prompt 只保留跨工具策略，不再重复 lsp/todo/shell/web 的工具描述。
   tool catalog 每个 turn 只生成一次；新增测试验证连续请求（含进入下一 turn）
   messages 前缀、tools、model、temperature、max_tokens 和 stream 均完全一致。
7. **`/resume` 前缀对齐**。session replay 为 tool result 恢复 `name`，但 provider 只在
   `requiresToolResultName` 时发送；tool_call
   arguments 统一为 canonical JSON；只有 reasoning/tool_calls 的 assistant 保持
   `content=None`；compaction summary 使用与 live 相同的 pi 文案。新增
   `resume_replays_the_live_message_shape` 测试。system prompt 和 tools 不存 session，
   跨版本/配置/MCP 变化时前缀仍会变。
8. **验证**。新增逐行哈希、CR 往返、非法 UTF-8、插入 slot、请求前缀、resume 回放测试；
   README、`docs/architecture.md`、system prompt 同步为“行号定位、锚点验内容”。

## 已完成：移除 LSP 校验机制 + 非流式 completion（2026-11-22）

1. **P0：`Backend::complete` 修复**。`Provider` 现在按 `request.stream` 分流：流式仍走
   `create_stream_byot`，非流式走 `create_byot` 并解析普通 JSON Chat Completions 响应
   （content、reasoning、tool_calls、usage、cache token）。`Compactor` 和网页抽取都恢复使用
   `complete()`，删除了 `web.rs` 里“只能走流式绕行”的 workaround。新增 mock HTTP server 测试
   `non_streaming_completion_parses_json_responses`。
2. **移除 LSP checksum 机制**。删除 GitHub release digest 查询、checksum 文件解析、digest 缓存、
   `lsp.downloads.<id>.sha256`、`--sha256`、`lsp checksum`、`ChecksumInfo`、`pin_sha256`、
   `DownloadConsent.sha256`、TUI `/lsp checksum` 与网页 consent 里的 SHA-256 行。保留：
   下载前 consent、HTTPS、host 白名单、256 MiB 上限、解压逃逸检查。
3. **低风险清理**。删除 `ProcRegistry::spawn_pipe_fallback`（pty 失败返回 `E_PTY`）；
   `ShellSpec` 不再保存 `tool_name`，按 `ShellFamily` 返回静态字符串，删除 `Box::leak`；
   删除未使用依赖 `moka`、`tokio-stream`、`tower-http`、`tracing-subscriber`、`hex`、`sha2`
   及各 crate 的未使用项。
4. **文档**。README 与 `docs/architecture.md` 删除 checksum 流程，更新 CLI/TUI 命令和信任模型。
5. **验证**。安装 `rustfmt` / `rust-clippy`（1.93.1）后跑通
   `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
   `cargo test --workspace`；core 154 passed，tui 36 passed，web 4 passed。
6. **测试归类**。`resume_timing`、`highlight_budget`、`prewarm_takes...` 移到 `benches/`
   （`cargo bench`，不再随 `cargo test` 跑）；删除 11 个只写 JSON 的 `ui-preview` 快照
   测试，`preview` 模块改名为 `ui_tests`，只保留有断言的渲染测试。当前 ignored 只剩
   2 个需要 rust-analyzer 的 live 探针和 1 个会覆盖剪贴板的测试。

## 已完成：LSP 下载进度 + 失败不再静默（2026-11-15 第二十七轮）

用户报告：下载只有一句 `downloading`，任务管理器里几乎没网络流量，疑似静默失败。

**实测结论：不是失败，是慢 + 完全没有反馈。** 用同配置客户端打真实地址测得：

```
send: 3.0s status=200 len=29690805      ← 28.3 MB，头很快就回来了
1 MB after 68.2s, 2 MB after 92.6s      ← 体只有 ~30 KB/s，一分钟才 1 MB
```

28 MB 在 30 KB/s 下要十几分钟，而原来的代码是 `response.bytes().await`：**整包读完才返回**，
中途零输出，看起来就是死掉了。

改动：
1. **流式下载**：`bytes_stream()` 逐块累积，边下边校验 256 MiB 上限（原来只能事后查）。
   每 250ms（或到 100%）发一次 `DownloadStatus`，`detail` 是 `"42% 12.3/29.4 MB"`；
   面板/状态栏里的 state detail 只放 `42%`。
2. **协议加字段**：`DownloadStatus` 新增 `percent: Option<u8>`，前端不用再解析字符串。
3. **TUI**：进度不再写进对话（一次下载几百行），而是进状态栏 —— `lsp clangd 42% 12.3/29.4 MB`
   （`App::download`）。失败时不会收到最终的 `DownloadStatus`，所以 `LspStatus` 一旦离开
   `downloading` 就把进度条清掉，不会永远停在 7%。
4. **CLI**：`lsp install` 原来传 `events: None`，所以**一句都不打印**；现在开事件通道，边下边打到
   stderr，10% 一步或至少每 5 秒一行（慢速镜像下按百分比打会几分钟才一行）。
5. **静默失败**：自动下载失败原来只写进 state detail（需要打开 /lsp 才看得到），现在额外发一条
   `Notice{level:"warning"}`，对话里直接能看到 `could not install <id> automatically: …`。

实测（同一台机器、同一地址）：

```
[lsp clangd: consented]
[lsp clangd: unverified] installing without checksum verification: could not read the release digest (HTTP 403 Forbidden)
[lsp clangd: downloading] 1% 0.3/28.3 MB
[lsp clangd: downloading] 2% 0.8/28.3 MB
```

测试：`a_download_shows_its_progress_on_the_status_line`（进度进状态栏、不增对话条目、
installed 与 LspStatus 失败都能清除进度），预览 `preview_download`。

## 已完成：ctrl+t 管 todo、/settings 回归、二进制文件不退出（2026-11-15 第二十六轮）

上一轮把 `/settings` 整个删掉是错的：用户要的是把那四行只读信息删掉，把"主模型压缩"开关放进去。
本轮补齐并修正命名：

1. **`anotherModelForCompact`**（默认 `true`）取代上一轮的 `compactWithMainModel`（语义相反）：
   `true` = 用 `compactModel` 写摘要，`false` = 用主模型。`Config::compaction_model()` 决定实际用哪个，
   **`compact_model` 不再被 normalize 覆写**，所以关掉再打开能恢复用户原来配置的小模型。
2. **`/settings` 回归**，只有一行：`another model for compact  on/off`，Enter 切换。
   `Agent::set_another_model_for_compact()` 立刻改 `ctx.compact_model`、写一条 `models` 会话记录，
   并通过 `Config::save_setting()` 落盘 —— 只改 `config.json` 里的这一个键，其余（包括我们不认识的键）
   原样保留。
3. **ctrl+t 改为开关 todo 面板**（原来只管思考块展开），`/todo` 命令取消；思考块的批量展开失去快捷键。
4. **删掉 `/clear`**。
5. **二进制文件不再退出**：提示 `binary file, not shown` 后回到所在目录的列表、光标停在该文件上
   （新增 `back_to_browser()`，与 Esc 共用同一段逻辑）。

测试：`ctrl_t_toggles_todos_and_settings_toggles_the_compact_model`（ctrl+t 开/合、Enter 切换设置、
`app.compact_model` 跟随 agent）、`the_menu_carries_no_redundant_entries`（`/clear`、`/todo` 不得回归，
`/settings` 必须存在）、`another_model_for_compact_decides_which_model_summarises`、
`saving_a_setting_edits_only_that_key`（只写一个键，其他键保留）。

顺带修了一处测试污染：`demo_app()` 的 `Config` 没有 `source`，导致设置开关的测试真的写进了
`~/.rustedharness/config.json`。现在 preview 的 `source` 指向临时目录，并已把误写进去的那个键删除。

## 已完成：TUI 精简 + 全屏文件浏览器 + 可选的主模型压缩（2026-11-15 第二十五轮）

要求（五条）：
1. 删掉 `/` 菜单里多余的 `/model <name>` 条目 —— 命令本身仍可直接输入，只是不再占菜单位；
   `/model` 的 picker 才是主路径。
2. 删掉 `ctrl+l` / `ctrl+r` / `ctrl+x` / `ctrl+g` 热键 —— 分别对应模型 picker、会话 picker、
   复制最后回复、跟随末尾/重绘，全部随菜单一起去掉；`/hotkeys` 面板同步删掉这四行，
   `ctrl+f`（文件浏览器）保留。
3. **主模型压缩开关**：新增 `compactWithMainModel`（默认 `false`）。打开后 `normalize()` 里
   `compact_model = model`，压缩走主模型 —— 主模型的 prompt cache 本来就是热的，另开一个小模型
   反而要重新暖缓存。状态栏在 `compact_model == agent_model` 时不再显示 `compact` 那一栏。
4. **`/files` 全屏 + 逐级 Esc**：文件列表从 12 行弹窗改成占满整屏的圆角框（自动滚动保持光标
   可见，底部一行 `enter open   esc back`）。查看文件时按 Esc 回到**它所在目录的列表**并且光标
   停在该文件上（新增 `open_file_browser_at(rel, focus)`），再按一次 Esc 才退出列表。
   顺带修掉列表里目录名显示成 `src//` 的双斜杠。
5. **删掉 `/settings`**：那四行（agent model / compact model / workspace / session）都是只读信息，
   与 footer、`/session`、`/cwd` 重复，整个 overlay、命令、`SETTINGS_ROWS` 常量和渲染分支一并删除。

测试：`the_removed_control_keys_do_nothing`（四组 ctrl 键不再开 overlay，ctrl+f 仍开）、
`esc_walks_back_out_of_the_file_viewer`（Esc→列表并选中该文件，再 Esc→关闭）、
`the_menu_carries_no_redundant_entries`（`/settings` 与 `/model <name>` 不得回归）、
`the_compact_column_disappears_when_it_is_the_same_model`（渲染后读 footer 断言），
以及预览 `preview_files_fullscreen`。

## 已完成：L0 索引挪到工作区的 .rustedharness/（2026-11-15 第二十四轮）

要求：tree-sitter 的 L0 index 从配置目录挪到工作目录下的 `.rustedharness/`。

- `intel.rs` 原来用 `Config::data_dir()/index/<xxh32(cwd)>/index.sqlite`，现在直接
  `<workspace>/.rustedharness/index.sqlite`（`INDEX_DIR` 常量 + `CodeIndex::db_path()`）。
  索引描述的就是这个工作区，跟着它一起失效、一起删除，不再留在用户目录里无限堆积。
- `.rustedharness/.gitignore` 写入 `*`：不改用户的 `.gitignore`，目录从内部忽略自己，
  `git status` 干净（`git check-ignore` 验证过）。
- 扫描器的跳过列表加入 `INDEX_DIR`，避免把自己的 sqlite/-wal 当成源码走一遍。
- 写不进去的工作区（只读/权限）仍然回退到 `open_in_memory()`。
- 删掉了 `intel.rs` 里对 `Config` 的依赖。
- 实测：`lsp list` 之后仓库里出现 `.rustedharness/{index.sqlite,.gitignore}`，
  `git status` 无输出。新增测试 `the_index_lives_in_the_workspace`
  （断言 db 路径、`.gitignore` 内容、且 `.rustedharness` 自身不被索引）。

注：旧的 `~/.rustedharness/index/` 目录不再被读取，可以直接删除（本轮不做迁移）。

## 已完成：lsp 工具支持 install（需用户同意）与 restart（2026-11-15 第二十三轮）

要求：在 agent 的 lsp 工具里增加安装（需要用户同意）和重启 LSP 的功能。

- `LspOp` 新增 `Install` / `Restart`，`is_maintenance()` 标记它们"作用于服务器而不是文件"。
  `LspQuery` 新增 `server: Option<String>`；`answer()` 在这两个 op 上先分派，完全不走文件/位置校验。
- **install**：注册表查 id（未知则报错并列出全部合法 id）→ 已在 PATH/缓存则直接返回路径 →
  `download_server()` 首次会发 `DownloadConsent` 并返回 `E_LSP_CONSENT_REQUIRED` →
  `await_consent()` 每 150ms 轮询 `consent`，**最多等 120s**（`CONSENT_WAIT`）→ 同意后重新下载 →
  `ensure_client` 立刻拉起 → 返回 `installed <id> <version> at <path>; the server is running`。
  拒绝或超时返回 `E_LSP_CONSENT_DECLINED`；装好但起不来返回 `E_LSP_SPAWN` 并带上 degraded 原因。
- **restart**：先检查是否已安装（否则 `E_LSP_NOT_INSTALLED`，提示改用 install）→ 从 `clients`
  里摘掉旧的并 `shutdown()` → `ensure_client` 重新 spawn → 返回
  `restarted rust-analyzer (pid 8508, replaced the running server); it is warming up`。
- 关闭 `lsp.enabled` 时两者都返回 `E_LSP_DISABLED`；`intel::query` 与 `execute_l1` 各有一条
  防御分支，保证维护 op 不会走进查询/索引路径。
- 工具侧：`path` 变为可选、新增 `server`，按 op 校验（维护 op 要 server，查询 op 要 path），
  description 与 schema 都写清楚了两个新 op 和同意流程。

测试：`server_management_ops_are_marked_as_such`、`install_refuses_an_unknown_server_with_the_known_ids`、
`install_waits_for_consent_and_reports_a_refusal`（用事件通道断言"确实卡在同意上"：
`!task.is_finished()`，然后拒绝 → `E_LSP_CONSENT_DECLINED`）、
`the_schema_lists_server_management`（schema 里必须有 install/restart/server）。
live 探针新增 `restart_replaces_the_server_and_it_answers_again`：实测
`restarted rust-analyzer (pid 8508, replaced the running server); it is warming up`，
重启后重新给出 precise 答案。

## 已完成：状态栏显示 LSP 与预热状态（2026-11-15 第二十二轮）

要求：底部状态栏显示运行中的 lsp 名称和预热状态。

**先补齐数据**：原来 `ensure_client` 一 spawn 完就写 `running`，`is_warming()`
只体现在 `LspAnswer::warmup_hint` 里，状态栏根本看不到"在预热"。改动：
- `mark_started()` 在 spawn 后按 `client.is_warming()` 写 `warming` 或 `running`；
- `new()` 里新增 400ms 的 `settle_warmups` 观察任务（`Weak<LspHub>`，锁 + 比较 Instant，
  无 I/O），`$/progress` 全部结束后把 `warming` 翻成 `running` 并 emit `LspStatus`，
  所以状态栏会自己从 warm 变回正常，不用轮询 UI。

**状态栏**：`App::lsp_summary()` 汇总 `running` / `warming` / `degraded` 的行：
- 文案 `lsp rust`（就绪）、`lsp lua warm`（预热中，黄色）、`lsp c down`（失败，红色），
  前缀是语言名而不是 id —— `typescript-language-server-js` 这种 id 会把整行挤爆，
  完整 id/版本/原因在 `/lsp` 面板里；
- 优先级按"需要注意的程度"排序（down > warm > running），其余用 `+N` 汇总；
- 没有任何在跑的服务器时不显示，避免占位噪声。

**牺牲顺序**也调整了：原来是 …→cost→tokens→compact，现在把 `lsp` 放到倒数第二位
（只在整行彻底放不下时才去掉），100 列下会先牺牲 `in/out` 与 compact 模型。实测 100 列：
`working.  1 queued|E:/rustedharness|claude-sonnet-4.5|ctx 12.4%/200.0k|lsp lua warm +1`。

测试：`the_status_line_summarises_live_servers`（app）、
`the_status_line_reports_the_language_server` + `the_language_server_survives_narrow_status_lines`
（preview，直接读渲染后最后一行的文本断言），live 探针断言预热后 state 变成 `running`。

## 已完成：LSP 检索速度 + pdb 瘦身（2026-11-15 第二十一轮）

### 实测数据（临时 cargo 工程 + `tests/lsp_live.rs`）

| 阶段 | 耗时 |
| --- | --- |
| 冷启动到第一个 precise 答案 | **8.9 s**（rust-analyzer 载入 crate） |
| 预热后单次 definition | **1.8 ms** 中位 / 2.3 ms p90 |
| 启动预热后再问 | **2.9 ms** |

结论：**慢的只有冷启动**，热查询已经是毫秒级。所以优化点全在"别让第一次调用等索引"。

### 改了三处

1. **启动预热**（`LspHub::prewarm`，`new()` 里 500ms 后后台跑）：用 `CodeIndex::language_counts()`
   （新方法，`SELECT language, COUNT(*) FROM files GROUP BY language`）取工作区占比最高的语言，
   只有 `resolve_command` 已找到可执行文件时才 spawn —— **预热永不下载**。实测把 8.9s 的
   冷启动移到后台：预热完再查询 2.9ms 就是 precise。
2. **`$/progress` 跟踪**（原来完全忽略）：`window/workDoneProgress/create` + `$/progress`
   的 begin/end 维护 `token -> title`，`is_warming()` 现在等于"仍在 2s 窗口内 或 还有未结束的
   progress"，上限 180s（`WARMUP_CEILING`，防止永不结束的 token 把我们永久钉在 L0）。
   提示因此变成 `rust-analyzer is still warming up (Indexing); showing name-based results`
   —— 之前是在索引期间直接发查询，服务器返回 null，我们把它当成 "no matches" 报给模型，
   是**错误答案**。
3. **空结果重试**：`empty_lookup()` 只对 definition/typeDefinition/implementation/references
   生效，刚 didOpen 的文件拿到空结果时重试 2 次（150/300ms），不再把"还没分析完"报成"没有匹配"。
   hover/diagnostics 的空中文是合法的，不重试（预热后中位 1.8ms 说明没有误触发）。

### pdb 瘦身

`Cargo.toml` 新增 `[profile.dev] debug = "line-tables-only"`，**增量编译保留**。重建后：

| 产物 | 之前 | 之后 |
| --- | --- | --- |
| `rustedharness.pdb` | 209 MB | **99 MB** |
| `rustedharness_tui-*.pdb` | 166–186 MB | **77 MB** |
| `web-*.pdb` | — | 20 MB |

代价：`target/debug` 里现在 4.1 GB 是本次构建、**13.9 GB 是旧 profile 的残留**（旧 pdb +
失效的 incremental），需要一次性 `cargo clean` 才会真正腾出来；清掉之后目录约 4 GB，
而且以后不会再长回 15 GB。

## 已完成：LSP 起不来时说明原因 + 端到端探针（2026-11-15 第二十轮）

问题起因：问"lsp 能否被正确调用"。实测本机 `rust-analyzer` 是 rustup 的**代理 shim**，
`~/.cargo/bin/rust-analyzer.exe` 存在但真跑起来立刻退出：

```
error: Unknown binary 'rust-analyzer.exe' in official toolchain 'stable-x86_64-pc-windows-msvc'.
```

`resolve_command` 只判断文件存在，于是 `lsp list` 显示 `available`；真正 spawn 时才失败，
状态变成 `degraded`，工具静默回退到 L0 —— **用户看不到任何原因**，只写进了 `tracing::debug`。

- `LspClient` 现在保留子进程 stderr 的最后 8 行（`STDERR_TAIL`），`initialize` 失败时错误里
  带上 `(server said: ...)`；因为 stderr 读取是独立任务，失败路径先等 120ms 再读。
- `degraded` 状态会把原因带进 L0 回答的 `warmup_hint`：`rust-analyzer is unavailable
  (E_LSP_INIT: ... (server said: error: Unknown binary ...)); name-based results`。
  原来只有 `warming` 才有提示，`degraded` 静默。
- 新增端到端探针 `crates/core/tests/lsp_live.rs`（`#[ignore]`）：临时建一个 cargo 工程，
  用 `LspHub::answer(definition)` 真跑一遍 spawn → initialize → didOpen → definition，
  要求拿到 `lsp-precise` 且定位到 `src/lib.rs:1`。服务器还在加载时会返回 `no matches`，
  探针会重试到 120s。

修复环境（`rustup component add rust-analyzer`）后实测：

```
[  84ms] name-based  (still warming up)
[ 2325ms] lsp-precise (no matches, 仍在加载)
[ 7208ms] lsp-precise answer: src/lib.rs:1:8
```

结论：链路本身正确（spawn/initialize/sync/request/定位/格式化都通），此前的"不能用"是
rustup 组件没装 + 错误不可见两个问题叠加。

## 已完成：lsp 服务器 id 的自动补全（2026-11-15 第十九轮）

要求：给 lsp 的 id 加自动补全。

**TUI**：斜杠补全原来只补命令名（输入里有空格就不补）。现在分两级：
- 命令名：`/` 开头、无空格时按首 token 过滤，行为不变；
- 服务器 id：`/lsp install|update|checksum` 之后补 `lsp::registry()` 的 id。
  - `lsp_id_query()` 把 `/lsp install ru` 拆成 `("/lsp install ", "ru")`；
    只写 `/lsp install`（还没敲空格）也直接给全量列表；
  - 列表项显示 `display | installed <ver>` / `downloads from GitHub` / `install manually`，
    已装版本来自 `self.lsp` 里的 `ServerStatus`；
  - `Overlay::Slash` 新增 `prefix` 字段区分两级：选中 id 后把整行补成
    `/lsp install rust-analyzer` 并关闭菜单（`slash_dismissed`），下一次按键再重新过滤；
  - 面板标题按级别显示 `commands` / `servers`。
- 新增测试：`lsp_id_query_splits_verb_and_partial`、`server_ids_complete_after_the_verb`、
  `the_verb_alone_lists_every_server`、`confirming_an_id_fills_the_command_line`、
  `ids_do_not_leak_into_other_commands`，以及预览 `preview_lsp_id_completion`
  （`--ignored`，写 `target/ui-preview-lsp-ids.json`，实机渲染确认 16 个 id 分页与对齐）。
- `demo_app()` 改为 `pub(crate)`，`App::refresh_slash()` 改为 `pub(crate)` 供预览/测试驱动。

**CLI**：`lsp install|update|checksum` 的 `<SERVER>` 用
`PossibleValuesParser::new(lsp_server_ids())`，拼错直接列出全部合法 id，`--help` 里也带上。

## 已完成：TUI 的 LSP 命令全部收进 /lsp 子命令（2026-11-15 第十八轮）

要求：把 `/install` 并进 `/lsp install`，其他 LSP 相关命令也合并成 `lsp` 子命令。

- 删除顶层 `/install`、`/checksum`，改为 `/lsp install|update|checksum <id>`；`/lsp` 仍然是状态面板。
- `App::lsp_command` 解析子命令；`install` 与 `update` 共用一条路径（`install_lsp(.., update)`），
  `checksum` 仍然只读。未知子命令给出可用列表。
- `/lsp` 面板加一列"INSTALLED"（`ServerStatus::version`），底部提示改为三条新命令；
  原来版本列宽 10 会让 `2026-11-12` 与状态列粘在一起，改成 12。
- 启动检查更新的通知不再写死 CLI 命令：core 只发 `<旧> -> <新>`，CLI 追加
  `rustedharness lsp update <id>`，TUI 追加 `/lsp update <id>`。
- 新增测试：`lsp_commands_live_under_lsp`、`slash_completion_sees_the_lsp_subcommands`
  （tui crate 原来没有测试模块），以及预览用例 `preview_lsp_panel`（`--ignored`，写
  `target/ui-preview-lsp.json`），实机渲染确认三列对齐。
- README 补一张 TUI 命令表。

## 已完成：pi 上下文互通，且不落盘 pi 的噪声（2026-11-15 第十七轮）

要求：**上下文与 pi 互通，但不保存 pi 里那些多余的东西（api 错误、模型切换历史、`/settings` 等命令输入）。**

pi 会话里到底有什么（实测本机两个真实会话，其中一个 2924 行 / 1933 条消息）：
- `message:assistant` 1444 条，其中 **973 条是 `content: []` + `stopReason: "error"` + `errorMessage`
  的 API 失败记录**（DeepSeek 的 reasoning_effort 400）；
- `model_change` 10 条（每切一次模型留一条）；
- `context_edit` 981 条（pi 用来裁剪工具输出，`replacement: null` 表示移出上下文）；
- 少量 `usage` / `compaction` / `thinking_level_change`。

本轮改动（`crates/core/src/session.rs`）：

1. **磁盘布局对齐 pi**：目录 `--<cwd 编码>--`（原来少了首尾 `--`，等于和 pi 分居两个目录），
   文件名 `<ISO 时间戳>_<id>.jsonl`（原来只有 `<id>.jsonl`）。`session.dir` 指到
   `~/.pi/agent/sessions` 后两边读写同一份文件；新增 `session_id()` 让 id 取文件名最后一个
   `_` 之后的部分，所以 pi 文件名也能用 id/前缀加载。
2. **读时过滤噪声**：
   - 内容为空的消息整条丢弃（覆盖 pi 的 API 错误记录）；
   - 纯命令输入（`/^[a-z][a-z0-9_-]*(\s|$)/`，如 `/settings`、`/model x`、`/compact 说明`）
     丢弃，但 `/usr/bin/env ...` 这类正文保留；
   - 新增 `bashExecution` / `custom` 消息角色，并按 pi 的 `bashExecutionToText` 原样转成
     user 消息；未知角色走 `#[serde(other)]` 丢弃 —— 原来未知角色会让整条 entry 解析失败，
     被误判成 "尾部被截断"。
3. **写时不产生噪声**：`user()` 拒绝空文本与命令输入，`assistant()` 拒绝空内容；
   模型不再写 `model_change`，而是一条 `rustedharness.models` `custom` 快照 —— **切换模型时
   原地重写这一条**（temp + rename + 重开 append 句柄），所以文件里永远只有一条当前状态，
   不会累积切换历史。
4. **修复 resume 路径**：`Agent::resume` 原来拼 `<sessions>/<id>.jsonl`（旧扁平布局），
   对 pi 文件和新布局都会 `os error 2`；改为使用 `LoadedSession::path`。
5. **compaction / branch_summary 摘要改用 pi 的原文措辞**（`The conversation history before
   this point was compacted into the following summary:` + `<summary>` 包裹），并修掉两处
   `//n` 字面量（应为 `\n`）。
6. `sessions show` 在 pi 会话没有 compact model 时不再打印空的 `→ `。

**互通实测**（双向，不是只看单测）：
- 读：`session.dir` 指向 pi 的 sessions 目录，`sessions list` 列出两个真实 pi 会话；
  `sessions show 01a1b9c6` 读出 11 user / 239 assistant / 234 tool 条（973 条 API 错误全被过滤）。
- 写：用 `run` 生成真实会话（含 thinking + toolCall + toolResult + usage），再用 pi 自己的
  `SessionManager.open()` + `buildSessionContext()` 解析该文件 —— 8 条 entry、模型解析为
  `{provider: intern, modelId: qwen3.8-27b}`、4 条消息全部还原。
- resume：`run --resume <id>` 追加写回同一文件，pi 再解析得到 6 条消息；文件里 `models`
  快照始终只有 1 条。

测试（core session 12 条，新增 6 条）：`session_files_are_named_like_pi`、
`pi_noise_is_skipped_on_load`、`command_inputs_and_empty_messages_are_never_written`、
`pi_only_message_roles_survive_the_round_trip`、`model_switches_replace_the_single_models_entry`、
`command_detection_matches_chat_commands_only`。

## 已完成：会话存储严格换成 pi v3 格式（2026-11-15 第十六轮）

要求：照搬 pi 的上下文存储、能无损迁移 pi 会话、不要旧格式兼容。按
`pi/packages/coding-agent/docs/session-format.md` 与 `session-manager.ts` / `ai/src/types.ts`
的类型定义实现。

**磁盘布局**：`<sessions>/<project-id>/<session-id>.jsonl`
- project-id = cwd 去掉开头的分隔符，再把 `/` `\` `:` 换成 `-`（`C:\work\app` →
  `C--work-app`），与 pi 完全一致；
- session-id = UUID v4；文件名即 id。

**文件内容**：首行 `{"type":"session","version":3,"id","timestamp","cwd"}`，其后每行一个
条目，全部带 `id`（8 位 hex）/`parentId`/`timestamp`（ISO），`type` 区分：
`message`（role 为 `user`/`assistant`/`toolResult`/`system`，内容块是
`text`/`thinking`/`toolCall`/`image`）、`model_change`、`usage`、`compaction`、
`custom`、`custom_message`、`branch_summary`、`context_edit`、`label`、`session_info`、
`thinking_level_change`。

**读取**（`session.rs::replay`）：从最后一条沿 `parentId` 回溯得到活跃分支 → 取最新的
`compaction`，窗口 = [compaction] + `firstKeptEntryId..compaction` + 之后全部 → 应用
`context_edit`（`replacement:null` 表示移出上下文）→ 转成我们的 `ChatMessage`。
`model_change`/`session_info`/`usage` 按文件顺序取最后一个。

**我们自己的东西**放在 `custom` 条目里（pi 会保留但不进模型上下文）：
`customType` = `rustedharness.todos` / `rustedharness.compact_model`。其余一律是 pi 原生
条目，所以两边可以互相打开。

**不再兼容旧格式**：旧版扁平的 `<sessions>/<id>.jsonl`（`kind` 字段）不再被读取，也不会
出现在 `sessions list` 里（新格式在子目录下）。

**其他**：`Agent::build` 遇到空的 `agent_model`/`compact_model`（pi 会话可能没有）会回退到
配置值；`SessionWriter::resume` 从文件里重建 `last_id` 与 message id 序列，追加时链条不断。

测试（core 6 条）：写出的头部/条目形状、消息+todo 往返、直接导入 pi 样例文件（thinking +
toolCall + usage + model_change）、compaction 窗口、usage 往返、project 目录命名。

## 已完成：恢复会话的统计归零 + 启动慢的真凶（2026-11-15 第十五轮）

**统计归零**：footer 的数字全部来自 provider 的 `usage` 回报，恢复会话时没有任何一次
请求，于是 `UsageTracker` 是空的 -> ctx/in/out/cache/cost 全 0。

- `usage` 条目（pi `UsageEntry`）：每次拿到 usage 就追加一行（约 200 字节/轮），
  加载时保留最后一条。`UsageTracker::restore()` 在 `Agent::build` 里回填，`snapshot()`
  带上 `usage` + `cost_usd`，TUI 的 `populate` 据此填 footer。
- `Usage::cache_hit_pct()/context_tokens()` 提到 provider 层，tracker 与快照共用。

**启动慢的真凶是 shell 探测，不是会话文件**（release，0.8 MiB / 2400 条消息）：

| 阶段 | 耗时 |
| --- | --- |
| `SessionStore::load` | 10.3 ms |
| `LspHub::new`（含 CodeIndex 打开 sqlite） | 70.4 ms |
| `ShellSpec::detect` | 0.7 ms（缓存前 2760 ms） |
| `Agent::resume` 合计 | 87.3 ms |

`ShellSpec::detect` 要启动一次 PowerShell 读版本号，本机冷启动 3.3 s（杀毒 + 模块加载）。
现在把结果缓存到 `<config_dir>/shell.json`，key = prefer + OS + 可执行文件的 (size, mtime)：
换 shell 或升级 PS 自动失效。debug 下 resume 从 2.94 s 降到 143 ms。

**二进制存储：不做**（数据见 `storage_benchmark`）：release 下 0.3 MiB 会话 load 3.86 ms，
其中 JSON 解析 3.22 ms；load 只占 resume 的 12%，即使解码免费也只省 ~9%。而且
条目是 internally-tagged 枚举 + 大量 `skip_serializing_if`，bincode 直接反序列化
会 `InvalidTagEncoding` / `DeserializeAnyNotSupported`（实测），要做二进制得另立一套 schema，
丢掉 JSONL 的追加写、截断恢复和可 grep。下一个值得动的是那 70 ms 的 `CodeIndex::new`
（可以懒加载）。

## 已完成：代码高亮（正文围栏 + /view）（2026-11-15 第十四轮）

- `tui-markdown` 打开默认特性 `highlight-code`（syntect + onig），主题用
  `BuiltinCodeTheme::Base16OceanDark`：色相和我们调色板一致（蓝/绿/紫/橙）。
- `/view` 的文件查看器复用同一条管线：`markdown::highlight(source, language)` 把文件
  包进五连波浪线的围栏再交给库，所以文件和正文里的同语言片段颜色永远一致；行数保持
  1:1（去掉末尾换行再包），查看器的行号不跳。
- 语言由扩展名映射（`markdown::language_for`），认不出就退回代码块的普通样式。
- **高亮必须异步**：debug 构建下 syntect 约 1.3ms/行（100 行 0.4s、1600 行 2.4s，
  其中 0.3s 是一次性的 syntax set 初始化）。所以打开文件先显示纯文本
  （`markdown::plain`），`spawn_blocking` 里高亮完成后再通过 `OpResult::Highlighted`
  换进来；超过 96 KB 的文件不高亮。结果只在该文件仍打开时生效。
- 副作用：库对代码块**不输出背景色**（`as_24_bit_terminal_escaped(parts, false)`），
  所以代码块不再有整块底色，只靠 token 颜色区分。要恢复底色得自己按围栏切分 markdown
  —— 与"别再自己写渲染器"冲突，暂时接受。

测试：`highlighting_keeps_one_row_per_source_line`、`a_finished_highlight_only_lands_on_its_own_file`。


## 已完成：markdown 渲染改用 tui-markdown（2026-11-15 第十三轮）

自己写的那套（标题/列表/引用/围栏/行内样式）只覆盖了模型实际会写的语法的一小部分，
表格直接躺平。改成依赖 `tui-markdown` 0.3（pulldown-cmark，MIT/Apache）：

- `crates/tui/src/markdown.rs` 只做三件事：把库的 `StyleSheet` 映射到我们的调色板；
  把 `Line.style` 合并进 span 并补上默认前景色（库把元素样式放在 line 上）；按转录宽度
  折行。
- 表格、任务列表、删除线、脚注、GFM alert 全部由库处理，`Options::table_width` 负责
  让表格收缩进面板宽度。
- **禁用 `highlight-code`**（默认特性）：会拉进 syntect，编译时间翻倍，而我们的代码块
  只需要一块底色。没有高亮时库走 `StyleSheet::code()`，正好是我们要的。
- 自写换行改成按词断行（`wrap_words`）：中文没有空格时按字符断，长 URL 也强制断；
  代码块整行补底色到面板宽度，保持"一整块"的观感。
- 删掉 `render.rs` 里的 `heading/is_rule/bullet/inline_spans/text_flow` 与手写表格实验。

测试：`markdown_tables_render_as_a_grid`（边框、表头、正文、不残留 `| ---`、不超宽）、
`a_table_too_wide_for_the_pane_is_wrapped`。


## 已完成：输入/开面板不再整屏重绘（2026-11-15 第十二轮）

现象：每敲一个字、每次打开命令面板，整个窗口闪一下。

两个原因：

1. `overlay_key()` 把**条目数**也编进了身份，而 picker 的过滤结果随每个按键变化 →
   "面板变了" → `terminal.clear()`。打字就是不断清屏。
2. 面板开合本身也走清屏这条路。

而清屏其实没必要：ratatui 每帧都从空白缓冲重画（`swap_buffers()` 会 reset 下一块缓冲），
面板变矮或关闭时，它不再覆盖的格子在新缓冲里是空白，diff 会正常写出空格把旧边框擦掉。
所以只有 **resize** 需要清屏（终端自己重排过，屏幕上的内容和我们的缓冲无关了）。

- `overlay_key()` 只保留 kind（u8），不再带行数；关掉 `paint(force_clear)` 里由覆盖层
  触发的那一支。
- 面板开合/切换时只把 `wide_rows_dirty` 置上（重写含双宽字符的行），顺手解决"面板里
  有中文文件名，收起后留半个字"的问题。
- 新增两个测试：`closing_a_panel_leaves_no_ghost_rows`（渲染两次，断言收起后边框行数
  回到 1）、`narrowing_a_picker_filter_keeps_the_overlay_identity`（过滤变短不改变身份）。


## 已完成：输入法（IME）锚点 —— 光标必须是真实终端光标（2026-11-15 第十一轮）

现象：中文输入时拼音串和候选框出现在输入框**上方**的转录区里。

两个原因叠在一起：

1. 编辑器光标是手绘的 `│` 字形，终端光标被 ratatui 隐藏、且停在上一帧最后一个写入
   单元格。conhost 把 IME 组合串画在**控制台光标位置**、候选窗口也锚在它上面 →
   拼音就落在光标残留的地方。
2. 事件循环每 120ms 无条件重绘，ratatui 每次 `draw` 结束都会重设一次光标位置。就算
   把光标放到 caret，这也会在组合过程中把 IME 自己移动的光标拽回去、把组合串擦掉。

修法：

- `draw_editor` 返回 caret 单元格，`draw` 里 `frame.set_cursor_position(...)`；覆盖层或
  下载同意卡打开时**不**设置（光标随之隐藏）。手绘 `│` 删除；空输入时占位符前留一格，
  否则块状光标会吃掉首字母。
- 事件循环加 `dirty` 标志：键鼠、op 结果、Harness 事件、忙碌 spinner、toast 到期、
  覆盖层变化、resize、`needs_repaint` 才重绘，**空闲帧一个字节都不写**。
- 宽字符行补写从「每帧」改成「转录重建或滚动位置变化时」（`wide_rows_dirty` /
  `painted_scroll`），否则每帧重写会把 IME 组合串擦掉。

测试：`the_caret_is_a_real_terminal_cursor_inside_the_composer`（断言光标可见、落在
编辑器行、紧贴最后一个字符、脚下的格子为空）。预览 dump 增加 `cursor` 字段，渲染脚本
会把它画出来，方便离线核对位置。

已知限制：忙碌（流式输出）时每 tick 都要重绘，组合串仍可能被打断——终端不会告诉我们
"正在组合中"，没有干净的办法。

## 已完成：工具结果并入调用行 + 收紧工具块间距（2026-11-15 第十轮）

问题：`ToolResult` 事件只要和"当前正在跑的那一个工具"对不上，就会新建一条
`[tool result]` 条目；一轮里并行发多个工具调用时几乎必现，转录被切成碎片。
恢复会话也走同样的坏路径（`push_message` 里 `Role::Tool` 直接推一条 `tool result`）。

- 链接键改成调用 id：`App::tool_slots: HashMap<String, usize>`（id → 条目下标）。
  ToolCall 登记、ToolResult/ToolOutput 按 id 回填，所以结果顺序无关、并行安全。
  找不到 id 时才退化成一条以工具名命名的条目（老会话被压缩掉调用时仍能看到输出）。
- 重放路径同样按 `tool_call_id` 回填（session 记录里带 id，`session.rs:463` 有断言守着），
  恢复出来的会话与实时会话结构一致。
- 间距规则：**只有工具条目前面不加空行**（`transcript_lines` 里 `index > 0 && kind != Tool`）。
  工具调用属于它上面那段话/那段思考，正文与工具之间、工具与工具之间都紧贴；
  段落之间、回合之间仍保留一行留白。

## 已完成：歧义宽度改为启动实测 + 可配置（2026-11-15 第九轮）

结论：**没有**任何转义序列或注册表项能要求控制台把歧义宽度字符按 1 列画，那是终端
自己按字体/代码页决定的策略（同一台机器上 conhost 与 Windows Terminal 都可能不一致），
所以改成「不猜，测」。

实测方法（`lib::measure_ambiguous_width`，在 `ratatui::init()` 之后、首帧之前跑，屏幕
还在备用屏里，不碰回滚缓冲）：
1. `Clear(All)` + `MoveTo(0,0)`；
2. `Print("→")`；
3. `cursor::position()` —— Windows 走 `GetConsoleScreenBufferInfo`（真实格子坐标），
   Unix 走 DSR `ESC[6n`；
4. 列号 1 = 终端把它当普通字符，2 = 两列。失败时返回 `None`，调用方按「两列」处理（安全侧）。

本机实测（winpty/conhost，中文 Windows）：`pos=Ok((2,0))` → 两列，所以当前的 ASCII 折叠
是正确行为；换到把歧义宽度画成 1 列的终端时，`→ — …` 会原样保留，不再降级成 `->`。

配置：`RH_AMBIWIDTH=single|double|auto`（默认 auto）。**强制 single 只在终端真的按 1 列
渲染时安全**，否则幽灵字符会立刻回来。`Ctrl+H` 快捷键面板底部会显示实测结果。

## 已完成：悬空字符第三类来源 —— 正文里的歧义宽度字符（2026-11-15 第八轮）

用户截图里那列 `3` 不是半个汉字，是**一次 `edit` 工具调用行尾的 `+3`**：

```
+ [tool result]
    └ rename `norm` → `length_squared` preview (…):  +3
```

`→`（U+2192）是 East-Asian-Ambiguous：控制台按 **2 列**画，`unicode-width` 和 ratatui 都按 **1 列**算。于是这一行整体右移 1 列，末尾的 `3` 落在我们「认为是空白」的格子里 —— 之后每帧都认为那格是空白，永远不再写它。

第六轮只清掉了**我们自己写进 UI** 的歧义字形（`● ○ × › ·`），但**正文内容**（模型输出、工具回显、文件内容）里的没管：`→ — … × “” ¥` 全是这一类。

修法（`render::sanitize`）：
- `is_ambiguous(c)` = `c.width_cjk() != c.width()`（引入 `unicode-width`）。
- 命中就换成 ASCII 等价物：`→` → `->`、`—`/`–` → `-`、`…` → `...`、`“”` → `"`、`×` → `x`、`±` → `+/-`、`¥` → `CNY `、`€` → `EUR ` 等；不在表里的直接丢弃。
- 快路径也要带上歧义判断，否则无控制字符的正文会整段跳过清洗（本轮实测踩到）。
- 工具行参数过长现在会按 4 空格缩进折行，不再冲出右边缘（`tool_lines` 改用 `flow`）。
- 含双宽字符（CJK）的行整行强制重绘（`repaint_wide_rows` → `CellDiffOption::AlwaysUpdate`）：ratatui 的 `BufferDiff` 只在旧格子有背景色/可见修饰符时才补写宽字符的尾列，纯文本没有这两个属性，conhost 会把右半个汉字留在屏幕上。不含宽字符的行仍然走增量 diff，不牺牲性能。

测试 `ambiguous_width_content_is_folded_before_layout`：喂 `→ — … “” × ¥` 的工具参数与正文，断言渲染结果里一个都不剩、`->` 被保留、每行不超过内容宽度。

## 已完成：悬空字符真正根因 —— ratatui 启动不清屏（2026-11-15 第七轮）

前六轮修的控制字符和歧义宽度字形都是真问题，但都不是这次残留字符的来源。最终定位：

**`ratatui::Terminal::new()` 只按终端尺寸分配缓冲，从不清屏。** 第一帧拿两个全空缓冲做 diff，于是**只有我们画过的格子会被写**；屏幕上原本就有的内容（上一次运行退出后留在备用屏里的画面 —— 同一个控制台会话里 alt buffer 是保留的）在所有空白格子里原样留着，而后续每一帧都认为那些格子“本来就空白”，永远不会覆盖它们。这就是那列永不消失的孤立字符：位置取决于上次运行的内容（所以每张截图都不一样），字符恰好都是我们自己 UI 的片段（`+3`、`)`、`s`）。

修法（`crates/tui/src/lib.rs`）：`ratatui::init()` 之后、首帧之前 `terminal.clear()?` 一次。

这也解释了为什么中间几轮现象和根因一直对不上：第四轮为兜底在滚动时加了 `terminal.clear()`，它顺手把残留擦掉了，看起来“修好了”；第五轮为消除闪烁去掉那次 clear，残留又回来了。中间一直隔着一层“清屏”。

## 已完成：悬空字符第二真凶 = 东亚歧义宽度字形（2026-11-15 第六轮）

第四轮的控制字符清洗是必要的，但不是全部：清完之后滚动几屏，幽灵字符又回来了——因为还有第二类「我们算 1 列、终端画 2 列」的东西。

实测用户截图：终端单元格宽 10.45px，而工具行开头的 `●` 墨迹宽 **16px**。中文 Windows 控制台把 **East-Asian-Ambiguous** 字符按双列渲染，`unicode-width` 却按 1 列算。于是 `● [bash] …` 这一行整体少算 1 列，行尾字符被推到帧缓冲认为空白的格子上；ratatui 之后不会再写这些格子，它们就永久残留 —— 位置随行内容漂移，正是截图里“随机位置的孤立字符”。

为什么框线没事：`─ │ └ ╭╮╰╯`（U+2500–257F）在 Windows 控制台里是单列渲染（编辑器边框一直是正常的），而几何符号（U+25A0–25FF）走了字体回退、按双列算。

修法：
- 所有 UI 字形改成 **ASCII**：工具状态 `+`（成功）/ `~`（运行中）/ `!`（失败），提示符与用户消息前缀 `>`，通知前缀 `*`，折叠标记 `>` / `v`，spinner 用 `.` / `..` / `...`。`● ○ × › ·` 全部清出 UI。
- 保留框线（实测单列）和 CJK（真双宽）。
- `sanitize()` 追加丢弃零宽连接符、bidi 覆盖符（U+202A–202E，能重排整行）、变体选择符（U+FE00–FE0F，会把 1 格变 2 格）、软连字符、BOM。
- 状态栏多留 1 列余量（`¥` 也是歧义宽度）。
- 测试 `transcript_uses_only_single_width_glyphs`：断言渲染出的每一行都不含几何符号/箭头/通用标点/Latin-1 歧义符号，防止以后再引入。

## 已完成：滚动闪烁（2026-11-15 第五轮）

悬空字符修好后，上一轮加的兜底变成了新问题：**每次滚轮都 `terminal.clear()` + 整屏重绘**，`ESC[2J` 先一个 write 落地、新内容后一个 write 才到，中间那一帧就是黑屏 —— 用户看到的就是“每滚一下闪一下”。

- 滚轮 / 拖拽到边缘自动滚动**不再**请求整屏重绘（`needs_repaint` 只留给 Ctrl+G 和切换会话）。
- 新增 `lib::paint()`：整帧（含可选的 clear）包在 `BeginSynchronizedUpdate` / `EndSynchronizedUpdate`（`CSI ?2026h/l`）里，终端把一帧当作原子操作呈现，即使是大面积重绘也不会撕裂或闪黑。`End` 无条件发送，避免出错时屏��卡死在未提交状态。
- 强制重绘（`terminal.clear()`）只在 resize 稳定后、切换会话、Ctrl+G 发生。**覆盖层开合不要清屏**：面板变矮时 diff 会用空格擦掉旧边框，清屏只会让整个窗口闪一下。
- 测试：`scrolling_does_not_force_a_full_repaint` —— 滚轮只改 `scroll`，不许置 `needs_repaint`。

## 已完成：悬空字符真凶 = 制表符/转义序列/裸换行（2026-11-15 第四轮）

上一轮的 `DisableLineWrap` 是必要的但不是充分条件：**问题真正的根源是文本里的控制字符被原样塞进了 `Span`**。

1. **制表符**：`read` / `grep` / `edit` 返回的是原始文件内容，源码缩进全是 `\t`。`unicode-width` 认为 `\t` 宽 0，终端却跳到下一个 8 列制表位 → 同一行后面的文字整体右移 → 尾部落进「帧缓冲认为是空白」的格子里 → 之后再也不会被重绘，就成了悬空字符。`bash` 输出的 ANSI 颜色序列同理，终端会真的去执行它。
2. **裸换行**：用户消息是整段塞进 `flow()` 的，正文里的 `\n` 被当成普通字符写进 span → 终端收到 LF 会把后续文字挪到下一行同一列继续写 → **同一列上连续出现一堆孤立字符**，正是截图里的现象。

修法（全部在渲染边界，`render::sanitize` + `flow`）：
- `sanitize()`：`\t` 展开成 4 空格、保留 `\n`、其余控制字符（含 ESC、`\r`、C1）全部丢弃；无控制字符时零拷贝返回。
- `flow()` 遇到 `\n` 真正换行（续行用 `cont` 前缀），不再把换行写进 span。
- 覆盖范围：转录每条 entry 的 title/arg/body、文件预览行、picker 标签与详情、todo 内容、LSP 详情——凡是可能来自文件/模型/子进程的字符串都过一遍。
- 折叠标记 `►/▼` 在用户字体里是豆腐块（截图里的 `□`），换成 ASCII 的 `>` / `v`。
- ~~每次滚轮滚动都置 `needs_repaint`~~ → 第五轮撤销：这是闪烁的来源，真正的修复是控制字符清洗。

测试：`tool_output_cannot_desync_the_screen` —— 构造含 `\t`、`\r\n`、ESC 序列的工具输出与用户消息，断言渲染出的每一行都不含控制字符且宽度不超过内容区。

选区：复制成功后自动取消选中（失败时保留，方便重试）。

## 已完成：修复滚动后悬空字符 + 改线性选区 + 修剪贴板乱码（2026-11-15 第三轮）

用户反馈三件事：**恢复历史会话后滚动屏幕会出现悬空字符；选区是矩形框，不符合终端原生选择；复制出来是乱码。**

### 1. 悬空字符：串行换行把整块屏幕顶歪了

根因是**终端自动换行**：只要有一行比我们量出来的宽度多出 1 列（东亚歧义宽度字符、CJK 标点、字体回退把某个字形画成 2 列），终端就会自动折行并把整屏向上顶一行；此后 ratatui 的增量刷新全部落在错误的行上，旧内容残留在各式各样的小格子里，就成了截图里那一列悬空的 `3` / `t` / `)`。

修法（`lib.rs`）：
- 启动时 `execute!(stdout, EnableMouseCapture, DisableLineWrap)`，退出时 `EnableLineWrap`；同时串接 panic hook，先开换行再交给 ratatui 的复原钩子，避免进程崩掉后终端一直不换行。
- ~~用户滚动（`follow` 翻转）和切换会话后强制整屏重绘~~ → 滚动那次已在第五轮撤销（会闪屏）；切换会话仍保留。
- `Ctrl+G` 除了回到末尾，也强制整屏重绘，作为“屏幕花了按一下”的兜底。

另外把缓存签名从「按长度」换成「按内容」（`xxh32`），避免恢复会话时同长度的正文替换后仍然渲染旧行。

### 2. 选区：矩形 -> 线性

- `App::ordered()` 先把两个角按「行优先、同行比列」排序，`selection_row_span()` 给出某一行应该取的半开列区间：首行从锚点列开始，中间行整行，末行到光标列结束——和终端一致。
- 高亮（`render`）与复制（`app`）共用同一套区间计算，不会再出现“看起来选中了、复制出来是另一块”的情况。
- 复制按**显示列**切片（`slice_columns`），双宽字符不会被劈成半个，每行尾部空白会被 trim。
- 拖到上下边缘会自动滚动，锚点跟着平移，可以选超过一屏的内容。
- 复制成功/失败改成状态栏 toast（`App::toast`，约 2 秒后消失），不再往转录里插一条 `copied N lines` 把内容顶走。

### 3. 剪贴板乱码

原来是 `text.as_bytes()` 直接灌给 `clip.exe`，它按 ANSI 代码页（中文 Windows 是 936）解码，中文/emoji 全成乱码。改用 `arboard`（Win32 `CF_UNICODETEXT` / macOS / X11 通用）：`arboard::{version = "3", default-features = false}`。失败时 toast 报错而不是静默。

### 测试

`preview` 模块（都跑在 TestBackend 上，不碰真实终端）：
- `selection_is_linear_not_rectangular`：首行切锚点、中间行整行、末行切光标。
- `slice_columns_never_splits_a_wide_character`：CJK 按列切片。
- `clipboard_round_trips_unicode`（ignore，会覆盖剪贴板）：验证平台剪贴板能往返中文。

`cargo test --workspace` 全绿（core 147 / tui 4 / cli 4）。

## 已完成：思考 / 工具折叠交互（2026-11-15 第二轮）

用户需求：**取消顶部栏；思考输出过程中展开、结束自动折叠、鼠标点击切换折叠；工具输出同理。**

1. **顶部栏删除**：`EntryKind::Welcome`、`App::push_welcome`、`render::welcome_lines` 一并删掉（第一版加的 masthead 整体作废），`theme::BORDER_DIM` 也随之无用而删除。转录第一行就是对话本身。
2. **折叠状态只有一个来源**：`Entry.expanded`。删掉 `App.tools_open` / `App.show_thinking` 两个全局开关，`Entry::visible_body(tools_open)` 换成 `Entry::foldable()`（工具或思考且正文非空）。
3. **自动展开 / 折叠时序**：
   - 思考：首个 `AssistantReasoning` 分片建条目并 `expanded = true`，边流边看；`App::end_reasoning()` 在「正文开始 / 工具调用 / 回合结束」时把它折叠并清空 `reasoning_at`。原先散落的四处 `reasoning_at = None` 全部改为调用它。
   - 工具：`ToolCall` 建条目时展开（看实时输出），`ToolResult` 到达时折叠。
   - 会话重放（`push_message`）一律折叠，回看历史不会满屏输出。
4. **鼠标点击**：`render::transcript_lines` 现在同时产出 `owner_cache`（每行属于哪个 entry，分隔行 `usize::MAX`）。`finish_selection` 在「按下即抬起」的单击情况下改调 `App::toggle_row()`，拖动仍然复制文本；点击只对 `foldable()` 的条目生效。
5. **键盘**：Ctrl+O / Ctrl+T 从「总开关」改为「全部展开 / 全部折叠」（只要有一个是开的就全关，否则全开）。
6. **视觉**：折叠的思考是 `► thinking  3 lines`，展开是 `▼ thinking` + 缩进正文；折叠的工具在标题下给 `└ 首行输出  +N`，展开是 4 空格缩进的全文。
7. **测试**（都在 `preview` 模块，正常跑不 ignore）：`clicking_a_row_folds_the_entry_that_owns_it`（owner 映射 / 点击 / 分隔行空转）、`streaming_blocks_open_themselves_and_fold_on_completion`（推理与工具的展开→折叠时序）。
8. 顺带：状态栏降级多一档——空间极窄时整块丢弃 `in/out`，避免出现 `out ...` 这种截断值；压缩提示里的 `⇣`（非 WGL4 字形）去掉。

## 已完成：TUI 视觉重做（2026-11-15）

用户需求：**当前 TUI 很难看，按 pi agent 的样式重做。**

参考：pi 的视觉语法是「一份文档，不是一堆表单」——无边框记录流、用户消息一条安静底色带、工具调用压成一行灰字、正文就是文本、只有一个圆角输入框、一行状态栏，颜色只留给必须一眼看到的状态。本轮把 TUI 整体搬成这套语法。

### 布局（`crates/tui/src/render.rs` 全量重写）

1. 三段式：`transcript`（顶部留 1 行呼吸 / 左缩进 1 列 / 右侧留 2 列防 CJK 溢出）→ `editor`（3 行圆角框）→ `footer`（1 行状态栏）。转录内容向右偏移后，`app.mouse_area` 同步指向内容区，拖拽选区坐标不用改。
2. 用户消息：整行底色带 `USER_BG` + 强调色 `›`，换行续行缩进 2 空格并补齐底色，形成一条完整色带。
3. 助手正文：内置极简 markdown——`#` 标题加粗、`-`/`1.` 列表（破折号用强调色）、`>` 引用（`│` 竖线 + 斜体）、` ``` ` 围栏代码（`CODE_BG` 通栏底 + 2 空格缩进）、行内 `` `code` ``（`CODE_BG` 底）、`**bold**`。
4. 思考：`·` + DIM 斜体；超 700 字截断。
5. 工具调用：单行 `● [name] 参数摘要`（`●` 绿=成功 / `×` 红=失败 / `○` 黄=运行中）。折叠时下面补一行首行输出的 DIM 预览 + `+N`；`Ctrl+O` 展开为 4 空格缩进的整段输出（失败时整段红色）。
6. 通知 `·`、错误 `×`、压缩 `── 标题 ──────`（铺满内容宽度）。
7. 顶部没有独立 header：转录第一行就是对话（第一版的 masthead 已在第二轮按用户要求删除）。
8. 编辑器：圆角框，`›` 强调色提示符，竖线光标；忙碌时边框转强调色；空输入时只显示 FAINT 斜体占位符（不画光标——`› │` 两竖挨着会被读成分隔线）。
9. 状态栏：字段与顺序保持不变（`cwd|model|compact|ctx|in/out|CacheHit|¥cost`），但空间不足时按 **路径长度 → CacheHit → cost → compact model** 逐级隐藏，只有实在放不下才截断；同一行绝不换行（换行会把编辑器顶掉）。`working` 后跟等宽三点动画，排队时显示 `N queued`。
10. 覆盖层：全部改成圆角 + 不透明底 `BG_BASE` + 左右各 1 列内边距；标题写在左上边框、`n of m` 写在右上边框；列表按最长 label 自适应两列；pick 面板与编辑器之间留 1 行空隙。

### 字形（重要）

只使用 WGL4 覆盖、Consolas/Cascadia 都有的字符：`● × ○ › │ · ─ ╭╮╰╯`。删掉了 `✔ ✘ ❯ ▏ ╌ ⠋`——它们在 Windows 默认等宽字体里是豆腐块（旧版工具行前面那个方块就是这么来的）。spinner 改成三帧等宽点 `·  / ·· / ···`。

### 数据模型

- `entries::Entry` 新增 `arg`（工具参数的一行摘要），`title` 现在只放工具名；`EntryKind` 新增 `Welcome`。
- `app::tool_summary` 从工具调用 JSON 里提取真正有意义的参数：shell 取 `command`、read/write/edit 取 `path`（edit 追加 `(N edits)`）、grep/glob 取 `pattern in path`、web_search 取 `query`、await 取 `pid`、todo 取条数，取不到就退回第一个字符串字段。原始 JSON 不再显示（`{"command":"cargo test","timeout_ms":120000}` 这种对用户没有价值）。
- `ToolOutput` 流式输出改为覆盖 `body`（原先往 JSON 参数后面追加，展开后是一堆废文本）。

### 预览工具（新增）

`crates/tui/src/preview.rs`：用 `Agent::with_backend` + 桩 backend 构造一个装满代表性条目的 `App`，用 `TestBackend` 渲染后把单元格（字符/fg/bg）dump 成 `target/ui-preview*.json`。

```bash
cargo test -p rustedharness-tui -- --ignored preview   # 主界面 + 窄屏
cargo test -p rustedharness-tui -- --ignored preview_picker|preview_panel|preview_idle
```

`.workbuddy/preview/render.py` 把 json 画成 PNG（不提交，随手看样式用）。四个预览测试都是 `#[ignore]`，不参与 `cargo test`。

### 验证

- `cargo test --workspace` 全绿（core 147 + cli 4 + 4 个 ignored 预览）。
- `cargo clippy --workspace --all-targets` 无警告；`cargo fmt` 已跑。
- 实机：`timeout 5 winpty -Xallow-non-tty ./target/debug/rustedharness.exe tui` 跑满 5s 未崩（80 列，状态栏自动省掉 compact 字段）。
- 注意：winpty 会把 24bit 颜色降成 ANSI-16，从 winpty 抓下来的帧偏色是采集工具的问题，不是应用的问题；Windows Terminal 下 `Color::Rgb` 是直出的。

## 已完成：会话文件惰性创建（2026-11-08，本轮已实现并提交）


用户需求：**在用户发送第一条消息之前，不得创建会话 jsonl 文件**。

实现方式（`crates/core/src/session.rs`）：

1. `SessionWriter::create()` 只生成 id/path 并 `create_dir_all`；Meta 记录暂存在 `pending: Vec<Entry>`，不打开文件、不写盘、不 prune。
2. 首个 `Message` 记录到达时 `open()`：创建文件 → 按序写入 header → `prune(max_saved)` → 写本条。首条消息之前到达的其他记录（`/model` 的 `Models`）只进 header。
3. `resume()` 直接 `file = Some(..)`、`header` 空。
4. 后续清理（同一轮规范扫描）删除了此前为“空会话”加的全部补偿逻辑：`shutdown` 删文件、prune 空文件探测、`SessionStore::list` 空会话过滤，以及未使用的 `SessionWriter::summary` / `process` / `notice` 和对应记录变体。
5. 新增测试：`create_writes_nothing_to_disk`、`header_records_precede_the_first_message`。

实机验证：`target/debug/rustedharness.exe run` 保持运行 3 秒期间与退出后，`~/.rustedharness/sessions` 文件数均不变（无新建）。`cargo test --workspace` 全绿。

## 已完成：LSP 自动装最新版 + 校验缺失仅警告 + 启动检查更新（2026-11-08）

用户需求：**取消固定版本（改用 GitHub latest 直链）、拿不到校验值只警告不拒绝安装、尽量装最新版、启动时自动检查已装 LSP 是否有更新。**

1. **下载改为 latest 直链**：`ServerSpec` 删除 `version`、`download_url`、`archive` 三个字段，改为 `repo`（如 `rust-lang/rust-analyzer`）+ `asset` 文件名模板（`{tag}` / `{target}` / `{os}` / `{ext}`）。`latest_tag()` 用 `GET https://github.com/<repo>/releases/latest` 的 302 `Location` 解析真实 tag（纯 HTTPS，不消耗 API 配额），`download_target()` 拼出 `.../releases/download/<tag>/<asset>`。
   - 原来固定版本的 3 个服务器全部失效：rust-analyzer 的 `2026-10-19` 已 404，其 Windows 资产是 `.zip` 而模板写 `.gz`；lua 的 Windows 资产是 `.zip` 而模板写 `.tar.gz`。改成 latest + 按平台取扩展名后不再有版本腐烂。
   - `archive` 类型改为从资产名后缀推断（`archive_kind_for`），删除 `spec.archive`；`lsp.downloads.<id>.archive` 覆盖保留。
   - `lsp.downloads.<id>.version` 变成可选 pin：设了就跳过 latest 查询（离线/私有镜像可用）。新增测试 `a_pinned_tag_builds_the_download_without_the_network` 证明该路径不触网。
2. **校验值缺失只警告**：`published_digest` 返回 `ExpectedDigest::{Verified, Unverified(reason)}`，删除 `E_LSP_CHECKSUM_UNAVAILABLE` / `E_LSP_CHECKSUM_LOOKUP_FAILED` 两个硬失败，改为发出 `DownloadStatus { state: "unverified", detail: reason }` 后照常安装；只有“发布了 digest 但对不上”仍是硬失败（`E_LSP_CHECKSUM_MISMATCH`），并清掉该服务器的 digest 缓存。`DownloadConsent.sha256`、`ChecksumInfo.sha256` 变为 `Option`，CLI/TUI 都显示 `none published`。
3. **启动检查更新**：`LspHub::new` 在 5 秒后跑一次 `check_updates()`，遍历已安装的服务器，比对 `latest_tag()` 与本地目录名，不同则发 `DownloadStatus { state: "update_available", detail: "<旧> -> <新>; run lsp update <id>" }`，不自动下载。
4. **只留一份**：装到 `lsp/<id>/<tag>/` 后 `prune_other_versions` 删掉被替换的版本目录；新增 `installed()` 读取唯一已装版本，取代原先写死的 `cached_binary(spec, spec.version)`。
5. `lsp list` 的 VERSION 列改为 INSTALLED（读本地缓存），`ServerStatus` 新增 `version` 字段。
6. 新增测试 `lua_assets_use_release_platform_names`、`archive_kind_follows_the_asset_name`、`latest_release_redirects_yield_the_tag`、`registry_assets_resolve_to_whitelisted_https_urls`（替换原 `registry_download_templates_*`）。
7. 顺带修可读性：`latest_tag` 的失败信息改用 `error_chain()` 打印完整 error source 链（原先只显示 "error sending request"）。

## 已完成：AGENTS.md 规范扫描（2026-11-08）

- 全仓 `cargo fmt`，清空所有 clippy 警告（含未使用 import/字段、identical blocks、doc list）。
- 删除向后兼容/迁移路径：`config.toml` 解析、旧平台配置目录回退、`[models]` 旧块、`apiKeyEnv`、所有 snake_case 别名、`RUSTEDHARNESS_CONFIG_DIR`、`toml` 依赖。
- 删除死代码/推测性 API：`SessionWriter::summary/process/notice`、`SessionRecord::Process/Notice`、`Config::{active_provider, provider_for_model, resolve_api_key*, session_dir, write_default, models_path, config_path}`、`UsageTracker::totals`、`ToolRegistry::with_tools/insert/remove`、`TodoStore::has_unfinished`、`WorkspacePathResolver::rel_string`、`ChatMessage::text_len`、`CompactionPlan::summarize_from`。
- 消除重复：CLI/core 共用 `lsp::{pin_sha256, install_server, checksum_server}`；CLI 抽出 `run_prompt`；web 抽出 `workspace_payload`/`client_error`/`client_notice`；`config show` 复用 `split_files`；移除三处多余的 session 重排序。
- 修正可读性/宽度缺陷：合并重复的 `shorten`/`shorten_path`（原 `render::shorten` 会超宽 2 列）、`clip`/`clip_line`/`truncate_line` 按显示宽度截断、`lsp checksum` 打印实际 `--config` 路径。
- 手写 base64 换成已存在的 `base64` crate。
- README / docs/architecture.md 同步移除迁移说明，配置键统一 camelCase。

## 已完成：LSP 自动下载校验 + 429 重试设置（2026-11-08）

### LSP 下载
- 安装/自动下载不再要求 `lsp.downloads.<id>.sha256` 或 `--sha256`；`download_manifest` 会自动解析校验值。
- 校验值解析顺序只有三步：配置显式 pin → 本地 digest 缓存 → GitHub release API（asset 的 `digest` 字段，或该 release 附带的 checksums 资产）。
- 不猜 sidecar、不扫工件目录旁的 `SHA256SUMS`：只看 release API 记录的 digest，避免拿到来路不明的文件。匿名配额 60 次/小时，`GITHUB_TOKEN` 可选、仅用于提额。成功解析的 digest 会写入 `<data>/lsp/checksums/<id>/<version>.sha256` 缓存，重复安装与后续 `lsp checksum` 不再请求 API；`lsp update` 会先清除该缓存再重新解析。
- 二进制本体是直接 `GET` release 下载链接（`github.com/<owner>/<repo>/releases/download/...`），跟随重定向到 `objects.githubusercontent.com`，不经 API。
- 支持 GNU `hash  file`、`hash *file`、BSD `SHA256 (file) = hash` 三种校验文件格式。
- 两种失败严格区分，`github_release_digest` 返回 `DigestLookup` 而非 `Option`：
  - `DigestLookup::NoDigest` → `E_LSP_CHECKSUM_UNAVAILABLE`：API 正常应答、但厂商确实没发布校验值，提示手动安装；
  - `DigestLookup::Unavailable(reason)` → `E_LSP_CHECKSUM_LOOKUP_FAILED`：API 本身不可用（离线 / `HTTP 403` 限流 / 返回体异常），提示重试、设 `GITHUB_TOKEN` 或显式 pin。
  - 之前两者合并成同一错误，IP 被限流时会误报"厂商未发布校验值"。
- ~~所有来源都拿不到校验值时拒绝安装~~ → 改为仅警告并照常安装（见下节）。
- `lsp checksum` 保留为只读检查命令；`lsp.downloads.<id>.sha256` 保留为可选覆盖（私有镜像），文档已说明非必需。
- 新增单测 `checksum_files_are_parsed_in_common_formats`、`release_assets_expose_digests_and_checksum_files`、`discovered_digests_roundtrip_through_the_cache`、`artifact_urls_outside_github_releases_answer_without_a_digest`。

### 429 重试设置
- 新增 `agent.rateLimitRetries`（`Option<u32>`，`null`=无限，建议 3/5/10）与 `agent.rateLimitIntervalSecs`（秒，建议 1/3/5，默认 5，normalize 夹在 1..=300）。
- `stream_with_retry` 按设置重试；达到上限发 error Notice 并返回错误；通知中显示 `attempt n/max`。
- 新增单测 `rate_limit_settings_parse_and_normalize`、`rate_limit_retries_stop_at_the_configured_limit`。
- README / docs/architecture.md 已同步配置样例与行为说明。

## 上一轮改动的已知坑（勿回退）

- 不再需要在 `shutdown` 里删除空会话（文件根本不会创建）；若重新引入持锁 `.await`，注意 web crate 的 `on_upgrade` future 要求 Send。
- `self.session.lock()` 建议统一 `unwrap_or_else(|e| e.into_inner())`。

## 验证与提交流程（每个任务必做，用户要求实机验证、禁止虚报成功）

```bash
taskkill /F /IM rustedharness.exe   # 旧进程会锁二进制导致“拒绝访问 os error 5”
cargo build -p rustedharness-cli    # gui 依赖由 web 编译错误间接覆盖
cargo test --workspace              # 当前基线：10 套件全绿，core 147 项
git -C E:/rustedharness add -A && \
git -C E:/rustedharness -c user.name="xhy37000616" -c user.email="xhy37000616@163.com" commit -m "..."
```

### 关于 `error deleting lock file for incremental compilation session ...`

这条**不是编译失败**，最后一行仍然是 `Finished dev profile`。它是 rustc 收尾时删不掉上一次增量编译 session 的 `.lock`（进程被强杀、或两个 cargo 同时跑撞在一起）而产生的告警。更坑的是 cargo 会把它记在 `target/debug/.fingerprint` 里，之后每次 `cargo build` 都当成“cached warning”原样重放——**哪怕那个目录早在几轮前就被删掉了**。

处理：
```bash
cargo clean -p rustedharness-tui && cargo clean -p rustedharness-cli   # 清指纹，停止重放
rm -rf target/debug/incremental                                       # 顺手回收（曾涨到 11G / 147 个 session）
```
另外不要并发跑两个 cargo（`build` 和 `clippy` 同时跑也会抢同一个 session 锁）。
禁止修改 git 全局/本地配置，只用 -c 参数。提交历史最近几条：
`5044382` 会话惰性创建 → `89f2ac5` fmt/clippy → `6671783` 删空会话补偿 → `e475b3a` 删配置兼容层 → `f4e56ed` 规范扫描 → 本次 LSP/429。

## 关键背景（勿踩）

- TUI 是全屏 + 拖拽复制方案（用户否决过 inline viewport 方案，别再碰）。启动时**必须** `terminal.clear()`：ratatui 不会替你清屏，不清就会把上一次运行的画面留在空白格子里（悬空字符的真正来源）。选区必须是**线性**的（首行切锚点列、中间整行、末行切光标列），不要退回矩形框。
- 运行期必须保持 `DisableLineWrap`（`lib.rs` 里 `execute!` 打开、退出/panic 时恢复）：一旦允许自动换行，任何一行超出 1 列都会把整屏顶歪，之后全是悬空字符。
- 渲染宽度：`draw()` 全局右留 1 列；UI 内已禁用东亚歧义宽度字符（· … ¥ 除外），新增 UI 字符串只用 ASCII 标点做分隔符/省略号。`¥` 允许保留（cost 字段）。
- UI 字形规则：**ASCII + 框线（U+2500–257F）**，我们自己的字符串里别的一律不要（这样无论终端怎么量都安全）。正文里的歧义宽度字符由 `render::sanitize` 按实测模式决定是否折叠成 ASCII，见 `render::AMBIGUOUS_IS_WIDE`。几何符号（`● ○`）、箭头、通用标点（`· › …`）、Latin-1 歧义符号（`× ±`）在中文控制台会按 **2 列**渲染，而 unicode-width 算 1 列，行尾字符会被推到帧缓冲之外变成永久幽灵字符。`test/transcript_uses_only_single_width_glyphs` 在守这条线。
- 输入框的光标**必须**是真实终端光标（`frame.set_cursor_position`）：Windows 控制台把 IME 组合串和候选窗都锚在控制台光标上，手绘 `│` 会让中文输入法错位。空闲时不要重绘，否则会把组合串擦掉。
- 工具输出**永远**挂在调用行下面（`└ 首行 +N`，Ctrl+O/点击展开），不要为结果再建条目；绑定键是 tool call id，不是"最近一个工具"。
- 状态栏格式：`cwd|model|compact X|ctx 1.2%/1M|in A|out B|CacheHit98%|¥cost`，分隔符无空格。宽度不足时按 路径→CacheHit→cost→compact 顺序隐藏字段（见 `render::footer_line` 的 layouts 表），不要退回强制截断。
- resize 防抖：lib.rs event_loop，Resize 风暴期间跳过 draw，静默 150ms 后 clear+强制重绘（RESIZE_SETTLE 常量）。
- 工作区根锁死（target/debug 启动限制）是设计内行为，不要改。
- 不要写依赖网络的实机测试。
- 用户偏好：“少思考，多看代码”；回复用中文；改动必须构建+测试通过并如实汇报。


## 已完成：依赖库替换（2026-11-16）

1. **Responses**：启用 `async-openai` 的 `responses` feature，Responses 后端改用
   `create_byot` / `create_stream_byot`，删除自写 HTTP 与 SSE 解析。
2. **终端模拟**：`proc.rs` 的 `ScreenEmulator` 改用 `vt100`，保留 PTY、进程注册、
   kill、原始字节和统计；屏幕宽度跟随 `pty_cols`。
3. **MCP**：改用官方 `rmcp` 客户端（stdio + Streamable HTTP），删除自写 JSON-RPC、
   pending map、SSE 和协议版本常量；保留配置解析、命名、deny、懒刷新和超时。
4. **JSONC**：配置解析改用 `jsonc-parser`，删除手写去注释/去尾逗号逻辑。
5. **grep**：`grep` 工具改用 `grep-regex` + `grep-searcher` 流式搜索，锚点仍由
   hashline 生成；不再整文件读入。
6. **LSP**：诊断、Location/LocationLink 用 `lsp-types` 解析，UTF-16 位置换算用
   `line-index`；JSON-RPC 传输和 supervisor 保留，以维持重启/进度/安装行为。
7. **二进制检测**：改用 `content_inspector`。
8. **内部重复**：统一三套折行逻辑；`SKIP_DIRS` 移到 `core::paths`；MCP 复用
   `config::interpolate_env`。
9. **HTML**：`read(URL)` 的 HTML 处理不再用正则。`html5ever` 解析一次，直接在
   DOM 上剪掉 `script`/`style`/`noscript`/`svg`/`head`/`iframe`/`template` 子树，
   再把同一棵树交给 `html2md::walk` 转 Markdown（不再序列化后二次解析）；
   标题取 `<title>` 元素的文本；标签剥离和实体解码分别改用 DOM 文本遍历和
   `html-escape`（此前只认 10 个命名实体）。
10. **小工具**：服务器二进制查找改用 `which`（旧实现自己拼 `PATHEXT`）；`~` 展开和
    `$ENV` 插值改用 `shellexpand`（旧实现是手写扫描，`$!` 转义在展开前重写）；
    `data:` URL 用 `data-url` 解析并统一重新编码为 base64（旧实现只认 `;base64,`，
    百分号编码的 URL 会被当成 base64 发出去）；浏览器打开改用 `webbrowser`；
    `config edit` 用 `shell-words` 切分 `$VISUAL`/`$EDITOR`——它们存的是命令行而不是
    程序名，旧代码把整串交给 `Command::new`，`EDITOR="code -w"` 直接 ENOENT。

## 已完成：压缩思考档位（2026-11-16）

- 删掉 `/compact-model` 斜杠命令：压缩模型只从 `/settings` → compaction → model 修改，
  和其余压缩参数放在一起（`ModelTarget::Compact` 与 `set_model` 保留给设置页用）。
- 新增 `agent.compactThinking`：`off` / `low` / `medium` / `high` / `xhigh` / `max`，
  `/settings` 里 Enter 循环切换。**主模型压缩和独立模型压缩都生效**——档位写在
  `ChatRequest.reasoning_effort` 上，两条路径共用同一个请求构造。
- `off` 不发送任何字段（交给 provider 默认值）；其余档位在 Chat Completions 上写
  `reasoning_effort`，在 Responses 上写 `reasoning.effort`。
- 只有压缩模型声明 `reasoning` 且 provider 的 `compat.supportsReasoningEffort` 为真
  时才发送（`Router::stream_chat` 清掉非推理模型上的档位）。该 compat 项此前只解析
  不使用，现在接上了。

## 已完成：无限 HTTP 超时、缓存过期提醒与 cache miss notice（2026-11-16）

1. **默认不设 provider 超时**（`agent.httpIdleTimeoutMs = 0`）。慢速免费 API 可以一直等，
   但每一步等待都能被打断：`provider.rs` 新增 `await_guarded`，把"等响应头 / 等下一个
   SSE chunk / 等非流式响应"三个点统一和 turn 的 `CancellationToken` 赛跑；
   `Backend::complete` 现在接收调用方的 token（之前它自己造一个永不取消的 token，
   压缩摘要和 `read(URL)` 抽取一旦卡住就只能杀进程）。设成非 0 时变成空闲预算：
   连续 N 毫秒没有一个字节就放弃并报 `provider sent no data for Ns`。
2. **不猜缓存寿命**：provider 的 API 不返回缓存有效时间，所以不做"发送前提醒"、
   不解析 `promptCache`、也没有任何按型号硬编码的 TTL。宁可少一个功能，也不编一个
   查不到的结论。
3. **cache miss notice**（`core/cache.rs`）：provider 只报告 `cached_tokens`，不报告为什么
   丢缓存，所以 `CacheWatch` 用相邻两轮比较——上一轮已在 prompt 里、这一轮却按普通
   input 计费的 token 就是 miss。噪声地板 1024 token，≥20k 才提示；标签只写可观测的
   观察到的事实（重计费 token 数、距上一次请求的空闲时长、是否换了模型），不声称
   任何寿命。压缩会重写 prompt，所以压缩后 `reset()`，下一轮不算 miss。`agent.showCacheMissNotices`（默认开）可关，`/settings` 根页第一项就是它。
4. **不做 cache warming**：缺 `cost` 价格数据就算不出收益，盲目重发只会烧钱。
