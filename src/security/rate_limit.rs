use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tracing::warn;

use crate::config::RateLimitConfig;

// ---------------------------------------------------------------------------
// Token Bucket
// ---------------------------------------------------------------------------

/// A single token bucket with configurable capacity and refill rate.
///
/// Tokens are refilled lazily on each `try_consume` call rather than via a
/// background timer, keeping the implementation simple and allocation-free.
#[derive(Debug)]
struct TokenBucket {
    capacity: u32,
    tokens: f64,
    refill_interval: Duration,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: u32, refill_interval: Duration) -> Self {
        Self {
            capacity,
            tokens: capacity as f64,
            refill_interval,
            last_refill: Instant::now(),
        }
    }

    /// Attempt to consume one token. Returns `true` if a token was available,
    /// `false` if the bucket is empty (rate limited).
    fn try_consume(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Lazily refill tokens based on elapsed time since last refill.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill);
        if elapsed >= self.refill_interval {
            let new_tokens = elapsed.as_secs_f64() / self.refill_interval.as_secs_f64();
            self.tokens = (self.tokens + new_tokens).min(self.capacity as f64);
            self.last_refill = now;
        }
    }
}

// ---------------------------------------------------------------------------
// Rate Limiter
// ---------------------------------------------------------------------------

/// Outcome of a rate limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitResult {
    /// The message is allowed.
    Allowed,
    /// The message was rejected by the per-user rate limit.
    UserLimited { user_id: String },
    /// The message was rejected by the global rate limit.
    GlobalLimited,
}

/// Per-user + global token bucket rate limiter.
///
/// Users and guilds on the allowlists bypass both per-user and global limits.
/// Per-user buckets are stored in a `DashMap` for concurrent access.
/// The global bucket is behind a `Mutex` since it's a single shared resource.
pub struct RateLimiter {
    user_capacity: u32,
    user_refill: Duration,
    user_buckets: DashMap<String, TokenBucket>,
    global_bucket: Mutex<TokenBucket>,
    allowed_users: HashSet<String>,
    allowed_guilds: HashSet<String>,
}

impl RateLimiter {
    /// Create a new rate limiter from the config.
    pub fn new(config: &RateLimitConfig) -> Self {
        Self {
            user_capacity: config.per_user.capacity,
            user_refill: Duration::from_secs(config.per_user.refill_secs),
            user_buckets: DashMap::new(),
            global_bucket: Mutex::new(TokenBucket::new(
                config.global.capacity,
                Duration::from_secs(config.global.refill_secs),
            )),
            allowed_users: config.allowed_users.iter().cloned().collect(),
            allowed_guilds: config.allowed_guilds.iter().cloned().collect(),
        }
    }

    /// Check whether a message from `user_id` in `guild_id` (if any) is
    /// allowed through the rate limiter.
    ///
    /// Allowlisted users and guilds bypass both limits. Otherwise, both the
    /// per-user and global buckets must have tokens available.
    pub fn check(&self, user_id: &str, guild_id: Option<&str>) -> RateLimitResult {
        // Allowlist bypass
        if self.allowed_users.contains(user_id) {
            return RateLimitResult::Allowed;
        }
        if let Some(gid) = guild_id {
            if self.allowed_guilds.contains(gid) {
                return RateLimitResult::Allowed;
            }
        }

        // Per-user check
        let user_allowed = self
            .user_buckets
            .entry(user_id.to_owned())
            .or_insert_with(|| TokenBucket::new(self.user_capacity, self.user_refill))
            .try_consume();

        if !user_allowed {
            warn!(user_id, "per-user rate limit exceeded");
            return RateLimitResult::UserLimited {
                user_id: user_id.to_owned(),
            };
        }

        // Global check
        let global_allowed = self
            .global_bucket
            .lock()
            .expect("global bucket lock poisoned")
            .try_consume();

        if !global_allowed {
            warn!("global rate limit exceeded");
            return RateLimitResult::GlobalLimited;
        }

        RateLimitResult::Allowed
    }

    /// Returns whether the given user is on the allowed users list.
    pub fn is_user_allowed(&self, user_id: &str) -> bool {
        self.allowed_users.contains(user_id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn test_config() -> RateLimitConfig {
        RateLimitConfig {
            per_user: crate::config::TokenBucketConfig {
                capacity: 3,
                refill_secs: 1,
            },
            global: crate::config::GlobalTokenBucketConfig {
                capacity: 5,
                refill_secs: 1,
            },
            allowed_users: vec!["admin".to_string()],
            allowed_guilds: vec!["trusted_guild".to_string()],
        }
    }

    // --- Token Bucket ---

    #[test]
    fn bucket_starts_full() {
        let mut bucket = TokenBucket::new(5, Duration::from_secs(1));
        for _ in 0..5 {
            assert!(bucket.try_consume());
        }
        assert!(!bucket.try_consume());
    }

    #[test]
    fn bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(2, Duration::from_millis(50));
        // Drain the bucket
        assert!(bucket.try_consume());
        assert!(bucket.try_consume());
        assert!(!bucket.try_consume());

        // Wait for refill
        thread::sleep(Duration::from_millis(60));
        assert!(bucket.try_consume());
    }

    #[test]
    fn bucket_does_not_exceed_capacity() {
        let mut bucket = TokenBucket::new(3, Duration::from_millis(10));
        // Wait well beyond what would refill more than capacity
        thread::sleep(Duration::from_millis(100));
        bucket.refill();
        // Should still only have capacity tokens
        for _ in 0..3 {
            assert!(bucket.try_consume());
        }
        assert!(!bucket.try_consume());
    }

    // --- Rate Limiter ---

    #[test]
    fn allowed_user_bypasses_limits() {
        let limiter = RateLimiter::new(&test_config());
        // "admin" is allowlisted — should always pass
        for _ in 0..100 {
            assert_eq!(limiter.check("admin", None), RateLimitResult::Allowed);
        }
    }

    #[test]
    fn allowed_guild_bypasses_limits() {
        let limiter = RateLimiter::new(&test_config());
        // Any user in "trusted_guild" bypasses
        for _ in 0..100 {
            assert_eq!(
                limiter.check("random_user", Some("trusted_guild")),
                RateLimitResult::Allowed
            );
        }
    }

    #[test]
    fn per_user_limit_enforced() {
        let limiter = RateLimiter::new(&test_config());
        // capacity=3 for per-user
        assert_eq!(limiter.check("user1", None), RateLimitResult::Allowed);
        assert_eq!(limiter.check("user1", None), RateLimitResult::Allowed);
        assert_eq!(limiter.check("user1", None), RateLimitResult::Allowed);
        assert_eq!(
            limiter.check("user1", None),
            RateLimitResult::UserLimited {
                user_id: "user1".to_string()
            }
        );
    }

    #[test]
    fn per_user_limits_are_independent() {
        let limiter = RateLimiter::new(&test_config());
        // Exhaust user1
        for _ in 0..3 {
            limiter.check("user1", None);
        }
        assert_eq!(
            limiter.check("user1", None),
            RateLimitResult::UserLimited {
                user_id: "user1".to_string()
            }
        );
        // user2 should still have tokens
        assert_eq!(limiter.check("user2", None), RateLimitResult::Allowed);
    }

    #[test]
    fn global_limit_enforced() {
        let limiter = RateLimiter::new(&test_config());
        // global capacity=5; use 5 different users to avoid per-user limits (cap=3 each)
        for i in 0..5 {
            assert_eq!(
                limiter.check(&format!("u{i}"), None),
                RateLimitResult::Allowed
            );
        }
        // 6th message from a new user should hit global limit
        assert_eq!(limiter.check("u5", None), RateLimitResult::GlobalLimited);
    }

    #[test]
    fn ac16_per_user_rejects_11th_message() {
        // Acceptance criteria: per-user limiter rejects the 11th message in 60s
        let config = RateLimitConfig {
            per_user: crate::config::TokenBucketConfig {
                capacity: 10,
                refill_secs: 6,
            },
            global: crate::config::GlobalTokenBucketConfig {
                capacity: 30,
                refill_secs: 2,
            },
            allowed_users: vec![],
            allowed_guilds: vec![],
        };
        let limiter = RateLimiter::new(&config);
        for i in 0..10 {
            assert_eq!(
                limiter.check("testuser", None),
                RateLimitResult::Allowed,
                "message {i} should be allowed"
            );
        }
        assert_eq!(
            limiter.check("testuser", None),
            RateLimitResult::UserLimited {
                user_id: "testuser".to_string()
            },
            "11th message should be rejected"
        );
    }

    #[test]
    fn ac16_global_rejects_31st_message() {
        // Acceptance criteria: global limiter triggers at 31 messages
        let config = RateLimitConfig {
            per_user: crate::config::TokenBucketConfig {
                capacity: 100, // high to avoid per-user limit
                refill_secs: 6,
            },
            global: crate::config::GlobalTokenBucketConfig {
                capacity: 30,
                refill_secs: 2,
            },
            allowed_users: vec![],
            allowed_guilds: vec![],
        };
        let limiter = RateLimiter::new(&config);
        // Use different users to avoid per-user limits
        for i in 0..30 {
            assert_eq!(
                limiter.check(&format!("user{i}"), None),
                RateLimitResult::Allowed,
                "message {i} should be allowed"
            );
        }
        assert_eq!(
            limiter.check("user30", None),
            RateLimitResult::GlobalLimited,
            "31st message should be rejected by global limit"
        );
    }

    // --- is_user_allowed ---

    #[test]
    fn is_user_allowed_works() {
        let limiter = RateLimiter::new(&test_config());
        assert!(limiter.is_user_allowed("admin"));
        assert!(!limiter.is_user_allowed("random"));
    }
}
