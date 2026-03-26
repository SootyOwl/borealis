# Extending Borealis

Borealis is built around five core traits. Each one follows a consistent pattern for adding new implementations.

## Adding a new Tool

Tools are LLM-callable capabilities. They self-register via `inventory` — no central file needs editing.

**Steps:**

1. Create `src/tools/my_tools.rs`
2. Implement the `Tool` trait for each tool
3. Add a registration function and `inventory::submit!` block
4. Add `mod my_tools;` to `src/tools/mod.rs`

**Template:**

```rust
use std::sync::Arc;
use crate::tools::{Tool, ToolContext, ToolDef, ToolDeps, ToolRegistration, ToolRegistry, ToolResult};

// Registration — this is all you need for auto-discovery
fn register(registry: &mut ToolRegistry, deps: &ToolDeps) {
    // Check config if this tool group is gated:
    // if !deps.settings.my_feature.enabled { return; }
    registry.register(MyTool);
}

inventory::submit! {
    ToolRegistration {
        name: "my_group",
        register_fn: register,
    }
}

// Tool implementation
struct MyTool;

impl Tool for MyTool {
    fn name(&self) -> &str { "my_tool" }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "my_tool".to_string(),
            description: "Does something useful".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string", "description": "The input" }
                },
                "required": ["input"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        // Your implementation here
        ToolResult {
            call_id: ctx.conversation_id.clone(),
            content: serde_json::json!({ "result": "ok" }),
            is_error: false,
        }
    }
}
```

**That's it.** The `inventory::submit!` block causes automatic registration at startup. No changes to `main.rs` or `register_all()`.

## Adding a new Provider

Providers are LLM backends. They also self-register via `inventory`.

**Steps:**

1. Create `src/providers/my_provider.rs`
2. Implement the `Provider` trait
3. Add an `inventory::submit!` block with a pipeline factory function
4. Add `mod my_provider;` to `src/providers/mod.rs`
5. Add config section to `src/config.rs` (in `ProvidersConfig`)

**Template:**

```rust
use std::sync::Arc;
use crate::core::pipeline::{Pipeline, PipelineDeps, PipelineRunner};
use crate::providers::{Provider, ProviderConfig};
use crate::providers::ProviderRegistration;

pub struct MyProvider { /* ... */ }

impl MyProvider {
    pub fn new(config: ProviderConfig) -> anyhow::Result<Self> {
        // Construct your provider
        Ok(Self { /* ... */ })
    }
}

impl Provider for MyProvider {
    // Implement chat(), estimate_tokens(), etc.
}

// Auto-registration
fn build_pipeline(
    config: ProviderConfig,
    sys_path: &std::path::Path,
    persona_path: &std::path::Path,
    deps: PipelineDeps,
) -> anyhow::Result<Arc<dyn PipelineRunner>> {
    let provider = Arc::new(MyProvider::new(config)?);
    tracing::info!(model = %config.model, "using MyProvider");
    let pipeline = Pipeline::new(provider, sys_path, persona_path, deps)?;
    Ok(Arc::new(pipeline))
}

inventory::submit! {
    ProviderRegistration {
        name: "my_provider",  // must match the config section key
        build_pipeline_fn: build_pipeline,
    }
}
```

**Config** (`config/local.toml`):
```toml
[providers.my_provider]
base_url = "https://api.example.com"
model = "my-model"
api_key_env = "MY_PROVIDER_API_KEY"
```

## Adding a new Channel

Channels are platform adapters (Discord, CLI, Telegram, etc.). They self-register via `inventory` — no changes to `main.rs` needed.

**Steps:**

1. Create `src/channels/my_channel.rs`
2. Implement the `Channel` trait
3. Add a `pub fn register()` function and `inventory::submit!` block
4. Add `pub mod my_channel;` to `src/channels/mod.rs`
5. Add config section to `src/config.rs` (in `ChannelsConfig`)

**Template:**

```rust
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use crate::channels::{Channel, ChannelRegistration, ChannelRegistry};
use crate::config::Settings;
use crate::core::event::{InEvent, OutEvent};
use crate::core::pipeline::PipelineRunner;

// Auto-registration via inventory
inventory::submit! {
    ChannelRegistration {
        name: "my_channel",
        register_fn: |registry, deps| {
            register(registry, deps.settings, deps.pipeline.clone(), deps.cancel.clone());
        },
    }
}

pub struct MyChannel { /* ... */ }

impl Channel for MyChannel {
    fn name(&self) -> &str { "my_channel" }

    async fn run_inbound(self: Arc<Self>, tx: Sender<InEvent>) -> Result<()> {
        // Listen for messages from your platform
    }

    async fn run_outbound(self: Arc<Self>, rx: Receiver<OutEvent>) -> Result<()> {
        // Send responses to your platform
    }
}

/// Register this channel if configured and enabled.
pub fn register(
    registry: &mut ChannelRegistry,
    settings: &Settings,
    pipeline: Arc<dyn PipelineRunner>,
    cancel: CancellationToken,
) {
    // Check config
    let config = match &settings.channels.my_channel {
        Some(c) if c.enabled => c,
        _ => return,
    };

    let channel = Arc::new(MyChannel::new(config));
    registry.register(channel, pipeline, cancel);
}
```

**That's it.** The `inventory::submit!` block causes automatic registration at startup. No changes to `main.rs` or `register_all_channels()`.

## Adding a new Memory backend

Memory backends implement the `Memory` trait. They self-register via `inventory` — no changes to `main.rs` needed.

**Steps:**

1. Create `src/memory/my_backend.rs`
2. Implement the `Memory` trait (10 methods)
3. Add an `inventory::submit!` block with a `MemoryRegistration`
4. Add `pub mod my_backend;` to `src/memory/mod.rs`

**Template:**

```rust
use std::sync::Arc;
use crate::config::Settings;
use crate::memory::{Memory, MemoryRegistration, MemoryResult, Note, Link};

pub struct MyMemory { /* ... */ }

impl Memory for MyMemory {
    // Implement all 10 methods
}

inventory::submit! {
    MemoryRegistration {
        name: "my_backend",
        build_fn: |settings| {
            let backend = MyMemory::new(/* from settings */)?;
            Ok(Arc::new(backend))
        },
    }
}
```

The `Memory` trait is synchronous — callers wrap calls in `tokio::task::spawn_blocking`. If your backend needs async I/O, do the async work internally and block on it.

## Adding a new Observer

Observers receive lifecycle events from the pipeline. They self-register via `inventory` — no changes to `registry.rs` or `main.rs` needed.

**Steps:**

1. Create your observer (can be in any module)
2. Implement the `Observer` trait
3. Add an `inventory::submit!` block with an `ObserverRegistration`

**Template:**

```rust
use std::time::Duration;
use crate::core::observer::{Observer, ObserverRegistration};
use crate::providers::LlmResponse;

pub struct MetricsObserver { /* ... */ }

impl Observer for MetricsObserver {
    fn on_llm_response(&self, response: &LlmResponse, duration: Duration) {
        // Record metrics
    }
}

inventory::submit! {
    ObserverRegistration {
        name: "metrics",
        build_fn: || Box::new(MetricsObserver { /* ... */ }),
    }
}
```

**That's it.** The `inventory::submit!` block causes automatic registration at startup. No changes to `registry.rs` or `main.rs`.

## Summary

| Trait | Registration | Config needed | main.rs change |
|-------|-------------|---------------|----------------|
| **Tool** | `inventory::submit!` | Optional (for enabled flag) | No |
| **Provider** | `inventory::submit!` | Yes (provider entry) | No |
| **Channel** | `inventory::submit!` | Yes (channel config) | No |
| **Memory** | `inventory::submit!` | Yes (backend choice) | No |
| **Observer** | `inventory::submit!` | No | No |
