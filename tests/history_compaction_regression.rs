use borealis::history::{schema, store::HistoryStore};
use borealis::types::{ChannelSource, ChatMessage, ConversationId, ConversationMode};
use std::sync::{Arc, Mutex};

#[test]
fn appended_message_remains_visible_after_compacting_all_history() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::initialize(&conn).unwrap();
    let store = HistoryStore::new(Arc::new(Mutex::new(conn)));
    let id = ConversationId::Dm {
        channel_type: ChannelSource::Cli,
        user_id: "compaction-regression".into(),
    };
    store
        .ensure_conversation(&id, ConversationMode::Pairing)
        .unwrap();
    store
        .append_message(&id, &ChatMessage::user("old"), None)
        .unwrap();
    store
        .commit_compaction(&id, "old conversation", 1, 3)
        .unwrap();
    store
        .append_message(&id, &ChatMessage::user("new"), None)
        .unwrap();

    let (summary, messages) = store.load_summary_and_messages(&id).unwrap();
    assert_eq!(summary.unwrap().compacted_up_to, 1);
    assert_eq!(
        messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["new"]
    );
    assert_eq!(messages[0].seq, 2);
}

#[test]
fn appended_message_remains_visible_after_deleting_uncompacted_tail() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::initialize(&conn).unwrap();
    let store = HistoryStore::new(Arc::new(Mutex::new(conn)));
    let id = ConversationId::Dm {
        channel_type: ChannelSource::Cli,
        user_id: "deleted-tail-regression".into(),
    };
    store
        .ensure_conversation(&id, ConversationMode::Pairing)
        .unwrap();
    store
        .append_message(&id, &ChatMessage::user("old"), Some("old-turn"))
        .unwrap();
    store
        .append_message(&id, &ChatMessage::user("tail"), Some("tail-turn"))
        .unwrap();
    // Compact the prefix and then delete the surviving turn.
    store
        .commit_compaction(&id, "old conversation", 1, 3)
        .unwrap();
    store.delete_turn(&id, "tail-turn").unwrap();
    store
        .append_message(&id, &ChatMessage::user("new"), None)
        .unwrap();

    let (_, messages) = store.load_summary_and_messages(&id).unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["new"]
    );
}
