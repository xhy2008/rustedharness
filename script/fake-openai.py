#!/usr/bin/env python3
"""An OpenAI-compatible stub so the web UI can be tested without a provider.

The scenario is chosen by the first word of the last user message, so a test can
drive any path by typing it into the composer:

    hello   -> a short answer (the plain path)
    reason  -> reasoning_content chunks, then content
    tool    -> a bash tool call, then an answer once the tool result is back
    todo    -> a todo tool call, then an answer
    edit    -> an edit tool call (multi-file shape), then an answer
    slow    -> one chunk every 400 ms, for the stop button and streaming UI
    long    -> ~40 KB of text in 200 chunks, for the virtualized feed
    usage   -> a final chunk carrying token usage
    rate    -> HTTP 429 on the first request of a turn, then a normal answer
    boom    -> HTTP 500
    silent  -> headers, then nothing (idle budget / "still waiting" probes)
    call X  -> run the tool call X and answer after the result, where X is a JSON
               object {"name": ..., "arguments": {...}} or an array of them, e.g.
               `call {"name":"grep","arguments":{"pattern":"fn main"}}`
               This makes every tool and every argument reachable without a model.

A second server (`--searxng-port`) answers like SearXNG at `/search?q=...`, so
`web_search` can be tested without the network: `empty` returns no results,
`boom` returns 500, anything else returns three fixed hits.

Every request body is appended to --log so a test can assert what the UI sent.

Usage:
    python3 script/fake-openai.py --port 7398 --log /tmp/fake-openai.log
"""

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CHUNK = "data: {}\n\n"
DONE = "data: [DONE]\n\n"
LOCK = threading.Lock()
STATE = {"rate_seen": 0, "log": None}


def parts(text, size=4):
    """Split text into pieces that look like a token stream."""
    if " " not in text:
        return [text[i:i + size] for i in range(0, len(text), size)]
    words = text.split(" ")
    return [word if index == 0 else " " + word for index, word in enumerate(words)]


def last_pid(messages):
    """The pid a previous tool message reported, for `"$pid"` substitution."""
    import re
    for message in reversed(messages):
        if message.get("role") != "tool":
            continue
        match = re.search(r"pid:\s*(\d+)", str(message.get("content") or ""))
        if match:
            return int(match.group(1))
    return None


def substitute(value, pid):
    if isinstance(value, str):
        return pid if value == "$pid" and pid else value
    if isinstance(value, list):
        return [substitute(item, pid) for item in value]
    if isinstance(value, dict):
        return {key: substitute(item, pid) for key, item in value.items()}
    return value


def parse_specs(text):
    """`call {...}` / `call [...]` -> the (name, arguments) pairs requested."""
    body = text.strip()[4:].strip()
    try:
        value = json.loads(body)
    except ValueError:
        return []
    calls = value if isinstance(value, list) else [value]
    return [(call["name"], call.get("arguments") or {})
            for call in calls
            if isinstance(call, dict) and isinstance(call.get("name"), str)]


def parse_calls(text, messages):
    pid = last_pid(messages)
    return [{"index": index, "id": "call_stub_%d" % (index + 1),
             "function": {"name": name, "arguments": json.dumps(substitute(args, pid))}}
            for index, (name, args) in enumerate(parse_specs(text))]


def last_user_text(messages):
    for message in reversed(messages):
        if message.get("role") == "user":
            content = message.get("content")
            if isinstance(content, list):
                content = " ".join(p.get("text", "") for p in content
                                   if isinstance(p, dict))
            return str(content or "")
    return ""


def next_call(text, messages):
    """The first requested call whose result is not in the conversation yet."""
    answered = {m.get("tool_call_id") for m in messages if m.get("role") == "tool"}
    pid = last_pid(messages)
    for index, (name, args) in enumerate(parse_specs(text)):
        call_id = "call_stub_%d" % (index + 1)
        if call_id in answered:
            continue
        return {"index": 0, "id": call_id,
                "function": {"name": name,
                             "arguments": json.dumps(substitute(args, pid))}}
    return None


def scenarios(word, messages):
    """Yield (delay_seconds, delta, finish_reason, usage) tuples."""
    has_tool_result = any(m.get("role") == "tool" for m in messages)
    if word == "reason":
        for piece in parts("先想清楚，再说结论。"):
            yield 0.02, {"reasoning_content": piece}, None, None
        for piece in parts("这是带思考的回答。"):
            yield 0.02, {"content": piece}, None, None
        yield 0, {}, "stop", None
        return
    if word == "call":
        # One call per round, like a model that has to read each result before
        # it can pick the next call: that is what makes `"$pid"` resolvable.
        pending = next_call(last_user_text(messages), messages)
        if pending:
            yield 0.02, {"tool_calls": [pending]}, "tool_calls", None
            return
        for piece in parts("工具已执行。"):
            yield 0.02, {"content": piece}, None, None
        yield 0, {}, "stop", None
        return
    if word == "tool" or word == "todo" or word == "edit":
        if not has_tool_result:
            if word == "tool":
                name, args = "bash", {"command": "printf 'tool says hi\\n'"}
            elif word == "todo":
                name, args = "todo", {"items": [{"content": "第一步", "status": "completed"},
                                                {"content": "第二步", "status": "in_progress"},
                                                {"content": "第三步", "status": "pending"}]}
            else:
                name, args = "edit", {"files": [{"path": "made-by-tool.txt",
                                                 "anchor": "", "text": "written by the stub\n"}]}
            call = {"index": 0, "id": "call_stub_1",
                    "function": {"name": name, "arguments": json.dumps(args)}}
            yield 0.02, {"tool_calls": [call]}, "tool_calls", None
            return
        for piece in parts("工具已经执行完毕。"):
            yield 0.02, {"content": piece}, None, None
        yield 0, {}, "stop", None
        return
    if word == "slow":
        for piece in parts("慢速流式回答，用来测试停止按钮。" * 6):
            yield 0.4, {"content": piece}, None, None
        yield 0, {}, "stop", None
        return
    if word == "long":
        line = "这是一行用来把消息撑长的文本，重复很多次也不会有任何意义。\n"
        for _ in range(200):
            yield 0.005, {"content": line}, None, None
        yield 0, {}, "stop", None
        return
    if word == "usage":
        for piece in parts("带用量的回答。"):
            yield 0.02, {"content": piece}, None, None
        yield 0, {}, "stop", {"prompt_tokens": 4321, "completion_tokens": 12,
                              "total_tokens": 4333,
                              "prompt_tokens_details": {"cached_tokens": 4096}}
        return
    if word == "silent":
        time.sleep(30)
        yield 0, {}, "stop", None
        return
    for piece in parts("这是来自假 provider 的回答：" + word):
        yield 0.02, {"content": piece}, None, None
    yield 0, {}, "stop", None


SEARXNG_HITS = [
    {"title": "Rust 官方文档", "url": "https://doc.rust-lang.org/book/",
     "content": "The Rust Programming Language."},
    {"title": "rust-analyzer", "url": "https://rust-analyzer.github.io/",
     "content": "A language server for Rust."},
    {"title": "例子站点", "url": "https://example.com/rust",
     "content": "an example host that domain filters should drop"},
]


class SearxngHandler(BaseHTTPRequestHandler):
    """Enough of SearXNG's JSON API for `web_search` to work offline."""

    protocol_version = "HTTP/1.0"

    def log_message(self, *args):
        pass

    def do_GET(self):
        query = ""
        if "?" in self.path:
            from urllib.parse import parse_qs, urlparse
            query = (parse_qs(urlparse(self.path).query).get("q") or [""])[0]
        if "boom" in query:
            body = b'{"error": "stub failure"}'
            self.send_response(500)
        elif "empty" in query or "无结果" in query:
            body = b'{"results": []}'
            self.send_response(200)
        else:
            body = json.dumps({"results": SEARXNG_HITS}, ensure_ascii=False).encode()
            self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, *args):
        with LOCK, open(STATE["log"] or "/dev/null", "a") as fh:
            fh.write("http %s\n" % (args[0] % args[1:]))

    def record(self, body):
        if not STATE["log"]:
            return
        with LOCK, open(STATE["log"], "a") as fh:
            fh.write(json.dumps({"path": self.path, "body": body}) + "\n")

    def json_response(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.endswith("/models"):
            self.json_response(200, {"data": [
                {"id": "discovered-a", "object": "model"},
                {"id": "discovered-b", "object": "model"},
            ]})
            return
        self.json_response(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            body = {}
        self.record(body)
        messages = body.get("messages") or []
        last_user = last_user_text(messages)
        word = (last_user.strip().split() or ["hello"])[0].lower()

        if word == "boom":
            self.json_response(500, {"error": {"message": "stub failure", "type": "server_error"}})
            return
        if word == "rate":
            with LOCK:
                STATE["rate_seen"] += 1
                first = STATE["rate_seen"] == 1
            if first:
                self.json_response(429, {"error": {"message": "Rate limit reached for stub",
                                                   "type": "rate_limit_error"}})
                return

        if not body.get("stream"):
            # Compaction summaries and other one-shot calls take this path.
            text = "".join(delta.get("content", "")
                           for _delay, delta, _finish, _usage in scenarios(word, messages))
            self.json_response(200, {"id": "stub", "choices": [
                {"index": 0, "message": {"role": "assistant", "content": text},
                 "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})
            return

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        try:
            for delay, delta, finish, usage in scenarios(word, messages):
                if delay:
                    time.sleep(delay)
                choice = {"index": 0, "delta": delta, "finish_reason": finish}
                payload = {"id": "stub", "object": "chat.completion.chunk", "choices": [choice]}
                if usage:
                    payload["usage"] = usage
                self.wfile.write(CHUNK.format(json.dumps(payload)).encode())
                self.wfile.flush()
                if usage:
                    # usage normally arrives on its own, empty-choice chunk
                    payload = {"id": "stub", "choices": [], "usage": usage}
                    self.wfile.write(CHUNK.format(json.dumps(payload)).encode())
                    self.wfile.flush()
                if finish:
                    break
            self.wfile.write(DONE.encode())
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=7398)
    parser.add_argument("--log", default="")
    parser.add_argument("--searxng-port", type=int, default=0,
                        help="also serve a SearXNG-like /search on this port")
    args = parser.parse_args()
    STATE["log"] = args.log or None
    if STATE["log"]:
        open(STATE["log"], "w").close()
    if args.searxng_port:
        searxng = ThreadingHTTPServer(("127.0.0.1", args.searxng_port), SearxngHandler)
        threading.Thread(target=searxng.serve_forever, daemon=True).start()
        print("fake searxng on http://127.0.0.1:%d/search" % args.searxng_port,
              flush=True)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print("fake openai on http://127.0.0.1:%d/v1" % args.port, flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
