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
use crate::providers::registry::ProviderRegistration;

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

Channels are platform adapters (Discord, CLI, Telegram, etc.). Unlike tools and providers, channels require async task spawning at startup, so they use explicit registration functions rather than `inventory`.

**Steps:**

1. Create `src/channels/my_channel.rs`
2. Implement the `Channel` trait
3. Add a `pub fn register()` function
4. Add `pub mod my_channel;` to `src/channels/mod.rs`
5. Add the registration call in `src/main.rs`
6. Add config section to `src/config.rs` (in `ChannelsConfig`)

**Template:**

```rust
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use crate::channels::{Channel, ChannelRegistry};
use crate::config::Settings;
use crate::core::pipeline::PipelineRunner;

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

**Why not inventory?** Channel registration spawns async tasks (inbound, outbound, processing loops) which requires the tokio runtime and `Arc<dyn PipelineRunner>`. These don't exist at link time when `inventory` collects registrations.

## Adding a new Memory backend

Memory backends implement the `Memory` trait. The active backend is selected at startup in `main.rs`.

**Steps:**

1. Create `src/memory/my_backend.rs`
2. Implement the `Memory` trait (10 methods)
3. Add `pub mod my_backend;` to `src/memory/mod.rs`
4. Add selection logic in `main.rs`

The `Memory` trait is synchronous — callers wrap calls in `tokio::task::spawn_blocking`. If your backend needs async I/O, do the async work internally and block on it.

## Adding a new Observer

Observers receive lifecycle events from the pipeline. They implement the `Observer` trait with default no-op methods — override only the hooks you care about.

**Steps:**

1. Create your observer (can be in any module)
2. Implement the `Observer` trait
3. Register it in `src/providers/registry.rs` where `ObserverRegistry` is created

```rust
use crate::core::observer::Observer;

pub struct MetricsObserver { /* ... */ }

impl Observer for MetricsObserver {
    fn on_llm_response(&self, response: &LlmResponse, duration: Duration) {
        // Record metrics
    }
}
```

## Summary

| Trait | Registration | Config needed | main.rs change |
|-------|-------------|---------------|----------------|
| **Tool** | `inventory::submit!` | Optional (for enabled flag) | No |
| **Provider** | `inventory::submit!` | Yes (provider entry) | No |
| **Channel** | Explicit `register()` | Yes (channel config) | Yes (one line) |
| **Memory** | Manual selection | Yes (backend choice) | Yes |
| **Observer** | Manual registration | No | No |
