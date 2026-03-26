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

fn build_pipeline(
    config: ProviderConfig,
    sys_path: &Path,
    persona_path: &Path,
    deps: PipelineDeps,
) -> Result<Arc<dyn PipelineRunner>> {
    let provider = Arc::new(OpenAiProvider::new(config)?);
    let pipeline = Pipeline::new(provider, sys_path, persona_path, deps)?;
    Ok(Arc::new(pipeline))
}

inventory::submit! {
    crate::providers::registry::ProviderRegistration {
        name: "openai",
        build_pipeline_fn: build_pipeline,
    }
}

/// OpenAI-compatible provider (works with OpenAI, Ollama, and other compatible APIs).
pub struct OpenAiProvider {
    client: Client,
    config: ProviderConfig,
}

impl OpenAiProvider {
    pub fn new(config: ProviderConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build HTTP client for OpenAI provider")?;

        Ok(Self { client, config })
    }

    fn build_request_body(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        config: &RequestConfig,
    ) -> serde_json::Value {
        // OpenAI: system messages go inline in the messages array.
        let wire_messages: Vec<serde_json::Value> = messages.iter().map(message_to_wire).collect();

        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": wire_messages,
            "stream": false,
        });

        if let Some(temp) = config.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        if let Some(max) = config.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }

        if !config.stop_sequences.is_empty() {
            body["stop"] = serde_json::json!(config.stop_sequences);
        }

        if !tools.is_empty() {
            let wire_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(wire_tools);
        }

        body
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn chat(
        &self,
        messages: Vec<ChatMessage>,
        tools: &[ToolDef],
        config: &RequestConfig,
    ) -> Result<LlmResponse> {
        let body = self.build_request_body(&messages, tools, config);
        let endpoint = self.endpoint();

        debug!(provider = "openai", model = %self.config.model, "Sending chat request");

        let response = with_retry("openai", self.config.max_retries, || {
            let mut req = self
                .client
                .post(&endpoint)
                .header("content-type", "application/json");

            // Only add auth header if API key is non-empty (Ollama doesn't need one).
            if !self.config.api_key.is_empty() {
                req = req.header("authorization", format!("Bearer {}", self.config.api_key));
            }

            req.json(&body).send()
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        let response_body: OpenAiResponse = response
            .json()
            .await
            .context("Failed to parse OpenAI response")?;

        let llm_response = parse_response(response_body)?;

        info!(
            provider = "openai",
            input_tokens = llm_response.usage.input_tokens,
            output_tokens = llm_response.usage.output_tokens,
            tool_calls = llm_response.tool_calls.len(),
            "Chat request completed"
        );

        Ok(llm_response)
    }

    fn estimate_tokens(&self, text: &str) -> usize {
        // Use tiktoken-rs with cl100k_base for accurate OpenAI token counting.
        tiktoken_rs::cl100k_base()
            .map(|bpe| bpe.encode_ordinary(text).len())
            .unwrap_or_else(|_| text.len() / 4) // fallback to heuristic
    }
}

// --- Wire format types (OpenAI Chat Completions API) ---

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChoiceMessage,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Debug, Deserialize)]
struct WireToolCall {
    id: String,
    function: WireFunction,
}

#[derive(Debug, Deserialize)]
struct WireFunction {
    name: String,
    arguments: String, // OpenAI returns arguments as a JSON string
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

// --- Helpers ---

/// Convert an internal ChatMessage to OpenAI wire format.
fn message_to_wire(msg: &ChatMessage) -> serde_json::Value {
    let role = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    // Tool result messages
    if msg.role == Role::Tool {
        return serde_json::json!({
            "role": "tool",
            "content": msg.content,
            "tool_call_id": msg.tool_call_id,
        });
    }

    // Assistant messages with tool calls
    if msg.role == Role::Assistant && !msg.tool_calls.is_empty() {
        let wire_calls: Vec<serde_json::Value> = msg
            .tool_calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments.to_string(),
                    }
                })
            })
            .collect();

        let mut obj = serde_json::json!({
            "role": "assistant",
            "tool_calls": wire_calls,
        });

        if !msg.content.is_empty() {
            obj["content"] = serde_json::json!(msg.content);
        }

        return obj;
    }

    // Simple text message
    serde_json::json!({
        "role": role,
        "content": msg.content,
    })
}

/// Parse OpenAI's response into our internal LlmResponse.
fn parse_response(response: OpenAiResponse) -> Result<LlmResponse> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .context("OpenAI response contained no choices")?;

    let tool_calls: Vec<ToolCall> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tc| {
            let arguments: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}));
            ToolCall {
                id: tc.id,
                name: tc.function.name,
                arguments,
            }
        })
        .collect();

    let usage = response.usage.unwrap_or(OpenAiUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
    });

    Ok(LlmResponse {
        text: choice.message.content,
        tool_calls,
        usage: TokenUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_to_wire_system() {
        let msg = ChatMessage {
            role: Role::System,
            content: "You are helpful.".into(),
            tool_call_id: None,
            tool_calls: vec![],
        };

        let wire = message_to_wire(&msg);
        assert_eq!(wire["role"], "system");
        assert_eq!(wire["content"], "You are helpful.");
    }

    #[test]
    fn test_message_to_wire_user() {
        let msg = ChatMessage {
            role: Role::User,
            content: "Hello".into(),
            tool_call_id: None,
            tool_calls: vec![],
        };

        let wire = message_to_wire(&msg);
        assert_eq!(wire["role"], "user");
        assert_eq!(wire["content"], "Hello");
    }

    #[test]
    fn test_message_to_wire_tool_result() {
        let msg = ChatMessage {
            role: Role::Tool,
            content: "result data".into(),
            tool_call_id: Some("call_abc".into()),
            tool_calls: vec![],
        };

        let wire = message_to_wire(&msg);
        assert_eq!(wire["role"], "tool");
        assert_eq!(wire["tool_call_id"], "call_abc");
        assert_eq!(wire["content"], "result data");
    }

    #[test]
    fn test_message_to_wire_assistant_with_tool_calls() {
        let msg = ChatMessage {
            role: Role::Assistant,
            content: "".into(),
            tool_call_id: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "search".into(),
                arguments: serde_json::json!({"q": "test"}),
            }],
        };

        let wire = message_to_wire(&msg);
        assert_eq!(wire["role"], "assistant");
        let calls = wire["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "search");
    }

    #[test]
    fn test_parse_response_text_only() {
        let response = OpenAiResponse {
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: Some("Hello!".into()),
                    tool_calls: vec![],
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(OpenAiUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
            }),
        };

        let result = parse_response(response).unwrap();
        assert_eq!(result.text, Some("Hello!".to_string()));
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 5);
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let response = OpenAiResponse {
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: None,
                    tool_calls: vec![WireToolCall {
                        id: "call_1".into(),
                        function: WireFunction {
                            name: "memory_search".into(),
                            arguments: r#"{"query":"test"}"#.into(),
                        },
                    }],
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: Some(OpenAiUsage {
                prompt_tokens: 20,
                completion_tokens: 10,
            }),
        };

        let result = parse_response(response).unwrap();
        assert!(result.text.is_none());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "memory_search");
        assert_eq!(result.tool_calls[0].arguments["query"], "test");
    }

    #[test]
    fn test_parse_response_no_usage() {
        let response = OpenAiResponse {
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: Some("Hi".into()),
                    tool_calls: vec![],
                },
                finish_reason: Some("stop".into()),
            }],
            usage: None,
        };

        let result = parse_response(response).unwrap();
        assert_eq!(result.usage.input_tokens, 0);
        assert_eq!(result.usage.output_tokens, 0);
    }

    #[test]
    fn test_parse_response_empty_choices() {
        let response = OpenAiResponse {
            choices: vec![],
            usage: None,
        };

        let result = parse_response(response);
        assert!(result.is_err());
    }

    #[test]
    fn test_estimate_tokens() {
        let config = ProviderConfig {
            api_key: "test".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            timeout_secs: 60,
            max_retries: 3,
        };
        let provider = OpenAiProvider::new(config).unwrap();
        // tiktoken should give a reasonable estimate
        let tokens = provider.estimate_tokens("hello world");
        assert!(tokens > 0);
    }

    #[test]
    fn test_build_request_body() {
        let config = ProviderConfig {
            api_key: "test".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            timeout_secs: 60,
            max_retries: 3,
        };
        let provider = OpenAiProvider::new(config).unwrap();

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
            description: "A test".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let req_config = RequestConfig {
            temperature: Some(0.5),
            max_tokens: Some(2048),
            stop_sequences: vec!["STOP".into()],
        };

        let body = provider.build_request_body(&messages, &tools, &req_config);

        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], false);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["stop"][0], "STOP");

        // System message stays inline for OpenAI
        let wire_messages = body["messages"].as_array().unwrap();
        assert_eq!(wire_messages.len(), 2);
        assert_eq!(wire_messages[0]["role"], "system");

        let wire_tools = body["tools"].as_array().unwrap();
        assert_eq!(wire_tools.len(), 1);
        assert_eq!(wire_tools[0]["type"], "function");
        assert_eq!(wire_tools[0]["function"]["name"], "test_tool");
    }

    #[test]
    fn test_endpoint() {
        let config = ProviderConfig {
            api_key: "".into(),
            base_url: "http://localhost:11434/v1".into(),
            model: "llama3".into(),
            timeout_secs: 60,
            max_retries: 3,
        };
        let provider = OpenAiProvider::new(config).unwrap();
        assert_eq!(
            provider.endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn test_endpoint_trailing_slash() {
        let config = ProviderConfig {
            api_key: "".into(),
            base_url: "http://localhost:11434/v1/".into(),
            model: "llama3".into(),
            timeout_secs: 60,
            max_retries: 3,
        };
        let provider = OpenAiProvider::new(config).unwrap();
        assert_eq!(
            provider.endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );
    }
}
