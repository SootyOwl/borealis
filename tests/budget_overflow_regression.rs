use borealis::history::budget::{ContextBudget, Turn};

#[test]
fn history_selection_handles_totals_larger_than_usize() {
    let max = usize::MAX;
    for (costs, budget, first_kept) in [
        (vec![max, 1], max, 1),
        (vec![1, max], 0, 1),
        (vec![max, max, 1], max, 2),
        (vec![max, 0, max, 0], max, 1),
    ] {
        let turns: Vec<_> = costs
            .into_iter()
            .enumerate()
            .map(|(i, total_tokens)| Turn {
                turn_id: i.to_string(),
                messages: vec![],
                total_tokens,
            })
            .collect();
        let result = ContextBudget::new(budget, 0, 0, 0, 0).select_turns(&turns);
        assert_eq!(
            result
                .included
                .iter()
                .map(|t| t.turn_id.as_str())
                .collect::<Vec<_>>(),
            turns[first_kept..]
                .iter()
                .map(|t| t.turn_id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            result.evicted,
            turns[..first_kept]
                .iter()
                .map(|t| t.turn_id.as_str())
                .collect::<Vec<_>>()
        );
    }
}
