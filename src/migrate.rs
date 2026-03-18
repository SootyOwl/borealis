//! One-time migration from Letta (MemGPT) JSON exports to Borealis storage.
//!
//! Reads JSON files previously exported from Letta's REST API and imports:
//! - Core memory blocks → `core.md` (persona block) + note rows (other blocks)
//! - Archival memory → note rows with `letta-archival` tag
//! - Conversation history (optional) → messages in the history store

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use crate::history::{schema as history_schema, store::HistoryStore};
use crate::memory::MemoryStore;
use crate::types::{ChatMessage, ConversationId, ConversationMode, ToolCall};

// ---------------------------------------------------------------------------
// Letta JSON structures — core memory
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LettaMemory {
    pub blocks: Vec<LettaBlock>,
}

#[derive(Debug, Deserialize)]
pub struct LettaBlock {
    #[serde(default)]
    pub id: Option<String>,
    pub value: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Letta JSON structures — archival memory (passages)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LettaPassage {
    #[serde(default)]
    pub id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Letta JSON structures — messages
// ---------------------------------------------------------------------------

/// Letta messages are a tagged union keyed by `message_type`.
/// We only deserialize the fields we need for migration.
#[derive(Debug, Deserialize)]
pub struct LettaMessageRaw {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    pub message_type: String,

    // user_message / assistant_message / system_message
    #[serde(default)]
    pub content: Option<Value>,

    // tool_call_message
    #[serde(default)]
    pub tool_call: Option<LettaToolCall>,

    // tool_return_message
    #[serde(default)]
    pub tool_return: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,

    // Ordering
    #[serde(default)]
    pub step_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LettaToolCall {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<LettaFunction>,
}

#[derive(Debug, Deserialize)]
pub struct LettaFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// ---------------------------------------------------------------------------
// Migration result
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct MigrationStats {
    pub core_blocks_imported: usize,
    pub persona_updated: bool,
    pub archival_notes_imported: usize,
    pub messages_imported: usize,
    pub messages_skipped: usize,
}

impl std::fmt::Display for MigrationStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Migration complete:")?;
        writeln!(
            f,
            "  Core memory blocks imported: {}",
            self.core_blocks_imported
        )?;
        writeln!(f, "  Persona (core.md) updated:   {}", self.persona_updated)?;
        writeln!(
            f,
            "  Archival notes imported:      {}",
            self.archival_notes_imported
        )?;
        writeln!(
            f,
            "  Messages imported:            {}",
            self.messages_imported
        )?;
        writeln!(
            f,
            "  Messages skipped:             {}",
            self.messages_skipped
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the full Letta migration.
///
/// `source_dir` should contain any combination of:
/// - `core_memory.json`  — Letta core memory export
/// - `archival_memory.json` — Letta archival memory export
/// - `messages.json` — Letta conversation history export
///
/// `db_path` is the path to the SQLite database.
/// `core_md_path` is the path to `memory/core.md`.
pub fn run_migration(
    source_dir: &Path,
    db_path: &Path,
    core_md_path: &Path,
) -> Result<MigrationStats> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;
    let conn = Arc::new(Mutex::new(conn));

    let memory_store = MemoryStore::new(Arc::clone(&conn), core_md_path.to_path_buf())
        .context("failed to initialize memory store")?;

    // Initialize history schema (creates tables if needed).
    {
        let c = conn.lock().expect("mutex poisoned");
        history_schema::initialize(&c).context("failed to initialize history schema")?;
    }
    let history_store = HistoryStore::new(Arc::clone(&conn));

    let mut stats = MigrationStats::default();

    // --- Core memory ---
    let core_path = source_dir.join("core_memory.json");
    if core_path.exists() {
        info!(path = %core_path.display(), "importing core memory");
        let data = std::fs::read_to_string(&core_path)
            .with_context(|| format!("failed to read {}", core_path.display()))?;
        let memory: LettaMemory = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", core_path.display()))?;
        import_core_memory(&memory_store, &memory, &mut stats)?;
    } else {
        info!("no core_memory.json found, skipping core memory import");
    }

    // --- Archival memory ---
    let archival_path = source_dir.join("archival_memory.json");
    if archival_path.exists() {
        info!(path = %archival_path.display(), "importing archival memory");
        let data = std::fs::read_to_string(&archival_path)
            .with_context(|| format!("failed to read {}", archival_path.display()))?;
        let passages: Vec<LettaPassage> = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", archival_path.display()))?;
        import_archival_memory(&memory_store, &passages, &mut stats)?;
    } else {
        info!("no archival_memory.json found, skipping archival memory import");
    }

    // --- Conversation history ---
    let messages_path = source_dir.join("messages.json");
    if messages_path.exists() {
        info!(path = %messages_path.display(), "importing conversation history");
        let data = std::fs::read_to_string(&messages_path)
            .with_context(|| format!("failed to read {}", messages_path.display()))?;
        let messages: Vec<LettaMessageRaw> = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", messages_path.display()))?;
        import_messages(&history_store, &messages, &mut stats)?;
    } else {
        info!("no messages.json found, skipping conversation history import");
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Core memory import
// ---------------------------------------------------------------------------

fn import_core_memory(
    store: &MemoryStore,
    memory: &LettaMemory,
    stats: &mut MigrationStats,
) -> Result<()> {
    for block in &memory.blocks {
        let label = block.label.as_deref().unwrap_or("unknown");

        if label == "persona" {
            // Persona block → overwrite core.md
            store
                .update_note("core", &block.value)
                .context("failed to update core.md with persona block")?;
            stats.persona_updated = true;
            info!(label, "persona block → core.md");
        } else {
            // Other blocks (human, custom) → note rows
            let title = format!("Letta core: {label}");
            let mut tags = vec!["letta-core".to_string(), label.to_string()];
            if let Some(ref desc) = block.description {
                // Truncate long descriptions for tag use
                if desc.len() <= 50 {
                    tags.push(desc.clone());
                }
            }
            store
                .create_note(&title, &block.value, &tags)
                .with_context(|| format!("failed to create note for block '{label}'"))?;
            info!(label, "core block → note row");
        }
        stats.core_blocks_imported += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Archival memory import
// ---------------------------------------------------------------------------

fn import_archival_memory(
    store: &MemoryStore,
    passages: &[LettaPassage],
    stats: &mut MigrationStats,
) -> Result<()> {
    for (i, passage) in passages.iter().enumerate() {
        let title = format!(
            "Letta archival #{}",
            passage.id.as_deref().unwrap_or(&format!("{}", i + 1))
        );

        let mut tags = vec!["letta-archival".to_string()];

        // Extract string tags from metadata if present
        if let Some(Value::Object(meta)) = &passage.metadata {
            if let Some(Value::Array(arr)) = meta.get("tags") {
                for v in arr {
                    if let Value::String(s) = v {
                        tags.push(s.clone());
                    }
                }
            }
        }

        store
            .create_note(&title, &passage.text, &tags)
            .with_context(|| format!("failed to create note for passage {}", i + 1))?;
        stats.archival_notes_imported += 1;
    }

    info!(count = passages.len(), "archival memory import complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Conversation history import
// ---------------------------------------------------------------------------

/// Extract the agent ID from the first message that has one, or use a default.
fn extract_agent_id(messages: &[LettaMessageRaw]) -> String {
    for msg in messages {
        if let Some(ref id) = msg.id {
            // Letta message IDs are like "message-<uuid>", try to use the
            // agent_id from the metadata if available, else use a hash of
            // the first message ID for uniqueness.
            return format!("{:08x}", fxhash(id.as_bytes()));
        }
    }
    "unknown".to_string()
}

/// Simple FNV-1a-inspired hash for generating a short identifier.
fn fxhash(data: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in data {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn import_messages(
    history_store: &HistoryStore,
    messages: &[LettaMessageRaw],
    stats: &mut MigrationStats,
) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }

    let agent_hash = extract_agent_id(messages);
    let conv_id = ConversationId::DM {
        channel_type: "letta".to_string(),
        user_id: agent_hash,
    };

    history_store
        .ensure_conversation(&conv_id, ConversationMode::Shared)
        .context("failed to create conversation for Letta history")?;

    // Group messages by step_id into turns. Messages with the same step_id
    // belong to the same turn. Messages without a step_id get their own turn.
    let mut current_turn_id: Option<String> = None;
    let mut current_step: Option<String> = None;

    for msg in messages {
        let chat_msg = match convert_letta_message(msg) {
            Some(m) => m,
            None => {
                stats.messages_skipped += 1;
                continue;
            }
        };

        // Determine if this belongs to the current turn or starts a new one.
        let reuse_turn =
            matches!((&current_step, &msg.step_id), (Some(cur), Some(step)) if cur == step);

        let turn_id = if reuse_turn {
            current_turn_id.as_deref()
        } else {
            None
        };

        let used_turn = history_store
            .append_message(&conv_id, &chat_msg, turn_id)
            .context("failed to append migrated message")?;

        current_turn_id = Some(used_turn);
        current_step = msg.step_id.clone();
        stats.messages_imported += 1;
    }

    info!(
        imported = stats.messages_imported,
        skipped = stats.messages_skipped,
        "conversation history import complete"
    );
    Ok(())
}

/// Convert a Letta message to a Borealis ChatMessage, or None to skip it.
fn convert_letta_message(msg: &LettaMessageRaw) -> Option<ChatMessage> {
    match msg.message_type.as_str() {
        "user_message" => {
            let content = extract_content_string(&msg.content)?;
            Some(ChatMessage::user(content))
        }
        "assistant_message" => {
            let content = extract_content_string(&msg.content)?;
            Some(ChatMessage::assistant(content))
        }
        "tool_call_message" => {
            let tc = msg.tool_call.as_ref()?;
            let func = tc.function.as_ref()?;
            let name = func.name.as_deref().unwrap_or("unknown");

            // If this is a send_message call, extract the message text as
            // assistant content rather than treating it as a tool call.
            if name == "send_message" {
                if let Some(ref args_str) = func.arguments {
                    if let Ok(args) = serde_json::from_str::<Value>(args_str) {
                        if let Some(text) = args.get("message").and_then(|v| v.as_str()) {
                            return Some(ChatMessage::assistant(text));
                        }
                    }
                }
            }

            // Generic tool call
            let arguments: Value = func
                .arguments
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);

            let tool_call = ToolCall {
                id: tc.id.clone().unwrap_or_default(),
                name: name.to_string(),
                arguments,
            };

            Some(ChatMessage::assistant_with_tool_calls(
                String::new(),
                vec![tool_call],
            ))
        }
        "tool_return_message" => {
            let call_id = msg.tool_call_id.as_deref().unwrap_or("");
            let content = msg.tool_return.as_deref().unwrap_or("");
            Some(ChatMessage::tool_result(call_id, content))
        }
        other => {
            warn!(
                message_type = other,
                "skipping unsupported Letta message type"
            );
            None
        }
    }
}

/// Extract a plain string from a Letta `content` field, which can be either
/// a JSON string or an array of content parts.
fn extract_content_string(content: &Option<Value>) -> Option<String> {
    match content.as_ref()? {
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Value::Array(parts) => {
            // Concatenate text parts
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join(""))
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, TempDir) {
        let source_dir = TempDir::new().unwrap();
        let data_dir = TempDir::new().unwrap();
        // Create initial core.md
        std::fs::write(
            data_dir.path().join("core.md"),
            "# Original Persona\nBefore migration.",
        )
        .unwrap();
        (source_dir, data_dir)
    }

    // --- Core memory tests ---

    #[test]
    fn migrate_core_memory_persona_updates_core_md() {
        let (source_dir, data_dir) = setup_test_env();
        let core_json = serde_json::json!({
            "blocks": [
                {
                    "id": "block-1",
                    "value": "I am Aurora, a digital person.",
                    "label": "persona",
                    "description": "Agent persona"
                },
                {
                    "id": "block-2",
                    "value": "The human is Tyto, who likes Rust.",
                    "label": "human",
                    "description": "Info about the human"
                }
            ]
        });
        std::fs::write(
            source_dir.path().join("core_memory.json"),
            serde_json::to_string(&core_json).unwrap(),
        )
        .unwrap();

        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        let stats = run_migration(source_dir.path(), &db_path, &core_md).unwrap();

        assert_eq!(stats.core_blocks_imported, 2);
        assert!(stats.persona_updated);

        // core.md should have the persona content
        let content = std::fs::read_to_string(&core_md).unwrap();
        assert!(content.contains("I am Aurora"));

        // Human block should be a note row
        let conn = Connection::open(&db_path).unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let store = MemoryStore::new(conn, core_md).unwrap();
        let notes = store.list_notes(Some("human")).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].content.contains("Tyto"));
    }

    #[test]
    fn migrate_core_memory_no_persona_block() {
        let (source_dir, data_dir) = setup_test_env();
        let core_json = serde_json::json!({
            "blocks": [
                {
                    "value": "Custom block content",
                    "label": "custom_data"
                }
            ]
        });
        std::fs::write(
            source_dir.path().join("core_memory.json"),
            serde_json::to_string(&core_json).unwrap(),
        )
        .unwrap();

        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        let stats = run_migration(source_dir.path(), &db_path, &core_md).unwrap();

        assert_eq!(stats.core_blocks_imported, 1);
        assert!(!stats.persona_updated);

        // core.md should be unchanged
        let content = std::fs::read_to_string(&core_md).unwrap();
        assert!(content.contains("Original Persona"));
    }

    // --- Archival memory tests ---

    #[test]
    fn migrate_archival_memory_creates_notes() {
        let (source_dir, data_dir) = setup_test_env();
        let archival_json = serde_json::json!([
            {
                "id": "passage-abc",
                "text": "Important fact about the user's cat.",
                "created_at": "2025-01-15T12:00:00Z"
            },
            {
                "id": "passage-def",
                "text": "The user prefers dark mode.",
                "metadata": {
                    "tags": ["preference", "ui"]
                },
                "created_at": "2025-02-01T08:30:00Z"
            }
        ]);
        std::fs::write(
            source_dir.path().join("archival_memory.json"),
            serde_json::to_string(&archival_json).unwrap(),
        )
        .unwrap();

        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        let stats = run_migration(source_dir.path(), &db_path, &core_md).unwrap();

        assert_eq!(stats.archival_notes_imported, 2);

        // Verify notes exist
        let conn = Connection::open(&db_path).unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let store = MemoryStore::new(conn, core_md).unwrap();
        let notes = store.list_notes(Some("letta-archival")).unwrap();
        assert_eq!(notes.len(), 2);

        // Check that metadata tags were preserved
        let dark_mode = notes
            .iter()
            .find(|n| n.content.contains("dark mode"))
            .unwrap();
        assert!(dark_mode.tags.contains(&"preference".to_string()));
        assert!(dark_mode.tags.contains(&"ui".to_string()));
    }

    // --- Message history tests ---

    #[test]
    fn migrate_messages_imports_conversation() {
        let (source_dir, data_dir) = setup_test_env();
        let messages_json = serde_json::json!([
            {
                "id": "message-001",
                "date": "2025-01-10T10:00:00Z",
                "message_type": "user_message",
                "content": "Hello Aurora!",
                "step_id": "step-1"
            },
            {
                "id": "message-002",
                "date": "2025-01-10T10:00:01Z",
                "message_type": "tool_call_message",
                "tool_call": {
                    "id": "call_1",
                    "function": {
                        "name": "send_message",
                        "arguments": "{\"message\": \"Hi there! How are you?\"}"
                    }
                },
                "step_id": "step-1"
            },
            {
                "id": "message-003",
                "date": "2025-01-10T10:00:02Z",
                "message_type": "reasoning_message",
                "content": "internal reasoning",
                "step_id": "step-1"
            }
        ]);
        std::fs::write(
            source_dir.path().join("messages.json"),
            serde_json::to_string(&messages_json).unwrap(),
        )
        .unwrap();

        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        let stats = run_migration(source_dir.path(), &db_path, &core_md).unwrap();

        // user_message + send_message (converted to assistant) = 2 imported
        // reasoning_message = 1 skipped
        assert_eq!(stats.messages_imported, 2);
        assert_eq!(stats.messages_skipped, 1);
    }

    #[test]
    fn migrate_tool_call_non_send_message() {
        let (source_dir, data_dir) = setup_test_env();
        let messages_json = serde_json::json!([
            {
                "id": "message-010",
                "date": "2025-01-10T10:00:00Z",
                "message_type": "tool_call_message",
                "tool_call": {
                    "id": "call_2",
                    "function": {
                        "name": "archival_memory_insert",
                        "arguments": "{\"content\": \"some memory\"}"
                    }
                },
                "step_id": "step-2"
            },
            {
                "id": "message-011",
                "date": "2025-01-10T10:00:01Z",
                "message_type": "tool_return_message",
                "tool_return": "Memory inserted.",
                "status": "success",
                "tool_call_id": "call_2",
                "step_id": "step-2"
            }
        ]);
        std::fs::write(
            source_dir.path().join("messages.json"),
            serde_json::to_string(&messages_json).unwrap(),
        )
        .unwrap();

        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        let stats = run_migration(source_dir.path(), &db_path, &core_md).unwrap();

        assert_eq!(stats.messages_imported, 2);
        assert_eq!(stats.messages_skipped, 0);
    }

    #[test]
    fn migrate_no_files_returns_empty_stats() {
        let (source_dir, data_dir) = setup_test_env();
        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        let stats = run_migration(source_dir.path(), &db_path, &core_md).unwrap();

        assert_eq!(stats.core_blocks_imported, 0);
        assert!(!stats.persona_updated);
        assert_eq!(stats.archival_notes_imported, 0);
        assert_eq!(stats.messages_imported, 0);
        assert_eq!(stats.messages_skipped, 0);
    }

    #[test]
    fn migrate_content_array_extraction() {
        let content = Some(serde_json::json!([
            {"type": "text", "text": "Hello "},
            {"type": "text", "text": "world!"}
        ]));
        let result = extract_content_string(&content);
        assert_eq!(result, Some("Hello world!".to_string()));
    }

    #[test]
    fn migrate_content_string_extraction() {
        let content = Some(serde_json::json!("Simple string"));
        let result = extract_content_string(&content);
        assert_eq!(result, Some("Simple string".to_string()));
    }

    #[test]
    fn migrate_content_empty_string_returns_none() {
        let content = Some(serde_json::json!(""));
        let result = extract_content_string(&content);
        assert!(result.is_none());
    }

    #[test]
    fn migrate_content_null_returns_none() {
        let result = extract_content_string(&None);
        assert!(result.is_none());
    }

    #[test]
    fn fxhash_deterministic() {
        let h1 = fxhash(b"test-data");
        let h2 = fxhash(b"test-data");
        assert_eq!(h1, h2);

        let h3 = fxhash(b"different");
        assert_ne!(h1, h3);
    }
}
