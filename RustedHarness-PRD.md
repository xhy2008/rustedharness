# RustedHarness PRD v1.7

- **项目**：RustedHarness——Rust 编写的 AI Agent Harness（Linux/macOS/Windows）
- **形态**：core crate + TUI（ratatui）/ GUI（Tauri 2）/ WebUI（axum），共享 HarnessEvent/HarnessCommand 协议
- **版本**：v1.7（2026-10-23）

## 1. 目标

1. OpenAI Chat Completions 协议兼容（含 Tools Schema），改 base_url 即接任意兼容端点。
2. 所有工具相对路径锚定 session cwd；命令执行无持久 cd；Windows 命令工具按平台分流命名（bash/powershell）。
3. Hashline 内容哈希锚点编辑（复刻 oh-my-pi，MIT attribution）；`edits[]` 批量补丁；write 追加语义。
4. 代码智能：LSP 一等公民（运行时自动下载 server）+ tree-sitter 索引透明兜底。
5. `models.agent` / `models.compact` 分角色指定模型（编码 vs 会话压缩）。
6. web_search 多提供商回退；URL 由统一 `read` 处理。
7. 命令执行：**pty（TTY）捕获**，进度条原地刷新、回传每行终态；后台 pid + `await`。
8. todo 工具 + 停转自动催办。
9. MCP client（rmcp）。
10. 工具一律自动执行（无审批），唯一安全闸门为路径边界检查。

## 2. 技术选型

| 域 | 选型 |
|---|---|
| LLM | `async-openai` + 自研 ProviderAdapter；schema：`schemars`；HTTP：`reqwest` |
| TUI | `ratatui`+`crossterm`+`syntect`+`tui-markdown` |
| GUI/Web | `tauri` 2.x；`axum` 0.8+`tower-http`+WS；前端 React+Vite 单工程双壳 |
| Hashline | `xxhash-rust`（xxHash32） |
| LSP | `lsp-types`（3.16）+自研 JSON-RPC 状态机（生态 client 均不成熟，自建） |
| pty/进程 | `portable-pty`（Unix openpty / Windows ConPTY）；终端仿真器候选 `vt100`/`tui-emulator`，或自研转义状态机；`tokio::process`+进程组（`nix`/Job Object） |
| 兜底索引 | `tree-sitter`+`tree-sitter-language-pack`+`rusqlite`+`ignore`/`globset`+`notify`；grep 内核 `grep-regex`/`grep-searcher` |
| 联网 | `html-to-markdown`、`moka`(TTL)、SearchProvider：brave/tavily/duckduckgo |
| MCP | `rmcp` 1.4（备选 rust-mcp-sdk） |
| Token | `tiktoken-rs` |
| 下载 | `reqwest`+`sha2`+`tar/zip/flate2` |
| 其他 | `clap` 4、`serde`/`toml`、`directories`、`similar`、`anyhow`/`thiserror`、`tracing`、`rayon`、`dunce` |

## 3. 架构

```
TUI | Tauri(同 Web 前端) | WebUI(axum serve)
        ↓ HarnessEvent / HarnessCommand
Core: Agent Loop(模型→工具→回填; 停转→todo催办; edit/write 后自动诊断回填)
  Provider(async-openai+Adapter) | 路由(agent/compact) | tiktoken+结构化压缩
  Tool Registry(schemars): read|write|edit|bash|powershell|await|grep|glob|lsp|web_search|todo|mcp__*
  WorkspaceFS(R1-R4) | Hashline | ProcRegistry(进程组+pty会话)
  CodeIntel: L1 LSP(Supervisor+Downloader+Registry) → L0 tree-sitter SQLite
```

## 4. 机制规格

### 4.1 Provider 层
- async-openai 类型全用；SSE 流式、tool_calls 分片聚合、parallel calls、`tool` role 回填、`usage`。
- ProviderAdapter 开关：`max_tokens`/`max_completion_tokens`；`system`/`developer`；`reasoning_content` 回传；`tool_choice` 不支持时降级。
- 配置 `providers.<name>={base_url,api_key_env,headers,compat}`；支持 OPENAI_BASE_URL/OPENAI_API_KEY。

### 4.2 路径规则
- R1 相对路径锚定 session_cwd；R2 shell 每次调用以 session_cwd 新建进程（无持久 cd）；R3 输出一律回显相对路径；R4 解析后逃逸 workspace 根→拒绝执行（结构化错误）。

### 4.3 Hashline / write / edit
- 锚点 = xxHash32(f(上一行,当前行,下一行)，行先去除 `\r` 与行尾空白) → 16 字符零混淆字母表 `ZPMQVRWSNKTXJBYH`，默认 2 字符（可配 2–4）。重复内容不同上下文不同号；编辑行 N 仅使 N−1/N/N+1 锚点失效。
- read 格式 `行号#锚|内容`（`raw=true` 输出原文）。
- edit：`edits[{op,pos,end?,lines}]`，op∈{replace, insert_before, insert_after, delete}（delete 的 schema 无 lines）；可选 op=replace_text{oldText,newText}（唯一命中否则失败，开关默认开）。批量对同一编辑前快照校验→自下而上应用→行号跨段冻结；任一发散：先 3-way 快照恢复，失败整批拒绝 `E_STALE_ANCHOR`（附当前锚点表）；`lines` 含显示前缀/diff 标记→`E_INVALID_PATCH`；连续 3 次相同空操作→`E_NOOP_LOOP`；成功后返回 unified diff + `--- Anchors A-B ---` 新锚点；temp+rename 原子落盘（symlink/权限保留）。`dry_run=true` 只回 diff。配置 `edit.max_ops_per_call`（默认不限，=1 即降级单条）。
- write：不存在→原子创建；已存在→末尾追加并返回 **"文件已存在，内容已追加"**+起始行号/字节数；`overwrite=true` 原子替换。

### 4.4 代码智能（单一 `lsp` 工具）
- 路由：有可用 server→L1 实时（`match:"lsp-precise"`）；无/降级→L0 tree-sitter 索引透明兜底（`match:"name-based"`）。server warmup 未完时返回 L0 结果+`warmup_hint`。
- LSP 客户端：stdio JSON-RPC 状态机（lsp-types）；按需 didOpen、edit/write 后 didChange；`$/cancelRequest`；LspSupervisor：每语言一进程（workspace=session_cwd）、崩溃退避重启≤3 后置 degraded 转 L0、空闲 10min 回收、纳入 ProcRegistry 进程组。
- 自动诊断：edit/write 后等待该文件 publishDiagnostics ≤2s，error 级附入工具结果（无错附 "no diagnostics"）；`diagnostics` op 用于查询未编辑文件（非冗余）。
- rename：默认返回跨文件 WorkspaceEdit 预览（dry-run，磁盘零变化）；`apply=true` 原子应用并逐文件返回 diff。无审批环节。
- 自动下载（LSP Manager）：解析链 配置覆盖→PATH→本地缓存(`~/.local/share/rustedharness/lsp/<id>/<ver>/`)→下载。下载=首次同意卡片（非交互模式禁自动下载、直接走 L0）+仅 HTTPS+注册表官方域名白名单+SHA-256 强制+版本固定；`rustedharness lsp list/install/update`；release 压缩包优先、包管理器兜底；缺失 runtime 依赖给一次性指引。
- L0：ignore 遍历→language-pack 解析→`.scm` 抽 defs/refs→SQLite（files/defs/refs）；notify+写后 InputEdit 增量；git 分支切换批刷；未就绪时 name 查询降级 grep。tree-sitter 局限（纯语法无语义、同名/宏/动态语言盲区、无诊断、query 维护成本）正是 L1 优先、L0 如实标注的依据。

#### 语言服务器注册表（内置 JSON，含 version/sha256/渠道/依赖字段；⚠️=实现期核验转正）

| 语言 | Server | 渠道 | 依赖 |
|---|---|---|---|
| Rust | rust-analyzer | github.com/rust-lang/rust-analyzer/releases（三平台包） | — |
| C/C++ | clangd | github.com/clangd/clangd/releases（独立 zip）；releases.llvm.org | — |
| Go | gopls | `go install golang.org/x/tools/gopls@latest` | Go |
| TS/JS | typescript-language-server | `npm i -g typescript typescript-language-server` | Node |
| Python | pyright | `npm i -g pyright`（`pyright-langserver --stdio`）；备选 pip python-lsp-server | Node/Python |
| Java | jdtls | download.eclipse.org/jdtls/milestones/（tar.gz） | JDK17+ |
| Kotlin | kotlin-lsp | github.com/Kotlin/kotlin-lsp/releases（zip 自带 sha256）；备选 fwcd/kotlin-language-server | JDK21+/11+ |
| C# | csharp-ls | `dotnet tool install --global csharp-ls`（NuGet） | .NET SDK |
| Ruby | ruby-lsp | `gem install ruby-lsp`（github.com/Shopify/ruby-lsp）；备选 solargraph | Ruby |
| PHP | ⚠️phpactor / intelephense | github.com/phpactor/language-server（渠道+许可待核验） | PHP |
| Swift | sourcekit-lsp | 随 Swift 工具链（swift.org/download；macOS 随 Xcode） | Swift |
| Scala | metals | scalameta.org/metals（coursier） | JDK |
| Lua | lua-language-server | github.com/LuaLS/lua-language-server/releases（三平台 zip） | — |
| Elixir | elixir-ls | github.com/elixir-lsp/elixir-ls/releases（zip） | Elixir |
| Haskell | haskell-language-server | github.com/haskell/haskell-language-server/releases；`ghcup install hls`（匹配 GHC） | GHC |
| Shell | bash-language-server | `npm i -g bash-language-server` | Node |
| YAML | yaml-language-server | `npm i -g yaml-language-server` | Node |
| HTML/CSS/JSON | vscode-langservers-extracted | `npm i -g vscode-langservers-extracted`（一套四 server） | Node |
| ⚠️ | Dart(`dart language-server`)/Zig(zls)/OCaml(opam)/R(languageserver)/Fortran(fortls)/Dockerfile(npm) | 实现期逐项核验 | 对应 SDK |

### 4.5 模型路由与压缩
- `models.agent`/`models.compact`；≥80% 窗口触发压缩：五字段结构化摘要（目标/已完成/关键约束/待办/踩坑），**摘要调用走 compact**；保留最近 N 轮。
- 计数：tiktoken-rs；未知模型字符估算+以 API usage 回校，留 10–15% 余量。前端可热切换 agent 模型（历史按 Adapter 转换保留）。

### 4.6 联网
- `web_search`：brave→tavily→duckduckgo 可用性回退（有 key 优先，DDG 免 key 兜底）；allowed/blocked_domains 结果侧统一二次过滤；后续深读交给 `read`(URL)。
- `read`(URL)：GET（UA、重定向≤5、超时、仅 http/https）→html-to-markdown→截断→moka TTL 缓存→给了 `prompt` 且启用时交 compact 模型提取（主模型不触原始 HTML）；外部内容 `<external_content url=...>` 包裹标注不可信；JS 渲染空页诚实报错。

### 4.7 命令执行（v1.7：pty 捕获根治进度条刷屏）

- **工具分流注册**：POSIX 注册 `bash`；Windows 注册 `powershell`（探测 pwsh.exe→powershell.exe，描述带版本）；仅 `shell.prefer="git-bash"` 且探到 Git Bash 才在 Windows 用 `bash`。命令工具名任何平台唯一。
- **ShellSpec 同源**：系统提示词 `<env>` 段（OS/shell/版本/≤8 行语法速查：分隔符、`$env:`、引号、here-string）与工具 description 由同一 ShellSpec 生成，单测断言恒等（封堵 Claude Code #51788 类 prompt/注册失同步 bug）。
- **bashism 拦截**：执行前规则检测典型 bash 痕迹（5.1 的 `&&`、`/dev/null`、`rm -rf`、heredoc、正斜杠路径），命中直接返回"当前是 PowerShell 7.x，等价写法：…"，不先跑一次注定失败的命令。
- **pty 捕获（根治层）**：
  1. 每条命令经 `portable-pty` 分配伪终端（Windows 用 ConPTY）启动，`TERM=xterm-256color`，默认 200×50。
  2. 程序因此走终端语义：tqdm/curl/pip/npm/cargo 的进度条以 `\r` 与光标移动**原地刷新**，不再逐帧另起新行。
  3. 进程内**终端仿真器**维护屏幕行终态（候选 `vt100`/`tui-emulator`，不合格则自研转义状态机）：重绘、行内覆盖、光标上移动画统一收敛为每行最终文本。
  4. 模型侧回传=仿真屏终态行（进度条恒为一行）+exit_code；缓冲另存原始字节流供前端渲染真动画与回放；`capture="raw"` 可取原始字节（diff/校验和类输出用）。
  5. activity 超时以"屏幕变更"为活动信号；stdout/stderr 合流是终端固有语义（与人工观察一致，omp 亦走 pty），如需分轨用 shell 重定向。
  6. pty 分配失败→回退管道捕获+`\r` 行内合并（降级路径保留）。
- **执行规格**：子进程=current_dir session_cwd + 独立进程组（Windows Job Object）；三类超时 min/max/activity（`timeout_ms`），到期整组 SIGTERM→宽限→SIGKILL；`run_in_background=true` 立即返回 pid，pty 输出持续进 per-pid 缓冲（1MB）；`await(pid, timeout_ms)` 完成或超时均回传净化后的当前输出+status+exit_code(完成)+elapsed_ms+sanitize_stats，超时不杀进程；终止走同 shell 系统命令（`kill -TERM -<pid>` / `taskkill /PID <pid> /T`）；会话结束级联清理全部存活进程组。
- 编码卫生：powershell 前置 UTF-8；read 已去 `\r`。v2 评估：pty 交互态（REPL 输入注入）。

### 4.8 ToDo 与催办
- `todo(items?, merge?=false)`：省略 items=查询；items=全量替换（模型自维护 status）；merge=true 按 id 增量；任何调用返回完整清单；实时推前端。
- 催办：回合结束且无待执行工具且存在 pending/in_progress→注入用户角色消息（完整清单+"任务尚未全部完成，请继续执行"）；同一停转事件 ≤3 次，恢复工作重置，超限转人工；空/全完成不触发；催办计 token、可被压缩。

### 4.9 MCP
- rmcp client；`mcp.json`（兼容 Claude/Cursor 格式）；stdio 子进程+Streamable HTTP；工具注入 `mcp__<server>__<tool>`（重名内置优先）；崩溃/超时隔离在 server 进程（崩溃仅下线该 server 工具）；v1.6 一律 allow，deny 黑名单为可选配置（默认空）；`tools/list_changed` 动态刷新。v2：resources/prompts 消费、反向 server。

### 4.10 三前端
- 共同语义：流式消息（markdown/代码块/工具卡）、diff 卡（非阻断回看）、双模型选择器、cwd 选择、会话列表/恢复、后台进程卡（pty 画面回放）、todo 面板（Ctrl+T）、LSP 状态面板（absent/downloading/running/degraded+warmup）、下载同意卡（供应链闸门，独立于工具权限）、中断级联清理。
- TUI：ratatui+crossterm，80 列稳定。WebUI：`rustedharness serve` REST+WS，断线补帧，API key 不出服务端。Tauri：复用 Web 前端，IPC 桥接，包 <15MB。

## 5. 工具目录（一律 allow；唯一拦截=R4 路径边界）

| # | 工具 | 参数 | 行为 | 返回 |
|---|---|---|---|---|
| 1 | `read` | `path, offset?=1, limit?, raw?=false, prompt?, max_bytes?=100000, use_compact_model?=true, cache_ttl?=900s` | 文件→`行号#锚\|内容` 窗口（raw 去前缀），二进制拒绝；目录→条目；URL→4.6 管线 | 内容窗口+总行数 / 条目 / 提取结果+cache_hit |
| 2 | `write` | `path, content, overwrite?=false` | 4.3 三态（创建/追加/覆盖） | 动作+行号区间+字节数 |
| 3 | `edit` | `path, edits[{op,pos,end?,lines}], dry_run?=false` | 4.3 Hashline 批量补丁全套校验 | diff+新锚点+诊断摘要 / 错误码 |
| 4 | `bash`/`powershell` | `command, run_in_background?=false, timeout_ms?=30000` | 4.7：pty 捕获、进程组、三类超时、bashism 拦截、ShellSpec 同源 | 前台：exit_code+仿真屏终态文本+truncated+stats；后台：pid+status |
| 5 | `await` | `pid, timeout_ms` | 等待/取快照，双结局回传净化输出；超时不杀；未登记 pid 报错列存活 | output+status+exit_code?+elapsed+stats |
| 6 | `grep` | `pattern, path?=., glob?, case_insensitive?, context?(0-5), limit?=50(max 200), offset?, output_mode?=content\|files_with_matches\|count, anchors?=false` | 进程内 ripgrep；gitignore；anchors=true 输出行锚点（直接喂 edit） | `file:line:内容`+总数 |
| 7 | `glob` | `pattern, path?=., limit?` | gitignore 路径模式匹配，mtime 排序（结构发现用，与 grep 互补） | 文件列表 |
| 8 | `lsp` | `op∈{definition,type_definition,implementation,references,hover,document_symbols,workspace_symbols,call_hierarchy,rename,diagnostics}, path, line?, column?, symbol?, apply?(rename), timeout_ms?=5000, limit?` | 4.4 L1→L0 路由；rename 默认 dry-run/`apply=true` 即应用 | 结构化结果（`file:line:col`+match 标注） |
| 9 | `web_search` | `query, num_results?=5, allowed_domains?, blocked_domains?, provider?="auto"` | 4.6 回退链+统一域名过滤 | `# \| title \| url \| snippet \| source \| time` |
| 10 | `todo` | `items?[{id?,content,status}], merge?=false` | 4.8 清单+停转催办 | 当前完整清单 |
| 11 | `mcp__<server>__<tool>` | server schema 透传 | 4.9 | server 返回 |
| — | v2 预留 | — | `ask`、`ast_edit`、`apply_patch`、browser、pty 交互、MCP server 角色、SCIP | — |

## 6. 用户故事（执行序）

1. **US-001** 五 crate workspace+clap CLI：三平台 build、clippy -D 干净。
2. **US-002** Provider 层+Adapter 开关：wiremock 全量往返；Ollama+云端各一次工具闭环。
3. **US-003** Agent 循环+schemars 注册表+取消（v1.6：无审批钩子断言；越界拒绝、界内零打断）。
4. **US-004** WorkspacePathResolver R1–R4：路径四类单测；`cd` 后 `pwd` 仍 session_cwd。
5. **US-005** read(锚点+raw)/grep(anchors/literal/context)/glob(path 作用域)：anchors 直喂 edit e2e；10 万文件 grep<3s。
6. **US-006** write 三态：追加返回"文件已存在，内容已追加"+行号；无静默覆盖。
7. **US-007** Hashline 编解码：黄金样本（CRLF/Tab/UTF-8/重复行异上下文异号）；N±1 失效断言。
8. **US-008** edit edits[] 补丁：四 op+replace_text 各≥5 场景首试成功；发散整批拒绝零脏写；E_INVALID_PATCH/E_NOOP_LOOP；批量 vs 单条 eval+max_ops_per_call；成功后诊断回填。
9. **US-009** L0 CodeIndex：批1 Rust/Python/TS/JS/TSX/Go、批2 Java/C/C++/C#/Ruby/PHP/Kotlin/Swift；2k 文件<60s；名称级一致率≥95%。
10. **US-010** `lsp` 10 op 契约+L1→L0 路由：mock server 全 op；Java 3 重载/7 调用点双路径 fixture 全中；match 标注断言；工具面无 symbol_* 名字（schema 断言）；1k 行 document_symbols<200 token。
11. **US-011** L0 增量刷新：写后 1s、分支 5s、2h 浸泡稳定。
12. **US-012** LspClient+Supervisor：rust-analyzer/pyright 真机闭环；kill -9 自动重启恢复会话；3 崩降级；shutdown 级联杀组。
13. **US-013** ServerRegistry+自动下载：清环境全链路（同意→下载→SHA-256→启动→答 definition）；篡改校验和拒装；非交互禁下载走 L0；依赖缺失一次性指引；⚠️ 项核验转正。
14. **US-014** 诊断注入+rename 流：类型错误 fixture 入 edit 结果；rename dry-run 零落盘/apply=true 原子跨文件；lsp_unavailable 结构化。
15. **US-015** tiktoken+压缩：100 轮不溢出；硬约束保留断言 100%。
16. **US-016** 角色路由：压缩请求体 model==compact 断言；热切换生效。
17. **US-017** 平台 shell+pty 捕获：Windows 默认无 bash 名工具；`<env>`==注册解释器断言；bashism 先拦后教；pwsh7 优先/5.1 速查注入；**tqdm 5000-iter→模型侧单行进度+收尾行；curl/pip fixture 单行、零 ANSI 残留；raw 字节完整可回放**；会话退出无残留进程。
18. **US-018** await：mid-run 快照非空且进度折叠；超时进程存活；完成带 exit_code；stats 恒在。
19. **US-019** todo+催办：半途而废 fixture 被催活；3 次封顶转人工；merge 增量；--resume 存活。
20. **US-020** web_search：三 provider mock+冒烟；零 key DDG 兜底出结果。
21. **US-021** read(URL) 管线：cache_hit；prompt 路径主模型不见原始 HTML；无独立 web_fetch。
22. **US-022** MCP client：server-everything stdio+一个远程 HTTP 各 list+call；崩溃仅下线其工具；deny 条目生效、默认空不拦。
23. **US-023** HarnessEvent+TUI：全工具闭环；中断级联；80 列；LSP 面板与下载同意卡；后台进程 pty 回放。
24. **US-024** WebUI serve：全闭环；双标签不串流；补帧。
25. **US-025** Tauri GUI：三平台闭环；<15MB；panic 可恢复提示。
26. **US-026** 会话持久化+恢复：kill -9 恢复到末条；旧锚按磁盘拒绝；todo/进程/ server 版本固定入快照。
27. **US-027** 文档+注册表转正+CI：工具文档与第 5 节一致；三平台 CI 绿。

## 7. 决策记录（关键不可逆项）

- 无持久 cd（防 agent 目录漂移）；bash→powershell 平台分流命名。
- write 追加而非报错；edit 批量补丁+四 op（对齐 omp/pi-hashline-edit 规范，max_ops_per_call 兜底弱模型）。
- LSP 一等公民+运行时自动下载（同意+SHA-256+域名白名单）；tree-sitter 降为 L0 兜底与 outline。
- symbol×3 并入 `lsp` 元工具；web_fetch 并入 `read`(URL)。
- 权限模型：全部工具一律 allow，仅 R4 路径边界拦截；diff 卡非阻断；LSP 下载同意保留（供应链闸门）；git 回滚为兜底，cap-std 沙箱 v2 评估。
- 命令捕获走 pty/ConPTY+终端仿真（进度条原地刷新，模型侧恒为屏终态文本）；pty 失败回退管道+CR 合并；stdout/stderr 合流为终端固有语义。
- 模型双角色 agent/compact；todo 催办 ≤3 次/停转事件。

## 8. 版本

v1.7（pty 捕获替代五步净化管线；全文精简）← v1.6（取消工具审批；4.7.3 净化管线）← v1.5（平台 shell 分流）← v1.4（symbol 并入 lsp；命名对齐主流）← v1.3（LSP 一等公民+自动下载）← v1.2/1.1/1.0。

---

内容由 AI 生成
