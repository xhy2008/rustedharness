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

1. Build the system prompt, including environment facts and edit rules.
2. Stream a Chat Completions request, forwarding deltas and reasoning through
   an unbounded channel.
3. Append the assistant message; execute each tool call through the registry
   (MCP tools are included dynamically).
4. Append tool results and repeat until the model stops, the turn limit is
   reached, compaction requires a summary, or cancellation fires.
5. Optionally chase incomplete todos, then persist every message as a JSONL
   session record.

Rate-limited (429) requests retry at `agent.rateLimitIntervalSecs` up to
`agent.rateLimitRetries` times, with `null` meaning retry until cancellation;
quota exhaustion is terminal.

The loop holds no borrows across `.await`: shared state is behind `Arc` and
snapshotted or cloned at await boundaries. Each tool call receives a
`ToolContext` with the workspace resolver, config, provider, LSP hub, web
client, code index, and process registry.

### Configuration

`config.rs` merges `~/.rustedharness/config.json` (settings and default model)
with `~/.rustedharness/models.json` (Pi-style providers and model lists). The
directory can be moved with `RUSTEDHARNESS_DIR`. Model references are
`provider/model` or a bare unique id, and each model carries its own context
window and optional output cap. API keys and header values support literals,
`$ENV`/`${ENV}` interpolation, and `!command`.

### Provider compatibility

`provider.rs` emits OpenAI Chat Completions bodies and parses BYOT SSE chunks.
`responses.rs` implements the OpenAI Responses API (`"api":
"openai-responses"`), translating the internal transcript into `input` items,
`instructions` and flat function tools, and folding `response.*` SSE events
back into the same `Completion`. A `Router` selects the provider that owns each
request's model, so the agent and compaction models can use different endpoints
and different protocols in one session. `Compat` controls `max_tokens` vs
`max_completion_tokens`, `system` vs `developer`, `reasoning_content`,
`tool_choice`, `parallel_tool_calls`, and `stream_options.include_usage`. If a
gateway rejects one of these fields, the provider retries without the offending
knob, one at a time.

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

`hashline.rs` hashes `(previous, current, next)` lines with xxHash32, normalizes
trailing whitespace/CR, and encodes the hash in a short alphabet. `read` emits
`line#anchor|content`; `edit` validates anchors against one snapshot, sorts
operations bottom-up, rejects overlapping ranges, and applies nothing unless
the whole batch is valid. The file is re-read before writing to detect external
changes. `write` appends to existing files instead of truncating.

### Token accounting

`token.rs` records only the `usage` object returned by the provider. It keeps
the latest prompt/completion/total counts for the status UI, the latest context
size for compaction, and cumulative totals. There is no local tokenizer,
heuristic, or calibration factor. If a gateway omits `usage`, the agent emits
one warning and leaves the last known numbers untouched rather than fabricating
values.

### Compaction

`compact.rs` fires when the latest provider-reported context size reaches
`compactThreshold` of `contextWindow` (using `total_tokens`, or
prompt plus completion when a gateway omits the total). It keeps the system
prompt and the last `keepRecentTurns` turns, summarises the middle with
`compactModel`, and records the summary in a five-section form. Orphaned tool
results are pruned with their assistant tool calls. After a successful
compaction the context counter is reset, so stale pre-summary numbers cannot
retrigger compaction.

### Sessions

`session.rs` writes one JSON object per message/record to
`<data>/sessions/<id>.jsonl`, with a header record and compaction markers.
Writes are append-only; a torn final line is ignored on load. Lookup scans the
session directory and accepts an id or a unique prefix. The transcript file is
created lazily with the first user message, so launching and quitting leaves no
artifact behind; compaction records are replayed so a resumed transcript
matches the compacted view.

### Code intelligence

`lsp.rs` speaks JSON-RPC over stdio to one server per language, with document
sync, bounded restarts, idle shutdown, and ten operations. Positions are
converted between UTF-16 (LSP) and byte offsets. `intel.rs` maintains the L0
index in SQLite using real tree-sitter parses, with 17 bundled grammars
(Rust, C, C++, Go, TypeScript, TSX, JavaScript, Python, Java, Kotlin, C#, Ruby,
PHP, Swift, Scala, Lua, Elixir). Definitions come from language-specific AST
node kinds and references from identifier leaves, so comments, strings, and
keywords are not mistaken for symbols. L0 is used while a server warms up or
when none is installed. `lsp_tool.rs` exposes both through a single `lsp` tool
and labels L0 answers as name-based.

Language servers are downloaded from their GitHub releases after consent. The
SHA-256 is resolved automatically from the release asset `digest`, a checksums
file attached to or published beside the release (`SHA256SUMS`, `*.sha256`), or
a `<url>.sha256` sidecar; an unverified download is refused.
`lsp checksum <server>` only inspects that digest, and
`lsp.downloads.<server>.sha256` is an optional override for mirrors. Downloads validate HTTPS, the host whitelist, the archive
cap, extraction safety, and the resolved digest.

### Processes

`proc.rs` spawns commands in a pty for foreground or background execution,
captures raw bytes, emulates the subset of terminal control sequences needed to
render progress bars/clear-screen output, strips ANSI, and tracks child status.
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
provider compatibility fallbacks, usage-only accounting, compaction, session
recovery, terminal emulation, LSP position conversion, tree-sitter extraction
across all bundled grammars, checksum/url validation, MCP naming, web
extraction, and tool schema/no-op guards. CLI/web/TUI end-to-end behavior is
exercised manually against a mock OpenAI-compatible SSE server.
