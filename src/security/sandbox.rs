use std::path::{Path, PathBuf};

use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("path traversal blocked: {path} escapes sandbox root {root}")]
    PathTraversal { path: String, root: String },

    #[error("path resolution failed for {path}: {reason}")]
    ResolutionFailed { path: String, reason: String },
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// Validates file paths against a sandbox root directory.
///
/// Rejects paths that escape the sandbox via traversal (e.g. `../../etc/passwd`).
/// Memory access is handled separately by the `memory_*` tools and their
/// authorization — the sandbox doesn't need to know about memory paths.
pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// Create a new sandbox rooted at the given directory.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Validate that `path` (relative to the sandbox root) stays within bounds.
    ///
    /// Returns the canonicalized absolute path on success.
    ///
    /// # Note
    /// Uses `canonicalize()` + prefix check. This has a TOCTOU race condition
    /// which is acceptable for single-user deployment (documented as a known
    /// limitation in the design doc).
    pub fn validate_path(&self, path: &Path) -> Result<PathBuf, SandboxError> {
        // Resolve relative to sandbox root
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        // Canonicalize to resolve symlinks and `..` components
        let canonical = absolute
            .canonicalize()
            .map_err(|e| SandboxError::ResolutionFailed {
                path: absolute.display().to_string(),
                reason: e.to_string(),
            })?;

        // Check: must be within sandbox root
        let canonical_root =
            self.root
                .canonicalize()
                .map_err(|e| SandboxError::ResolutionFailed {
                    path: self.root.display().to_string(),
                    reason: e.to_string(),
                })?;

        if !canonical.starts_with(&canonical_root) {
            return Err(SandboxError::PathTraversal {
                path: path.display().to_string(),
                root: self.root.display().to_string(),
            });
        }

        Ok(canonical)
    }

    /// Returns the sandbox root path.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_sandbox() -> (tempfile::TempDir, Sandbox) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        // Create a file inside the sandbox
        fs::write(tmp.path().join("allowed.txt"), "ok").expect("write allowed.txt");

        let sandbox = Sandbox::new(tmp.path().to_path_buf());
        (tmp, sandbox)
    }

    #[test]
    fn valid_path_within_sandbox() {
        let (_tmp, sandbox) = setup_sandbox();
        let result = sandbox.validate_path(Path::new("allowed.txt"));
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_path_traversal() {
        let (_tmp, sandbox) = setup_sandbox();
        let result = sandbox.validate_path(Path::new("../../etc/passwd"));
        assert!(matches!(result, Err(SandboxError::PathTraversal { .. })));
    }

    #[test]
    fn rejects_nonexistent_path() {
        let (_tmp, sandbox) = setup_sandbox();
        let result = sandbox.validate_path(Path::new("does_not_exist.txt"));
        assert!(matches!(result, Err(SandboxError::ResolutionFailed { .. })));
    }
}
