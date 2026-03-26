use std::sync::Arc;

use reqwest::Client;

use crate::config::WebToolsConfig;
use crate::tools::{Tool, ToolContext, ToolDef, ToolDeps, ToolRegistry, ToolResult};

fn register(registry: &mut ToolRegistry, deps: &ToolDeps) {
    if !deps.settings.tools.web.enabled {
        return;
    }
    register_web_tools(registry, &deps.settings.tools.web);
}

inventory::submit! {
    crate::tools::ToolRegistration {
        name: "web",
        register_fn: register,
    }
}

/// Configuration shared by both web tools at runtime.
struct WebConfig {
    client: Client,
    api_key: Option<String>,
    max_fetch_bytes: usize,
}

/// Register web tools (`web_fetch`, `web_search`) into the given registry.
pub fn register_web_tools(registry: &mut ToolRegistry, config: &WebToolsConfig) {
    let api_key = config
        .jina_api_key_env
        .as_ref()
        .and_then(|env_name| std::env::var(env_name).ok());

    let shared = Arc::new(WebConfig {
        client: Client::new(),
        api_key,
        max_fetch_bytes: config.max_fetch_bytes,
    });

    registry.register(WebFetch(Arc::clone(&shared)));
    registry.register(WebSearch(shared));
}

fn error_result(call_id: &str, msg: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: serde_json::json!({ "error": msg }),
        is_error: true,
    }
}

fn ok_result(call_id: &str, value: serde_json::Value) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: value,
        is_error: false,
    }
}

// ---------------------------------------------------------------------------
// web_fetch — Jina Reader API (r.jina.ai)
// ---------------------------------------------------------------------------

struct WebFetch(Arc<WebConfig>);

impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_fetch".to_string(),
            description: "Fetch a web page and return its content as markdown.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;

        let url = match args.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => return error_result(call_id, "missing required field: url"),
        };

        // Validate URL scheme
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return error_result(call_id, "url must start with http:// or https://");
        }

        let jina_url = format!("https://r.jina.ai/{url}");

        let mut req = self
            .0
            .client
            .get(&jina_url)
            .header("Accept", "text/markdown");

        if let Some(ref key) = self.0.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => return error_result(call_id, &format!("request failed: {e}")),
        };

        let status = response.status();
        if !status.is_success() {
            return error_result(call_id, &format!("Jina Reader returned HTTP {status}"));
        }

        let body = match response.text().await {
            Ok(t) => t,
            Err(e) => return error_result(call_id, &format!("failed to read response: {e}")),
        };

        // Truncate to max_fetch_bytes to avoid blowing up the context window.
        let content = if body.len() > self.0.max_fetch_bytes {
            let mut end = self.0.max_fetch_bytes;
            // Avoid splitting a multi-byte UTF-8 character.
            while !body.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            format!(
                "{}\n\n[truncated — {}/{} bytes shown]",
                &body[..end],
                end,
                body.len()
            )
        } else {
            body
        };

        ok_result(
            call_id,
            serde_json::json!({
                "url": url,
                "content": content,
            }),
        )
    }
}

// ---------------------------------------------------------------------------
// web_search — Jina Search API (s.jina.ai)
// ---------------------------------------------------------------------------

struct WebSearch(Arc<WebConfig>);

impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_search".to_string(),
            description: "Search the web and return results with titles, URLs, and snippets."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;

        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => return error_result(call_id, "missing required field: query"),
        };

        let jina_url = format!("https://s.jina.ai/?q={}", urlencoded(query));

        let mut req = self
            .0
            .client
            .get(&jina_url)
            .header("Accept", "application/json");

        if let Some(ref key) = self.0.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => return error_result(call_id, &format!("request failed: {e}")),
        };

        let status = response.status();
        if !status.is_success() {
            return error_result(call_id, &format!("Jina Search returned HTTP {status}"));
        }

        let body = match response.text().await {
            Ok(t) => t,
            Err(e) => return error_result(call_id, &format!("failed to read response: {e}")),
        };

        // Parse the JSON response. Jina returns { "data": [...] } with result objects.
        let parsed: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return error_result(call_id, &format!("failed to parse search results: {e}"));
            }
        };

        ok_result(
            call_id,
            serde_json::json!({
                "query": query,
                "results": parsed.get("data").cloned().unwrap_or(parsed),
            }),
        )
    }
}

/// Minimal percent-encoding for query strings.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(HEX[(b >> 4) as usize]));
                out.push(char::from(HEX[(b & 0x0F) as usize]));
            }
        }
    }
    out
}

static HEX: [u8; 16] = *b"0123456789ABCDEF";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoded_basic() {
        assert_eq!(urlencoded("hello world"), "hello+world");
        assert_eq!(urlencoded("rust programming"), "rust+programming");
        assert_eq!(urlencoded("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn urlencoded_preserves_unreserved() {
        assert_eq!(urlencoded("abc-_.~123"), "abc-_.~123");
    }

    #[test]
    fn register_web_tools_adds_two_tools() {
        let config = WebToolsConfig::default();
        let mut registry = ToolRegistry::new();
        register_web_tools(&mut registry, &config);
        assert_eq!(registry.tool_count(), 2);
        assert!(registry.has_tool("web_fetch"));
        assert!(registry.has_tool("web_search"));
    }

    #[test]
    fn web_fetch_definition_has_url_param() {
        let config = WebToolsConfig::default();
        let shared = Arc::new(WebConfig {
            client: Client::new(),
            api_key: None,
            max_fetch_bytes: config.max_fetch_bytes,
        });
        let tool = WebFetch(shared);
        let def = tool.definition();
        assert_eq!(def.name, "web_fetch");
        let props = def.parameters.get("properties").unwrap();
        assert!(props.get("url").is_some());
    }

    #[test]
    fn web_search_definition_has_query_param() {
        let config = WebToolsConfig::default();
        let shared = Arc::new(WebConfig {
            client: Client::new(),
            api_key: None,
            max_fetch_bytes: config.max_fetch_bytes,
        });
        let tool = WebSearch(shared);
        let def = tool.definition();
        assert_eq!(def.name, "web_search");
        let props = def.parameters.get("properties").unwrap();
        assert!(props.get("query").is_some());
    }

    #[tokio::test]
    async fn web_fetch_rejects_missing_url() {
        let shared = Arc::new(WebConfig {
            client: Client::new(),
            api_key: None,
            max_fetch_bytes: 51200,
        });
        let tool = WebFetch(shared);
        let ctx = ToolContext {
            author_id: "user1".into(),
            conversation_id: "conv1".into(),
            channel_source: "cli".into(),
        };
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn web_fetch_rejects_bad_scheme() {
        let shared = Arc::new(WebConfig {
            client: Client::new(),
            api_key: None,
            max_fetch_bytes: 51200,
        });
        let tool = WebFetch(shared);
        let ctx = ToolContext {
            author_id: "user1".into(),
            conversation_id: "conv1".into(),
            channel_source: "cli".into(),
        };
        let result = tool
            .execute(serde_json::json!({"url": "ftp://example.com"}), &ctx)
            .await;
        assert!(result.is_error);
        let err = result.content.get("error").unwrap().as_str().unwrap();
        assert!(err.contains("http://"));
    }

    #[tokio::test]
    async fn web_search_rejects_missing_query() {
        let shared = Arc::new(WebConfig {
            client: Client::new(),
            api_key: None,
            max_fetch_bytes: 51200,
        });
        let tool = WebSearch(shared);
        let ctx = ToolContext {
            author_id: "user1".into(),
            conversation_id: "conv1".into(),
            channel_source: "cli".into(),
        };
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        assert!(result.is_error);
    }
}
