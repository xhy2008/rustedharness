#!/usr/bin/env python3
"""Send every tool call through the agent and check what came back.

Each case is one real turn: the prompt is `call {"name": ..., "arguments": ...}`,
`script/fake-openai.py` turns it into a tool call, the harness executes it, and
the script asserts the `tool_result` event plus the workspace on disk. No model,
no API key, no browser.

    python3 script/tool-test.py [--area edit] [--list] [--keep]

Requirements: a built binary (`cargo build -p rustedharness-cli`) and python.
`docs/tool-testing.md` explains the matrix and what a failure means.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / "rustedharness"
SCRATCH = Path("/tmp/rh-tool-test")
WS = SCRATCH / "ws"
CFG = SCRATCH / "cfg"
STUB_PORT = 7497
SEARX_PORT = 7496

RESULTS = []
ONLY = None

BASE = {
    "src/main.rs": """// 工具测试用的 Rust 文件
use std::collections::BTreeMap;

const LIMIT: usize = 42;

fn main() {
    let mut map: BTreeMap<String, usize> = BTreeMap::new();
    map.insert("answer".to_string(), LIMIT);
    println!("{:?}", map);
}

fn helper() -> u64 {
    LIMIT as u64
}
""",
    "src/second.rs": """fn duplicate() -> u32 {
    1
}
""",
    "notes.txt": "alpha line one\nbeta line two\ngamma line three\n",
    "data.json": '{"name":"fixture","ok":true}\n',
    "no-newline.txt": "no trailing newline",
    "empty.txt": "",
    ".hidden.md": "# hidden\n\nhidden files are ignored by grep and glob\n",
    "ignored.txt": "ignored file with the word alpha\n",
    ".gitignore": "ignored.txt\n",
}
BINARY_FILE = bytes([0, 1, 2, 0, 255, 254, 0, 65])


# --------------------------------------------------------------- fixture ----
def reset():
    if WS.exists():
        shutil.rmtree(WS)
    WS.mkdir(parents=True)
    for rel, text in BASE.items():
        path = WS / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    (WS / "binary.bin").write_bytes(BINARY_FILE)
    (WS / "big.txt").write_text("x" * 400_000)
    make_images()


def png(width, height, path, colour=(90, 40, 200)):
    """A minimal PNG, so image reading does not need Pillow."""
    import struct
    import zlib

    row = bytes(colour) * width
    raw = b"".join(b"\x00" + row for _ in range(height))

    def chunk(kind, payload):
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(
            ">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
                     + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def make_images():
    png(64, 48, WS / "tiny.png")
    # 9000x5000 = 45 megapixels, past imaging::MAX_PIXELS (40 M).
    png(9000, 5000, WS / "huge.png")


def write_config(deny=None):
    CFG.mkdir(parents=True, exist_ok=True)
    (CFG / "models.json").write_text(json.dumps({
        "providers": {"fake": {
            "name": "Fake",
            "baseUrl": f"http://127.0.0.1:{STUB_PORT}/v1",
            "apiKey": "stub-key",
            "api": "openai-completions",
            "models": [{"id": "fake-a", "name": "Fake A", "contextWindow": 32768}],
        }},
    }, indent=2))
    config = {
        "model": "fake/fake-a",
        "agent": {"rateLimitRetries": 0},
        "search": {"backend": "searxng",
                   "searxngUrl": f"http://127.0.0.1:{SEARX_PORT}/search"},
        "lsp": {"requestTimeoutMs": 800},
        "shell": {"maxOutputBytes": 200 * 1024},
    }
    if deny:
        config["denyTools"] = deny
    (CFG / "config.json").write_text(json.dumps(config, indent=2))
    for name, tools in (("deny.json", ["write"]),):
        denied = dict(config)
        denied["denyTools"] = tools
        denied["providers"] = json.loads((CFG / "models.json").read_text())["providers"]
        (SCRATCH / name).write_text(json.dumps(denied, indent=2))


# ---------------------------------------------------------------- runner ----
def check(group, name, ok, detail=""):
    RESULTS.append({"group": group, "name": name, "ok": bool(ok),
                    "detail": str(detail)[:400]})


def area(name):
    return ONLY is None or ONLY == name


def run_turn(prompt, config=None, timeout=90, yes=False):
    """Run one turn and return (events, stderr, returncode)."""
    cmd = [str(BINARY), "--config", config or str(CFG), "--cwd", str(WS),
           "run", "--json"]
    env = {**os.environ, "RUSTEDHARNESS_DIR": str(SCRATCH)}
    if yes:
        cmd.append("--yes")
    cmd.append(prompt)
    started = time.time()
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout,
                          env=env)
    events = []
    for line in proc.stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
    return events, proc.stderr, proc.returncode, time.time() - started


def tool_prompt(tool, arguments):
    return "call " + json.dumps({"name": tool, "arguments": arguments},
                                ensure_ascii=False)


# ------------------------------------------------------------------ cases ----
# name, tool, arguments, and what the result must look like:
#   ok        expected tool_result.ok
#   code      expected E_* inside the result
#   has       substrings that must appear
#   absent    substrings that must not appear
#   files     {path: (text)} exact content after the turn,
#             {path: ("~", text)} substring
#   missing   paths that must not exist
#   meta      expected metadata subset
#   events    event types that must appear (e.g. download_consent)
#   config    "--config" override
CASES = [
    # ---- read -------------------------------------------------------------
    dict(area="read", name="窗口：offset/limit 只返回该范围",
         tool="read", args={"path": "src/main.rs", "offset": 2, "limit": 2},
         has=["2#", "3#", "use std::collections::BTreeMap;"], absent=["4#", "5#"],
         meta={"kind": "file", "start": 2, "end": 3, "truncated": False}),
    dict(area="read", name="raw=true 去掉行号与锚点",
         tool="read", args={"path": "no-newline.txt", "raw": True},
         has=["no trailing newline"], absent=["|"]),
    dict(area="read", name="目录：列出条目并用 / 标目录",
         tool="read", args={"path": "src"},
         has=["entries", "main.rs", "second.rs"], meta={"kind": "directory"}),
    dict(area="read", name="空文件仍是合法读取",
         tool="read", args={"path": "empty.txt"}, has=["0 lines"]),
    dict(area="read", name="文件不存在 -> E_NOT_FOUND",
         tool="read", args={"path": "nope.txt"}, ok=False, code="E_NOT_FOUND"),
    dict(area="read", name="二进制文件 -> E_BINARY",
         tool="read", args={"path": "binary.bin"}, ok=False, code="E_BINARY"),
    dict(area="read", name="offset 超出末尾 -> E_OUT_OF_RANGE",
         tool="read", args={"path": "notes.txt", "offset": 99}, ok=False,
         code="E_OUT_OF_RANGE"),
    dict(area="read", name="max_bytes 截断并说明原因",
         tool="read", args={"path": "src/main.rs", "max_bytes": 60},
         has=["byte limit reached"], meta={"truncated": True}),
    dict(area="read", name="图片走附件而不是字节",
         tool="read", args={"path": "tiny.png"},
         has=["[image]", "64x48"], meta={"kind": "image", "width": 64, "height": 48},
         log=['"image_url"']),
    dict(area="read", name="超过 4000 万像素的图片被拒绝",
         tool="read", args={"path": "huge.png"}, ok=False, code="E_IMAGE_TOO_LARGE"),
    dict(area="read", name="工作区之外 -> E_PATH_ESCAPE",
         tool="read", args={"path": "/etc/passwd"}, ok=False, code="E_PATH_ESCAPE"),
    dict(area="read", name="URL 读成 markdown（本地搜素桩）",
         tool="read", args={"path": f"http://127.0.0.1:{SEARX_PORT}/search?q=rust",
                           "use_compact_model": False},
         meta={"kind": "url"}),
    dict(area="read", name="URL 走 compact 模型提炼",
         tool="read", args={"path": f"http://127.0.0.1:{SEARX_PORT}/search?q=rust",
                           "prompt": "说重点"}, meta={"kind": "url"}),
    dict(area="read", name="同一 turn 内第二次读 URL 命中缓存",
         tool="read", calls=[
             {"name": "read", "arguments": {
                 "path": f"http://127.0.0.1:{SEARX_PORT}/search?q=cache",
                 "use_compact_model": False}},
             {"name": "read", "arguments": {
                 "path": f"http://127.0.0.1:{SEARX_PORT}/search?q=cache",
                 "use_compact_model": False}}],
         meta={"kind": "url", "cache_hit": True}),
    dict(area="read", name="单行超长文件按 max_bytes 截断",
         tool="read", args={"path": "big.txt"},
         has=["[line truncated at max_bytes]"], absent=["tool output was"],
         meta={"truncated": True, "total_lines": 1}),

    # ---- write ------------------------------------------------------------
    dict(area="write", name="新建文件（含父目录）",
         tool="write", args={"path": "made/new.txt", "content": "hello\n"},
         has=["created"], meta={"action": "created"},
         files={"made/new.txt": "hello\n"}),
    dict(area="write", name="已存在时追加并给出行号",
         tool="write", args={"path": "notes.txt", "content": "delta\n"},
         has=["文件已存在，内容已追加", "starting line: 4"],
         meta={"action": "appended", "start_line": 4},
         files={"notes.txt": "alpha line one\nbeta line two\ngamma line three\ndelta\n"}),
    dict(area="write", name="追加时补齐缺失的换行",
         tool="write", args={"path": "no-newline.txt", "content": "tail\n"},
         files={"no-newline.txt": "no trailing newline\ntail\n"}),
    dict(area="write", name="overwrite=true 原子替换",
         tool="write", args={"path": "notes.txt", "content": "replaced\n",
                            "overwrite": True},
         has=["overwrote"], meta={"action": "overwritten"},
         files={"notes.txt": "replaced\n"}),
    dict(area="write", name="目标是目录 -> E_IS_DIR",
         tool="write", args={"path": "src", "content": "x"}, ok=False,
         code="E_IS_DIR"),
    dict(area="write", name="denyTools 拒绝 -> E_TOOL_DENIED",
         tool="write", args={"path": "denied.txt", "content": "x"}, ok=False,
         code="E_TOOL_DENIED", config=str(SCRATCH / "deny.json"),
         missing=["denied.txt"]),

    # ---- edit -------------------------------------------------------------
    dict(area="edit", name="replace 单行",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace", "pos": 2, "lines": ["BETA"]}]}]},
         has=["=== notes.txt ===", "BETA"],
         files={"notes.txt": "alpha line one\nBETA\ngamma line three\n"}),
    dict(area="edit", name="replace 行范围",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace", "pos": 1, "end": 3, "lines": ["one", "two"]}]}]},
         files={"notes.txt": "one\ntwo\n"}),
    dict(area="edit", name="insert_before / insert_after",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "insert_before", "pos": 1, "lines": ["before"]},
             {"op": "insert_after", "pos": 3, "lines": ["after"]}]}]},
         files={"notes.txt": "before\nalpha line one\nbeta line two\n"
                             "gamma line three\nafter\n"}),
    dict(area="edit", name="delete 行",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "delete", "pos": 2}]}]},
         files={"notes.txt": "alpha line one\ngamma line three\n"}),
    dict(area="edit", name="replace_text 唯一匹配",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace_text", "pos": 1, "oldText": "beta",
              "newText": "BETA"}]}]},
         files={"notes.txt": "alpha line one\nBETA line two\ngamma line three\n"}),
    dict(area="edit", name="replace_text all=true 替换全部",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace_text", "pos": 1, "oldText": "line",
              "newText": "row", "all": True}]}]},
         files={"notes.txt": "alpha row one\nbeta row two\ngamma row three\n"}),
    dict(area="edit", name="replace_text 不唯一且未加 all -> 报错",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace_text", "pos": 1, "oldText": "line",
              "newText": "row"}]}]},
         ok=False, code="E_INVALID_PATCH",
         files={"notes.txt": "alpha line one\nbeta line two\ngamma line three\n"}),
    dict(area="edit", name="replace_text 没有匹配 -> 报错",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace_text", "pos": 1, "oldText": "不存在",
              "newText": "x"}]}]},
         ok=False, code="E_INVALID_PATCH"),
    dict(area="edit", name="replace_text 与其它编辑同批 -> 报错",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace_text", "pos": 1, "oldText": "alpha",
              "newText": "ALPHA"},
             {"op": "delete", "pos": 3}]}]},
         ok=False, code="E_INVALID_PATCH"),
    dict(area="edit", name="读到的锚点可以直接用于编辑",
         tool="edit", special="anchor_roundtrip"),
    dict(area="edit", name="过期锚点 -> E_STALE_ANCHOR",
         tool="edit", args={"files": [{"path": "src/main.rs", "edits": [
             {"op": "replace", "pos": "5#ZZ", "lines": ["// patched"]}]}]},
         ok=False, code="E_STALE_ANCHOR"),
    dict(area="edit", name="pos 不是数字也不是锚点 -> E_INVALID_PATCH",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace", "pos": "abc", "lines": ["x"]}]}]},
         ok=False, code="E_INVALID_PATCH"),
    dict(area="edit", name="行号越界 -> E_INVALID_PATCH",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace", "pos": 99, "lines": ["x"]}]}]},
         ok=False, code="E_INVALID_PATCH"),
    dict(area="edit", name="同一位置两次插入 -> E_OVERLAP",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "insert_after", "pos": 1, "lines": ["a"]},
             {"op": "insert_before", "pos": 2, "lines": ["b"]}]}]},
         ok=False, code="E_OVERLAP"),
    dict(area="edit", name="files[] 为空 -> E_INVALID_PATCH",
         tool="edit", args={"files": []}, ok=False, code="E_INVALID_PATCH"),
    dict(area="edit", name="dry_run 不写盘",
         tool="edit", args={"dry_run": True, "files": [{"path": "notes.txt",
             "edits": [{"op": "replace", "pos": 1, "lines": ["nope"]}]}]},
         has=["dry_run"], meta={"dry_run": True},
         files={"notes.txt": "alpha line one\nbeta line two\ngamma line three\n"}),
    dict(area="edit", name="多文件一次调用",
         tool="edit", args={"files": [
             {"path": "notes.txt", "edits": [
                 {"op": "replace", "pos": 1, "lines": ["first"]}]},
             {"path": "src/second.rs", "edits": [
                 {"op": "replace", "pos": 1,
                  "lines": ["fn renamed() -> u32 {"]}]},
         ]},
         has=["=== notes.txt ===", "=== src/second.rs ==="],
         files={"notes.txt": "first\nbeta line two\ngamma line three\n",
                "src/second.rs": "fn renamed() -> u32 {\n    1\n}\n"}),
    dict(area="edit", name="多文件里一个失败则全都不写",
         tool="edit", args={"files": [
             {"path": "notes.txt", "edits": [{"op": "delete", "pos": 1}]},
             {"path": "src/second.rs", "edits": [{"op": "delete", "pos": 99}]}]},
         ok=False, code="E_INVALID_PATCH",
         files={"notes.txt": "alpha line one\nbeta line two\ngamma line three\n"}),
    dict(area="edit", name="无改动的补丁返回 no changes",
         tool="edit", args={"files": [{"path": "notes.txt", "edits": [
             {"op": "replace", "pos": 1, "lines": ["alpha line one"]}]}]},
         has=["no changes"]),
    dict(area="edit", name="没有结尾换行的文件保持原样",
         tool="edit", args={"files": [{"path": "no-newline.txt", "edits": [
             {"op": "insert_after", "pos": 1, "lines": ["second"]}]}]},
         files={"no-newline.txt": "no trailing newline\nsecond"}),

    # ---- grep -------------------------------------------------------------
    dict(area="grep", name="content 模式：file:line:text",
         tool="grep", args={"pattern": "beta"},
         has=["notes.txt:2:beta line two"], meta={"mode": "content"}),
    dict(area="grep", name="anchors=true 输出 line#anchor|",
         tool="grep", args={"pattern": "beta", "anchors": True},
         has=["notes.txt:2#"]),
    dict(area="grep", name="files_with_matches",
         tool="grep", args={"pattern": "alpha", "output_mode": "files_with_matches"},
         has=["notes.txt"], absent=["alpha line one"]),
    dict(area="grep", name="count 模式",
         tool="grep", args={"pattern": "line", "output_mode": "count"},
         has=["notes.txt"], absent=["beta line two"]),
    dict(area="grep", name="case_insensitive",
         tool="grep", args={"pattern": "BETA", "case_insensitive": True},
         has=["beta line two"]),
    dict(area="grep", name="context 行",
         tool="grep", args={"pattern": "beta", "context": 1},
         has=["alpha line one", "gamma line three"]),
    dict(area="grep", name="limit + offset 翻页",
         tool="grep", args={"pattern": "line", "path": "notes.txt",
                            "limit": 1, "offset": 1},
         has=["notes.txt:2:beta line two"], absent=["alpha line one"]),
    dict(area="grep", name="glob 过滤",
         tool="grep", args={"pattern": "fn ", "glob": "**/*.rs"},
         has=["main.rs"], absent=["notes.txt"]),
    dict(area="grep", name="literal=true 把元字符当字面量",
         tool="grep", args={"pattern": "fn main(", "literal": True},
         has=["main.rs:6:fn main()"]),
    dict(area="grep", name="空 pattern -> E_INVALID_ARGUMENT",
         tool="grep", args={"pattern": ""}, ok=False, code="E_INVALID_ARGUMENT"),
    dict(area="grep", name="坏正则 -> E_INVALID_PATTERN",
         tool="grep", args={"pattern": "(["}, ok=False, code="E_INVALID_PATTERN"),
    dict(area="grep", name="遵守 .gitignore，跳过隐藏文件",
         tool="grep", args={"pattern": "alpha"},
         has=["notes.txt"], absent=["ignored.txt", ".hidden.md"]),
    dict(area="grep", name="offset 超过总数 -> no matches",
         tool="grep", args={"pattern": "line", "offset": 99},
         has=["no matches"]),
    dict(area="grep", name="没有匹配时说明 0 个",
         tool="grep", args={"pattern": "definitely-not-here"},
         has=["no matches"], meta={"matches": 0}),

    # ---- glob -------------------------------------------------------------
    dict(area="glob", name="**/*.rs 按修改时间倒序",
         tool="glob", args={"pattern": "**/*.rs"},
         has=["src/main.rs", "src/second.rs"], meta={"files": 2}),
    dict(area="glob", name="path 作用域 + limit",
         tool="glob", args={"pattern": "*.rs", "path": "src", "limit": 1},
         has=["src/"], absent=["notes.txt"],
         meta={"files": 2, "returned": 1}),
    dict(area="glob", name="**/*.rs 也能匹配作用域下的直接子文件",
         tool="glob", args={"pattern": "**/*.rs", "path": "src"},
         meta={"files": 2}),
    dict(area="glob", name="坏 glob -> E_INVALID_GLOB",
         tool="glob", args={"pattern": "["}, ok=False, code="E_INVALID_GLOB"),
    dict(area="glob", name="无匹配",
         tool="glob", args={"pattern": "**/*.zzz"},
         has=["(no files matched)"], meta={"files": 0}),

    # ---- shell ------------------------------------------------------------
    dict(area="bash", name="成功命令：退出码与输出",
         tool="bash", args={"command": "printf 'hello-from-bash\\n'"},
         has=["exit_code: 0", "hello-from-bash", "--- stats ---"],
         meta={"status": "exited", "exit_code": 0}),
    dict(area="bash", name="非零退出码仍是成功调用",
         tool="bash", args={"command": "exit 3"},
         has=["exit_code: 3"], meta={"exit_code": 3}),
    dict(area="bash", name="超时被标记为 timeout",
         tool="bash", args={"command": "sleep 5", "timeout_ms": 300},
         has=["status: timeout"], meta={"status": "timeout"}),
    dict(area="bash", name="后台运行返回 pid",
         tool="bash", args={"command": "sleep 1; echo done-later",
                            "run_in_background": True},
         has=["started in background", "pid:"], meta={"background": True}),
    dict(area="bash", name="空命令 -> E_INVALID_ARGUMENT",
         tool="bash", args={"command": "   "}, ok=False, code="E_INVALID_ARGUMENT"),
    dict(area="bash", name="ANSI 颜色被净化",
         tool="bash", args={"command": "printf '\\033[31mred\\033[0m\\n'"},
         has=["red"], absent=["\\033", "\x1b"]),
    dict(area="bash", name="\\r 进度行折叠成最后状态",
         tool="bash", args={"command": "printf 'step1\\rstep2\\rstep3\\n'"},
         has=["step3"], absent=["step1"]),
    dict(area="bash", name="cwd 锚定工作区",
         tool="bash", args={"command": "pwd"}, has=["/ws"]),
    dict(area="bash", name="capture=raw 附带 base64",
         tool="bash", args={"command": "printf 'raw-bytes\\n'", "capture": "raw"},
         meta={"status": "exited"}),
    dict(area="bash", name="超大输出溢写到文件而不是截断",
         tool="bash", args={"command": 'python3 -c "import sys; '
                            'sys.stdout.write(chr(121)*200000)"'},
         has=["too large to return", "grep or read that file"],
         verify="spilled"),

    # ---- await ------------------------------------------------------------
    # A background process belongs to the run that started it, so `await` has
    # to happen in the same turn: the stub substitutes `$pid` from the previous
    # tool result.
    dict(area="await", name="等待后台进程拿到退出码与输出",
         tool="await", special="await_background",
         calls=[{"name": "bash", "arguments": {
                    "command": "sleep 0.2; echo awaited-output",
                    "run_in_background": True}},
                {"name": "await", "arguments": {"pid": "$pid",
                                               "timeout_ms": 20000}}],
         has=["status: exited", "exit_code: 0", "awaited-output"]),
    dict(area="await", name="带超时的等待仍返回运行中",
         tool="await", special="await_timeout",
         calls=[{"name": "bash", "arguments": {"command": "sleep 30",
                                              "run_in_background": True}},
                {"name": "await", "arguments": {"pid": "$pid",
                                               "timeout_ms": 400}}],
         has=["status: running"]),
    dict(area="await", name="未知 pid 报错并列出存活进程",
         tool="await", args={"pid": 999999}, ok=False, code="E_UNKNOWN_PID",
         has=["live_pids"]),

    # ---- todo -------------------------------------------------------------
    dict(area="todo", name="replace：整表替换并回报",
         tool="todo", args={"items": [
             {"content": "第一步", "status": "completed"},
             {"content": "第二步", "status": "in_progress"}]},
         has=["第一步", "第二步"], meta={"pending": 1}),
    dict(area="todo", name="不带 items 就是查询",
         tool="todo", args={}, has=["items"]),
    dict(area="todo", name="merge=true 按 id 更新",
         tool="todo", args={"items": [{"id": "t1", "content": "改过的第一步",
                                       "status": "completed"}], "merge": True},
         has=["改过的第一步"]),
    dict(area="todo", name="merge=true 可以插入新条目",
         tool="todo", args={"items": [{"id": "t9", "content": "新插入",
                                       "status": "pending"}], "merge": True},
         has=["新插入"]),
    dict(area="todo", name="空 content -> E_INVALID_ARGUMENT",
         tool="todo", args={"items": [{"content": "  ", "status": "pending"}]},
         ok=False, code="E_INVALID_ARGUMENT"),

    # ---- lsp --------------------------------------------------------------
    dict(area="lsp", name="document_symbols（无服务器时走名字索引）",
         tool="lsp", args={"op": "document_symbols", "path": "src/main.rs"},
         has=["main", "helper"], meta={"match": "name-based"}),
    dict(area="lsp", name="hover 需要行号",
         tool="lsp", args={"op": "hover", "path": "src/main.rs", "line": 5,
                           "column": 4}, meta={"match": "name-based"}),
    dict(area="lsp", name="definition",
         tool="lsp", args={"op": "definition", "path": "src/main.rs", "line": 9,
                           "column": 5}, meta={"match": "name-based"}),
    dict(area="lsp", name="type_definition",
         tool="lsp", args={"op": "type_definition", "path": "src/main.rs",
                           "line": 9, "column": 5}, meta={"match": "name-based"}),
    dict(area="lsp", name="implementation",
         tool="lsp", args={"op": "implementation", "path": "src/main.rs",
                           "line": 12, "column": 4}, meta={"match": "name-based"}),
    dict(area="lsp", name="call_hierarchy",
         tool="lsp", args={"op": "call_hierarchy", "path": "src/main.rs",
                           "line": 12, "column": 4}, meta={"match": "name-based"}),
    dict(area="lsp", name="references",
         tool="lsp", args={"op": "references", "path": "src/main.rs", "line": 9,
                           "column": 5}, meta={"match": "name-based"}),
    dict(area="lsp", name="workspace_symbols 按名字查",
         tool="lsp", args={"op": "workspace_symbols", "path": "src/main.rs",
                           "symbol": "helper"}, has=["helper"]),
    dict(area="lsp", name="diagnostics",
         tool="lsp", args={"op": "diagnostics", "path": "src/main.rs"},
         has=["no diagnostics"]),
    dict(area="lsp", name="rename 默认只预览不写盘",
         tool="lsp", args={"op": "rename", "path": "src/main.rs", "line": 12,
                           "column": 4, "symbol": "renamed"},
         has=["preview"], files={"src/main.rs": ("~", "fn helper()")}),
    dict(area="lsp", name="无服务器时 rename apply=true 也不会改写",
         tool="lsp", args={"op": "rename", "path": "src/main.rs", "line": 12,
                           "column": 4, "symbol": "renamed", "apply": True},
         files={"src/main.rs": ("~", "fn helper()")}),
    dict(area="lsp", name="缺少 path -> E_INVALID_ARGUMENT",
         tool="lsp", args={"op": "hover", "line": 1}, ok=False,
         code="E_INVALID_ARGUMENT"),
    dict(area="lsp", name="缺少 line -> E_INVALID_ARGUMENT",
         tool="lsp", args={"op": "hover", "path": "src/main.rs"}, ok=False,
         code="E_INVALID_ARGUMENT"),
    dict(area="lsp", name="rename 缺少 symbol -> E_INVALID_ARGUMENT",
         tool="lsp", args={"op": "rename", "path": "src/main.rs", "line": 12},
         ok=False, code="E_INVALID_ARGUMENT"),
    dict(area="lsp", name="install 缺少 server -> E_INVALID_ARGUMENT",
         tool="lsp", args={"op": "install"}, ok=False, code="E_INVALID_ARGUMENT"),
    dict(area="lsp", name="目录不是文件 -> E_INVALID_ARGUMENT",
         tool="lsp", args={"op": "document_symbols", "path": "src"}, ok=False,
         code="E_INVALID_ARGUMENT"),

    # ---- web_search -------------------------------------------------------
    dict(area="web_search", name="搜索返回表格（来自本地 SearXNG 桩）",
         tool="web_search", args={"query": "rust"},
         has=["search results for `rust`", "doc.rust-lang.org", "rust-analyzer"]),
    dict(area="web_search", name="allowed_domains 只留指定域名",
         tool="web_search", args={"query": "rust", "allowed_domains": ["example.com"]},
         has=["example.com/rust"], absent=["doc.rust-lang.org"]),
    dict(area="web_search", name="blocked_domains 去掉指定域名",
         tool="web_search", args={"query": "rust", "blocked_domains": ["example.com"]},
         has=["doc.rust-lang.org"], absent=["example.com/rust"]),
    dict(area="web_search", name="num_results 限制条数",
         tool="web_search", args={"query": "rust", "num_results": 2},
         has=["rust-analyzer"]),
    dict(area="web_search", name="没有结果时明确说明",
         tool="web_search", args={"query": "empty"}, has=["no results"]),
    dict(area="web_search", name="后端 500 时映射成错误",
         tool="web_search", args={"query": "boom"}, ok=False),
    dict(area="web_search", name="空 query -> E_INVALID_ARGUMENT",
         tool="web_search", args={"query": "  "}, ok=False,
         code="E_INVALID_ARGUMENT"),

    # ---- registry ---------------------------------------------------------
    dict(area="registry", name="未知工具 -> E_UNKNOWN_TOOL",
         tool="move", args={"path": "notes.txt"}, ok=False, code="E_UNKNOWN_TOOL"),
]


# --------------------------------------------------------------- assertions ----
def last_stub_request():
    try:
        lines = (SCRATCH / "stub.log").read_text().splitlines()
    except OSError:
        return ""
    # The stub logs both request bodies and one HTTP access line per request.
    bodies = [line for line in lines if line.startswith("{")]
    return bodies[-1] if bodies else ""


def tool_results(events):
    return [e for e in events if e.get("type") == "tool_result"]


def describe(results):
    if not results:
        return "no tool_result event"
    result = results[-1]
    return f"ok={result.get('ok')} content={result.get('content', '')[:220]!r}"


def check_case(case):
    name = case["name"]
    group = case.get("area", "?")
    special = case.get("special")
    reset()

    if special == "anchor_roundtrip":
        events, _stderr, _rc, _t = run_turn(
            tool_prompt("read", {"path": "src/main.rs"}))
        content = "\n".join((r.get("content") or "") for r in tool_results(events))
        import re
        match = re.search(r"^6#([0-9A-Za-z]{2})\|", content, re.MULTILINE)
        if not match:
            check(group, name, False, "no 6#anchor in read output")
            return
        anchor = "6#" + match.group(1)
        events, _stderr, _rc, _t = run_turn(tool_prompt("edit", {"files": [
            {"path": "src/main.rs", "edits": [
                {"op": "replace", "pos": anchor,
                 "lines": ["fn main() { // patched by anchor"]}]}]}))
        results = tool_results(events)
        check(group, name,
              bool(results) and bool(results[-1].get("ok"))
              and "patched by anchor" in (WS / "src/main.rs").read_text(),
              describe(results))
        return

    if case.get("calls"):
        prompt = "call " + json.dumps(case["calls"], ensure_ascii=False)
        expected = case["calls"][-1].get("arguments") or {}
    else:
        prompt = tool_prompt(case["tool"], case["args"])
        expected = case["args"]
    events, stderr, code, _elapsed = run_turn(prompt, case.get("config"))
    calls = [e for e in events if e.get("type") == "tool_call"]
    if not calls:
        check(group, name, False, f"no tool_call; rc={code} stderr={stderr[-200:]}")
        return
    sent = json.loads(json.dumps(calls[-1].get("arguments"), sort_keys=True))
    # `$pid` becomes a real number on the way through the stub.
    if case.get("calls"):
        expected = {key: (sent.get(key) if value == "$pid" else value)
                    for key, value in expected.items()}
    args_ok = sent == json.loads(json.dumps(expected, sort_keys=True))

    results = tool_results(events)
    if not results:
        check(group, name, False, "no tool_result; " + describe(results))
        return
    result = results[-1]
    content = result.get("content") or ""
    metadata = result.get("metadata") or {}
    ok = bool(result.get("ok")) == bool(case.get("ok", True)) and args_ok

    if case.get("code"):
        ok = ok and case["code"] in content
    for needle in case.get("has", []):
        ok = ok and needle in content
    for needle in case.get("absent", []):
        ok = ok and needle not in content
    for key, want in case.get("meta", {}).items():
        have = metadata.get(key)
        ok = ok and (have == want if not isinstance(want, float)
                     else abs(float(have or 0) - want) < 1e-6)
    for path, want in case.get("files", {}).items():
        target = WS / path
        if isinstance(want, tuple):
            ok = ok and target.exists() and want[1] in target.read_text()
        else:
            ok = ok and target.exists() and target.read_text() == want
    for path in case.get("missing", []):
        ok = ok and not (WS / path).exists()
    if case.get("verify") == "spilled":
        import re
        match = re.search(r"the full text is in (\S+)\.log", content)
        spilled = Path(match.group(1) + ".log") if match else None
        ok = ok and bool(spilled) and spilled.exists() and spilled.stat().st_size > 130_000
    for needle in case.get("log", []):
        body = last_stub_request()
        ok = ok and needle in body

    detail = describe(results)
    if not ok:
        detail += f" | metadata={json.dumps(metadata, ensure_ascii=False)[:200]}"
        if not args_ok:
            detail += f" | arguments arrived as {sent}"
    check(group, name, ok, detail)


# ------------------------------------------------------------------- main ----
def port_free(port):
    import socket
    with socket.socket() as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            sock.bind(("127.0.0.1", port))
            return True
        except OSError:
            return False


def start_stub():
    for port in (STUB_PORT, SEARX_PORT):
        if not port_free(port):
            sys.exit(f"port {port} is in use; stop the old stub first "
                     f"(fuser -k -n tcp {port})")
    proc = subprocess.Popen(
        [sys.executable, str(ROOT / "script" / "fake-openai.py"),
         "--port", str(STUB_PORT), "--searxng-port", str(SEARX_PORT),
         "--log", str(SCRATCH / "stub.log")],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    import urllib.request
    for _ in range(80):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{STUB_PORT}/v1/models", timeout=1)
            return proc
        except Exception:
            time.sleep(0.1)
    proc.terminate()
    sys.exit(f"stub provider did not start on {STUB_PORT}")


def main():
    global ONLY
    parser = argparse.ArgumentParser()
    parser.add_argument("--area", default=None,
                        help="only one tool's cases (read, write, edit, grep, glob, "
                             "bash, await, todo, lsp, web_search, registry)")
    parser.add_argument("--list", action="store_true", help="list cases and exit")
    parser.add_argument("--keep", action="store_true",
                        help="keep the stub and the workspace for inspection")
    args = parser.parse_args()
    ONLY = args.area

    if args.list:
        for case in CASES:
            if area(case.get("area", "")):
                print(f"{case['area']:12} {case['name']}")
        return
    if not BINARY.exists():
        sys.exit(f"missing {BINARY}; run: cargo build -p rustedharness-cli")
    SCRATCH.mkdir(parents=True, exist_ok=True)
    write_config()
    stub = start_stub()
    started = time.time()
    try:
        for case in CASES:
            if not area(case.get("area", "")):
                continue
            try:
                check_case(case)
            except subprocess.TimeoutExpired:
                check(case["area"], case["name"], False, "turn timed out")
            except Exception as exc:
                check(case["area"], case["name"], False,
                      f"{type(exc).__name__}: {exc}")
    finally:
        if not args.keep:
            stub.terminate()

    passed = [r for r in RESULTS if r["ok"]]
    failed = [r for r in RESULTS if not r["ok"]]
    for result in RESULTS:
        mark = "ok  " if result["ok"] else "FAIL"
        print(f"{mark} {result['group']} · {result['name']}"
              + ("" if result["ok"] else f"   <{result['detail']}>"))
    print(f"\n{len(passed)}/{len(RESULTS)} 通过 · {time.time() - started:.1f}s")
    if failed:
        sys.exit(1)


if __name__ == "__main__":
    main()
