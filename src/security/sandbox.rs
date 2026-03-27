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

    #[error("access to memory directory blocked: {path} — use memory_* tools instead")]
    MemoryDirBlocked { path: String },
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
    memory_dir: Option<PathBuf>,
}

impl Sandbox {
    /// Create a new sandbox rooted at the given directory.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            memory_dir: None,
        }
    }

    /// Create a sandbox that also blocks access to a memory directory.
    pub fn with_memory_dir(root: PathBuf, memory_dir: PathBuf) -> Self {
        Self {
            root,
            memory_dir: Some(memory_dir),
        }
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

        // Check: must not be within the memory directory
        if let Some(ref memory_dir) = self.memory_dir {
            let in_memory = if let Ok(canonical_memory) = memory_dir.canonicalize() {
                canonical.starts_with(&canonical_memory)
            } else {
                // Directory doesn't exist yet — fall back to component matching
                // against the canonical root + memory subdir name.
                let memory_suffix = memory_dir.strip_prefix(&self.root).unwrap_or(memory_dir);
                let fallback = canonical_root.join(memory_suffix);
                canonical.starts_with(&fallback)
            };
            if in_memory {
                return Err(SandboxError::MemoryDirBlocked {
                    path: path.display().to_string(),
                });
            }
        }

        Ok(canonical)
    }

    /// Returns the sandbox root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the memory directory path, if configured.
    pub fn memory_dir(&self) -> Option<&Path> {
        self.memory_dir.as_deref()
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

    fn setup_sandbox_with_memory() -> (tempfile::TempDir, Sandbox) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        fs::write(tmp.path().join("allowed.txt"), "ok").expect("write allowed.txt");
        fs::create_dir_all(tmp.path().join("memory")).expect("mkdir memory");
        fs::write(tmp.path().join("memory/core.md"), "persona").expect("write core.md");

        let memory_dir = tmp.path().join("memory");
        let sandbox = Sandbox::with_memory_dir(tmp.path().to_path_buf(), memory_dir);
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

    #[test]
    fn rejects_memory_dir_access() {
        let (_tmp, sandbox) = setup_sandbox_with_memory();
        let result = sandbox.validate_path(Path::new("memory/core.md"));
        assert!(matches!(result, Err(SandboxError::MemoryDirBlocked { .. })));
    }

    #[test]
    fn allows_non_memory_path_with_memory_dir_set() {
        let (_tmp, sandbox) = setup_sandbox_with_memory();
        let result = sandbox.validate_path(Path::new("allowed.txt"));
        assert!(result.is_ok());
    }
}
