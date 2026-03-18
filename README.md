# Borealis

A modular, multi-channel bot runtime written in Rust, built to power **Aurora** — a digital person with her own personality, interests, and evolving memory.

## Prerequisites

- **Rust 1.85+** (2024 edition) — install via [rustup](https://rustup.rs/)
- **At least one LLM provider:**
  - [Ollama](https://ollama.ai/) running locally (OpenAI-compatible, no API key needed)
  - Or an Anthropic API key
- **Discord bot token** (only if testing the Discord adapter)

## Build

```bash
cargo build
cargo test
```

## Quick start — CLI adapter

The fastest way to test Borealis is with the CLI adapter and a local Ollama instance.

### 1. Start Ollama

```bash
ollama serve
ollama pull llama3
```

### 2. Create a local config override

```bash
cp config/default.toml config/local.toml
```

Edit `config/local.toml` — the CLI adapter and OpenAI provider (pointing at Ollama) are already enabled by default, so minimal config is needed. The default config works out of the box with Ollama on `localhost:11434`.

### 3. Set up the runtime directories

```bash
mkdir -p memory
# core.md should already exist from the repo
```

### 4. Run

```bash
cargo run
```

You should see:

```
INFO borealis: configuration loaded bot_name="Aurora"
INFO borealis: borealis ready providers.anthropic=false providers.openai=true ...
INFO borealis: database opened with WAL mode path="memory/borealis.db"
```

The bot is running and waiting. Currently `main.rs` sets up config, database, and signal handling — the event loop and CLI adapter wiring are the next integration step. Press `Ctrl+C` for graceful shutdown.

### 5. Test with Anthropic instead

Set the API key and update your local config:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
```

The default config already has `providers.anthropic` configured pointing at `https://api.anthropic.com` with `claude-sonnet-4-20250514`.

## Discord setup

1. Create a bot in the [Discord Developer Portal](https://discord.com/developers/applications)
2. Enable the **MESSAGE_CONTENT** privileged gateway intent
3. Create `config/local.toml` with:

```toml
[channels.discord]
enabled = true
token_env = "DISCORD_BOT_TOKEN"

[[channels.discord.groups]]
guild_id = "YOUR_GUILD_ID"
response_mode = "always"
```

4. Set the token:

```bash
export DISCORD_BOT_TOKEN="your-token-here"
```

## Configuration

Borealis uses layered configuration (lowest to highest priority):

1. `config/default.toml` — base config (committed)
2. `config/{RUN_MODE}.toml` — environment-specific (e.g., `config/production.toml`)
3. `config/local.toml` — developer overrides (**gitignored**)
4. Environment variables prefixed `BOREALIS__` (e.g., `BOREALIS__DATABASE__PATH`)

API keys use env var indirection — the config specifies the *name* of the env var, not the key itself:

```toml
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"   # reads from $ANTHROPIC_API_KEY at runtime
```

See `config/default.toml` for all available options.

## Letta migration

If you're migrating from a Letta (MemGPT) deployment:

1. Export your Letta data as JSON via the Letta API
2. Place the export files in a directory
3. Run:

```bash
cargo run -- migrate-letta --source ./letta-export/
```

Options:

```
--source <path>   Directory containing Letta JSON export files (required)
--db <path>       SQLite database path (default: memory/borealis.db)
--core-md <path>  Path to core.md (default: memory/core.md)
```

## Project structure

```
borealis/
├── config/
│   ├── default.toml              # Base configuration
│   ├── compaction_prompt.md      # LLM prompt for context summarisation
│   └── local.toml                # Your local overrides (gitignored)
├── memory/
│   ├── core.md                   # Aurora's core persona (hand-editable)
│   └── borealis.db               # SQLite database (created at runtime)
├── src/
│   ├── main.rs                   # Entry point, config loading, shutdown
│   ├── config.rs                 # Layered config with validation
│   ├── channels/                 # Channel adapters (CLI, Discord)
│   │   └── modes.rs              # Pluggable response modes (always, mention-only, digest)
│   ├── core/
│   │   ├── event.rs              # InEvent, OutEvent, Directive types
│   │   ├── event_loop.rs         # Event bus + conversation worker dispatch
│   │   ├── directive.rs          # XML action block parser
│   │   └── supervisor.rs         # JoinSet task supervision + circuit breaker
│   ├── history/
│   │   ├── store.rs              # Turn-based conversation storage
│   │   ├── budget.rs             # Context window budget + eviction
│   │   └── compaction.rs         # LLM-driven context summarisation
│   ├── memory/                   # SQLite-backed note system
│   ├── providers/                # LLM providers (Anthropic, OpenAI-compatible)
│   ├── scheduler/                # Cron + recurring event scheduler
│   ├── tools/                    # Tool registry + memory tool handlers
│   ├── migrate.rs                # Letta JSON import
│   ├── rate_limit.rs             # Token bucket rate limiting
│   ├── shutdown.rs               # Graceful shutdown (signals, WAL checkpoint)
│   └── token.rs                  # Token estimation (tiktoken + heuristic)
└── tests/
```

## Environment variables

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `DISCORD_BOT_TOKEN` | Discord bot token |
| `RUST_LOG` | Tracing filter (e.g., `debug`, `borealis=trace`) |
| `RUN_MODE` | Config layer selection (e.g., `production`) |

## Running tests

```bash
cargo test              # All tests
cargo test config       # Config tests only
cargo test memory       # Memory module tests
cargo test history      # History + compaction tests
cargo test provider     # Provider tests (uses mock HTTP)
```
