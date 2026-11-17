//! Discovery-only models: canonical IDs, duration arithmetic, and history eviction.
use borealis::core::event::{ChannelSource, ConversationId};
use borealis::history::budget::{ContextBudget, Turn};
use borealis::scheduler::events::{is_within_active_hours, parse_duration};
use chrono::NaiveTime;
use proptest::prelude::*;

proptest! {
    #[test]
    fn conversation_ids_roundtrip(tail in prop_oneof![".{0,40}", (".{0,12}", ".{0,12}").prop_map(|(a,b)| format!("{a}:{b}"))], variant in 0u8..3) {
        let id = match variant {
            0 => ConversationId::System { event_name: tail },
            1 => ConversationId::Dm { channel_type: ChannelSource::Cli, user_id: tail },
            _ => ConversationId::Group { channel_type: ChannelSource::Discord, group_id: tail },
        };
        prop_assert_eq!(ConversationId::parse(&id.to_string()), Ok(id));
    }

    #[test]
    fn duration_matches_wide_arithmetic(value in prop_oneof![any::<u64>(), 0u64..100_000, prop::sample::select(vec![u64::MAX / 60, u64::MAX / 60 + 1, u64::MAX / 3600, u64::MAX / 3600 + 1])], unit in 0usize..3) {
        let (suffix, multiplier) = [('s', 1u128), ('m', 60), ('h', 3600)][unit];
        let expected = u128::from(value) * multiplier;
        let result = parse_duration(&format!("{value}{suffix}"));
        if expected <= u128::from(u64::MAX) {
            prop_assert_eq!(result.unwrap().as_secs(), expected as u64);
        } else {
            prop_assert!(result.is_err());
        }
    }

    #[test]
    fn budget_selects_longest_fitting_suffix(
        costs in prop::collection::vec(prop_oneof![0usize..10_000, Just(usize::MAX)], 0..20),
        budget in prop_oneof![any::<usize>(), 0usize..30_000, Just(usize::MAX)],
    ) {
        let turns: Vec<_> = costs.iter().enumerate().map(|(i, &cost)| Turn {
            turn_id: i.to_string(), messages: vec![], total_tokens: cost,
        }).collect();
        // Exhaustively enumerate suffixes with wider arithmetic, including the
        // documented exception that the final turn must always survive.
        let start = (0..costs.len()).find(|&i| {
            costs[i..].iter().map(|&n| n as u128).sum::<u128>() <= budget as u128
                || i + 1 == costs.len()
        }).unwrap_or(0);
        let result = ContextBudget::new(budget, 0, 0, 0, 0).select_turns(&turns);
        prop_assert_eq!(result.included.iter().map(|t| t.turn_id.clone()).collect::<Vec<_>>(),
            turns[start..].iter().map(|t| t.turn_id.clone()).collect::<Vec<_>>());
        prop_assert_eq!(result.evicted, turns[..start].iter().map(|t| t.turn_id.as_str()).collect::<Vec<_>>());
    }

    #[test]
    fn active_hours_match_enumerated_minutes(start in 0u32..1440, end in 0u32..1440, minute in 0u32..1440) {
        let time = |m| NaiveTime::from_hms_opt(m / 60, m % 60, 0).unwrap();
        let mut allowed = vec![false; 1440];
        let mut cursor = start;
        loop {
            allowed[cursor as usize] = true;
            if cursor == end { break; }
            cursor = (cursor + 1) % 1440;
        }
        prop_assert_eq!(is_within_active_hours(time(minute), time(start), time(end)), allowed[minute as usize]);
    }
}
