//! Channel-specific action tools.
//!
//! Each channel can register tools that the LLM can call for platform-specific
//! actions (reactions, cross-channel messaging, etc.). These replace the old
//! XML directive system.

use crate::tools::{Tool, ToolContext, ToolDef, ToolGroup, ToolRegistry, ToolResult};

// ---------------------------------------------------------------------------
// Discord channel tools
// ---------------------------------------------------------------------------

/// Adds an emoji reaction to a message.
///
/// Stub implementation — the real Discord API call requires the serenity HTTP
/// client which is behind OnceCell in the Discord adapter.
pub struct ReactTool;

impl Tool for ReactTool {
    fn name(&self) -> &str {
        "react"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "react".to_string(),
            description: "Add an emoji reaction to the current message or a specific message."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "emoji": {
                        "type": "string",
                        "description": "The emoji to react with (e.g. 'heart', '👍', 'thumbsup')"
                    },
                    "message_id": {
                        "type": "string",
                        "description": "Optional message ID to react to. If omitted, reacts to the current message."
                    }
                },
                "required": ["emoji"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let emoji = args
            .get("emoji")
            .and_then(|v| v.as_str())
            .unwrap_or("👍");
        let message_id = args.get("message_id").and_then(|v| v.as_str());

        // Stub: in a real implementation, this would use the serenity HTTP client
        // to add the reaction via the Discord API.
        ToolResult {
            call_id: String::new(), // filled by registry
            content: serde_json::json!({
                "status": "ok",
                "emoji": emoji,
                "message_id": message_id,
                "note": "reaction queued (stub implementation)"
            }),
            is_error: false,
        }
    }
}

/// Sends a message to a named channel.
///
/// Stub implementation — the real Discord API call requires the serenity HTTP
/// client and channel ID resolution.
pub struct SendMessageTool;

impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "send_message".to_string(),
            description: "Send a message to a specific channel by name.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel_name": {
                        "type": "string",
                        "description": "The name of the channel to send to"
                    },
                    "text": {
                        "type": "string",
                        "description": "The message text to send"
                    }
                },
                "required": ["channel_name", "text"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let channel_name = args
            .get("channel_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Stub: in a real implementation, this would resolve the channel name
        // to an ID and send via the serenity HTTP client.
        ToolResult {
            call_id: String::new(),
            content: serde_json::json!({
                "status": "ok",
                "channel_name": channel_name,
                "text_length": text.len(),
                "note": "message queued (stub implementation)"
            }),
            is_error: false,
        }
    }
}

/// Register Discord-specific channel tools into the registry.
pub fn register_discord_channel_tools(registry: &mut ToolRegistry) {
    registry.register_with_group(ReactTool, ToolGroup::Channel);
    registry.register_with_group(SendMessageTool, ToolGroup::Channel);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn react_tool_definition() {
        let tool = ReactTool;
        assert_eq!(tool.name(), "react");
        let def = tool.definition();
        assert_eq!(def.name, "react");
        assert!(def.description.contains("reaction"));
    }

    #[test]
    fn send_message_tool_definition() {
        let tool = SendMessageTool;
        assert_eq!(tool.name(), "send_message");
        let def = tool.definition();
        assert_eq!(def.name, "send_message");
        assert!(def.description.contains("channel"));
    }

    #[tokio::test]
    async fn react_tool_returns_success() {
        let tool = ReactTool;
        let ctx = ToolContext {
            author_id: "user1".into(),
            conversation_id: "conv1".into(),
            channel_source: "discord".into(),
        };
        let result = tool
            .execute(serde_json::json!({"emoji": "heart"}), &ctx)
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content["emoji"], "heart");
    }

    #[tokio::test]
    async fn send_message_tool_returns_success() {
        let tool = SendMessageTool;
        let ctx = ToolContext {
            author_id: "user1".into(),
            conversation_id: "conv1".into(),
            channel_source: "discord".into(),
        };
        let result = tool
            .execute(
                serde_json::json!({"channel_name": "general", "text": "hello world"}),
                &ctx,
            )
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content["channel_name"], "general");
    }

    #[test]
    fn register_discord_channel_tools_adds_to_registry() {
        let mut registry = ToolRegistry::new();
        register_discord_channel_tools(&mut registry);

        assert!(registry.has_tool("react"));
        assert!(registry.has_tool("send_message"));
        assert_eq!(registry.tool_count(), 2);

        // Verify they are in the Channel group.
        let channel_defs = registry.definitions_for_groups(&[ToolGroup::Channel]);
        assert_eq!(channel_defs.len(), 2);

        // Verify they are NOT in other groups.
        let memory_defs = registry.definitions_for_groups(&[ToolGroup::Memory]);
        assert!(memory_defs.is_empty());
    }
}
