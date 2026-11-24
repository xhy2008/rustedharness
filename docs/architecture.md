# Architecture

RustedHarness is a workspace of five crates. `rustedharness-core` owns all
state and behavior; the four binaries are thin adapters over the same event
protocol.

```
                 ┌──────────────────────────────────────────┐
                 │           rustedharness-core            │
                 │                                          │
  OpenAI-compatible  ──▶  Provider ──▶ Agent loop ──▶ Tools │
                 │             │            │        │      │
                 │        UsageTracker  Compactor  LSP/MCP │
                 │             │            │        │      │
                 │         SessionStore   CodeIndex    Pty   │
                 └───────────────┬──────────────────────────┘
                                 │ HarnessEvent / HarnessCommand
          ┌──────────────────────┼───────────────────────┐
          │                      │                       │
   rustedharness-cli     rustedharness-tui      rustedharness-web
   (run/serve/tui/...)   (ratatui)              (axum + WebSocket)
                                                        │
                                               rustedharness-gui
                                               (Tauri, optional)
```

## Core

`agent.rs` implements the turn loop:

1. Build the minimal system prompt with cross-tool policy; tool-specific
   details stay in each tool description.
2. Stream a Chat Completions request, forwarding deltas and reasoning through
   an unbounded channel.
3. Append the assistant message; execute each tool call through the registry
   (MCP tools are included dynamically).
4. Append tool results and repeat until the model stops, the turn limit is
   reached, compaction requires a summary, or cancellation fires.
5. Optionally chase incomplete todos, then persist every message as a JSONL
   session record.

Rate-limited (429) requests retry at a fixed three-second interval
(`RATE_LIMIT_RETRY_INTERVAL`) for up to `agent.rateLimitRetries` attempts;
quota exhaustion is terminal.

Provider requests have **no timeout by default** (`agent.httpIdleTimeoutMs`
is `0`): a slow free API may take as long as it needs. Every wait — response
headers, each streamed chunk, and the non-streaming `complete` used for
summaries and URL extraction — is raced against the turn's cancellation token,
so Esc/Ctrl+C ends it immediately; `complete` takes the caller's token for
exactly that reason. Setting `httpIdleTimeoutMs` adds an idle budget in
milliseconds: the request is abandoned once that long passes without a single
byte, which is the only case where the harness gives up on a stalled provider.

Cache accounting lives in `cache.rs`. Providers report cached prompt tokens in
`usage`, but not why an entry was lost, and none of them publishes a cache
lifetime, so `CacheWatch` compares each turn with the previous one: prompt
tokens that were already cached but billed again are a miss. Misses above ~20k
tokens are announced once, with the facts that were observed — the missed
tokens, the gap since the previous request, and whether the model changed.
Nothing claims a lifetime, because there is no API to ask for one. Compaction
resets the comparison because it rewrites the prompt on purpose.
`agent.showCacheMissNotices` (default on) silences the notices.

The loop holds no borrows across `.await`: shared state is behind `Arc` and
snapshotted or cloned at await boundaries. Each tool call receives a
`ToolContext` with the workspace resolver, config, provider, LSP hub, web
client, code index, and process registry.

### Configuration

`config.rs` merges `~/.rustedharness/config.json` (settings and model choice)
with `~/.rustedharness/models.json` (Pi-style providers and model lists), using
`jsonc-parser` so comments and trailing commas keep accurate error locations. The
directory can be moved with `RUSTEDHARNESS_DIR`. There is no built-in provider
or default model: an unset choice stays empty, displays as `None`, and the TUI
prompts for configuration; headless runs fail before the first turn. Model
references are `provider/model` or a bare unique id, and each model carries its
own optional context window and output cap. API keys and header values support
literals, `$ENV`/`${ENV}` interpolation (`shellexpand`, with unset variables
expanding to nothing), and `!command`.

### Provider compatibility

`provider.rs` emits OpenAI Chat Completions bodies and parses BYOT SSE chunks.
`responses.rs` implements the OpenAI Responses API (`"api":
"openai-responses"`), translating the internal transcript into `input` items,
`instructions` and flat function tools, and folding `response.*` SSE events
back into the same `Completion` through `async-openai`'s BYOT Responses client. A `Router` selects the provider that owns each
request's model, so the agent and compaction models can use different endpoints
and different protocols in one session. `Compat` resolves Pi's defaults from
the endpoint: `max_tokens` vs `max_completion_tokens`, `system` vs `developer`,
streaming usage,
`finish_reason`, `store`, tool-result names, assistant content, and DeepSeek's
empty `reasoning_content`. `reasoning_content` is replayed automatically when
present; `tool_choice`/`parallel_tool_calls` are left to request options, so
the default request omits them. The provider never retries a rejected request
with fields removed.

Advanced Pi compat fields that require thinking levels, strict/grammar tools,
cache markers, routing, or Anthropic message blocks are not implemented:
`thinkingFormat`, `chatTemplate*`, `openRouterRouting`, `vercelGatewayRouting`,
`zaiToolStream`, `cacheControlFormat`, `promptCacheKeyMode`, session-affinity
headers, `thinkingTokenBudget*`, `supportsStrictMode`,
`supportsOpenAIGrammarTools`, and mid-conversation tool/system flags. They are
ignored, not guessed, so a request never contains a field the endpoint did not
opt into through a supported switch.

### Path anchoring

`paths.rs` resolves every tool path against the session workspace:

- relative paths are joined to the workspace root;
- `..` segments are normalized before the check;
- symlinks are canonicalized before the check;
- existing and new paths are handled separately, so a file can be created
  inside the workspace but not outside it;
- absolute paths inside the workspace are accepted.

This is the only tool-level policy: all tools are auto-allowed otherwise.

### Hashline editing

`hashline.rs` hashes each line's normalized content with xxHash32, ignoring
neighbouring lines, and encodes the hash in a short alphabet. `read` emits
`line#anchor|content`; the line number is authoritative and the anchor only
confirms that the line still has the content the model saw. Identical lines
therefore share an anchor; collisions are accepted because the hash is a guard,
not an identity. `edit` validates anchors against one snapshot, sorts operations
bottom-up, rejects overlapping ranges and duplicate insertion points, and
applies nothing unless the whole batch is valid. The file is re-read before
writing to detect external changes. `write` appends to existing files instead of
truncating.

### Token accounting

`token.rs` records only the `usage` object returned by the provider. It keeps
the latest prompt/completion/total counts for the status UI, the latest context
size for compaction, and cumulative totals. There is no local tokenizer,
heuristic, or calibration factor. If a gateway omits `usage`, the agent emits
one warning and leaves the last known numbers untouched rather than fabricating
values.

### Compaction

`compact.rs` fires when the latest provider-reported context size reaches
`compactThreshold` of `contextWindow` (using `total_tokens`, or prompt plus
completion when a gateway omits the total). It keeps the system prompt and the
last `keepRecentTurns` turns and records the summary in a five-section form.
Orphaned tool results are pruned with their assistant tool calls. After a
successful compaction the context counter is reset, so stale pre-summary
numbers cannot retrigger compaction.

`compactModel` is optional. With no separate model (the default, shown as
`None` in `/settings`), the summary call reuses the agent model's live system
prompt, tool catalog and old messages, appending only the summary instruction:
the provider sees the same prefix and can serve it from prompt cache. A
separate summary model starts cold. `compactStripContext` enables trimming:
`sendRecentTurns` recent user turns of the summarised transcript are sent in
full, while older turns lose the content selected by `stripContent` (`none`,
`toolOutput`, or
`toolOutputAndThinking`). `keepRecentTurns` independently controls how many
turns are kept verbatim after compaction. `compactThinking` sets the reasoning
effort of the summary request for both paths: `off` omits the field, otherwise
the level is sent as `reasoning_effort` on Chat Completions and as
`reasoning.effort` on Responses, but only when the compaction model declares
`reasoning` and the provider's compat allows reasoning effort. `/settings`
exposes the model, thinking, threshold, both turn counts and the strip
controls. Auto-compaction is always
on, and each change applies to the running agent and is persisted under
`agent` in `config.json`. While the model is `None` or
`compactStripContext` is off, the dependent trim controls are disabled.

### Adding models at runtime

`/model` lists the configured models and ends with an "Add model..." row that
opens a full-screen form (`AddModelState` in `tui/src/app.rs`): base URL, API
key, then the models the endpoint advertises. The list comes from
`discovery.rs`, which asks `GET {base}/models` and accepts the envelopes and
field names seen in the wild (`data`/`models`, `context_length`/
`context_window`/`max_model_len`/OpenRouter's nested `top_provider`), leaving
anything it does not recognise unknown instead of guessing. The request runs on
a worker task and comes back as `OpResult::Catalog`, so the UI never blocks on
the network; a reply whose URL no longer matches the open wizard is dropped.

`Config::add_model` edits `models.json` as JSON rather than regenerating it, so
hand-written keys and comments-adjacent structure survive; the provider id is
derived from the endpoint host and an unknown context window writes no key at
all. `Agent::add_model` then reloads `config.json`/`models.json`, rebuilds the
router and swaps the `ToolContext`'s config and backend, so the model the wizard
just saved is usable in the running session.

### Sessions

`session.rs` writes pi's v3 session format, one JSON object per entry, to
`<dir>/--<encoded cwd>--/<timestamp>_<id>.jsonl`. The directory and file naming,
the header, the entry ids and `parentId` chaining, and every entry type are pi's,
so `session.dir` can point at `~/.pi/agent/sessions` and both tools read and
append the same files. Writes are append-only; a torn final line is ignored on
load. Lookup scans the session directory and accepts an id or a unique prefix,
including for pi file names where the id follows the last `_`.

Two kinds of noise are filtered rather than carried forward. On load, a message
whose content is empty is dropped (pi stores every failed API call as an
assistant message with `stopReason: "error"` and no content), as is a user line
that is a frontend command (`/model x`, `/view ...`); roles we do not model
(`bashExecution`, `custom`, future ones) are converted like pi converts them or
skipped, never treated as a torn line. On write, the same two shapes are never
produced, and the model is kept as a single `custom` snapshot entry that is
rewritten in place instead of a `model_change` trail.

The transcript file is created lazily with the first user message, so launching
and quitting leaves no artifact behind; compaction records are replayed with
pi's exact summary wording so a resumed transcript matches the compacted view.
`LoadedSession::path` carries the resolved file, which is what `Agent::resume`
appends to.

### Code intelligence

`lsp.rs` speaks JSON-RPC over stdio to one server per language, with document
sync, bounded restarts, idle shutdown, and ten operations. Result types come
from `lsp-types`, and `line-index` converts between UTF-16 (LSP) and byte
offsets. `intel.rs` maintains the L0
index in SQLite using real tree-sitter parses, with 17 bundled grammars. The
database lives in `<workspace>/.rustedharness/index.sqlite`: the index describes
one workspace, so it is created, invalidated and deleted with it, and a
`.gitignore` inside that directory keeps it out of version control. A workspace
that cannot be written to falls back to an in-memory database.
(Rust, C, C++, Go, TypeScript, TSX, JavaScript, Python, Java, Kotlin, C#, Ruby,
PHP, Swift, Scala, Lua, Elixir). Definitions come from language-specific AST
node kinds and references from identifier leaves, so comments, strings, and
keywords are not mistaken for symbols. L0 is used while a server warms up or
when none is installed. `lsp_tool.rs` exposes both through a single `lsp` tool
and labels L0 answers as name-based.

A server binary is found through `which` (PATH with `PATHEXT` on Windows) or
the per-language/id `lsp.servers` override before the cache is consulted.

Language servers are installed from their GitHub releases after consent. The
registry stores no version: `ServerSpec::repo` names the project and
`ServerSpec::asset` is the file name template, so `latest_tag` reads the tag out
of the `releases/latest` redirect (a plain HTTPS request, no API quota) and
`download_target` builds `.../releases/download/<tag>/<asset>`. The asset
template carries `{tag}`, `{target}`, `{os}` and `{ext}`, because GitHub assets
embed the tag and change extension per OS; the archive format is inferred from
the resolved file name. A pinned `lsp.downloads.<server>.version` skips the
redirect, and `.url` replaces the URL entirely, which is what keeps installs
working when github.com is unreachable.

`download_manifest` validates the resolved URL against the HTTPS and
official-host rules, then streams the artifact into the cache. Downloads are
not checksum-verified: trust comes from explicit consent, the host whitelist,
the 256 MiB cap and extraction checks that reject paths escaping the target
directory.

`LspHub::new` schedules `check_updates`, which compares each installed version
with `latest_tag` and emits `update_available` when they differ. Installs write
`lsp/<server>/<tag>/` and `prune_other_versions` deletes the directory they
replaced, so `installed` always describes a single copy.

### Web search

`web_search` runs exactly one backend, picked in `/settings` → web search and
stored under `[search]` in `config.json` (`SearchConfig`). Each backend has its
own payload parser as a pure function (`parse_brave`, `parse_tavily`,
`parse_searxng`, `parse_bing_api`, `parse_bing_html`, `parse_duckduckgo_html`)
so the shapes are tested without a network. Brave, Tavily and SearXNG are
JSON APIs; Bing scrapes its results page until an API key upgrades it to the
official v7 endpoint; DuckDuckGo scrapes `html.duckduckgo.com`, unwrapping its
`/l/?uddg=` redirects. A backend that cannot answer says which credential it
needs instead of silently returning nothing, and `SearchConfig::credential`
falls back to the documented environment variables when the file is empty.

### Web fetching

`web.rs` fetches a page, decides whether the body is HTML, and converts it for
the model. HTML is parsed once with `html5ever`; the boilerplate subtrees
(`script`, `style`, `noscript`, `svg`, `head`, `iframe`, `template`) are pruned
from that DOM, and the same tree is walked by `html2md` to produce Markdown, so
nothing is serialized and re-parsed on the way. Titles come from the `<title>`
element's text, which the parser has already entity-decoded, and search-result
titles and snippets are reduced to text with the same walk. Entity decoding uses
`html-escape`, so every named reference is understood rather than a fixed list.
Bodies over `MAX_BODY_BYTES` are rejected before parsing and the Markdown is
truncated to `MAX_MARKDOWN_CHARS`.

### Processes

`proc.rs` spawns commands in a pty for foreground or background execution,
captures raw bytes, feeds them to `vt100` for progress bars/clear-screen output,
strips ANSI, and tracks child status.
Foreground runs can also publish throttled, byte-bounded screen tails as
`tool_output` events keyed by tool-call id; the final `tool_result` remains
authoritative. Background processes are addressable by pid through `await`;
their process groups are killed on timeout.

## Frontends

All frontends translate their input into `HarnessCommand` and render
`HarnessEvent`. The CLI streams human-readable cards or JSONL. The TUI renders
a scrollback, input editor, todo/LSP/process panels, and a consent modal. The
web crate keeps one `Agent` per WebSocket connection; each connection gets a
snapshot on attach and the session is shut down on disconnect. The GUI starts
the same web server on an ephemeral loopback port and opens a Tauri window at
that URL.

## Testing

Unit tests live beside the modules. They cover path escapes (including
symlinks), anchor stability/staleness, batch atomicity, CRLF preservation,
provider compatibility switches, usage-only accounting, compaction, session
recovery, terminal emulation, LSP position conversion, tree-sitter extraction
across all bundled grammars, download URL validation, rmcp tool results, web
extraction, and tool schema/no-op guards. CLI/web/TUI end-to-end behavior is
exercised manually against a mock OpenAI-compatible SSE server.

Performance probes live in `benches/` and run with `cargo bench`: session
load/resume (`core/benches/resume.rs`), syntax highlighting
(`tui/benches/highlight.rs`), and live LSP cold/warm latency
(`core/benches/lsp_latency.rs`). The LSP probe skips when `rust-analyzer` is not
on PATH.
