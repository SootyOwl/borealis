use serde::{Deserialize, Serialize};
use std::str::FromStr;
use thiserror::Error;

// Re-export canonical types from their home modules.
pub use crate::core::event::{ChannelSource, ConversationId, ConversationIdError};
pub use crate::tools::{ToolCall, ToolDef, ToolResult};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// General parse error used for types like Role, ConversationMode.
#[derive(Debug, Error, PartialEq)]
pub enum ParseError {
    #[error("unknown value: {0}")]
    UnknownValue(String),
}

// ---------------------------------------------------------------------------
// Role
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "tool" => Ok(Role::Tool),
            "system" => Ok(Role::System),
            other => Err(ParseError::UnknownValue(other.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// ChatMessage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }

    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
}

// ---------------------------------------------------------------------------
// ConversationMode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationMode {
    Shared,
    Pairing,
}

impl ConversationMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConversationMode::Shared => "shared",
            ConversationMode::Pairing => "pairing",
        }
    }
}

impl std::fmt::Display for ConversationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ConversationMode {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "shared" => Ok(ConversationMode::Shared),
            "pairing" => Ok(ConversationMode::Pairing),
            other => Err(ParseError::UnknownValue(other.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Rough token estimate: 1 token per 4 characters.
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- ChatMessage constructors ---

    #[test]
    fn chat_message_user_sets_correct_fields() {
        let msg = ChatMessage::user("hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "hello");
        assert!(msg.tool_calls.is_empty());
        assert!(msg.tool_call_id.is_none());
    }

    #[test]
    fn chat_message_assistant_sets_correct_fields() {
        let msg = ChatMessage::assistant("hi there");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content, "hi there");
        assert!(msg.tool_calls.is_empty());
        assert!(msg.tool_call_id.is_none());
    }

    #[test]
    fn chat_message_assistant_with_tool_calls_sets_correct_fields() {
        let tc = ToolCall {
            id: "call_1".to_string(),
            name: "search".to_string(),
            arguments: serde_json::json!({"query": "rust"}),
        };
        let msg = ChatMessage::assistant_with_tool_calls("thinking", vec![tc.clone()]);
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0], tc);
        assert!(msg.tool_call_id.is_none());
    }

    #[test]
    fn chat_message_tool_result_sets_correct_fields() {
        let msg = ChatMessage::tool_result("call_1", "result content");
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.content, "result content");
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
        assert!(msg.tool_calls.is_empty());
    }

    #[test]
    fn chat_message_system_sets_correct_fields() {
        let msg = ChatMessage::system("you are a helpful assistant");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.content, "you are a helpful assistant");
        assert!(msg.tool_calls.is_empty());
        assert!(msg.tool_call_id.is_none());
    }

    // --- Role FromStr ---

    #[test]
    fn role_from_str_roundtrip() {
        for role in [Role::User, Role::Assistant, Role::Tool, Role::System] {
            assert_eq!(Role::from_str(role.as_str()).unwrap(), role);
        }
    }

    #[test]
    fn role_from_str_rejects_unknown() {
        assert!(Role::from_str("superuser").is_err());
    }

    // --- ConversationMode ---

    #[test]
    fn conversation_mode_from_str_roundtrip() {
        for mode in [ConversationMode::Shared, ConversationMode::Pairing] {
            assert_eq!(ConversationMode::from_str(mode.as_str()).unwrap(), mode);
        }
    }

    #[test]
    fn conversation_mode_from_str_rejects_unknown() {
        assert!(ConversationMode::from_str("broadcast").is_err());
    }

    // --- estimate_tokens ---

    #[test]
    fn estimate_tokens_heuristic() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        // 100 chars → 25 tokens
        let hundred = "a".repeat(100);
        assert_eq!(estimate_tokens(&hundred), 25);
    }
}
