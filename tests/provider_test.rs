use borealis::providers::anthropic::AnthropicProvider;
use borealis::providers::openai::OpenAiProvider;
use borealis::providers::{ChatMessage, Provider, ProviderConfig, RequestConfig, Role, ToolDef};

fn make_config(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        api_key: "test-key".into(),
        base_url: base_url.into(),
        model: "test-model".into(),
        timeout_secs: 5,
        max_retries: 2,
    }
}

fn simple_messages() -> Vec<ChatMessage> {
    vec![
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
    ]
}

// --- Anthropic integration tests ---

#[tokio::test]
async fn anthropic_chat_success() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "content": [{"type": "text", "text": "Hello! How can I help?"}],
                "usage": {"input_tokens": 15, "output_tokens": 8},
                "stop_reason": "end_turn"
            })
            .to_string(),
        )
        .create_async()
        .await;

    let provider = AnthropicProvider::new(make_config(&server.url())).unwrap();
    let response = provider
        .chat(simple_messages(), &[], &RequestConfig::default())
        .await
        .unwrap();

    assert_eq!(response.text, Some("Hello! How can I help?".to_string()));
    assert!(response.tool_calls.is_empty());
    assert_eq!(response.usage.input_tokens, 15);
    assert_eq!(response.usage.output_tokens, 8);
    mock.assert_async().await;
}

#[tokio::test]
async fn anthropic_chat_with_tool_calls() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "content": [
                    {"type": "text", "text": "Let me search for that."},
                    {
                        "type": "tool_use",
                        "id": "toolu_01",
                        "name": "memory_search",
                        "input": {"query": "rust programming"}
                    }
                ],
                "usage": {"input_tokens": 25, "output_tokens": 20},
                "stop_reason": "tool_use"
            })
            .to_string(),
        )
        .create_async()
        .await;

    let tools = vec![ToolDef {
        name: "memory_search".into(),
        description: "Search memory".into(),
        parameters: serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    }];

    let provider = AnthropicProvider::new(make_config(&server.url())).unwrap();
    let response = provider
        .chat(simple_messages(), &tools, &RequestConfig::default())
        .await
        .unwrap();

    assert_eq!(response.text, Some("Let me search for that.".to_string()));
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "memory_search");
    assert_eq!(response.tool_calls[0].id, "toolu_01");
    mock.assert_async().await;
}

#[tokio::test]
async fn anthropic_retry_on_429() {
    let mut server = mockito::Server::new_async().await;

    // First request returns 429
    let rate_limit_mock = server
        .mock("POST", "/v1/messages")
        .with_status(429)
        .with_body(r#"{"error": {"type": "rate_limit_error", "message": "Rate limited"}}"#)
        .expect(1)
        .create_async()
        .await;

    // Second request succeeds
    let success_mock = server
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "content": [{"type": "text", "text": "Recovered!"}],
                "usage": {"input_tokens": 10, "output_tokens": 5},
                "stop_reason": "end_turn"
            })
            .to_string(),
        )
        .create_async()
        .await;

    let mut config = make_config(&server.url());
    config.max_retries = 2;
    let provider = AnthropicProvider::new(config).unwrap();
    let response = provider
        .chat(simple_messages(), &[], &RequestConfig::default())
        .await
        .unwrap();

    assert_eq!(response.text, Some("Recovered!".to_string()));
    rate_limit_mock.assert_async().await;
    success_mock.assert_async().await;
}

#[tokio::test]
async fn anthropic_exhausted_retries_returns_error() {
    let mut server = mockito::Server::new_async().await;

    // All requests return 500
    let _mock = server
        .mock("POST", "/v1/messages")
        .with_status(500)
        .with_body(r#"{"error": {"type": "server_error", "message": "Internal error"}}"#)
        .expect_at_least(2)
        .create_async()
        .await;

    let mut config = make_config(&server.url());
    config.max_retries = 1;
    let provider = AnthropicProvider::new(config).unwrap();
    let result = provider
        .chat(simple_messages(), &[], &RequestConfig::default())
        .await;

    assert!(result.is_err());
}

// --- OpenAI integration tests ---

#[tokio::test]
async fn openai_chat_success() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/chat/completions")
        .match_header("authorization", "Bearer test-key")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "choices": [{
                    "message": {"content": "Hello there!", "tool_calls": []},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 12, "completion_tokens": 6}
            })
            .to_string(),
        )
        .create_async()
        .await;

    let provider = OpenAiProvider::new(make_config(&server.url())).unwrap();
    let response = provider
        .chat(simple_messages(), &[], &RequestConfig::default())
        .await
        .unwrap();

    assert_eq!(response.text, Some("Hello there!".to_string()));
    assert!(response.tool_calls.is_empty());
    assert_eq!(response.usage.input_tokens, 12);
    assert_eq!(response.usage.output_tokens, 6);
    mock.assert_async().await;
}

#[tokio::test]
async fn openai_chat_with_tool_calls() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_abc",
                            "type": "function",
                            "function": {
                                "name": "memory_search",
                                "arguments": "{\"query\":\"rust\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 20, "completion_tokens": 10}
            })
            .to_string(),
        )
        .create_async()
        .await;

    let tools = vec![ToolDef {
        name: "memory_search".into(),
        description: "Search memory".into(),
        parameters: serde_json::json!({"type": "object"}),
    }];

    let provider = OpenAiProvider::new(make_config(&server.url())).unwrap();
    let response = provider
        .chat(simple_messages(), &tools, &RequestConfig::default())
        .await
        .unwrap();

    assert!(response.text.is_none());
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "memory_search");
    assert_eq!(response.tool_calls[0].arguments["query"], "rust");
    mock.assert_async().await;
}

#[tokio::test]
async fn openai_retry_on_500() {
    let mut server = mockito::Server::new_async().await;

    let error_mock = server
        .mock("POST", "/chat/completions")
        .with_status(500)
        .with_body(r#"{"error": {"message": "Server error"}}"#)
        .expect(1)
        .create_async()
        .await;

    let success_mock = server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "choices": [{
                    "message": {"content": "Back online!", "tool_calls": []},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            })
            .to_string(),
        )
        .create_async()
        .await;

    let mut config = make_config(&server.url());
    config.max_retries = 2;
    let provider = OpenAiProvider::new(config).unwrap();
    let response = provider
        .chat(simple_messages(), &[], &RequestConfig::default())
        .await
        .unwrap();

    assert_eq!(response.text, Some("Back online!".to_string()));
    error_mock.assert_async().await;
    success_mock.assert_async().await;
}

#[tokio::test]
async fn openai_no_auth_header_when_key_empty() {
    let mut server = mockito::Server::new_async().await;

    // This mock asserts NO authorization header is present
    let mock = server
        .mock("POST", "/chat/completions")
        .match_header("authorization", mockito::Matcher::Missing)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "choices": [{
                    "message": {"content": "Ollama response", "tool_calls": []},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 3}
            })
            .to_string(),
        )
        .create_async()
        .await;

    let config = ProviderConfig {
        api_key: "".into(),
        base_url: server.url(),
        model: "llama3".into(),
        timeout_secs: 5,
        max_retries: 0,
    };
    let provider = OpenAiProvider::new(config).unwrap();
    let response = provider
        .chat(simple_messages(), &[], &RequestConfig::default())
        .await
        .unwrap();

    assert_eq!(response.text, Some("Ollama response".to_string()));
    mock.assert_async().await;
}

#[tokio::test]
async fn openai_400_not_retried() {
    let mut server = mockito::Server::new_async().await;

    // 400 should NOT be retried
    let mock = server
        .mock("POST", "/chat/completions")
        .with_status(400)
        .with_body(r#"{"error": {"message": "Bad request"}}"#)
        .expect(1) // called exactly once, no retry
        .create_async()
        .await;

    let mut config = make_config(&server.url());
    config.max_retries = 3;
    let provider = OpenAiProvider::new(config).unwrap();
    let result = provider
        .chat(simple_messages(), &[], &RequestConfig::default())
        .await;

    assert!(result.is_err());
    mock.assert_async().await;
}
