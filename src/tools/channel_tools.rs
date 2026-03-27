use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::builder::{CreateAttachment, CreateMessage};

use crate::security::Sandbox;
use crate::tools::{
    DiscordHttpHandle, Tool, ToolContext, ToolDef, ToolDeps, ToolGroup, ToolRegistry, ToolResult,
    error_result, get_str, ok_result,
};

fn register(registry: &mut ToolRegistry, deps: &ToolDeps) {
    let root = deps.settings.tools.computer_use.sandbox_root.clone();
    let memory_dir = root.join("memory");
    let sandbox = Arc::new(Sandbox::with_memory_dir(root, memory_dir));
    register_channel_tools(registry, Arc::clone(&deps.discord_http), sandbox);
}

inventory::submit! {
    crate::tools::ToolRegistration {
        name: "channel",
        register_fn: register,
    }
}

/// Register channel tools (react, send_message, send_file) into the registry.
pub fn register_channel_tools(
    registry: &mut ToolRegistry,
    http: DiscordHttpHandle,
    sandbox: Arc<Sandbox>,
) {
    registry.register_with_group(
        React {
            http: Arc::clone(&http),
        },
        ToolGroup::Channel,
    );
    registry.register_with_group(
        SendMessage {
            http: Arc::clone(&http),
        },
        ToolGroup::Channel,
    );
    registry.register_with_group(
        SendFile { http, sandbox },
        ToolGroup::Channel,
    );
}

/// Wait for the Discord HTTP client to be available, returning an error if not connected.
async fn get_http(handle: &DiscordHttpHandle) -> Result<Arc<serenity::Http>, String> {
    // Poll with a short timeout — if Discord isn't connected yet, fail fast.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(http) = handle.get() {
            return Ok(Arc::clone(http));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Discord is not connected".to_string());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// react
// ---------------------------------------------------------------------------

struct React {
    http: DiscordHttpHandle,
}

impl Tool for React {
    fn name(&self) -> &str {
        "react"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "react".to_string(),
            description: "Add a reaction emoji to a Discord message.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel_id": {
                        "type": "string",
                        "description": "The Discord channel ID containing the message"
                    },
                    "message_id": {
                        "type": "string",
                        "description": "The Discord message ID to react to"
                    },
                    "emoji": {
                        "type": "string",
                        "description": "Unicode emoji character (e.g. \"\u{1f44d}\") or custom emoji in name:id format"
                    }
                },
                "required": ["channel_id", "message_id", "emoji"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;

        let channel_id_str = match get_str(&args, "channel_id") {
            Some(s) => s,
            None => return error_result(call_id, "missing required field: channel_id"),
        };
        let message_id_str = match get_str(&args, "message_id") {
            Some(s) => s,
            None => return error_result(call_id, "missing required field: message_id"),
        };
        let emoji_str = match get_str(&args, "emoji") {
            Some(s) => s,
            None => return error_result(call_id, "missing required field: emoji"),
        };

        let channel_id: u64 = match channel_id_str.parse() {
            Ok(id) => id,
            Err(_) => return error_result(call_id, "invalid channel_id: must be a numeric ID"),
        };
        let message_id: u64 = match message_id_str.parse() {
            Ok(id) => id,
            Err(_) => return error_result(call_id, "invalid message_id: must be a numeric ID"),
        };

        let http = match get_http(&self.http).await {
            Ok(h) => h,
            Err(e) => return error_result(call_id, &e),
        };

        let reaction = parse_emoji(emoji_str);
        let channel = serenity::ChannelId::new(channel_id);
        let msg_id = serenity::MessageId::new(message_id);

        match channel.create_reaction(&*http, msg_id, reaction).await {
            Ok(()) => ok_result(
                call_id,
                serde_json::json!({
                    "status": "reacted",
                    "emoji": emoji_str,
                }),
            ),
            Err(e) => error_result(call_id, &format!("failed to add reaction: {e}")),
        }
    }
}

/// Parse an emoji string into a ReactionType.
/// Supports unicode emoji (e.g. "👍") and custom emoji in "name:id" format.
fn parse_emoji(emoji: &str) -> serenity::ReactionType {
    if let Some((name, id_str)) = emoji.rsplit_once(':') {
        if let Ok(id) = id_str.parse::<u64>() {
            return serenity::ReactionType::Custom {
                animated: false,
                id: serenity::EmojiId::new(id),
                name: Some(name.to_string()),
            };
        }
    }
    serenity::ReactionType::Unicode(emoji.to_string())
}

// ---------------------------------------------------------------------------
// send_message
// ---------------------------------------------------------------------------

struct SendMessage {
    http: DiscordHttpHandle,
}

impl Tool for SendMessage {
    fn name(&self) -> &str {
        "send_message"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "send_message".to_string(),
            description: "Send a text message to a Discord channel.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel_id": {
                        "type": "string",
                        "description": "The Discord channel ID to send the message to"
                    },
                    "content": {
                        "type": "string",
                        "description": "The message text to send (max 2000 characters)"
                    }
                },
                "required": ["channel_id", "content"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;

        let channel_id_str = match get_str(&args, "channel_id") {
            Some(s) => s,
            None => return error_result(call_id, "missing required field: channel_id"),
        };
        let content = match get_str(&args, "content") {
            Some(s) => s,
            None => return error_result(call_id, "missing required field: content"),
        };

        let channel_id: u64 = match channel_id_str.parse() {
            Ok(id) => id,
            Err(_) => return error_result(call_id, "invalid channel_id: must be a numeric ID"),
        };

        if content.is_empty() {
            return error_result(call_id, "content must not be empty");
        }

        let http = match get_http(&self.http).await {
            Ok(h) => h,
            Err(e) => return error_result(call_id, &e),
        };

        let channel = serenity::ChannelId::new(channel_id);

        match channel.say(&*http, content).await {
            Ok(msg) => ok_result(
                call_id,
                serde_json::json!({
                    "status": "sent",
                    "message_id": msg.id.to_string(),
                }),
            ),
            Err(e) => error_result(call_id, &format!("failed to send message: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// send_file
// ---------------------------------------------------------------------------

struct SendFile {
    http: DiscordHttpHandle,
    sandbox: Arc<Sandbox>,
}

impl Tool for SendFile {
    fn name(&self) -> &str {
        "send_file"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "send_file".to_string(),
            description: "Send a file to a Discord channel, optionally with a text message."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel_id": {
                        "type": "string",
                        "description": "The Discord channel ID to send the file to"
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to upload (relative to sandbox root)"
                    },
                    "filename": {
                        "type": "string",
                        "description": "Display filename for the attachment (defaults to the file's basename)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Optional message text to accompany the file"
                    }
                },
                "required": ["channel_id", "file_path"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;

        let channel_id_str = match get_str(&args, "channel_id") {
            Some(s) => s,
            None => return error_result(call_id, "missing required field: channel_id"),
        };
        let file_path = match get_str(&args, "file_path") {
            Some(s) => s,
            None => return error_result(call_id, "missing required field: file_path"),
        };

        let channel_id: u64 = match channel_id_str.parse() {
            Ok(id) => id,
            Err(_) => return error_result(call_id, "invalid channel_id: must be a numeric ID"),
        };

        let path = std::path::Path::new(file_path);

        // Validate file path through sandbox before reading
        let canonical_path = match self.sandbox.validate_path(path) {
            Ok(p) => p,
            Err(e) => return error_result(call_id, &e.to_string()),
        };

        let filename = get_str(&args, "filename")
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                canonical_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            });
        if filename.is_empty() {
            return error_result(call_id, "file has no name component");
        }
        let content = get_str(&args, "content").unwrap_or("");

        let http = match get_http(&self.http).await {
            Ok(h) => h,
            Err(e) => return error_result(call_id, &e),
        };

        let attachment = match CreateAttachment::path(&canonical_path).await {
            Ok(mut a) => {
                a.filename = filename.clone();
                a
            }
            Err(e) => {
                return error_result(call_id, &format!("failed to read file: {e}"));
            }
        };

        let channel = serenity::ChannelId::new(channel_id);
        let builder = CreateMessage::new().content(content);

        match channel.send_files(&*http, [attachment], builder).await {
            Ok(msg) => ok_result(
                call_id,
                serde_json::json!({
                    "status": "sent",
                    "message_id": msg.id.to_string(),
                    "filename": filename,
                }),
            ),
            Err(e) => error_result(call_id, &format!("failed to send file: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unicode_emoji() {
        let result = parse_emoji("\u{1f44d}");
        assert!(matches!(result, serenity::ReactionType::Unicode(s) if s == "\u{1f44d}"));
    }

    #[test]
    fn parse_custom_emoji() {
        let result = parse_emoji("borealis:123456789");
        match result {
            serenity::ReactionType::Custom { id, name, .. } => {
                assert_eq!(id.get(), 123456789);
                assert_eq!(name, Some("borealis".to_string()));
            }
            _ => panic!("expected custom emoji"),
        }
    }

    #[test]
    fn register_channel_tools_adds_three() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let http: DiscordHttpHandle = Arc::new(tokio::sync::OnceCell::new());
        let sandbox = Arc::new(Sandbox::new(tmp.path().to_path_buf()));
        let mut registry = ToolRegistry::new();
        register_channel_tools(&mut registry, http, sandbox);

        assert_eq!(registry.tool_count(), 3);
        assert!(registry.has_tool("react"));
        assert!(registry.has_tool("send_message"));
        assert!(registry.has_tool("send_file"));
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            call_id: "test_call".to_string(),
            author_id: "test_user".to_string(),
            conversation_id: "test_conv".to_string(),
            channel_source: "discord".to_string(),
        }
    }

    #[tokio::test]
    async fn react_missing_fields() {
        let http: DiscordHttpHandle = Arc::new(tokio::sync::OnceCell::new());
        let tool = React { http };

        let result = tool
            .execute(serde_json::json!({}), &test_ctx())
            .await;
        assert!(result.is_error);
        assert!(result.content["error"].as_str().unwrap().contains("channel_id"));
    }

    #[tokio::test]
    async fn send_message_empty_content_rejected() {
        let http: DiscordHttpHandle = Arc::new(tokio::sync::OnceCell::new());
        let tool = SendMessage { http };

        let result = tool
            .execute(
                serde_json::json!({"channel_id": "123", "content": ""}),
                &test_ctx(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content["error"].as_str().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn send_message_invalid_channel_id() {
        let http: DiscordHttpHandle = Arc::new(tokio::sync::OnceCell::new());
        let tool = SendMessage { http };

        let result = tool
            .execute(
                serde_json::json!({"channel_id": "not-a-number", "content": "hello"}),
                &test_ctx(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content["error"].as_str().unwrap().contains("numeric"));
    }

    #[tokio::test]
    async fn send_file_rejects_traversal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let http: DiscordHttpHandle = Arc::new(tokio::sync::OnceCell::new());
        let sandbox = Arc::new(Sandbox::new(tmp.path().to_path_buf()));
        let tool = SendFile { http, sandbox };

        let result = tool
            .execute(
                serde_json::json!({"channel_id": "123", "file_path": "../../etc/passwd"}),
                &test_ctx(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content["error"].as_str().unwrap().contains("traversal"));
    }

    #[tokio::test]
    async fn send_file_rejects_memory_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("memory")).expect("mkdir");
        std::fs::write(tmp.path().join("memory/secret.md"), "secret").expect("write");
        let http: DiscordHttpHandle = Arc::new(tokio::sync::OnceCell::new());
        let memory_dir = tmp.path().join("memory");
        let sandbox = Arc::new(Sandbox::with_memory_dir(tmp.path().to_path_buf(), memory_dir));
        let tool = SendFile { http, sandbox };

        let result = tool
            .execute(
                serde_json::json!({"channel_id": "123", "file_path": "memory/secret.md"}),
                &test_ctx(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content["error"].as_str().unwrap().contains("memory directory blocked"));
    }
}
