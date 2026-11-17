//! Discovery-only state model. The oracle stores logical messages and removes
//! prefixes/turns directly; it does not model SQL sequence allocation.
use borealis::history::{schema, store::HistoryStore};
use borealis::types::{ChannelSource, ChatMessage, ConversationId, ConversationMode};
use proptest::prelude::*;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
enum Op {
    Append { conversation: usize, turn: u8 },
    Compact { conversation: usize, count: usize },
    Delete { conversation: usize, turn: u8 },
    Read,
}

fn operation() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (0..2usize, 0..4u8).prop_map(|(conversation, turn)| Op::Append { conversation, turn }),
        3 => (0..2usize, 0..12usize).prop_map(|(conversation, count)| Op::Compact { conversation, count }),
        2 => (0..2usize, 0..4u8).prop_map(|(conversation, turn)| Op::Delete { conversation, turn }),
        1 => Just(Op::Read),
    ]
}

fn replay(ops: &[Op]) -> Result<(), TestCaseError> {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::initialize(&conn).unwrap();
    let store = HistoryStore::new(Arc::new(Mutex::new(conn)));
    let ids: Vec<_> = (0..2)
        .map(|n| ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: format!("model-{n}"),
        })
        .collect();
    for id in &ids {
        store
            .ensure_conversation(id, ConversationMode::Pairing)
            .unwrap();
    }
    let mut models: [Vec<(u8, String)>; 2] = [vec![], vec![]];
    for (step, op) in ops.iter().enumerate() {
        match *op {
            Op::Append { conversation, turn } => {
                let content = format!("message-{step}");
                store
                    .append_message(
                        &ids[conversation],
                        &ChatMessage::user(&content),
                        Some(&format!("turn-{turn}")),
                    )
                    .unwrap();
                models[conversation].push((turn, content));
            }
            Op::Compact {
                conversation,
                count,
            } => {
                let model = &mut models[conversation];
                if !model.is_empty() {
                    // Select by model length. Production seq is only an opaque
                    // API handle for the independently selected logical prefix.
                    let count = count.saturating_add(1).min(model.len());
                    let rows = store.load_messages(&ids[conversation]).unwrap();
                    let boundary = rows[count - 1].seq;
                    let deleted = store
                        .commit_compaction(&ids[conversation], "summary", boundary, 1)
                        .unwrap();
                    prop_assert_eq!(deleted, count, "step {}: {:?}", step, op);
                    model.drain(..count);
                }
            }
            Op::Delete { conversation, turn } => {
                let model = &mut models[conversation];
                let before = model.len();
                model.retain(|(t, _)| *t != turn);
                let deleted = store
                    .delete_turn(&ids[conversation], &format!("turn-{turn}"))
                    .unwrap();
                prop_assert_eq!(deleted, before - model.len());
            }
            Op::Read => {}
        }
        for conversation in 0..2 {
            let expected: Vec<_> = models[conversation]
                .iter()
                .map(|(_, content)| content.clone())
                .collect();
            let raw = store.load_messages(&ids[conversation]).unwrap();
            prop_assert_eq!(
                raw.iter().map(|m| m.content.clone()).collect::<Vec<_>>(),
                expected.clone(),
                "raw rows at step {}: {:?}",
                step,
                op
            );
            let (summary, visible) = store.load_summary_and_messages(&ids[conversation]).unwrap();
            prop_assert_eq!(
                visible
                    .iter()
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>(),
                expected,
                "visible rows at step {}: {:?}; summary={:?}",
                step,
                op,
                summary
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct(
            "tests/proptest_history.proptest-regressions",
        ))),
        ..ProptestConfig::default()
    })]
    #[test]
    fn history_matches_logical_contents(ops in prop::collection::vec(operation(), 1..80)) {
        replay(&ops)?;
    }

    #[test]
    fn append_after_full_compaction(prefix in prop::collection::vec(operation(), 0..30), conversation in 0..2usize, turn in 0..4u8) {
        let mut ops = prefix;
        ops.extend([
            Op::Append { conversation, turn },
            Op::Compact { conversation, count: usize::MAX },
            Op::Delete { conversation, turn: 0 },
            Op::Delete { conversation, turn: 1 },
            Op::Delete { conversation, turn: 2 },
            Op::Delete { conversation, turn: 3 },
            Op::Append { conversation, turn },
            Op::Read,
        ]);
        replay(&ops)?;
    }
}
