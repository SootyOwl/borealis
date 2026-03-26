use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing::{debug, info};

use super::retry::with_retry;
use super::{
    ChatMessage, LlmResponse, Provider, ProviderConfig, RequestConfig, Role, TokenUsage, ToolCall,
    ToolDef,
};
use crate::core::pipeline::{Pipeline, PipelineDeps, PipelineRunner};

const ANTHROPIC_API_VERSION: &str = "2023-06-01";

fn build_pipeline(
    config: ProviderConfig,
    sys_path: &Path,
    persona_path: &Path,
    deps: PipelineDeps,
) -> Result<Arc<dyn PipelineRunner>> {
    let provider = Arc::new(AnthropicProvider::new(config)?);
    let pipeline = Pipeline::new(provider, sys_path, persona_path, deps)?;
    Ok(Arc::new(pipeline))
}

inventory::submit! {
    crate::providers::ProviderRegistration {
        name: "anthropic",
        build_pipeline_fn: build_pipeline,
    }
}

/// Anthropic native API provider (Claude models).
pub struct AnthropicProvider {
    client: Client,
    config: ProviderConfig,
}

impl AnthropicProvider {
    pub fn new(config: ProviderConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build HTTP client for Anthropic provider")?;

        Ok(Self { client, config })
    }

    fn build_request_body(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        config: &RequestConfig,
    ) -> serde_json::Value {
        // Anthropic: system prompt is a top-level field, not in the messages array.
        let (system_text, conversation_messages) = extract_system(messages);

        let wire_messages: Vec<serde_json::Value> = conversation_messages
            .iter()
            .map(|msg| message_to_wire(msg))
            .collect();

        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": wire_messages,
            "stream": false,
        });

        if let Some(system) = system_text {
            body["system"] = serde_json::json!(system);
        }

        if let Some(temp) = config.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        if let Some(max) = config.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        } else {
            // Anthropic requires max_tokens
            body["max_tokens"] = serde_json::json!(4096);
        }

        if !config.stop_sequences.is_empty() {
            body["stop_sequences"] = serde_json::json!(config.stop_sequences);
        }

        if !tools.is_empty() {
            let wire_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(wire_tools);
        }

        body
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        format!("{base}/v1/messages")
    }
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn chat(
        &self,
        messages: Vec<ChatMessage>,
        tools: &[ToolDef],
        config: &RequestConfig,
    ) -> Result<LlmResponse> {
        let body = self.build_request_body(&messages, tools, config);
        let endpoint = self.endpoint();

        debug!(provider = "anthropic", model = %self.config.model, "Sending chat request");

        let response = with_retry("anthropic", self.config.max_retries, || {
            self.client
                .post(&endpoint)
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", ANTHROPIC_API_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send()
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        let response_body: AnthropicResponse = response
            .json()
            .await
            .context("Failed to parse Anthropic response")?;

        let llm_response = parse_response(response_body)?;

        info!(
            provider = "anthropic",
            input_tokens = llm_response.usage.input_tokens,
            output_tokens = llm_response.usage.output_tokens,
            tool_calls = llm_response.tool_calls.len(),
            "Chat request completed"
        );

        Ok(llm_response)
    }

    fn estimate_tokens(&self, text: &str) -> usize {
        // Anthropic has no public tokenizer crate; chars/4 heuristic per design spec.
        text.len() / 4
    }
}

// --- Wire format types (Anthropic Messages API) ---

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    usage: AnthropicUsage,
    #[allow(dead_code)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// --- Helpers ---

/// Extract all system messages from the message list and concatenate them.
/// Anthropic expects system as a top-level field, not in messages.
fn extract_system(messages: &[ChatMessage]) -> (Option<String>, Vec<&ChatMessage>) {
    let mut system_parts = Vec::new();
    let mut rest = Vec::new();

    for msg in messages {
        if msg.role == Role::System {
            system_parts.push(msg.content.clone());
        } else {
            rest.push(msg);
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, rest)
}

/// Convert an internal ChatMessage to Anthropic wire format.
fn message_to_wire(msg: &ChatMessage) -> serde_json::Value {
    let role = match msg.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
        Role::System => unreachable!("system messages are extracted separately"),
    };

    // Tool result messages: wrap in tool_result content block
    if msg.role == Role::Tool {
        if let Some(ref call_id) = msg.tool_call_id {
            return serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": msg.content,
                }],
            });
        }
    }

    // Assistant messages with tool calls: include tool_use content blocks
    if msg.role == Role::Assistant && !msg.tool_calls.is_empty() {
        let mut content: Vec<serde_json::Value> = Vec::new();

        if !msg.content.is_empty() {
            content.push(serde_json::json!({
                "type": "text",
                "text": msg.content,
            }));
        }

        for tc in &msg.tool_calls {
            content.push(serde_json::json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.name,
                "input": tc.arguments,
            }));
        }

        return serde_json::json!({
            "role": "assistant",
            "content": content,
        });
    }

    // Simple text message
    serde_json::json!({
        "role": role,
        "content": msg.content,
    })
}

/// Parse Anthropic's response into our internal LlmResponse.
fn parse_response(response: AnthropicResponse) -> Result<LlmResponse> {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in response.content {
        match block {
            ContentBlock::Text { text } => text_parts.push(text),
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: input,
                });
            }
        }
    }

    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };

    Ok(LlmResponse {
        text,
        tool_calls,
        usage: TokenUsage {
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_system() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "You are helpful.".into(),
                tool_call_id: None,
                tool_calls: vec![],
            },
            ChatMessage {
                role: Role::User,
                content: "Hello".into(),
                tool_call_id: None,
                tool_calls: vec![],
            },
        ];

        let (system, rest) = extract_system(&messages);
        assert_eq!(system, Some("You are helpful.".to_string()));
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].content, "Hello");
    }

    #[test]
    fn test_extract_system_concatenates_multiple() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "You are helpful.".into(),
                tool_call_id: None,
                tool_calls: vec![],
            },
            ChatMessage {
                role: Role::System,
                content: "Remember: the user likes Rust.".into(),
                tool_call_id: None,
                tool_calls: vec![],
            },
            ChatMessage {
                role: Role::User,
                content: "Hello".into(),
                tool_call_id: None,
                tool_calls: vec![],
            },
        ];

        let (system, rest) = extract_system(&messages);
        assert_eq!(
            system,
            Some("You are helpful.\n\nRemember: the user likes Rust.".to_string())
        );
        assert_eq!(rest.len(), 1);
    }

    #[test]
    fn test_message_to_wire_user() {
        let msg = ChatMessage {
            role: Role::User,
            content: "Hi there".into(),
            tool_call_id: None,
            tool_calls: vec![],
        };

        let wire = message_to_wire(&msg);
        assert_eq!(wire["role"], "user");
        assert_eq!(wire["content"], "Hi there");
    }

    #[test]
    fn test_message_to_wire_tool_result() {
        let msg = ChatMessage {
            role: Role::Tool,
            content: r#"{"result": "ok"}"#.into(),
            tool_call_id: Some("call_123".into()),
            tool_calls: vec![],
        };

        let wire = message_to_wire(&msg);
        assert_eq!(wire["role"], "user");
        assert_eq!(wire["content"][0]["type"], "tool_result");
        assert_eq!(wire["content"][0]["tool_use_id"], "call_123");
    }

    #[test]
    fn test_message_to_wire_assistant_with_tool_calls() {
        let msg = ChatMessage {
            role: Role::Assistant,
            content: "Let me check.".into(),
            tool_call_id: None,
            tool_calls: vec![ToolCall {
                id: "tc_1".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({"query": "rust"}),
            }],
        };

        let wire = message_to_wire(&msg);
        assert_eq!(wire["role"], "assistant");
        let content = wire["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["name"], "memory_search");
    }

    #[test]
    fn test_parse_response_text_only() {
        let response = AnthropicResponse {
            content: vec![ContentBlock::Text {
                text: "Hello!".into(),
            }],
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
            stop_reason: Some("end_turn".into()),
        };

        let result = parse_response(response).unwrap();
        assert_eq!(result.text, Some("Hello!".to_string()));
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 5);
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let response = AnthropicResponse {
            content: vec![
                ContentBlock::Text {
                    text: "Searching...".into(),
                },
                ContentBlock::ToolUse {
                    id: "tu_1".into(),
                    name: "memory_search".into(),
                    input: serde_json::json!({"query": "test"}),
                },
            ],
            usage: AnthropicUsage {
                input_tokens: 20,
                output_tokens: 15,
            },
            stop_reason: Some("tool_use".into()),
        };

        let result = parse_response(response).unwrap();
        assert_eq!(result.text, Some("Searching...".to_string()));
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "memory_search");
    }

    #[test]
    fn test_estimate_tokens() {
        let config = ProviderConfig {
            api_key: "test".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-4-20250514".into(),
            timeout_secs: 60,
            max_retries: 3,
        };
        let provider = AnthropicProvider::new(config).unwrap();
        // "hello world" = 11 chars, 11/4 = 2
        assert_eq!(provider.estimate_tokens("hello world"), 2);
    }

    #[test]
    fn test_build_request_body() {
        let config = ProviderConfig {
            api_key: "test".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-4-20250514".into(),
            timeout_secs: 60,
            max_retries: 3,
        };
        let provider = AnthropicProvider::new(config).unwrap();

        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "System prompt".into(),
                tool_call_id: None,
                tool_calls: vec![],
            },
            ChatMessage {
                role: Role::User,
                content: "Hi".into(),
                tool_call_id: None,
                tool_calls: vec![],
            },
        ];

        let tools = vec![ToolDef {
            name: "test_tool".into(),
            description: "A test tool".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let req_config = RequestConfig {
            temperature: Some(0.7),
            max_tokens: Some(1024),
            stop_sequences: vec![],
        };

        let body = provider.build_request_body(&messages, &tools, &req_config);

        assert_eq!(body["system"], "System prompt");
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["stream"], false);
        assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 0.001);
        assert_eq!(body["max_tokens"], 1024);

        let wire_messages = body["messages"].as_array().unwrap();
        assert_eq!(wire_messages.len(), 1); // system extracted
        assert_eq!(wire_messages[0]["content"], "Hi");

        let wire_tools = body["tools"].as_array().unwrap();
        assert_eq!(wire_tools.len(), 1);
        assert_eq!(wire_tools[0]["name"], "test_tool");
        assert_eq!(wire_tools[0]["input_schema"]["type"], "object");
    }
}
