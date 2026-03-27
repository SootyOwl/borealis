# Feature: Runtime Gaps — Unimplemented Design Doc Features

## Summary

The original `BOREALIS-DESIGN-DOC.md` specifies several runtime capabilities that were not implemented during the modular runtime refactor. Additionally, the modular runtime design doc has two ACs (AC-1, AC-7) that were incorrectly marked complete. This plan addresses all gaps.

## Requirements

- REQ-1: **Fix clippy warnings** — Resolve the 2 clippy warnings (type complexity in `providers/mod.rs`, unused field in config test) to satisfy AC-1.

- REQ-2: **Memory directory exclusion in sandbox** — `file_write` and `file_read` must reject paths under the `memory/` directory with an error directing users to the `memory_*` tools. Currently the exclusion is only mentioned in the tool description but not enforced in code. Satisfies AC-7.

- REQ-3: **Wire rate limiter into message flow** — The `RateLimiter` is fully implemented and tested but never called. Per the design doc, rate limiting should be applied before events reach the pipeline. Channel adapters or the processing loop should call `security.rate_limiter.check()` and drop rate-limited messages with a log warning.

- REQ-4: **LLM concurrency semaphore** — Add a `tokio::sync::Semaphore` (configurable, default 4) to limit concurrent LLM API calls. Acquired in the pipeline just before calling `provider.chat()`, released after the response. Prevents unbounded API cost when multiple conversations are active.

- REQ-5: **400 recovery (context overflow retry)** — When the LLM provider returns HTTP 400 (context too large), the pipeline should drop the oldest non-fixed turns from history and retry the LLM call once. If the retry also fails, fall back to system prompt + core persona + current message only.

- REQ-6: **Per-conversation worker isolation** — Replace the single processing loop per channel with per-conversation workers dispatched via `DashMap<ConversationId, Sender<InEvent>>`. Workers are spawned lazily on first message and cleaned up after 30 minutes idle. A periodic sweep (every 5 minutes) handles eviction. This prevents one slow conversation from blocking all others on the same channel.

## Acceptance Criteria

- [x] AC-1: `cargo clippy` produces zero warnings. (REQ-1) — *type alias in providers/mod.rs, test annotations fixed*
- [x] AC-2: `file_write("memory/core.md", "hack")` returns an error referencing the memory tools. `file_read("memory/anything.txt")` is similarly rejected. Test coverage for both. (REQ-2) — *SandboxError::MemoryDirBlocked + component matching fallback for missing dirs, 7+ tests*
- [x] AC-3: A non-allowlisted user sending >capacity messages in quick succession receives no pipeline processing for the excess messages. Rate-limited messages are logged with `warn!`. Allowlisted users bypass. (REQ-3) — *wired in dispatcher loop, scheduler bypass, 2 tests*
- [x] AC-4: With semaphore permits = 2, three concurrent pipeline calls result in at most 2 active LLM calls at any time. The third blocks until a permit is released. (REQ-4) — *Arc<Semaphore> in PipelineDeps, config validated > 0, ConcurrencyTrackingProvider test*
- [x] AC-5: When a mock provider returns HTTP 400, the pipeline retries with fewer messages. If retry succeeds, the response is returned normally. If retry also fails (400), the pipeline falls back to minimal context. (REQ-5) — *call_llm_with_400_recovery, RetryError::status_code(), 2 tests*
- [x] AC-6: Two messages to different conversations on the same channel are processed concurrently (not serialized). A slow conversation does not block a fast one. Workers idle for >30min are cleaned up. (REQ-6) — *ConversationDispatcher with DashMap + AtomicU64 ActivityTimestamp, lock-free eviction, 4 tests*

## Implementation Notes

### REQ-1: Clippy fixes
- `providers/mod.rs:28`: Extract the complex function pointer type into a type alias
- Config test unused field: Remove or use the field

### REQ-2: Memory dir exclusion
- Add a `memory_dir` field to `Sandbox` (optional `PathBuf`)
- In `validate_path()`, after canonicalization, check if the path starts with the memory dir
- New error variant: `SandboxError::MemoryDirBlocked`
- Wire from `Settings::tools.computer_use.memory_dir` (defaults to `memory/`)

### REQ-3: Rate limiter wiring
- The `ChannelRegistry` processing loop already has access to the pipeline
- Add `security: Arc<Security>` to `ChannelDeps` and `ChannelRegistry`
- In the processing loop, before calling `pipeline.process()`, call `security.rate_limiter.check()`
- If rate limited, log and skip (don't send to pipeline)
- Scheduler events bypass rate limiting (they have `ChannelSource::Scheduler`)

### REQ-4: Semaphore
- Add `llm_semaphore: Arc<Semaphore>` to `PipelineDeps`
- In `Pipeline::process_impl()`, acquire permit just before the LLM call, release after
- Configurable via `[bot] max_concurrent_llm = 4` in config
- Create in `main.rs`, pass through deps

### REQ-5: 400 recovery
- In `Pipeline::process_impl()`, catch 400 errors from `provider.chat()`
- First retry: evict oldest half of turns, rebuild messages, retry
- Second failure: strip to system prompt + core persona + current user message only
- Requires provider errors to carry HTTP status — check if `RetryError::HttpStatus` is propagated

### REQ-6: Per-conversation workers
- This is the largest change. Replace the processing loop in `ChannelRegistry::register()`
- New struct: `ConversationDispatcher` wrapping `DashMap<ConversationId, Sender<InEvent>>`
- On each inbound event, route to existing worker or spawn a new one
- Each worker: loop recv → pipeline.process → out_tx.send, with idle timeout
- Eviction sweep: tokio::spawn a periodic task that drops idle senders
- Workers that find their receiver closed exit naturally
