use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;

use crate::config::ComputerUseConfig;
use crate::security::Sandbox;
use crate::tools::{Tool, ToolContext, ToolDef, ToolRegistry, ToolResult};

/// Safe environment variables that are re-added after `env_clear()`.
/// Prevents leaking API keys (ANTHROPIC_API_KEY, etc.) to child processes.
const SAFE_ENV_VARS: &[&str] = &["PATH", "HOME", "LANG", "TERM", "USER", "SHELL"];

/// Register all 4 computer use tools into the given registry.
///
/// `bash_exec` and `file_write` are registered as restricted tools via `Security`
/// at the call site — not here, to keep this module decoupled from authorization.
pub fn register_computer_tools(
    registry: &mut ToolRegistry,
    sandbox: Arc<Sandbox>,
    config: &ComputerUseConfig,
) {
    let allowlist = if config.command_allowlist.is_empty() {
        None
    } else {
        Some(Arc::from(
            config.command_allowlist.clone().into_boxed_slice(),
        ))
    };
    let timeout = Duration::from_secs(config.command_timeout_secs);

    registry.register(BashExec {
        sandbox: Arc::clone(&sandbox),
        command_allowlist: allowlist,
        timeout,
    });
    registry.register(FileRead {
        sandbox: Arc::clone(&sandbox),
    });
    registry.register(FileWrite {
        sandbox: Arc::clone(&sandbox),
    });
    registry.register(FileList {
        sandbox: Arc::clone(&sandbox),
    });
}

fn error_result(call_id: &str, msg: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: serde_json::json!({ "error": msg }),
        is_error: true,
    }
}

fn ok_result(call_id: &str, value: serde_json::Value) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: value,
        is_error: false,
    }
}

fn get_str<'a>(args: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    args.get(field).and_then(|v| v.as_str())
}

// ---------------------------------------------------------------------------
// bash_exec
// ---------------------------------------------------------------------------

struct BashExec {
    sandbox: Arc<Sandbox>,
    command_allowlist: Option<Arc<[String]>>,
    timeout: Duration,
}

impl Tool for BashExec {
    fn name(&self) -> &str {
        "bash_exec"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "bash_exec".to_string(),
            description: "Execute a shell command within the sandbox. Returns stdout, stderr, \
                          and exit code. Stdin is closed (no interactive commands)."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let command = match get_str(&args, "command") {
            Some(c) => c,
            None => return error_result(call_id, "missing required field: command"),
        };

        // Check command allowlist if configured
        if let Some(ref allowlist) = self.command_allowlist {
            let base_command = command.split_whitespace().next().unwrap_or("");
            if !allowlist.iter().any(|allowed| allowed == base_command) {
                return error_result(
                    call_id,
                    &format!(
                        "command '{}' is not in the allowlist. Allowed: {}",
                        base_command,
                        allowlist.join(", ")
                    ),
                );
            }
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(self.sandbox.root())
            .stdin(std::process::Stdio::null());

        // Clear environment and re-add only safe variables
        cmd.env_clear();
        for var in SAFE_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }

        let result = tokio::time::timeout(self.timeout, cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code().unwrap_or(-1);

                ok_result(
                    call_id,
                    serde_json::json!({
                        "stdout": stdout,
                        "stderr": stderr,
                        "exit_code": exit_code,
                    }),
                )
            }
            Ok(Err(e)) => error_result(call_id, &format!("failed to execute command: {e}")),
            Err(_) => error_result(
                call_id,
                &format!("command timed out after {}s", self.timeout.as_secs()),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// file_read
// ---------------------------------------------------------------------------

struct FileRead {
    sandbox: Arc<Sandbox>,
}

impl Tool for FileRead {
    fn name(&self) -> &str {
        "file_read"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "file_read".to_string(),
            description: "Read the contents of a file within the sandbox. Path is relative to \
                          the sandbox root. The memory/ directory is excluded — use memory_* \
                          tools instead."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the sandbox root"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let path_str = match get_str(&args, "path") {
            Some(p) => p,
            None => return error_result(call_id, "missing required field: path"),
        };

        let canonical = match self.sandbox.validate_path(Path::new(path_str)) {
            Ok(p) => p,
            Err(e) => return error_result(call_id, &e.to_string()),
        };

        match tokio::fs::read_to_string(&canonical).await {
            Ok(content) => ok_result(
                call_id,
                serde_json::json!({
                    "path": path_str,
                    "content": content,
                }),
            ),
            Err(e) => error_result(call_id, &format!("failed to read file: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// file_write
// ---------------------------------------------------------------------------

struct FileWrite {
    sandbox: Arc<Sandbox>,
}

impl Tool for FileWrite {
    fn name(&self) -> &str {
        "file_write"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "file_write".to_string(),
            description: "Write content to a file within the sandbox. Creates parent directories \
                          if needed. Path is relative to the sandbox root. The memory/ directory \
                          is excluded — use memory_* tools instead."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the sandbox root"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let path_str = match get_str(&args, "path") {
            Some(p) => p,
            None => return error_result(call_id, "missing required field: path"),
        };
        let content = match get_str(&args, "content") {
            Some(c) => c,
            None => return error_result(call_id, "missing required field: content"),
        };

        // For file_write, the file may not exist yet so canonicalize won't work.
        // Validate the parent directory instead, then ensure the target stays in sandbox.
        let target = if Path::new(path_str).is_absolute() {
            Path::new(path_str).to_path_buf()
        } else {
            self.sandbox.root().join(path_str)
        };

        // Create parent directories if needed
        if let Some(parent) = target.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return error_result(
                    call_id,
                    &format!("failed to create parent directories: {e}"),
                );
            }
        }

        // Now validate the path (parent exists so canonicalize of parent works)
        // We validate by checking the canonical parent is within sandbox
        let canonical_parent = match target.parent() {
            Some(parent) => match parent.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    return error_result(
                        call_id,
                        &format!("failed to resolve parent directory: {e}"),
                    );
                }
            },
            None => return error_result(call_id, "invalid path: no parent directory"),
        };

        let canonical_root = match self.sandbox.root().canonicalize() {
            Ok(r) => r,
            Err(e) => {
                return error_result(call_id, &format!("sandbox root resolution failed: {e}"));
            }
        };

        if !canonical_parent.starts_with(&canonical_root) {
            return error_result(
                call_id,
                &format!("path traversal blocked: {} escapes sandbox root", path_str),
            );
        }

        // Check memory directory exclusion
        let memory_dir = self.sandbox.root().join("memory");
        if let Ok(canonical_memory) = memory_dir.canonicalize() {
            let file_in_parent = canonical_parent.join(target.file_name().unwrap_or_default());
            if file_in_parent.starts_with(&canonical_memory)
                || canonical_parent.starts_with(&canonical_memory)
            {
                return error_result(
                    call_id,
                    "access denied: the memory/ directory is protected. Use memory_* tools instead.",
                );
            }
        }

        match tokio::fs::write(&target, content).await {
            Ok(()) => ok_result(
                call_id,
                serde_json::json!({
                    "path": path_str,
                    "bytes_written": content.len(),
                }),
            ),
            Err(e) => error_result(call_id, &format!("failed to write file: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// file_list
// ---------------------------------------------------------------------------

struct FileList {
    sandbox: Arc<Sandbox>,
}

impl Tool for FileList {
    fn name(&self) -> &str {
        "file_list"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "file_list".to_string(),
            description: "List directory contents within the sandbox. Returns file names, sizes, \
                          and types. Path is relative to the sandbox root. The memory/ directory \
                          is excluded."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path relative to sandbox root (default: root)"
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "Whether to list recursively (default: false)"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Maximum recursion depth (default: 3, only used if recursive=true)"
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let path_str = get_str(&args, "path").unwrap_or(".");
        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;

        let canonical = match self.sandbox.validate_path(Path::new(path_str)) {
            Ok(p) => p,
            Err(e) => return error_result(call_id, &e.to_string()),
        };

        if !canonical.is_dir() {
            return error_result(call_id, &format!("{path_str} is not a directory"));
        }

        let mut entries = Vec::new();
        if let Err(e) = list_dir(
            &canonical,
            &canonical,
            recursive,
            max_depth,
            0,
            &mut entries,
        )
        .await
        {
            return error_result(call_id, &format!("failed to list directory: {e}"));
        }

        ok_result(
            call_id,
            serde_json::json!({
                "path": path_str,
                "entries": entries,
            }),
        )
    }
}

/// Recursively list directory entries.
async fn list_dir(
    base: &std::path::Path,
    dir: &std::path::Path,
    recursive: bool,
    max_depth: usize,
    current_depth: usize,
    entries: &mut Vec<serde_json::Value>,
) -> Result<(), std::io::Error> {
    let mut read_dir = tokio::fs::read_dir(dir).await?;

    while let Some(entry) = read_dir.next_entry().await? {
        let metadata = entry.metadata().await?;
        let file_type = if metadata.is_dir() {
            "directory"
        } else if metadata.is_symlink() {
            "symlink"
        } else {
            "file"
        };

        let relative = entry
            .path()
            .strip_prefix(base)
            .unwrap_or(&entry.path())
            .to_string_lossy()
            .to_string();

        entries.push(serde_json::json!({
            "name": relative,
            "type": file_type,
            "size": metadata.len(),
        }));

        if recursive && metadata.is_dir() && current_depth < max_depth {
            Box::pin(list_dir(
                base,
                &entry.path(),
                recursive,
                max_depth,
                current_depth + 1,
                entries,
            ))
            .await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_ctx() -> ToolContext {
        ToolContext {
            author_id: "test_user".to_string(),
            conversation_id: "test_call".to_string(),
            channel_source: "cli".to_string(),
        }
    }

    fn setup_sandbox() -> (tempfile::TempDir, Arc<Sandbox>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join("memory")).expect("mkdir memory");
        fs::write(tmp.path().join("memory/core.md"), "persona").expect("write core.md");
        fs::write(tmp.path().join("hello.txt"), "hello world").expect("write hello.txt");
        fs::create_dir_all(tmp.path().join("subdir")).expect("mkdir subdir");
        fs::write(tmp.path().join("subdir/nested.txt"), "nested").expect("write nested");

        let sandbox = Arc::new(Sandbox::new(tmp.path().to_path_buf(), "memory"));
        (tmp, sandbox)
    }

    // -- bash_exec tests --

    #[tokio::test]
    async fn bash_exec_echo() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = BashExec {
            sandbox,
            command_allowlist: None,
            timeout: Duration::from_secs(5),
        };

        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}), &test_ctx())
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content["stdout"], "hello\n");
        assert_eq!(result.content["exit_code"], 0);
    }

    #[tokio::test]
    async fn bash_exec_respects_allowlist() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = BashExec {
            sandbox,
            command_allowlist: Some(Arc::from(vec!["ls".to_string()].into_boxed_slice())),
            timeout: Duration::from_secs(5),
        };

        let result = tool
            .execute(serde_json::json!({"command": "rm -rf /"}), &test_ctx())
            .await;
        assert!(result.is_error);
        assert!(
            result.content["error"]
                .as_str()
                .unwrap()
                .contains("not in the allowlist")
        );
    }

    #[tokio::test]
    async fn bash_exec_timeout() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = BashExec {
            sandbox,
            command_allowlist: None,
            timeout: Duration::from_millis(100),
        };

        let result = tool
            .execute(serde_json::json!({"command": "sleep 10"}), &test_ctx())
            .await;
        assert!(result.is_error);
        assert!(
            result.content["error"]
                .as_str()
                .unwrap()
                .contains("timed out")
        );
    }

    #[tokio::test]
    async fn bash_exec_does_not_leak_api_keys() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = BashExec {
            sandbox,
            command_allowlist: None,
            timeout: Duration::from_secs(5),
        };

        // Set a fake API key in the environment
        // SAFETY: test-only, single-threaded test
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-test-secret") };

        let result = tool
            .execute(
                serde_json::json!({"command": "printenv ANTHROPIC_API_KEY || echo NOT_SET"}),
                &test_ctx(),
            )
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content["stdout"], "NOT_SET\n");

        // SAFETY: test-only, single-threaded test
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    }

    // -- file_read tests --

    #[tokio::test]
    async fn file_read_success() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = FileRead { sandbox };

        let result = tool
            .execute(serde_json::json!({"path": "hello.txt"}), &test_ctx())
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content["content"], "hello world");
    }

    #[tokio::test]
    async fn file_read_rejects_memory_dir() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = FileRead { sandbox };

        let result = tool
            .execute(serde_json::json!({"path": "memory/core.md"}), &test_ctx())
            .await;
        assert!(result.is_error);
        assert!(result.content["error"].as_str().unwrap().contains("memory"));
    }

    #[tokio::test]
    async fn file_read_rejects_traversal() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = FileRead { sandbox };

        let result = tool
            .execute(serde_json::json!({"path": "../../etc/passwd"}), &test_ctx())
            .await;
        assert!(result.is_error);
    }

    // -- file_write tests --

    #[tokio::test]
    async fn file_write_creates_file() {
        let (tmp, sandbox) = setup_sandbox();
        let tool = FileWrite { sandbox };

        let result = tool
            .execute(
                serde_json::json!({"path": "new_file.txt", "content": "written"}),
                &test_ctx(),
            )
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content["bytes_written"], 7);

        let content = fs::read_to_string(tmp.path().join("new_file.txt")).unwrap();
        assert_eq!(content, "written");
    }

    #[tokio::test]
    async fn file_write_creates_parent_dirs() {
        let (tmp, sandbox) = setup_sandbox();
        let tool = FileWrite { sandbox };

        let result = tool
            .execute(
                serde_json::json!({"path": "deep/nested/file.txt", "content": "deep"}),
                &test_ctx(),
            )
            .await;
        assert!(!result.is_error);

        let content = fs::read_to_string(tmp.path().join("deep/nested/file.txt")).unwrap();
        assert_eq!(content, "deep");
    }

    #[tokio::test]
    async fn file_write_rejects_memory_dir() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = FileWrite { sandbox };

        let result = tool
            .execute(
                serde_json::json!({"path": "memory/secret.md", "content": "evil"}),
                &test_ctx(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content["error"].as_str().unwrap().contains("memory"));
    }

    #[tokio::test]
    async fn file_write_rejects_traversal() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = FileWrite { sandbox };

        let result = tool
            .execute(
                serde_json::json!({"path": "../../etc/evil", "content": "bad"}),
                &test_ctx(),
            )
            .await;
        assert!(result.is_error);
    }

    // -- file_list tests --

    #[tokio::test]
    async fn file_list_root() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = FileList { sandbox };

        let result = tool
            .execute(serde_json::json!({"path": "."}), &test_ctx())
            .await;
        assert!(!result.is_error);
        let entries = result.content["entries"].as_array().unwrap();
        let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        assert!(names.contains(&"hello.txt"));
        assert!(names.contains(&"subdir"));
        assert!(names.contains(&"memory"));
    }

    #[tokio::test]
    async fn file_list_recursive() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = FileList { sandbox };

        let result = tool
            .execute(
                serde_json::json!({"path": "subdir", "recursive": true}),
                &test_ctx(),
            )
            .await;
        assert!(!result.is_error);
        let entries = result.content["entries"].as_array().unwrap();
        let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        assert!(names.contains(&"nested.txt"));
    }

    #[tokio::test]
    async fn file_list_rejects_memory_dir() {
        let (_tmp, sandbox) = setup_sandbox();
        let tool = FileList { sandbox };

        let result = tool
            .execute(serde_json::json!({"path": "memory"}), &test_ctx())
            .await;
        assert!(result.is_error);
        assert!(result.content["error"].as_str().unwrap().contains("memory"));
    }

    // -- registration test --

    #[test]
    fn register_computer_tools_adds_four() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join("memory")).expect("mkdir");
        let sandbox = Arc::new(Sandbox::new(tmp.path().to_path_buf(), "memory"));
        let config = ComputerUseConfig::default();
        let mut registry = ToolRegistry::new();

        register_computer_tools(&mut registry, sandbox, &config);

        assert_eq!(registry.tool_count(), 4);
        assert!(registry.has_tool("bash_exec"));
        assert!(registry.has_tool("file_read"));
        assert!(registry.has_tool("file_write"));
        assert!(registry.has_tool("file_list"));
    }
}
