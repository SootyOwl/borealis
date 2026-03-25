use std::path::{Path, PathBuf};

use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("path traversal blocked: {path} escapes sandbox root {root}")]
    PathTraversal { path: String, root: String },

    #[error("access denied: {path} is inside the protected memory directory")]
    MemoryDirAccess { path: String },

    #[error("path resolution failed for {path}: {reason}")]
    ResolutionFailed { path: String, reason: String },
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// Validates file paths against a sandbox root directory.
///
/// Rejects paths that escape the sandbox via traversal (e.g. `../../etc/passwd`)
/// and paths that access the protected `memory/` directory (memory access is
/// gated behind the `memory_*` tool handlers).
pub struct Sandbox {
    root: PathBuf,
    memory_dir: PathBuf,
}

impl Sandbox {
    /// Create a new sandbox rooted at the given directory.
    ///
    /// The `memory_subdir` is relative to `root` (e.g. `"memory"`).
    pub fn new(root: PathBuf, memory_subdir: &str) -> Self {
        let memory_dir = root.join(memory_subdir);
        Self { root, memory_dir }
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

        // Check: must not be inside the memory directory
        let canonical_memory = self.memory_dir.canonicalize().unwrap_or_default();
        if !canonical_memory.as_os_str().is_empty() && canonical.starts_with(&canonical_memory) {
            return Err(SandboxError::MemoryDirAccess {
                path: path.display().to_string(),
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
        // Create memory subdirectory
        fs::create_dir_all(tmp.path().join("memory")).expect("mkdir memory");
        // Create a file inside the sandbox
        fs::write(tmp.path().join("allowed.txt"), "ok").expect("write allowed.txt");
        // Create a file inside memory/
        fs::write(tmp.path().join("memory/core.md"), "persona").expect("write core.md");

        let sandbox = Sandbox::new(tmp.path().to_path_buf(), "memory");
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
    fn rejects_memory_directory_access() {
        let (_tmp, sandbox) = setup_sandbox();
        let result = sandbox.validate_path(Path::new("memory/core.md"));
        assert!(matches!(result, Err(SandboxError::MemoryDirAccess { .. })));
    }

    #[test]
    fn rejects_nonexistent_path() {
        let (_tmp, sandbox) = setup_sandbox();
        let result = sandbox.validate_path(Path::new("does_not_exist.txt"));
        assert!(matches!(result, Err(SandboxError::ResolutionFailed { .. })));
    }
}
