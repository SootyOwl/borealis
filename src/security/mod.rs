//! Security module composing rate limiting, path sandboxing, and tool authorization.
//!
//! The `Security` struct bundles all three concerns and is injected into the
//! pipeline as a single dependency.

mod authorization;
mod rate_limit;
mod sandbox;

pub use authorization::{Authorization, AuthorizationResult};
pub use rate_limit::{RateLimitResult, RateLimiter};
pub use sandbox::{Sandbox, SandboxError};

use std::path::{Path, PathBuf};

use crate::config::RateLimitConfig;

/// Unified security façade composing rate limiting, sandboxing, and authorization.
///
/// Constructed once at startup and shared (via `Arc`) with the pipeline and
/// any tool groups that need to register restricted tools.
pub struct Security {
    pub rate_limiter: RateLimiter,
    pub sandbox: Sandbox,
    pub authorization: Authorization,
}

impl Security {
    /// Create a new `Security` instance from configuration.
    ///
    /// - `rate_limit_config`: token bucket settings for per-user and global limits.
    /// - `sandbox_root`: root directory for file operations.
    /// - `memory_subdir`: subdirectory within `sandbox_root` that is off-limits
    ///   (defaults to `"memory"` in the design doc).
    /// - `authorized_users`: users permitted to call restricted tools.
    pub fn new(
        rate_limit_config: &RateLimitConfig,
        sandbox_root: PathBuf,
        memory_subdir: &str,
        authorized_users: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            rate_limiter: RateLimiter::new(rate_limit_config),
            sandbox: Sandbox::new(sandbox_root, memory_subdir),
            authorization: Authorization::new(authorized_users),
        }
    }

    /// Convenience: check rate limit for a user/guild pair.
    pub fn check_rate_limit(&self, user_id: &str, guild_id: Option<&str>) -> RateLimitResult {
        self.rate_limiter.check(user_id, guild_id)
    }

    /// Convenience: validate a file path against the sandbox.
    pub fn validate_path(&self, path: &Path) -> Result<PathBuf, SandboxError> {
        self.sandbox.validate_path(path)
    }

    /// Convenience: check tool authorization for a user.
    pub fn check_authorization(&self, tool_name: &str, user_id: &str) -> AuthorizationResult {
        self.authorization.check(tool_name, user_id)
    }

    /// Register a tool as restricted (delegates to authorization).
    pub fn register_restricted(&mut self, tool_name: &str) {
        self.authorization.register_restricted(tool_name);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_security() -> (tempfile::TempDir, Security) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        fs::create_dir_all(tmp.path().join("memory")).expect("mkdir memory");
        fs::write(tmp.path().join("test.txt"), "ok").expect("write test.txt");

        let config = RateLimitConfig {
            per_user: crate::config::TokenBucketConfig {
                capacity: 3,
                refill_secs: 1,
            },
            global: crate::config::GlobalTokenBucketConfig {
                capacity: 5,
                refill_secs: 1,
            },
            allowed_users: vec!["admin".to_string()],
            allowed_guilds: vec![],
        };

        let mut security = Security::new(
            &config,
            tmp.path().to_path_buf(),
            "memory",
            ["admin".to_string()],
        );
        security.register_restricted("bash_exec");

        (tmp, security)
    }

    #[test]
    fn security_composes_rate_limiting() {
        let (_tmp, security) = setup_security();
        assert_eq!(
            security.check_rate_limit("admin", None),
            RateLimitResult::Allowed,
        );
    }

    #[test]
    fn security_composes_sandbox() {
        let (_tmp, security) = setup_security();
        assert!(security.validate_path(Path::new("test.txt")).is_ok());
    }

    #[test]
    fn security_composes_authorization() {
        let (_tmp, security) = setup_security();
        assert_eq!(
            security.check_authorization("bash_exec", "admin"),
            AuthorizationResult::Allowed,
        );
        assert_eq!(
            security.check_authorization("bash_exec", "rando"),
            AuthorizationResult::Denied {
                tool_name: "bash_exec".to_string(),
                user_id: "rando".to_string(),
            },
        );
    }
}
