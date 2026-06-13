use std::sync::Arc;

use dashmap::DashMap;
use tracing::{debug, info, warn};

use crate::config::CompactionConfig;
use crate::providers::{Provider, RequestConfig};
use crate::types::{ChatMessage, ConversationId};

use super::store::{CompactionSummary, HistoryStore, StoredMessage};

// ---------------------------------------------------------------------------
// CompactionState — per-conversation atomic flag
// ---------------------------------------------------------------------------

/// Tracks which conversations currently have a compaction task in flight.
#[derive(Debug, Default)]
pub struct CompactionState {
    in_progress: DashMap<String, ()>,
}

impl CompactionState {
    pub fn new() -> Self {
        Self {
            in_progress: DashMap::new(),
        }
    }

    /// Returns `true` if the flag was successfully set (no compaction in progress).
    fn try_start(&self, conversation_id: &str) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.in_progress.entry(conversation_id.to_string()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(e) => {
                e.insert(());
                true
            }
        }
    }

    fn finish(&self, conversation_id: &str) {
        self.in_progress.remove(conversation_id);
    }

    /// Check whether compaction is currently in progress for a conversation.
    pub fn is_compacting(&self, conversation_id: &str) -> bool {
        self.in_progress.contains_key(conversation_id)
    }
}

// ---------------------------------------------------------------------------
// CompactionService
// ---------------------------------------------------------------------------

/// Drives LLM-based conversation compaction.
///
/// Generic over `P: Provider` because the Provider trait uses `impl Future`
/// in return position, which prevents dyn dispatch.
///
/// When the history token count exceeds the configured threshold of the
/// available budget, a background task summarises older messages into a
/// compact summary that replaces them in prompt assembly.
pub struct CompactionService<P: Provider + 'static> {
    store: Arc<HistoryStore>,
    provider: Arc<P>,
    config: CompactionConfig,
    state: Arc<CompactionState>,
    compaction_prompt: String,
}

impl<P: Provider + 'static> CompactionService<P> {
    pub fn new(
        store: Arc<HistoryStore>,
        provider: Arc<P>,
        config: CompactionConfig,
        state: Arc<CompactionState>,
        compaction_prompt: String,
    ) -> Self {
        Self {
            store,
            provider,
            config,
            state,
            compaction_prompt,
        }
    }

    /// Check whether compaction should be triggered and, if so, spawn a
    /// background task. Returns `true` if a compaction task was spawned.
    ///
    /// `history_tokens` is the current total token estimate for the conversation.
    /// `history_budget` is the token budget available for history.
    pub fn maybe_trigger(
        &self,
        conversation_id: &ConversationId,
        history_tokens: usize,
        history_budget: usize,
    ) -> bool {
        if !self.config.enabled {
            return false;
        }

        let threshold_tokens = (history_budget as f64 * self.config.threshold) as usize;

        if history_tokens < threshold_tokens {
            return false;
        }

        let conv_key = conversation_id.to_string();
        if !self.state.try_start(&conv_key) {
            debug!(
                conversation = %conversation_id,
                "compaction already in progress, skipping"
            );
            return false;
        }

        let store = Arc::clone(&self.store);
        let provider = Arc::clone(&self.provider);
        let state = Arc::clone(&self.state);
        let prompt = self.compaction_prompt.clone();
        let conv_id = conversation_id.clone();

        tokio::spawn(async move {
            let conv_key = conv_id.to_string();
            match run_compaction(&store, &conv_id, &prompt, provider.as_ref()).await {
                Ok(()) => {
                    info!(conversation = %conv_id, "compaction completed successfully");
                }
                Err(e) => {
                    warn!(
                        conversation = %conv_id,
                        error = %e,
                        "compaction failed, will retry on next threshold crossing"
                    );
                }
            }
            state.finish(&conv_key);
        });

        true
    }
}

// ---------------------------------------------------------------------------
// Core compaction logic
// ---------------------------------------------------------------------------

/// Execute a single compaction pass for a conversation.
///
/// 1. Load existing summary (if any) + messages after it (one lock, consistent pair)
/// 2. Select messages to compact (up to midpoint, snapped to a turn boundary)
/// 3. Build summarization prompt: prior summary + selected messages
/// 4. Call the provider to produce a summary
/// 5. Atomically store the summary and delete compacted messages
///
/// All SQLite I/O runs on the blocking thread pool via `spawn_blocking`,
/// per the codebase convention (see `core::pipeline`).
async fn run_compaction<P: Provider>(
    store: &Arc<HistoryStore>,
    conversation_id: &ConversationId,
    compaction_prompt: &str,
    provider: &P,
) -> anyhow::Result<()> {
    // Load existing summary + the messages after its boundary in a single
    // lock acquisition so the pair is mutually consistent.
    let (existing_summary, messages) = {
        let store = Arc::clone(store);
        let conv_id = conversation_id.clone();
        tokio::task::spawn_blocking(move || store.load_summary_and_messages(&conv_id))
            .await
            .map_err(|e| anyhow::anyhow!("task join error: {e}"))??
    };

    if messages.len() < 2 {
        debug!(
            conversation = %conversation_id,
            "fewer than 2 messages, skipping compaction"
        );
        return Ok(());
    }

    // Select messages to compact: everything up to the midpoint, snapped to a
    // turn boundary so a turn (user msg + assistant tool_calls + tool results
    // + final assistant msg) is never split. Splitting a turn would leave
    // orphaned `role=tool` messages behind, which providers reject with a 400.
    let midpoint = messages.len() / 2;
    let end = snap_to_turn_boundary(&messages, midpoint);
    let to_compact = &messages[..end];

    debug_assert!(
        !to_compact.is_empty(),
        "snap_to_turn_boundary must not yield an empty compaction window"
    );

    let compaction_boundary_seq = to_compact.last().expect("non-empty slice").seq;

    // Build the user message content for the summarization LLM call
    let user_content = build_summarization_input(&existing_summary, to_compact);

    // Call the provider
    let provider_messages = vec![
        ChatMessage::system(compaction_prompt),
        ChatMessage::user(user_content),
    ];

    let request_config = RequestConfig {
        temperature: Some(0.3),
        max_tokens: Some(2048),
        stop_sequences: vec![],
    };

    let response = provider
        .chat(provider_messages, &[], &request_config)
        .await?;

    let summary_text = response.text.unwrap_or_default();

    if summary_text.is_empty() {
        anyhow::bail!("provider returned empty summary");
    }

    let token_estimate = provider.estimate_tokens(&summary_text);

    // Store the new summary (replaces any existing one — accumulation) and
    // delete the compacted messages in ONE transaction, so concurrent
    // prompt-assembly reads never observe one without the other.
    let deleted = {
        let store = Arc::clone(store);
        let conv_id = conversation_id.clone();
        let text = summary_text.clone();
        tokio::task::spawn_blocking(move || {
            store.commit_compaction(&conv_id, &text, compaction_boundary_seq, token_estimate)
        })
        .await
        .map_err(|e| anyhow::anyhow!("task join error: {e}"))??
    };
    info!(
        conversation = %conversation_id,
        deleted_messages = deleted,
        summary_tokens = token_estimate,
        compacted_up_to_seq = compaction_boundary_seq,
        "compaction stored summary and cleaned up messages"
    );

    Ok(())
}

/// Snap a tentative compaction end index (exclusive) to a turn boundary.
///
/// If `midpoint` already sits on a turn boundary it is returned unchanged.
/// Otherwise the boundary is shrunk to exclude the partially-covered turn
/// (preserving recent context); if that turn starts the window — so shrinking
/// would compact zero messages — the boundary is extended to the end of the
/// turn instead. The result never splits a turn and is never 0 while
/// `messages` is non-empty.
///
/// Assumes messages of a turn are contiguous in seq order, which is how the
/// pipeline appends them.
fn snap_to_turn_boundary(messages: &[StoredMessage], midpoint: usize) -> usize {
    debug_assert!(midpoint >= 1 && midpoint <= messages.len());
    let mut end = midpoint;
    let turn_id = messages[end - 1].turn_id.clone();

    // Already at a turn boundary?
    if messages.get(end).is_none_or(|m| m.turn_id != turn_id) {
        return end;
    }

    // Shrink: drop the partially-covered turn entirely.
    let turn_start = messages[..end]
        .iter()
        .rposition(|m| m.turn_id != turn_id)
        .map_or(0, |i| i + 1);
    if turn_start > 0 {
        return turn_start;
    }

    // The partial turn opens the window — shrinking would compact nothing,
    // so extend to the end of that turn instead.
    while end < messages.len() && messages[end].turn_id == turn_id {
        end += 1;
    }
    end
}

/// Format the conversation messages (and optional prior summary) into text
/// for the summarization LLM call.
fn build_summarization_input(
    existing_summary: &Option<CompactionSummary>,
    messages: &[StoredMessage],
) -> String {
    let mut parts = Vec::new();

    if let Some(summary) = existing_summary {
        parts.push(format!("## Previous Summary\n\n{}", summary.summary_text));
        parts.push(String::new()); // blank line separator
    }

    parts.push("## Conversation Messages\n".to_string());

    for msg in messages {
        let role_label = msg.role.as_str();
        parts.push(format!("[{role_label}]: {}", msg.content));
    }

    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::schema;
    use crate::providers::{LlmResponse, TokenUsage};
    use crate::types::{ChannelSource, ConversationMode, ToolDef};
    use anyhow::Result;
    use rusqlite::Connection;
    use std::sync::Mutex;

    fn make_store() -> Arc<HistoryStore> {
        let conn = Connection::open_in_memory().expect("in-memory db");
        schema::initialize(&conn).expect("schema init");
        Arc::new(HistoryStore::new(Arc::new(Mutex::new(conn))))
    }

    fn test_conv_id() -> ConversationId {
        ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "user1".to_string(),
        }
    }

    /// A mock provider that returns a fixed summary text.
    struct MockProvider {
        response_text: String,
    }

    impl MockProvider {
        fn new(text: &str) -> Self {
            Self {
                response_text: text.to_string(),
            }
        }
    }

    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn chat(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: &[ToolDef],
            _config: &RequestConfig,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                text: Some(self.response_text.clone()),
                tool_calls: vec![],
                usage: TokenUsage::default(),
                stop_reason: crate::providers::StopReason::EndTurn,
            })
        }

        fn estimate_tokens(&self, text: &str) -> usize {
            text.len() / 4
        }
    }

    // --- CompactionState ---

    #[test]
    fn compaction_state_try_start_and_finish() {
        let state = CompactionState::new();
        let key = "dm:cli:user1";

        assert!(!state.is_compacting(key));
        assert!(state.try_start(key));
        assert!(state.is_compacting(key));
        assert!(!state.try_start(key)); // already in progress
        state.finish(key);
        assert!(!state.is_compacting(key));
        assert!(state.try_start(key)); // can start again
    }

    // --- Summary CRUD ---

    #[test]
    fn save_and_load_summary() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        store
            .save_summary(&conv_id, "This is a summary.", 5, 10)
            .unwrap();

        let summary = store.load_summary(&conv_id).unwrap().expect("should exist");
        assert_eq!(summary.summary_text, "This is a summary.");
        assert_eq!(summary.compacted_up_to, 5);
        assert_eq!(summary.token_estimate, 10);
    }

    #[test]
    fn save_summary_replaces_previous() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        store
            .save_summary(&conv_id, "First summary.", 3, 5)
            .unwrap();
        store
            .save_summary(&conv_id, "Updated summary.", 8, 12)
            .unwrap();

        let summary = store.load_summary(&conv_id).unwrap().expect("should exist");
        assert_eq!(summary.summary_text, "Updated summary.");
        assert_eq!(summary.compacted_up_to, 8);
        assert_eq!(summary.token_estimate, 12);
    }

    #[test]
    fn load_summary_returns_none_when_absent() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        let summary = store.load_summary(&conv_id).unwrap();
        assert!(summary.is_none());
    }

    // --- commit_compaction (COMP-3, write side) ---

    #[test]
    fn commit_compaction_saves_summary_and_deletes_in_one_call() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        // Insert 4 messages (seq 1..=4).
        let t1 = store
            .append_message(&conv_id, &ChatMessage::user("msg1"), None)
            .unwrap();
        store
            .append_message(&conv_id, &ChatMessage::assistant("msg2"), Some(&t1))
            .unwrap();
        let t2 = store
            .append_message(&conv_id, &ChatMessage::user("msg3"), None)
            .unwrap();
        store
            .append_message(&conv_id, &ChatMessage::assistant("msg4"), Some(&t2))
            .unwrap();

        let deleted = store
            .commit_compaction(&conv_id, "Committed summary.", 2, 9)
            .unwrap();
        assert_eq!(deleted, 2);

        let summary = store.load_summary(&conv_id).unwrap().expect("summary");
        assert_eq!(summary.summary_text, "Committed summary.");
        assert_eq!(summary.compacted_up_to, 2);
        assert_eq!(summary.token_estimate, 9);

        let remaining = store.load_messages(&conv_id).unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].seq, 3);
    }

    #[test]
    fn commit_compaction_replaces_previous_summary() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        store
            .append_message(&conv_id, &ChatMessage::user("msg1"), None)
            .unwrap();
        store
            .append_message(&conv_id, &ChatMessage::user("msg2"), None)
            .unwrap();

        store
            .commit_compaction(&conv_id, "First.", 1, 3)
            .unwrap();
        store
            .commit_compaction(&conv_id, "Second.", 2, 5)
            .unwrap();

        let summary = store.load_summary(&conv_id).unwrap().expect("summary");
        assert_eq!(summary.summary_text, "Second.");
        assert_eq!(summary.compacted_up_to, 2);
        assert!(store.load_messages(&conv_id).unwrap().is_empty());
    }

    // --- load_summary_and_messages (COMP-3, read side) ---

    #[test]
    fn load_summary_and_messages_returns_consistent_pair() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        for i in 1..=4 {
            store
                .append_message(&conv_id, &ChatMessage::user(format!("msg{i}")), None)
                .unwrap();
        }
        store
            .commit_compaction(&conv_id, "Summary covers 1-2.", 2, 5)
            .unwrap();

        let (summary, messages) = store.load_summary_and_messages(&conv_id).unwrap();
        let summary = summary.expect("summary should exist");
        assert_eq!(summary.compacted_up_to, 2);
        assert_eq!(messages.len(), 2);
        assert!(
            messages.iter().all(|m| m.seq > summary.compacted_up_to),
            "every returned message must lie after the summary boundary"
        );
    }

    #[test]
    fn load_summary_and_messages_without_summary_returns_all() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        store
            .append_message(&conv_id, &ChatMessage::user("only msg"), None)
            .unwrap();

        let (summary, messages) = store.load_summary_and_messages(&conv_id).unwrap();
        assert!(summary.is_none());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "only msg");
    }


    // --- max_seq ---

    #[test]
    fn max_seq_returns_correct_value() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        assert_eq!(store.max_seq(&conv_id).unwrap(), None);

        store
            .append_message(&conv_id, &ChatMessage::user("first"), None)
            .unwrap();
        assert_eq!(store.max_seq(&conv_id).unwrap(), Some(1));

        store
            .append_message(&conv_id, &ChatMessage::user("second"), None)
            .unwrap();
        assert_eq!(store.max_seq(&conv_id).unwrap(), Some(2));
    }

    // --- snap_to_turn_boundary ---

    /// Build a `StoredMessage` carrying only the fields `snap_to_turn_boundary`
    /// inspects (`turn_id`); the rest are filler.
    fn msg(turn_id: &str, seq: i64) -> StoredMessage {
        StoredMessage {
            id: format!("m{seq}"),
            conversation_id: "test".into(),
            turn_id: turn_id.into(),
            seq,
            role: crate::types::Role::User,
            content: "x".into(),
            tool_call_id: None,
            tool_calls: vec![],
            token_estimate: 1,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn snap_midpoint_on_turn_boundary_returned_unchanged() {
        // [t1, t1 | t2, t2] — midpoint 2 already sits between two turns.
        let messages = vec![msg("t1", 1), msg("t1", 2), msg("t2", 3), msg("t2", 4)];
        assert_eq!(snap_to_turn_boundary(&messages, 2), 2);
    }

    #[test]
    fn snap_midturn_with_prior_turn_shrinks_to_turn_start() {
        // [t1, t1, t2, t2, t3] — midpoint 3 lands inside the t2 turn; a complete
        // t1 turn precedes it, so the window shrinks to the start of t2 (index 2).
        let messages = vec![
            msg("t1", 1),
            msg("t1", 2),
            msg("t2", 3),
            msg("t2", 4),
            msg("t3", 5),
        ];
        assert_eq!(snap_to_turn_boundary(&messages, 3), 2);
    }

    #[test]
    fn snap_midturn_opening_window_extends_to_turn_end() {
        // [t1, t1, t1, t2] — midpoint 2 lands inside t1, which opens the window;
        // shrinking would compact nothing, so it extends to the end of t1 (index 3).
        let messages = vec![msg("t1", 1), msg("t1", 2), msg("t1", 3), msg("t2", 4)];
        assert_eq!(snap_to_turn_boundary(&messages, 2), 3);
    }

    // --- build_summarization_input ---

    #[test]
    fn build_input_without_prior_summary() {
        let messages = vec![
            StoredMessage {
                id: "m1".into(),
                conversation_id: "test".into(),
                turn_id: "t1".into(),
                seq: 1,
                role: crate::types::Role::User,
                content: "Hello there".into(),
                tool_call_id: None,
                tool_calls: vec![],
                token_estimate: 5,
                created_at: "2026-01-01T00:00:00Z".into(),
            },
            StoredMessage {
                id: "m2".into(),
                conversation_id: "test".into(),
                turn_id: "t1".into(),
                seq: 2,
                role: crate::types::Role::Assistant,
                content: "Hi! How can I help?".into(),
                tool_call_id: None,
                tool_calls: vec![],
                token_estimate: 8,
                created_at: "2026-01-01T00:00:01Z".into(),
            },
        ];

        let input = build_summarization_input(&None, &messages);
        assert!(input.contains("[user]: Hello there"));
        assert!(input.contains("[assistant]: Hi! How can I help?"));
        assert!(!input.contains("Previous Summary"));
    }

    #[test]
    fn build_input_with_prior_summary() {
        let summary = CompactionSummary {
            conversation_id: "test".into(),
            summary_text: "Earlier, the user asked about Rust.".into(),
            compacted_up_to: 5,
            token_estimate: 10,
            created_at: "2026-01-01T00:00:00Z".into(),
        };

        let messages = vec![StoredMessage {
            id: "m6".into(),
            conversation_id: "test".into(),
            turn_id: "t3".into(),
            seq: 6,
            role: crate::types::Role::User,
            content: "What about lifetimes?".into(),
            tool_call_id: None,
            tool_calls: vec![],
            token_estimate: 5,
            created_at: "2026-01-01T00:01:00Z".into(),
        }];

        let input = build_summarization_input(&Some(summary), &messages);
        assert!(input.contains("## Previous Summary"));
        assert!(input.contains("Earlier, the user asked about Rust."));
        assert!(input.contains("[user]: What about lifetimes?"));
    }

    // --- run_compaction integration ---

    #[tokio::test]
    async fn run_compaction_produces_summary_and_deletes_old_messages() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        // Insert 6 messages across 3 turns
        for i in 1..=3 {
            let t = store
                .append_message(&conv_id, &ChatMessage::user(format!("user msg {i}")), None)
                .unwrap();
            store
                .append_message(
                    &conv_id,
                    &ChatMessage::assistant(format!("assistant msg {i}")),
                    Some(&t),
                )
                .unwrap();
        }

        let provider = MockProvider::new("Summary: users discussed topics 1-3.");
        let prompt = "Summarize the conversation.";

        run_compaction(&store, &conv_id, prompt, &provider)
            .await
            .expect("compaction should succeed");

        // Summary should exist
        let summary = store
            .load_summary(&conv_id)
            .unwrap()
            .expect("summary should be stored");
        assert_eq!(summary.summary_text, "Summary: users discussed topics 1-3.");
        // Midpoint of 6 messages = 3, which would split turn 2 — the boundary
        // snaps back to the end of turn 1 (seq 2).
        assert_eq!(summary.compacted_up_to, 2);

        // Only messages after the compaction point should remain
        let remaining = store.load_messages(&conv_id).unwrap();
        assert_eq!(
            remaining.len(),
            4,
            "4 messages should remain after turn-snapped compaction"
        );
        assert_eq!(remaining[0].seq, 3);
    }

    /// Append a 4-message tool-loop turn (user → assistant+tool_calls →
    /// tool result → final assistant) and return its turn_id.
    fn append_tool_loop_turn(store: &HistoryStore, conv_id: &ConversationId, label: &str) -> String {
        let t = store
            .append_message(conv_id, &ChatMessage::user(format!("{label} question")), None)
            .unwrap();
        let tc = crate::types::ToolCall {
            id: format!("call_{label}"),
            name: "search".to_string(),
            arguments: serde_json::json!({"q": label}),
        };
        store
            .append_message(
                conv_id,
                &ChatMessage::assistant_with_tool_calls(format!("{label} searching"), vec![tc]),
                Some(&t),
            )
            .unwrap();
        store
            .append_message(
                conv_id,
                &ChatMessage::tool_result(format!("call_{label}"), format!("{label} result")),
                Some(&t),
            )
            .unwrap();
        store
            .append_message(
                conv_id,
                &ChatMessage::assistant(format!("{label} answer")),
                Some(&t),
            )
            .unwrap();
        t
    }

    #[tokio::test]
    async fn compaction_boundary_never_splits_a_turn() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        // turn1: 2 messages (seq 1-2), turn2: 4-message tool loop (seq 3-6),
        // turn3: 2 messages (seq 7-8). Naive midpoint of 8 messages = 4, which
        // lands inside turn2 — right between the assistant tool_calls and the
        // tool result.
        let t1 = store
            .append_message(&conv_id, &ChatMessage::user("hi"), None)
            .unwrap();
        store
            .append_message(&conv_id, &ChatMessage::assistant("hello"), Some(&t1))
            .unwrap();
        let t2 = append_tool_loop_turn(&store, &conv_id, "loop");
        let t3 = store
            .append_message(&conv_id, &ChatMessage::user("bye"), None)
            .unwrap();
        store
            .append_message(&conv_id, &ChatMessage::assistant("bye!"), Some(&t3))
            .unwrap();

        let provider = MockProvider::new("Snapped summary.");
        run_compaction(&store, &conv_id, "Summarize.", &provider)
            .await
            .expect("compaction should succeed");

        let remaining = store.load_messages(&conv_id).unwrap();
        assert!(!remaining.is_empty(), "must not compact everything here");

        // The remaining messages must start at a turn boundary: the first
        // message is the user message that opens turn2 — never an orphaned
        // tool result or assistant continuation.
        assert_eq!(remaining[0].role, crate::types::Role::User);
        assert!(remaining[0].tool_call_id.is_none());
        assert_eq!(remaining[0].turn_id, t2);
        assert_eq!(remaining[0].seq, 3, "boundary snapped back to end of turn1");

        // Every remaining turn is fully intact (no partially-deleted turn).
        let loop_msgs: Vec<_> = remaining.iter().filter(|m| m.turn_id == t2).collect();
        assert_eq!(loop_msgs.len(), 4, "tool-loop turn must be intact");

        let summary = store.load_summary(&conv_id).unwrap().expect("summary");
        assert_eq!(summary.compacted_up_to, 2);
    }

    #[tokio::test]
    async fn compaction_extends_boundary_when_history_starts_mid_turn() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        // turn1: 4-message tool loop (seq 1-4), turn2: 2 messages (seq 5-6).
        // Naive midpoint of 6 messages = 3, inside turn1 — and turn1 starts
        // the window, so shrinking would compact zero messages. The boundary
        // must extend to the end of turn1 instead.
        let t1 = append_tool_loop_turn(&store, &conv_id, "first");
        let t2 = store
            .append_message(&conv_id, &ChatMessage::user("follow-up"), None)
            .unwrap();
        store
            .append_message(&conv_id, &ChatMessage::assistant("sure"), Some(&t2))
            .unwrap();
        let _ = t1;

        let provider = MockProvider::new("Extended summary.");
        run_compaction(&store, &conv_id, "Summarize.", &provider)
            .await
            .expect("compaction should succeed");

        let summary = store.load_summary(&conv_id).unwrap().expect("summary");
        assert_eq!(
            summary.compacted_up_to, 4,
            "boundary extended to the end of the tool-loop turn"
        );

        let remaining = store.load_messages(&conv_id).unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].seq, 5);
        assert_eq!(remaining[0].turn_id, t2);
        assert_eq!(remaining[0].role, crate::types::Role::User);
    }

    #[tokio::test]
    async fn successive_compaction_accumulates_summary() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        // Insert 4 messages
        for i in 1..=2 {
            let t = store
                .append_message(&conv_id, &ChatMessage::user(format!("user msg {i}")), None)
                .unwrap();
            store
                .append_message(
                    &conv_id,
                    &ChatMessage::assistant(format!("assistant msg {i}")),
                    Some(&t),
                )
                .unwrap();
        }

        // First compaction
        let provider = MockProvider::new("First summary.");
        run_compaction(&store, &conv_id, "Summarize.", &provider)
            .await
            .unwrap();

        let summary1 = store.load_summary(&conv_id).unwrap().unwrap();
        assert_eq!(summary1.summary_text, "First summary.");

        // Add more messages
        for i in 3..=4 {
            let t = store
                .append_message(&conv_id, &ChatMessage::user(format!("user msg {i}")), None)
                .unwrap();
            store
                .append_message(
                    &conv_id,
                    &ChatMessage::assistant(format!("assistant msg {i}")),
                    Some(&t),
                )
                .unwrap();
        }

        // Second compaction — should accumulate
        let provider2 = MockProvider::new("Accumulated summary of everything.");
        run_compaction(&store, &conv_id, "Summarize.", &provider2)
            .await
            .unwrap();

        let summary2 = store.load_summary(&conv_id).unwrap().unwrap();
        assert_eq!(summary2.summary_text, "Accumulated summary of everything.");
        // Only one summary should exist (replaced, not chained)
        assert!(summary2.compacted_up_to > summary1.compacted_up_to);
    }

    // --- maybe_trigger ---

    #[tokio::test]
    async fn maybe_trigger_spawns_when_threshold_exceeded() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        // Insert enough messages to exceed threshold
        for i in 1..=4 {
            let t = store
                .append_message(
                    &conv_id,
                    &ChatMessage::user(format!("message {i} with enough content to have tokens")),
                    None,
                )
                .unwrap();
            store
                .append_message(
                    &conv_id,
                    &ChatMessage::assistant(format!("reply {i} with enough content for tokens")),
                    Some(&t),
                )
                .unwrap();
        }

        let provider = Arc::new(MockProvider::new("Test summary."));
        let state = Arc::new(CompactionState::new());
        let config = CompactionConfig {
            enabled: true,
            threshold: 0.5, // 50% threshold for easier testing
            ..Default::default()
        };

        let service = CompactionService::new(
            Arc::clone(&store),
            provider,
            config,
            Arc::clone(&state),
            "Summarize.".to_string(),
        );

        let history_tokens = store.total_history_tokens(&conv_id).unwrap();
        // Set budget so tokens exceed threshold
        let history_budget = history_tokens + 10;

        let spawned = service.maybe_trigger(&conv_id, history_tokens, history_budget);
        assert!(spawned, "should have spawned a compaction task");

        // Wait for the background task to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Summary should now exist
        let summary = store.load_summary(&conv_id).unwrap();
        assert!(
            summary.is_some(),
            "summary should be stored after compaction"
        );
        assert!(!state.is_compacting(&conv_id.to_string()));
    }

    #[tokio::test]
    async fn maybe_trigger_skips_when_below_threshold() {
        let store = make_store();
        let conv_id = test_conv_id();
        let provider = Arc::new(MockProvider::new("Should not be called."));
        let state = Arc::new(CompactionState::new());
        let config = CompactionConfig::default();

        let service =
            CompactionService::new(store, provider, config, state, "Summarize.".to_string());

        // 100 tokens used, 1000 budget → 10% usage, well below 75% threshold
        let spawned = service.maybe_trigger(&conv_id, 100, 1000);
        assert!(!spawned);
    }

    #[tokio::test]
    async fn maybe_trigger_skips_when_disabled() {
        let store = make_store();
        let conv_id = test_conv_id();
        let provider = Arc::new(MockProvider::new("Should not be called."));
        let state = Arc::new(CompactionState::new());
        let config = CompactionConfig {
            enabled: false,
            ..Default::default()
        };

        let service =
            CompactionService::new(store, provider, config, state, "Summarize.".to_string());

        let spawned = service.maybe_trigger(&conv_id, 900, 1000);
        assert!(!spawned);
    }

    #[tokio::test]
    async fn maybe_trigger_skips_when_already_compacting() {
        let store = make_store();
        let conv_id = test_conv_id();
        let provider = Arc::new(MockProvider::new("Test."));
        let state = Arc::new(CompactionState::new());

        // Pre-mark as compacting
        state.try_start(&conv_id.to_string());

        let config = CompactionConfig {
            enabled: true,
            threshold: 0.5,
            ..Default::default()
        };

        let service =
            CompactionService::new(store, provider, config, state, "Summarize.".to_string());

        let spawned = service.maybe_trigger(&conv_id, 900, 1000);
        assert!(!spawned, "should not spawn when already compacting");
    }
}
