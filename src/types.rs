use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq)]
pub enum ConversationIdError {
    #[error("invalid conversation id format: {0}")]
    InvalidFormat(String),
}

/// General parse error used for types other than ConversationId.
#[derive(Debug, Error, PartialEq)]
pub enum ParseError {
    #[error("unknown value: {0}")]
    UnknownValue(String),
}

// ---------------------------------------------------------------------------
// ConversationId
// ---------------------------------------------------------------------------

/// Identifies a conversation by its context type.
///
/// String formats:
/// - `dm:<channel_type>:<user_id>`
/// - `group:<channel_type>:<group_id>`
/// - `system:<event_name>`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConversationId {
    DM {
        channel_type: String,
        user_id: String,
    },
    Group {
        channel_type: String,
        group_id: String,
    },
    System {
        event_name: String,
    },
}

impl ConversationId {
    /// Parse from the canonical string representation.
    pub fn parse(s: &str) -> Result<Self, ConversationIdError> {
        let parts: Vec<&str> = s.splitn(3, ':').collect();
        match parts.as_slice() {
            ["dm", channel_type, user_id] => Ok(ConversationId::DM {
                channel_type: channel_type.to_string(),
                user_id: user_id.to_string(),
            }),
            ["group", channel_type, group_id] => Ok(ConversationId::Group {
                channel_type: channel_type.to_string(),
                group_id: group_id.to_string(),
            }),
            ["system", event_name] => Ok(ConversationId::System {
                event_name: event_name.to_string(),
            }),
            _ => Err(ConversationIdError::InvalidFormat(s.to_string())),
        }
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversationId::DM {
                channel_type,
                user_id,
            } => write!(f, "dm:{channel_type}:{user_id}"),
            ConversationId::Group {
                channel_type,
                group_id,
            } => write!(f, "group:{channel_type}:{group_id}"),
            ConversationId::System { event_name } => write!(f, "system:{event_name}"),
        }
    }
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
// ToolCall
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

// ---------------------------------------------------------------------------
// ChatMessage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: None,
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
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: None,
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

    // --- ConversationId serialization ---

    #[test]
    fn conversation_id_dm_to_string() {
        let id = ConversationId::DM {
            channel_type: "slack".to_string(),
            user_id: "U12345".to_string(),
        };
        assert_eq!(id.to_string(), "dm:slack:U12345");
    }

    #[test]
    fn conversation_id_group_to_string() {
        let id = ConversationId::Group {
            channel_type: "slack".to_string(),
            group_id: "G99999".to_string(),
        };
        assert_eq!(id.to_string(), "group:slack:G99999");
    }

    #[test]
    fn conversation_id_system_to_string() {
        let id = ConversationId::System {
            event_name: "startup".to_string(),
        };
        assert_eq!(id.to_string(), "system:startup");
    }

    // --- ConversationId roundtrip ---

    #[test]
    fn conversation_id_dm_roundtrip() {
        let id = ConversationId::DM {
            channel_type: "matrix".to_string(),
            user_id: "alice".to_string(),
        };
        assert_eq!(ConversationId::parse(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn conversation_id_group_roundtrip() {
        let id = ConversationId::Group {
            channel_type: "matrix".to_string(),
            group_id: "room-42".to_string(),
        };
        assert_eq!(ConversationId::parse(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn conversation_id_system_roundtrip() {
        let id = ConversationId::System {
            event_name: "heartbeat".to_string(),
        };
        assert_eq!(ConversationId::parse(&id.to_string()).unwrap(), id);
    }

    // --- ConversationId rejects invalid input ---

    #[test]
    fn conversation_id_parse_rejects_empty() {
        assert!(ConversationId::parse("").is_err());
    }

    #[test]
    fn conversation_id_parse_rejects_unknown_prefix() {
        assert!(ConversationId::parse("channel:foo:bar").is_err());
    }

    #[test]
    fn conversation_id_parse_rejects_dm_missing_user_id() {
        // "dm:slack" has only 2 parts when split on ':' with no third segment
        assert!(ConversationId::parse("dm:slack").is_err());
    }

    #[test]
    fn conversation_id_parse_rejects_system_missing_event() {
        assert!(ConversationId::parse("system").is_err());
    }

    // --- ChatMessage constructors ---

    #[test]
    fn chat_message_user_sets_correct_fields() {
        let msg = ChatMessage::user("hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "hello");
        assert!(msg.tool_calls.is_none());
        assert!(msg.tool_call_id.is_none());
    }

    #[test]
    fn chat_message_assistant_sets_correct_fields() {
        let msg = ChatMessage::assistant("hi there");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content, "hi there");
        assert!(msg.tool_calls.is_none());
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
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(msg.tool_calls.unwrap()[0], tc);
        assert!(msg.tool_call_id.is_none());
    }

    #[test]
    fn chat_message_tool_result_sets_correct_fields() {
        let msg = ChatMessage::tool_result("call_1", "result content");
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.content, "result content");
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
        assert!(msg.tool_calls.is_none());
    }

    #[test]
    fn chat_message_system_sets_correct_fields() {
        let msg = ChatMessage::system("you are a helpful assistant");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.content, "you are a helpful assistant");
        assert!(msg.tool_calls.is_none());
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
