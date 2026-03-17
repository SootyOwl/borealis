# Borealis Design Spec

> **Rust edition:** 2024 (stable since 1.85) — native `async fn` in traits, no `async_trait` dependency.
> **Discord:** `poise` 0.6.1 (built on `serenity` 0.12.4) for simplified event handling and future slash command support.
> **Config:** `config` crate 0.15.x for layered TOML + environment variable config with `try_deserialize()`.
> **Deployment:** Linux targets — Docker container (`debian:slim` runtime) or bare VPS. Relative paths by default, logs to stdout/stderr.

## Context

Borealis is a custom multi-channel bot harness/runtime written in Rust, built to power **Aurora** — a digital person, and chill friend, not an AI assistant. She has her own personality, interests, and evolving memory. Aurora interacts with multiple users on a small-medium Discord server and will expand to other platforms (Bluesky planned). The architecture should support future self-extension (Aurora adding her own capabilities).

Borealis replaces LettaBot + self-hosted Letta server, which required constant patching for bugs, missing features, and bad code. The goal is to own the entire stack with a modular, extensible architecture that does exactly what's needed — nothing more, nothing less.

---

## Architecture: Hybrid Traits + Message Bus

Traits define **what** components can do. Tokio channels define **how** they communicate at runtime.

```
┌─────────────────────────────────────────────────────┐
│                    borealis binary                   │
│                                                      │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐          │
│  │ Discord  │  │  Future  │  │   CLI    │  ...      │
│  │ Adapter  │  │ Adapters │  │ Adapter  │          │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘          │
│       └──────────────┼──────────────┘                │
│                      ▼                               │
│              ┌──────────────┐                        │
│              │  Event Bus   │ (tokio mpsc channels)  │
│              └──────┬───────┘                        │
│                     ▼                                │
│              ┌──────────────┐                        │
│              │  Core Loop   │  ("corealis")          │
│              │              │                        │
│              │  1. Receive  │                        │
│              │  2. Retrieve │◄──── Memory Module     │
│              │  3. LLM Call │◄──── Provider Module   │
│              │  4. Tools    │                        │
│              │  5. Parse    │                        │
│              │  6. Route    │                        │
│              └──────┬───────┘                        │
│         ┌───────────┼───────────┐                    │
│         ▼           ▼           ▼                    │
│    ┌─────────┐ ┌──────────┐ ┌───────────┐          │
│    │Scheduler│ │ Memory   │ │ Directive │          │
│    │ Module  │ │ Module   │ │ Handler   │          │
│    └─────────┘ └──────────┘ └───────────┘          │
└─────────────────────────────────────────────────────┘
```

### Concurrency Model

- The event bus uses **bounded** `tokio::mpsc` channels (backpressure via configurable buffer size, default 256)
- Each channel adapter runs as its own `tokio::spawn`ed task, producing `InEvent`s
- **Per-conversation sequential workers:** each `ConversationId` gets a dedicated `mpsc` channel (created on first message, stored in `DashMap<ConversationId, Sender<InEvent>>`). A worker task per conversation consumes events sequentially, guaranteeing message ordering within a conversation. Workers are spawned lazily and cleaned up after idle timeout (see eviction below).
- **LLM concurrency:** a global `tokio::sync::Semaphore` (configurable, default 4) limits concurrent LLM requests. The semaphore is acquired *inside* the per-conversation worker, just before the LLM call — never before the conversation dispatch. This prevents N tasks for the same conversation from consuming all permits.
- `ConversationId` enum: `DM { channel_type, user_id }` | `Group { channel_type, group_id }` | `System { event_name }` — used for routing, history storage, and worker dispatch
- **Worker map eviction:** conversation workers that have been idle for >30 minutes are cleaned up (sender dropped, worker task exits naturally). A periodic sweep (every 5 minutes) handles this. Uses `DashMap::entry` API so eviction and new-message dispatch are atomic (no race between sweep removing a worker and a new message creating one). Prevents unbounded memory growth from historic conversations.
- **Outbound routing:** the core loop holds a `HashMap<ChannelSource, Sender<OutEvent>>` — each adapter gets its own dedicated outbound `mpsc` channel. On startup, `main.rs` creates a channel pair per adapter and wires senders into the core loop's router, receivers into the adapters.

### Task Supervision

- Channel adapters and the core loop are managed via `tokio::task::JoinSet`
- If a task exits (panic or error), the supervisor logs and restarts it with a fresh channel pair, updating the outbound router
- No `catch_unwind` — `JoinSet` handles panic detection naturally
- If restarts exceed 5 in 60 seconds, log a critical error and stop retrying (avoid crash loops)

### Core Loop Pipeline

For each incoming event:

1. **Receive** — `InEvent` from any channel adapter or scheduler
2. **Acquire conversation lock** — serialize processing for the same conversation
3. **Retrieve** — query memory for relevant context (core block always included, semantic search for relevant notes)
4. **Build prompt** — system prompt (with core memory) + retrieved memories + conversation history + user message
5. **LLM Call** — send to configured provider, get response with optional tool calls
6. **Tool execution loop** — if the LLM requests tool calls, execute and feed results back (max 10 rounds, 120s total timeout)
7. **Parse directives** — scan response text for `<actions>` XML tags, extract directives
8. **Route** — send text response + directives back to the originating channel via `OutEvent`

---

## Conversation History

### Conversation Mode

- **shared** (default for groups) — one conversation per group, all users' messages in the same history. Aurora sees the full group context.
- **pairing** (default for DMs) — one conversation per user-channel pair. Each user has their own history with Aurora.

### Storage

- Stored in SQLite (`conversations` and `messages` tables), not in markdown
- Per-conversation, keyed by `ConversationId` (same enum used in concurrency model)
- Each message stored with: role, content, timestamp, token count estimate, `turn_id` (groups messages in the same turn for atomic eviction)

**Schema:**
```sql
CREATE TABLE conversations (
    id              TEXT PRIMARY KEY,    -- ConversationId serialized
    mode            TEXT NOT NULL,       -- "shared" or "pairing"
    created_at      TEXT NOT NULL,
    last_active_at  TEXT NOT NULL
);

CREATE TABLE messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    turn_id         TEXT NOT NULL,       -- groups messages in the same turn
    role            TEXT NOT NULL,       -- "user", "assistant", "tool"
    content         TEXT NOT NULL,
    tool_call_id    TEXT,               -- for tool result messages
    tool_calls      TEXT,               -- JSON array of tool calls (for assistant messages)
    token_estimate  INTEGER NOT NULL,
    created_at      TEXT NOT NULL
);
CREATE INDEX idx_messages_conv_turn ON messages(conversation_id, turn_id, created_at);
```

**Turn ID assignment**: new `turn_id` per user message; assistant response + tool calls/results in the same loop share the `turn_id`. Enables atomic eviction via `DELETE WHERE turn_id = ?`.
- **Token counting:** use a simple heuristic (chars / 4) for context budget estimation. Not exact, but fast and good enough for sliding window decisions. Each provider's `LlmResponse` returns actual `TokenUsage` from the API for logging/tracking.
- **SQLite access:** `rusqlite::Connection` is `Send` but not `Sync`, and its operations are blocking I/O that would starve the tokio executor. All DB operations go through `tokio::task::spawn_blocking`. Single `Arc<Mutex<rusqlite::Connection>>` shared across the application — no connection pool needed for MVP (LLM calls are the bottleneck, not DB ops). WAL mode enabled at initialization. Busy timeout set to 5s. Upgrade path: swap to `deadpool-sqlite` if concurrent read contention ever becomes measurable.

### Context Window Management

- Budget system: total context = model max tokens − reserve for response
- Priority allocation: (1) system prompt + core memory (fixed), (2) tool definitions (fixed), (3) conversation history (sliding window, most recent first), (4) retrieved memories (ranked by relevance, fill remaining space)
- If history exceeds budget, oldest **turns** are evicted first. A turn is the atomic eviction unit:
  - **User turn:** the user message
  - **Assistant turn:** the assistant response + all `tool_calls` it made + their `tool_results` + any follow-up assistant responses within the same tool loop
  - **System turn:** system/injected messages (never evicted)
  This guarantees the message sequence always satisfies provider API schema requirements — no orphaned `tool_calls` or `tool_results`, no need for special-case pairing logic.
- **400 recovery:** if the provider returns a 400 (context too large despite budget estimation), drop the oldest non-fixed messages and retry once. If it fails again, reset to system prompt + core persona + current message only.
- Configurable `max_history_messages` and `max_history_tokens` per-channel in config

### Context Compaction (LLM-Driven Summarization)

When conversation history exceeds a configurable threshold (default 75% of the history token budget), a background LLM call summarizes the oldest messages into a compact summary. This preserves conversational continuity — Aurora remembers what was discussed rather than silently losing it.

**Trigger:** During prompt assembly, if history tokens exceed `compaction_threshold × history_budget`, a `tokio::spawn`ed compaction task runs. The current request falls back to simple eviction; subsequent requests use the summary.

**Summarization scope:** Messages from conversation start (or last compaction point) up to the midpoint of current history. The summarization prompt includes the previous summary (if any) + selected messages → produces a single updated summary.

**Storage:** `conversation_summaries` table in SQLite:
```sql
CREATE TABLE conversation_summaries (
    conversation_id TEXT PRIMARY KEY,
    summary_text    TEXT NOT NULL,
    compacted_up_to TEXT NOT NULL,      -- message ID of last summarized message
    token_estimate  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
```

**Prompt assembly with summary:** Summary is injected as a labeled block between tool definitions and recent messages:
```
[System prompt + core persona] → [Tools] → [Summary of earlier conversation] → [Recent messages] → [Memories]
```

**Accumulation:** Each compaction replaces the previous summary — there is always at most one active summary per conversation. The second compaction's input is: previous summary + messages since last compaction → single new summary.

**Config:**
```toml
[bot.compaction]
enabled = true
threshold = 0.75
compaction_model = "default"                         # or e.g. "local" for cheaper/faster
summary_prompt_path = "config/compaction_prompt.md"
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

**Error handling:** If the compaction LLM call fails, log and continue with simple eviction. Retry on next threshold crossing. No user-visible impact.

### Retention

- Messages retained for configurable duration (default 30 days)
- Periodic cleanup on startup and via scheduler event

---

## Memory System ("Second Brain")

A standalone, loosely-coupled module. Usable independently of the rest of borealis.

### Storage

```
memory/
├── core.md          # Aurora's core persona — hand-editable, always in prompt
└── borealis.db      # SQLite: notes, links, tags, conversation history
```

**`core.md`** is Aurora's core persona — always injected into the prompt alongside the system prompt. Aurora can modify it via `memory(command: "update", id: "core", ...)` to evolve her personality over time. If `core.md` grows too large (exceeds configurable token limit, default 2000 tokens), the memory tool warns Aurora to trim it. This is the one file that stays as markdown because you'll want to hand-edit it directly.

**Notes** are stored in SQLite (same database file as conversation history, but the memory module is standalone — it takes an `Arc<Mutex<Connection>>` and only knows about its own tables). Schema:

```sql
CREATE TABLE notes (
    id          TEXT PRIMARY KEY,   -- "note_a1b2c3d4"
    title       TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL,      -- ISO 8601
    updated_at  TEXT NOT NULL,
    deleted_at  TEXT                -- soft delete (NULL = active)
);

CREATE TABLE tags (
    note_id     TEXT NOT NULL REFERENCES notes(id),
    tag         TEXT NOT NULL,
    PRIMARY KEY (note_id, tag)
);

CREATE TABLE links (
    from_id     TEXT NOT NULL REFERENCES notes(id),
    to_id       TEXT NOT NULL REFERENCES notes(id),
    relation    TEXT NOT NULL,      -- "related_to", "contradicts", etc.
    PRIMARY KEY (from_id, to_id)
);
```

### MVP Search

SQL queries over the notes table:
- By tag (`JOIN tags WHERE tag IN (...)`)
- By title (`LIKE '%query%'`)
- By content (`LIKE '%query%'`)
- By links (`JOIN links` traversal)

No embeddings or vector search in MVP. Simple but correct — and the natural upgrade path to FTS5 is just adding an index to the same table.

### Concurrency

No special locking needed — SQLite WAL mode handles concurrent reads, and writes are serialized by SQLite itself. All DB operations go through `spawn_blocking` (same as conversation history). The memory module receives its `Arc<Mutex<Connection>>` from `main.rs` — it does not create or manage the connection.

### Tool Interface

Separate tools per operation — explicit tool names are easier for LLMs to select accurately than subcommand dispatch.

**Note IDs:** generated as `note_{8-char-hex}` (e.g., `note_a1b2c3d4`) using a random u32. Collision check on create; regenerate if exists. `core` is a reserved ID for `core.md`.

- `memory_create(title, content, tags)` — create a new note
- `memory_search(query, limit)` — searches across title, tags, and content
- `memory_read(id)` — read a note by ID
- `memory_update(id, content)` — update note content (also used for `core.md` via `id: "core"`)
- `memory_link(from, to, relation)` — link two notes
- `memory_tag(id, tags)` — update tags on a note
- `memory_forget(id)` — soft delete (sets `deleted_at`, excluded from search)
- `memory_list(filter)` — list notes, optionally filtered by tag

### Export / Import

`borealis export` — dumps all notes to markdown files with YAML frontmatter (for human inspection, git snapshots, or backup). `borealis import` — loads markdown files back into SQLite. These are CLI commands, not runtime features.

### Future: Advanced Retrieval (Post-MVP)

Planned enhancements once the bot is working end-to-end:

1. **FTS5 keyword search** — add FTS5 virtual table over `notes.content` + `notes.title` for BM25 ranking. Trivial since data is already in SQLite.
2. **Embeddings** — `trait Embedder: Send + Sync { async fn embed(&self, text: &str) -> Result<Vec<f32>>; }` via OpenAI-compatible `/v1/embeddings` endpoint (Ollama). sqlite-vec for storage (note: C FFI extension, requires unsafe loading).
3. **Semantic chunking** — ~400 tokens, sentence boundaries, each chunk embedded separately
4. **Hybrid search + RRF** — BM25 + cosine similarity + link traversal, ranked with Reciprocal Rank Fusion

### Letta Migration

- One-time migration script (not an architectural component)
- Core memory blocks → `core.md` + note rows in SQLite
- Archival memory → note rows (embeddings deferred to post-MVP)
- Conversation history → optionally imported into conversation tables
- Run separately before cutover, verify personality consistency

---

## Channel Adapters

### Channel Trait

```rust
// Rust 2024 edition — native async fn in traits, no #[async_trait] needed
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;

    /// Listen for inbound messages, send InEvents to the core.
    async fn run_inbound(self: Arc<Self>, tx: Sender<InEvent>) -> Result<()>;

    /// Consume outbound events and dispatch to the platform.
    async fn run_outbound(self: Arc<Self>, rx: Receiver<OutEvent>) -> Result<()>;

    fn supported_directives(&self) -> Vec<DirectiveKind>;
}
```

Split into two methods so inbound and outbound run as separate tasks. If `run_outbound` panics, the supervisor can restart it with a new `Receiver` without affecting inbound listening.

### Event Types

```rust
pub struct InEvent {
    pub source: ChannelSource,      // discord/telegram/tui/scheduler
    pub message: Message,           // text, author, timestamp, metadata
    pub context: MessageContext,    // group/DM, channel ID, reply-to
}

pub struct OutEvent {
    pub target: ChannelSource,
    pub text: Option<String>,
    pub directives: Vec<Directive>,
    pub reply_to: Option<MessageId>,
}
```

### Directives

**Reasoning**: Reduce the number of LLM requests by handling straightforward actions within the application. Tool calls are expensive (require separate request, returning result to LLM, etc.) whereas directives are fast and cheap.

Defined at core level, each channel adapter handles them:

```rust
pub enum Directive {
    NoReply,
    React { emoji: String, message_id: Option<MessageId> },
    Voice { text: String },
    SendFile { path: PathBuf, kind: FileKind },
    Send { channel: String, chat: String, text: String },  // for silent mode
}
```

Unsupported directives are gracefully skipped with a log.

**Directive parsing:** Two-phase approach: (1) strip fenced code blocks (`` ``` ... ``` ``) from the response text, (2) regex-scan the remaining text for `<actions>...</actions>` blocks. Malformed XML within action blocks logs a warning and is skipped.

### Response Length Control

Response length is controlled via `max_tokens` in the LLM request, configured per-channel (e.g., Discord default ~500 tokens to stay well under 2000 chars). If a response still exceeds the platform limit, it is truncated with a `…` suffix rather than attempting complex message splitting.

### MVP Adapters

- **CLI** — simple stdin/stdout line-based chat for dev/testing. Reads a line, sends as `InEvent`, prints the response. No special terminal handling needed — `tracing` output goes to stderr, chat goes to stdout.
- **Discord** — `poise` 0.6.1 (wraps `serenity` 0.12.4). Requires MESSAGE_CONTENT privileged gateway intent (configured in Discord Developer Portal) to read message content in guilds. Per-group config with pluggable response modes (adapter-independent, see Response Modes below):
  - **`digest`** — collects messages into a buffer and processes them as a batch. Fires when: (a) `digest_interval_min` elapses since last digest, OR (b) `digest_debounce_min` elapses since last message with no new messages arriving. Whichever triggers first. @mentions of Aurora bypass the buffer and trigger immediate processing. On restart, the buffer starts empty (unprocessed messages from before restart are lost — acceptable for MVP). The digest prompt includes all buffered messages with timestamps and authors.
  - **`mention-only`** — Aurora only responds when directly mentioned (@Aurora)
  - **`always`** — Aurora participates in all messages (for small/personal channels)
  - Default mode configurable via `"*"` wildcard group

### Pluggable Response Modes

Response modes are adapter-independent components defined in `src/channels/modes.rs`. Each mode implements a `ResponseMode` trait controlling when messages are dispatched to the core pipeline:

- **`AlwaysMode`** — dispatches immediately
- **`MentionOnlyMode`** — dispatches only if Aurora is mentioned, drops otherwise
- **`DigestMode`** — buffers messages, dispatches on interval/debounce timer; @mentions bypass buffer

A `ModeRouter` per adapter maps group IDs to mode instances. Adapters forward all messages to the router, keeping adapter logic thin and modes reusable across any future adapter.

### Token Counting

Token estimation is a method on the `Provider` trait: `fn estimate_tokens(&self, text: &str) -> usize`. Strategy per provider:
- **OpenAI-compatible:** `tiktoken-rs` with cl100k_base encoding (accurate)
- **Anthropic:** `chars / 4` heuristic (no public tokenizer; ~20% accuracy, sufficient with compaction + 400 recovery as safety nets)

---

## LLM Providers

### Provider Trait

```rust
// Rust 2024 edition — native async fn in traits, no #[async_trait] needed
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    async fn chat(
        &self,
        messages: Vec<ChatMessage>,
        tools: &[ToolDef],
        config: &RequestConfig,
    ) -> Result<LlmResponse>;
}

pub struct RequestConfig {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stop_sequences: Vec<String>,
}

pub struct LlmResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
}
```

Both providers built using raw `reqwest` against their respective REST APIs (no official Anthropic Rust SDK exists; rolling our own is good Rust practice).

Both support configurable `base_url` for compatible/proxy APIs.

**Provider-specific mapping:** Each provider implementation handles the translation between borealis's internal types (`ChatMessage`, `ToolCall`, `ToolDef`) and the provider's wire format. Anthropic and OpenAI differ in: system prompt placement, tool call/result format, content block types, and token counting field names. This mapping lives inside each provider implementation, not in the trait.

**Note:** Streaming support is not in the MVP trait. When we add it, it will be an optional `chat_stream` method with a default no-op implementation, avoiding breakage of existing implementations.

### Tool Execution Model

```rust
/// Describes a tool that the LLM can call.
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,  // JSON Schema
}

/// A tool call from the LLM.
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Result of executing a tool (matches Anthropic/OpenAI wire format).
pub struct ToolResult {
    pub call_id: String,
    pub content: serde_json::Value,
    pub is_error: bool,
}

/// Context passed to tool handlers for authorization and routing.
pub struct ToolContext {
    pub author: Author,           // who triggered this (for auth checks)
    pub conversation_id: ConversationId,
    pub channel_source: ChannelSource,
}

/// Registry that maps tool names to handler functions.
// Rust 2024 edition — native async fn in traits, no #[async_trait] needed
pub trait ToolHandler: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDef;
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult;
}
```

- Tools are registered in a `ToolRegistry` (`HashMap<String, Box<dyn ToolHandler>>`) at startup
- The core loop sends all registered `ToolDef`s to the provider with each request
- When the LLM returns `tool_calls`, the core loop dispatches each to the matching handler
- Results are fed back as `ToolResult`s in the next LLM request
- **Parallel tool calls:** if the LLM returns multiple tool calls in one response, they execute concurrently (via `join_all`)
- **Timeout:** 120s wall-clock for the entire tool loop; 30s per individual tool call
- **Max rounds:** 10 round-trips before force-stopping
- On tool error: the error message is fed back to the LLM (not the user) so it can recover

### System Prompt Construction

The system prompt is assembled from:

1. **System prompt** — loaded from a configurable template file (e.g., `config/system_prompt.md`). Human-authored, Aurora cannot modify. Contains behavioral guidelines, response format rules, directive syntax, tool usage instructions.
2. **Core persona** — contents of `memory/core.md`, injected as a labeled section. Aurora's self-authored identity: personality traits, key facts about herself, evolving self-knowledge. Aurora can modify this via the memory tool to grow and change over time.
3. **Channel context** — metadata about the current channel, chat type (DM/group), available directives
4. **Tool descriptions** — passed separately via the provider's tool parameter (not in the message array)

Token budget validation at startup: if system prompt + core persona exceeds 50% of the model's context window, log a warning.

### Error Handling for Providers

- Retry with exponential backoff on 429 (rate limit) and 5xx errors (max 3 retries, 1s base delay, 2x multiplier, 30s max delay, +/- 25% jitter)
- Timeout per request (configurable, default 60s)
- On persistent failure: return a user-friendly error message to the channel ("I'm having trouble thinking right now, try again in a moment")
- Token usage tracked per-request and logged via `tracing`

---

## Scheduler

Config-driven event system. General-purpose — heartbeats are one event type.

```toml
[scheduler]
timezone = "Europe/London"

[[scheduler.events]]
name = "heartbeat"
type = "recurring"
interval = "30m"
jitter = "5m"
active_hours = "06:00-23:00"
prompt = """
TRIGGER: Scheduled heartbeat
TIME: {time} ({timezone})
NEXT HEARTBEAT: in {interval}

No one messaged you. The system woke you on schedule.
YOUR TEXT OUTPUT IS PRIVATE - only you can see it.

This is your time. You can:
- Reflect on recent conversations and update your memory
- Research something that interests you
- Continue multi-step work from previous heartbeats
"""

[[scheduler.events]]
name = "daily_reflection"
type = "cron"
schedule = "0 22 * * *"
jitter = "15m"
prompt = """
TRIGGER: Daily reflection
TIME: {time}

Review today's conversations. What's worth remembering?
Update your memory with anything important.
"""
```

Template variables (`{time}`, `{timezone}`, `{interval}`) substituted at runtime.

### Scheduler Behavior

- **Jitter:** uniform random within ± jitter range
- **Active hours:** events outside the window are skipped entirely (not queued)
- **Overlap prevention:** if a previous event of the same name is still processing, the new one is skipped with a log
- **First run / restart:** recurring events fire after one full interval from startup (not immediately). Cron events fire at their next scheduled time. No persistent scheduler state needed.
- **Interval base:** from last fire time during the current process lifetime. On restart, resets to "one interval from now."

### Silent Mode

Scheduler-triggered events run in silent mode:
- Aurora's text output is private (not sent to any channel)
- To reach the user, she uses directives: `<actions><send channel="discord" chat="...">message</send></actions>`
- System prompt tells her the rules

---

## Error Handling & Resilience

### General Strategy

- Use `anyhow::Result` at application boundaries, `thiserror` enums for library-like modules (memory, providers)
- Task supervision via `JoinSet` (see Task Supervision section above) — a panic in one adapter does not kill others
- Structured logging via `tracing` with JSON output in production, pretty-print in dev

### Per-Component

| Component | Failure | Response |
|---|---|---|
| LLM provider | 429/5xx/timeout | Retry with backoff (3x), then error message to user |
| LLM provider | Malformed response | Log, return error message to user |
| Tool call loop | Exceeds 10 rounds or 120s | Force-stop loop, return partial response |
| Channel adapter | Disconnect | Log, attempt reconnect with backoff |
| SQLite | Write failure | Log error, surface to user if it was a memory op |
| core.md file I/O | Permission/disk error | Return error to caller, do not silently drop |

### Graceful Shutdown

- `tokio::signal` handler for SIGTERM/SIGINT
- `CancellationToken` propagated to all tasks
- On shutdown: drain event bus, close channel connections, checkpoint SQLite WAL
- Timeout on shutdown (5s) — force exit if tasks don't terminate

---

## Security

### Rate Limiting

- Per-user token bucket rate limiter in channel adapters (configurable, default: 10 messages/minute)
- Applied before events hit the event bus — prevents a single user from monopolizing LLM slots
- Global rate limit as backstop (configurable, default: 30 messages/minute across all users)

### Authorization

- Configurable per-channel allowlists in config:
  ```toml
  [channels.discord]
  allowed_users = ["tyto"]           # Discord usernames, empty = allow all
  allowed_guilds = ["123456789"]     # Server IDs, empty = allow all
  ```
- Memory-mutating tool calls (create, update, forget, link) restricted to allowed users
- Read-only operations (search, read, list) available to all users who can message the bot

### Path Safety

- `SendFile` directive restricts paths to a configured `allowed_paths` list (default: working directory only)
- No path traversal — paths are canonicalized and checked against allowlist

### Config Validation

- All config validated at startup with clear error messages
- Missing required env vars → startup failure with descriptive error
- `default_provider` must reference a defined provider
- Enabled channels must have their token env vars set

---

## Project Structure

```
borealis/
├── Cargo.toml
├── config/
│   ├── default.toml
│   ├── system_prompt.md          # Human-authored system prompt (Aurora can't modify)
│   └── compaction_prompt.md      # Default summarization prompt for context compaction
├── src/
│   ├── main.rs                   # Entry, config, component wiring, shutdown
│   ├── core/
│   │   ├── mod.rs
│   │   ├── event.rs              # InEvent, OutEvent, Directive
│   │   ├── event_loop.rs         # Main event loop + conversation locking
│   │   └── pipeline.rs           # Message processing pipeline
│   ├── channels/
│   │   ├── mod.rs                # Channel trait
│   │   ├── modes.rs              # ResponseMode trait + digest/mention-only/always
│   │   ├── discord.rs
│   │   └── cli.rs
│   ├── providers/
│   │   ├── mod.rs                # Provider trait
│   │   ├── openai.rs
│   │   └── anthropic.rs
│   ├── memory/
│   │   ├── mod.rs                # Memory module public API
│   │   └── store.rs              # SQLite-backed note CRUD + search
│   ├── scheduler/
│   │   ├── mod.rs
│   │   └── events.rs
│   ├── tools/
│   │   ├── mod.rs                # Tool registry
│   │   └── memory_tools.rs       # memory_create, memory_search, etc.
│   ├── history/
│   │   ├── mod.rs                # Conversation history storage + context window mgmt
│   │   └── compaction.rs         # LLM-driven summarization of old messages
│   └── config.rs                 # Config types + TOML deserialization + validation
├── memory/                       # Runtime data (Aurora's brain)
│   └── core.md
└── tests/                        # Integration tests
    └── common/
        └── mod.rs                # Shared test utilities: mock providers, test DB, fixtures
```

## Configuration

Config loaded via the `config` crate with layered sources (lowest to highest priority):
1. `config/default.toml` — base config (committed)
2. `config/{RUN_MODE}.toml` — environment-specific overrides (e.g., `config/production.toml`)
3. `config/local.toml` — developer overrides (gitignored)
4. Environment variables prefixed `BOREALIS__` (e.g., `BOREALIS__PROVIDERS__ANTHROPIC__MODEL`)

API keys use the `_env` indirection pattern: config specifies the env var name (e.g., `api_key_env = "ANTHROPIC_API_KEY"`), startup validation resolves and checks the actual env var.

```toml
[bot]
name = "Aurora"
system_prompt_path = "config/system_prompt.md"
core_persona_path = "memory/core.md"
database_path = "memory/borealis.db"
max_tool_rounds = 10
tool_timeout_secs = 120

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://api.anthropic.com"
model = "claude-sonnet-4-20250514"
timeout_secs = 60
max_retries = 3

[providers.openai]
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
timeout_secs = 60
max_retries = 3

[providers.local]
base_url = "http://localhost:11434/v1"
model = "llama3"

[bot.routing]
default_provider = "anthropic"
embedding_provider = "local"

[channels.discord]
enabled = true
token_env = "DISCORD_TOKEN"
allowed_users = []
allowed_guilds = []
max_history_messages = 50
dm_policy = "pairing"                    # how DMs are handled

[channels.discord.groups]
# Default for all groups not explicitly configured
"*" = { mode = "digest", digest_interval_min = 360, digest_debounce_min = 15 }
# Aurora's own channel — faster response
"1428561552696676473" = { mode = "digest", digest_interval_min = 2, digest_debounce_min = 1 }
# Read-only / crisis channels — only respond when mentioned
"1249210986716594250" = { mode = "mention-only" }
"1249210707015241820" = { mode = "mention-only" }

[channels.cli]
enabled = true
```

---

## Key Design Decisions

1. **Adapter pattern** (Ports & Adapters) — core logic is channel/provider agnostic
2. **Traits for interfaces, message bus for communication** — clean boundaries + async concurrency
3. **SQLite for notes, markdown for core persona** — notes in SQLite (simple CRUD, natural FTS5 path), core persona as hand-editable `core.md`. Export/import commands for human-readable snapshots.
4. **Separate tools per operation** — `memory_create`, `memory_search`, etc. — explicit names are easier for LLMs than subcommand dispatch
5. **Hybrid directives + tools** — XML directives for cheap inline actions, proper tool calls for complex ops
6. **Config-driven scheduler** — events, prompts, timing all in TOML
7. **Loosely coupled memory module** — standalone Rust module that takes an `Arc<Mutex<Connection>>`, owns only its tables (notes/tags/links), knows nothing about conversation history. Same DB file, decoupled at the module boundary.
8. **Raw reqwest for providers** — no official Anthropic Rust SDK; both providers are HTTP clients
9. **MVP memory uses SQLite LIKE queries** — no FTS5 or embeddings yet; simple but correct. FTS5 is a trivial upgrade (same DB, just add a virtual table).
10. **Per-event task spawning with conversation locking** — concurrent processing without races
11. **Turn-based eviction** — eviction operates on turns (user message or assistant response + tool loop), not individual messages. Makes provider API schema compliance the default rather than a special case.
12. **Pluggable response modes** — digest/mention-only/always are adapter-independent components implementing a `ResponseMode` trait, not baked into adapters. Any adapter can use any mode; new modes don't require adapter changes.
13. **Provider-aware token counting** — `tiktoken-rs` for OpenAI, `chars/4` heuristic for Anthropic. Exact counting isn't critical with compaction + 400 recovery as safety nets.

## Key Crates (MVP)

- `tokio` — async runtime + signals + sync primitives
- `tokio-util` — CancellationToken for graceful shutdown
- `config` 0.15.x — layered TOML + env config with `try_deserialize()`
- `serde` + `serde_json` — serialization for config, tool args, provider wire formats
- `reqwest` — HTTP/LLM API calls
- `rusqlite` 0.38.x — SQLite with `bundled` feature (WAL mode)
- `poise` 0.6.1 — Discord bot framework (wraps serenity 0.12.4)
- `tracing` + `tracing-subscriber` — structured logging (JSON prod, pretty dev)
- `anyhow` + `thiserror` — error handling
- `dashmap` 6.x — concurrent conversation worker dispatch
- `regex` — directive XML parsing from LLM responses
- `tiktoken-rs` — accurate token counting for OpenAI-compatible providers
- `croner` — cron expression parsing for scheduler events

### Post-MVP Crates

- Enable `bundled` + FTS5 features on `rusqlite` — full-text search over existing notes table
- `sqlite-vec` (via rusqlite) — vector embeddings (C FFI extension)

## MVP Build Order

6 stages grouped into 3 implementation phases. Each phase builds on the last.

```
Phase 1: Foundation (Stages 1-2)
──────────────────────────────────
Stage 1: Scaffolding
├── Cargo project (Rust 2024 edition), git init, CI (cargo test + clippy + fmt)
├── Config types + `config` crate builder + validation
├── Event types (InEvent, OutEvent, Directive enum)
├── Core loop skeleton with cancellation token
└── tracing setup

Stage 2: Providers (parallel)
├── OpenAI-compatible provider (reqwest, works with Ollama)
└── Anthropic provider (reqwest, native Claude API)
    → validates Provider trait against both API shapes early
    → Phase 1 gate: providers make real API calls and return typed responses

Phase 2: Core Experience (Stages 3-4)
──────────────────────────────────────
Stage 3: CLI + Storage (parallel tracks)
├── Track A: CLI adapter (stdin/stdout, immediate dev feedback)
├── Track B: SQLite schema (notes + tags + links + conversations + messages tables)
├── Track C: core.md loading + conversation history + context window budget + compaction
├── Track D: Tool execution loop skeleton (dispatch, max rounds, timeout) — no tools registered yet
    → first end-to-end: type in CLI → LLM responds (text-only, tool loop exists but has no handlers)

Stage 4: Memory Tools + Directives (parallel)
├── Memory tools (memory_create, memory_search, memory_read, etc.)
├── Directive enum + XML parser + per-channel dispatch
└── Register memory tools in ToolRegistry
    → Phase 2 gate: CLI chat with working memory tools and directive parsing

Phase 3: Platform & Autonomy (Stages 5-6)
──────────────────────────────────────────
Stage 5: Discord Adapter
├── Discord channel adapter (poise 0.6.1)
├── Directive handling (reactions, etc.)
├── Rate limiting (per-user + global)
└── Authorization (allowlists, memory-write restrictions)

Stage 6: Scheduler + Migration (parallel)
├── Config-driven event system
├── Silent mode + Send directive
├── Heartbeat + daily reflection events
├── Jitter, active hours, overlap prevention
└── Letta migration script (one-time, separate from main binary)
    → Phase 3 gate: full MVP deployed on Discord with autonomous scheduled behavior
```

## Post-MVP Enhancements

```
Enhancement A: FTS5 Keyword Search
├── FTS5 virtual table over notes.content + notes.title
└── BM25 ranking for search results

Enhancement B: Embeddings + Hybrid Search
├── Embedder trait + HTTP implementation (Ollama /v1/embeddings)
├── sqlite-vec integration (C FFI, allocate extra time)
├── Semantic chunking (~400 tokens, sentence boundaries)
├── Hybrid search (BM25 + cosine + RRF)
└── Explicit link traversal via SQL adjacency list

Enhancement C: Telegram Adapter
├── teloxide, long-polling
└── Per-chat config (similar to Discord groups)

Enhancement D: Streaming Responses
├── chat_stream method on Provider trait
└── Per-channel streaming support (Discord typing indicator, TUI live update)

Enhancement E: Voice Support
├── Whisper/Voxtral transcription for incoming voice messages
└── TTS for Voice directive
```

## Deployment

Target: Linux (Docker container or bare VPS).

- **Paths:** relative by default (`database_path = "memory/borealis.db"`), absolute paths also accepted
- **Logging:** stdout/stderr (compatible with Docker log drivers and systemd journal), `RUST_LOG` for tracing filter
- **Docker:** multi-stage build, `debian:slim` runtime base (glibc for rusqlite's bundled SQLite)
- **VPS:** direct binary execution, systemd service file for process management
- **No musl/static linking required** — `debian:slim` provides glibc

## Testing Strategy

- **Unit tests:** memory module (CRUD, search, link integrity, soft delete), config validation, directive parsing, context window budget calculation, compaction trigger/accumulation logic
- **Integration tests:** mock provider (returns canned responses) → core loop → verify event flow, tool execution, directive routing
- **CI:** `cargo test` + `cargo clippy` + `cargo fmt --check` on every commit

## Verification Gates (per stage)

Each stage has a verification gate that must pass before moving to the next.

**Stage 1: Scaffolding**
- `cargo build` succeeds, `cargo clippy` clean, `cargo fmt --check` passes
- Config loads from `default.toml`, invalid config produces clear error messages
- Event types compile with correct derives (Clone, Debug, Serialize/Deserialize as needed)
- CancellationToken wired, SIGTERM/SIGINT triggers clean shutdown of empty loop

**Stage 2: Providers**
- OpenAI provider sends a request to Ollama (local), gets a valid `LlmResponse` back
- Anthropic provider sends a request to Anthropic API, gets a valid `LlmResponse` back
- Both handle 429/5xx with retry + backoff (mock or real)
- Timeout triggers graceful error, not panic

**Stage 3: CLI + Storage + History + Compaction + Tool Loop Skeleton**
- Type a message in CLI → LLM responds with text (tool loop exists but no tools registered)
- SQLite schema creates all tables (notes, tags, links, conversations, messages, conversation_summaries)
- `core.md` loads and appears in system prompt
- Conversation history persists across messages in SQLite
- Context window budget evicts oldest turns when exceeded; an assistant turn with tool calls is never partially evicted; core persona always present
- Compaction: when history exceeds 75% of budget, background LLM call produces summary; subsequent requests use it
- Compaction fallback: if compaction is in-progress, simple eviction is used (no blocking)
- Tool execution loop: dispatch, max rounds (10), timeout (120s) — verified with a dummy/echo tool in tests
- Sending many messages doesn't crash or OOM

**Stage 4: Memory Tools + Directives**
- Memory CRUD: `memory_create` → row exists in SQLite → `memory_read` returns it → `memory_update` changes content → `memory_search` finds it by tag/title/content → `memory_forget` sets `deleted_at`
- Link two notes → search surfaces linked note
- Directive parsing: `<actions><react emoji="👍"/></actions>` extracts correctly
- Code blocks excluded from directive parsing
- Memory tools registered in ToolRegistry and callable via LLM

**Stage 5: Discord Adapter**
- Bot connects to Discord, receives messages, responds in correct channel
- Per-group modes work: digest batches messages, mention-only ignores non-mentions, always responds to all
- Digest @mention bypass triggers immediate response
- Responses stay under 2000 chars via `max_tokens` config; overflow truncated with `…`
- Rate limiting rejects excessive messages with log
- Memory-write tools rejected for non-allowed users

**Stage 6: Scheduler + Migration**
- Heartbeat fires within expected interval ± jitter
- Events outside active hours are skipped
- Silent mode: text output not sent, `<send>` directive reaches correct channel
- Overlap prevention: second heartbeat skipped if first still running
- Letta migration script imports core memory → `core.md` + note rows in SQLite
- Personality consistency: compare migrated core persona with original Letta blocks

**Cross-cutting (run after each stage):**
- `cargo test` — all tests pass, no regressions
- `cargo clippy` — no warnings
- SIGTERM → clean shutdown (no SQLite corruption, no panics)
