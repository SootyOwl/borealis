# Workspace Split + Runtime API

**Date:** 2026-04-07
**Status:** Draft — review amendments applied 2026-06-13 (see below)
**Related:** `.design/modular-runtime-and-new-tools.md`

## Review amendments (2026-06-13)

A harness audit (PR #9, `harness-audit-fixes`) hardened the runtime and, in the
process, resolved or sharpened several items this plan depends on. Apply these
before/while implementing the split:

1. **Workspace edition is 2024, not 2021.** The crate already uses edition-2024
   features (let-chains; edition-2024 `set_var` semantics in tests). The
   `[workspace.package]` block below has been corrected to `edition = "2024"` —
   do not downgrade or the build breaks.
2. **`Runtime::run` shutdown must drain more than channels.** The audit made
   `ChannelRegistry::await_shutdown()` *drain in-flight per-conversation workers*
   (via a `TaskTracker`) and added `Scheduler::shutdown()` + joining of the
   scheduler event-feed loop. So `Runtime::run`'s graceful shutdown must, after
   cancel: `channels.await_shutdown().await` → `scheduler.shutdown().await` →
   join the feed-loop handle → THEN WAL-checkpoint, all under the existing ~5s
   force-exit backstop. (§2/§5 below predate this; the ordering matters so an
   in-flight LLM call finishes its history write before exit.)
3. **Enumerate `sandbox_root` (and the rate-limit dir) in path resolution.** §1's
   path rules cover `db_path`/`core_md_path`/`system_prompt_path`, but the
   highest-impact relative path is `tools.computer_use.sandbox_root`, which
   **defaults to `"."`**. Post-split, that resolves to Aurora's CWD (which holds
   `.env`, the DB, `memory/`) — intended, but it MUST be an explicit resolution
   rule (resolved against `data_dir` or an explicit `sandbox_dir`, never left as
   bare `"."`), because `bash_exec` is confined only by CWD + the authorization
   layer, not by OS sandboxing. Also resolve the rate-limiter persistence dir.
4. **MIG-1/MIG-2 are now fixed in core (§4 unblocked).** `migrate::run_migration`
   now preserves original Letta timestamps and is idempotent (a re-run imports
   only new rows, tracked in a `letta_imported` table) — verified against the
   real export. So moving `migrate-letta` to aurora and re-running it to capture
   new Letta data is now safe and won't duplicate. (This was the main reason the
   re-run had been deferred.)
5. **PROV-8 is now done (revisit the §"Risks" #4 deferral).** Provider selection
   is no longer hardcoded: `[providers.<name>]` flattens into a map, resolution
   matches the name against the `inventory` registry, and `bot.default_provider`
   picks the primary (validated — a typo fails fast). Aurora can therefore add a
   **custom provider** in her own crate via `inventory::submit!` +
   `[providers.her-provider]` + `bot.default_provider = "her-provider"`, with NO
   edits to borealis-core. This directly serves the §intro goal "Aurora extends
   her own functionality … eventually providers."
6. **Open question #3 resolved:** `ChannelDeps` already carries a
   `CancellationToken` (confirmed), so §5's CLI-cancel fix (Option 1: add a
   `cancel` field) needs no new plumbing. That exact pattern is already proven —
   the audit added a `cancel` field to `DiscordAdapter` for the digest-tick task.
   Mirror it for `CliAdapter`.

The body below is the original draft; read it together with the amendments above.

## Context

`borealis` is currently a single-crate binary. Aurora (the "production" bot) lives
inside this repo, sharing `config/`, `memory/`, and the binary entry point with
the generic runtime. We want to:

1. Extract Aurora into a **separate private repo** so her persona, memory
   database, and personality-specific tools aren't coupled to borealis commits.
2. Let Aurora **extend her own functionality** by writing custom tools (and
   eventually providers or channels) in her own crate, with minimal friction.
3. Keep borealis itself usable as a reference runtime — `cargo run` in this
   repo should still work end-to-end against local `config/` and `memory/`.

The audit (see chat history 2026-04-07) confirmed the codebase is ready:
inventory-based registration already exists for tools, providers, channels,
and memory backends. No circular dependencies. `main.rs` is reasonably thin.

This document specifies the split, the `Runtime` API, the
`Settings::load_from()` change, and a fix for the CLI channel's stdin
cancellation wart.

## Non-goals

- Changing the on-wire protocol, message format, or any tool definitions.
- Replacing the inventory-based registration with a runtime builder. Inventory
  works and downstream crates can submit to it transparently; no reason to
  duplicate that mechanism.
- Migrating Aurora's data. That's a separate step (Letta export → `migrate-letta`)
  and happens *after* the split lands.
- Provider selection by config. Flagged as optional in the audit; deferring.

## Target layout

### This repo (`borealis/`)

```
borealis/
├── Cargo.toml                      # [workspace]
├── crates/
│   ├── borealis-core/              # the library
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs              # re-exports
│   │       ├── channels/           # (moved from src/)
│   │       ├── config.rs
│   │       ├── core/
│   │       ├── history/
│   │       ├── memory/
│   │       ├── migrate.rs
│   │       ├── providers/
│   │       ├── runtime.rs          # NEW — Runtime struct
│   │       ├── scheduler/
│   │       ├── security/
│   │       ├── shutdown.rs
│   │       ├── tools/
│   │       └── types.rs
│   └── borealis-cli/               # the reference binary
│       ├── Cargo.toml
│       └── src/main.rs             # thin: tracing init, dotenv, Runtime::run
├── config/                         # unchanged — used by borealis-cli for dev
├── memory/                         # unchanged
└── docs/
```

### Aurora repo (new, private)

```
aurora/
├── Cargo.toml                      # depends on borealis-core (git dep)
├── src/
│   ├── main.rs                     # ~30 lines
│   └── tools/                      # Aurora's custom tools
│       ├── mod.rs
│       └── *.rs                    # each ends with inventory::submit!()
├── config/
│   ├── default.toml
│   └── system_prompt.md
├── memory/
│   ├── core.md
│   └── borealis.db                 # gitignored
├── .env                            # gitignored
└── .gitignore
```

## Component changes

### 1. `Settings::load_from(config_dir, data_dir)`

**Current:** `Settings::load()` hardcodes `config/default`, `config/{run_mode}`,
`config/local` as relative paths, and paths like `memory/borealis.db`,
`memory/core.md`, `config/system_prompt.md` are literal strings in
`BotConfig` defaults.

**New:**

```rust
impl Settings {
    /// Load settings using the default layout (config/ and memory/ relative to CWD).
    /// Kept for backwards compatibility — `borealis-cli` still uses this.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(Path::new("config"), Path::new("memory"))
    }

    /// Load settings from explicit directories.
    /// - `config_dir`: where default.toml, {run_mode}.toml, local.toml,
    ///   and system_prompt.md live.
    /// - `data_dir`: where borealis.db and core.md live.
    ///
    /// Paths inside the loaded config that were relative to CWD are resolved
    /// against `data_dir` (for database + memory files) or `config_dir`
    /// (for system_prompt.md), unless they're absolute.
    pub fn load_from(config_dir: &Path, data_dir: &Path)
        -> Result<Self, ConfigError>
    { ... }
}
```

**Path resolution rules:**
- `bot.db_path`: if relative, resolved against `data_dir`. Default
  `borealis.db` → `data_dir/borealis.db`.
- `bot.core_md_path`: if relative, resolved against `data_dir`. Default
  `core.md` → `data_dir/core.md`.
- `bot.system_prompt_path`: if relative, resolved against `config_dir`.
  Default `system_prompt.md` → `config_dir/system_prompt.md`.
- Any other file-system path in config that's currently relative (memory
  sandbox roots, etc.) needs a similar rule — we'll enumerate them during
  implementation.

**`BOREALIS_RUN_MODE`** env var behavior is unchanged.

### 2. `Runtime` struct

New file: `crates/borealis-core/src/runtime.rs`.

```rust
pub struct Runtime {
    settings: Settings,
    db: Arc<Mutex<Connection>>,
    history: Arc<HistoryStore>,
    memory: Arc<dyn Memory>,
    tools: Arc<ToolRegistry>,
    security: Arc<Security>,
    pipeline: Arc<dyn PipelineRunner>,
    channels: ChannelRegistry,
    scheduler: Option<SchedulerHandle>,
    cancel: CancellationToken,
}

impl Runtime {
    /// Build a fully-wired runtime from settings.
    ///
    /// Performs: DB open + schema init, memory store build (inventory
    /// dispatch), security setup, tool registration (inventory dispatch),
    /// pipeline build (inventory dispatch), channel registration (inventory
    /// dispatch), optional scheduler setup.
    ///
    /// Does NOT spawn long-running tasks yet — that happens in `run()`.
    pub async fn new(settings: Settings) -> Result<Self> { ... }

    /// Returns a clone of the cancellation token so callers can trigger
    /// shutdown from a signal handler or test harness.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Spawn all adapters and the scheduler, then wait for cancellation.
    /// On cancel, run graceful shutdown (WAL checkpoint, flush channels).
    pub async fn run(self) -> Result<()> { ... }
}
```

**What `main.rs` keeps:**
- `tracing_subscriber` init (binary-specific logging choice)
- `dotenvy::dotenv()` (optional `.env` load)
- Signal handler registration (SIGINT, SIGTERM) → calls `runtime.cancel_token().cancel()`
- Path resolution (where is `config/`, where is `memory/`)

**What the `migrate-letta` subcommand does NOT do:** live in `borealis-cli`.
It moves to aurora's binary — see §5 below. `borealis_core::migrate::run_migration`
stays as a public API so aurora can call it.

**What moves into `Runtime::new`:**
- All of the current wiring in `main.rs` between "load settings" and "wait for cancellation."
- Roughly lines ~60–250 of current `main.rs`.

**What moves into `Runtime::run`:**
- Scheduler task spawn
- Channel adapter spawn (`register_all_channels`)
- `cancel.cancelled().await`
- Graceful shutdown orchestration (currently in `shutdown.rs`, already a pub fn)

### 3. Aurora's `main.rs` (reference target)

```rust
use borealis_core::{Runtime, Settings};
use std::path::Path;
use tokio::signal;

mod tools; // each submodule fires inventory::submit!() at link time

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into())
        )
        .init();
    let _ = dotenvy::dotenv();

    let settings = Settings::load_from(
        Path::new("./config"),
        Path::new("./memory"),
    )?;

    let runtime = Runtime::new(settings).await?;
    let cancel = runtime.cancel_token();

    tokio::spawn(async move {
        let _ = signal::ctrl_c().await;
        cancel.cancel();
    });

    runtime.run().await
}
```

### 4. `migrate-letta` moves to aurora

The Letta migration is a one-time, aurora-specific operation. There's no reason
for the generic `borealis-cli` reference binary to carry the subcommand.

**Changes:**
- **Delete** the `migrate-letta` subcommand dispatch from `borealis-cli/src/main.rs`.
  The subcommand never runs against the reference `config/` + `memory/` anyway —
  it only ever made sense for Aurora's data.
- **Keep** `borealis_core::migrate::run_migration(source_dir, db_path, core_md_path)`
  as a public function. It already is public; this just confirms it stays that way.
- **Add** subcommand dispatch to aurora's `main.rs`:

  ```rust
  #[tokio::main]
  async fn main() -> anyhow::Result<()> {
      // ... tracing + dotenv init ...

      let args: Vec<String> = std::env::args().collect();
      if args.get(1).map(String::as_str) == Some("migrate-letta") {
          return run_migrate_letta(&args[2..]);
      }

      // ... Runtime::new + run ...
  }

  fn run_migrate_letta(args: &[String]) -> anyhow::Result<()> {
      // parse --source / --db / --core-md flags
      let source = /* from args */;
      let db = /* from args, default ./memory/borealis.db */;
      let core_md = /* from args, default ./memory/core.md */;
      let stats = borealis_core::migrate::run_migration(&source, &db, &core_md)?;
      println!("{stats}");
      Ok(())
  }
  ```

Aurora runs `cargo run -- migrate-letta --source ./letta-export` from her own
repo, which picks up her own `./memory/` paths as defaults. Clean and obvious.

### 5. CLI channel stdin cancellation fix

**Current wart:** `tokio::io::stdin()` holds a blocking OS read. Cancellation
tokens don't unblock it, so `main.rs` force-exits after a 5-second shutdown
timeout.

**Fix:** Spawn stdin reading on a dedicated blocking thread and bridge to
async via a bounded channel. On shutdown, drop the channel sender — the
async loop detects it, breaks, and the blocking thread is allowed to leak
(it'll die when the process exits, which is fine because there's nothing
to checkpoint for stdin).

```rust
async fn run_inbound(self: Arc<Self>, tx: Sender<InEvent>) -> Result<()> {
    info!("CLI adapter inbound started — type messages below");

    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(16);

    // Reader thread — blocking stdin reads, forwards to async side.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut buf = String::new();
        loop {
            buf.clear();
            match stdin.lock().read_line(&mut buf) {
                Ok(0) => break,                           // EOF
                Ok(_) => {
                    let line = buf.trim_end().to_string();
                    if line_tx.blocking_send(line).is_err() {
                        break;                            // async side dropped
                    }
                }
                Err(_) => break,
            }
        }
    });

    let cancel = self.cancel.clone(); // see note below
    let mut seq: u64 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("CLI inbound cancelled");
                break;
            }
            maybe_line = line_rx.recv() => {
                let Some(line) = maybe_line else {
                    info!("stdin closed — CLI adapter shutting down");
                    break;
                };
                // ... existing logic: trim, /quit, send InEvent ...
            }
        }
    }

    Ok(())
}
```

**Dependency:** `CliAdapter` needs access to a `CancellationToken`. Options:
1. Add a `cancel: CancellationToken` field to `CliAdapter`, populated at
   construction by whoever registers it (the channels registry already has
   access to the token via `ChannelDeps`).
2. Accept it as a second parameter to `run_inbound` — but that breaks the
   `Channel` trait signature.

**Chosen:** Option 1. Add the field. `ChannelDeps` already carries the token,
so the registration closure can pass it through when constructing `CliAdapter`.

**Result:** After this fix, `main.rs` can stop force-exiting after 5s — the
shutdown timeout can be removed (or kept as a safety net at 30s).

## Dependency movement

**Lives in `borealis-core` Cargo.toml:**
- `anyhow`, `thiserror`
- `chrono`, `chrono-tz`, `croner`
- `config`
- `dashmap`
- `inventory`
- `poise`, `serenity` (Discord adapter)
- `rand`, `regex`, `reqwest`
- `rusqlite`
- `serde`, `serde_json`
- `tiktoken-rs`
- `tokio` (features TBD — probably not `full`, just what the lib needs)
- `tokio-util`
- `tracing` (just the facade, not `tracing-subscriber`)
- `uuid`

**Lives in `borealis-cli` (and aurora) Cargo.toml:**
- `borealis-core`
- `anyhow`
- `dotenvy`
- `tokio` with `full` + `macros` + `rt-multi-thread`
- `tracing-subscriber` with `env-filter`, `json`

## Workspace Cargo.toml shape

```toml
[workspace]
resolver = "2"
members = ["crates/borealis-core", "crates/borealis-cli"]

[workspace.package]
edition = "2024"   # the crate uses edition-2024 features — do not downgrade
license = "..."

[workspace.dependencies]
# Shared versions pinned here; member crates reference via workspace = true
anyhow = "1"
tokio = { version = "1", default-features = false }
# ... etc
```

This lets aurora's separate `Cargo.toml` depend on borealis-core via either:
- `borealis-core = { git = "https://...", branch = "main" }` (normal case)
- `borealis-core = { path = "../borealis/crates/borealis-core" }` (local dev)

## Migration / rollout plan

1. **Plan review** (this doc) — confirm shape before touching code.
2. **Workspace-ify** — move `src/` → `crates/borealis-core/src/`, create
   `crates/borealis-cli/src/main.rs` from the current `main.rs`, create the
   workspace `Cargo.toml`. Build must pass; `cargo run -p borealis-cli`
   must still work end-to-end against local `config/` and `memory/`.
3. **`Settings::load_from`** — add the variant, keep `load()` as a shim.
   Parametrize `db_path`, `core_md_path`, `system_prompt_path` (and any
   other relative paths discovered during implementation). Update
   `borealis-cli` to call `load_from` with `Path::new("config")` and
   `Path::new("memory")` explicitly.
4. **Extract `Runtime`** — move wiring from `main.rs` into
   `crates/borealis-core/src/runtime.rs`. `borealis-cli/src/main.rs` shrinks
   to the "reference target" shape (essentially the same as aurora's
   planned main.rs but pointing at `./config` and `./memory`).
5. **Remove `migrate-letta` from `borealis-cli`** — delete the subcommand
   dispatch. Confirm `borealis_core::migrate::run_migration` is public.
6. **CLI stdin fix** — implement the blocking-thread bridge, remove the
   5s force-exit from shutdown (or raise it to a safety-net value).
7. **Verify** — run the reference binary, connect Discord, run CLI channel,
   type `/quit`, make sure shutdown is clean and fast.
7. **Out-of-scope follow-ups** that unblock aurora but don't block this PR:
   - Create the aurora repo
   - Move `config/` and `memory/` into aurora
   - Run Letta export + `migrate-letta` against aurora's data dir

Steps 2–6 are all in this repo and can land as a single PR or a small stack.

## Risks and open questions

1. **Relative paths we haven't enumerated.** The audit identified the
   obvious ones (`db_path`, `core_md_path`, `system_prompt_path`). During
   implementation we'll grep for any other `"config/..."` or `"memory/..."`
   literals and handle them. Low risk but worth being explicit.
2. **`config` crate's `File::from(path)` vs `with_name(&str)`.** The current
   loader uses `with_name`. `load_from` needs `File::from(path.join("default"))`.
   Extension auto-detection should still work. Needs verification during
   implementation.
3. **`ChannelDeps` shape.** Need to confirm `CancellationToken` is already
   in it. If not, adding it is trivial but touches the registration closure
   for every channel.
4. **Provider selection** stays hardcoded Anthropic-first for now. If
   aurora wants a custom provider, it needs to be configured such that
   the hardcoded priority picks it — which means either patching the
   priority list or making provider selection config-driven. Deferred.
5. **Git dependency vs published crate.** Aurora will depend on borealis-core
   via a git dep initially. If borealis-core ever gets published to crates.io,
   aurora can switch. Not a blocker.

## What success looks like

- `cargo build` at the workspace root builds both crates cleanly.
- `cargo run -p borealis-cli` in this repo behaves identically to
  `cargo run` today.
- `Settings::load_from("./some/config", "./some/data")` works and resolves
  paths correctly.
- Ctrl-C in the CLI shuts down cleanly in <500 ms (vs. the current 5s
  force-exit).
- A hypothetical aurora crate with a `main.rs` matching the reference target
  in §3 compiles and runs against borealis-core as a git dependency.
- Aurora can drop a new file under `src/tools/`, call
  `inventory::submit!(ToolRegistration { ... })`, and have the tool appear
  in the registry without any edits to `main.rs` or `Cargo.toml` beyond
  `mod` declarations.
