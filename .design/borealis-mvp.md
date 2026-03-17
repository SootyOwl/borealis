# Feature: Borealis MVP — Aurora's Bot Runtime

## Summary

Borealis is a modular, multi-channel bot runtime written in Rust that powers Aurora — a digital person with her own personality, interests, and evolving memory. It replaces the existing LettaBot + self-hosted Letta server with a fully owned stack using a hybrid traits + message bus architecture. The MVP delivers end-to-end functionality: config loading, LLM providers (Anthropic + OpenAI-compatible), CLI and Discord adapters, a SQLite-backed memory system with tool integration, directive parsing, LLM-driven context window compaction, a config-driven scheduler, and Letta data migration.

## Requirements

- REQ-1: Layered TOML configuration with environment variable overrides, validated at startup with clear error messages for missing/invalid values. Uses the `config` crate (`Config::builder()` with file + env sources, `try_deserialize()` into typed structs).
- REQ-2: Two LLM providers (Anthropic native API, OpenAI-compatible for Ollama/GPT) built with raw `reqwest`, implementing a shared `Provider` trait. Both support configurable `base_url`, retry with exponential backoff on 429/5xx (max 3 retries, 1s base delay, 2x multiplier, 30s max delay, +/- 25% jitter), and per-request timeout (default 60s). Both use non-streaming mode (`stream: false`). Each provider maps between borealis's internal types and the provider's wire format (Anthropic content blocks vs OpenAI choices array).
- REQ-3: Channel adapters (CLI for dev, Discord via `poise` 0.6.1) implementing the `Channel` trait with split inbound/outbound task methods. Three pluggable response modes (`digest`, `mention-only`, `always`) implemented as adapter-independent components via a `ResponseMode` trait. Modes are assigned per-group in config and sit between the event bus and the core pipeline, controlling when messages are dispatched for processing. Any adapter can use any mode.
- REQ-4: Tokio-based event bus with bounded `mpsc` channels (default 256). Per-conversation sequential workers dispatched via `DashMap<ConversationId, Sender<InEvent>>`, with idle eviction after 30 minutes. Global LLM concurrency limited by `tokio::sync::Semaphore` (default 4 permits).
- REQ-5: SQLite-backed memory system (notes, tags, links tables) with 8 discrete tool handlers (`memory_create`, `memory_search`, `memory_read`, `memory_update`, `memory_link`, `memory_tag`, `memory_forget`, `memory_list`) registered in a `ToolRegistry`. Core persona loaded from `memory/core.md`, always injected into system prompt.
- REQ-6: Conversation history stored in SQLite with token-budget-aware context window management. Priority: system prompt + core persona (fixed) > tool definitions (fixed) > conversation history (sliding window) > retrieved memories. Eviction operates on **turns**, not individual messages. A turn is the atomic eviction unit: a user turn is the user message; an assistant turn is the assistant response plus all tool_calls it made, their tool_results, and any follow-up assistant responses within the same tool loop. This guarantees the message sequence always satisfies provider API schema requirements (no orphaned tool_calls or tool_results). Oldest turns are evicted first.
- REQ-13: LLM-driven context window compaction. When conversation history exceeds a configurable threshold (default 75% of token budget), a background LLM call summarizes the oldest messages into a compact summary block. The summary replaces the summarized messages in prompt assembly, preserving conversational continuity without losing context. Summaries are stored in SQLite per-conversation and accumulate (a new compaction summarizes the previous summary + messages since). Compaction runs asynchronously — it does not block the current request; the current request uses simple eviction as a fallback until the summary is ready. Configurable per-channel: `compaction_threshold` (0.0-1.0, default 0.75), `compaction_model` (can use a cheaper/faster model than the main conversation model).
- REQ-7: Directive system — XML `<actions>` blocks parsed from LLM responses with variants: `NoReply`, `React`, `Voice`, `SendFile`, `Send`. Parsing strategy: strip fenced code blocks (`` ``` ... ``` ``) from the response text first, then regex-scan for `<actions>` blocks in the remaining text. Malformed XML within action blocks logs a warning and is skipped. Unsupported directives gracefully skipped with log.
- REQ-8: Config-driven scheduler with recurring and cron event types, supporting jitter, active hours filtering, overlap prevention, and silent mode (text output private, `Send` directive for channel output).
- REQ-9: Task supervision via `JoinSet` — panicked/errored adapter tasks are restarted with fresh channel pairs. Circuit breaker: max 5 restarts in 60 seconds before stopping retries.
- REQ-10: Graceful shutdown via `CancellationToken` propagated to all tasks, triggered by SIGTERM/SIGINT. Drain event bus, close connections, checkpoint SQLite WAL, force exit after 5s timeout.
- REQ-11: Security — per-user token bucket rate limiting (capacity=10, refill 1 token per 6s → 10 msg/min), global rate limit (capacity=30, refill 1 token per 2s → 30 msg/min), configurable user/guild allowlists, memory-write tool restriction to allowed users, path traversal prevention for `SendFile`.
- REQ-12: One-time Letta migration script — reads JSON files exported from Letta's REST API. Core memory blocks (markdown) to `core.md`, archival memory to note rows, optional conversation history import. Runs as a separate CLI command, not a runtime feature. No PostgreSQL dependency.

## Acceptance Criteria

- [ ] AC-1: `cargo build` succeeds on Rust 2024 edition with zero `cargo clippy` warnings and `cargo fmt --check` passing. (REQ-1, REQ-2, REQ-3)
- [ ] AC-2: Loading a valid `config/default.toml` produces a fully populated `Settings` struct; loading a config with a missing required env var produces a startup error naming the missing variable. (REQ-1)
- [ ] AC-3: OpenAI-compatible provider sends a chat request to a local Ollama instance and returns a valid `LlmResponse` with `text`, `tool_calls`, and `usage` fields. (REQ-2)
- [ ] AC-4: Anthropic provider sends a chat request to the Anthropic API and returns a valid `LlmResponse`; a simulated 429 response triggers retry with backoff and eventually returns an error message (not a panic). (REQ-2)
- [ ] AC-5: Typing a message in the CLI adapter produces an LLM response printed to stdout; `tracing` output goes to stderr. (REQ-3, REQ-4)
- [ ] AC-6: Sending 20 rapid messages from 4 simulated conversations processes them without deadlock or OOM; per-conversation ordering is preserved (verified by sequence numbers in test). (REQ-4)
- [ ] AC-7: `memory_create` inserts a row in SQLite; `memory_read` retrieves it; `memory_update` changes content; `memory_search` finds it by tag, title, and content substring; `memory_forget` sets `deleted_at` and excludes from subsequent searches. (REQ-5)
- [ ] AC-8: `memory_link` creates a bidirectional relationship; `memory_list` with tag filter returns only matching notes. (REQ-5)
- [ ] AC-9: Core persona from `memory/core.md` appears in every LLM request's system prompt section. Modifying it via `memory_update(id: "core", ...)` persists changes to disk. (REQ-5, REQ-6)
- [ ] AC-10: With `max_history_tokens` set to a low value, oldest turns are evicted first. An assistant turn containing tool_calls + tool_results + follow-up response is always evicted as a complete unit — the resulting message array always satisfies Anthropic and OpenAI API schema validation (no orphaned tool_calls or tool_results). (REQ-6)
- [ ] AC-11: A 400 "context too long" provider response triggers one retry with reduced history; a second 400 resets to system prompt + core persona + current message only. (REQ-6)
- [ ] AC-12: `<actions><react emoji="thumbsup"/></actions>` in an LLM response parses into a `Directive::React`; the same text inside a markdown code block is ignored. (REQ-7)
- [ ] AC-13: (Manual verification) Discord bot connects, receives a message in a `mention-only` channel when @mentioned, responds in the correct channel, and stays under 2000 chars (truncated with `...` if needed). Automated tests use mock adapters + in-process event flow. (REQ-3, REQ-11)
- [ ] AC-14: Discord digest mode batches messages and fires after `digest_interval_min` OR `digest_debounce_min` of silence (whichever first); an @mention bypasses the buffer for immediate processing. (REQ-3)
- [ ] AC-15: A non-allowed user's attempt to call `memory_create` via tool use returns an authorization error to the LLM (not to the user channel). (REQ-11)
- [ ] AC-16: Per-user rate limiter rejects the 11th message in 60 seconds with a log entry; the global limiter triggers at 31 messages across all users. (REQ-11)
- [ ] AC-17: A heartbeat scheduler event fires within `interval +/- jitter`; an event configured outside `active_hours` is skipped entirely (not queued). (REQ-8)
- [ ] AC-18: Silent mode scheduler event: LLM text output is not sent to any channel; a `<actions><send channel="discord" chat="general">hello</send></actions>` directive routes to the Discord adapter. (REQ-8)
- [ ] AC-19: If a heartbeat is still processing when the next one triggers, the duplicate is skipped with a log entry. (REQ-8)
- [ ] AC-20: SIGTERM during active processing triggers graceful shutdown: in-flight LLM calls complete or timeout within 5s, SQLite WAL is checkpointed, process exits cleanly. (REQ-10)
- [ ] AC-21: A panicked Discord adapter task is automatically restarted by the `JoinSet` supervisor; after 5 restarts in 60s, retries stop and a critical error is logged. (REQ-9)
- [ ] AC-22: `borealis migrate-letta --source <path>` reads JSON files exported from Letta's API, imports core memory blocks into `core.md` and archival memory entries into SQLite note rows with preserved tags. (REQ-12)
- [ ] AC-23: When conversation history exceeds 75% of the token budget, a background compaction task is spawned; subsequent requests for the same conversation include the resulting summary in place of the summarized messages. (REQ-13)
- [ ] AC-24: A compaction summary stored in SQLite survives process restart — on reload, the conversation's prompt assembly uses the stored summary plus messages after the compaction point. (REQ-13)
- [ ] AC-25: If compaction is still in progress when a new message arrives, the pipeline falls back to simple oldest-first eviction for that request (no blocking). (REQ-13)
- [ ] AC-26: Compaction can be configured to use a different (cheaper) provider/model than the main conversation model; a test with `compaction_model = "local"` uses the local Ollama provider for summarization while the conversation uses Anthropic. (REQ-13)
- [ ] AC-27: Successive compactions accumulate — the second compaction's input includes the first compaction's summary plus messages since, producing a single updated summary (not a chain of summaries). (REQ-13)

## Architecture

### Phase Structure

The MVP is organized into 3 implementation phases, each building on the last:

**Phase 1 — Foundation** (spec stages 1-2): Project scaffolding, config, event types, core loop skeleton, tracing, both LLM providers. Ends with: providers can make real API calls and return typed responses.

**Phase 2 — Core Experience** (spec stages 3-4): CLI adapter, SQLite schema, conversation history with context window management and LLM-driven compaction, core.md loading, tool execution loop, memory module with all 8 tool handlers, directive parsing, tool registry wiring. Ends with: type in CLI, get LLM responses with working memory tools, directive parsing, and automatic context compaction.

**Phase 3 — Platform & Autonomy** (spec stages 5-6): Discord adapter via poise, response modes (digest/mention-only/always), rate limiting, authorization, scheduler with jitter/active hours/overlap prevention/silent mode, Letta migration script. Ends with: full MVP deployed on Discord with autonomous scheduled behavior.

### Project Layout

```
borealis/
├── Cargo.toml
├── config/
│   ├── default.toml
│   ├── system_prompt.md
│   └── compaction_prompt.md
├── src/
│   ├── main.rs                   # Entry, config, component wiring, shutdown
│   ├── config.rs                 # Settings struct + config crate builder + validation
│   ├── core/
│   │   ├── mod.rs
│   │   ├── event.rs              # InEvent, OutEvent, Directive, ConversationId
│   │   ├── event_loop.rs         # Main event loop + conversation worker dispatch
│   │   └── pipeline.rs           # Message processing pipeline (retrieve → build prompt → LLM → tools → parse → route)
│   ├── channels/
│   │   ├── mod.rs                # Channel trait
│   │   ├── modes.rs              # ResponseMode trait + digest/mention-only/always implementations
│   │   ├── discord.rs            # poise-based Discord adapter
│   │   └── cli.rs                # stdin/stdout dev adapter
│   ├── providers/
│   │   ├── mod.rs                # Provider trait + RequestConfig + LlmResponse
│   │   ├── openai.rs             # OpenAI-compatible (also serves Ollama)
│   │   └── anthropic.rs          # Anthropic native API
│   ├── memory/
│   │   ├── mod.rs                # Memory module public API
│   │   └── store.rs              # SQLite-backed note CRUD + search
│   ├── scheduler/
│   │   ├── mod.rs
│   │   └── events.rs             # Event types, jitter, active hours, overlap
│   ├── tools/
│   │   ├── mod.rs                # ToolRegistry + ToolHandler trait + ToolDef/ToolCall/ToolResult types
│   │   └── memory_tools.rs       # 8 memory tool handlers
│   ├── history/
│   │   ├── mod.rs                # Conversation storage + context window budget + eviction
│   │   └── compaction.rs         # LLM-driven summarization of old messages
│   └── rate_limit.rs             # Token bucket rate limiter (per-user + global)
├── memory/
│   └── core.md                   # Aurora's core persona (hand-editable, always in prompt)
└── tests/
    ├── common/
    │   └── mod.rs                # Shared test utilities: mock providers, test DB setup, fixture helpers
    ├── config_test.rs
    ├── memory_test.rs
    ├── provider_test.rs
    ├── pipeline_test.rs
    ├── directive_test.rs
    └── compaction_test.rs
```

### Key Crate Decisions

| Crate | Version | Role |
|-------|---------|------|
| `tokio` | latest stable | Async runtime, signals, sync primitives |
| `tokio-util` | latest stable | `CancellationToken` for graceful shutdown |
| `config` | 0.15.x | Layered TOML + env config with `try_deserialize()` |
| `serde` + `serde_json` | latest stable | Serialization for config, tool args, provider wire formats |
| `reqwest` | latest stable | HTTP client for LLM provider APIs |
| `rusqlite` | 0.38.x | SQLite with `bundled` feature (WAL mode) |
| `poise` | 0.6.1 | Discord bot framework (wraps serenity 0.12.4) |
| `tracing` + `tracing-subscriber` | latest stable | Structured logging (JSON in prod, pretty in dev) |
| `anyhow` + `thiserror` | latest stable | Error handling (anyhow at boundaries, thiserror for module errors) |
| `dashmap` | 6.x | Concurrent conversation worker dispatch map |
| `regex` | latest stable | Directive XML parsing from LLM responses |
| `tiktoken-rs` | latest stable | Accurate token counting for OpenAI-compatible providers |
| `croner` | latest stable | Cron expression parsing for scheduler events |

### Rust 2024 Edition

Targeting Rust 2024 edition (stable since 1.85). This provides native `async fn` in traits, eliminating the `async_trait` crate dependency. The `Channel`, `Provider`, and `ToolHandler` traits all use `async fn` directly.

### Config Architecture

Replaces raw `toml` deserialization with the `config` crate's builder pattern:

```
Config::builder()
    .add_source(File::with_name("config/default"))
    .add_source(File::with_name(&format!("config/{}", run_mode)).required(false))
    .add_source(File::with_name("config/local").required(false))
    .add_source(Environment::with_prefix("BOREALIS").separator("__"))
    .build()?
    .try_deserialize::<Settings>()?
```

This gives us: base config from `config/default.toml`, optional environment-specific overrides (`config/production.toml`), local developer overrides (`config/local.toml` — gitignored), and environment variable overrides (e.g., `BOREALIS__PROVIDERS__ANTHROPIC__MODEL=claude-sonnet-4-20250514`). API keys still use the `_env` indirection pattern from the spec (`api_key_env = "ANTHROPIC_API_KEY"`) — the config crate loads the key name, then startup validation resolves and validates the actual env var.

### Pluggable Response Modes

Response modes control *when* messages get dispatched to the core pipeline. They are adapter-independent — defined in `src/channels/modes.rs` as implementations of a `ResponseMode` trait:

```
trait ResponseMode: Send + Sync {
    /// Called when a new message arrives. Returns messages to dispatch now, if any.
    async fn on_message(&self, event: InEvent) -> Vec<InEvent>;
    /// Called periodically by a timer. Returns buffered messages to dispatch, if any.
    async fn on_tick(&self) -> Vec<InEvent>;
}
```

- **`AlwaysMode`**: `on_message` returns the event immediately. `on_tick` is a no-op.
- **`MentionOnlyMode`**: `on_message` returns the event only if it contains a mention of Aurora. Otherwise drops it.
- **`DigestMode`**: `on_message` buffers the event. `on_tick` checks debounce/interval timers and returns the batch when either fires. @mentions bypass the buffer via `on_message` returning immediately.

A `ModeRouter` (per-adapter) maps group IDs to mode instances. The adapter forwards all messages to its `ModeRouter`, which delegates to the appropriate mode and dispatches results to the event bus. This keeps adapters thin and modes testable without platform dependencies.

### Token Counting

Token estimation is a method on the `Provider` trait: `fn estimate_tokens(&self, text: &str) -> usize`. Each provider uses the appropriate strategy:
- **OpenAI-compatible**: `tiktoken-rs` with cl100k_base encoding (accurate)
- **Anthropic**: `chars / 4` heuristic (no public tokenizer crate; Anthropic's tokenizer is ~100k vocab BPE, so the heuristic is within ~20%)

With compaction at 75% and 400 recovery as fallback, precision isn't critical — the estimate prevents most overflow cases, and the safety nets handle the rest.

### Discord via Poise

Poise 0.6.1 (built on serenity 0.12.4) replaces raw serenity for the Discord adapter. Benefits:
- Built-in event handler callback (`FrameworkOptions::event_handler`) for processing `FullEvent::Message` — maps directly to our inbound event flow
- `serenity::Context` available inside the handler for sending responses, reactions, etc.
- Edit tracking for free (useful for digest mode corrections)
- Slash command support if we want it later (not in MVP, but zero-cost to have available)

The `Channel` trait's `run_inbound` and `run_outbound` split still applies — poise's event handler feeds `InEvent`s to the bus, and a separate outbound task consumes `OutEvent`s from the adapter's dedicated `mpsc` receiver.

### Conversation History Schema

The `messages` table stores individual messages with turn grouping for atomic eviction:

```sql
CREATE TABLE conversations (
    id              TEXT PRIMARY KEY,    -- ConversationId serialized
    mode            TEXT NOT NULL,       -- "shared" or "pairing"
    created_at      TEXT NOT NULL,
    last_active_at  TEXT NOT NULL
);

CREATE TABLE messages (
    id              TEXT PRIMARY KEY,    -- UUID or monotonic ID
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    turn_id         TEXT NOT NULL,       -- groups messages in the same turn
    role            TEXT NOT NULL,       -- "user", "assistant", "tool"
    content         TEXT NOT NULL,
    tool_call_id    TEXT,               -- for tool result messages
    tool_calls      TEXT,               -- JSON array of tool calls (for assistant messages)
    token_estimate  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    INDEX idx_messages_conv_turn (conversation_id, turn_id, created_at)
);
```

**Turn ID assignment**: A new `turn_id` is generated for each user message. The assistant's response, any tool_calls it makes, their tool_results, and follow-up assistant responses all share the same `turn_id`. This allows turn-based eviction via `DELETE FROM messages WHERE turn_id = ?` and turn-based token counting via `SUM(token_estimate) WHERE turn_id = ?`.

### Context Window Compaction

When a conversation's history approaches the token budget, borealis summarizes older messages via a background LLM call rather than silently dropping them. This preserves conversational continuity — Aurora remembers what was discussed, not just the most recent messages.

**Trigger**: During prompt assembly in `src/core/pipeline.rs`, if the conversation history's estimated token count exceeds `compaction_threshold` (default 75%) of the available history budget, a compaction task is spawned. The current request proceeds using simple eviction as a fallback; subsequent requests use the summary once it's ready.

**Flow**:
1. `pipeline.rs` detects threshold exceeded → spawns `tokio::spawn` compaction task
2. `src/history/compaction.rs` selects messages to summarize: everything from the conversation start (or last compaction point) up to the midpoint of the current history
3. Compaction builds a summarization prompt: previous summary (if any) + selected messages → "Summarize this conversation so far, preserving key facts, decisions, emotional context, and any commitments made"
4. Calls the configured compaction provider (can be a cheaper model, e.g., local Ollama) via the `Provider` trait
5. Stores the result in `conversation_summaries` table with `conversation_id`, `summary_text`, `compacted_up_to` (message ID/timestamp marking the boundary), `token_estimate`, `created_at`
6. Marks a `compaction_ready` flag (atomic bool per conversation, stored in the worker's state)

**Prompt assembly with summary**: When a summary exists, the history section of the prompt becomes:
```
[System prompt + core persona]
[Tool definitions]
[Summary: "Here is a summary of the earlier conversation: {summary_text}"]
[Recent messages after compaction point — sliding window as before]
[Retrieved memories]
```

**Accumulation**: When a second compaction triggers, the input to the summarization prompt includes the previous summary text plus all messages since the last compaction point. The output replaces the previous summary — there is always at most one active summary per conversation.

**SQLite schema addition** (in `src/history/compaction.rs`):
```sql
CREATE TABLE conversation_summaries (
    conversation_id TEXT PRIMARY KEY,
    summary_text    TEXT NOT NULL,
    compacted_up_to TEXT NOT NULL,      -- message ID of the last summarized message
    token_estimate  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,       -- ISO 8601
    updated_at      TEXT NOT NULL
);
```

**Config** (in `config/default.toml`):
```toml
[bot.compaction]
enabled = true
threshold = 0.75                        # fraction of history budget
compaction_model = "default"            # "default" uses the conversation's provider, or specify e.g. "local"
summary_prompt_path = "config/compaction_prompt.md"  # customizable summarization prompt
```

**Default compaction prompt** (`config/compaction_prompt.md`):
```
Summarize the following conversation, preserving:
- Key facts and information shared by participants
- Decisions made and commitments given
- Emotional context and relationship dynamics
- Any unresolved questions or ongoing topics
- Names and who said what when it matters

Be concise but do not lose important details. Write in third person narrative form.
If a previous summary is provided, integrate it with the new messages into a single cohesive summary.
```

**Error handling**: If the compaction LLM call fails (timeout, provider error), the failure is logged and the conversation continues with simple eviction. Compaction is retried on the next threshold crossing. No user-visible impact.

### Deployment

Target: Linux (Docker container or bare VPS). The binary should:
- Use relative paths by default (`database_path = "memory/borealis.db"`) but accept absolute paths
- Log to stdout/stderr (compatible with Docker log drivers and systemd journal)
- Respect `RUST_LOG` for tracing filter configuration
- Ship with a minimal `Dockerfile` (multi-stage build, `debian:slim` runtime base for rusqlite's bundled SQLite)
- No musl/static linking required — `debian:slim` provides glibc

## Resolved Questions

### Q1: Response modes are pluggable, adapter-independent
Response modes (`digest`, `mention-only`, `always`) are defined as separate, pluggable components — not baked into individual adapters. Each mode implements a `ResponseMode` trait that controls message buffering/filtering. Adapters forward all messages as `InEvent`s immediately; a per-group mode handler sits between the event bus and the core pipeline, deciding when to dispatch. This allows future adapters to reuse existing modes and new modes to be added without modifying adapter or core internals.

### Q2: Provider-aware token counting with tiktoken-rs
Use `tiktoken-rs` for OpenAI-compatible providers (accurate cl100k_base counting). Use `chars / 4` heuristic for Anthropic (no public tokenizer crate). Token counting is a method on the `Provider` trait (`fn estimate_tokens(&self, text: &str) -> usize`) so each provider uses the appropriate strategy. With compaction and 400 recovery as safety nets, exact counts are not critical — the estimate just needs to be "close enough" to avoid unnecessary eviction or avoidable 400s.

### Q3: Letta migration reads JSON export
The migration script expects JSON files exported from Letta's REST API before shutdown. No PostgreSQL dependency needed. Expected input: JSON files containing core memory blocks (markdown content), archival memory entries, and optionally conversation history. The migration script parses these and writes to `core.md` + SQLite note rows.

## Out of Scope

- Embedding-based semantic search (post-MVP Enhancement B)
- FTS5 full-text search (post-MVP Enhancement A)
- Streaming LLM responses (post-MVP Enhancement D)
- Voice transcription/TTS (post-MVP Enhancement E)
- Telegram/Bluesky adapters (post-MVP Enhancement C and beyond)
- Slash commands in Discord (poise supports them, but MVP uses message-based interaction only)
- Web dashboard or admin UI
- Multi-instance / distributed deployment
- Automated testing against live Discord (integration tests use mock providers and in-process event flow)
