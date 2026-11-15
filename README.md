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
- An OpenAI-compatible endpoint and an API key in `OPENAI_API_KEY`, or a local
  server such as Ollama added to `models.json`.
- Optional: language servers for semantic code intelligence. The agent falls
  back to a built-in name-based index when none is available.

## Quick start

```sh
cargo build --release
export OPENAI_API_KEY=sk-...

# Create ~/.rustedharness/config.json and models.json (optional; defaults work)
./target/release/rustedharness config init

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
approves language server downloads without prompting (still requires a pinned
SHA-256).

## Configuration

Configuration lives in `~/.rustedharness` by default. Override the directory
with `RUSTEDHARNESS_DIR`, which keeps config, sessions and downloaded language
servers together:

| Path | Purpose |
| --- | --- |
| `config.json` | Default model and harness settings |
| `models.json` | Providers, endpoints and their models (Pi-style) |
| `mcp.json` | Optional MCP servers shared by every project |
| `sessions/` | Append-only JSONL sessions |
| `lsp/` | Cached language-server installations |

The L0 tree-sitter index is the one piece of state that belongs to a workspace
rather than to the user: it lives in `<workspace>/.rustedharness/index.sqlite`
and the directory writes its own `.gitignore` (`*`), so it never shows up in
`git status`. A read-only workspace falls back to an in-memory index.

`rustedharness config init` creates `config.json` and `models.json` without
overwriting existing files. `config path` prints every path, `config show`
prints the effective configuration with credentials redacted, `config models`
lists the resolved model catalog, and `config edit [config|models]` opens the
file in `$VISUAL`/`$EDITOR` (or the platform default).

`--config PATH` accepts a `config.json`, a `models.json`, or a directory
containing both.

### models.json

Declare as many providers as you like. A model is selected as
`provider/model`; bare ids are accepted when they are unique. Model ids that
contain slashes (for example OpenRouter ids) can be used as-is when unique, or
prefixed with their provider.

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
        "maxTokensField": "max_tokens",
        "reasoningContent": true
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
interpolation, or `!command` output. `$$` and `$!` produce literal `$` and `!`.
Local endpoints may use a dummy key or omit it.

Provider fields: `name`, `baseUrl`, `api` (`openai-completions` or
`openai-responses`), `apiKey`, `headers`, `compat`, and `models`.
Model fields: `id`, `name`, `contextWindow` (default 128000), `maxTokens`
(optional output cap), and `reasoning`. A model entry with an existing id
replaces it; new entries are appended. A provider with an empty `baseUrl` is
treated as disabled.

`api` selects the wire protocol:

- `openai-completions` (default) — `POST {baseUrl}/chat/completions`.
- `openai-responses` — `POST {baseUrl}/responses`. The Responses backend maps
  the agent transcript to `input` items and `instructions`, flattens function
  tools, streams `response.output_text.*`, `response.reasoning_*` and
  `response.function_call_arguments.*` events, and reads `input_tokens` /
  `output_tokens` from `usage`. Rejected knobs (`temperature`, `tool_choice`,
  `parallel_tool_calls`, `store`, `max_output_tokens`) are retried without that
  field. Reasoning items are not replayed between turns, so stateless use does
  not require server-side response storage.

`compat` defaults are `maxTokensField: "max_tokens"`, `systemRole: "system"`,
and `reasoningContent`, `toolChoice`, `parallelToolCalls`, `streamUsage` all
enabled. Rejected knobs are retried one at a time.

### config.json

```json
{
  "model": "openai/gpt-4o",
  "compactModel": "openai/gpt-4o-mini",
  "compactWithMainModel": false,
  "contextWindow": 128000,
  "agent": {
    "maxTurns": 100,
    "compactThreshold": 0.8,
    "keepRecentTurns": 8,
    "autoCompact": true,
    "temperature": 0.2,
    "chaseLimit": 3,
    "maxOutputTokens": 8192,
    "rateLimitRetries": null,
    "rateLimitIntervalSecs": 5
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
      "rust-analyzer": { "version": "2026-11-02", "sha256": "<64 hex chars>" }
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
whole URL (a mirror), `sha256` requires a specific digest, `binary` and
`archive` describe an unusual layout.

`model` and `compactModel` may point at different providers; the backend routes
each request independently. `contextWindow` is the fallback for models that do
not declare one. `agent.maxOutputTokens` is the fallback when a model has no
`maxTokens`. Environment variables `RUSTEDHARNESS_MODEL`,
`RUSTEDHARNESS_COMPACT_MODEL`, and `RUSTEDHARNESS_PROVIDER` override the file.

`config show` redacts inline keys and header values.

## Agent behavior

The loop streams assistant text and reasoning, executes tool calls, feeds
results back, and repeats until the model stops or a limit is reached.
Cancellation is cooperative. After edits, language-server diagnostics are
appended to the tool result.

- **Model routing**: `model` drives the loop; `compactModel` is used only for
  summaries. Each request is routed to the provider that owns its model, so the
  two roles can live on different providers. Setting `compactWithMainModel` to
  `true` drops the second model and summarises with `model`: the main model's
  prompt cache is already warm, while a separate summary model starts cold and
  the measured savings do not cover that. The status line then stops showing a
  `compact` column, since there is nothing distinct to show.
- **429 retries**: rate-limited responses (HTTP 429 / `resource_exhausted`)
  retry every `agent.rateLimitIntervalSecs` seconds (recommended 1, 3 or 5)
  for at most `agent.rateLimitRetries` attempts (recommended 3, 5 or 10).
  Set `rateLimitRetries` to `null` to retry until the turn is cancelled.
  Quota/credit exhaustion is terminal and never retried.
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
| `grep` | Regex content search with glob filters. |
| `bash` / `powershell` | Run commands in a pty; background mode returns a pid. |
| `await` | Wait for a background process and return output/exit status. |
| `todo` | Replace, merge, or inspect the task list. |
| `web_search` | Brave → Tavily → DuckDuckGo fallback chain. |
| `lsp` | Ten semantic operations backed by a real language server, with L0 fallback. |
| `mcp__server__tool` | Tools exposed by configured MCP servers. |

Remote `read` results are wrapped as untrusted external content so fetched pages
cannot impersonate instructions. `web_search` uses `BRAVE_API_KEY` and
`TAVILY_API_KEY` when present and always falls back to keyless DuckDuckGo.

### Anchored editing (Hashline)

`read` returns lines tagged as `line#anchor|content`. The anchor is an xxHash32
of the line and its neighbourhood, rendered in the alphabet
`ZPMQVRWSNKTXJBYH`. `edit` accepts a batch:

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
`edit.anchorWidth`. Ops are `replace`, `insert_before`, `insert_after`,
`delete`, and `replace_text`.

If any anchor is stale, the whole batch is rejected with `E_STALE_ANCHOR`; no
partial write occurs. Editing a line invalidates the anchors of its neighbours,
and `write` into an existing file appends without destroying content. The
append result message is exactly `文件已存在，内容已追加`.

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

The expected SHA-256 is then looked up, in order:

1. a pinned `lsp.downloads.<server>.sha256` (explicit override, e.g. a mirror);
2. the digest cached by a previous install;
3. the GitHub release API: the asset's `digest` field, or a checksums file the
   vendor attached to that release (`SHA256SUMS`, `*.sha256`).

Only step 3 calls the API; a resolved digest is cached under
`lsp/checksums/<server>/<version>.sha256`, so repeat installs read the cache and
never call the API again. `GITHUB_TOKEN` is optional and only raises the API's
anonymous 60-requests-per-hour quota.

Verification is best effort, and the outcome is stated:

- a published digest that matches means the binary is verified;
- no digest at all (the vendor published none, the API is rate limited, or the
  network is blocked) is reported as a warning and the install proceeds;
- a published digest that does **not** match is a hard failure
  (`E_LSP_CHECKSUM_MISMATCH`) and nothing is written to the cache.

`lsp checksum <server>` prints what an install would verify, or says that the
release publishes none, so a digest can be pinned before installing.

Shortly after startup the harness asks each *installed* server's release page
for its newest tag and emits `update_available` when it differs from the cached
copy; nothing is downloaded automatically. `lsp update <server>` installs the
newest release and keeps a single version per server in the cache.

Transport rules are enforced for every download:

- HTTPS only, with redirects re-checked against a host whitelist;
- the archive is at most 256 MiB and extracts without escapes;
- connecting times out after 15s and a stalled transfer after 60s;
- the user consents interactively, or `lsp.allowDownloadsNoninteractive` /
  `--yes` is set.

```sh
rustedharness lsp list                    # registry + installed versions
rustedharness lsp install rust-analyzer   # newest release, cached locally
rustedharness lsp update rust-analyzer    # re-install the newest release
rustedharness lsp checksum rust-analyzer  # show the digest and its source
```

The TUI exposes the same four verbs under one command, so nothing LSP-related
is a top-level slash command:

| Command | Effect |
| --- | --- |
| `/lsp` | status panel: language, installed version, state |
| `/lsp install <id>` | download the newest release |
| `/lsp update <id>` | re-install from the newest release |
| `/lsp checksum <id>` | digest an install would verify |

Typing a space after `install`, `update` or `checksum` completes server ids
from the registry and marks each one installed, downloadable, or manual. On the
CLI every subcommand validates the id against the same registry, so a typo
prints the valid list.

The status line carries the live state: `lsp rust` while a server is up,
`lsp rust warm` while it is still loading the workspace, `lsp c down` when it
failed to start, and `+N` for the servers behind it. `/lsp` shows the full list
with ids and versions.

`lsp checksum` is informational: installation performs the same lookup and
verification automatically. Set `GITHUB_TOKEN` only to raise the GitHub API
quota; it is never required for a normal install.

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
directory, unless `mcp.configPath` points elsewhere. Stdio and HTTP+SSE
transports are supported. Exposed tools are namespaced as `mcp__server__tool`;
`mcp.deny` removes specific tools or whole servers. MCP tool calls are subject
to the same workspace anchoring and timeouts as built-in tools.

## Security notes

- The workspace boundary is enforced on every filesystem tool, including
  symlink resolution; `..` traversal is rejected.
- The web server binds to loopback by default. If exposed, use a token and a
  TLS-terminating proxy.
- LSP downloads require explicit consent plus an explicitly pinned checksum.
  `lsp checksum` only reads published release metadata; there is no
  "trust anything from GitHub" install path.
- Fetched web content is clearly marked untrusted and truncated.
- Shell commands run with the user's privileges. The harness is not a sandbox;
  run it in a container or VM if you need isolation.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

CI runs formatting, clippy with warnings denied, tests, and builds on Linux,
macOS, and Windows. The GUI is built in its default configuration, which does
not require webview libraries; building `--features desktop` additionally needs
the platform webview stack
(WebKitGTK on Linux, WebView2 on Windows, WKWebView on macOS).

## Deliberate choices

These are documented substitutions or additions that keep the build portable
and the trust model explicit:

- **Language-server downloads** never invent checksums; the user must pin one.
  `lsp checksum` helps discover the official digest but does not weaken this.
- **GUI** is an optional `desktop` feature rather than a default build target.

## License

MIT. See `LICENSE`.

`html2md` (used for `read(URL)` HTML conversion) is **GPL-3.0+**. Building and
distributing a binary that links it means the combined distribution must comply
with GPL-3.0; the project's own source remains MIT. If a fully permissive build
is required, replace the one call in `crates/core/src/web.rs` with a converter
that has compatible licensing.
