# TODO / 交接文档（2026-11-08）

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
7. 顶部 masthead（`EntryKind::Welcome`）：`RustedHarness 0.1.0` / `cwd · model` / 一条 `BORDER_DIM` 分隔线 / 三个快捷键提示。新建会话与切换会话都会重新插入。
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

1. `SessionWriter::create()` 只生成 id/path 并 `create_dir_all`；Meta 记录暂存在 `header: Vec<SessionRecord>`，不打开文件、不写盘、不 prune。
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
cargo test --workspace              # 当前基线：10 套件全绿，core 142 项
git -C E:/rustedharness add -A && \
git -C E:/rustedharness -c user.name="xhy37000616" -c user.email="xhy37000616@163.com" commit -m "..."
```
禁止修改 git 全局/本地配置，只用 -c 参数。提交历史最近几条：
`5044382` 会话惰性创建 → `89f2ac5` fmt/clippy → `6671783` 删空会话补偿 → `e475b3a` 删配置兼容层 → `f4e56ed` 规范扫描 → 本次 LSP/429。

## 关键背景（勿踩）

- TUI 是全屏 + 拖拽复制方案（用户否决过 inline viewport 方案，别再碰）。
- 渲染宽度：`draw()` 全局右留 1 列；UI 内已禁用东亚歧义宽度字符（· … ¥ 除外），新增 UI 字符串只用 ASCII 标点做分隔符/省略号。`¥` 允许保留（cost 字段）。
- UI 字形只能用 WGL4 覆盖集（`● × ○ › │ · ─` + 圆角框），不要再引入 `✔ ✘ ❯ ▏ ╌` 或 braille spinner：Windows 等宽字体没这些字形，会显示成方块。
- 状态栏格式：`cwd|model|compact X|ctx 1.2%/1M|in A|out B|CacheHit98%|¥cost`，分隔符无空格。宽度不足时按 路径→CacheHit→cost→compact 顺序隐藏字段（见 `render::footer_line` 的 layouts 表），不要退回强制截断。
- resize 防抖：lib.rs event_loop，Resize 风暴期间跳过 draw，静默 150ms 后 clear+强制重绘（RESIZE_SETTLE 常量）。
- 工作区根锁死（target/debug 启动限制）是设计内行为，不要改。
- 不要写依赖网络的实机测试。
- 用户偏好：“少思考，多看代码”；回复用中文；改动必须构建+测试通过并如实汇报。
