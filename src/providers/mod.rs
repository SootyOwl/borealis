pub mod anthropic;
pub mod openai;
pub mod registry;
pub mod retry;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::pipeline::{PipelineDeps, PipelineRunner};

// Re-export unified types so provider implementations can keep using
// `use super::{ChatMessage, Role, ToolCall, ToolDef, ToolResult, ...}`.
pub use crate::tools::{ToolCall, ToolDef, ToolResult};
pub use crate::types::{ChatMessage, Role};

/// Factory function that builds a `PipelineRunner` from config and deps.
pub type BuildPipelineFn = fn(
    config: ProviderConfig,
    sys_path: &Path,
    persona_path: &Path,
    deps: PipelineDeps,
) -> Result<Arc<dyn PipelineRunner>>;

/// A self-registering provider factory.
///
/// Each provider module submits one of these via `inventory::submit!`. The
/// registry iterates them to construct the correct `PipelineRunner` without
/// hardcoded match arms.
pub struct ProviderRegistration {
    /// Provider name (must match the config key, e.g. "anthropic", "openai").
    pub name: &'static str,
    /// Build a `PipelineRunner` from resolved provider config and pipeline deps.
    pub build_pipeline_fn: BuildPipelineFn,
}

inventory::collect!(ProviderRegistration);

/// Token usage reported by the provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Configuration for a single LLM request.
#[derive(Debug, Clone, Default)]
pub struct RequestConfig {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stop_sequences: Vec<String>,
}

/// Response from an LLM provider.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
}

/// Configuration for constructing a provider instance.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub timeout_secs: u64,
    pub max_retries: u32,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: String::new(),
            model: String::new(),
            timeout_secs: 60,
            max_retries: 3,
        }
    }
}

/// The shared provider trait. Rust 2024 edition supports async fn in traits natively.
pub trait Provider: Send + Sync {
    /// Human-readable name for this provider (e.g., "anthropic", "openai").
    fn name(&self) -> &str;

    /// Send a chat request and return the response.
    fn chat(
        &self,
        messages: Vec<ChatMessage>,
        tools: &[ToolDef],
        config: &RequestConfig,
    ) -> impl std::future::Future<Output = Result<LlmResponse>> + Send;

    /// Estimate token count for a string. Strategy varies by provider.
    fn estimate_tokens(&self, text: &str) -> usize;
}
