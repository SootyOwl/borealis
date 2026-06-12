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

use std::path::PathBuf;

use crate::config::RateLimitConfig;

/// Tools that have side effects and therefore require authorization.
///
/// This is the single source of truth for the default restricted set —
/// registered at startup via [`Security::register_default_restricted`].
/// When adding a new tool, ask: can it mutate state, execute code, or send
/// output somewhere? If yes, it belongs here. Read-only tools (`file_read`,
/// `file_list`, `web_fetch`, `web_search`, memory reads) stay unrestricted.
pub const RESTRICTED_TOOLS: &[&str] = &[
    // Memory write tools
    "memory_create",
    "memory_update",
    "memory_link",
    "memory_tag",
    "memory_forget",
    // Computer-use tools with side effects (bash_exec can run arbitrary code)
    "bash_exec",
    "file_write",
    // Channel tools (can send messages/files/reactions anywhere the bot can reach)
    "send_message",
    "send_file",
    "react",
];

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
    /// - `authorized_users`: users permitted to call restricted tools.
    pub fn new(
        rate_limit_config: &RateLimitConfig,
        sandbox_root: PathBuf,
        authorized_users: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            rate_limiter: RateLimiter::new(rate_limit_config),
            sandbox: Sandbox::new(sandbox_root),
            authorization: Authorization::new(authorized_users),
        }
    }

    /// Convenience: check tool authorization for a user.
    pub fn check_authorization(&self, tool_name: &str, user_id: &str) -> AuthorizationResult {
        self.authorization.check(tool_name, user_id)
    }

    /// Register a tool as restricted (delegates to authorization).
    pub fn register_restricted(&mut self, tool_name: &str) {
        self.authorization.register_restricted(tool_name);
    }

    /// Register the default restricted-tool set ([`RESTRICTED_TOOLS`]).
    ///
    /// Called once at startup so that every side-effecting tool requires
    /// authorization regardless of which tool groups are enabled.
    pub fn register_default_restricted(&mut self) {
        for tool_name in RESTRICTED_TOOLS {
            self.register_restricted(tool_name);
        }
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
            ["admin".to_string()],
        );
        security.register_restricted("bash_exec");

        (tmp, security)
    }

    #[test]
    fn default_restricted_set_denies_dangerous_tools_for_unauthorized_users() {
        // Mirror exactly how main.rs registers restricted tools:
        // Security::new(...) followed by register_default_restricted().
        let mut security = Security::new(
            &RateLimitConfig::default(),
            PathBuf::from("."),
            ["admin".to_string()],
        );
        security.register_default_restricted();

        // Every tool with side effects must be in the default restricted set.
        for tool in [
            "bash_exec",
            "file_write",
            "send_message",
            "send_file",
            "react",
            "memory_create",
            "memory_update",
            "memory_link",
            "memory_tag",
            "memory_forget",
        ] {
            assert!(
                RESTRICTED_TOOLS.contains(&tool),
                "{tool} must be in RESTRICTED_TOOLS"
            );
            assert_eq!(
                security.check_authorization(tool, "rando"),
                AuthorizationResult::Denied {
                    tool_name: tool.to_string(),
                    user_id: "rando".to_string(),
                },
                "{tool} must be denied for unauthorized users"
            );
            assert_eq!(
                security.check_authorization(tool, "admin"),
                AuthorizationResult::Allowed,
                "{tool} must be allowed for authorized users"
            );
        }

        // Read-only tools stay unrestricted.
        for tool in ["file_read", "file_list", "web_fetch", "web_search"] {
            assert_eq!(
                security.check_authorization(tool, "rando"),
                AuthorizationResult::Allowed,
                "{tool} is read-only and must stay unrestricted"
            );
        }
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
