use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing::{debug, info, warn};

use super::retry::with_retry;
use super::{
    ChatMessage, LlmResponse, Provider, ProviderConfig, RequestConfig, Role, StopReason,
    TokenUsage, ToolCall, ToolDef,
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
    crate::providers::ProviderRegistration {
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
        .map_err(anyhow::Error::from)?;

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
        cl100k_bpe()
            .map(|bpe| bpe.encode_ordinary(text).len())
            .unwrap_or_else(|| text.len() / 4) // fallback to heuristic
    }
}

/// Shared cl100k_base tokenizer, built once on first use.
///
/// Building the BPE takes hundreds of milliseconds; budget computation calls
/// `estimate_tokens` once per message, so it must not be rebuilt per call.
/// `None` is cached if construction fails (callers fall back to a heuristic).
fn cl100k_bpe() -> Option<&'static tiktoken_rs::CoreBPE> {
    static BPE: std::sync::OnceLock<Option<tiktoken_rs::CoreBPE>> = std::sync::OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
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
            let arguments = match serde_json::from_str(&tc.function.arguments) {
                Ok(args) => args,
                Err(e) => {
                    // Don't silently turn invalid JSON into `{}` — flag it so
                    // the tool registry returns an is_error result the model
                    // can learn from.
                    warn!(
                        tool = %tc.function.name,
                        error = %e,
                        raw_arguments = %tc.function.arguments,
                        "tool call arguments are not valid JSON"
                    );
                    crate::tools::malformed_arguments(&tc.function.arguments)
                }
            };
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
        stop_reason: map_finish_reason(choice.finish_reason.as_deref()),
    })
}

/// Map OpenAI's `finish_reason` strings to the provider-neutral enum.
fn map_finish_reason(reason: Option<&str>) -> StopReason {
    match reason {
        None | Some("stop") => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some("tool_calls") => StopReason::ToolUse,
        Some("content_filter") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_string()),
    }
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
        assert_eq!(result.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn test_parse_response_length_finish_reason() {
        let response = OpenAiResponse {
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: Some("Truncated mid-sen".into()),
                    tool_calls: vec![],
                },
                finish_reason: Some("length".into()),
            }],
            usage: None,
        };

        let result = parse_response(response).unwrap();
        assert_eq!(result.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn test_map_finish_reason() {
        assert_eq!(map_finish_reason(Some("stop")), StopReason::EndTurn);
        assert_eq!(map_finish_reason(Some("length")), StopReason::MaxTokens);
        assert_eq!(map_finish_reason(Some("tool_calls")), StopReason::ToolUse);
        assert_eq!(
            map_finish_reason(Some("content_filter")),
            StopReason::Refusal
        );
        assert_eq!(
            map_finish_reason(Some("function_call")),
            StopReason::Other("function_call".into())
        );
        assert_eq!(map_finish_reason(None), StopReason::EndTurn);
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
        assert_eq!(result.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn test_parse_response_malformed_tool_arguments_flagged() {
        // Local models sometimes emit invalid JSON for tool arguments. That
        // must not silently become `{}` — it is flagged via the malformed-args
        // sentinel so the registry turns it into an is_error tool result.
        let raw = r#"{"query": "unterminated"#;
        let response = OpenAiResponse {
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: None,
                    tool_calls: vec![WireToolCall {
                        id: "call_bad".into(),
                        function: WireFunction {
                            name: "memory_search".into(),
                            arguments: raw.into(),
                        },
                    }],
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: None,
        };

        let result = parse_response(response).unwrap();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "memory_search");
        assert_eq!(
            result.tool_calls[0].arguments[crate::tools::MALFORMED_ARGS_KEY],
            raw,
            "raw argument string must be preserved in the sentinel"
        );
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
    fn test_estimate_tokens_does_not_rebuild_bpe_per_call() {
        // Budget computation calls estimate_tokens once per message; rebuilding
        // the cl100k BPE (hundreds of ms) per call is pathological. With the
        // shared tokenizer, 50 calls after warmup take microseconds.
        let config = ProviderConfig {
            api_key: "test".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            timeout_secs: 60,
            max_retries: 3,
        };
        let provider = OpenAiProvider::new(config).unwrap();

        // Warm up (first call may build the tokenizer once).
        provider.estimate_tokens("warmup");

        let start = std::time::Instant::now();
        for _ in 0..50 {
            provider.estimate_tokens("the quick brown fox jumps over the lazy dog");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "50 estimate_tokens calls took {elapsed:?}; tokenizer is being rebuilt per call"
        );
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
    fn test_build_request_body_omits_temperature_when_none() {
        let config = ProviderConfig {
            api_key: "test".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            timeout_secs: 60,
            max_retries: 3,
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let messages = vec![ChatMessage {
            role: Role::User,
            content: "Hi".into(),
            tool_call_id: None,
            tool_calls: vec![],
        }];

        let req_config = RequestConfig {
            temperature: None,
            max_tokens: Some(1024),
            stop_sequences: vec![],
        };

        let body = provider.build_request_body(&messages, &[], &req_config);
        assert!(
            body.get("temperature").is_none(),
            "temperature must be omitted when None so the API default applies"
        );
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
