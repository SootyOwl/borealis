use std::sync::Arc;

use crate::history::store::HistoryStore;
use crate::tools::{Tool, ToolContext, ToolDef, ToolDeps, ToolGroup, ToolRegistry, ToolResult};

fn register(registry: &mut ToolRegistry, deps: &ToolDeps) {
    register_history_tools(registry, Arc::clone(&deps.history_store));
}

inventory::submit! {
    crate::tools::ToolRegistration {
        name: "history",
        register_fn: register,
    }
}

/// Register history tools into the given registry under the Memory group.
pub fn register_history_tools(registry: &mut ToolRegistry, store: Arc<HistoryStore>) {
    registry.register_with_group(HistoryRecent(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(HistorySearch(store), ToolGroup::Memory);
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

// --- history_recent ---

struct HistoryRecent(Arc<HistoryStore>);

impl Tool for HistoryRecent {
    fn name(&self) -> &str {
        "history_recent"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "history_recent".to_string(),
            description:
                "Return a summary of recent conversations, grouped by conversation ID."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "hours": {
                        "type": "integer",
                        "description": "How far back to look in hours (default: 24)"
                    },
                    "channel": {
                        "type": "string",
                        "description": "Filter to a specific channel (e.g. 'discord', 'cli')"
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let hours = args
            .get("hours")
            .and_then(|v| v.as_u64())
            .unwrap_or(24) as u32;
        let channel = args.get("channel").and_then(|v| v.as_str()).map(String::from);

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || {
            store.recent_conversations(hours, channel.as_deref())
        })
        .await
        {
            Ok(Ok(convos)) => ok_result(call_id, serde_json::to_value(convos).unwrap()),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- history_search ---

struct HistorySearch(Arc<HistoryStore>);

impl Tool for HistorySearch {
    fn name(&self) -> &str {
        "history_search"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "history_search".to_string(),
            description: "Search message content across all conversations.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search string to match against message content"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results (default: 10)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) => q.to_string(),
            None => return error_result(call_id, "missing required field: query"),
        };
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.search_messages(&query, limit)).await {
            Ok(Ok(results)) => ok_result(call_id, serde_json::to_value(results).unwrap()),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}
