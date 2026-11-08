# TODO / 交接文档（2026-11-08）

## 已完成：会话文件惰性创建（2026-11-08，本轮已实现并提交）

用户需求：**在用户发送第一条消息之前，不得创建会话 jsonl 文件**。

实现方式（`crates/core/src/session.rs`）：

1. `SessionWriter::create()` 只生成 id/path 并 `create_dir_all`；Meta 记录暂存在 `header: Vec<SessionRecord>`，不打开文件、不写盘、不 prune。
2. 首个 `Message` 记录到达时 `open()`：创建文件 → 按序写入 header → `prune(max_saved)` → 写本条。首条消息之前到达的其他记录（`/model` 的 `Models`、`Notice` 等）只进 header。
3. `resume()` 直接 `file = Some(..)`、`header` 空。
4. `Agent::shutdown` 的删空文件逻辑保留，作为崩溃/旧数据的兜底。
5. 新增测试：`create_writes_nothing_to_disk`、`header_records_precede_the_first_message`；`pruning_sweeps_empty_sessions_regardless_of_quota` 改为手工写入“只有 Meta 头”的 jsonl 再验证被清扫。

实机验证：`target/debug/rustedharness.exe run` 保持运行 3 秒期间与退出后，`~/.rustedharness/sessions` 文件数均不变（无新建）。`cargo test --workspace` 全绿（core 141 项）。

## 上一轮改动的已知坑（勿回退）

- `Agent::shutdown` 中同步 `MutexGuard<SessionWriter>` 必须用**块作用域** `{ let s = ...lock...; ... }` 包住，不能只靠 `drop(s)`——否则 web crate 的 `on_upgrade` future 报 E0277 非 Send（web/src/lib.rs:512）。
- `self.session.lock()` 建议统一 `unwrap_or_else(|e| e.into_inner())`。

## 验证与提交流程（每个任务必做，用户要求实机验证、禁止虚报成功）

```bash
taskkill /F /IM rustedharness.exe   # 旧进程会锁二进制导致“拒绝访问 os error 5”
cargo build -p rustedharness-cli    # gui 依赖由 web 编译错误间接覆盖
cargo test --workspace              # 当前基线：10 套件全绿，core 139 项
git -C E:/rustedharness add -A && \
git -C E:/rustedharness -c user.name="xhy37000616" -c user.email="xhy37000616@163.com" commit -m "..."
```
禁止修改 git 全局/本地配置，只用 -c 参数。提交历史最近几条：
`f34fc65` 空会话清理（shutdown删除+prune清扫） → `dc7635d` footer收紧+CacheHit+resize防抖 → `fbb1af8` ASCII分隔符修宽度。

## 关键背景（勿踩）

- TUI 是全屏 + 拖拽复制方案（用户否决过 inline viewport 方案，别再碰）。
- 渲染宽度：`draw()` 全局右留 1 列；UI 内已禁用东亚歧义宽度字符（· … ¥ 除外），新增 UI 字符串只用 ASCII 标点做分隔符/省略号。`¥` 允许保留（cost 字段）。
- 状态栏格式：`cwd|model|compact X|ctx 1.2%/1M|in A|out B|CacheHit98%|¥cost`，分隔符无空格。
- resize 防抖：lib.rs event_loop，Resize 风暴期间跳过 draw，静默 150ms 后 clear+强制重绘（RESIZE_SETTLE 常量）。
- 工作区根锁死（target/debug 启动限制）是设计内行为，不要改。
- 不要写依赖网络的实机测试。
- 用户偏好：“少思考，多看代码”；回复用中文；改动必须构建+测试通过并如实汇报。
