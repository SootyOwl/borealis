use std::str::FromStr;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rusqlite::params;
use tracing::warn;
use uuid::Uuid;

use crate::types::{
    ChatMessage, ConversationId, ConversationIdError, ConversationMode, ParseError, Role, ToolCall,
    estimate_tokens,
};
// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid data: {0}")]
    InvalidData(String),
    #[error("conversation id error: {0}")]
    ConversationId(#[from] ConversationIdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
}

// ---------------------------------------------------------------------------
// StoredMessage
// ---------------------------------------------------------------------------

pub struct StoredMessage {
    pub id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub seq: i64,
    pub role: Role,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub token_estimate: usize,
    pub created_at: String,
}

impl StoredMessage {
    pub fn to_chat_message(&self) -> ChatMessage {
        ChatMessage {
            role: self.role.clone(),
            content: self.content.clone(),
            tool_call_id: self.tool_call_id.clone(),
            tool_calls: self.tool_calls.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// HistoryStore
// ---------------------------------------------------------------------------

pub struct HistoryStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl HistoryStore {
    pub fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Acquire the database connection, recovering from mutex poisoning.
    ///
    /// A panic while holding the lock poisons the mutex, but the SQLite
    /// connection state itself is sound across a Rust panic (statement-level
    /// rollback applies), so we recover the guard rather than permanently
    /// bricking every store that shares this connection.
    fn lock_conn(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| {
            warn!("history store connection mutex was poisoned by a panic; recovering");
            poisoned.into_inner()
        })
    }

    /// INSERT OR UPDATE a conversation record (upsert).
    ///
    /// On conflict (same `id`), the `mode` and `last_active_at` fields are
    /// updated so the record reflects the latest state.
    pub fn ensure_conversation(
        &self,
        id: &ConversationId,
        mode: ConversationMode,
    ) -> Result<(), StoreError> {
        let conn = self.lock_conn();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO conversations (id, mode, created_at, last_active_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 mode           = excluded.mode,
                 last_active_at = excluded.last_active_at",
            params![id.to_string(), mode.as_str(), now],
        )?;
        Ok(())
    }

    /// Append a message to the conversation's history.
    ///
    /// - `turn_id = None`  → generate a new UUID (first message of a turn).
    /// - `turn_id = Some`  → reuse the supplied turn id.
    ///
    /// Returns the turn_id that was used (new or supplied).
    pub fn append_message(
        &self,
        conversation_id: &ConversationId,
        message: &ChatMessage,
        turn_id: Option<&str>,
    ) -> Result<String, StoreError> {
        self.append_message_at(conversation_id, message, turn_id, &Utc::now().to_rfc3339())
    }

    /// Append a message stamped with a caller-supplied `created_at`, instead of
    /// the current time.
    ///
    /// This exists for the Letta migration, which must preserve the original
    /// `msg.date` rather than stamping `Utc::now()`. The supplied timestamp is
    /// stored verbatim — callers are responsible for any normalization. Both
    /// the message row's `created_at` and the parent conversation's
    /// `last_active_at` bump use this value, so a migrated conversation's
    /// activity reflects the original message time.
    ///
    /// The regular [`append_message`](Self::append_message) delegates here with
    /// `Utc::now()`, so the two share one code path.
    pub fn append_message_at(
        &self,
        conversation_id: &ConversationId,
        message: &ChatMessage,
        turn_id: Option<&str>,
        created_at: &str,
    ) -> Result<String, StoreError> {
        let used_turn_id = match turn_id {
            Some(t) => t.to_string(),
            None => Uuid::new_v4().to_string(),
        };

        let tool_calls_json: Option<String> = if message.tool_calls.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(&message.tool_calls)
                    .map_err(|e| StoreError::InvalidData(e.to_string()))?,
            )
        };

        // Token estimate covers content + serialised tool calls (if any).
        let mut tokens = estimate_tokens(&message.content);
        if let Some(ref json) = tool_calls_json {
            tokens += estimate_tokens(json);
        }

        let msg_id = Uuid::new_v4().to_string();
        let now = created_at.to_string();
        let conv_id_str = conversation_id.to_string();

        let conn = self.lock_conn();

        // Monotonic sequence number per conversation for stable ordering.
        let next_seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE conversation_id = ?1",
            params![conv_id_str],
            |row| row.get(0),
        )?;

        conn.execute(
            "INSERT INTO messages
                 (id, conversation_id, turn_id, seq, role, content,
                  tool_call_id, tool_calls, token_estimate, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                msg_id,
                conv_id_str,
                used_turn_id,
                next_seq,
                message.role.as_str(),
                message.content,
                message.tool_call_id,
                tool_calls_json,
                tokens as i64,
                now,
            ],
        )?;

        // Bump last_active_at on the parent conversation.
        conn.execute(
            "UPDATE conversations SET last_active_at = ?1 WHERE id = ?2",
            params![now, conv_id_str],
        )?;

        Ok(used_turn_id)
    }

    /// Load all messages for a conversation, ordered by creation time (ASC).
    pub fn load_messages(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let conn = self.lock_conn();
        // seq starts at 1, so `after_seq = 0` returns every message.
        Self::query_messages_after(&conn, conversation_id, 0)
    }

    /// Query messages with seq > `after_seq` using an already-held connection.
    ///
    /// Shared by [`load_messages`](Self::load_messages) and
    /// [`load_summary_and_messages`](Self::load_summary_and_messages) so the
    /// combined read can run under a single lock acquisition.
    fn query_messages_after(
        conn: &rusqlite::Connection,
        conversation_id: &ConversationId,
        after_seq: i64,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, turn_id, seq, role, content,
                    tool_call_id, tool_calls, token_estimate, created_at
             FROM messages
             WHERE conversation_id = ?1 AND seq > ?2
             ORDER BY seq ASC",
        )?;

        let rows = stmt.query_map(params![conversation_id.to_string(), after_seq], |row| {
            let seq: i64 = row.get(3)?;
            let role_str: String = row.get(4)?;
            let tool_calls_json: Option<String> = row.get(7)?;
            let token_estimate_i64: i64 = row.get(8)?;

            Ok((
                row.get::<_, String>(0)?, // id
                row.get::<_, String>(1)?, // conversation_id
                row.get::<_, String>(2)?, // turn_id
                seq,
                role_str,
                row.get::<_, String>(5)?,         // content
                row.get::<_, Option<String>>(6)?, // tool_call_id
                tool_calls_json,
                token_estimate_i64,
                row.get::<_, String>(9)?, // created_at
            ))
        })?;

        let mut messages = Vec::new();
        for row_result in rows {
            let (
                id,
                conv_id,
                turn_id,
                seq,
                role_str,
                content,
                tool_call_id,
                tool_calls_json,
                token_i64,
                created_at,
            ) = row_result?;

            let role = Role::from_str(&role_str)?;

            let tool_calls: Vec<ToolCall> = match tool_calls_json {
                None => vec![],
                Some(json) => serde_json::from_str(&json)
                    .map_err(|e| StoreError::InvalidData(e.to_string()))?,
            };

            messages.push(StoredMessage {
                id,
                conversation_id: conv_id,
                turn_id,
                seq,
                role,
                content,
                tool_call_id,
                tool_calls,
                token_estimate: token_i64 as usize,
                created_at,
            });
        }

        Ok(messages)
    }
}

// ---------------------------------------------------------------------------
// TurnSummary
// ---------------------------------------------------------------------------

/// Summary of a turn for eviction decisions.
#[derive(Debug, Clone)]
pub struct TurnSummary {
    pub turn_id: String,
    pub total_tokens: usize,
    pub message_count: usize,
    pub earliest_created_at: String,
}

// ---------------------------------------------------------------------------
// Turn-based operations on HistoryStore
// ---------------------------------------------------------------------------

impl HistoryStore {
    /// Return one `TurnSummary` per distinct `turn_id` for the given
    /// conversation, ordered oldest-first by the earliest message timestamp.
    pub fn get_turns(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Vec<TurnSummary>, StoreError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT turn_id,
                    SUM(token_estimate),
                    COUNT(*),
                    MIN(created_at),
                    MIN(seq)
             FROM messages
             WHERE conversation_id = ?1
             GROUP BY turn_id
             ORDER BY MIN(seq) ASC",
        )?;

        let rows = stmt.query_map(params![conversation_id.to_string()], |row| {
            let total_tokens_i64: i64 = row.get(1)?;
            let message_count_i64: i64 = row.get(2)?;
            Ok(TurnSummary {
                turn_id: row.get(0)?,
                total_tokens: total_tokens_i64 as usize,
                message_count: message_count_i64 as usize,
                earliest_created_at: row.get(3)?,
            })
        })?;

        let mut summaries = Vec::new();
        for r in rows {
            summaries.push(r?);
        }
        Ok(summaries)
    }

    /// Delete every message that belongs to `turn_id` inside `conversation_id`.
    ///
    /// Returns the number of deleted rows.
    pub fn delete_turn(
        &self,
        conversation_id: &ConversationId,
        turn_id: &str,
    ) -> Result<usize, StoreError> {
        let conn = self.lock_conn();
        let deleted = conn.execute(
            "DELETE FROM messages WHERE conversation_id = ?1 AND turn_id = ?2",
            params![conversation_id.to_string(), turn_id],
        )?;
        Ok(deleted)
    }

    /// Return the sum of `token_estimate` for all messages in `conversation_id`.
    pub fn total_history_tokens(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<usize, StoreError> {
        let conn = self.lock_conn();
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(token_estimate), 0) FROM messages WHERE conversation_id = ?1",
            params![conversation_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(total as usize)
    }

}

// ---------------------------------------------------------------------------
// CompactionSummary
// ---------------------------------------------------------------------------

/// A stored compaction summary for a conversation.
#[derive(Debug, Clone)]
pub struct CompactionSummary {
    pub conversation_id: String,
    pub summary_text: String,
    /// All messages with seq <= this value have been compacted into the summary.
    pub compacted_up_to: i64,
    pub token_estimate: usize,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Compaction operations on HistoryStore
// ---------------------------------------------------------------------------

impl HistoryStore {
    /// Save or replace the compaction summary for a conversation.
    ///
    /// There is at most one active summary per conversation — successive
    /// compactions replace the previous summary (accumulation).
    pub fn save_summary(
        &self,
        conversation_id: &ConversationId,
        summary_text: &str,
        compacted_up_to: i64,
        token_estimate: usize,
    ) -> Result<(), StoreError> {
        let conn = self.lock_conn();
        Self::upsert_summary(
            &conn,
            conversation_id,
            summary_text,
            compacted_up_to,
            token_estimate,
        )
    }

    /// Upsert the summary row using an already-held connection.
    fn upsert_summary(
        conn: &rusqlite::Connection,
        conversation_id: &ConversationId,
        summary_text: &str,
        compacted_up_to: i64,
        token_estimate: usize,
    ) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO conversation_summaries
                 (conversation_id, summary_text, compacted_up_to, token_estimate, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(conversation_id) DO UPDATE SET
                 summary_text    = excluded.summary_text,
                 compacted_up_to = excluded.compacted_up_to,
                 token_estimate  = excluded.token_estimate,
                 created_at      = excluded.created_at",
            params![
                conversation_id.to_string(),
                summary_text,
                compacted_up_to,
                token_estimate as i64,
                now,
            ],
        )?;
        Ok(())
    }

    /// Load the compaction summary for a conversation, if one exists.
    pub fn load_summary(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<CompactionSummary>, StoreError> {
        let conn = self.lock_conn();
        Self::query_summary(&conn, conversation_id)
    }

    /// Query the summary row using an already-held connection.
    fn query_summary(
        conn: &rusqlite::Connection,
        conversation_id: &ConversationId,
    ) -> Result<Option<CompactionSummary>, StoreError> {
        let mut stmt = conn.prepare(
            "SELECT conversation_id, summary_text, compacted_up_to, token_estimate, created_at
             FROM conversation_summaries
             WHERE conversation_id = ?1",
        )?;
        let mut rows = stmt.query(params![conversation_id.to_string()])?;
        match rows.next()? {
            None => Ok(None),
            Some(row) => {
                let token_est: i64 = row.get(3)?;
                Ok(Some(CompactionSummary {
                    conversation_id: row.get(0)?,
                    summary_text: row.get(1)?,
                    compacted_up_to: row.get(2)?,
                    token_estimate: token_est as usize,
                    created_at: row.get(4)?,
                }))
            }
        }
    }

    /// Atomically commit a compaction pass: save (or replace) the summary AND
    /// delete the compacted messages in a single transaction under a single
    /// lock acquisition.
    ///
    /// Using one transaction prevents concurrent readers from observing the
    /// new summary without the matching deletion (or vice versa), which would
    /// silently drop or duplicate context in prompt assembly.
    ///
    /// Returns the number of deleted message rows.
    pub fn commit_compaction(
        &self,
        conversation_id: &ConversationId,
        summary_text: &str,
        compacted_up_to: i64,
        token_estimate: usize,
    ) -> Result<usize, StoreError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        Self::upsert_summary(
            &tx,
            conversation_id,
            summary_text,
            compacted_up_to,
            token_estimate,
        )?;
        let deleted = tx.execute(
            "DELETE FROM messages WHERE conversation_id = ?1 AND seq <= ?2",
            params![conversation_id.to_string(), compacted_up_to],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Load the compaction summary (if any) and the messages after its
    /// boundary in ONE lock acquisition, so the pair is always mutually
    /// consistent even while a concurrent compaction commits.
    pub fn load_summary_and_messages(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<(Option<CompactionSummary>, Vec<StoredMessage>), StoreError> {
        let conn = self.lock_conn();
        let summary = Self::query_summary(&conn, conversation_id)?;
        // seq starts at 1, so a missing summary (`after_seq = 0`) loads all.
        let after_seq = summary.as_ref().map_or(0, |s| s.compacted_up_to);
        let messages = Self::query_messages_after(&conn, conversation_id, after_seq)?;
        Ok((summary, messages))
    }

    /// Return the maximum seq number for a conversation, or None if empty.
    pub fn max_seq(&self, conversation_id: &ConversationId) -> Result<Option<i64>, StoreError> {
        let conn = self.lock_conn();
        let max: Option<i64> = conn.query_row(
            "SELECT MAX(seq) FROM messages WHERE conversation_id = ?1",
            params![conversation_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(max)
    }
}

// ---------------------------------------------------------------------------
// RecentConversation — summary of a recent conversation for the history tool
// ---------------------------------------------------------------------------

/// Summary of a recent conversation returned by `recent_conversations`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecentConversation {
    pub conversation_id: String,
    pub message_count: i64,
    pub earliest: String,
    pub latest: String,
    pub first_preview: String,
    pub last_preview: String,
    pub summary: Option<String>,
}

/// A single search hit from `search_messages`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageSearchResult {
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// History query operations on HistoryStore
// ---------------------------------------------------------------------------

impl HistoryStore {
    /// Return summaries of the most recent conversations that have messages
    /// within the last `hours` hours.  Optionally filter to conversations
    /// whose id contains `channel` (e.g. "discord", "cli").
    ///
    /// Limited to 10 conversations, ordered by most-recent message DESC.
    pub fn recent_conversations(
        &self,
        hours: u32,
        channel: Option<&str>,
    ) -> Result<Vec<RecentConversation>, StoreError> {
        let conn = self.lock_conn();
        let cutoff = Utc::now() - chrono::Duration::hours(i64::from(hours));
        let cutoff_str = cutoff.to_rfc3339();

        // Build channel filter dynamically, escaping LIKE wildcards.
        let channel_pattern = channel.map(|c| format!("%{}%", escape_like(c)));

        let sql = "\
            SELECT conversation_id,
                   COUNT(*) AS msg_count,
                   MIN(created_at) AS earliest,
                   MAX(created_at) AS latest
            FROM messages
            WHERE created_at >= ?1
              AND (?2 IS NULL OR conversation_id LIKE ?2 ESCAPE '\\')
            GROUP BY conversation_id
            ORDER BY MAX(created_at) DESC
            LIMIT 10";

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(
            params![cutoff_str, channel_pattern],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )?;

        let mut results = Vec::new();
        for row_result in rows {
            let (conv_id, msg_count, earliest, latest) = row_result?;

            // Fetch first message preview.
            let first_preview: String = conn
                .query_row(
                    "SELECT content FROM messages WHERE conversation_id = ?1 ORDER BY seq ASC LIMIT 1",
                    params![conv_id],
                    |row| row.get(0),
                )
                .unwrap_or_default();

            // Fetch last message preview.
            let last_preview: String = conn
                .query_row(
                    "SELECT content FROM messages WHERE conversation_id = ?1 ORDER BY seq DESC LIMIT 1",
                    params![conv_id],
                    |row| row.get(0),
                )
                .unwrap_or_default();

            // Check for compaction summary.
            let summary: Option<String> = conn
                .query_row(
                    "SELECT summary_text FROM conversation_summaries WHERE conversation_id = ?1",
                    params![conv_id],
                    |row| row.get(0),
                )
                .ok();

            results.push(RecentConversation {
                conversation_id: conv_id,
                message_count: msg_count,
                earliest,
                latest,
                first_preview: truncate_chars(&first_preview, 200),
                last_preview: truncate_chars(&last_preview, 200),
                summary,
            });
        }

        Ok(results)
    }

    /// Search message content across all conversations using SQL LIKE.
    ///
    /// Returns up to `limit` results ordered by created_at DESC.
    pub fn search_messages(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MessageSearchResult>, StoreError> {
        let conn = self.lock_conn();
        let pattern = format!("%{}%", escape_like(query));

        let mut stmt = conn.prepare(
            "SELECT conversation_id, role, content, created_at
             FROM messages
             WHERE content LIKE ?1 ESCAPE '\\'
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut results = Vec::new();
        for row_result in rows {
            let (conv_id, role, content, created_at) = row_result?;
            results.push(MessageSearchResult {
                conversation_id: conv_id,
                role,
                content: truncate_chars(&content, 200),
                created_at,
            });
        }

        Ok(results)
    }
}

/// Escape LIKE wildcards. Backslash MUST be escaped first, before % and _,
/// so the escaping backslashes it introduces are not themselves re-escaped.
/// Pairs with `ESCAPE '\'` in the query.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary, appending an
/// ellipsis when truncation occurs. Safe for multi-byte UTF-8 content.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() > max {
        let end = (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
        format!("{}…", &s[..end])
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::schema;
    use crate::types::ChannelSource;
    use rusqlite::Connection;
    use serde_json::json;

    fn make_store() -> HistoryStore {
        let conn = Connection::open_in_memory().expect("in-memory db");
        schema::initialize(&conn).expect("schema init");
        HistoryStore::new(Arc::new(Mutex::new(conn)))
    }

    fn test_conv_id() -> ConversationId {
        ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "U001".to_string(),
        }
    }

    // --- Task 4: Conversation CRUD ---

    #[test]
    fn ensure_conversation_is_idempotent() {
        let store = make_store();
        let id = test_conv_id();

        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .expect("first ensure");
        store
            .ensure_conversation(&id, ConversationMode::Pairing)
            .expect("second ensure");

        // Count rows — must still be exactly 1.
        let conn = store.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .expect("count query");
        assert_eq!(count, 1, "upsert should not duplicate rows");
    }

    // --- Task 5: Message and Turn Storage ---

    #[test]
    fn append_and_load_user_turn() {
        let store = make_store();
        let id = test_conv_id();
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();

        let msg = ChatMessage::user("Hello, Borealis!");
        let turn_id = store
            .append_message(&id, &msg, None)
            .expect("append should succeed");

        assert!(!turn_id.is_empty(), "turn_id should be a non-empty UUID");

        let messages = store.load_messages(&id).expect("load should succeed");
        assert_eq!(messages.len(), 1);

        let stored = &messages[0];
        assert_eq!(stored.role, Role::User);
        assert_eq!(stored.content, "Hello, Borealis!");
        assert_eq!(stored.turn_id, turn_id);
        assert!(stored.tool_call_id.is_none());
        assert!(stored.tool_calls.is_empty());
    }

    #[test]
    fn assistant_turn_groups_response_and_tool_calls() {
        let store = make_store();
        let id = test_conv_id();
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();

        // 1. User message — starts a new turn.
        let user_msg = ChatMessage::user("What's the weather?");
        let turn_id = store
            .append_message(&id, &user_msg, None)
            .expect("user append");

        // 2. Assistant reply with tool call — same turn.
        let tc = ToolCall {
            id: "call_weather_1".to_string(),
            name: "get_weather".to_string(),
            arguments: json!({"city": "Oslo"}),
        };
        let assistant_msg = ChatMessage::assistant_with_tool_calls("Checking…", vec![tc]);
        store
            .append_message(&id, &assistant_msg, Some(&turn_id))
            .expect("assistant append");

        // 3. Tool result — same turn.
        let tool_msg = ChatMessage::tool_result("call_weather_1", "Oslo: 5°C, cloudy");
        store
            .append_message(&id, &tool_msg, Some(&turn_id))
            .expect("tool result append");

        // 4. Assistant follow-up — same turn.
        let follow_up = ChatMessage::assistant("It's 5°C and cloudy in Oslo.");
        store
            .append_message(&id, &follow_up, Some(&turn_id))
            .expect("follow-up append");

        // All 4 messages must share the same turn_id.
        let messages = store.load_messages(&id).expect("load");
        assert_eq!(messages.len(), 4);
        for m in &messages {
            assert_eq!(
                m.turn_id, turn_id,
                "all messages should share the same turn_id"
            );
        }
    }

    #[test]
    fn load_messages_ordered_by_created_at() {
        let store = make_store();
        let id = test_conv_id();
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();

        // Insert three messages with a small delay between each so that the
        // RFC-3339 timestamps differ.  (sub-second precision means we insert
        // them quickly without sleeping — the ORDER BY is what we really care
        // about; we just verify the contents come back in insertion order.)
        let msgs = [
            ChatMessage::user("first"),
            ChatMessage::assistant("second"),
            ChatMessage::user("third"),
        ];

        // Use a single turn_id for simplicity.
        let turn_id = store.append_message(&id, &msgs[0], None).expect("append 1");
        store
            .append_message(&id, &msgs[1], Some(&turn_id))
            .expect("append 2");
        store
            .append_message(&id, &msgs[2], Some(&turn_id))
            .expect("append 3");

        let loaded = store.load_messages(&id).expect("load");
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].content, "first");
        assert_eq!(loaded[1].content, "second");
        assert_eq!(loaded[2].content, "third");
    }

    // --- Task 6: Turn-based operations ---

    #[test]
    fn get_turns_returns_ordered_turn_summaries() {
        let store = make_store();
        let id = test_conv_id();
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();

        // Turn 1: user + assistant
        let turn1 = store
            .append_message(&id, &ChatMessage::user("turn1 user"), None)
            .unwrap();
        store
            .append_message(
                &id,
                &ChatMessage::assistant("turn1 assistant"),
                Some(&turn1),
            )
            .unwrap();

        // Turn 2: user + assistant
        let turn2 = store
            .append_message(&id, &ChatMessage::user("turn2 user"), None)
            .unwrap();
        store
            .append_message(
                &id,
                &ChatMessage::assistant("turn2 assistant"),
                Some(&turn2),
            )
            .unwrap();

        let turns = store.get_turns(&id).expect("get_turns should succeed");
        assert_eq!(turns.len(), 2, "should have exactly 2 turns");

        // Oldest turn first
        assert_eq!(turns[0].turn_id, turn1);
        assert_eq!(turns[0].message_count, 2);
        assert!(turns[0].total_tokens > 0);

        assert_eq!(turns[1].turn_id, turn2);
        assert_eq!(turns[1].message_count, 2);
        assert!(turns[1].total_tokens > 0);
    }

    #[test]
    fn delete_turn_removes_all_messages_in_turn() {
        let store = make_store();
        let id = test_conv_id();
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();

        let turn1 = store
            .append_message(&id, &ChatMessage::user("first turn"), None)
            .unwrap();
        store
            .append_message(&id, &ChatMessage::assistant("first reply"), Some(&turn1))
            .unwrap();

        let turn2 = store
            .append_message(&id, &ChatMessage::user("second turn"), None)
            .unwrap();
        store
            .append_message(&id, &ChatMessage::assistant("second reply"), Some(&turn2))
            .unwrap();

        let deleted = store
            .delete_turn(&id, &turn1)
            .expect("delete_turn should succeed");
        assert_eq!(deleted, 2, "should have deleted 2 messages");

        let remaining = store.load_messages(&id).expect("load_messages");
        assert_eq!(remaining.len(), 2, "only turn2 messages should remain");
        for m in &remaining {
            assert_eq!(m.turn_id, turn2);
        }
    }

    #[test]
    fn total_history_tokens_sums_all_messages() {
        let store = make_store();
        let id = test_conv_id();
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();

        // "aaaa" → estimate_tokens = 1; "bbbbbbbb" → 2; total = 3
        let turn_id = store
            .append_message(&id, &ChatMessage::user("aaaa"), None)
            .unwrap();
        store
            .append_message(&id, &ChatMessage::assistant("bbbbbbbb"), Some(&turn_id))
            .unwrap();

        let total = store
            .total_history_tokens(&id)
            .expect("total_history_tokens should succeed");

        // Each message: estimate_tokens(content)
        let expected = estimate_tokens("aaaa") + estimate_tokens("bbbbbbbb");
        assert_eq!(total, expected);
    }

    // --- LIKE escaping (STORE-1) ---

    #[test]
    fn search_messages_matches_literal_backslash() {
        let store = make_store();
        let id = test_conv_id();
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();

        store
            .append_message(&id, &ChatMessage::user(r"the path is C:\Users\tyto"), None)
            .unwrap();
        // Decoy: same text minus the backslashes. If the backslash is not
        // escaped, `\U` in the pattern collapses to a literal `U` and this
        // message matches instead.
        store
            .append_message(&id, &ChatMessage::user("the path is C:Userstyto"), None)
            .unwrap();

        let results = store.search_messages(r"C:\Users", 10).unwrap();
        assert_eq!(results.len(), 1, "backslash should match literally");
        assert!(
            results[0].content.contains(r"C:\Users"),
            "matched the wrong message: {}",
            results[0].content
        );
    }

    #[test]
    fn recent_conversations_channel_filter_matches_literal_backslash() {
        let store = make_store();
        let id = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: r"alice\backslash".to_string(),
        };
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();
        store
            .append_message(&id, &ChatMessage::user("hi"), None)
            .unwrap();

        let results = store
            .recent_conversations(24, Some(r"alice\backslash"))
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "channel filter containing a backslash should match literally"
        );
    }

    // --- Mutex poisoning recovery (STORE-2) ---

    #[test]
    fn store_recovers_from_poisoned_mutex() {
        let conn = Arc::new(Mutex::new(
            Connection::open_in_memory().expect("in-memory db"),
        ));
        schema::initialize(&conn.lock().unwrap()).expect("schema init");
        let store = HistoryStore::new(Arc::clone(&conn));
        let id = test_conv_id();
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();

        // Poison the mutex by panicking while holding the lock.
        let poisoner = Arc::clone(&conn);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the shared connection mutex");
        })
        .join();
        assert!(conn.is_poisoned(), "mutex should be poisoned");

        // The store must keep working: SQLite connection state is sound
        // across a Rust panic (statement-level rollback applies).
        store
            .append_message(&id, &ChatMessage::user("after poison"), None)
            .expect("store should recover from a poisoned mutex");
        let messages = store.load_messages(&id).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "after poison");
    }

    // --- StoredMessage::to_chat_message ---

    #[test]
    fn stored_message_to_chat_message_round_trips() {
        let store = make_store();
        let id = test_conv_id();
        store
            .ensure_conversation(&id, ConversationMode::Shared)
            .unwrap();

        let original = ChatMessage::tool_result("call_42", "some result");
        let turn_id = store.append_message(&id, &original, None).expect("append");

        let loaded = store.load_messages(&id).expect("load");
        assert_eq!(loaded.len(), 1);
        let chat = loaded[0].to_chat_message();
        assert_eq!(chat.role, original.role);
        assert_eq!(chat.content, original.content);
        assert_eq!(chat.tool_call_id, original.tool_call_id);
        assert_eq!(chat.tool_calls, original.tool_calls);
        let _ = turn_id; // used above
    }
}
