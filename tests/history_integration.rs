use std::sync::{Arc, Mutex};

use borealis::history::budget::{ContextBudget, Turn};
use borealis::history::schema;
use borealis::history::store::HistoryStore;
use borealis::types::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn setup() -> HistoryStore {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::initialize(&conn).unwrap();
    HistoryStore::new(Arc::new(Mutex::new(conn)))
}

/// Build Turn structs from stored messages grouped by turn_id.
fn build_turns(store: &HistoryStore, conv_id: &ConversationId) -> Vec<Turn> {
    let messages = store.load_messages(conv_id).unwrap();
    let turn_summaries = store.get_turns(conv_id).unwrap();
    turn_summaries
        .iter()
        .map(|ts| {
            let turn_messages: Vec<ChatMessage> = messages
                .iter()
                .filter(|m| m.turn_id == ts.turn_id)
                .map(|m| m.to_chat_message())
                .collect();
            Turn {
                turn_id: ts.turn_id.clone(),
                messages: turn_messages,
                total_tokens: ts.total_tokens,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Task 8: Integration tests
// ---------------------------------------------------------------------------

/// Store 2 turns (user+assistant each), select with a large budget, assemble.
/// Verify: system message first with persona, then 4 history messages in order.
#[test]
fn full_flow_store_and_assemble() {
    let store = setup();
    let conv_id = ConversationId::DM {
        channel_type: "cli".into(),
        user_id: "alice".into(),
    };
    store
        .ensure_conversation(&conv_id, ConversationMode::Pairing)
        .unwrap();

    // Turn 1
    let t1 = store
        .append_message(&conv_id, &ChatMessage::user("Hello!"), None)
        .unwrap();
    store
        .append_message(&conv_id, &ChatMessage::assistant("Hi there!"), Some(&t1))
        .unwrap();

    // Turn 2
    let t2 = store
        .append_message(&conv_id, &ChatMessage::user("How are you?"), None)
        .unwrap();
    store
        .append_message(&conv_id, &ChatMessage::assistant("Doing great!"), Some(&t2))
        .unwrap();

    let turns = build_turns(&store, &conv_id);
    assert_eq!(turns.len(), 2);

    // Large budget — all turns fit.
    let budget = ContextBudget::new(100_000, 500, 0, 0, 0);
    let result = budget.select_turns(&turns);
    assert!(result.evicted.is_empty());
    assert_eq!(result.included.len(), 2);

    let included_turns: Vec<Turn> = result.included.into_iter().cloned().collect();
    let prompt = budget.assemble(
        "You are Borealis.",
        "Helpful assistant.",
        &included_turns,
        &[],
    );

    // [0] system, [1..=4] history
    assert_eq!(prompt.len(), 5);
    assert_eq!(prompt[0].role, Role::System);
    assert!(prompt[0].content.contains("You are Borealis."));
    assert!(prompt[0].content.contains("Helpful assistant."));

    assert_eq!(prompt[1].role, Role::User);
    assert_eq!(prompt[1].content, "Hello!");
    assert_eq!(prompt[2].role, Role::Assistant);
    assert_eq!(prompt[2].content, "Hi there!");
    assert_eq!(prompt[3].role, Role::User);
    assert_eq!(prompt[3].content, "How are you?");
    assert_eq!(prompt[4].role, Role::Assistant);
    assert_eq!(prompt[4].content, "Doing great!");
}

/// Turn 1 is a tool loop (4 messages). Turn 2 is simple (2 messages).
/// A tight budget forces eviction of turn 1 as a whole unit.
/// Verify no orphaned tool_calls/results remain, and included turns have
/// valid message sequences.
#[test]
fn eviction_preserves_turn_integrity() {
    let store = setup();
    let conv_id = ConversationId::DM {
        channel_type: "cli".into(),
        user_id: "bob".into(),
    };
    store
        .ensure_conversation(&conv_id, ConversationMode::Pairing)
        .unwrap();

    // Turn 1: tool loop — user → assistant_with_tool_calls → tool_result → follow-up assistant
    let t1 = store
        .append_message(&conv_id, &ChatMessage::user("Search for something"), None)
        .unwrap();
    let tc = ToolCall {
        id: "call_1".to_string(),
        name: "search".to_string(),
        arguments: serde_json::json!({"query": "rust"}),
    };
    store
        .append_message(
            &conv_id,
            &ChatMessage::assistant_with_tool_calls("Searching…", vec![tc]),
            Some(&t1),
        )
        .unwrap();
    store
        .append_message(
            &conv_id,
            &ChatMessage::tool_result("call_1", "Found: Rust is awesome"),
            Some(&t1),
        )
        .unwrap();
    store
        .append_message(
            &conv_id,
            &ChatMessage::assistant("Rust is awesome!"),
            Some(&t1),
        )
        .unwrap();

    // Turn 2: simple exchange
    let t2 = store
        .append_message(&conv_id, &ChatMessage::user("Tell me more"), None)
        .unwrap();
    store
        .append_message(
            &conv_id,
            &ChatMessage::assistant("Sure, Rust is a systems language."),
            Some(&t2),
        )
        .unwrap();

    let turns = build_turns(&store, &conv_id);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].messages.len(), 4); // tool loop
    assert_eq!(turns[1].messages.len(), 2); // simple

    // Tight budget: turn 2 tokens fit, turn 1 does not.
    // We calculate exactly enough for turn 2 only.
    let turn2_tokens = turns[1].total_tokens;
    let budget = ContextBudget::new(turn2_tokens + 500, 500, 0, 0, 0);

    let result = budget.select_turns(&turns);

    // Turn 1 must be evicted as a whole unit.
    assert_eq!(result.evicted.len(), 1, "turn 1 should be evicted");
    assert_eq!(result.evicted[0], turns[0].turn_id);
    assert_eq!(result.included.len(), 1);
    assert_eq!(result.included[0].turn_id, turns[1].turn_id);

    // No tool_call or tool_result messages in the included turns.
    for msg in &result.included[0].messages {
        assert_ne!(msg.role, Role::Tool, "no orphaned tool results");
        assert!(
            msg.tool_calls.is_none(),
            "no orphaned tool_calls in included turns"
        );
    }

    // Included turn has a valid user → assistant sequence.
    assert_eq!(result.included[0].messages[0].role, Role::User);
    assert_eq!(result.included[0].messages[1].role, Role::Assistant);
}

/// Build 5 turns. Simulate 400-error recovery:
/// (a) Tight budget forces eviction of several turns.
/// (b) Second 400: assemble with just the last turn.
///     Verify only system + last turn's messages.
#[test]
fn four_hundred_recovery_drops_to_minimal() {
    let store = setup();
    let conv_id = ConversationId::DM {
        channel_type: "cli".into(),
        user_id: "carol".into(),
    };
    store
        .ensure_conversation(&conv_id, ConversationMode::Pairing)
        .unwrap();

    // Build 5 turns of user+assistant each.
    let mut turn_ids = Vec::new();
    for i in 0..5usize {
        let tid = store
            .append_message(&conv_id, &ChatMessage::user(format!("turn{i} user")), None)
            .unwrap();
        store
            .append_message(
                &conv_id,
                &ChatMessage::assistant(format!("turn{i} assistant")),
                Some(&tid),
            )
            .unwrap();
        turn_ids.push(tid);
    }

    let turns = build_turns(&store, &conv_id);
    assert_eq!(turns.len(), 5);

    // --- (a) Tight budget forces eviction of older turns ---
    // Budget only covers turns 4 and 5 (last two).
    let last_two_tokens: usize = turns[3..].iter().map(|t| t.total_tokens).sum();
    let budget_a = ContextBudget::new(last_two_tokens + 500, 500, 0, 0, 0);
    let result_a = budget_a.select_turns(&turns);

    assert!(
        !result_a.evicted.is_empty(),
        "some turns should be evicted under tight budget"
    );
    assert!(
        result_a.included.len() < turns.len(),
        "not all turns should be included"
    );

    // --- (b) Second 400: only the very last turn ---
    let last_turn = &turns[turns.len() - 1];
    let prompt = budget_a.assemble("SYS", "PERSONA", std::slice::from_ref(last_turn), &[]);

    // system + last turn's 2 messages = 3 total
    assert_eq!(prompt.len(), 3);
    assert_eq!(prompt[0].role, Role::System);
    assert_eq!(prompt[1].role, Role::User);
    assert_eq!(prompt[2].role, Role::Assistant);
    assert!(prompt[1].content.contains("turn4"));
}

/// 2 turns stored. Delete turn 1 via store.delete_turn.
/// Verify only turn 2's messages remain.
#[test]
fn delete_oldest_turns_from_store() {
    let store = setup();
    let conv_id = ConversationId::DM {
        channel_type: "cli".into(),
        user_id: "dave".into(),
    };
    store
        .ensure_conversation(&conv_id, ConversationMode::Shared)
        .unwrap();

    // Turn 1
    let t1 = store
        .append_message(&conv_id, &ChatMessage::user("first user"), None)
        .unwrap();
    store
        .append_message(
            &conv_id,
            &ChatMessage::assistant("first assistant"),
            Some(&t1),
        )
        .unwrap();

    // Turn 2
    let t2 = store
        .append_message(&conv_id, &ChatMessage::user("second user"), None)
        .unwrap();
    store
        .append_message(
            &conv_id,
            &ChatMessage::assistant("second assistant"),
            Some(&t2),
        )
        .unwrap();

    // Delete turn 1.
    let deleted = store.delete_turn(&conv_id, &t1).unwrap();
    assert_eq!(deleted, 2, "should have deleted 2 messages from turn 1");

    // Only turn 2 messages remain.
    let remaining = store.load_messages(&conv_id).unwrap();
    assert_eq!(remaining.len(), 2);
    for msg in &remaining {
        assert_eq!(msg.turn_id, t2, "remaining messages belong to turn 2");
    }
}

// ---------------------------------------------------------------------------
// Task 10: Async wrapper test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn async_store_operations() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::initialize(&conn).unwrap();
    let store = Arc::new(HistoryStore::new(Arc::new(Mutex::new(conn))));

    let conv_id = ConversationId::DM {
        channel_type: "cli".into(),
        user_id: "dev".into(),
    };

    let store_clone = store.clone();
    let conv_id_clone = conv_id.clone();
    tokio::task::spawn_blocking(move || {
        store_clone
            .ensure_conversation(&conv_id_clone, ConversationMode::Pairing)
            .unwrap();
        store_clone
            .append_message(&conv_id_clone, &ChatMessage::user("async hello"), None)
            .unwrap();
    })
    .await
    .unwrap();

    let store_clone = store.clone();
    let conv_id_clone = conv_id.clone();
    let messages =
        tokio::task::spawn_blocking(move || store_clone.load_messages(&conv_id_clone).unwrap())
            .await
            .unwrap();

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "async hello");
}
