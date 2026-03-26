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
/// 1. Load existing summary (if any) + all messages
/// 2. Select messages to compact (up to midpoint of current history)
/// 3. Build summarization prompt: prior summary + selected messages
/// 4. Call the provider to produce a summary
/// 5. Store the summary and delete compacted messages
async fn run_compaction<P: Provider>(
    store: &HistoryStore,
    conversation_id: &ConversationId,
    compaction_prompt: &str,
    provider: &P,
) -> anyhow::Result<()> {
    // Load existing summary
    let existing_summary = store.load_summary(conversation_id)?;

    // Load messages — if we have a prior summary, only load messages after
    // the compaction point; otherwise load all.
    let messages = match &existing_summary {
        Some(summary) => store.load_messages_after(conversation_id, summary.compacted_up_to)?,
        None => store.load_messages(conversation_id)?,
    };

    if messages.len() < 2 {
        debug!(
            conversation = %conversation_id,
            "fewer than 2 messages, skipping compaction"
        );
        return Ok(());
    }

    // Select messages to compact: everything up to the midpoint.
    // This preserves recent context while compacting older messages.
    let midpoint = messages.len() / 2;
    let to_compact = &messages[..midpoint];

    if to_compact.is_empty() {
        return Ok(());
    }

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

    // Store the new summary (replaces any existing one — accumulation)
    store.save_summary(
        conversation_id,
        &summary_text,
        compaction_boundary_seq,
        token_estimate,
    )?;

    // Delete the compacted messages
    let deleted = store.delete_messages_up_to(conversation_id, compaction_boundary_seq)?;
    info!(
        conversation = %conversation_id,
        deleted_messages = deleted,
        summary_tokens = token_estimate,
        compacted_up_to_seq = compaction_boundary_seq,
        "compaction stored summary and cleaned up messages"
    );

    Ok(())
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

    // --- delete_messages_up_to ---

    #[test]
    fn delete_messages_up_to_removes_correct_messages() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        // Insert 4 messages (seq 1, 2, 3, 4)
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

        // Delete messages with seq <= 2
        let deleted = store.delete_messages_up_to(&conv_id, 2).unwrap();
        assert_eq!(deleted, 2);

        let remaining = store.load_messages(&conv_id).unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].content, "msg3");
        assert_eq!(remaining[1].content, "msg4");
    }

    // --- load_messages_after ---

    #[test]
    fn load_messages_after_returns_only_newer() {
        let store = make_store();
        let conv_id = test_conv_id();
        store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .unwrap();

        let t1 = store
            .append_message(&conv_id, &ChatMessage::user("old"), None)
            .unwrap();
        store
            .append_message(&conv_id, &ChatMessage::assistant("old reply"), Some(&t1))
            .unwrap();
        let t2 = store
            .append_message(&conv_id, &ChatMessage::user("new"), None)
            .unwrap();
        store
            .append_message(&conv_id, &ChatMessage::assistant("new reply"), Some(&t2))
            .unwrap();

        // Load only messages after seq 2
        let messages = store.load_messages_after(&conv_id, 2).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "new");
        assert_eq!(messages[1].content, "new reply");
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
        // Midpoint of 6 messages = 3, so compacted_up_to = seq 3
        assert_eq!(summary.compacted_up_to, 3);

        // Only messages after the compaction point should remain
        let remaining = store.load_messages(&conv_id).unwrap();
        assert_eq!(
            remaining.len(),
            3,
            "3 messages should remain after compaction"
        );
        assert_eq!(remaining[0].seq, 4);
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
