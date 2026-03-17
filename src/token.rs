use tiktoken_rs::o200k_base;

pub trait TokenEstimator: Send + Sync {
    fn estimate(&self, text: &str) -> usize;
}

pub struct TiktokenEstimator {
    bpe: tiktoken_rs::CoreBPE,
}

impl TiktokenEstimator {
    pub fn new() -> Self {
        Self {
            bpe: o200k_base().expect("failed to initialize o200k_base tokenizer"),
        }
    }
}

impl Default for TiktokenEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenEstimator for TiktokenEstimator {
    fn estimate(&self, text: &str) -> usize {
        self.bpe.encode_with_special_tokens(text).len()
    }
}

pub struct HeuristicEstimator;

impl TokenEstimator for HeuristicEstimator {
    fn estimate(&self, text: &str) -> usize {
        text.len() / 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tiktoken_estimator_counts_tokens() {
        let estimator = TiktokenEstimator::new();
        let count = estimator.estimate("Hello, world!");
        assert!(count > 0, "should produce at least 1 token");
        assert!(count < 10, "Hello, world! should be ~4 tokens, got {count}");
    }

    #[test]
    fn test_tiktoken_estimator_empty_string() {
        let estimator = TiktokenEstimator::new();
        assert_eq!(estimator.estimate(""), 0);
    }

    #[test]
    fn test_heuristic_estimator() {
        let estimator = HeuristicEstimator;
        assert_eq!(estimator.estimate("abcdefgh"), 2); // 8 / 4
        assert_eq!(estimator.estimate(""), 0);
    }

    #[test]
    fn test_heuristic_estimator_rounds_down() {
        let estimator = HeuristicEstimator;
        assert_eq!(estimator.estimate("abc"), 0); // 3 / 4 = 0
        assert_eq!(estimator.estimate("abcd"), 1); // 4 / 4 = 1
    }
}
