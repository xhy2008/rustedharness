# TODO / 交接文档（2026-11-08）

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
- 强制重绘剩下的场景都是低频且语义上必要的：resize 稳定后、覆盖层开合、切换会话、Ctrl+G。
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
- 工具输出**永远**挂在调用行下面（`└ 首行 +N`，Ctrl+O/点击展开），不要为结果再建条目；绑定键是 tool call id，不是"最近一个工具"。
- 状态栏格式：`cwd|model|compact X|ctx 1.2%/1M|in A|out B|CacheHit98%|¥cost`，分隔符无空格。宽度不足时按 路径→CacheHit→cost→compact 顺序隐藏字段（见 `render::footer_line` 的 layouts 表），不要退回强制截断。
- resize 防抖：lib.rs event_loop，Resize 风暴期间跳过 draw，静默 150ms 后 clear+强制重绘（RESIZE_SETTLE 常量）。
- 工作区根锁死（target/debug 启动限制）是设计内行为，不要改。
- 不要写依赖网络的实机测试。
- 用户偏好：“少思考，多看代码”；回复用中文；改动必须构建+测试通过并如实汇报。
