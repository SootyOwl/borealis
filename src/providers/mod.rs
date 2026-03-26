pub mod anthropic;
pub mod openai;
pub mod registry;
mod retry;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::pipeline::{PipelineDeps, PipelineRunner};

/// A self-registering provider factory.
///
/// Each provider module submits one of these via `inventory::submit!`. The
/// registry iterates them to construct the correct `PipelineRunner` without
/// hardcoded match arms.
pub struct ProviderRegistration {
    /// Provider name (must match the config key, e.g. "anthropic", "openai").
    pub name: &'static str,
    /// Build a `PipelineRunner` from resolved provider config and pipeline deps.
    pub build_pipeline_fn: fn(
        config: ProviderConfig,
        sys_path: &Path,
        persona_path: &Path,
        deps: PipelineDeps,
    ) -> Result<Arc<dyn PipelineRunner>>,
}

inventory::collect!(ProviderRegistration);

/// Role in a conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A message in a conversation, used as input to the provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Tool call ID this message is responding to (for Role::Tool messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls made by the assistant in this message.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
}

/// Describes a tool the LLM can call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A tool call returned by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Result of executing a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: serde_json::Value,
    pub is_error: bool,
}

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
