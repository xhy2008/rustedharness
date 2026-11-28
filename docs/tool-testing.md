# 工具测试手册

这份文档回答一个问题：**改动了某个工具之后，怎么证明它的每一种用法都还成立。**

脚本是唯一的事实来源，本文件解释矩阵为什么长这样、每条用例怎样才算"好"：

```
script/tool-test.py       # 102 条用例，约 130 秒，失败退出码 1
script/fake-openai.py     # 既能当模型，也能当 SearXNG（--searxng-port）
```

## 0. 一句话

```bash
cargo build -p rustedharness-cli && python3 script/tool-test.py
```

`102/102 通过` 即全绿。常用：

```bash
python3 script/tool-test.py --list            # 列出全部用例
python3 script/tool-test.py --area edit       # 只跑一个工具
python3 script/tool-test.py --keep            # 保留桩与工作区，手工复查
```

## 1. 为什么走 `run --json` 而不是浏览器

工具是 agent 的一部分，不是 UI 的一部分。`rustedharness run --json "<prompt>"` 会
**一行一个 JSON 事件**打印出来，包括 `tool_call`（工具名 + 参数）和 `tool_result`
（`ok` / `content` / `metadata`），跑完一个回合就退出。于是每条用例就是：

```
用户消息：call {"name": "grep", "arguments": {"pattern": "beta"}}
  → 桩 provider 把它变成一个 tool_call
  → harness 真的执行 grep
  → 脚本断言 tool_result 与磁盘上的文件
```

浏览器测试（`script/web-ui-test.py`）只验证"工具事件怎么显示"；工具**行为**在这里测，
两者不重复。

## 2. 桩 provider 的 `call` 协议

`script/fake-openai.py` 按 prompt 首词选场景。工具测试只用 `call`：

| 写法 | 行为 |
| --- | --- |
| `call {"name":"glob","arguments":{"pattern":"**/*.rs"}}` | 执行这个工具，拿到结果后再回一段文本 |
| `call [{...},{...}]` | 按顺序执行，**一次回合调用一个**，等前一个结果回来再发下一个 |
| 参数里的 `"$pid"` | 替换成上一条 tool 消息里报告的 pid（为了测 `await`） |

为什么要"一次一个"：真实模型看不到结果就无法决定下一个调用，而 `await(pid=…)` 的 pid
只能从 `bash run_in_background` 的结果里读出来。桩模仿这个顺序，于是**一个回合内**可以
测"启动后台进程 → 等它"。

`--searxng-port` 让同一个进程在另一个端口回答 `/search?q=…`：`empty` 返回空结果，
`boom` 返回 500，其它返回三条固定结果。`web_search` 与 `read <url>` 因此不需要网络。

## 3. 工作区与配置

`/tmp/rh-tool-test/`（每次运行重建）：

| 路径 | 用途 |
| --- | --- |
| `ws/src/main.rs` | 多行 Rust 文件：锚点、行号、`lsp` 名字索引、`grep`/`glob` |
| `ws/src/second.rs` | 多文件编辑的第二个文件 |
| `ws/notes.txt` | 三行文本：replace/insert/delete/replace_text |
| `ws/no-newline.txt` | 没有结尾换行：append 补换行、edit 保持原样 |
| `ws/empty.txt` | 0 行文件 |
| `ws/big.txt` | 40 万字节单行：`max_bytes` 截断 |
| `ws/binary.bin` | NUL 字节：`E_BINARY` |
| `ws/tiny.png` / `ws/huge.png` | 64×48 图片 / 4500 万像素图片 |
| `ws/ignored.txt` + `.gitignore` | grep/glob 遵守忽略规则 |
| `ws/.hidden.md` | grep/glob 跳过隐藏文件 |
| `cfg/config.json` | `search.backend=searxng` 指向桩；`shell.maxOutputBytes=200 KiB`；`lsp.requestTimeoutMs=800` |
| `cfg/models.json` | provider 指向 7497 的桩 |
| `deny.json` | `denyTools: ["write"]`，用于 `E_TOOL_DENIED` |
| `stub.log` | 每个请求体（JSON 行）+ 一行 HTTP 访问日志 |

**每个用例开始都会清空 `ws/`** 并重写上面的文件，所以用例之间不会互相污染。

## 4. 断言方式

一条用例声明期望，脚本逐项比较：

| 字段 | 含义 |
| --- | --- |
| `ok` | `tool_result.ok` 的期望值（默认 true）。工具**执行成功但命令失败**仍为 true（`bash exit 3`） |
| `code` | `content` 里必须出现某个 `E_*` |
| `has` / `absent` | `content` 中必须/不得出现的子串 |
| `meta` | `metadata` 的子集匹配 |
| `files` / `missing` | 回合结束后磁盘上的精确内容（或 `("~", 子串)`） |
| `calls` | 一个回合内的多个调用 |
| `log` | 桩进程收到的最后一个请求体里必须出现的子串（例如图片真的作为 `image_url` 发出） |
| `verify` | 自定义检查（例如溢写文件确实存在且 > 130 KiB） |
| `config` | 换一份配置（deny 用例） |

参数会先做一次往返比较：`tool_call.arguments` 必须与用例里的 `arguments` **完全一致**
（`$pid` 会被替换成真实数字），这样"参数被悄悄改写/丢弃"也能被测出来。

## 5. 覆盖矩阵

下面就是全部 102 条用例。每个工具一节，"用法"列对应 schema 里的字段。

### 5.1 `read`（15）

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| `path` 文件 | 窗口：offset/limit | 只含 `2#`~`3#`，`4#` 不出现 | 返回整份文件 |
| `raw=true` | raw | 无 `line#anchor\|` 前缀 | 仍带前缀（复制到 edit 会坏） |
| `path` 目录 | 目录 | `entries`、目录带 `/` | 当文件读，报 E_IO |
| 空文件 | 空文件 | `0 lines`，ok | E_OUT_OF_RANGE |
| — | 不存在 | `E_NOT_FOUND` | 返回空内容当成功 |
| — | 二进制 | `E_BINARY` | 把乱码塞进上下文 |
| `offset` | 超出末尾 | `E_OUT_OF_RANGE` | 静默返回空 |
| `offset+limit` | 窗口 | metadata `start`/`end` 正确 | 偏移一位 |
| `max_bytes` | max_bytes | `byte limit reached`，`truncated=true` | 忽略上限返回 400 KB |
| `path` 图片 | 图片 | `[image] 64x48` + metadata `kind=image`；请求体里有 `image_url` | 只给一句说明，模型看不到图（曾经的 bug） |
| `path` 超大图 | 超大图 | `E_IMAGE_TOO_LARGE` | 解码 40 MP 把内存吃光 |
| `path` 越界 | `/etc/passwd` | `E_PATH_ESCAPE` | 读到工作区外 |
| `path` URL | URL 读成 markdown | `kind=url` | 当本地路径解析失败 |
| `prompt` | URL 提炼 | 走 compact 模型后返回文本 | 忽略 prompt |
| 缓存 | 同回合第二次 | `cache_hit=true` | 每次都重新抓取 |
| 单行超长 | max_bytes 截断单行 | `[line truncated at max_bytes]` | 整行 400 KB 直接返回，`max_bytes` 形同虚设（曾经的 bug） |

`cache_ttl`、`use_compact_model=false` 分别由"缓存"与"URL 读成 markdown"覆盖；
`cache_ttl` 的**过期**难以在一个回合内验证，见第 8 节。

### 5.2 `write`（6）

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| 新文件 + 缺失父目录 | 新建 | `created`，文件内容一致 | 报 E_IO |
| 已存在、无 overwrite | 追加 | `文件已存在，内容已追加`、`starting line: 4`，旧内容保留 | 静默覆盖（丢用户数据） |
| 末尾无换行 | 追加补换行 | `no trailing newline\ntail\n` | 粘成一行 |
| `overwrite=true` | 覆盖 | `overwrote`，内容完全替换 | 追加而不是替换 |
| `path` 是目录 | 目录 | `E_IS_DIR` | 在目录里建文件失败得含糊 |
| `denyTools` | 被拒绝 | `E_TOOL_DENIED`，磁盘无文件 | 仍然写盘 |

### 5.3 `edit`（20）

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| `op=replace` 单行 | replace 单行 | 返回 diff，文件变 | 行号错位 |
| `pos..end` 范围 | replace 行范围 | 3 行变 2 行 | 只改第一行 |
| `insert_before` / `insert_after` | 插入 | 上下各一行，原有行不动 | 插反 |
| `op=delete` | delete | 该行消失 | 留下空行 |
| `replace_text` 唯一 | 唯一匹配 | 只改那一处 | 误改多处 |
| `replace_text all=true` | 全部 | 三处全改 | 仍要求唯一 |
| 不唯一且无 all | 报错 | `E_INVALID_PATCH`，文件不变 | 随便挑一处改 |
| 零匹配 | 报错 | `E_INVALID_PATCH` | 静默成功 |
| replace_text + 其它编辑 | 同批 | `E_INVALID_PATCH` | 部分生效 |
| 锚点 `"12#AB"` | 读到的锚点可直接编辑 | 断言通过、文件变 | 锚点被当成行号 |
| 过期锚点 | `E_STALE_ANCHOR` | 整批拒绝，文件不变 | 按行号硬改 |
| `pos` 非法 | `pos: "abc"` | `E_INVALID_PATCH` | panic / 改到第 1 行 |
| 行号越界 | `E_INVALID_PATCH` | 拒绝 | 越界写 |
| 同位置两次插入 | `E_OVERLAP` | 拒绝并说明拆开 | 顺序随机 |
| `files: []` | 空批 | `E_INVALID_PATCH` | 报成功 |
| `dry_run=true` | 干跑 | `dry_run`、文件不变、`changed` 元数据 | 仍然写盘 |
| 多文件 | 一次调用 | 两个 diff，两个文件都变 | 只改第一个 |
| 多文件有失败 | 全或无 | 整批拒绝，**第一个文件也没变** | 半改（最危险） |
| 内容相同 | 无改动 | `no changes` | 报告改了 |
| 无结尾换行文件 | 保持 | 仍无结尾换行 | 补上换行（无意义 diff） |

### 5.4 `grep`（14）

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| `pattern` | content | `notes.txt:2:beta line two` | 只给文件名 |
| `anchors=true` | 锚点 | `notes.txt:2#AB\|` | 锚点与行号错位 |
| `output_mode=files_with_matches` | — | 只有路径 | 混入内容 |
| `output_mode=count` | — | `path:N` | 逐行输出 |
| `case_insensitive` | — | 小写也命中 | 大小写敏感 |
| `context` | — | 上下文行用 `-` 分隔 | 没有上下文 |
| `limit`/`offset` | 翻页 | 第 2 条起、只 1 条 | offset 被忽略（先确认顺序！） |
| `glob` | — | 只有 `.rs` | 忽略 glob |
| `literal=true` | 元字符 | `fn main(` 当字面量命中 | 当正则解析（`E_INVALID_PATTERN`，曾经的 bug） |
| 空 pattern | — | `E_INVALID_ARGUMENT` | 匹配所有行 |
| 坏正则 | — | `E_INVALID_PATTERN` | panic |
| `.gitignore` / 隐藏文件 | — | `ignored.txt`、`.hidden.md` 不出现 | 搜索噪音 |
| `offset` 超总数 | — | `no matches` | 报错 |
| 无匹配 | — | `no matches`，`matches=0` | 返回空字符串 |

### 5.5 `glob`（5）

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| `pattern` | `**/*.rs` | 两个文件，`files=2` | 漏文件 |
| `path` + `limit` | 作用域 | 只有作用域内、`returned=1` | 越过作用域 |
| `path` + `**/` | 直接子文件 | 仍能匹配 | 作用域下静默为空 |
| 坏 `pattern` | — | `E_INVALID_GLOB` | panic |
| 无匹配 | — | `(no files matched)` | 空输出 |

### 5.6 `bash` / `await`（10 + 3）

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| 成功 | — | `exit_code: 0` + 输出 + `--- stats ---` | 丢输出 |
| 非零退出 | `exit 3` | `exit_code: 3`，`ok=true` | 当成工具失败 |
| `timeout_ms` | 超时 | `status: timeout` | 永远挂着 |
| `run_in_background` | 后台 | `pid:` + `background=true` | 前台阻塞 |
| 空 `command` | — | `E_INVALID_ARGUMENT` | 执行空串 |
| ANSI | 颜色 | 只剩文本 | 转义序列进上下文 |
| `\r` 进度 | — | 只剩最后状态 | 每个状态一行 |
| cwd | `pwd` | 工作区路径 | 落到 `/` |
| `capture=raw` | — | metadata 有 base64 | 空 |
| 超大输出 | 溢写 | `too large to return` + 文件存在 > 130 KiB | 截断丢掉头部 |
| `await pid` | 完成 | `status: exited` + 输出 | 拿不到退出码 |
| `await` 超时 | — | `status: running`，进程仍活着 | 杀进程或阻塞 |
| `await` 未知 pid | — | `E_UNKNOWN_PID` + `live_pids` | panic |

`await` 必须与 `bash` 在**同一个回合**：后台进程属于启动它的那个进程，`run` 一退出就
被清理。桩的 `"$pid"` 替换就是为这个准备的。

### 5.7 `todo`（5）

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| `items` | 整表替换 | 两条，`pending=1` | 累加而不是替换 |
| 省略 `items` | 查询 | 返回当前列表 | 清空 |
| `merge=true` 更新 | 按 id 改 | 内容变 | 新增一条重复的 |
| `merge=true` 插入 | 新 id | 出现新条目 | 丢掉 |
| 空 `content` | — | `E_INVALID_ARGUMENT` | 建一条空任务 |

### 5.8 `lsp`（16）

无语言服务器时全部走名字索引（`match=name-based`），这也是 CI 里的默认路径。

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| `document_symbols` | — | 列出 `main`/`helper` | 空 |
| `hover`+`line`+`column` | — | 有摘要 | 忽略行列 |
| `definition` / `type_definition` / `implementation` / `references` | 各一条 | 返回位置（name-based） | 抛错 |
| `workspace_symbols`+`symbol` | — | 命中 `helper` | 忽略查询 |
| `call_hierarchy` | — | name-based 结果 | panic |
| `diagnostics` | — | `no diagnostics` | 把错误当诊断 |
| `rename` 预览 | — | `preview`，文件不变 | 直接写盘 |
| `rename apply=true` | 无服务器 | 仍不改写 | 半套用 workspace edit |
| 缺 `path` / 缺 `line` / 缺 `symbol` | 各一条 | `E_INVALID_ARGUMENT` | 默认值走错分支 |
| `install` 缺 `server` | — | `E_INVALID_ARGUMENT` | 猜一个服务器 |
| `path` 是目录 | — | `E_INVALID_ARGUMENT` | 按目录查符号 |

`install`（带 server）会弹下载同意并**真的联网**，因此不在自动用例里，见第 8 节。

### 5.9 `web_search`（7）

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| `query` | — | 表格含标题/URL | 空 |
| `allowed_domains` | — | 只剩 `example.com` | 忽略过滤 |
| `blocked_domains` | — | 去掉 `example.com` | 忽略过滤 |
| `num_results` | — | 条数受限 | 全返回 |
| 无结果 | — | `no results`（不报错） | 报错 |
| 后端 500 | — | 工具失败，错误可读 | 静默返回空 |
| 空 `query` | — | `E_INVALID_ARGUMENT` | 发空查询 |

### 5.10 注册表（1）

| 用法 | 用例 | 好 | 坏 |
| --- | --- | --- | --- |
| 未知工具名 | `move` | `E_UNKNOWN_TOOL` + 可用列表 | 静默成功 |

## 6. 错误码清单（工具侧）

| code | 出现处 | 自动化 |
| --- | --- | --- |
| `E_INVALID_ARGUMENT` | 参数缺失/形状错、空 pattern、空 command、空 todo、空 query | 是 |
| `E_UNKNOWN_TOOL` | 注册表没有这个名字 | 是 |
| `E_TOOL_DENIED` | `denyTools` | 是 |
| `E_PATH_ESCAPE` | 解析后在工作区外 | 是 |
| `E_NOT_FOUND` | 文件不存在 | 是 |
| `E_IO` | 读写失败（例如追加到非 UTF-8 文件） | 否（难以稳定构造） |
| `E_IS_DIR` | write 目标是目录 | 是 |
| `E_BINARY` | read 非 UTF-8 / 含 NUL | 是 |
| `E_IMAGE_TOO_LARGE` | 超过 40 MP | 是 |
| `E_OUT_OF_RANGE` | read 的 offset 超出末尾 | 是 |
| `E_INVALID_PATTERN` / `E_INVALID_GLOB` | 正则 / glob 编译失败 | 是 |
| `E_INVALID_PATCH` | 行号越界、锚点格式错、replace_text 问题 | 是 |
| `E_STALE_ANCHOR` | 锚点与磁盘不符 | 是 |
| `E_OVERLAP` | 两个编辑覆盖同一范围 | 是 |
| `E_NOOP_LOOP` | 连续三次相同补丁 | 否（需要三段对话） |
| `E_UNKNOWN_PID` | await 一个不存在的 pid | 是 |
| `E_LSP_*` | 服务器缺失/崩溃/需要同意 | 部分 |

## 7. 这一轮抓到的三个真 bug

都不是"确认现状"，而是用例先红、代码后改：

1. **`grep` 的 `literal=true` 根本不字面**。`grep-regex` 的 `build_literals` 是给"已经
   转义好的多模式"用的，它**不转义**输入，于是 `fn main(` 仍被当正则解析并报
   `E_INVALID_PATTERN`。改用 `RegexMatcherBuilder::fixed_strings(true)`，一个模式即可。
2. **单行文件的 `max_bytes` 形同虚设**。截断循环只在"已经输出过至少一行"后才执行
   `break`，所以第一行无论多长都会整行返回——一个 400 KB 的压缩 JS 能直接吃掉上下文。
   现在单行超预算时按预算切开并标 `[line truncated at max_bytes]`。
3. **图片从来没有送到模型**。`read` 把图片放进 `ToolOutput.images`，agent 也把它拼成了
   `user_with_images`，但 `Provider::body_with` 是**手写**请求体，只写 `content` 字符串，
   完全忽略 `images`；页面/会话里那句 `[image] …` 说明一切正常，模型其实只看到一句说明。
   修复：`body_with` 在有图片时发 content parts；顺手删掉从未被调用、却让人以为
   "序列化会处理"的 `impl Serialize for ChatMessage`；补了一条直接断言请求体的测试。

## 8. 这套用例测不了什么

| 测不了 | 原因 | 怎么补 |
| --- | --- | --- |
| 真语言服务器的精确结果与 `rename apply=true` | CI 无服务器 | 本机装 `rust-analyzer` 后重跑 `--area lsp`，`match` 会变成 `lsp-precise` |
| `lsp install` / `restart` 的下载流程 | 会真的联网下载 | 手动 `rustedharness run --json` 观察 `download_consent`；拒绝路径会得到 `E_LSP_CONSENT_DECLINED` |
| `E_IO`（追加到非 UTF-8 文件） | 需要构造只读/非 UTF-8 目标，跨平台不稳 | 手动：`printf '\xff' > f` 后 `write` 追加 |
| `E_NOOP_LOOP` | 需要同一文件连续三次相同补丁 | 手动两轮对话，或加一条多回合用例 |
| URL 缓存的 `cache_ttl` 过期 | 一个回合内等不到过期 | 手动设 `cache_ttl=1`，隔两秒再读 |
| 真实 provider 的图片支持差异 | 桩不校验图片 | 用真 key 读一张图，确认模型描述得出内容 |
| MCP 工具（`mcp__server__tool`） | 需要外部 MCP 服务 | 起一个 `mcp.json` 后手动跑 `call` |
| Windows 上的 `powershell` 工具名 | 工具名随 shell 变化 | 在 Windows 跑同一套，用例里的 `bash` 换成 `powershell` |
| chase 注入（回合结束仍有待办） | 需要多回合 | 手动：`todo` 留 pending，看下一回合前的 chase 提示 |
| 压缩（达到阈值后摘要） | 需要真实长上下文 | 手动，或专门的 `compaction` 测试 |

## 9. 怎么确认"能测出好坏"

改坏一处，跑对应区域，应该变红：

| 改坏什么 | 应该失败 |
| --- | --- |
| `grep` 用回 `build_literals` | grep · literal |
| `read` 去掉单行截断分支 | read · 单行超长 |
| `body_with` 不写 content parts | read · 图片（`log` 断言） |
| `edit` 先写盘再校验 | edit · 多文件里一个失败 |
| `write` 默认覆盖 | write · 已存在时追加 |
| `bash` 不净化 ANSI | bash · ANSI 颜色 |
| `todo` 忽略 `merge` | todo · merge 按 id 更新 |
| 溢写阈值改成无限 | bash · 超大输出 |

新增用例时照这个思路写：**先说清楚哪个真实缺陷会让它红**。

## 10. 加用例的坑

- **一个回合一次调用**：桩按 `tool_call_id` 推进；写多个调用时顺序就是你写的顺序。
- **`await` 的 pid 只能同回合**：跨回合后台进程已经随进程退出被清理。
- **`stub.log` 里有两种行**：`{...}` 请求体和 `http "POST …"` 访问日志，取最后一条
  JSON 才是请求体。
- **grep 的 `flat` 顺序按文件名字典序**，不是按行号跨文件；断言 `offset` 时要限定 `path`
  或先确认顺序（我第一版就写错了）。
- **`fmtTokens` 式的内部格式别猜**：断言 metadata 的数值，而不是拼好的字符串。
- **断言"服务端收到了什么"**：`log` / `files` / `metadata` 比 UI 文字可靠。
- **shell 输出是屏幕文本**：`yes | head -c 300000` 只有屏幕上留下的 ~20 KB，想测溢写得
  输出一行超长文本（见用例）。
- **测试配置会写进 `cfg/config.json`**，需要特殊配置的用例用 `config=` 指到另一份文件。
