use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq)]
pub enum ConversationIdError {
    #[error("invalid conversation id format: {0}")]
    InvalidFormat(String),
}

// ---------------------------------------------------------------------------
// ChannelSource
// ---------------------------------------------------------------------------

/// Identifies the source/target channel type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelSource {
    Discord,
    Cli,
    Scheduler,
    /// Legacy channel source for imported Letta/MemGPT conversations.
    Letta,
}

impl fmt::Display for ChannelSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discord => write!(f, "discord"),
            Self::Cli => write!(f, "cli"),
            Self::Scheduler => write!(f, "scheduler"),
            Self::Letta => write!(f, "letta"),
        }
    }
}

impl FromStr for ChannelSource {
    type Err = ConversationIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "discord" => Ok(Self::Discord),
            "cli" => Ok(Self::Cli),
            "scheduler" => Ok(Self::Scheduler),
            "letta" => Ok(Self::Letta),
            other => Err(ConversationIdError::InvalidFormat(format!(
                "unknown channel source: {other}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// ConversationId
// ---------------------------------------------------------------------------

/// Identifies a conversation for routing and history storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConversationId {
    Dm {
        channel_type: ChannelSource,
        user_id: String,
    },
    Group {
        channel_type: ChannelSource,
        group_id: String,
    },
    System {
        event_name: String,
    },
}

impl ConversationId {
    /// Parse from the canonical string representation.
    ///
    /// Formats:
    /// - `dm:<channel_source>:<user_id>`
    /// - `group:<channel_source>:<group_id>`
    /// - `system:<event_name>`
    pub fn parse(s: &str) -> Result<Self, ConversationIdError> {
        let parts: Vec<&str> = s.splitn(3, ':').collect();
        match parts.as_slice() {
            ["dm", channel_type, user_id] => Ok(ConversationId::Dm {
                channel_type: ChannelSource::from_str(channel_type)?,
                user_id: user_id.to_string(),
            }),
            ["group", channel_type, group_id] => Ok(ConversationId::Group {
                channel_type: ChannelSource::from_str(channel_type)?,
                group_id: group_id.to_string(),
            }),
            ["system", event_name] => Ok(ConversationId::System {
                event_name: event_name.to_string(),
            }),
            _ => Err(ConversationIdError::InvalidFormat(s.to_string())),
        }
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dm {
                channel_type,
                user_id,
            } => write!(f, "dm:{channel_type}:{user_id}"),
            Self::Group {
                channel_type,
                group_id,
            } => write!(f, "group:{channel_type}:{group_id}"),
            Self::System { event_name } => write!(f, "system:{event_name}"),
        }
    }
}

/// Unique message identifier (platform-specific).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

/// Information about a message author.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    pub id: String,
    pub display_name: String,
}

/// A message received from a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub author: Author,
    pub text: String,
    pub timestamp: DateTime<Utc>,
    /// Whether this message mentions Aurora (the bot).
    pub mentions_bot: bool,
}

/// Contextual information about where a message was received.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageContext {
    pub conversation_id: ConversationId,
    /// The raw platform channel/chat identifier (e.g., Discord channel ID).
    pub channel_id: String,
    /// Message this is a reply to, if any.
    pub reply_to: Option<MessageId>,
}

/// An inbound event from a channel adapter to the core.
#[derive(Debug, Clone)]
pub struct InEvent {
    pub source: ChannelSource,
    pub message: Message,
    pub context: MessageContext,
    /// Optional list of tool group names available for this event.
    /// When `None`, all enabled tool groups are available.
    pub tool_groups: Option<Vec<String>>,
}

/// An outbound event from the core to a channel adapter.
#[derive(Debug, Clone)]
pub struct OutEvent {
    pub target: ChannelSource,
    pub channel_id: String,
    pub text: Option<String>,
    pub reply_to: Option<MessageId>,
}
