#!/usr/bin/env python3
"""End-to-end UI test for the web front end, driven through a real browser.

Everything is exercised through the page the user sees: no internals are called
except the documented `?debug=1` hooks (`__rh.state` for assertions and
`__rh.handleEvent` to feed one server event the harness cannot be asked for on
demand, such as a download prompt or a degraded LSP).

The script starts its own three processes and cleans the fixture up front, so
one command covers the whole surface:

    python3 script/web-ui-test.py [--area feed] [--keep] [--headed]

Requirements: a built binary (`cargo build -p rustedharness-cli`), python
`playwright` with Chromium installed, and `git` for the status badges.

See docs/web-ui-testing.md for what each check means and for the by-hand steps.
"""

import argparse
import asyncio
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

from playwright.async_api import async_playwright

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / "rustedharness"
SCRATCH = Path("/tmp/rh-ui-test")
WS = SCRATCH / "ws"
STUB_PORT = 7497
MAIN_PORT = 7499
AUTH_PORT = 7498
MAIN = f"http://127.0.0.1:{MAIN_PORT}"
TOKEN = "s3cret"

RESULTS = []
PAGE_ERRORS = []
ONLY = None


# ---------------------------------------------------------------- fixture ----
def write(path, content, mode="w"):
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, mode) as fh:
        fh.write(content)


def make_fixture():
    """A workspace with one of every file kind the viewer distinguishes."""
    if SCRATCH.exists():
        shutil.rmtree(SCRATCH)
    WS.mkdir(parents=True)
    write(WS / "README.md", """# 测试工作区

这是一个 **Markdown** 预览用的文件，包含列表：

- 第一项
- 第二项，带 `inline code`
- 第三项，带 [链接](https://example.com)

> 引用块

```python
def hello():
    return "world"
```
""")
    write(WS / "src" / "main.rs", """// 一个用来测试行号与语法高亮的 Rust 文件
use std::collections::BTreeMap;

const LIMIT: usize = 42;

fn main() {
    let mut map: BTreeMap<String, usize> = BTreeMap::new();
    map.insert("answer".to_string(), LIMIT);
    println!("{:?}", map);
}

/// 文档注释
pub fn double(value: u64) -> u64 {
    value * 2
}
""")
    write(WS / "data.json", '{"name":"fixture","nested":{"a":1,"b":[1,2,3]},"ok":true}\n')
    write(WS / "page.html", "<!doctype html><html><body><h1>not running</h1>"
                            "<script>alert(1)</script></body></html>\n")
    write(WS / "notes.txt", "notes line 1\nnotes line 2\n")
    write(WS / ".hidden.md", "# 隐藏文件\n\n点文件也要出现在树里。\n")
    with open(WS / "binary.bin", "wb") as fh:
        fh.write(bytes([0, 1, 2, 0, 255, 254, 0, 65]))
    write(WS / "big.txt", "x" * (300 * 1024))
    write(WS / "build" / "out.bin", "ignored artifact\n")
    write(WS / ".gitignore", "build/\n")
    for cmd in (["git", "init", "-q", "."],
                ["git", "add", "-A"],
                ["git", "-c", "user.name=t", "-c", "user.email=t@t",
                 "commit", "-qm", "fixture"]):
        subprocess.run(cmd, cwd=WS, capture_output=True)
    # both are written after the commit so git has something to report
    write(WS / "README.md", "\nmore\n", "a")
    write(WS / "src" / "extra.txt", "untracked\n")
    write(SCRATCH / "models.json", json.dumps({
        "providers": {"fake": {
            "name": "Fake",
            "baseUrl": f"http://127.0.0.1:{STUB_PORT}/v1",
            "apiKey": "stub-key",
            "api": "openai-completions",
            "models": [
                {"id": "fake-a", "name": "Fake A", "contextWindow": 32768,
                 "reasoning": True},
                {"id": "fake-b", "name": "Fake B", "contextWindow": 32768},
            ],
        }},
    }, indent=2))
    write(SCRATCH / "config.json", json.dumps({
        "model": "fake/fake-a",
        "compactModel": "fake/fake-b",
        "agent": {"rateLimitRetries": 5, "compactThreshold": 0.8},
        "search": {"backend": "duckduckgo"},
    }, indent=2))


def config_json():
    try:
        return json.loads((SCRATCH / "config.json").read_text())
    except Exception:
        return {}


def models_json():
    try:
        return json.loads((SCRATCH / "models.json").read_text())
    except Exception:
        return {}


# ---------------------------------------------------------------- runner ----
def check(group, name, ok, detail=""):
    RESULTS.append({"group": group, "name": name, "ok": bool(ok),
                    "detail": str(detail)[:300]})


def area(name):
    return ONLY is None or ONLY == name


async def run_area(name, factory):
    if not area(name):
        return
    print(f"\n== {name}")
    try:
        await factory()
    except Exception as exc:  # a crashed area is a failed check, not a crash
        check(name, f"{name} 区域跑完", False, f"{type(exc).__name__}: {exc}")


async def new_page(browser, url=None, errors=None, viewport=None):
    page = await browser.new_page(viewport=viewport or {"width": 1440, "height": 900})
    sink = errors if errors is not None else PAGE_ERRORS
    page.on("pageerror", lambda e: sink.append(str(e)))
    await page.goto(url or (MAIN + "/?debug=1"))
    await page.wait_for_function(
        "() => document.querySelector('#connPill').classList.contains('online')",
        timeout=15000)
    return page


# --------------------------------------------------------------- helpers ----
async def text_of(page, selector):
    try:
        return (await page.inner_text(selector)).strip()
    except Exception:
        return ""


async def has(page, selector):
    return await page.locator(selector).count() > 0


async def visible(page, selector):
    return await page.is_visible(selector)


async def feed(page):
    return await page.evaluate("""() => window.__rh.state.feed.items.map((item) => ({
        kind: item.kind,
        text: String((item.data && item.data.text) || ''),
        name: String((item.data && item.data.name) || ''),
        status: String((item.data && item.data.status) || ''),
        args: (item.data && item.data.args) || null,
        result: item.data && item.data.result != null ? String(item.data.result) : null
    }))""")


async def wait_feed(page, predicate, timeout=20000):
    """Poll the feed model until `predicate(items)` is true."""
    deadline = time.time() + timeout / 1000
    items = []
    while time.time() < deadline:
        items = await feed(page)
        if predicate(items):
            return items
        await page.wait_for_timeout(80)
    return items


async def send(page, text):
    """Type and press Enter, without waiting for the turn to finish."""
    await page.fill("#input", text)
    await page.press("#input", "Enter")


async def turn(page, text, timeout=25000):
    """Send one prompt and wait until the turn has produced items and settled."""
    before = await page.evaluate("() => window.__rh.state.feed.items.length")
    await send(page, text)
    await page.wait_for_function(
        """(args) => {
             const s = window.__rh.state;
             return !s.busy && s.feed.items.length > args.n;
           }""",
        arg={"n": before}, timeout=timeout)
    return await feed(page)


async def inject(page, event):
    await page.evaluate("(e) => window.__rh.handleEvent(e)", event)
    await page.wait_for_timeout(120)


async def right_click(page, locator):
    """Right-click a message after its scroll has settled.

    A scroll hides the context menu, so opening one while the feed is still
    scrolling into view would close it on the next frame - which is the intended
    behaviour, not something a test should trip over.
    """
    await locator.scroll_into_view_if_needed()
    await page.wait_for_timeout(250)
    await locator.click(button="right")
    await page.wait_for_timeout(200)


async def ctx_click(page, label):
    """Click a context-menu entry at its pixel, so nothing scrolls the feed."""
    item = page.locator("#ctxMenu .ctx-item").filter(has_text=label)
    await item.wait_for(timeout=5000)
    hit = await item.evaluate("""(node) => {
        const box = node.getBoundingClientRect();
        const at = document.elementFromPoint(box.x + box.width / 2,
                                             box.y + box.height / 2);
        return !!(at && node.contains(at));
    }""")
    if not hit:
        return False
    box = await item.bounding_box()
    await page.mouse.click(box["x"] + box["width"] / 2, box["y"] + box["height"] / 2)
    await page.wait_for_timeout(200)
    return True


async def row(page, label):
    return page.locator(".set-row").filter(has_text=label).first


async def click_opt(page, label, option):
    """Click one choice in a settings row by its exact label.

    Substring matching is wrong here: `high` also matches `xhigh`.
    """
    import re
    button = (await row(page, label)).get_by_role(
        "button", name=re.compile("^" + re.escape(option)))
    await button.click()
    await page.wait_for_timeout(700)


async def toast_text(page):
    return await text_of(page, "#toast")


async def wait_toast(page, needle, timeout=4000):
    try:
        await page.wait_for_function(
            "(t) => document.querySelector('#toast').textContent.includes(t)",
            arg=needle, timeout=timeout)
        return True
    except Exception:
        return False


# ------------------------------------------------------------ A. boot ----
async def area_boot(browser):
    errors = []
    page = await new_page(browser, errors=errors)

    check("A 启动", "A1 加载无 JS 报错", not errors, errors)
    check("A 启动", "A2 连接状态 online",
          "已连接" == await text_of(page, "#statusText")
          and await text_of(page, "#footStatus") == "已连接",
          await text_of(page, "#statusText"))
    check("A 启动", "A3 配置已应用（工作目录 + 模型）",
          await text_of(page, "#wsPathText") == str(WS)
          and await text_of(page, "#modelBtnValue") == "fake/fake-a",
          [await text_of(page, "#wsPathText"), await text_of(page, "#modelBtnValue")])
    session = await page.evaluate("() => window.__rh.state.sessionId")
    check("A 启动", "A4 会话 chip 显示短 id",
          session and await text_of(page, "#sessionChipValue") == session[:8],
          [session, await text_of(page, "#sessionChipValue")])
    await page.click("#sessionChip")
    check("A 启动", "A5 点会话 chip 打开会话列表",
          await page.is_visible("#sessionsTab")
          and "active" in (await page.get_attribute(
              ".stab[data-tab='sessions']", "class") or ""),
          await page.get_attribute(".stab[data-tab='sessions']", "class"))
    await page.click(".stab[data-tab='files']")

    theme_before = await page.get_attribute("html", "data-theme")
    await page.click("#themeBtn")
    light = await page.get_attribute("html", "data-theme")
    stored = await page.evaluate("() => localStorage.getItem('rh_theme')")
    meta = await page.get_attribute("#themeColor", "content")
    await page.click("#themeBtn")
    after = await page.get_attribute("html", "data-theme")
    check("A 启动", "A6 主题切换并持久化",
          theme_before is None and light == "light" and stored == "light"
          and meta == "#f5f7fa" and after is None,
          [theme_before, light, stored, meta, after])

    await page.click("#keysBtn")
    rows = await page.locator("#keysModal .key-row").count()
    open_ok = await page.is_visible("#keysModal")
    await page.keyboard.press("Escape")
    check("A 启动", "A7 快捷键面板 6 行且 Esc 关闭",
          open_ok and rows == 6 and not await page.is_visible("#keysModal"), rows)

    chips = await page.locator("#emptyState .empty-chip").count()
    if chips:
        await page.locator("#emptyState .empty-chip").first.click()
        filled = await page.input_value("#input")
        check("A 启动", "A8 空状态提示词填入输入框", bool(filled), filled)
        await page.fill("#input", "")
    else:
        check("A 启动", "A8 空状态提示词填入输入框", False, "空状态未出现")

    check("A 启动", "A9 标题与品牌",
          "RustedHarness" in (await page.title() or "")
          and await text_of(page, ".brand-name") == "RustedHarness",
          await page.title())
    await page.close()


# -------------------------------------------------------- B. composer ----
async def area_composer(browser):
    page = await new_page(browser)

    await send(page, "hello 输入框")
    items = await wait_feed(page, lambda it: len(it) >= 2 and
                            any(i["kind"] == "assistant" for i in it))
    check("B 输入", "B1 Enter 发送并清空输入框",
          await page.input_value("#input") == "" and items[0]["kind"] == "user",
          items[:2])

    await page.fill("#input", "第一行")
    await page.press("#input", "Shift+Enter")
    await page.type("#input", "第二行")
    value = await page.input_value("#input")
    count_before = len(await feed(page))
    check("B 输入", "B2 Shift+Enter 换行且不发送",
          "\n" in value and len(await feed(page)) == count_before, repr(value))
    await page.fill("#input", "")
    await page.wait_for_function("() => window.__rh.state.busy === false",
                                 timeout=20000)

    before = len(await feed(page))
    await page.press("#input", "Enter")
    await page.wait_for_timeout(400)
    check("B 输入", "B3 空输入不发送", len(await feed(page)) == before)

    items = await turn(page, "/hello 斜杠是普通文本")
    check("B 输入", "B4 斜杠命令当普通消息发送",
          items[-2]["kind"] == "user" and items[-2]["text"].startswith("/hello"),
          [i["text"][:20] for i in items[-2:]])

    await send(page, "slow 慢速")
    await page.wait_for_function("() => window.__rh.state.busy === true",
                                 timeout=8000)
    busy_label = await text_of(page, "#sendBtn")
    check("B 输入", "B5 运行中发送按钮变停止",
          "停止" in busy_label
          and "stop" in (await page.get_attribute("#sendBtn", "class") or ""),
          busy_label)

    await send(page, "排队消息一")
    items = await wait_feed(page, lambda it: any(
        i["kind"] == "note" and "已排队" in i["text"] for i in it), timeout=5000)
    queued = [i for i in items if i["kind"] == "note" and "已排队" in i["text"]]
    check("B 输入", "B6 运行中发送进入队列",
          bool(queued) and await page.evaluate("() => window.__rh.state.queue.length") == 1,
          queued[-1]["text"] if queued else "")

    items = await wait_feed(page, lambda it: any(
        i["kind"] == "note" and "发送排队消息" in i["text"] for i in it), timeout=40000)
    flushed = [i for i in items if i["kind"] == "note" and "发送排队消息" in i["text"]]
    check("B 输入", "B7 回合结束后自动发送排队消息", bool(flushed),
          flushed[-1]["text"] if flushed else "")
    await page.wait_for_function("() => window.__rh.state.busy === false",
                                 timeout=40000)

    await send(page, "slow 停止我")
    await page.wait_for_function("() => window.__rh.state.busy === true",
                                 timeout=8000)
    await page.wait_for_timeout(700)
    await page.click("#sendBtn")
    await page.wait_for_function("() => window.__rh.state.busy === false",
                                 timeout=8000)
    items = await feed(page)
    growing = len([i for i in items if i["kind"] == "assistant"])
    await page.wait_for_timeout(1200)
    check("B 输入", "B8 停止按钮取消当前回合",
          await page.evaluate("() => window.__rh.state.busy === false")
          and len([i for i in await feed(page) if i["kind"] == "assistant"]) == growing,
          growing)

    await page.fill("#input", "x")
    one = await page.evaluate("() => document.querySelector('#input').offsetHeight")
    await page.fill("#input", "\n".join(["line %d" % i for i in range(30)]))
    many = await page.evaluate("() => document.querySelector('#input').offsetHeight")
    await page.fill("#input", "")
    check("B 输入", "B9 输入框自增高并封顶 170px", one < many <= 170, [one, many])
    await page.close()


# ------------------------------------------------------------ C. feed ----
async def area_feed(browser):
    page = await new_page(browser)
    await turn(page, "hello 你好")
    await page.wait_for_timeout(400)

    check("C 消息", "C1 用户气泡与「你」标记",
          await has(page, ".msg.user .bubble") and
          "你" in await text_of(page, ".msg.user .meta"))
    check("C 消息", "C2 Agent 卡片带头像与模型标签",
          await has(page, ".msg.agent .agent-head .model-tag")
          and "fake/fake-a" == await text_of(page, ".msg.agent .model-tag"))

    await turn(page, "reason 请思考")
    await page.wait_for_timeout(600)
    think = page.locator(".think").last
    check("C 消息", "C3 思考块完成后折叠并显示字数",
          await think.count() > 0
          and not await think.evaluate("n => n.open")
          and "字" in await think.locator(".think-count").inner_text(),
          await think.locator(".think-count").inner_text())

    await turn(page, "tool 跑一个命令")
    items = await wait_feed(page, lambda it: any(
        i["kind"] == "tool" and i["status"] == "ok" for i in it), timeout=25000)
    tool = [i for i in items if i["kind"] == "tool"][-1]
    card = page.locator("details.tool").last
    check("C 消息", "C4 工具卡片显示参数、状态与输出",
          tool["name"] == "bash" and "tool says hi" in (tool["result"] or "")
          and "完成" == await card.locator(".tstat").inner_text(),
          [tool["name"], (tool["result"] or "")[:40]])
    check("C 消息", "C5 完成后的工具卡片默认折叠",
          not await card.evaluate("n => n.open"), await card.evaluate("n => n.open"))
    await card.locator("summary").click()
    check("C 消息", "C6 点击摘要展开工具卡片",
          await card.evaluate("n => n.open")
          and await card.locator(".result-wrap").is_visible())

    await turn(page, "boom 触发错误")
    await page.wait_for_timeout(400)
    check("C 消息", "C7 provider 报错渲染错误卡片",
          await has(page, ".error-card")
          and "stub failure" in await text_of(page, ".error-card"))

    await inject(page, {"type": "compaction", "summary": "摘要正文",
                        "dropped_messages": 7})
    check("C 消息", "C8 压缩卡片显示丢弃条数与摘要",
          await has(page, "details.compaction")
          and "丢弃 7 条" in await text_of(page, "details.compaction"),
          await text_of(page, "details.compaction"))

    await inject(page, {"type": "assistant_message",
                        "content": "## 标题\n\n- 列表项\n\n**粗体** 与 [链接](https://example.com)\n\n"
                                   "```python\ndef f():\n    return 1\n```"})
    md = page.locator(".msg.agent .md").last
    check("C 消息", "C9 Markdown：标题/列表/粗体/链接/代码块",
          await md.locator("h2").count() == 1
          and await md.locator("li").count() >= 1
          and await md.locator("strong").count() >= 1
          and await md.locator("a[target=_blank]").count() == 1
          and (await md.locator(".codeblock .cb-lang").inner_text()).lower() == "python",
          [await md.locator("h2").count(), await md.locator("li").count(),
           await md.locator("strong").count(),
           await md.locator("a[target=_blank]").count()])

    await inject(page, {"type": "user_message",
                        "text": "[x](javascript:alert(1)) 不该变成链接"})
    await page.wait_for_timeout(200)
    bad = page.locator(".msg.user").last
    check("C 消息", "C10 javascript: 链接不渲染为可点击链接",
          await bad.locator("a").count() == 0
          and "javascript:alert(1)" in await bad.inner_text())

    await page.context.grant_permissions(["clipboard-read", "clipboard-write"])
    await page.locator(".cb-copy").last.click()
    await page.wait_for_timeout(300)
    clip = await page.evaluate("() => navigator.clipboard.readText()")
    label = await page.locator(".cb-copy").last.inner_text()
    check("C 消息", "C11 代码块复制按钮写入剪贴板并显示已复制",
          "def f():" in clip and "已复制" in label, [clip[:24], label])

    await right_click(page, page.locator(".msg.user .bubble").first)
    menu_items = await page.locator("#ctxMenu .ctx-item").all_inner_texts()
    check("C 消息", "C12 用户消息右键菜单两项",
          menu_items == ["回滚到这条消息", "复制这条消息"], menu_items)
    clicked = await ctx_click(page, "复制这条消息")
    clip = await page.evaluate("() => navigator.clipboard.readText()")
    check("C 消息", "C13 复制这条消息写入剪贴板",
          clicked and "hello 你好" in clip, [clicked, clip[:40]])

    await right_click(page, page.locator(".msg.agent .md").first)
    assistant_menu = await page.locator("#ctxMenu .ctx-item").all_inner_texts()
    check("C 消息", "C14 Agent 消息右键菜单只有复制",
          assistant_menu == ["复制这条回复"], assistant_menu)
    await page.keyboard.press("Escape")

    # --- scroll behaviour: sending follows the bottom, but scrolling up during a
    #     stream must stop the follow, or reading history becomes impossible.
    await send(page, "long 长输出")
    await page.wait_for_function("() => window.__rh.state.busy === true",
                                 timeout=8000)
    await page.wait_for_timeout(300)
    await page.evaluate("() => { const f = document.getElementById('feed');"
                        " f.scrollTop = 300; }")
    await page.wait_for_timeout(300)
    held = await page.evaluate("() => document.getElementById('feed').scrollTop")
    await page.wait_for_function("() => window.__rh.state.busy === false",
                                 timeout=20000)
    await page.wait_for_timeout(400)
    after = await page.evaluate("() => document.getElementById('feed').scrollTop")
    check("C 消息", "C15 流式期间上滚后不再跟随底部",
          held <= 320 and after <= 340, [held, after])

    await page.evaluate("() => { const f = document.getElementById('feed');"
                        " f.scrollTop = f.scrollHeight; }")
    await page.wait_for_timeout(200)
    await inject(page, {"type": "user_message", "text": "底部新增的一条"})
    await page.wait_for_timeout(600)
    at_bottom = await page.evaluate(
        "() => { const f = document.getElementById('feed');"
        " return f.scrollHeight - f.scrollTop - f.clientHeight; }")
    check("C 消息", "C16 在底部时新消息跟随滚动", at_bottom < 120, at_bottom)

    # --- virtualization with 2000 messages
    messages = [{"role": "user", "content": "第 %d 条历史消息 %s" % (i, "y" * (i % 40))}
                for i in range(2000)]
    await page.evaluate("""(messages) => window.__rh.applySnapshot({
        session_id: 'snapshot-1', cwd: '""" + str(WS) + """',
        agent_model: 'fake/fake-a', compact_model: '', messages: messages,
        todos: [], context_tokens: 0, context_limit: 32768 })""", messages)
    await page.wait_for_timeout(700)
    mounted = await page.evaluate("() => window.__rh.state.feed.el.size")
    inner = await page.evaluate("() => document.getElementById('feedInner').style.height")
    gap = await page.evaluate(
        "() => { const f = document.getElementById('feed');"
        " return f.scrollHeight - f.scrollTop - f.clientHeight; }")
    check("C 消息", "C17 2000 条消息只挂载少量节点且落在底部",
          mounted < 120 and gap < 200 and inner.endswith("px"), [mounted, inner, gap])

    tops = await page.evaluate("""() => {
        const s = window.__rh.state;
        const bad = [];
        for (const [index, node] of s.feed.el) {
          if (Math.abs(parseFloat(node.style.top) - s.feed.offsets[index]) > 0.6) bad.push(index);
        }
        return bad;
    }""")
    check("C 消息", "C18 挂载节点的 top 与 offsets 一致", not tops, tops)

    await page.evaluate("() => { const f = document.getElementById('feed');"
                        " f.scrollTop = 0; window.__rh.feedSync(); }")
    await page.wait_for_timeout(300)
    first = await page.evaluate("() => { const s = window.__rh.state;"
                                " return { top: parseFloat(s.feed.el.get(0).style.top),"
                                " mounted: s.feed.el.size }; }")
    check("C 消息", "C19 滚到顶部时第一条可见", first["top"] == 0 and first["mounted"] < 120,
          first)
    await page.close()


# -------------------------------------------------------- D. rollback ----
async def area_rollback(browser):
    page = await new_page(browser)
    await turn(page, "rollback one")
    await turn(page, "rollback two")
    await turn(page, "rollback three")
    users = page.locator(".msg.user")
    before = await users.count()

    await right_click(page, users.nth(1))
    check("D 回滚", "D0 右键菜单可点击", await ctx_click(page, "回滚到这条消息"))
    preview = await text_of(page, "#rollbackPreview")
    check("D 回滚", "D1 回滚对话框显示保留的那一条",
          await page.is_visible("#rollbackModal") and "保留：#2" in preview
          and "rollback two" in preview, preview)

    await page.click("#rollbackConfirmBtn")
    await page.wait_for_timeout(1500)
    after = await page.locator(".msg.user").all_inner_texts()
    kinds = [i["kind"] for i in await feed(page)]
    check("D 回滚", "D2 确认回滚保留选中输入、删除其后的消息",
          len(after) == 2 and "rollback two" in after[-1]
          and any("rollback three" in i["text"] for i in await feed(page)) is False
          and kinds[0] == "user",
          [len(after), after[-1][:24], kinds[:4]])

    await right_click(page, page.locator(".msg.user").last)
    await ctx_click(page, "回滚到这条消息")
    await page.click("#rollbackModal .btn:not(.danger):not(.primary)")
    check("D 回滚", "D3 取消关闭对话框且不改动消息",
          not await page.is_visible("#rollbackModal")
          and await page.locator(".msg.user").count() == 2)
    await page.close()


# -------------------------------------------------------- E. settings ----
async def area_settings(browser):
    page = await new_page(browser)
    await page.click("#settingsBtn")
    await page.wait_for_selector("#setList .set-row", timeout=8000)
    tabs = await page.locator("#setTabs .set-tab").all_inner_texts()
    session_rows = await page.locator("#setList .set-row").count()
    token_visible = await page.is_visible("#tokenField")
    check("E 设置", "E1 四个分组与会话页三行 + 令牌框",
          tabs == ["会话", "通用", "压缩", "网络搜索"] and session_rows == 3
          and token_visible, [tabs, session_rows])

    await page.click(".set-tab[data-set-tab='general']")
    await page.wait_for_timeout(150)
    token_hidden = not await page.is_visible("#tokenField")
    toggle = (await row(page, "缓存未命中提示")).locator(".switch")
    on_before = await toggle.get_attribute("aria-checked")
    await toggle.click()
    await page.wait_for_timeout(600)
    on_after = await (await row(page, "缓存未命中提示")).locator(
        ".switch").get_attribute("aria-checked")
    cfg = config_json().get("agent", {})
    check("E 设置", "E2 开关立即生效并写入 config.json",
          token_hidden and on_before == "true" and on_after == "false"
          and cfg.get("showCacheMissNotices") is False,
          [on_before, on_after, cfg.get("showCacheMissNotices")])
    await (await row(page, "缓存未命中提示")).locator(".switch").click()
    await page.wait_for_timeout(400)

    await click_opt(page, "429 重试次数", "30 次")
    retry = config_json().get("agent", {}).get("rateLimitRetries")
    selected = await (await row(page, "429 重试次数")).locator(".opt.sel").inner_text()
    check("E 设置", "E3 数字选项写入数字而不是标签", retry == 30 and selected == "30 次",
          [retry, selected])

    await click_opt(page, "图片最长边", "1024 px")
    check("E 设置", "E4 图片最长边写入像素值",
          config_json().get("image", {}).get("maxEdge") == 1024,
          config_json().get("image"))

    await page.click(".set-tab[data-set-tab='compaction']")
    await page.wait_for_timeout(200)
    await click_opt(page, "思考等级", "high")
    await click_opt(page, "压缩阈值", "90%")
    await click_opt(page, "保留最近轮次", "4 轮")
    agent = config_json().get("agent", {})
    check("E 设置", "E5 压缩：思考等级/阈值/保留轮次写入",
          agent.get("compactThinking") == "high"
          and agent.get("compactThreshold") == 0.9
          and agent.get("keepRecentTurns") == 4, agent)

    strip = (await row(page, "裁剪旧上下文")).locator(".switch")
    strip_enabled = not await strip.is_disabled()
    await strip.click()
    await page.wait_for_timeout(600)
    send_row = await row(page, "完整发送最近轮次")
    send_disabled = await send_row.locator(".opt").first.is_disabled()
    check("E 设置", "E6 关闭裁剪后「完整发送最近轮次」被禁用",
          strip_enabled and send_disabled and
          config_json().get("agent", {}).get("compactStripContext") is False,
          [strip_enabled, send_disabled])

    await page.click(".set-tab[data-set-tab='search']")
    await page.wait_for_timeout(200)
    await click_opt(page, "搜索后端", "searxng")
    search = config_json().get("search", {})
    cred = await (await row(page, "实例地址")).count()
    await click_opt(page, "搜索后端", "duckduckgo")
    cred_disabled = await (await row(page, "凭据")).locator("input").is_disabled()
    check("E 设置", "E7 搜索后端切换后凭据行随之变化",
          search.get("backend") == "searxng" and cred == 1 and cred_disabled,
          [search.get("backend"), cred, cred_disabled])

    await page.keyboard.press("Escape")
    esc_closed = not await page.is_visible("#settingsModal")
    await page.click("#settingsBtn")
    await page.wait_for_timeout(400)
    await page.locator("#settingsModal .modal-close").click()
    check("E 设置", "E8 Esc 与 X 都能关闭设置",
          esc_closed and not await page.is_visible("#settingsModal"))

    await page.click("#settingsBtn")
    await page.wait_for_timeout(300)
    await page.mouse.click(30, 450)  # backdrop
    backdrop = not await page.is_visible("#settingsModal")
    await page.click("#settingsBtn")
    await page.wait_for_timeout(300)
    await page.click("#settingsSave")
    check("E 设置", "E9 令牌未变化时提示且不重连",
          backdrop and await wait_toast(page, "令牌未变化", 2000),
          [backdrop, await toast_text(page)])
    await page.keyboard.press("Escape")
    await page.close()


# ---------------------------------------------------------- F. models ----
async def area_models(browser):
    page = await new_page(browser)
    await page.click("#modelBtn")
    await page.wait_for_selector("#modelListBox .model-opt", timeout=6000)
    options = await page.locator("#modelListBox .model-opt").all_inner_texts()
    selected = await page.locator("#modelListBox .model-opt.sel").count()
    check("F 模型", "F1 下拉列出模型并标记当前",
          len(options) == 2 and selected == 1 and "fake/fake-a" in options[0],
          options)

    await page.fill("#modelSearch", "fake-b")
    await page.wait_for_timeout(150)
    filtered = await page.locator("#modelListBox .model-opt").all_inner_texts()
    check("F 模型", "F2 过滤框缩小列表",
          len(filtered) == 1 and "fake/fake-b" in filtered[0], filtered)

    await page.locator("#modelListBox .model-opt").first.click()
    await page.wait_for_timeout(1200)
    # The choice is a session decision, exactly like the TUI's: the next request
    # carries the new model id, while config.json keeps the default.
    await send(page, "hello 换了模型")
    await page.wait_for_function("() => !window.__rh.state.busy", timeout=25000)
    last = [json.loads(line) for line in
            (SCRATCH / "stub.log").read_text().splitlines() if line.startswith("{")]
    sent = last[-1]["body"].get("model") if last else None
    check("F 模型", "F3 选择模型后下一次请求使用新模型且不改默认值",
          await text_of(page, "#modelBtnValue") == "fake/fake-b"
          and sent == "fake-b" and config_json().get("model") == "fake/fake-a",
          [await text_of(page, "#modelBtnValue"), sent, config_json().get("model")])

    await page.click("#modelBtn")
    await page.wait_for_timeout(200)
    await page.keyboard.press("Escape")
    esc = not await page.is_visible("#modelPop")
    await page.click("#modelBtn")
    await page.wait_for_timeout(200)
    await page.mouse.click(700, 400)
    check("F 模型", "F4 Esc 与点击空白都能关闭下拉",
          esc and not await page.is_visible("#modelPop"), esc)

    # restore the fast model and open the compact picker from settings
    await page.click("#modelBtn")
    await page.wait_for_timeout(400)
    await page.locator("#modelListBox .model-opt").filter(has_text="fake-a").click()
    await page.wait_for_timeout(800)

    await page.click("#settingsBtn")
    await page.click(".set-tab[data-set-tab='compaction']")
    await page.wait_for_timeout(300)
    await (await row(page, "压缩模型")).locator("button").first.click()
    await page.wait_for_selector("#modelList .pick", timeout=6000)
    none_row = await page.locator("#modelList .pick").first.inner_text()
    title = await text_of(page, "#modelTitle")
    check("F 模型", "F5 压缩模型选择器含 None（复用主模型）",
          "None" in none_row and "prompt 缓存" in none_row
          and title == "选择压缩模型", [none_row.replace("\n", " "), title])
    await page.click("#modelModal .modal-close")
    await page.click("#settingsModal .modal-close")
    await page.click("#modelBtn")
    await page.wait_for_timeout(300)
    await page.click("#modelAddBtn")
    await page.wait_for_timeout(300)
    await page.click("#addModelNext")
    empty_error = await text_of(page, "#addModelError")
    check("F 模型", "F6 添加模型：空 Base URL 报错",
          "请填写 Base URL" in empty_error, empty_error)

    await page.fill("#addBaseUrl", f"http://127.0.0.1:{STUB_PORT}/v1")
    await page.fill("#addApiKey", "key-for-test")
    await page.click("#addModelNext")
    await page.wait_for_selector("#addModelList .pick", timeout=10000)
    fetched = await page.locator("#addModelList .pick").all_inner_texts()
    step2 = await page.is_visible("#addModelPick")
    check("F 模型", "F7 添加模型：拉取并列出端点模型",
          step2 and len(fetched) == 2 and "discovered-a" in fetched[0],
          [step2, fetched])

    await page.fill("#addModelFilter", "discovered-b")
    await page.wait_for_timeout(150)
    await page.locator("#addModelList .pick").first.click()
    await page.click("#addModelNext")
    await page.wait_for_timeout(1200)
    providers = models_json().get("providers", {})
    added = [pid for pid, prov in providers.items()
             if any(m.get("id") == "discovered-b" for m in prov.get("models", []))]
    check("F 模型", "F8 添加模型写入 models.json 并提示",
          bool(added) and await wait_toast(page, "models.json", 2000),
          [added, await toast_text(page)])
    await page.close()


# ------------------------------------------------------------- G. cwd ----
async def area_cwd(browser):
    page = await new_page(browser)
    await page.click("#cwdBtn")
    await page.wait_for_selector("#dirList .pick", timeout=6000)
    path = await text_of(page, "#dirPath")
    entries = await page.locator("#dirList .pick").all_inner_texts()
    check("G 目录", "G1 目录选择器列出子目录（不过滤）",
          path == str(WS) and any("src/" in e for e in entries)
          and any("build/" in e for e in entries), [path, entries])

    await page.locator("#dirList .pick").filter(has_text="src/").click()
    await page.wait_for_function(
        "(p) => document.querySelector('#dirPath').textContent === p",
        arg=str(WS / "src"), timeout=5000)
    inside = await text_of(page, "#dirPath")
    await page.click("#dirUp")
    await page.wait_for_function(
        "(p) => document.querySelector('#dirPath').textContent === p",
        arg=str(WS), timeout=5000)
    check("G 目录", "G2 进入子目录并返回上一级",
          inside == str(WS / "src") and await text_of(page, "#dirPath") == str(WS),
          inside)

    await page.locator("#dirList .pick").filter(has_text="src/").click()
    await page.wait_for_timeout(400)
    await page.click("#dirChoose")
    await page.wait_for_function(
        "(p) => document.querySelector('#wsPathText').textContent === p",
        arg=str(WS / "src"), timeout=8000)
    check("G 目录", "G3 选择目录后工作区切换",
          await wait_toast(page, "工作目录已切换", 3000)
          and await text_of(page, "#wsPathText") == str(WS / "src"),
          await text_of(page, "#wsPathText"))

    await page.click("#cwdBtn")
    await page.wait_for_selector("#dirList .pick", timeout=5000)
    await page.keyboard.press("Escape")
    check("G 目录", "G4 Esc 关闭目录选择器",
          not await page.is_visible("#dirModal"))
    await page.close()


# ------------------------------------------------------------ H. tree ----
async def area_tree(browser):
    page = await new_page(browser)
    await page.wait_for_selector("#tree .trow.file", timeout=10000)
    names = await page.locator("#tree .trow.file .tname").all_inner_texts()
    dirs = await page.locator("#tree .trow.dir .tname").all_inner_texts()
    check("H 文件树", "H1 点文件与 build 目录都出现在树里",
          ".hidden.md" in names and "big.txt" in names and "build" in dirs,
          [names[:6], dirs])

    marks = await page.locator("#tree .git-mark").all_inner_texts()
    check("H 文件树", "H2 git 状态用字母标出 M/U/I（不只靠颜色）",
          sorted(set(marks)) == ["I", "M", "U"], marks)

    requests = []
    page.on("request", lambda r: requests.append(r.url) if "/api/tree?" in r.url
            else None)
    await page.locator("#tree .trow.dir").filter(has_text="src").click()
    await page.wait_for_selector("#tree .tkids .trow.file", timeout=8000)
    expanded = await page.locator("#tree .tkids .trow.file").count()
    first_requests = len(requests)
    await page.locator("#tree .trow.dir").filter(has_text="src").click()
    collapsed = await page.locator("#tree .tkids").first.is_hidden()
    await page.locator("#tree .trow.dir").filter(has_text="src").click()
    await page.wait_for_timeout(300)
    check("H 文件树", "H3 展开只请求一次，折叠再展开不重复请求",
          expanded >= 2 and collapsed and len(requests) == first_requests,
          [expanded, collapsed, first_requests, len(requests)])

    await page.click("#treeRefresh")
    await page.wait_for_selector("#tree .trow.file", timeout=8000)
    check("H 文件树", "H4 刷新按钮重新读取树",
          await page.locator("#tree .trow.file").count() > 0)
    await page.close()


# ---------------------------------------------------------- I. viewer ----
async def area_viewer(browser):
    errors = []
    page = await new_page(browser, errors=errors)
    await page.wait_for_selector("#tree .trow.file", timeout=10000)
    await page.locator("#tree .trow.dir").filter(has_text="src").click()
    await page.wait_for_selector("#tree .tkids .trow.file", timeout=8000)
    await page.locator("#tree .tkids .trow.file").filter(has_text="main.rs").click()
    await page.wait_for_selector("#codeContent .code-lines .cl", timeout=8000)

    lines = await page.locator("#codeContent .code-lines .cl").count()
    numbers = await page.locator("#codeContent .cl .ln").first.inner_text()
    keywords = await page.locator("#codeContent .tok-kw").count()
    meta = await text_of(page, "#viewMeta")
    check("I 查看器", "I1 代码视图：行号、行数与高亮",
          lines >= 14 and numbers == "1" and keywords >= 3
          and "行" in meta
          and (await text_of(page, "#viewFile")).endswith("main.rs"),
          [lines, numbers, keywords, meta])
    check("I 查看器", "I2 打开的文件在树里高亮",
          await page.locator("#tree .trow.file.active").count() == 1)

    await page.locator("#tree .trow.file").filter(has_text="README.md").click()
    await page.wait_for_timeout(800)
    preview_active = "active" in (await page.get_attribute(
        ".itab[data-itab='preview']", "class") or "")
    md = await page.locator("#previewContent .md-pane").inner_text()
    check("I 查看器", "I3 Markdown 自动切到预览并渲染",
          preview_active and "测试工作区" in md and "第一项" in md, md[:40])

    await page.locator("#tree .trow.file").filter(has_text="data.json").click()
    await page.wait_for_timeout(800)
    json_text = await page.locator("#previewContent .json-preview").inner_text()
    await page.click(".itab[data-itab='preview']")
    check("I 查看器", "I4 JSON 预览已格式化", '"name": "fixture"' in json_text,
          json_text[:60])

    await page.locator("#tree .trow.file").filter(has_text="page.html").click()
    await page.wait_for_timeout(800)
    await page.click(".itab[data-itab='preview']")
    safety = await text_of(page, "#previewContent")
    await page.locator("#previewContent .safety-card .btn").click()
    check("I 查看器", "I5 HTML 预览被禁用且可跳到源码",
          "HTML 预览已禁用" in safety
          and "active" in (await page.get_attribute(
              ".itab[data-itab='code']", "class") or ""), safety[:40])

    await page.locator("#tree .trow.file").filter(has_text="binary.bin").click()
    await page.wait_for_timeout(800)
    copy_disabled = await page.locator("#copyFileBtn").is_disabled()
    check("I 查看器", "I6 二进制文件不可复制并显示说明",
          copy_disabled and "二进制" in await text_of(page, "#viewMeta"),
          await text_of(page, "#viewMeta"))

    await page.locator("#tree .trow.file").filter(has_text="big.txt").click()
    await page.wait_for_timeout(1200)
    check("I 查看器", "I7 大文件显示 256 KiB 截断提示",
          "仅显示前 256 KiB" in await text_of(page, "#codeContent"),
          (await text_of(page, "#viewMeta")))

    await page.context.grant_permissions(["clipboard-read", "clipboard-write"])
    await page.locator("#tree .trow.file").filter(has_text="notes.txt").click()
    await page.wait_for_timeout(700)
    await page.click("#copyFileBtn")
    clip = await page.evaluate("() => navigator.clipboard.readText()")
    check("I 查看器", "I8 复制文件内容",
          "notes line 1" in clip and await wait_toast(page, "已复制文件内容", 2000),
          clip[:20])

    (WS / "temp-remove-me.txt").write_text("gone\n")
    await page.click("#treeRefresh")
    await page.wait_for_selector("#tree .trow.file", timeout=8000)
    await page.locator("#tree .trow.file").filter(has_text="temp-remove-me").click()
    await page.wait_for_timeout(600)
    (WS / "temp-remove-me.txt").unlink()
    await page.click("#fileRefresh")
    await page.wait_for_timeout(700)
    check("I 查看器", "I9 文件消失后显示错误视图",
          "无法读取文件" in await text_of(page, "#codeContent")
          and await wait_toast(page, "无法读取", 2000),
          (await text_of(page, "#codeContent"))[:40])

    await page.locator("#tree .trow.file").filter(has_text="notes.txt").click()
    await page.wait_for_timeout(600)
    await page.click(".itab[data-itab='activity']")
    activity = await page.is_visible("#paneActivity")
    await page.click(".itab[data-itab='terminal']")
    await page.wait_for_timeout(600)
    terminal = await page.is_visible("#paneTerminal")
    await page.click(".itab[data-itab='code']")
    check("I 查看器", "I10 标签页切换面板",
          activity and terminal and await page.is_visible("#paneCode"))

    await page.click("#rightCollapse")
    collapsed = "right-collapsed" in (await page.get_attribute("body", "class") or "")
    edge_visible = await page.is_visible("#edgeRight")
    await page.click("#edgeRight")
    check("I 查看器", "I11 折叠/展开查看器",
          collapsed and edge_visible
          and "right-collapsed" not in (await page.get_attribute("body", "class") or ""))

    box = await page.locator("#rzRight").bounding_box()
    width_before = await page.evaluate(
        "() => document.getElementById('rightPanel').getBoundingClientRect().width")
    await page.mouse.move(box["x"] + 2, box["y"] + 200)
    await page.mouse.down()
    await page.mouse.move(box["x"] - 80, box["y"] + 200, steps=6)
    await page.mouse.up()
    await page.wait_for_timeout(300)
    width_after = await page.evaluate(
        "() => document.getElementById('rightPanel').getBoundingClientRect().width")
    stored = await page.evaluate("() => localStorage.getItem('rh_right_w')")
    check("I 查看器", "I12 拖动分隔条改宽并记住",
          width_after > width_before + 40 and abs(float(stored) - width_after) < 2,
          [width_before, width_after, stored])

    await page.click("#rightCollapse")
    check("I 查看器", "I13 全程无 JS 报错", not errors, errors)
    await page.close()


# -------------------------------------------------------- J. activity ----
async def area_activity(browser):
    page = await new_page(browser)
    await page.click(".itab[data-itab='activity']")
    # Opening the tab asks the server to describe every language server, so the
    # baseline has to wait for that answer instead of counting half of it.
    await page.wait_for_timeout(1200)
    body = await text_of(page, "#activityContent")
    lsp_before = await page.evaluate("() => window.__rh.state.lsp.size")
    empty_lsp = "暂无 LSP 状态事件" in body
    check("J 活动", "J1 会话信息与空态文案",
          "会话信息" in body and "工作区" in body
          and (empty_lsp if lsp_before == 0 else lsp_before > 0)
          and "暂无后台进程" in body, [lsp_before, body[:60]])

    # A language the harness cannot have probed, so the counts are unambiguous.
    await inject(page, {"type": "lsp_servers", "servers": [
        {"language": "cobol", "server": "cobol-ls", "state": "running",
         "version": "2024-01-01"}]})
    body = await text_of(page, "#activityContent")
    bar = await text_of(page, "#lspStatusText")
    lsp_after = await page.evaluate("() => window.__rh.state.lsp.size")
    running = await page.evaluate("""() => Array.from(window.__rh.state.lsp.values())
        .filter((s) => s.state === 'running').length""")
    check("J 活动", "J2 LSP 事件渲染到面板与状态栏",
          "cobol" in body
          and lsp_after == lsp_before + 1 and bar.startswith("LSP ")
          and f"{running} 运行" in bar,
          [lsp_before, lsp_after, running, bar, "cobol" in body])

    messages = [i for i in await feed(page) if i["kind"] == "note"]
    await inject(page, {"type": "lsp_status", "language": "zig",
                        "state": "degraded", "detail": "崩溃"})
    notes = [i["text"] for i in await feed(page) if i["kind"] == "note"]
    badge = not await page.locator("#activityBadge").is_hidden()
    check("J 活动", "J3 降级状态给出提示并点亮徽标",
          any("LSP zig" in n and "degraded" in n for n in notes[-3:])
          and badge and "warn" in (await page.get_attribute("#lspDot", "class") or ""),
          [notes[-1] if notes else "", badge])

    await inject(page, {"type": "process_snapshot", "pid": 4242,
                        "status": "running", "screen": "line one\nline two"})
    body = await text_of(page, "#activityContent")
    check("J 活动", "J4 后台进程出现在面板与状态栏",
          "pid 4242" in body and "进程 1/1" == await text_of(page, "#procStatusText"),
          await text_of(page, "#procStatusText"))

    await page.click("#lspStatus")
    await page.wait_for_timeout(600)
    opened = await page.is_visible("#lspModal")
    listing = await text_of(page, "#lspServers")
    if not opened:
        check("J 活动", "J5 点击 LSP 状态打开对话框", False,
              ["modal hidden", listing[:80], await text_of(page, "#toast")])
    try:
        await page.wait_for_selector("#lspServers .lsp-row", timeout=8000)
    except Exception:
        pass

    rows = await page.locator("#lspServers .lsp-row").count()
    languages = await page.evaluate("() => window.__rh.state.lsp.size")
    install = await page.locator("#lspServers .install-btn").count()
    await page.click("#lspRefresh")
    await page.wait_for_timeout(400)
    check("J 活动", "J5 LSP 对话框列出全部服务器并带下载按钮",
          await page.is_visible("#lspModal") and rows == languages and install >= 2,
          [rows, languages, install, (await text_of(page, "#lspServers"))[:60]])
    await page.keyboard.press("Escape")

    await page.click(".itab[data-itab='code']")
    await page.click("#procStatus")
    check("J 活动", "J6 状态栏进程按钮切到活动面板",
          "active" in (await page.get_attribute(
              ".itab[data-itab='activity']", "class") or ""))
    await page.close()


# -------------------------------------------------------- K. terminal ----
async def area_terminal(browser):
    page = await new_page(browser)
    await page.click(".itab[data-itab='terminal']")
    await page.wait_for_function(
        "() => window.__rh.state.terminal.id > 0 && window.__rh.state.terminal.open",
        timeout=15000)
    first_id = await page.evaluate("() => window.__rh.state.terminal.id")
    await page.wait_for_function(
        """() => {
             const t = window.__rh.state.terminal.term;
             if (!t) return false;
             const b = t.buffer.active;
             for (let i = 0; i < b.length; i++) {
               const line = b.getLine(i);
               if (line && line.translateToString(true).includes('#')) return true;
             }
             return false;
           }""", timeout=15000)
    info = await text_of(page, "#termInfo")
    check("K 终端", "K1 打开终端后出现 shell 提示符",
          first_id > 0 and "·" in info, [first_id, info])

    await page.locator("#termWrap").click()
    await page.keyboard.type("echo ui-marker-1")
    await page.keyboard.press("Enter")
    await page.wait_for_function(
        """() => {
             const t = window.__rh.state.terminal.term;
             const b = t.buffer.active;
             for (let i = 0; i < b.length; i++) {
               const line = b.getLine(i);
               if (line && line.translateToString(true).includes('ui-marker-1')) {
                 if (line.translateToString(true).includes('echo')) continue;
                 return true;
               }
             }
             return false;
           }""", timeout=15000)
    check("K 终端", "K2 键入命令后输出回到屏幕", True)

    await page.keyboard.type("sleep 30")
    await page.keyboard.press("Enter")
    await page.wait_for_timeout(600)
    await page.click("#termCtrlC")
    await page.wait_for_timeout(800)
    await page.keyboard.type("echo after-ctrl-c")
    await page.keyboard.press("Enter")
    await page.wait_for_function(
        """() => {
             const t = window.__rh.state.terminal.term;
             const b = t.buffer.active;
             for (let i = 0; i < b.length; i++) {
               const line = b.getLine(i);
               if (line && line.translateToString(true).includes('after-ctrl-c')
                   && !line.translateToString(true).includes('echo')) return true;
             }
             return false;
           }""", timeout=15000)
    check("K 终端", "K3 Ctrl+C 中断前台命令", True)

    await page.click("#termClear")
    await page.wait_for_timeout(300)
    empty = await page.evaluate("""() => {
        const t = window.__rh.state.terminal.term;
        const b = t.buffer.active;
        for (let i = 0; i < b.length; i++) {
          const line = b.getLine(i);
          if (line && line.translateToString(true).includes('after-ctrl-c')) return false;
        }
        return true;
    }""")
    check("K 终端", "K4 清屏清掉回看内容", empty)

    await page.click("#termRestart")
    await page.wait_for_function(
        "(old) => window.__rh.state.terminal.id > old && window.__rh.state.terminal.open",
        arg=first_id, timeout=15000)
    second_id = await page.evaluate("() => window.__rh.state.terminal.id")
    await page.wait_for_timeout(700)
    old_gone = await page.evaluate("""() => {
        const t = window.__rh.state.terminal.term;
        const b = t.buffer.active;
        for (let i = 0; i < b.length; i++) {
          const line = b.getLine(i);
          if (line && line.translateToString(true).includes('ui-marker-1')) return false;
        }
        return true;
    }""")
    await page.locator("#termWrap").click()
    await page.keyboard.type("echo restart-ok")
    await page.keyboard.press("Enter")
    await page.wait_for_function(
        """() => {
             const t = window.__rh.state.terminal.term;
             const b = t.buffer.active;
             for (let i = 0; i < b.length; i++) {
               const line = b.getLine(i);
               if (line && line.translateToString(true).includes('restart-ok')
                   && !line.translateToString(true).includes('echo')) return true;
             }
             return false;
           }""", timeout=15000)
    check("K 终端", "K5 重启换新 id、旧输出不残留",
          second_id > first_id and old_gone and True, [first_id, second_id, old_gone])

    await page.context.grant_permissions(["clipboard-read", "clipboard-write"])
    await page.evaluate("() => window.__rh.state.terminal.term.clearSelection()")
    await page.click("#termCopy")
    await page.wait_for_timeout(300)
    clip = await page.evaluate("() => navigator.clipboard.readText()")
    check("K 终端", "K6 未选中时复制整屏", "restart-ok" in clip, clip[:40])

    cols_before = await page.evaluate("() => window.__rh.state.terminal.cols")
    await page.set_viewport_size({"width": 1100, "height": 900})
    await page.wait_for_timeout(800)
    cols_after = await page.evaluate("() => window.__rh.state.terminal.cols")
    check("K 终端", "K7 窗口变化后重新适配列数",
          cols_after != cols_before and cols_after > 0,
          [cols_before, cols_after])
    await page.set_viewport_size({"width": 1440, "height": 900})
    await page.close()


# -------------------------------------------------------- L. sessions ----
async def area_sessions(browser):
    page = await new_page(browser)
    await turn(page, "hello session-one")
    await page.click("#newBtn")
    await page.wait_for_function("() => window.__rh.state.sessionId",
                                 timeout=15000)
    await turn(page, "hello session-two")
    current = await page.evaluate("() => window.__rh.state.sessionId")

    await page.click("#sessionChip")
    await page.wait_for_selector("#sessions .session-item", timeout=8000)
    items = await page.locator("#sessions .session-item").count()
    api = await page.evaluate("async () => (await (await fetch('/api/sessions')).json()).sessions.length")
    active = await page.locator("#sessions .session-item.active").count()
    check("L 会话", "L1 会话列表与 API 一致并标记当前会话",
          items == api and items >= 2 and active == 1, [items, api, active])

    await page.click("#refreshSessions")
    await page.wait_for_timeout(500)
    check("L 会话", "L2 刷新按钮重新拉取列表",
          await page.locator("#sessions .session-item").count() == items)

    # `.active` sits on the <li> itself, so `has_not` would not exclude it.
    active_text = await page.locator("#sessions .session-item.active").inner_text()
    check("L 会话", "L2b 活动项标着当前会话",
          current[:8] in active_text, [current[:8], active_text.replace("\n", " | ")])
    other = page.locator("#sessions .session-item:not(.active)").first
    titles = await page.locator("#sessions .s-title").all_inner_texts()
    await other.click()
    await page.wait_for_function(
        "(id) => window.__rh.state.sessionId && window.__rh.state.sessionId !== id",
        arg=current, timeout=15000)
    switched = await page.evaluate("() => window.__rh.state.sessionId")
    has_user = await page.locator(".msg.user").count()
    check("L 会话", "L3 点击会话恢复历史消息",
          switched != current and has_user >= 1, [titles[:2], switched])

    page.on("dialog", lambda dialog: asyncio.ensure_future(dialog.accept()))
    before = await page.locator("#sessions .session-item").count()
    inactive = page.locator("#sessions .session-item:not(.active)").first
    await inactive.locator(".s-del").click()
    await page.wait_for_timeout(1200)
    after = await page.locator("#sessions .session-item").count()
    check("L 会话", "L4 删除非当前会话需要确认",
          after == before - 1 and await wait_toast(page, "会话已删除", 2000),
          [before, after])

    await page.locator("#sessions .session-item.active .s-del").click()
    await page.wait_for_timeout(600)
    check("L 会话", "L5 当前会话不可删除",
          await wait_toast(page, "当前活动会话无法删除", 2000)
          and await page.locator("#sessions .session-item").count() == after,
          await toast_text(page))
    await page.close()


# ------------------------------------------- M. todos, stats, events ----
async def area_events(browser):
    page = await new_page(browser)
    await turn(page, "todo 建一个待办")
    await page.wait_for_timeout(500)
    card = await page.is_visible("#todoCard")
    count = await text_of(page, "#todoCount")
    statuses = await page.locator("#todoList .tlabel").all_inner_texts()
    check("M 事件", "M1 待办卡片：计数与三种状态",
          card and count == "1/3" and statuses == ["已完成", "进行中", "待处理"],
          [card, count, statuses])

    await page.click("#todoHead")
    collapsed = "closed" in (await page.get_attribute("#todoCard", "class") or "")
    stored = await page.evaluate("() => localStorage.getItem('rh_todo_closed')")
    await page.click("#todoHead")
    check("M 事件", "M2 待办折叠状态被记住",
          collapsed and stored == "1"
          and "closed" not in (await page.get_attribute("#todoCard", "class") or ""))

    await inject(page, {"type": "token_usage", "input_tokens": 12345,
                        "output_tokens": 678, "context_tokens": 15000,
                        "context_limit": 32768, "cost_usd": 0.0123,
                        "cache_hit_pct": 42})
    check("M 事件", "M3 token 统计：上下文、in/out、缓存、费用",
          await text_of(page, "#ctxText") == "46%"
          and "15k/33k" in await text_of(page, "#ctxNums")
          and await text_of(page, "#statIn") == "12k"
          and await text_of(page, "#statOut") == "678"
          and "42%" in await text_of(page, "#statCache")
          and await text_of(page, "#statCost") == "$0.0123",
          [await text_of(page, "#ctxText"), await text_of(page, "#ctxNums"),
           await text_of(page, "#statIn"), await text_of(page, "#statOut"),
           await text_of(page, "#statCache"), await text_of(page, "#statCost")])

    warn_width = await page.evaluate(
        "() => parseFloat(document.getElementById('ctxFill').style.width)")
    await inject(page, {"type": "token_usage", "input_tokens": 1, "output_tokens": 1,
                        "context_tokens": 32000, "context_limit": 32768})
    check("M 事件", "M4 上下文超过 85% 时进度条告警",
          abs(warn_width - 45.8) < 0.2
          and "warn" in (await page.get_attribute("#ctxFill", "class") or "")
          and await text_of(page, "#ctxText") == "98%",
          [warn_width, await page.get_attribute("#ctxFill", "class"),
           await text_of(page, "#ctxText")])

    await inject(page, {"type": "notice", "level": "info",
                        "message": "model changed to fake/fake-b"})
    chip = await text_of(page, "#modelBtnValue")
    model_toast = await wait_toast(page, "模型已切换", 2000)
    await inject(page, {"type": "notice", "level": "info",
                        "message": "workspace changed to " + str(WS / "src")})
    ws_toast = await wait_toast(page, "工作目录已切换", 2000)
    await inject(page, {"type": "notice", "level": "warning", "message": "普通警告"})
    notes = [i["text"] for i in await feed(page) if i["kind"] == "note"]
    check("M 事件", "M5 notice 前缀映射到 chip 与 toast，其余进消息流",
          chip == "fake/fake-b" and model_toast and ws_toast
          and any("普通警告" in n for n in notes), [chip, model_toast, ws_toast])

    await inject(page, {"type": "chase", "attempt": 2, "pending": ["a", "b"]})
    await inject(page, {"type": "process_exited", "pid": 4242,
                        "status": "failed", "exit_code": 1})
    await inject(page, {"type": "download_status", "server_id": "pyright",
                        "state": "downloading", "percent": 40})
    notes = [i["text"] for i in await feed(page) if i["kind"] == "note"]
    check("M 事件", "M6 chase/进程失败/下载进度都写成提示",
          any("待办追问 2" in n for n in notes)
          and any("exit 1" in n for n in notes)
          and any("pyright" in n and "40%" in n for n in notes),
          notes[-3:])

    await inject(page, {"type": "download_consent", "id": "d1",
                        "display_name": "pyright", "version": "1.2.3",
                        "url": "https://example.com/pyright.tar.gz",
                        "runtime_hint": "Node.js 18"})
    consent = await text_of(page, "#consentModal")
    await page.click("#allowBtn")
    notes = [i["text"] for i in await feed(page) if i["kind"] == "note"]
    allow_note = any("已允许下载语言服务器" in n for n in notes)
    await inject(page, {"type": "download_consent", "id": "d2",
                        "display_name": "pyright", "version": "1.2.3",
                        "url": "https://example.com/x", "runtime_hint": ""})
    await page.keyboard.press("Escape")
    notes = [i["text"] for i in await feed(page) if i["kind"] == "note"]
    check("M 事件", "M7 下载同意对话框：允许与 Esc 拒绝",
          "pyright 1.2.3" in consent and "Node.js 18" in consent and allow_note
          and any("已拒绝下载语言服务器" in n for n in notes)
          and not await page.is_visible("#consentModal"), consent[:40].replace("\n", " "))

    await page.click("#settingsBtn")
    await page.wait_for_timeout(400)
    await inject(page, {"type": "error", "message": "设置写入失败"})
    check("M 事件", "M8 对话框打开时错误同时进消息流与 toast",
          await wait_toast(page, "设置写入失败", 2000)
          and any("设置写入失败" in i["text"] for i in await feed(page)),
          await toast_text(page))
    await page.keyboard.press("Escape")

    # mobile drawers
    await page.set_viewport_size({"width": 430, "height": 900})
    await page.wait_for_timeout(400)
    await page.click("#leftToggle")
    opened = "left-open" in (await page.get_attribute("body", "class") or "")
    scrim = await page.is_visible("#scrim")
    # The drawer covers the middle of the screen; the scrim is only reachable
    # on the strip beside it, exactly as for a real finger.
    await page.mouse.click(420, 300)
    closed = "left-open" not in (await page.get_attribute("body", "class") or "")
    await page.click("#rightToggle")
    right = "right-open" in (await page.get_attribute("body", "class") or "")
    await page.keyboard.press("Escape")
    check("M 事件", "M9 窄屏抽屉：打开、遮罩关闭、Esc 关闭",
          opened and scrim and closed and right
          and "right-open" not in (await page.get_attribute("body", "class") or ""),
          [opened, scrim, closed, right])
    await page.set_viewport_size({"width": 1440, "height": 900})
    await page.close()


# ------------------------------------------------------------ N. auth ----
async def area_auth(browser):
    errors = []
    page = await browser.new_page(viewport={"width": 1440, "height": 900})
    page.on("pageerror", lambda e: errors.append(str(e)))
    await page.goto(f"http://127.0.0.1:{AUTH_PORT}/")
    await page.wait_for_timeout(2500)
    modal = await page.is_visible("#settingsModal")
    message = await text_of(page, "#settingsError")
    status = await text_of(page, "#statusText")
    check("N 鉴权", "N1 无令牌：提示输入令牌且连接失败",
          modal and "令牌" in message and status != "已连接",
          [modal, message, status])

    await page.evaluate("() => localStorage.setItem('rh_token', 'wrong-token')")
    await page.reload()
    await page.wait_for_timeout(2500)
    wrong = await text_of(page, "#statusText")
    check("N 鉴权", "N2 错误令牌连不上", wrong != "已连接", wrong)

    page2 = await browser.new_page(viewport={"width": 1440, "height": 900})
    page2.on("pageerror", lambda e: errors.append(str(e)))
    await page2.goto(f"http://127.0.0.1:{AUTH_PORT}/?debug=1&token={TOKEN}")
    await page2.wait_for_function(
        "() => document.querySelector('#connPill').classList.contains('online')",
        timeout=15000)
    url = page2.url
    stored = await page2.evaluate("() => localStorage.getItem('rh_token')")
    check("N 鉴权", "N3 正确令牌连接成功且从地址栏消失",
          "token" not in url and stored == TOKEN and not errors, [url, stored])

    await inject(page2, {"type": "user_message", "text": "带令牌的消息"})
    check("N 鉴权", "N4 带令牌时消息正常渲染",
          await page2.locator(".msg.user").count() == 1)

    # dropping the socket must offer a way back
    await page2.evaluate("() => window.__rh.state.ws.close()")
    await page2.wait_for_timeout(900)
    reconnect = await page2.is_visible("#reconnectBtn")
    check("N 鉴权", "N5 连接断开后显示重连按钮与提示",
          reconnect and "已断开" in await text_of(page2, "#statusText"),
          await text_of(page2, "#statusText"))
    await page.close()
    await page2.close()


# --------------------------------------------------------------- main ----
async def main():
    global ONLY
    parser = argparse.ArgumentParser()
    parser.add_argument("--area", default=None,
                        help="only run one area (boot, composer, feed, rollback, "
                             "settings, models, cwd, tree, viewer, activity, "
                             "terminal, sessions, events, auth)")
    parser.add_argument("--no-start", action="store_true",
                        help="use an already running server on port 7499")
    parser.add_argument("--keep", action="store_true",
                        help="leave the servers running after the run")
    parser.add_argument("--headed", action="store_true")
    args = parser.parse_args()
    ONLY = args.area

    if not BINARY.exists():
        sys.exit(f"missing {BINARY}; run: cargo build -p rustedharness-cli")
    make_fixture()

    servers = []
    if not args.no_start:
        servers.append(subprocess.Popen(
            [sys.executable, str(ROOT / "script" / "fake-openai.py"),
             "--port", str(STUB_PORT), "--log", str(SCRATCH / "stub.log")],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        common = dict(stdout=open(SCRATCH / "server.log", "w"),
                      stderr=subprocess.STDOUT, start_new_session=True)
        servers.append(subprocess.Popen(
            [str(BINARY), "--config", str(SCRATCH), "--cwd", str(WS),
             "serve", "--port", str(MAIN_PORT)], **common))
        servers.append(subprocess.Popen(
            [str(BINARY), "--config", str(SCRATCH), "--cwd", str(WS),
             "serve", "--port", str(AUTH_PORT)],
            env={**os.environ, "RUSTEDHARNESS_TOKEN": TOKEN},
            stdout=open(SCRATCH / "server-auth.log", "w"),
            stderr=subprocess.STDOUT, start_new_session=True))
        import urllib.error
        import urllib.request

        def ready(url, expect_unauthorized=False):
            try:
                urllib.request.urlopen(url, timeout=1)
                return not expect_unauthorized
            except urllib.error.HTTPError as err:
                return expect_unauthorized and err.code == 401
            except Exception:
                return False

        for _ in range(80):
            if (ready(f"{MAIN}/api/config")
                    and ready(f"http://127.0.0.1:{AUTH_PORT}/api/config",
                              expect_unauthorized=True)):
                break
            time.sleep(0.25)
        else:
            for proc in servers:
                proc.terminate()
            sys.exit(f"servers did not come up; is something else on "
                     f"{STUB_PORT}/{MAIN_PORT}/{AUTH_PORT}? see {SCRATCH}/server.log")
        time.sleep(0.5)

    started = time.time()
    async with async_playwright() as pw:
        browser = await pw.chromium.launch(headless=not args.headed)
        await run_area("boot", lambda: area_boot(browser))
        await run_area("composer", lambda: area_composer(browser))
        await run_area("feed", lambda: area_feed(browser))
        await run_area("rollback", lambda: area_rollback(browser))
        await run_area("settings", lambda: area_settings(browser))
        await run_area("models", lambda: area_models(browser))
        await run_area("cwd", lambda: area_cwd(browser))
        await run_area("tree", lambda: area_tree(browser))
        await run_area("viewer", lambda: area_viewer(browser))
        await run_area("activity", lambda: area_activity(browser))
        await run_area("terminal", lambda: area_terminal(browser))
        await run_area("sessions", lambda: area_sessions(browser))
        await run_area("events", lambda: area_events(browser))
        await run_area("auth", lambda: area_auth(browser))
        await browser.close()

    if not args.keep:
        for proc in servers:
            try:
                proc.terminate()
            except Exception:
                pass

    if area("boot") or ONLY is None:
        check("Z 全程", "Z1 所有页面都没有未捕获的 JS 报错",
              not PAGE_ERRORS, PAGE_ERRORS[:3])

    passed = [r for r in RESULTS if r["ok"]]
    failed = [r for r in RESULTS if not r["ok"]]
    for result in RESULTS:
        mark = "ok  " if result["ok"] else "FAIL"
        print(f"{mark} {result['group']} · {result['name']}"
              + ("" if result["ok"] else f"   <{result['detail']}>"))
    print(f"\n{len(passed)}/{len(RESULTS)} 通过 · {time.time() - started:.1f}s")
    if failed:
        print("失败项：")
        for result in failed:
            print(f"  - {result['name']}: {result['detail']}")
        sys.exit(1)


if __name__ == "__main__":
    asyncio.run(main())
