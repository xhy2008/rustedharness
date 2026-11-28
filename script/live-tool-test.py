#!/usr/bin/env python3
"""Ask a real model to use the tools, and check that it did.

`script/tool-test.py` proves the tools work: it hands the agent the exact call
it wants. This script proves the *other* half - that a model reading the tool
schemas actually calls them, with usable arguments, and that the harness feeds
the result back so the model can answer.

Each case is one real turn against an OpenAI-compatible endpoint:

    python3 script/live-tool-test.py --base-url http://127.0.0.1:8080/v1 \
        --model minicpm5-2b

Nothing is stubbed, so a weak model will genuinely fail some cases; the point is
to see which, and to keep the plumbing honest (schema shape, tool_choice, the
tool message round trip, context budget).

Requirements: a built binary (`cargo build -p rustedharness-cli`), python, and
a running server. `--list` prints the cases; `--keep` leaves the workspace and
the raw event log in /tmp/rh-live-test for inspection.
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
SCRATCH = Path("/tmp/rh-live-test")
WS = SCRATCH / "ws"
CFG = SCRATCH / "cfg"

BASE_URL = os.environ.get("RH_LIVE_BASE", "http://127.0.0.1:8080/v1")
MODEL = os.environ.get("RH_LIVE_MODEL", "minicpm5-2b")
PROVIDER = "live"
CHECK_TIMEOUT = 300

RESULTS = []
ONLY = None
VERBOSE = False

# A workspace small enough to fit in a 2B model's context next to the ~4k-token
# system prompt, but with every shape a case needs.
BASE = {
    "src/main.rs": """use std::collections::BTreeMap;

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
    "src/util.rs": """pub fn double(value: u32) -> u32 {
    value * 2
}
""",
    "notes.txt": "alpha line one\nbeta line two\ngamma line three\n",
    "data.json": '{"name":"fixture","ok":true}\n',
}

# name, prompt, expected tool names (any of), what else must hold.
CASES = [
    dict(area="glob", name="按扩展名找文件",
         prompt="用 glob 工具列出这个工作区里所有的 .rs 文件。",
         tools=["glob"], answer="src/main.rs"),
    dict(area="read", name="读一个文件并说明内容",
         prompt="用 read 工具读 notes.txt，然后告诉我第一行是什么。",
         tools=["read"], answer="alpha"),
    dict(area="grep", name="在文件里搜一个词",
         prompt="用 grep 工具在 notes.txt 里搜索 gamma，把命中的行告诉我。",
         tools=["grep"], answer="gamma line three"),
    dict(area="write", name="新建文件",
         prompt="用 write 工具创建 hello.txt，内容是 hello live model。",
         # reading it back to confirm is normal, not a failure
         tools=["write", "read"], files={"hello.txt": "hello live model"}),
    # Known-hard for a 2B model: it has to copy `oldText` and invent `newText`.
    # Reported as a warning, not a failure - the plumbing is covered by
    # script/tool-test.py, which hands the tool the exact arguments.
    dict(area="edit", name="改一处文本",
         prompt="用 edit 工具把 notes.txt 里的 beta 改成 BETA，用 replace_text。",
         tools=["edit"], soft=True,
         files={"notes.txt": "alpha line one\nBETA line two\ngamma line three\n"}),
    dict(area="bash", name="跑一条 shell 命令",
         prompt="用 shell 工具执行 `echo live-ok`，把输出告诉我。",
         tools=["bash", "shell", "run", "powershell"], answer="live-ok"),
    dict(area="bash", name="跑命令并读结果",
         prompt="用 shell 工具统计 src/main.rs 有多少行（命令自选），把数字告诉我。",
         tools=["bash", "shell", "run", "powershell"], answer="13"),
    dict(area="todo", name="写待办",
         prompt="用 todo 工具把『跑通工具测试』加入待办，状态 pending。",
         tools=["todo"], answer="跑通工具测试"),
    dict(area="lsp", name="查符号",
         prompt="用 lsp 工具以 document_symbols 列出 src/main.rs 的符号。",
         tools=["lsp"], answer="helper"),
    dict(area="read", name="组合：先读再算",
         prompt="先读 data.json，再读出其中的 name 字段值，用一句话回答。",
         tools=["read"], answer="fixture"),
    dict(area="chat", name="普通问话不调工具",
         prompt="用一句话解释什么是二分查找。",
         tools=[], answer="二分"),
    dict(area="read", name="工具报错后能解释",
         prompt="用 read 工具读 does-not-exist.md，然后告诉我为什么失败。",
         tools=["read"], error=True, answer=""),
]


def check(group, name, ok, detail="", soft=False):
    RESULTS.append({"group": group, "name": name, "ok": bool(ok), "soft": soft,
                    "detail": str(detail)[:500]})


def area(name):
    return ONLY is None or ONLY == name


def reset_workspace():
    if WS.exists():
        shutil.rmtree(WS)
    for rel, body in BASE.items():
        path = WS / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)
    (WS / "build").mkdir(exist_ok=True)
    (WS / ".gitignore").write_text("build/\n")
    subprocess.run(["git", "init", "-q"], cwd=WS, check=False)
    subprocess.run(["git", "add", "-A"], cwd=WS, check=False)
    subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t",
                    "commit", "-qm", "fixture"], cwd=WS, check=False)


def write_config(base_url, model):
    CFG.mkdir(parents=True, exist_ok=True)
    (CFG / "models.json").write_text(json.dumps({"providers": {PROVIDER: {
        "name": "Live model",
        "baseUrl": base_url,
        "api": "openai-completions",
        "apiKey": os.environ.get("RH_LIVE_KEY", "none"),
        "models": [{"id": model, "name": model,
                    "contextWindow": int(os.environ.get("RH_LIVE_CTX", "8192")),
                    "maxTokens": 1024}],
    }}}, indent=2))
    (CFG / "config.json").write_text(json.dumps({
        "model": f"{PROVIDER}/{model}",
        # A small model can loop on a tool it does not understand; the point of
        # this suite is the call itself, so stop the turn early instead of
        # filling the context and waiting minutes for the 400.
        "agent": {"rateLimitRetries": 0, "maxTurns": 8},
        "shell": {"maxOutputBytes": 32 * 1024},
    }, indent=2))


def run_turn(prompt, timeout):
    cmd = [str(BINARY), "--config", str(CFG), "--cwd", str(WS),
           "run", "--json", "--yes", prompt]
    env = {**os.environ, "RUSTEDHARNESS_DIR": str(SCRATCH)}
    started = time.time()
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True,
                              timeout=timeout, env=env)
    except subprocess.TimeoutExpired:
        return [], f"超时（{timeout}s）", 1, time.time() - started
    return proc.stdout.splitlines(), proc.stderr.strip(), proc.returncode, \
        time.time() - started


def parse(events):
    calls, results, usage, answer = [], [], [], ""
    for line in events:
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = event.get("type")
        if kind == "tool_call":
            calls.append(event)
        elif kind == "tool_result":
            results.append(event)
        elif kind == "token_usage":
            usage.append(event)
        elif kind == "assistant_message":
            answer = event.get("content") or answer
    return calls, results, usage, answer


def tool_matches(expected, actual):
    """`bash` is named after the platform shell, so compare loosely."""
    if actual in expected:
        return True
    return any(actual.startswith(name) for name in expected)


def run_case(case, base_url, model):
    reset_workspace()
    events, stderr, code, elapsed = run_turn(case["prompt"], CHECK_TIMEOUT)
    calls, results, usage, answer = parse(events)
    (SCRATCH / "last-events.jsonl").write_text("\n".join(events))
    (SCRATCH / "last-stderr.txt").write_text(stderr)

    detail = f"{elapsed:.1f}s"
    ok = code == 0
    if not ok:
        detail += f" exit={code} {stderr[:200]}"

    # Extra calls are allowed: a model that reads a file back to confirm its
    # own edit is doing the right thing. What must hold is that the tool the
    # case is about was called, and that nothing it called failed.
    expected = case["tools"]
    names = [call.get("name", "") for call in calls]
    if expected:
        if not names:
            ok, detail = False, detail + " 没有调用任何工具"
        elif not any(tool_matches(expected, name) for name in names):
            ok = False
            detail += f" 期望 {expected}，实际 {names}"
        elif not all(tool_matches(expected, name) for name in names):
            detail += f"（另有 {[n for n in names if not tool_matches(expected, n)]}）"
    elif names:
        ok, detail = False, detail + f" 不该调用工具却调用了 {names}"
    detail += f" 调用={names or '-'}"

    if expected and results:
        # Only the tool the case is about has to succeed. A small model often
        # wanders off afterwards and tries something that fails; that is noise
        # from the model, not from this tool.
        own = [r for r in results if tool_matches(expected, r.get("name", ""))]
        failed = [r.get("name") for r in own if not r.get("ok")]
        others = [r.get("name") for r in results
                  if not tool_matches(expected, r.get("name", "")) and not r.get("ok")]
        if failed and not case.get("error"):
            ok = False
            detail += f" 工具失败 {failed}"
        elif failed and case.get("error"):
            detail += " 失败符合预期"
        elif not failed and case.get("error"):
            ok, detail = False, detail + " 应该报错却成功了"
        if others:
            detail += f"（其它工具失败 {others}）"
    elif expected and names and not results:
        ok, detail = False, detail + " 没有 tool_result"

    for rel, body in case.get("files", {}).items():
        path = WS / rel
        got = path.read_text() if path.exists() else None
        if got != body:
            ok = False
            detail += f" {rel}={got!r}"

    if case.get("answer") and case["answer"] not in answer:
        ok = False
        detail += f" 回答里没有 {case['answer']!r}（{answer[:80]!r}）"

    if usage:
        detail += (f" tokens={usage[-1].get('input_tokens')}"
                   f"+{usage[-1].get('output_tokens')}"
                   f" 缓存={usage[-1].get('cache_hit_pct')}%")
    check(case["area"], case["name"], ok, detail, case.get("soft", False))


def main():
    global ONLY, VERBOSE, BASE_URL, MODEL
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base-url", default=BASE_URL)
    parser.add_argument("--model", default=MODEL)
    parser.add_argument("--area", help="只跑一个区域")
    parser.add_argument("--list", action="store_true", help="列出用例")
    parser.add_argument("--keep", action="store_true", help="保留工作区")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    BASE_URL, MODEL, ONLY, VERBOSE = args.base_url, args.model, args.area, \
        args.verbose

    if args.list:
        for case in CASES:
            print(f"{case['area']:<6} {case['name']:<24} {case['prompt']}")
        return 0

    if not BINARY.exists():
        sys.exit(f"缺少 {BINARY}，先 cargo build -p rustedharness-cli")
    if not health(BASE_URL):
        sys.exit(f"{BASE_URL} 没有应答，先启动 llama-server")

    if SCRATCH.exists() and not args.keep:
        shutil.rmtree(SCRATCH)
    SCRATCH.mkdir(parents=True, exist_ok=True)
    write_config(BASE_URL, MODEL)
    reset_workspace()

    print(f"模型 {MODEL} @ {BASE_URL}")
    started = time.time()
    for case in CASES:
        if not area(case["area"]):
            continue
        run_case(case, BASE_URL, MODEL)
    elapsed = time.time() - started

    for result in RESULTS:
        mark = "ok  " if result["ok"] else ("warn" if result["soft"] else "FAIL")
        print(f"{mark} {result['group']:<6} {result['name']:<24} {result['detail']}")
    passed = sum(1 for r in RESULTS if r["ok"])
    failed = [r for r in RESULTS if not r["ok"] and not r["soft"]]
    warned = [r for r in RESULTS if not r["ok"] and r["soft"]]
    print(f"\n{passed}/{len(RESULTS)} 通过 · {elapsed:.1f}s")
    if failed or warned:
        print("未通过：")
        for result in failed + warned:
            mark = "FAIL" if not result["soft"] else "warn"
            print(f"  - [{mark}] {result['group']} · {result['name']}: {result['detail']}")
    return 1 if failed else 0


def health(base_url):
    import urllib.request
    root = base_url.rstrip("/")
    if root.endswith("/v1"):
        root = root[:-3]
    try:
        with urllib.request.urlopen(root + "/health", timeout=5) as response:
            return response.status == 200
    except Exception:
        return False


if __name__ == "__main__":
    sys.exit(main())
