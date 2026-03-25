# Feature: Modular Runtime, Computer Use Tools, and Web Tools

## Summary

Refactor Borealis from hardcoded component wiring to a fully trait-driven, registry-based runtime. Every major subsystem becomes a swappable trait: `Channel`, `Tool`, `Provider`, `Memory`, and `Observer`. New tool groups — computer use (bash, file read/write/list) and web (fetch via Jina Reader, search via Jina Search) — are the first modules built on the new pattern. The XML directive system is replaced with channel-provided action tools — each channel registers its own tools (react, send_message, send_file) with platform-specific implementations, making actions native tool calls instead of fragile regex-parsed XML. Security moves from a catch-all `rate_limit.rs` into a dedicated `Security` module with middleware-style tool authorization. The goal: adding a new channel, tool, provider, memory backend, or lifecycle hook requires zero changes to `main.rs`.

Inspired by [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw)'s trait-based modularity where core systems are all swappable traits.

## Requirements

- REQ-1: **Channel registry** — Channels register themselves via a `ChannelRegistry` that replaces the hardcoded CLI/Discord wiring in `main.rs:144-226`. Each registered channel gets its own inbound/outbound/processing loop spawned automatically. Adding a new channel means implementing the `Channel` trait and calling `registry.register()`.

- REQ-2: **Tool trait rename and group registry** — Rename `ToolHandler` (`tools/mod.rs:40`) to `Tool` for consistency with the other core traits (`Channel`, `Provider`, `Memory`). Rename `ErasedToolHandler` to `ErasedTool`. Tool groups register via a function pattern (`register_*_tools(&mut ToolRegistry, ...)`) controlled by config. The existing `register_memory_tools` is already this pattern. New groups follow the same convention. Scheduler events can optionally restrict which tool groups are available via a `tools` field.

- REQ-3: **Provider registry** — The `build_pipeline` function (`main.rs:240-331`) currently hardcodes Anthropic-then-OpenAI fallback. Refactor to iterate configured providers and build the pipeline from the first valid one, or allow named provider selection per use case (conversation vs compaction).

- REQ-4: **Computer use tools** — Four tools: `bash_exec`, `file_read`, `file_write`, `file_list`. All operate within a configurable `sandbox_root` (default: project root). The `memory/` directory is excluded — memory access stays behind `memory_*` tools. `bash_exec` has an optional command allowlist for restrictive deployments. Aurora can write scripts and execute them as a first step toward self-extension.

- REQ-5: **Web tools via Jina AI** — Two tools: `web_fetch(url)` via Jina Reader API (`r.jina.ai`) and `web_search(query)` via Jina Search API (`s.jina.ai`). Jina handles HTML-to-markdown conversion server-side, eliminating the need for an HTML parsing dependency. Free tier provides 10M tokens with no credit card required. Configurable backend trait allows adding SearXNG/Tavily alternatives later.

- REQ-6: **Discord wiring via channel registry** — The existing Discord adapter (`channels/discord.rs`, 329 lines) is registered through the new channel registry instead of being hardcoded. No changes to the adapter itself.

- REQ-7: **Memory trait extraction** — Extract `MemoryStore` (`memory/store.rs`) from a concrete struct into a `Memory` trait. The current SQLite implementation becomes `SqliteMemory`. This enables swapping to the MemFS v2 backend (jj-backed markdown) without touching tool handlers, pipeline, or any consumer. The trait surface matches the existing public methods: `create_note`, `read_note`, `update_note`, `forget_note`, `search_notes`, `list_notes`, `link_notes`, `get_links_for_note`, `tag_note`, `load_core_persona`.

- REQ-8: **Security module** — Split `rate_limit.rs` into focused modules. Currently it contains three unrelated concerns: token bucket rate limiting, path traversal validation, and tool authorization — and the latter two aren't even wired into the pipeline yet. New structure:
  - `security/rate_limit.rs` — Token bucket rate limiter (per-user + global), unchanged
  - `security/sandbox.rs` — Path validation, sandbox root enforcement, memory dir exclusion
  - `security/authorization.rs` — Tool authorization (which tools are restricted, which users can call them), applied as middleware in the tool execution path
  - `security/mod.rs` — `Security` struct that composes all three, injected into the pipeline

- REQ-9: **Observer trait (lifecycle hooks)** — An `Observer` trait with default no-op methods for lifecycle events. Multiple observers can be registered. Hook points: before/after LLM call, before/after tool execution, on message received, on error. Initial implementation: a `TracingObserver` that logs events via `tracing`. Future uses: cost tracking, metrics, Aurora's self-monitoring, rate limit enforcement.

- REQ-10: **Per-event tool configuration** — Scheduler events (`config.rs:344-363`) gain an optional `tools` field listing available tool groups (e.g. `["memory", "computer", "web"]`). When omitted, all enabled tool groups are available. The pipeline receives the active tool set and filters `ToolRegistry.definitions()` accordingly.

- REQ-11: **Replace directives with channel-provided tools** — Delete the XML directive system (`core/directive.rs`, 367 lines) and replace with channel-specific action tools. Each channel registers its own action tools (e.g. `react`, `send_message`, `send_file`) with platform-specific implementations when it joins the registry. The pipeline no longer parses `<actions>` blocks from LLM output — actions are native tool calls. The `Directive` enum, `DirectiveKind`, `parse_directives()`, `strip_directives()`, and `supported_directives()` on the `Channel` trait are all removed. A new `ToolGroup::Channel` group is dynamically populated per-conversation based on which channel the message originated from. `NoReply` is handled by convention: empty text response = no reply.

## Acceptance Criteria

- [ ] AC-1: `cargo build` succeeds with zero clippy warnings after all changes. (REQ-1 through REQ-10)
- [ ] AC-2: Adding a new channel requires only: (a) implementing `Channel` trait, (b) adding config section, (c) calling `registry.register()`. No changes to `main.rs`. (REQ-1)
- [ ] AC-3: All references to `ToolHandler` are renamed to `Tool`. `ErasedToolHandler` becomes `ErasedTool`. All existing tests pass with the rename. (REQ-2)
- [ ] AC-4: Disabling a tool group in config (e.g. `[tools.computer_use] enabled = false`) removes those tools from `ToolRegistry.definitions()` — they are not sent to the LLM and cannot be called. (REQ-2)
- [ ] AC-5: Adding a new provider requires only: (a) implementing the `Provider` trait, (b) adding a config section, (c) adding a match arm in the provider builder. No changes to pipeline or main.rs. (REQ-3)
- [ ] AC-6: `bash_exec` with command `echo hello` returns `hello`. A command that tries to escape the sandbox is rejected. (REQ-4, REQ-8)
- [ ] AC-7: `file_write` to `workspace/test.txt` succeeds. `file_write` to `memory/core.md` is rejected with an error referencing the memory tools. `file_write` to `../../etc/passwd` is rejected by path traversal prevention. (REQ-4, REQ-8)
- [ ] AC-8: `file_read` reads a file within sandbox_root. `file_list` returns directory contents. Both reject paths outside the sandbox. (REQ-4)
- [ ] AC-9: `web_fetch("https://example.com")` returns markdown content via Jina Reader. `web_search("rust programming")` returns structured results via Jina Search. Both respect rate limiting. (REQ-5)
- [ ] AC-10: Discord bot connects, receives a message in a `mention-only` channel when @mentioned, responds in the correct channel. Registered via channel registry. (REQ-1, REQ-6)
- [ ] AC-11: A test `MockMemory` implementing the `Memory` trait can be substituted for `SqliteMemory` in the pipeline without changing any tool handler code. (REQ-7)
- [ ] AC-12: `rate_limit.rs` is split into `security/` module. `Security` struct is injected into the pipeline. Tool authorization is enforced in the tool execution path — a non-allowed user calling `bash_exec` gets an authorization error. (REQ-8)
- [ ] AC-13: A `TracingObserver` logs before/after events for LLM calls and tool executions. An observer registered via `pipeline.add_observer()` receives all lifecycle events. (REQ-9)
- [ ] AC-14: A scheduler event with `tools = ["memory"]` does not include computer or web tools in the LLM request, even when those groups are globally enabled. An event with no `tools` field includes all enabled tool groups. (REQ-10)
- [ ] AC-15: `core/directive.rs` is deleted. No XML parsing of `<actions>` blocks occurs in the pipeline. The `Directive` enum and `DirectiveKind` are removed from `core/event.rs`. (REQ-11)
- [ ] AC-16: A Discord message includes `react`, `send_message`, `send_file` in the tool definitions sent to the LLM. A CLI message includes only `send_message` (or no channel tools). The LLM calling `react(emoji="heart")` on a Discord conversation adds the reaction via the Discord API. (REQ-11)
- [ ] AC-17: When the LLM returns an empty text response (no text, only tool calls), no message is sent to the channel. This replaces the `NoReply` directive. (REQ-11)

## Architecture

### Core Traits

The five core traits that make Borealis modular:

```
Channel    — Platform adapters (CLI, Discord, Telegram, ...)
Tool       — LLM-callable capabilities (memory, bash, file, web, ...)
Provider   — LLM backends (Anthropic, OpenAI, Ollama, ...)
Memory     — Knowledge storage backends (SQLite, MemFS v2, ...)
Observer   — Lifecycle hooks (tracing, metrics, cost tracking, ...)
```

Each trait has a registry or injection point. Adding a new implementation of any trait requires zero changes to `main.rs` or the pipeline.

### Channel Registry

Replace the hardcoded channel wiring in `main.rs:144-226` with a registry:

```
src/channels/
├── mod.rs          # Channel trait + ChannelRegistry
├── cli.rs          # CliAdapter (unchanged)
├── discord.rs      # DiscordAdapter (unchanged)
└── modes.rs        # ResponseMode trait (unchanged)
```

The `Channel` trait uses `impl Future` returns (`channels/mod.rs:21-30`), making it non-object-safe — same situation as `Tool`. Two options:

1. **Erased wrapper** (matches `ErasedTool` pattern at `tools/mod.rs:56-77`): Create `ErasedChannel` trait with `Pin<Box<dyn Future>>` returns, blanket-impl for all `Channel`, store as `Vec<Box<dyn ErasedChannel>>`.
2. **Concrete registration** (simpler): each `register()` call takes the concrete type and spawns immediately. The registry tracks `JoinHandle`s for shutdown, not trait objects.

Option 2 is simpler and sufficient since channels are registered at startup, not dynamically. The registry becomes a spawn coordinator rather than a collection of trait objects.

`main.rs` becomes:

```rust
let mut channels = ChannelRegistry::new();
borealis::channels::cli::register(&mut channels, &settings, pipeline.clone(), cancel.clone());
borealis::channels::discord::register(&mut channels, &settings, pipeline.clone(), cancel.clone());
channels.await_shutdown().await;
```

### Tool Rename and Groups

Rename `ToolHandler` → `Tool`, `ErasedToolHandler` → `ErasedTool` throughout the codebase.

```
src/tools/
├── mod.rs              # Tool trait, ToolRegistry, ToolGroup enum
├── memory_tools.rs     # register_memory_tools() — 9 tools
├── computer_tools.rs   # register_computer_tools() — 4 tools
└── web_tools.rs        # register_web_tools() — 2 tools
```

Add a `ToolGroup` enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolGroup {
    Memory,
    Computer,
    Web,
    Channel,  // dynamically populated per-conversation based on source channel
}
```

`ToolRegistry` gains group-awareness: each registered tool is tagged with its group. `definitions()` can be filtered by active groups:

```rust
impl ToolRegistry {
    pub fn register_with_group<T: Tool + 'static>(&mut self, handler: T, group: ToolGroup);
    pub fn definitions_for(&self, groups: &[ToolGroup]) -> Vec<ToolDef>;
}
```

### Memory Trait

Extract from `memory/store.rs`:

```rust
pub trait Memory: Send + Sync {
    fn create_note(&self, title: &str, content: &str, tags: &[String]) -> MemoryResult<Note>;
    fn read_note(&self, id: &str) -> MemoryResult<Note>;
    fn update_note(&self, id: &str, content: &str) -> MemoryResult<Note>;
    fn forget_note(&self, id: &str) -> MemoryResult<()>;
    fn search_notes(&self, query: &str, limit: usize) -> MemoryResult<Vec<Note>>;
    fn list_notes(&self, tag_filter: Option<&str>) -> MemoryResult<Vec<Note>>;
    fn link_notes(&self, from_id: &str, to_id: &str, relation: &str) -> MemoryResult<Link>;
    fn get_links_for_note(&self, id: &str) -> MemoryResult<Vec<Link>>;
    fn tag_note(&self, id: &str, tags: &[String]) -> MemoryResult<Note>;
    fn load_core_persona(&self) -> MemoryResult<String>;
}
```

The current `MemoryStore` struct becomes `SqliteMemory` implementing `Memory`. All consumers (`memory_tools.rs`, `pipeline.rs`) receive `Arc<dyn Memory>` instead of `MemoryStore`. This is the seam for MemFS v2 — when the jj-backed backend is ready, it implements `Memory` and swaps in via config.

### Observer Trait

```rust
pub trait Observer: Send + Sync {
    fn on_message_received(&self, _event: &InEvent) {}
    fn on_llm_request(&self, _messages: &[ChatMessage], _tools: &[ToolDef]) {}
    fn on_llm_response(&self, _response: &LlmResponse, _duration: Duration) {}
    fn on_tool_call(&self, _call: &ToolCall, _ctx: &ToolContext) {}
    fn on_tool_result(&self, _call: &ToolCall, _result: &ToolResult, _duration: Duration) {}
    fn on_error(&self, _error: &anyhow::Error) {}
}
```

All methods have default empty implementations — observers only override what they care about. Multiple observers can be registered:

```rust
pub struct ObserverRegistry {
    observers: Vec<Box<dyn Observer>>,
}

impl ObserverRegistry {
    pub fn notify_llm_request(&self, messages: &[ChatMessage], tools: &[ToolDef]) {
        for observer in &self.observers {
            observer.on_llm_request(messages, tools);
        }
    }
}
```

The pipeline calls `observer_registry.notify_*()` at each hook point. Initial implementation ships with `TracingObserver` that logs structured events via `tracing`:

```rust
struct TracingObserver;

impl Observer for TracingObserver {
    fn on_llm_response(&self, response: &LlmResponse, duration: Duration) {
        tracing::info!(
            tokens_in = response.usage.input_tokens,
            tokens_out = response.usage.output_tokens,
            tool_calls = response.tool_calls.len(),
            duration_ms = duration.as_millis(),
            "llm response"
        );
    }

    fn on_tool_result(&self, call: &ToolCall, result: &ToolResult, duration: Duration) {
        tracing::info!(
            tool = call.name,
            is_error = result.is_error,
            duration_ms = duration.as_millis(),
            "tool execution"
        );
    }
}
```

### Computer Use Tools

**`bash_exec`** — Runs a shell command via `tokio::process::Command`. Returns stdout, stderr, and exit code as JSON. Enforced constraints:
- Working directory set to `sandbox_root`
- Optional command allowlist (if configured, rejects commands not in the list)
- Timeout (configurable, default 30s)
- No interactive commands (stdin is closed)

**`file_read`** — Reads a file path relative to `sandbox_root`. Returns content as string. Rejects paths outside sandbox (resolved via `canonicalize()` + prefix check). Rejects paths under `memory/`.

**`file_write`** — Writes content to a file path relative to `sandbox_root`. Creates parent directories. Same path restrictions as `file_read`. Restricted tool (authorization enforced).

**`file_list`** — Lists directory contents relative to `sandbox_root`. Returns file names, sizes, and types (file/dir). Optional recursion with depth limit.

### Web Tools (Jina AI)

Jina AI provides two APIs that map directly to our needs:

**`web_fetch`** — Uses Jina Reader API (`https://r.jina.ai/{url}`). Returns clean markdown — no HTML parsing dependency needed. The API handles JavaScript rendering, cookie walls, and content extraction server-side. Request via `reqwest` with `Accept: text/markdown` header and optional `Authorization: Bearer {api_key}` for higher rate limits.

**`web_search`** — Uses Jina Search API (`https://s.jina.ai/?q={query}`). Returns structured SERP results as JSON with `title`, `url`, `content` (snippet). Same auth pattern.

```rust
pub trait SearchBackend: Send + Sync {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>>;
}

pub trait ReaderBackend: Send + Sync {
    async fn fetch(&self, url: &str) -> Result<String>;
}
```

**Jina free tier**: 10M tokens per API key, no credit card. Rate limits tracked by RPM/TPM per key. Without an API key, rate limited by IP.

Config supports alternative backends for future use:

```toml
[tools.web]
enabled = true
reader_backend = "jina"            # default, or "raw" for direct reqwest
search_backend = "jina"            # default, or "searxng", "tavily"
jina_api_key_env = "JINA_API_KEY"  # optional, for higher rate limits
searxng_url = ""                   # required if search_backend = "searxng"
tavily_api_key_env = ""            # required if search_backend = "tavily"
max_fetch_bytes = 51200            # 50KB
max_search_results = 5
```

### Channel Tools (replacing directives)

Delete `core/directive.rs` (367 lines of XML regex parsing) and the `Directive`/`DirectiveKind` enums from `core/event.rs`. Replace with channel-provided tools.

Each channel registers its action tools when it joins the registry. These form a `ToolGroup::Channel` group that is dynamically assembled per-conversation:

```rust
// ToolGroup gains a Channel variant
pub enum ToolGroup {
    Memory,
    Computer,
    Web,
    Channel,  // dynamically populated per-conversation
}
```

**Discord channel tools:**

| Tool | Description | Maps to |
|---|---|---|
| `react(emoji, message_id?)` | Add emoji reaction to a message | `serenity::Message::react()` |
| `send_message(channel, text)` | Send a message to a named channel | `serenity::ChannelId::say()` |
| `send_file(path, kind)` | Send a file attachment | `serenity::ChannelId::send_files()` |

**CLI channel tools:**

| Tool | Description |
|---|---|
| `send_message(text)` | Print to stdout (limited utility, but consistent) |

Each channel implements a method that returns its available tools:

```rust
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    fn run_inbound(...) -> impl Future<Output = Result<()>> + Send;
    fn run_outbound(...) -> impl Future<Output = Result<()>> + Send;

    /// Register channel-specific action tools into the registry.
    /// Called once at startup. Tools receive Arc<Self> for platform access.
    fn register_tools(&self, registry: &mut ToolRegistry);
}
```

This replaces `supported_directives() -> Vec<DirectiveKind>`. The pipeline no longer needs to parse LLM output for embedded XML — the LLM calls tools natively via the provider API.

**Per-conversation tool filtering:** When a message arrives from Discord, the pipeline includes Discord's channel tools in the tool definitions. When a message arrives from CLI, it includes CLI's tools (or none). The `ToolRegistry.definitions_for()` method handles this by accepting both static groups (Memory, Computer, Web) and the dynamic channel group.

**NoReply convention:** If the LLM produces only tool calls and no text (or empty text), the pipeline does not send a message to the channel. This naturally replaces the `NoReply` directive.

**What gets deleted:**
- `src/core/directive.rs` — entire file (367 lines)
- `Directive` enum from `src/core/event.rs`
- `DirectiveKind` enum from `src/core/event.rs`
- `supported_directives()` from `Channel` trait
- `directives` field from `OutEvent`
- `parse_directives()` and `strip_directives()` calls from `pipeline.rs`
- Directive handling in `channels/discord.rs` outbound loop (replaced by tool execution)
- 13 directive tests

### Security Module

Split `rate_limit.rs` (which currently contains three unrelated concerns, two of which aren't wired into the pipeline) into a focused module:

```
src/security/
├── mod.rs              # Security struct composing all three
├── rate_limit.rs       # TokenBucket, RateLimiter (moved from rate_limit.rs)
├── sandbox.rs          # Path validation, sandbox root, memory dir exclusion
└── authorization.rs    # Tool authorization (restricted tools, user allowlists)
```

**Authorization** integrates into the tool execution path in `pipeline.rs`. Before executing a tool, the pipeline checks:

```rust
// In pipeline.rs tool execution loop
if security.is_restricted_tool(&call.name) && !security.is_user_authorized(&ctx.author_id) {
    // Return error to LLM, don't execute
}
```

**Restricted tools** are declared by each tool group at registration time, not hardcoded in a constant:

```rust
pub fn register_computer_tools(registry: &mut ToolRegistry, security: &mut Security, ...) {
    security.register_restricted("bash_exec");
    security.register_restricted("file_write");
    registry.register_with_group(BashExec::new(...), ToolGroup::Computer);
    // ...
}
```

### Config Additions

```toml
[tools.computer_use]
enabled = true
sandbox_root = "."                    # default: project root
memory_dir = "memory"                 # excluded from file tools
command_allowlist = []                # empty = all commands allowed
command_timeout_secs = 30

[tools.web]
enabled = true
reader_backend = "jina"
search_backend = "jina"
jina_api_key_env = "JINA_API_KEY"     # optional
max_fetch_bytes = 51200
max_search_results = 5
```

Scheduler event config gains optional tools field:

```toml
[[scheduler.events]]
name = "heartbeat"
type = "recurring"
interval = "30m"
prompt = "..."
tools = ["memory", "computer", "web"]   # optional, omit for all

[[scheduler.events]]
name = "daily_reflection"
type = "cron"
cron = "0 22 * * *"
prompt = "..."
tools = ["memory"]                       # reflection only gets memory tools
```

### Provider Registry (lightweight)

Replace the if-else chain in `main.rs:252-326` with a provider builder that:
1. Iterates configured providers in priority order
2. Builds the first one that has valid config (API key present, etc.)
3. Returns `Arc<dyn Provider>` wrapped in `Pipeline`

This doesn't need a full registry pattern — just a function that returns the right provider. Adding a new provider means adding `providers/gemini.rs` and a match arm, not restructuring main.rs.

## Implementation Notes (from gap analysis)

These are actionable findings from the `kickoff plan` gap analysis that implementers must address:

### Memory trait: sync vs async
The `Memory` trait methods are synchronous (matching the current `MemoryStore` API). Callers wrap them in `tokio::task::spawn_blocking`. If a future `Memory` impl (MemFS v2) needs async I/O, the trait would need to change. **For now**: keep sync, use `spawn_blocking` at call sites. Revisit when MemFS v2 is designed.

### Memory consumers need Arc<dyn Memory>
`MemoryStore` is currently `Clone` and stored by value in `Pipeline` (`pipeline.rs:61`) and all 9 tool structs (`memory_tools.rs`). Extracting the trait requires changing all of these to `Arc<dyn Memory>`. The `migrate.rs` module should keep the concrete `SqliteMemory` type since migration is not polymorphic.

### Pipeline parameter explosion
`Pipeline::new()` already takes 9 parameters with `#[allow(clippy::too_many_arguments)]`. Adding `Security` and `ObserverRegistry` makes it 11. **Solution**: introduce a `PipelineDeps` struct that bundles the injected dependencies.

### Scheduler events → tool restrictions propagation
`InEvent` (`core/event.rs`) has no field for tool restrictions. Adding per-event tool config requires: (1) `tools: Option<Vec<ToolGroup>>` on `SchedulerEventConfig`, (2) propagating it through the scheduler → `InEvent` → pipeline. The pipeline uses filtered definitions when restrictions are present.

### Authorization for system events
`ToolContext.author_id` for scheduler events is synthetic. Tool authorization must auto-allow system-originated events (scheduler, cron) — authorization only applies to user-initiated channel requests.

### bash_exec environment leakage
Spawned processes inherit the parent's environment, including API keys (`ANTHROPIC_API_KEY`, etc.). **Solution**: `Command::env_clear()` then selectively re-add safe vars (`PATH`, `HOME`, `LANG`, `TERM`), or use a configurable env allowlist.

### TOCTOU in file path validation
`canonicalize()` + prefix check has a time-of-check/time-of-use race. Acceptable for single-user deployment. Document as known limitation.

### SearchBackend/ReaderBackend object safety
These traits use `async fn` which has the same object-safety issue as `Tool` and `Channel`. **Solution**: use `Pin<Box<dyn Future>>` return types directly in the trait, or use the erased wrapper pattern.

### Type namespace overlap after Tool rename
`tools::Tool`, `tools::ToolCall`, `tools::ToolResult`, `tools::ToolDef`, `tools::ToolContext` all in the same module. Not a compilation issue (namespaced) but `providers::ToolDef` and `providers::ToolCall` are separate types with identical field names. Document clearly which is which.

### Dead code removed
`validate_sendfile_path()`, `is_memory_write_tool()`, and `MEMORY_WRITE_TOOLS` were dead code in `rate_limit.rs` — never called by the pipeline. Deleted in commit `8a7c65f`. Path validation and tool authorization will be reimplemented properly in the `security/` module.

## Resolved Questions

### Q1: Bash sandboxing beyond directory restriction
**Decision**: Both containerization and in-process sandboxing, both optional and stackable. For this iteration, implement directory sandboxing via `Command::current_dir` + path validation. Document that production deployments should run in a container (Docker/Podman). Research Linux sandboxing (landlock, seccomp, bubblewrap) as a future enhancement — add as a config option (`[tools.computer_use] sandbox_mode = "directory" | "landlock" | "container"`) when ready.

### Q2: Web search implementation
**Decision**: Lead with Jina AI for both reader and search. Jina Reader (`r.jina.ai`) handles URL-to-markdown conversion server-side — no HTML parsing dependency needed. Jina Search (`s.jina.ai`) provides web search. 10M free tokens per API key. Configurable `SearchBackend` and `ReaderBackend` traits allow adding SearXNG, Tavily, or raw reqwest as alternatives.

### Q3: Tool security architecture
**Decision**: Split `rate_limit.rs` into a `security/` module with three focused files: rate limiting, sandboxing, and authorization. Tool authorization is enforced as middleware in the pipeline's tool execution loop. Restricted tools are declared at registration time by each tool group, not hardcoded in a constant. This makes adding new restricted tools automatic when a new tool group registers them.

### Q4: ToolHandler naming
**Decision**: Rename `ToolHandler` to `Tool` and `ErasedToolHandler` to `ErasedTool` for consistency with the other core traits (`Channel`, `Provider`, `Memory`, `Observer`).

### Q5: Text-as-reply vs explicit reply tool
**Decision**: Ship with text-as-reply first (LLM text output is automatically sent to the originating channel — simpler, lower latency, matches ZeroClaw's model). If narration leakage is a problem with target models (smaller LLMs tend to append "I replied to X about Y"), switch to an explicit `reply(text)` tool where text output is private and only `reply()` sends to the channel. The architecture supports both — the switch is a one-line conditional in the pipeline's output path. Test in production and decide based on real behavior.

### Q6: Memory trait extraction
**Decision**: Extract `Memory` trait from `MemoryStore`. Current implementation becomes `SqliteMemory`. All consumers receive `Arc<dyn Memory>`. This is the seam for MemFS v2 — when the jj-backed backend is ready, it implements `Memory` and swaps in via config with zero changes to tools or pipeline.

## Out of Scope

- Self-modifying tool handlers (Aurora writing new Tool implementations in Rust) — future design doc
- Embedding/RAG for web search results
- File watching / filesystem event subscriptions
- Multi-instance coordination for tool access
- GUI/web dashboard for tool management
- Streaming command output from bash_exec
- Skills platform (ZeroClaw-style SKILL.md) — future design doc
