use std::collections::{HashMap, HashSet};

use tracing::warn;

// ---------------------------------------------------------------------------
// Authorization Result
// ---------------------------------------------------------------------------

/// Outcome of a tool authorization check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationResult {
    /// The tool call is allowed.
    Allowed,
    /// The tool is restricted and the user is not authorized.
    Denied { tool_name: String, user_id: String },
}

// ---------------------------------------------------------------------------
// Tool Authorization
// ---------------------------------------------------------------------------

/// Controls which tools are restricted and which users may call them.
///
/// Restricted tools are declared at registration time by each tool group
/// (e.g. `bash_exec`, `file_write`). Authorized users are configured globally
/// via the allowlist. This is enforced as middleware in the pipeline's tool
/// execution loop.
pub struct Authorization {
    /// Tools that require authorization to execute.
    restricted_tools: HashSet<String>,
    /// Users authorized to call restricted tools, keyed by tool name.
    /// If a tool has no entry, only globally authorized users may call it.
    per_tool_users: HashMap<String, HashSet<String>>,
    /// Users authorized to call any restricted tool.
    global_authorized_users: HashSet<String>,
}

impl Authorization {
    /// Create a new authorization manager with a set of globally authorized users.
    pub fn new(authorized_users: impl IntoIterator<Item = String>) -> Self {
        Self {
            restricted_tools: HashSet::new(),
            per_tool_users: HashMap::new(),
            global_authorized_users: authorized_users.into_iter().collect(),
        }
    }

    /// Register a tool as restricted. Only authorized users may call it.
    pub fn register_restricted(&mut self, tool_name: &str) {
        self.restricted_tools.insert(tool_name.to_owned());
    }

    /// Check whether `user_id` is authorized to call `tool_name`.
    ///
    /// Unrestricted tools are always allowed. For restricted tools, the user
    /// must be either globally authorized or specifically authorized for that tool.
    pub fn check(&self, tool_name: &str, user_id: &str) -> AuthorizationResult {
        if !self.restricted_tools.contains(tool_name) {
            return AuthorizationResult::Allowed;
        }

        // Global authorization
        if self.global_authorized_users.contains(user_id) {
            return AuthorizationResult::Allowed;
        }

        // Per-tool authorization
        if let Some(users) = self.per_tool_users.get(tool_name) {
            if users.contains(user_id) {
                return AuthorizationResult::Allowed;
            }
        }

        warn!(tool_name, user_id, "unauthorized tool access attempted");
        AuthorizationResult::Denied {
            tool_name: tool_name.to_owned(),
            user_id: user_id.to_owned(),
        }
    }

}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_auth() -> Authorization {
        let mut auth = Authorization::new(["admin".to_string()]);
        auth.register_restricted("bash_exec");
        auth.register_restricted("file_write");
        auth
    }

    #[test]
    fn unrestricted_tool_always_allowed() {
        let auth = setup_auth();
        assert_eq!(
            auth.check("memory_create", "random_user"),
            AuthorizationResult::Allowed
        );
    }

    #[test]
    fn restricted_tool_denied_for_unauthorized_user() {
        let auth = setup_auth();
        assert_eq!(
            auth.check("bash_exec", "random_user"),
            AuthorizationResult::Denied {
                tool_name: "bash_exec".to_string(),
                user_id: "random_user".to_string(),
            }
        );
    }

    #[test]
    fn restricted_tool_allowed_for_global_authorized_user() {
        let auth = setup_auth();
        assert_eq!(
            auth.check("bash_exec", "admin"),
            AuthorizationResult::Allowed
        );
    }

}
