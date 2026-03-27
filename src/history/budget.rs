use crate::types::ChatMessage;

// ---------------------------------------------------------------------------
// Turn
// ---------------------------------------------------------------------------

/// A turn is the atomic eviction unit.
#[derive(Debug, Clone)]
pub struct Turn {
    pub turn_id: String,
    pub messages: Vec<ChatMessage>,
    pub total_tokens: usize,
}

// ---------------------------------------------------------------------------
// SelectionResult
// ---------------------------------------------------------------------------

/// Result of turn selection.
#[derive(Debug)]
pub struct SelectionResult<'a> {
    pub included: Vec<&'a Turn>,
    pub evicted: Vec<&'a str>,
}

// ---------------------------------------------------------------------------
// ContextBudget
// ---------------------------------------------------------------------------

/// Context window budget calculator.
pub struct ContextBudget {
    model_max_tokens: usize,
    response_reserve: usize,
    system_prompt_tokens: usize,
    core_persona_tokens: usize,
    tool_def_tokens: usize,
}

impl ContextBudget {
    pub fn new(
        model_max_tokens: usize,
        response_reserve: usize,
        system_prompt_tokens: usize,
        core_persona_tokens: usize,
        tool_def_tokens: usize,
    ) -> Self {
        Self {
            model_max_tokens,
            response_reserve,
            system_prompt_tokens,
            core_persona_tokens,
            tool_def_tokens,
        }
    }

    /// Tokens remaining for conversation history after all fixed costs.
    pub fn available_for_history(&self) -> usize {
        self.model_max_tokens
            .saturating_sub(self.response_reserve)
            .saturating_sub(self.system_prompt_tokens)
            .saturating_sub(self.core_persona_tokens)
            .saturating_sub(self.tool_def_tokens)
    }

    /// Select which turns to include given the history budget.
    ///
    /// - If all turns fit, include all.
    /// - Otherwise evict from the front (oldest) until the remainder fits.
    /// - The most recent turn is always kept, even if it alone exceeds budget.
    pub fn select_turns<'a>(&self, turns: &'a [Turn]) -> SelectionResult<'a> {
        if turns.is_empty() {
            return SelectionResult {
                included: vec![],
                evicted: vec![],
            };
        }

        let budget = self.available_for_history();
        let total: usize = turns.iter().map(|t| t.total_tokens).sum();

        if total <= budget {
            return SelectionResult {
                included: turns.iter().collect(),
                evicted: vec![],
            };
        }

        // Evict oldest turns until the remaining sum fits within budget.
        // Always keep the last turn regardless.
        let last_idx = turns.len() - 1;
        let mut evict_up_to = 0usize; // exclusive index of first kept turn

        let mut running: usize = turns.iter().map(|t| t.total_tokens).sum();
        for (i, turn) in turns.iter().enumerate() {
            if i == last_idx {
                // Never evict the most recent turn.
                break;
            }
            if running <= budget {
                break;
            }
            running = running.saturating_sub(turn.total_tokens);
            evict_up_to = i + 1;
        }

        let evicted = turns[..evict_up_to]
            .iter()
            .map(|t| t.turn_id.as_str())
            .collect();
        let included = turns[evict_up_to..].iter().collect();

        SelectionResult { included, evicted }
    }

    /// Assemble the full message array to send to the model.
    ///
    /// Order:
    /// 1. System message: system_prompt (+ core_persona if non-empty)
    /// 2. All messages from `turns` in order (flattened)
    /// 3. If `retrieved_memories` is non-empty: a system message with the memories
    pub fn assemble(
        &self,
        system_prompt: &str,
        core_persona: &str,
        turns: &[Turn],
        retrieved_memories: &[String],
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::new();

        // 1. System message
        let system_content = if core_persona.is_empty() {
            system_prompt.to_string()
        } else {
            format!("{}\n\n## Core Persona\n\n{}", system_prompt, core_persona)
        };
        messages.push(ChatMessage::system(system_content));

        // 2. History
        for turn in turns {
            for msg in &turn.messages {
                messages.push(msg.clone());
            }
        }

        // 3. Retrieved memories (optional trailing system message)
        if !retrieved_memories.is_empty() {
            let memory_content = format!(
                "## Retrieved Memories\n\n{}",
                retrieved_memories.join("\n\n")
            );
            messages.push(ChatMessage::system(memory_content));
        }

        messages
    }

    /// Same as [`assemble`](Self::assemble) but callable without a `ContextBudget` instance.
    ///
    /// Used by the 400-recovery path which rebuilds messages without a budget.
    pub fn assemble_static(
        system_prompt: &str,
        core_persona: &str,
        turns: &[Turn],
        retrieved_memories: &[String],
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::new();

        let system_content = if core_persona.is_empty() {
            system_prompt.to_string()
        } else {
            format!("{}\n\n## Core Persona\n\n{}", system_prompt, core_persona)
        };
        messages.push(ChatMessage::system(system_content));

        for turn in turns {
            for msg in &turn.messages {
                messages.push(msg.clone());
            }
        }

        if !retrieved_memories.is_empty() {
            let memory_content = format!(
                "## Retrieved Memories\n\n{}",
                retrieved_memories.join("\n\n")
            );
            messages.push(ChatMessage::system(memory_content));
        }

        messages
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_turn(id: &str, tokens: usize, msg_content: &str) -> Turn {
        Turn {
            turn_id: id.to_string(),
            messages: vec![
                ChatMessage::user(msg_content),
                ChatMessage::assistant(msg_content),
            ],
            total_tokens: tokens,
        }
    }

    fn budget(history_budget: usize) -> ContextBudget {
        // model_max = history_budget + 500 (response) + 0 + 0 + 0
        ContextBudget::new(history_budget + 500, 500, 0, 0, 0)
    }

    // --- available_for_history ---

    #[test]
    fn available_for_history_subtracts_fixed_costs() {
        let cb = ContextBudget::new(10_000, 1_000, 500, 200, 100);
        assert_eq!(cb.available_for_history(), 8_200);
    }

    #[test]
    fn available_for_history_saturates_at_zero() {
        let cb = ContextBudget::new(100, 200, 0, 0, 0);
        assert_eq!(cb.available_for_history(), 0);
    }

    // --- select_turns ---

    #[test]
    fn budget_with_plenty_of_room_keeps_all_history() {
        let cb = budget(10_000);
        let turns = vec![make_turn("t1", 100, "hello"), make_turn("t2", 100, "world")];
        let result = cb.select_turns(&turns);
        assert_eq!(result.included.len(), 2);
        assert!(result.evicted.is_empty());
    }

    #[test]
    fn budget_evicts_oldest_turns_first() {
        // budget = 200 tokens; 3 turns of 100 each → oldest must be evicted
        let cb = budget(200);
        let turns = vec![
            make_turn("t1", 100, "a"),
            make_turn("t2", 100, "b"),
            make_turn("t3", 100, "c"),
        ];
        let result = cb.select_turns(&turns);
        assert_eq!(result.evicted, vec!["t1"]);
        assert_eq!(result.included.len(), 2);
        assert_eq!(result.included[0].turn_id, "t2");
        assert_eq!(result.included[1].turn_id, "t3");
    }

    #[test]
    fn budget_evicts_multiple_turns_if_needed() {
        // budget = 100; 4 turns of 100 each → keep only the last
        let cb = budget(100);
        let turns = vec![
            make_turn("t1", 100, "a"),
            make_turn("t2", 100, "b"),
            make_turn("t3", 100, "c"),
            make_turn("t4", 100, "d"),
        ];
        let result = cb.select_turns(&turns);
        assert_eq!(result.evicted, vec!["t1", "t2", "t3"]);
        assert_eq!(result.included.len(), 1);
        assert_eq!(result.included[0].turn_id, "t4");
    }

    #[test]
    fn budget_always_keeps_most_recent_turn() {
        // budget = 50; single turn of 200 tokens — still kept
        let cb = budget(50);
        let turns = vec![make_turn("t1", 200, "very long message")];
        let result = cb.select_turns(&turns);
        assert!(result.evicted.is_empty());
        assert_eq!(result.included.len(), 1);
        assert_eq!(result.included[0].turn_id, "t1");
    }

    #[test]
    fn budget_with_zero_history_budget() {
        // all fixed costs eat the entire budget → available = 0
        let cb = ContextBudget::new(1_000, 400, 300, 200, 100);
        assert_eq!(cb.available_for_history(), 0);

        let turns = vec![make_turn("t1", 50, "older"), make_turn("t2", 50, "newest")];
        let result = cb.select_turns(&turns);
        // most recent turn must always survive
        assert!(result.included.iter().any(|t| t.turn_id == "t2"));
    }

    // --- assemble ---

    #[test]
    fn assemble_prompt_messages_in_correct_order() {
        let cb = ContextBudget::new(10_000, 500, 0, 0, 0);
        let turns = vec![make_turn("t1", 10, "hello")];
        let memories = vec!["memory one".to_string(), "memory two".to_string()];

        let msgs = cb.assemble("SYS", "PERSONA", &turns, &memories);

        // [0] system, [1] user, [2] assistant, [3] memories system
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, crate::types::Role::System);
        assert!(msgs[0].content.contains("SYS"));
        assert!(msgs[0].content.contains("PERSONA"));
        assert_eq!(msgs[1].role, crate::types::Role::User);
        assert_eq!(msgs[2].role, crate::types::Role::Assistant);
        assert_eq!(msgs[3].role, crate::types::Role::System);
        assert!(msgs[3].content.contains("## Retrieved Memories"));
        assert!(msgs[3].content.contains("memory one"));
        assert!(msgs[3].content.contains("memory two"));
    }

    #[test]
    fn assemble_with_empty_persona() {
        let cb = ContextBudget::new(10_000, 500, 0, 0, 0);
        let turns: Vec<Turn> = vec![];
        let msgs = cb.assemble("SYS_ONLY", "", &turns, &[]);

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "SYS_ONLY");
        assert!(!msgs[0].content.contains("## Core Persona"));
    }

    #[test]
    fn assemble_with_no_memories() {
        let cb = ContextBudget::new(10_000, 500, 0, 0, 0);
        let turns = vec![make_turn("t1", 10, "hi")];
        let msgs = cb.assemble("SYS", "PERSONA", &turns, &[]);

        // system + user + assistant — no trailing memories message
        assert_eq!(msgs.len(), 3);
        assert!(
            msgs.iter()
                .all(|m| { m.role != crate::types::Role::System || m.content.contains("SYS") })
        );
        // verify no memories system message
        let memory_msgs: Vec<_> = msgs
            .iter()
            .filter(|m| m.content.contains("## Retrieved Memories"))
            .collect();
        assert!(memory_msgs.is_empty());
    }
}
