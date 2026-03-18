# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Added
- Add core pipeline and wire up CLI adapter for end-to-end chat (#34)
- Add README with setup and manual testing instructions (#32)
- REQ-7: Directive system — XML `<actions>` blocks parsed from LLM responses with variants: `NoReply`, `React`, `Voice`, `SendFile`, `Send`. Parsing strategy: strip fenced code blocks (`` ``` ... ``` ``) from the response text first, then regex-scan for `<actions>` blocks in the remaining text. Malformed XML within action blocks logs a warning and is skipped. Unsupported directives gracefully skipped with log. (#14)
- REQ-13: LLM-driven context window compaction. When conversation history exceeds a configurable threshold (default 75% of token budget), a background LLM call summarizes the oldest messages into a compact summary block. The summary replaces the summarized messages in prompt assembly, preserving conversational continuity without losing context. Summaries are stored in SQLite per-conversation and accumulate (a new compaction summarizes the previous summary + messages since). Compaction runs asynchronously — it does not block the current request; the current request uses simple eviction as a fallback until the summary is ready. Configurable per-channel: `compaction_threshold` (0.0-1.0, default 0.75), `compaction_model` (can use a cheaper/faster model than the main conversation model). (#13)
- REQ-6: Conversation history stored in SQLite with token-budget-aware context window management. Priority: system prompt + core persona (fixed) > tool definitions (fixed) > conversation history (sliding window) > retrieved memories. Eviction operates on **turns**, not individual messages. A turn is the atomic eviction unit: a user turn is the user message; an assistant turn is the assistant response plus all tool_calls it made, their tool_results, and any follow-up assistant responses within the same tool loop. This guarantees the message sequence always satisfies provider API schema requirements (no orphaned tool_calls or tool_results). Oldest turns are evicted first. (#12)
- REQ-5: SQLite-backed memory system (notes, tags, links tables) with 8 discrete tool handlers (`memory_create`, `memory_search`, `memory_read`, `memory_update`, `memory_link`, `memory_tag`, `memory_forget`, `memory_list`) registered in a `ToolRegistry`. Core persona loaded from `memory/core.md`, always injected into system prompt. (#11)
- REQ-4: Tokio-based event bus with bounded `mpsc` channels (default 256). Per-conversation sequential workers dispatched via `DashMap<ConversationId, Sender<InEvent>>`, with idle eviction after 30 minutes. Global LLM concurrency limited by `tokio::sync::Semaphore` (default 4 permits). (#10)
- REQ-3: Channel adapters (CLI for dev, Discord via `poise` 0.6.1) implementing the `Channel` trait with split inbound/outbound task methods. Three pluggable response modes (`digest`, `mention-only`, `always`) implemented as adapter-independent components via a `ResponseMode` trait. Modes are assigned per-group in config and sit between the event bus and the core pipeline, controlling when messages are dispatched for processing. Any adapter can use any mode. (#9)
- REQ-2: Two LLM providers (Anthropic native API, OpenAI-compatible for Ollama/GPT) built with raw `reqwest`, implementing a shared `Provider` trait. Both support configurable `base_url`, retry with exponential backoff on 429/5xx (max 3 retries, 1s base delay, 2x multiplier, 30s max delay, +/- 25% jitter), and per-request timeout (default 60s). Both use non-streaming mode (`stream: false`). Each provider maps between borealis's internal types and the provider's wire format (Anthropic content blocks vs OpenAI choices array). (#8)
- REQ-1: Layered TOML configuration with environment variable overrides, validated at startup with clear error messages for missing/invalid values. Uses the `config` crate (`Config::builder()` with file + env sources, `try_deserialize()` into typed structs). (#7)
- Fold gap analysis findings into design doc (#6)
- Resolve 3 open questions in borealis-mvp design doc (#5)
- Resolve open questions and update Borealis design doc with pluggable response modes, tiktoken, and JSON migration (#3)
- Update context window eviction to turn-based model (#2)
- Draft Borealis MVP design document with 3-phase implementation plan (#1)

### Fixed
- Fix process not exiting cleanly on Ctrl+C (#35)
- Fix provider config so missing API keys don't block startup (#33)

### Changed
