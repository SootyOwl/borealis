pub mod anthropic;
pub mod openai;
mod retry;

use anyhow::Result;
use serde::{Deserialize, Serialize};

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
