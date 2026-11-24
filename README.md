# RustedHarness

An autonomous coding agent written in Rust, with anchored file editing, a
terminal UI, a web UI, and an optional Tauri desktop shell.

The implementation follows the RustedHarness PRD v1.7 (2026-10-23). It speaks
the OpenAI Chat Completions API (including compatible local servers), runs an
agent loop with tools, and keeps every frontend on the same typed event and
command protocol.

## Workspace

| Crate | Purpose |
| --- | --- |
| `rustedharness-core` | Provider, agent loop, tools, Hashline editing, LSP/MCP clients, sessions, compaction, code index |
| `rustedharness-cli` | `rustedharness` binary: `run`, `serve`, `tui`, `lsp`, `sessions`, `config` |
| `rustedharness-tui` | Full-screen ratatui interface |
| `rustedharness-web` | Axum REST + WebSocket server and embedded UI |
| `rustedharness-gui` | Optional Tauri 2 shell; reuses the web UI over a loopback socket |

All frontends consume `HarnessEvent` and send `HarnessCommand`. The GUI crate is
excluded from `default-members` so a normal build needs no webview libraries.

## Requirements

- Rust 1.85 or newer (edition 2021).
- A provider and model in `models.json`; API keys may be literals or use
  `$ENV` interpolation. There is no built-in provider or default model.
- Optional: language servers for semantic code intelligence. The agent falls
  back to a built-in name-based index when none is available.

## Quick start

```sh
cargo build --release
export OPENAI_API_KEY=sk-...

# Create ~/.rustedharness/config.json and models.json
./target/release/rustedharness config init
# Then add a provider/model to models.json and set `model` in config.json.
# Without a model, the TUI shows `None` and `run` exits with a prompt.

# One-shot, headless
./target/release/rustedharness run "Add a test for the parser and run it"

# Streaming REPL (no TUI)
./target/release/rustedharness run

# Full-screen terminal UI
./target/release/rustedharness tui

# Web UI: http://127.0.0.1:8787
./target/release/rustedharness serve --token "$RUSTEDHARNESS_TOKEN"

# Optional desktop shell (needs platform webview libraries)
cargo run -p rustedharness-gui --features desktop
```

`--cwd DIR` anchors the workspace (R4) and `--model MODEL` overrides the agent
model for any subcommand. `MODEL` may be `provider/model` or a bare model id
that is unique across the configured providers. `run --json` emits one JSON
event per line, and `run --resume <id>` continues a saved session. `run --yes`
approves language server downloads without prompting.

## Configuration

Configuration lives in `~/.rustedharness` by default. Override the directory
with `RUSTEDHARNESS_DIR`, which keeps config, sessions and downloaded language
servers together:

| Path | Purpose |
| --- | --- |
| `config.json` | Model choice (optional) and harness settings |
| `models.json` | Providers, endpoints and their models (Pi-style) |
| `mcp.json` | Optional MCP servers shared by every project |
| `sessions/` | Append-only JSONL sessions |
| `lsp/` | Cached language-server installations |

The L0 tree-sitter index is the one piece of state that belongs to a workspace
rather than to the user: it lives in `<workspace>/.rustedharness/index.sqlite`
and the directory writes its own `.gitignore` (`*`), so it never shows up in
`git status`. A read-only workspace falls back to an in-memory index.

`rustedharness config init` creates `config.json` and `models.json` without
overwriting existing files. There is no built-in provider or model: until one
is configured, the TUI shows `None`, `run` exits with a configuration prompt,
and `config models` prints `default: None`. `config path` prints every path,
`config show` prints the effective configuration with credentials redacted,
`config models` lists the resolved model catalog, and
`config edit [config|models]` opens the file in `$VISUAL`/`$EDITOR` (or the
platform default).

`--config PATH` accepts a `config.json`, a `models.json`, or a directory
containing both.

### models.json

Declare as many providers as you like. A model is selected as
`provider/model`; bare ids are accepted when they are unique. Model ids that
contain slashes (for example OpenRouter ids) can be used as-is when unique, or
prefixed with their provider. The example below is illustrative; this harness
does not ship any of these providers or models by default.

The TUI can do this for you: `/model` ends with **Add model...**, a
full-screen form for a base URL and API key. Enter fetches `GET {base}/models`,
lists what the endpoint serves (with context window and output cap when the
provider reports them), and writes the provider and the chosen model into
`models.json` — creating the provider entry from the URL host, e.g.
`https://api.deepseek.com/v1` becomes `deepseek`. Endpoints without a model
list fall back to typing the id. The new model is selected immediately: the
agent reloads `models.json` and rebuilds its provider router, so no restart is
needed. The key is stored in plain text in `models.json`.

```json
{
  "providers": {
    "openai": {
      "name": "OpenAI",
      "baseUrl": "https://api.openai.com/v1",
      "api": "openai-completions",
      "apiKey": "$OPENAI_API_KEY",
      "models": [
        { "id": "gpt-4o", "contextWindow": 128000 },
        { "id": "gpt-4o-mini", "contextWindow": 128000 }
      ]
    },
    "gpt-responses": {
      "name": "OpenAI Responses",
      "baseUrl": "https://api.openai.com/v1",
      "api": "openai-responses",
      "apiKey": "$OPENAI_API_KEY",
      "models": [
        { "id": "gpt-5", "contextWindow": 400000, "reasoning": true }
      ]
    },
    "deepseek": {
      "name": "DeepSeek",
      "baseUrl": "https://api.deepseek.com/v1",
      "apiKey": "$DEEPSEEK_API_KEY",
      "headers": { "X-Title": "RustedHarness" },
      "compat": {
        "supportsDeveloperRole": false,
        "maxTokensField": "max_tokens"
      },
      "models": [
        { "id": "deepseek-chat", "contextWindow": 128000 },
        { "id": "deepseek-reasoner", "contextWindow": 128000, "reasoning": true }
      ]
    },
    "ollama": {
      "baseUrl": "http://127.0.0.1:11434/v1",
      "apiKey": "ollama",
      "models": [
        { "id": "qwen2.5-coder:7b", "contextWindow": 32768 }
      ]
    }
  }
}
```

`apiKey` and header values accept a literal, `$NAME`/`${NAME}` environment
interpolation (`${NAME:-default}` for a fallback, unset variables expand to
nothing), or `!command` output. `$$` and `$!` produce literal `$` and `!`.
Local endpoints may use a dummy key or omit it.

Provider fields: `name`, `baseUrl`, `api` (`openai-completions` or
`openai-responses`), `apiKey`, `headers`, `compat`, and `models`. Pi-style
JSONC comments and trailing commas are accepted. `$schema` is ignored.
Model fields: `id`, `name`, `contextWindow` (optional), `maxTokens`
(optional output cap), and `reasoning`. A model entry with an existing id
replaces it; new entries are appended. A provider with an empty `baseUrl` is
treated as disabled. When a model omits `contextWindow`, the context limit is
shown as unavailable instead of being guessed.

`api` selects the wire protocol:

- `openai-completions` (default) — `POST {baseUrl}/chat/completions`.
- `openai-responses` — `POST {baseUrl}/responses`. The Responses backend maps
  the agent transcript to `input` items and `instructions`, flattens function
  tools, and uses `async-openai` in BYOT mode to stream
  `response.output_text.*`, `response.reasoning_*` and
  `response.function_call_arguments.*` events, reading `input_tokens` /
  `output_tokens` from `usage`. Reasoning items are not replayed between turns,
  so stateless use does not require server-side response storage.

`compat` uses Pi's field names and auto-detection: `supportsStore`,
`supportsDeveloperRole`, `supportsUsageInStreaming`, `supportsFinishReason`,
`maxTokensField`, `requiresToolResultName`,
`requiresAssistantAfterToolResult`, `requiresNonNullAssistantContent`,
`requiresToolsWithToolHistory`, and
`requiresReasoningContentOnAssistantMessages`. The defaults follow Pi:
`maxTokensField` is detected from the endpoint, streaming usage defaults on,
the developer role is used for OpenAI Chat Completions reasoning models (and
for Responses), and DeepSeek gets empty `reasoning_content` on replayed
assistant messages. `reasoning_content` is replayed automatically when present.
`tool_choice` and `parallel_tool_calls` are not emitted from `models.json`;
Pi only sends them from request options, so the default request omits them.
There is no automatic field-degradation retry: if a gateway rejects a field,
the provider returns the error and the request is not retried.

Higher-level Pi compat fields that depend on thinking levels, strict/grammar
tools, prompt-cache markers, routing, or Anthropic-specific message blocks are
not implemented yet: `thinkingFormat`, `chatTemplate*`, `openRouterRouting`,
`vercelGatewayRouting`, `zaiToolStream`, `cacheControlFormat`,
`promptCacheKeyMode`, session-affinity headers, `thinkingTokenBudget*`,
`supportsStrictMode`, `supportsOpenAIGrammarTools`, and the mid-conversation
tool/system flags. They are ignored rather than emitted with guessed values.

Pi's advanced model fields (`input`, `inputLimits`, `cost`, `promptCache`,
`samplingParams`, model-level `api`/`baseUrl`/`headers`/`compat`) and provider
`oauth`/`authHeader`/`modelOverrides` are currently ignored; basic provider
and model selection loads unchanged.

### config.json

```json
{
  "agent": {
    "maxTurns": 100,
    "compactThreshold": 0.8,
    "keepRecentTurns": 8,
    "compactStripContext": true,
    "sendRecentTurns": 16,
    "stripContent": "toolOutputAndThinking",
    "compactThinking": "off",
    "temperature": 0.2,
    "chaseLimit": 3,
    "maxOutputTokens": 8192,
    "rateLimitRetries": 5,
    "httpIdleTimeoutMs": 0,
    "showCacheMissNotices": true
  },
  "edit": {
    "anchorWidth": 2,
    "enableReplaceText": true,
    "diagnosticsTimeoutMs": 2000
  },
  "shell": {
    "prefer": "default",
    "timeoutMs": 30000,
    "activityTimeoutMs": 300000,
    "maxOutputBytes": 1048576,
    "ptyRows": 50,
    "ptyCols": 200
  },
  "lsp": {
    "enabled": true,
    "autoDownload": true,
    "allowDownloadsNoninteractive": false,
    "idleTimeoutSecs": 600,
    "maxRestarts": 3,
    "requestTimeoutMs": 5000,
    "downloads": {
      "rust-analyzer": { "version": "2026-11-02" }
    }
  },
  "mcp": {
    "enabled": true,
    "deny": [],
    "callTimeoutMs": 30000
  },
  "web": {
    "host": "127.0.0.1",
    "port": 8787
  }
}
```

`lsp.downloads.<server>` is optional and only overrides the default flow:
`version` pins a tag instead of taking the newest release, `url` replaces the
whole URL (a mirror), `binary` and `archive` describe an unusual layout.

`model` and `compactModel` may point at different providers; the backend routes
each request independently. The example intentionally omits both: omit `model`
to start without one, and the TUI shows `None` with a configuration prompt;
headless `run` exits with the same prompt. `compactModel` is optional too:
leaving it unset (or choosing `None` in `/settings`) summarises with the main
model. `contextWindow` is an optional fallback for models that do not declare
one. `agent.maxOutputTokens` is the fallback when a model has no `maxTokens`.
Environment variables `RUSTEDHARNESS_MODEL`, `RUSTEDHARNESS_COMPACT_MODEL`, and
`RUSTEDHARNESS_PROVIDER` override the file.

`config show` redacts inline keys and header values.

## Agent behavior

The loop streams assistant text and reasoning, executes tool calls, feeds
results back, and repeats until the model stops or a limit is reached.
Cancellation is cooperative. After edits, language-server diagnostics are
appended to the tool result.

- **Model routing**: `model` drives the loop; an optional `compactModel` is
  used only for summaries. Each request is routed to the provider that owns its
  model, so the two roles can live on different providers. When
  `compactModel` is unset (the default), compaction uses the main model and
  sends the live system prompt, tool catalog and old messages unchanged, so
  the provider can serve the whole prefix from prompt cache. A separate model
  starts cold. When `agent.compactStripContext` is on, `sendRecentTurns`
  controls how many recent user turns of the summarised transcript are sent in
  full; older turns lose the content selected by `stripContent` (`none`,
  `toolOutput`, or
  `toolOutputAndThinking`). `keepRecentTurns` separately controls how many
  turns stay verbatim after compaction. `compactThinking` asks for a reasoning
  effort on the summary request (`off`, `low`, `medium`, `high`, `xhigh` or
  `max`) and applies to both compaction paths; `off` sends no effort field, and
  the value is only sent when the compaction model declares `reasoning` and its
  provider accepts `reasoning_effort`. `/settings` opens a compaction submenu
  for all of these. Auto-compaction is always on. Changes apply immediately and
  are written to `config.json`.
- **Slow providers**: provider requests have no timeout. The turn is still
  cancellable at any time, including while waiting for the first byte, so a
  free API that trickles can be interrupted with Esc/Ctrl+C. Set
  `agent.httpIdleTimeoutMs` (milliseconds) to give up instead when nothing
  arrives for that long; `0` keeps waiting forever.
- **Cache notices**: when a turn re-bills prompt tokens the previous turn had
  already cached, a notice reports the missed tokens plus the gap since the
  previous request and whether the model changed. Providers neither explain the
  miss nor publish a cache lifetime, so no lifetime is assumed or configured.
  `agent.showCacheMissNotices` (default `true`) turns the notices off.
- **429 retries**: rate-limited responses (HTTP 429 / `resource_exhausted`)
  retry for up to `agent.rateLimitRetries` attempts, three seconds apart. The
  settings panel offers 5, 15, 30, 50 and 100; the gap is fixed, so a
  rate-limited provider is never hammered faster than one request per three
  seconds. Quota/credit exhaustion is terminal and never retried.
- **Compaction**: when the provider-reported context size reaches 80% of the
  active model's `contextWindow`, the transcript is replaced by a five-section
  summary (goal, done, constraints, pending, pitfalls). The most recent turns
  are kept verbatim.
- **Token accounting**: counts come exclusively from the `usage` object returned
  by the provider; nothing is estimated or tokenized locally. If a gateway omits
  `usage`, the harness warns once and reports no invented numbers.
- **Todos**: `todo` maintains a plan. If work remains and the model stops, the
  harness injects a bounded chase message (at most `chaseLimit` times).
- **Sessions**: append-only JSONL under `~/.rustedharness/sessions` in pi's
  v3 format, with torn-tail recovery, pruning, and unique-prefix lookup. Saved
  sessions can be resumed from the CLI, TUI, or web UI; compaction records are
  replayed so the resumed transcript matches the compacted view.
  The layout is pi's, so `session.dir` can point at `~/.pi/agent/sessions` and
  both tools open the same files: `<dir>/--<encoded cwd>--/<timestamp>_<id>.jsonl`.
  What pi records as noise is deliberately left out - failed API calls (stored
  by pi as empty error messages), per-switch `model_change` history (we keep one
  model snapshot), and frontend commands such as `/model x` entered in the
  composer.
- **Workspace anchoring (R1–R4)**: relative paths resolve against the session
  workspace; absolute paths outside it are rejected. Symlinks are resolved
  before the check. Every tool is auto-allowed except a would-be R4 escape.

### Tools

| Tool | Description |
| --- | --- |
| `read` | Read text or binary ranges from files, or fetch an `http(s)` URL as markdown. |
| `write` | Create/overwrite a file, or append when it already exists. |
| `edit` | Hashline anchored edits, optionally several per call. |
| `glob` | Workspace file discovery (respects `.gitignore`). |
| `grep` | Regex content search with glob filters, backed by `grep-searcher`. |
| `bash` / `powershell` | Run commands in a pty; background mode returns a pid. |
| `await` | Wait for a background process and return output/exit status. |
| `todo` | Replace, merge, or inspect the task list. |
| `web_search` | Brave → Tavily → DuckDuckGo fallback chain. |
| `lsp` | Ten semantic operations backed by a real language server, with L0 fallback. |
| `mcp__server__tool` | Tools exposed by configured MCP servers. |

Remote `read` results are wrapped as untrusted external content so fetched pages
cannot impersonate instructions. HTML is parsed once with `html5ever`; the
boilerplate subtrees are pruned from that DOM and the same tree is handed to
`html2md`, so scripts, styles and `<head>` metadata never reach the Markdown and
every named entity is decoded. `web_search` uses `BRAVE_API_KEY` and
`TAVILY_API_KEY` when present and always falls back to keyless DuckDuckGo.

### Anchored editing (Hashline)

`read` returns lines tagged as `line#anchor|content`. Line numbers choose the
position; the anchor is an xxHash32 of that line's normalized content, rendered
in the alphabet `ZPMQVRWSNKTXJBYH`, and only confirms the line is still the one
the model saw. `edit` accepts a batch:

```json
{
  "path": "src/lib.rs",
  "edits": [
    { "op": "replace", "pos": "12#VQ", "lines": ["fn main() {}"] },
    { "op": "insert_after", "pos": "18#KZ", "lines": ["// new"] }
  ]
}
```

Anchor width defaults to 2 characters and can be raised to 4 with
`edit.anchorWidth`. The primary ops are `replace`, `insert_before`,
`insert_after`, and `delete`; `replace_text` with a unique `oldText`/`newText`
is the fallback for agents that cannot use line anchors.

If any anchor is stale, the whole batch is rejected with `E_STALE_ANCHOR`; no
partial write occurs. Editing a line changes only that line's anchor; inserts
and deletes shift later line numbers, so the edit result returns fresh anchors
for the shifted range. Identical lines share an anchor because the hash covers
only content, not neighbours. `write` into an existing file appends without
destroying content. The append result message is exactly `文件已存在，内容已追加`.

## Language servers

The `lsp` tool supports `definition`, `type_definition`, `implementation`,
`references`, `hover`, `document_symbols`, `workspace_symbols`,
`call_hierarchy`, `rename`, and `diagnostics`. Servers are spawned lazily,
restarted a bounded number of times, and shut down when idle.

Two extra ops let the agent manage the server itself, taking `server` (the id)
instead of `path`:

| Op | Effect |
| --- | --- |
| `install` | download the newest release; the consent card is shown and the call waits for the answer (120s), then fails with `E_LSP_CONSENT_DECLINED` if refused |
| `restart` | stop the server, start it again and wait until it is ready |

Both are refused when `lsp.enabled` is false, and `install` reports an unknown
id together with the ids the registry does have.

When a server is missing and `lsp.autoDownload` is enabled (the default), the
harness installs the newest release. The version is resolved from the
`github.com/<owner>/<repo>/releases/latest` redirect - a plain HTTPS request
that costs no API quota - and the artifact is fetched with a direct `GET` of
`.../releases/download/<tag>/<asset>`. Nothing is pinned in the registry, so a
release every week never leaves a stale URL behind.

Downloads are not checksum-verified. Trust comes from explicit consent, HTTPS
only, a host whitelist, a 256 MiB archive cap and extraction checks that reject
paths escaping the target directory.

Shortly after startup the harness asks each *installed* server's release page
for its newest tag and emits `update_available` when it differs from the cached
copy; nothing is downloaded automatically. `lsp update <server>` installs the
newest release and keeps a single version per server in the cache.

Transport rules are enforced for every download:

- HTTPS only, with redirects re-checked against a host whitelist;
- the archive is at most 256 MiB, enforced while it streams in;
- connecting times out after 15s and a stalled transfer after 60s;
- the body is streamed, so the status line shows `lsp clangd 42% 12.3/29.4 MB`
  and `lsp install` prints a line at least every few seconds while it runs;
- the user consents interactively, or `lsp.allowDownloadsNoninteractive` /
  `--yes` is set.

```sh
rustedharness lsp list                    # registry + installed versions
rustedharness lsp install rust-analyzer   # newest release, cached locally
rustedharness lsp update rust-analyzer    # re-install the newest release
```

The TUI exposes the same verbs under one command, so nothing LSP-related is a
top-level slash command:

| Command | Effect |
| --- | --- |
| `/lsp` | status panel: language, installed version, state |
| `/lsp install <id>` | download the newest release |
| `/lsp update <id>` | re-install from the newest release |

Typing a space after `install` or `update` completes server ids from the
registry and marks each one installed, downloadable, or manual. On the CLI
every subcommand validates the id against the same registry, so a typo prints
the valid list.

The status line carries the live state: `lsp rust` while a server is up,
`lsp rust warm` while it is still loading the workspace, `lsp c down` when it
failed to start, and `+N` for the servers behind it. `/lsp` shows the full list
with ids and versions.

Without a server, L0 provides definitions, references, and identifier lookup
from a real tree-sitter parse indexed in SQLite (stored per workspace under
`.rustedharness/`). All 17 bundled grammars are
available: Rust, C, C++, Go, TypeScript, TSX, JavaScript, Python, Java, Kotlin,
C#, Ruby, PHP, Swift, Scala, Lua, and Elixir. L0 answers are labelled
name-based and lower confidence than LSP results.

## Web UI and API

`rustedharness serve` binds to `127.0.0.1:8787` by default. Set `--token` (or
`web.auth_token`) to require `Authorization: Bearer …`; the WebSocket accepts
the token as a query parameter because browsers cannot set headers on a
WebSocket handshake.

| Endpoint | Description |
| --- | --- |
| `GET /` | Embedded single-page UI |
| `GET /api/health` | Unauthenticated health probe |
| `GET /api/config` | Redacted runtime configuration and resolved model catalog |
| `GET /api/sessions` | Saved session summaries |
| `GET /api/sessions/{id}` | Full transcript |
| `DELETE /api/sessions/{id}` | Delete a saved session |
| `GET /api/tree?session=…&path=…` | Lazy directory listing for the session workspace |
| `GET /api/file?session=…&path=…` | Read-only text payload (256 KiB cap) |
| `GET /ws` | Bidirectional event/command stream |

Each WebSocket connection gets its own `Agent`; closing the tab cancels the
turn and shuts the agent down. The UI is a three-column workspace: a lazy
session/file tree, the chat stream with a todo dock and composer, and a
read-only inspector for code, Markdown/JSON preview, and LSP/process activity.
It streams text and reasoning, renders tool cards and consent prompts, offers
dark/light themes, and uses the real `HarnessEvent` protocol without demo data.
Reasoning cards stay expanded while thinking streams and fold automatically once
the message completes. Tool cards stay open while running, show sanitized live
screen updates from `bash` commands (throttled 16 KiB tails), then fold when the
result arrives. Large transcripts open with only the newest page rendered; a
"load earlier" control walks backwards in bounded pages (up to 1200 messages)
instead of building one DOM node per historical message.
The same page is responsive: the right inspector becomes an off-canvas drawer
below 1180 px, the workspace tree below 900 px, and phone layouts use
safe-area padding, bottom-sheet modals, and 16 px composer text to avoid iOS
zoom. HTML/SVG files are never executed in the preview.

The tree and file endpoints resolve paths through `WorkspacePathResolver`;
they reject traversal and symlinks, skip hidden and build directories, cap
listings at 500 entries, and return binary/oversized files without rendering.

The Tauri GUI starts the same server on an ephemeral loopback port and points a
native window at it, so the desktop and browser experiences stay identical.

## MCP

MCP servers are loaded from `mcp.json` in the workspace, then the config
directory, unless `mcp.configPath` points elsewhere. Stdio and Streamable HTTP
transports are supported through the official `rmcp` client. Exposed tools are
namespaced as `mcp__server__tool`;
`mcp.deny` removes specific tools or whole servers. MCP tool calls are subject
to the same workspace anchoring and timeouts as built-in tools.

## Security notes

- The workspace boundary is enforced on every filesystem tool, including
  symlink resolution; `..` traversal is rejected.
- The web server binds to loopback by default. If exposed, use a token and a
  TLS-terminating proxy.
- LSP downloads require explicit consent and are restricted to HTTPS on
  whitelisted hosts. They are not checksum-verified; run the harness in a
  container or VM if the download source is not trusted.
- Fetched web content is clearly marked untrusted and truncated.
- Shell commands run with the user's privileges. The harness is not a sandbox;
  run it in a container or VM if you need isolation.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo bench --workspace
cargo build --workspace
```

`cargo bench` runs the resume, syntax-highlighting and live-LSP probes. The LSP
probe requires `rust-analyzer` on PATH and skips cleanly when it is absent.

CI runs formatting, clippy with warnings denied, tests, and builds on Linux,
macOS, and Windows. The GUI is built in its default configuration, which does
not require webview libraries; building `--features desktop` additionally needs
the platform webview stack
(WebKitGTK on Linux, WebView2 on Windows, WKWebView on macOS).

## Deliberate choices

These are documented substitutions or additions that keep the build portable
and the trust model explicit:

- **Language-server downloads** are consented, HTTPS-only and host-whitelisted,
  but not checksum-verified.
- **GUI** is an optional `desktop` feature rather than a default build target.

## License

MIT. See `LICENSE`.

`html2md` (used for `read(URL)` HTML conversion) is **GPL-3.0+**. Building and
distributing a binary that links it means the combined distribution must comply
with GPL-3.0; the project's own source remains MIT. If a fully permissive build
is required, replace `html_to_markdown` in `crates/core/src/web.rs` with a
converter that has compatible licensing.
