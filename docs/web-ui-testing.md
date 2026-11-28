# Web UI 测试手册

这份文档是给**在 rustedharness 里工作的会话**看的：拿到一个改动之后，怎么把浏览器端从头到尾
跑一遍，并且知道每一项"什么样算好、什么样算坏"。

脚本是唯一的事实来源，本文件解释它为什么这么断言：

```
script/web-ui-test.py     # 113 项检查，80 秒跑完，失败时退出码 1
script/fake-openai.py     # 本地假 provider，不需要任何 API key
```

## 0. 一句话

```bash
cargo build -p rustedharness-cli && python3 script/web-ui-test.py
```

末尾打印 `113/113 通过`，退出码 0，就是全绿。失败时逐条列出 `<实际值>`。

## 1. 准备

| 需要 | 检查方式 |
| --- | --- |
| `target/debug/rustedharness` | 脚本启动时检查，缺失会直接提示 `cargo build` |
| Python + `playwright` | `python3 -c "import playwright"` |
| Chromium（Playwright 自带） | `python3 -m playwright install chromium` |
| `git` | 文件树的 M/U/I 标记要用 |

不需要 provider、不需要网络、不需要真终端——脚本自己起三个进程。

## 2. 脚本自己启动的东西

| 端口 | 进程 | 用途 |
| --- | --- | --- |
| 7497 | `script/fake-openai.py` | OpenAI 兼容 SSE 假 provider，模型 `fake/fake-a`、`fake/fake-b` |
| 7499 | `rustedharness serve` | 被测服务，无令牌 |
| 7498 | `rustedharness serve` | 被测服务，`RUSTEDHARNESS_TOKEN=s3cret`，测 401 |

工作目录 `/tmp/rh-ui-test/`（每次运行前清空重建）：

```
/tmp/rh-ui-test/config.json    # model / compactModel / search 等
/tmp/rh-ui-test/models.json    # provider 指向 7497
/tmp/rh-ui-test/ws/            # fixture 工作区（见第 4 节）
/tmp/rh-ui-test/stub.log       # 假 provider 收到的每个请求体（JSON 行）
/tmp/rh-ui-test/server.log     # 被测服务的输出
```

常用参数：

```bash
python3 script/web-ui-test.py --area feed      # 只跑一组
python3 script/web-ui-test.py --headed         # 用眼睛看着它点
python3 script/web-ui-test.py --keep           # 跑完不杀进程，用浏览器手工接着看
python3 script/web-ui-test.py --no-start       # 复用已经在 7499/7498 跑着的服务
```

`--keep` 之后可以直接开 `http://127.0.0.1:7499/?debug=1` 手工复查失败的那一项。

## 3. 假 provider：怎么让某条路径发生

测试不靠"等一个真模型"，而是靠 prompt 的第一个词选场景（输入框里照打就行）：

| 首词 | 行为 | 用来测 |
| --- | --- | --- |
| `hello` 等任意词 | 一段普通文本，逐块流式返回 | 基本回合、Markdown、统计 |
| `reason` | 先 `reasoning_content` 再 `content` | 思考块折叠、字数 |
| `tool` | 一次 `bash` 工具调用；拿到结果后再回一段文本 | 工具卡片：运行中/完成/输出/折叠 |
| `todo` | 一次 `todo` 工具调用（3 项不同状态） | 待办卡片、chase |
| `edit` | 一次 `edit` 工具调用（`files` 形状） | 工具卡片参数渲染 |
| `slow` | 每 400 ms 一块，持续约 10 秒 | 停止按钮、排队 |
| `long` | 200 块长文本（约 40 KB） | 虚拟列表、跟随滚动 |
| `usage` | 末尾带 usage（含 cached_tokens） | token 统计、缓存命中率 |
| `rate` | 第一次请求返回 429，之后正常 | 429 重试（固定 3 秒间隔） |
| `boom` | HTTP 500 | 错误卡片 |
| `silent` | 只回响应头，然后不说话 | 空闲超时、长时间等待 |

**断言要断言服务端收到了什么**，不要只断言界面状态。`/tmp/rh-ui-test/stub.log` 每行是
`{"path": ..., "body": ...}`，例如 F3 用它证明"切换模型后下一次请求真的换了 model id"，
而不是"chip 上的字变了"。

## 4. fixture 工作区

| 文件 | 存在理由 |
| --- | --- |
| `README.md`（已提交后又被改动） | git `M`、Markdown 预览 |
| `src/main.rs` | 行号、语法高亮、目录展开 |
| `src/extra.txt`（提交后新建） | git `U`（未跟踪） |
| `build/out.bin` + `.gitignore` | git `I`（忽略）、"点目录也要显示" |
| `.hidden.md` | 文件树不过滤点文件 |
| `data.json`（单行） | 预览要格式化 |
| `page.html`（含 `<script>`） | HTML 预览必须被禁用 |
| `binary.bin`（含 NUL） | 二进制提示、复制按钮禁用 |
| `big.txt`（300 KB） | 256 KiB 截断提示 |
| `.rustedharness/`（服务自己建的） | 会话索引；它自带 `.gitignore`，所以在树里是 `I` |

`/tmp/rh-ui-test/ws` 里的 `git status --porcelain=v1 -z --ignored=matching` 必须同时出现
` M README.md`、`?? src/extra.txt`、`!! build/`——这是 H2 的前提。fixture 每次重建，改动它
要保证这三点仍然成立。

## 5. `?debug=1` 钩子

只有带 `?debug=1` 时页面才挂 `window.__rh`：

| 钩子 | 用途 |
| --- | --- |
| `__rh.state` | 读真实状态（feed 条目、busy、queue、terminal、lsp、settings…） |
| `__rh.handleEvent(event)` | **就是 socket 调用的那个函数**，把一个服务端事件灌进去 |
| `__rh.applySnapshot(snapshot)` | 灌一份完整快照（2000 条消息的虚拟列表测试） |
| `__rh.feedSync()` / `__rh.feedPush()` / `__rh.feedInvalidate()` | 直接驱动列表布局 |
| `__rh.clearFeed()` / `__rh.showEmptyState()` | 清空 / 空状态 |
| `__rh.renderSnapshotMessage()` | 单条快照消息 |

`handleEvent` 很重要：有些事件服务端很难按需制造（下载同意、LSP 降级、usage、chase、
compaction、进程退出）。灌事件走的是**与真实路径完全相同的代码**，所以能测到渲染与状态，
但**测不到服务端行为**——例如灌 `download_consent` 再去点"允许"，只会证明前端把
`download_consent` 命令发出去并写了提示，不会真的下载。这类边界在第 11 节列清楚。

## 6. 检查清单

共 113 项。每项都满足一个条件：**把对应代码改坏，它必须报红**（第 9 节给了反向验证的方法）。

### A 启动与连接

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| A1 | 加载页面 | 零 `pageerror` | 控制台报错（比如某个 `__rh` 调用 NPE） |
| A2 | 连接状态 | `#statusText` 与 `#footStatus` 都是"已连接" | 一个变了一个没变，或停在"连接中…" |
| A3 | 配置已应用 | `#wsPathText` = 工作目录，模型 chip = `fake/fake-a` | `/api/config` 失败被静默吞掉，chip 停在 `—` |
| A4 | 会话 chip | 显示 session uuid 前 8 位 | 显示 `—` 或整个 uuid |
| A5 | 点会话 chip | 切到"会话"标签、左侧展开 | 没有任何反应 |
| A6 | 主题 | dark → light（`data-theme`、`localStorage.rh_theme`、`#themeColor` 三处同时变）再切回 | 只有图标变；刷新后回到深色却仍是浅色样式 |
| A7 | 快捷键面板 | 6 行、Esc 关闭 | 行数变了（说明清单与实现漂移） |
| A8 | 空状态提示词 | 点一下填进输入框 | 只填了一半 / 没反应 |
| A9 | 标题与品牌 | `<title>` 含 RustedHarness | — |

### B 输入与回合

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| B1 | Enter | 发送、输入框清空 | 发送了但不清空（会重复发送） |
| B2 | Shift+Enter | 换行、不发送 | 直接发出去（会切掉半句话） |
| B3 | 空输入 | Enter 无反应 | 生成一条空消息 |
| B4 | 斜杠 | `/hello` 作为普通文本进消息流 | 被当命令吞掉（旧实现的行为） |
| B5 | 运行中 | 按钮变"停止"、带 `stop` class | 仍显示"发送"或可点两次 |
| B6 | 排队 | 出现"已排队（1）"，`state.queue` 长度 1 | 静默丢弃，或直接并发发出去 |
| B7 | 队列放行 | 回合结束出现"发送排队消息"并真的发送 | 永远卡在队列里 |
| B8 | 停止 | 点停止后 `busy=false` 且助手内容不再增长 | 停止无效（内容继续涨），或连接被关掉 |
| B9 | 自增高 | 单行 < 多行 ≤ 170 px | 无限变高，把会话挤没 |

### C 消息渲染

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| C1 | 用户气泡 | `.msg.user .bubble` + "你" | 没有 meta 或气泡被当 Markdown 渲染 |
| C2 | Agent 卡片 | 头像 + 模型标签 = 当前模型 | 标签还是旧模型（说明没跟着 `set_agent_model` 走） |
| C3 | 思考块 | 完成后 `open=false`，字数含"字" | 一直展开；字数不更新 |
| C4 | 工具卡片 | 名称 `bash`、状态"完成"、输出含工具真实输出 | 状态永远"运行中"；输出为空 |
| C5/C6 | 折叠 | 完成后默认折叠，点摘要展开 | 一直展开（长会话被工具输出淹没） |
| C7 | provider 错误 | `.error-card` 含服务端原文 `stub failure` | 错误被吞掉，只在 toast 里闪一下 |
| C8 | 压缩卡片 | 含"丢弃 7 条"与摘要正文 | 只显示"丢弃 0 条"或摘要为空 |
| C9 | Markdown | h2 / li / strong / `a[target=_blank]` / 代码块语言各一份 | 整段变纯文本（渲染器异常被 catch 吃掉） |
| C10 | `javascript:` 链接 | 保持纯文本 | 变成可点击 `<a>` —— **安全问题** |
| C11 | 代码块复制 | 剪贴板等于代码、按钮显示"已复制" | 复制到空串；按钮不变 |
| C12 | 用户右键 | 恰好两项：回滚 / 复制 | 多出"删除"等没有实现的项 |
| C13 | 复制消息 | 剪贴板等于该条文本 | 复制到全部消息 |
| C14 | Agent 右键 | 只有"复制这条回复" | 出现"回滚"（回滚只对用户输入有意义） |
| C15 | 流式期间上滚 | 上滚后滚动位置保持在原地（≤ 340 px） | 被拽回底部——用户没法读历史 |
| C16 | 在底部时 | 新消息跟随（距底 < 120 px） | 不跟随（用户以为卡住） |
| C17 | 2000 条消息 | 挂载 < 120 个节点、落在底部 | 全量挂载（几千节点，滚动卡顿） |
| C18 | 布局一致性 | 每个挂载节点的 `top` 等于自己的 offset | 出现重叠/空隙（前缀和与高度不同步） |
| C19 | 滚到顶部 | 第一条 `top=0` | 顶到一半看到空白 |

### D 回滚

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| D0 | 右键可点 | 菜单项真的在最上层、可被点中 | 被遮挡（有透明层覆盖） |
| D1 | 对话框 | "保留：#2 …rollback two" | 序号差 1（用户输入与轮次索引混淆） |
| D2 | 确认 | 只剩 2 条用户消息，最后一条是选中那条 | 删多了（把选中的也删了）或删少了 |
| D3 | 取消 | 关窗、消息不变 | 关窗但已经删了 |

### E 设置

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| E1 | 结构 | 4 个分组；会话页 3 行 + 令牌框只在会话页 | 令牌框跑到"通用"页；行数变了（服务端描述与前端不一致） |
| E2 | 开关 | 点击后 `aria-checked` 与 `config.json` 同时变 | 只变界面不落盘，或落盘了界面不回读 |
| E3 | 数字 | `rateLimitRetries` 写成数字 `30`，选中项显示"30 次" | 写入字符串 `"30"`（服务端报"expects a number"） |
| E4 | 像素 | `image.maxEdge = 1024` | 写入标签 `"1024 px"` |
| E5 | 压缩 | `compactThinking=high`、`compactThreshold=0.9`、`keepRecentTurns=4` | 阈值写成 `90`（而不是 0.9） |
| E6 | 联动 | 关掉裁剪后"完整发送最近轮次"变灰 | 仍可点（会发一个被忽略的值） |
| E7 | 搜索 | 切到 searxng 出现"实例地址"；切回 duckduckgo 显示禁用的"凭据" | 后端变了但凭据行不刷新 |
| E8 | 关闭 | Esc 与 X 都能关 | 只有其中一个 |
| E9 | 令牌不变 | 提示"令牌未变化"且不断线 | 无谓地重连（会丢当前回合） |

### F 模型

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| F1 | 下拉 | 列出全部模型，当前项有 `sel` | 一个都没有 / 当前项没标 |
| F2 | 过滤 | 只剩匹配项 | 过滤不生效 |
| F3 | 切换 | 下一次请求的 `body.model` 是新的 id；`config.json` 的默认模型**不变** | 请求仍用旧模型；或把会话选择写进默认配置 |
| F4 | 关闭 | Esc 与点空白都关 | 菜单赖着不走 |
| F5 | 压缩模型 | 第一行 `None · 复用主模型 · 命中 prompt 缓存` | 没有 None（无法回到复用主模型） |
| F6 | 空 URL | 提示"请填写 Base URL" | 发出空请求 |
| F7 | 拉取 | 第二步列出 `/models` 返回的两项 | 卡在"正在拉取…"（`model_catalog` 没处理） |
| F8 | 写入 | `models.json` 出现该模型，toast 含 `models.json` | 提示成功但文件没变 |

### G 工作目录

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| G1 | 列表 | 显示 `src/`、`build/`（隐藏目录也不过滤） | 过滤掉点目录（旧行为） |
| G2 | 导航 | 进入子目录、上一级回来 | 面包屑不动 / 上一级死循环 |
| G3 | 选择 | toast + `#wsPathText` 更新 + 文件树重读 | 只有一个变了（状态分叉） |
| G4 | Esc | 关闭 | — |

### H 文件树

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| H1 | 内容 | `.hidden.md`、`big.txt`、`build` 都在 | 少了任意一个 |
| H2 | git 状态 | 字母集合恰好 `{I, M, U}`，**不只靠颜色** | 只有颜色（色盲不可读）；或总是 `I`（canonical 路径不匹配，macOS/Windows 会全丢） |
| H3 | 懒加载 | 展开一次请求；折叠再展开不重复请求 | 每次点击都请求（大目录卡） |
| H4 | 刷新 | 重新读取 | — |

### I 查看器

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| I1 | 代码视图 | ≥14 行、行号从 1、关键字高亮、meta 含"行" | 无行号；整篇不高亮（说明走了纯文本兜底） |
| I2 | 高亮同步 | 文件树里当前文件只有 1 个 `.active` | 多个或没有 |
| I3 | Markdown | 自动切到"预览"且渲染出标题/列表 | 停在代码页 |
| I4 | JSON | `"name": "fixture"` 带缩进 | 原样单行 |
| I5 | HTML | "HTML 预览已禁用" + 按钮跳到源码 | 执行了脚本（**安全问题**） |
| I6 | 二进制 | meta 含"二进制"、复制禁用 | 复制按钮可点（会复制乱码） |
| I7 | 大文件 | 含"仅显示前 256 KiB" | 不提示就截断（用户以为文件就这么长） |
| I8 | 复制文件 | 剪贴板有内容 + toast | 复制空串 |
| I9 | 文件消失 | "无法读取文件" + toast，不白屏 | 停在"正在读取…" |
| I10 | 标签页 | code/preview/activity/terminal 四页都能切 | 切了但面板没换 |
| I11 | 折叠 | 折叠后出现边缘把手，点回来 | 折叠后没法展开（死路） |
| I12 | 分隔条 | 拖动改宽，`localStorage.rh_right_w` 记住 | 宽度不落盘 |
| I13 | 报错 | 本区域零 `pageerror` | 渲染路径抛异常 |

### J 活动面板

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| J1 | 会话信息 | 连接/会话/工作区/Agent/Compact/上下文六行 | 缺项或值为空 |
| J2 | LSP 事件 | 注入的语言出现在面板，状态栏 `N 运行` 与 `state.lsp` 一致 | 状态栏数字与面板不一致 |
| J3 | 降级 | 写一条"LSP zig：degraded"提示、点亮徽标、`#lspDot` 变 warn | 徽标不亮（用户不知道代码智能降级了） |
| J4 | 进程 | `pid 4242`、状态栏"进程 1/1" | 状态栏与面板不一致 |
| J5 | LSP 对话框 | 行数 = `state.lsp.size`，至少 2 个"下载" | 停在"正在读取语言服务器…"（渲染时抛异常，见第 8 节） |
| J6 | 进程按钮 | 点状态栏进程 → 切到活动页 | 点了没反应 |

### K 终端

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| K1 | 打开 | `terminal.id > 0` 且屏幕出现提示符 | 一片空白（xterm 没加载/尺寸为 0） |
| K2 | 输入输出 | `echo ui-marker-1` 的输出出现 | 按键没送到 pty（焦点/队列问题） |
| K3 | Ctrl+C | `sleep 30` 被中断，后续命令可用 | 中断无效，shell 卡死 |
| K4 | 清屏 | 回看内容被清掉 | 清了一半 |
| K5 | 重启 | id 变大、旧输出不残留、新命令可用 | 旧输出画进新屏幕（帧没带 id） |
| K6 | 复制 | 未选中时复制整屏 | 复制空串（`terminalText` 没走 selectAll） |
| K7 | 自适应 | 窗口变窄后 `cols` 改变 | 列数不变，换行错乱 |

### L 会话列表

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| L1 | 一致性 | 条数 = `/api/sessions` 条数，恰好 1 个活动 | 条数对不上（缓存没刷） |
| L2/L2b | 刷新与标记 | 刷新后条数不变；活动项文本含当前短 id | 活动标在别的会话上（会误删） |
| L3 | 打开 | 点击后 `sessionId` 切换、历史消息渲染出来 | 点了没反应 / 空白 |
| L4 | 删除他人 | 弹确认，确认后少一条 + toast | 不确认就删 |
| L5 | 删除当前 | 拒绝并提示"当前活动会话无法删除" | 真删了（正在写的会话被拔掉） |

### M 待办 / 统计 / 事件 / 响应式

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| M1 | 待办卡片 | `1/3`，"已完成/进行中/待处理"三种标签 | 计数错 / 状态色与文字不一致 |
| M2 | 折叠记忆 | 折叠后 `rh_todo_closed=1`，刷新仍折叠 | 不记忆 |
| M3 | token 统计 | 46%、`15k/33k`、in 12k、out 678、缓存 42%、$0.0123 | 少一项；或本地估算（本项只认 API 的 usage） |
| M4 | 告警 | ≥85% 时 `#ctxFill` 带 `warn` | 不告警（用户被静默压缩） |
| M5 | 通知前缀 | `model changed to …` 只改 chip + toast，不落消息流；普通 notice 落成提示 | 把模型切换当普通消息刷屏 |
| M6 | 其它事件 | chase / 进程失败 / 下载进度都写成提示，含关键数字 | 事件被 `default:` 丢掉 |
| M7 | 下载同意 | 弹窗含名称/版本/URL/运行时；允许或 Esc 各写一条提示 | 弹窗不出现（LSP 装不上又没有解释） |
| M8 | 模态内的错误 | toast + 错误卡片都出现 | 只在被遮住的背景里显示 |
| M9 | 窄屏 | `#leftToggle` 开抽屉、点遮罩关、Esc 关、右侧同理 | 抽屉关不掉（移动端死锁） |

### N 鉴权

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| N1 | 无令牌 | 打开设置页、提示需要令牌、状态不是"已连接" | 白屏或一直转圈 |
| N2 | 错令牌 | 同样连不上 | 连上了（鉴权被绕过） |
| N3 | 对令牌 | 连上，且 `?token=` 从地址栏消失、存进 localStorage | 令牌留在地址栏（会被截图/历史泄漏） |
| N4 | 带令牌发消息 | 正常渲染 | — |
| N5 | 断线 | 出现"重连"按钮与提示 | 静默断开，用户输入丢失 |

### Z 全程

| ID | 检查 | 好 | 坏 |
| --- | --- | --- | --- |
| Z1 | JS 报错 | 所有页面累计 0 个未捕获异常 | 任何一条 `pageerror` 都是 bug，即使界面"看起来正常" |

## 7. 数字基线

偶发失败时先看这几项（都是 debug 构建、headless Chromium、1600×900）：

| 指标 | 正常范围 |
| --- | --- |
| 全量耗时 | 75–110 s |
| 2000 条消息挂载节点 | 11–40 |
| 一次滚轮（120 px）的 `feedSync` | < 2 ms（20 000 条消息也是） |
| 终端 `echo` 往返 | < 1 s |

虚拟列表与终端相关的深入数字见 `TODO.md` 里"虚拟列表的滚动开销"一节。

## 8. 这套测试抓到的真 bug

不是"跑一遍确认现有行为"，下面每一条都是本套件第一次跑出来的：

1. **流式输出会把用户拽回底部**（C15）。`appendAssistantDelta` 闭包里捕获的 `stick` 在
   用户上滚后仍然为真，一帧之后又把页面拉回底部，且拉回后下一帧继续认为"在底部"，
   形成持续对抗。修法：滚动是唯一决定"是否跟随"的东西（`state.feed.follow`），在 rAF
   里重新读它，而不是用事件到达时的快照。
2. **`lsp_status` 先于 `lsp_servers` 到达会让 LSP 面板崩掉**（J5）。`lsp_status` 只把
   `state`/`detail` 写进 map，没写 `language`；`renderLspDialog` 按 `language.localeCompare`
   排序时抛 `TypeError`，异常发生在 ws 回调里，对话框永远停在"正在读取语言服务器…"。
   修法：状态事件也带上 `language`。
3. **文件树里 git 状态只有颜色、没有字母**（H2）。目录有 `git-mark`，文件没有，色盲用户
   读不到；TUI 是给字母的。修法：文件行也渲染同一个字母。
4. **模型选择器的说明与实际行为不符**（F3）。原文说"每个模型写入 config.json 的 model
   字段"，但 `set_agent_model` 只改会话与内存（TUI 也是如此）。修法是改文案，并让测试去
   断言**下一个请求的 model**，而不是断言配置文件。

## 9. 怎么确认"能测出好坏"

断言能失败才是断言。抽查方法（改坏一处，跑对应区域，应该红）：

| 改坏什么 | 应该失败 |
| --- | --- |
| `settings.rs` 的 `number_value` 只接受 JSON 数字 | E3 |
| `feedSync` 里去掉 `state.feed.follow` 复查 | C15 |
| `lsp_status` 不写 `language` | J5、Z1 |
| 文件行不渲染 `git-mark` | H2 |
| `renderTerminal` 不校验 `event.id` | K5 |
| `constant_time_eq` 换成 `==` / 去掉令牌校验 | N1、N2 |
| `applySnapshot` 后不调用 `scrollBottomInstant` | C17 |
| `decideConsent` 不发命令 | M7 |

新增检查时照这个思路写：**先想清楚哪一个真实缺陷会让它红**，想不出来就说明它只是在
描述现有行为。

## 10. 写新检查时的坑（都踩过）

- **`has_text` 是子串匹配**：`"high"` 会同时匹配 `"xhigh"`。用 `^` 锚定的正则（见
  `click_opt`）。
- **`.filter(has_not=...)` 只看后代**：`.session-item` 的 `.active` 在 `<li>` 自己身上，
  所以要用 `:not(.active)`。
- **在会话区上滚会关掉右键菜单**（这是设计）。测试要先 `scroll_into_view_if_needed()`、
  等 250 ms、再右键；点菜单项要用真实像素坐标（见 `right_click` / `ctx_click`）。
- **`inner_text()` 会拿到 CSS 大写化后的文本**：`.cb-lang` 显示 `PYTHON`，比较时统一小写。
- **标签不是值**：断言配置要读 `config.json`，不要读按钮文字；反过来也不要假设按钮文字
  等于配置值（"30 次" vs `30`）。
- **数字格式**：`fmtTokens(32768)` 是 `33k`，不是 `32.8k`；`#ctxFill.style.width` 是
  `45.7764%`，不是 `46%`。
- **共享状态**：整个套件共用一个服务进程与一个 fixture。新检查要么自己清理，要么放在
  顺序里靠后，并只断言"相对变化"（例如 `state.lsp.size` 前后差 1），不要断言绝对条数。
- **别用固定 `sleep` 等状态**：用 `wait_for_function` 轮询 `__rh.state` 或 DOM，超时就失败，
  这样失败信息里能看到当时的真实值。
- **服务端事件的基线要等它到齐**：打开活动面板会触发 `describe_lsp`，立刻读
  `state.lsp.size` 会读到一半。

## 11. 这套测试测不了什么

诚实清单，遇到下面这些别指望它：

| 测不了 | 原因 | 怎么补 |
| --- | --- | --- |
| 真实 provider 的怪癖（缺 `finish_reason`、usage 缺失、非 UTF-8） | 假 provider 只模拟已知情况 | 用真 key 手测；或在 `fake-openai.py` 加场景 |
| 真的下载/安装 LSP | 需要 HTTPS 白名单与网络 | M7 只验证"同意/拒绝"的界面与命令 |
| Windows 的终端选择行（Git Bash / PowerShell） | 只在 `git_bash_available()` 时出现 | 在 Windows 上跑同一套，E1 的行数会不同（见下） |
| 真 429 退避时序（固定 3 秒一轮） | `rate` 场景只测一次 | 手工把 `rateLimitRetries` 调小观察 |
| 真实压缩（达到阈值后摘要） | 需要真实长上下文 | 手测；C8 只测卡片渲染 |
| 缓存未命中提示的数值 | 需要真实 usage 序列 | M3 只测渲染 |
| GUI（Tauri）与 TUI 的同样功能 | 各自的原生控件 | 分别手测；TUI 有自己的 `cargo test` |
| Safari / Firefox / 真移动设备 | 只用 Playwright Chromium | 需要时手动开一个 |
| 视觉回归（像素级） | 没有基线截图 | `--keep` 后人工看，或以后加截图对比 |

**平台差异**：E1 在 Windows 上会话页之外多一行"终端"，`describe` 的 LSP 语言数量也不同，
所以涉及绝对条数的地方一律用相对断言。

## 12. 与其它测试的关系

| 层 | 命令 | 覆盖 |
| --- | --- | --- |
| Rust 单元/集成 | `cargo test --workspace` | agent、工具、配置、协议、渲染纯函数 |
| 本套件 | `python3 script/web-ui-test.py` | 浏览器里的一切交互 + 真实 ws 协议往返 |
| 手工 | `--keep` 后开浏览器 | 视觉、真 provider、跨平台 |

提交前最低要求仍然是 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets
-- -D warnings`、`cargo test --workspace`；改了浏览器端再加本套件。
