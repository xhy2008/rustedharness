# TODO / 交接文档（2026-11-08）

## 已完成：会话文件惰性创建（2026-11-08，本轮已实现并提交）

用户需求：**在用户发送第一条消息之前，不得创建会话 jsonl 文件**。

实现方式（`crates/core/src/session.rs`）：

1. `SessionWriter::create()` 只生成 id/path 并 `create_dir_all`；Meta 记录暂存在 `header: Vec<SessionRecord>`，不打开文件、不写盘、不 prune。
2. 首个 `Message` 记录到达时 `open()`：创建文件 → 按序写入 header → `prune(max_saved)` → 写本条。首条消息之前到达的其他记录（`/model` 的 `Models`）只进 header。
3. `resume()` 直接 `file = Some(..)`、`header` 空。
4. 后续清理（同一轮规范扫描）删除了此前为“空会话”加的全部补偿逻辑：`shutdown` 删文件、prune 空文件探测、`SessionStore::list` 空会话过滤，以及未使用的 `SessionWriter::summary` / `process` / `notice` 和对应记录变体。
5. 新增测试：`create_writes_nothing_to_disk`、`header_records_precede_the_first_message`。

实机验证：`target/debug/rustedharness.exe run` 保持运行 3 秒期间与退出后，`~/.rustedharness/sessions` 文件数均不变（无新建）。`cargo test --workspace` 全绿。

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
- 校验值解析顺序：GitHub release asset `digest` → 同一 release 附带的 checksums 文件（`SHA256SUMS` / `*.sha256`，支持 GNU `hash  file`、`hash *file`、BSD `SHA256 (file) = hash`）→ `<url>.sha256` sidecar。
- 三处都拿不到校验值时拒绝安装（`E_LSP_CHECKSUM_UNAVAILABLE`），提示可用 `GITHUB_TOKEN` 避免 API 限流，或手动安装。
- `lsp checksum` 保留为只读检查命令；`lsp.downloads.<id>.sha256` 保留为可选覆盖（私有镜像），文档已说明非必需。
- 新增单测 `checksum_files_are_parsed_in_common_formats`、`release_assets_expose_digests_and_checksum_files`。

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
- 状态栏格式：`cwd|model|compact X|ctx 1.2%/1M|in A|out B|CacheHit98%|¥cost`，分隔符无空格。
- resize 防抖：lib.rs event_loop，Resize 风暴期间跳过 draw，静默 150ms 后 clear+强制重绘（RESIZE_SETTLE 常量）。
- 工作区根锁死（target/debug 启动限制）是设计内行为，不要改。
- 不要写依赖网络的实机测试。
- 用户偏好：“少思考，多看代码”；回复用中文；改动必须构建+测试通过并如实汇报。
