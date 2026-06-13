# Harness Audit Fixes — 2026-06-12

Five parallel audit agents swept the codebase (core/channels, history/memory,
providers, tools/security, config/scheduler/lifecycle). Baseline: 308 tests
passing, clean clippy. This plan executes the confirmed findings as five
sequential batches, one commit each, TDD per fix.

Invariant: modularity is intentional — Aurora extends her own harness later.
Do not collapse trait-based extension points.

## Batch 1 — Security & tools (commit: `fix(security)`)

- **SEC-1 (critical, certain)** `src/main.rs:85-94`, `src/security/authorization.rs:57-59`:
  only the five memory write tools are registered restricted. `bash_exec`,
  `file_write`, `send_message`, `send_file`, `react` are NOT, despite
  `computer_tools.rs:36-38` claiming they are. Authorization defaults to
  Allowed for unregistered tools → prompt-injected users get arbitrary command
  execution. Fix: register the dangerous tools as restricted in main.rs; fix
  the stale doc comment; add an end-to-end test that `bash_exec` is denied for
  a non-authorized user.
- **SEC-3 (medium, certain)** `src/tools/computer_tools.rs:155-168`: `bash_exec`
  buffers unbounded child output via `wait_with_output()`; `file_read` and
  `file_list` similarly unbounded. Fix: cap captured stdout/stderr bytes
  (config, sane default e.g. 64 KiB) with truncation marker; cap file_read
  size and file_list entries.
- **SEC-2 mitigation (high, certain)** `bash_exec` runs `sh -c` with only
  `current_dir` set; absolute paths escape the sandbox; default sandbox_root
  is `"."`. Full OS confinement is out of scope; mitigate: document the
  limitation honestly in the doc comment + config comments (replace the false
  claim), and ship restriction-by-default (SEC-1 fix covers the authz gap).
- **SEC-5 (low, certain)** `src/security/rate_limit.rs:136-139`: global bucket
  fails OPEN on mutex poison. Fix: recover with `PoisonError::into_inner()`.
- **SEC-7 (medium, certain)** `src/tools/channel_tools.rs`: channel tools
  registered unconditionally — no `enabled` config gate (parity with
  computer/web tools). Fix: add `[tools.channel] enabled` config (default
  true), gate registration; restriction handled by SEC-1.

## Batch 2 — Providers (commit: `fix(providers)`)

- **PROV-1 (high, certain)** stop_reason deserialized then discarded in both
  providers; `LlmResponse` has no field. 1024-token truncation is silent.
  Fix: add provider-neutral `StopReason` enum (`EndTurn | MaxTokens | ToolUse
  | Refusal | Other(String)`) to `LlmResponse`; pipeline warns on MaxTokens
  and appends a visible `[response truncated]` marker rather than persisting
  truncated text as a clean turn. Make `max_response_tokens` configurable
  (currently hardcoded 1024 in registry.rs:152).
- **PROV-2 (medium, certain)** `anthropic.rs:177-188`: ContentBlock enum has
  no fallback variant → any unknown block type (thinking etc.) fails the whole
  response parse, and the body is consumed so the error is opaque. Fix:
  `#[serde(other)]`-style catch-all (untagged Unknown variant), skip with warn.
- **PROV-3 (medium, certain)** `retry.rs`: ignores Retry-After header; 408 not
  retryable. Fix: parse Retry-After (seconds form), sleep
  `max(retry_after, backoff)` capped; add 408 to retryable set.
- **PROV-5 (medium, certain)** `openai.rs:154-159`: `cl100k_base()` rebuilt on
  every estimate_tokens call (hundreds of ms). Fix: use
  `cl100k_base_singleton()` or build once in constructor.
- **PROV-6 (medium, likely)** `openai.rs:271-273`: malformed tool-call args
  silently coerced to `{}`. Fix: log warning with raw args; surface parse
  failure so the model sees an is_error tool result.
- **PROV-9 (low, certain)** `retry.rs`: dead `last_error` accumulation,
  unreachable `RetryError::Exhausted`, error body read after the sleep and
  discarded. Fix: read body before sleeping, log at warn, drop dead code.
- **PROV-4 (partial)** hardcoded `temperature: Some(0.7)` in registry.rs.
  Fix: make temperature per-provider config (`Option<f32>`, default 0.7
  preserved); omit from request body when None.

## Batch 3 — Pipeline & history (commit: `fix(history)`)

- **COMP-1 (high, certain)** `compaction.rs:182-189`: boundary = messages.len()/2
  ignores turn_id → splits tool loops, orphaned tool_results → provider 400s.
  Fix: snap boundary to a turn boundary (extend until turn_id changes).
- **COMP-2 (medium, certain)** compaction does sync SQLite on async threads.
  Fix: wrap store calls in spawn_blocking (codebase convention, see pipeline).
- **COMP-3 (medium, likely)** save_summary + delete_messages_up_to are separate
  critical sections racing readers. Fix: single store method doing both in one
  transaction under one lock; likewise a combined load_summary+load_messages_after
  read method for the pipeline.
- **BUD-1 / CORE-9 (medium, certain)** summary token_estimate counted both as
  synthetic turn AND as fixed overhead; summary turn is also FIRST evicted.
  Fix: count once; pin `__summary__` turn as non-evictable (exclude from
  select_turns, always prepend).
- **CORE-2 (high, certain→likely)** `pipeline.rs:527-541`: 400-recovery rebuilds
  prompt from pre-loop `included_turns`, dropping in-flight tool_use/tool_result
  messages → model re-issues side-effectful tool calls. Fix: track loop
  messages appended after assembly and re-append them onto the reduced set.
- **CORE-3 (medium, certain)** max-iterations break: final response with
  tool_calls never persisted but returned to user → history diverges. Fix:
  persist final text as plain assistant message on that path.
- **STORE-1 (low, certain)** LIKE escaping misses backslash itself (two sites).
  Fix: escape `\` first.
- **STORE-2 (low, certain)** lock_conn claims to recover from poisoning but
  returns Err. Fix: actually recover via `PoisonError::into_inner()` (both
  history and memory stores), log warn.

## Batch 4 — Channels & dispatcher (commit: `fix(channels)`)

- **CORE-1 (high, likely)** plain DMs silently dropped: DM "guild" key falls
  to default mention-only factory; nobody @-mentions in a DM. Fix: route DMs
  (`guild_id.is_none()`) to AlwaysMode before consulting the router.
- **CORE-5 (medium, certain)** Discord inbound fatal error (bad token,
  client.start() Err) → registry logs and exits; process stays up, deaf;
  outbound busy-polls http OnceCell forever. Fix: trigger cancellation token
  on inbound task failure (fail fast); give outbound poll a cancellation check.
- **CORE-6 (medium, likely)** eviction race: worker draining backlog evicted
  by idle timeout while still working → two workers per conversation. Fix:
  worker touches last_activity after each processed event.
- **CORE-7 (medium, certain)** dispatch() awaits send into a full per-worker
  buffer, blocking the whole channel (head-of-line). Fix: try_send + drop with
  warn on full buffer.
- **CORE-4 (medium, certain)** allowed_guilds rate-limit allowlist is dead:
  guild_id never reaches the check. Fix: add `guild_id: Option<String>` to
  MessageContext, populate from serenity msg, pass to rate_limiter.check.
- **CORE-11 (low, certain)** discord.rs:336-349: truncation compares bytes vs
  Discord's 2000-char limit (mangles non-ASCII); long replies destroyed.
  Fix: split into multiple ≤2000-char messages on char boundaries instead of
  truncating.
- **CORE-12 (low, certain)** digest tick task ignores cancellation → task leak.
  Fix: pass CancellationToken, add cancelled() select branch.
- **LIFE-7 scoped (medium, certain)** per-conversation workers never tracked or
  joined; shutdown kills in-flight work. Fix (scoped): track workers with
  tokio_util::task::TaskTracker, join them in await_shutdown before WAL
  checkpoint (5s force-exit backstop stays).

## Batch 5 — Scheduler & config (commit: `fix(scheduler)`)

- **LIFE-1 (high, certain)** symmetric jitter + next-occurrence-from-now →
  cron events double-fire (negative jitter) or skip (large positive). Fix:
  anchor next-occurrence search at max(now, last_scheduled_T); clamp cron
  jitter to non-negative or compute from T.
- **LIFE-2 (high, certain)** cron evaluated in UTC regardless of
  scheduler.timezone; {time} placeholder rendered UTC while {timezone} prints
  config zone. Fix: parse Tz once, find_next_occurrence on now.with_timezone(&tz),
  format {time} in tz.
- **LIFE-3 (medium, certain)** invalid timezone string silently → UTC at every
  fire. Fix: validate in Settings::validate(), fail fast; store parsed Tz.
- **LIFE-6 / CORE-8 (high, certain)** response_mode free-form string; factory
  wildcard falls back to AlwaysMode (most permissive). Fix: parse into enum at
  config load, fail fast on unknown; explicit "always" arm.
- **LIFE-8 (medium, certain)** interval "0s" → hot loop. Fix: reject zero
  intervals in ScheduledEventRunner::new.
- **LIFE-9 (low, certain)** parse_duration split_at panics on multi-byte last
  char. Fix: char-boundary-safe split.
- **LIFE-10 (low, certain)** invalid scheduler events warn-skipped. Fix:
  fail fast (Scheduler::new already returns Result).
- **LIFE-4 (medium, certain)** Jina key documented optional but validate()
  hard-fails when env var unset (and it's Some by default) → fresh checkout
  won't boot. Fix: warn instead of error for this key; align serde/Default.
- **LIFE-12 (medium, likely)** validate() range gaps: compaction threshold
  (require 0 < t <= 1), provider timeout_secs > 0, command_timeout_secs > 0,
  max_history_tokens > 0. Fix: add checks.
- **LIFE-13 (low, certain)** get_secret is dead panicking API. Fix: delete.
- **LIFE-14 (low, certain)** config tests assert against a copy-pasted mirror
  of resolve_env_var ("can't import from a binary crate" — false, it's a lib).
  Fix: export the real function, delete the mirror.

## Deferred (documented in PR, not fixed here)

- SEC-2 full OS-level bash confinement (needs container/namespace design)
- MEM-1 notes_fts keyed on implicit rowid (VACUUM hazard — needs schema migration)
- MIG-1/MIG-2 Letta migration timestamps + idempotency (one-shot tool, already run)
- HIST-2 history_search cross-conversation scoping (design decision)
- LIFE-5 [event_bus] config section wiring (decide: wire or remove from local.toml)
- LIFE-11 scheduler events processed sequentially (route through dispatcher)
- PROV-8 provider config as map for true provider modularity (design change)
- CORE-10 DigestMode mention bypass reorders history (needs flush-on-mention design)
- CORE-13 notify_error only fires for LLM errors (observer coverage)
