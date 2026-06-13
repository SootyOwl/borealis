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
use crate::memory::{Memory, SqliteMemory, normalize_tag};
use crate::types::{ChannelSource, ChatMessage, ConversationId, ConversationMode, ToolCall};

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
    /// Messages skipped because their `message_type` is unsupported (e.g.
    /// `reasoning_message`). NOT a dedup counter — see `messages_skipped_existing`.
    pub messages_skipped: usize,

    // --- Idempotent re-import skip counters (MIG-2) ---
    /// Non-persona core blocks skipped because they were already imported on a
    /// previous run (matched in `letta_imported`).
    pub core_blocks_skipped: usize,
    /// Archival passages skipped because they were already imported.
    pub archival_notes_skipped: usize,
    /// Messages skipped because they were already imported (distinct from
    /// `messages_skipped`, which counts unsupported types).
    pub messages_skipped_existing: usize,
}

impl std::fmt::Display for MigrationStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Migration complete:")?;
        writeln!(
            f,
            "  Core memory blocks imported: {}",
            self.core_blocks_imported
        )?;
        writeln!(
            f,
            "  Core blocks skipped (already imported): {}",
            self.core_blocks_skipped
        )?;
        writeln!(f, "  Persona (core.md) updated:   {}", self.persona_updated)?;
        writeln!(
            f,
            "  Archival notes imported:      {}",
            self.archival_notes_imported
        )?;
        writeln!(
            f,
            "  Archival notes skipped (already imported): {}",
            self.archival_notes_skipped
        )?;
        writeln!(
            f,
            "  Messages imported:            {}",
            self.messages_imported
        )?;
        writeln!(
            f,
            "  Messages skipped (unsupported type): {}",
            self.messages_skipped
        )?;
        writeln!(
            f,
            "  Messages skipped (already imported): {}",
            self.messages_skipped_existing
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Provenance tracking (MIG-2: idempotent re-import)
// ---------------------------------------------------------------------------

/// Tracks which Letta source rows have already been imported, so a re-run
/// against an updated export skips what is already present rather than
/// duplicating it.
///
/// Backed by a migration-owned `letta_imported` table (created in
/// [`run_migration`]). This deliberately does NOT touch the `notes` or
/// `messages` schemas — provenance is recorded out-of-band keyed by the Letta
/// source id (`passage.id` / `msg.id` / `block.id`).
///
/// Wiring choice: the import functions already receive `&SqliteMemory` /
/// `&HistoryStore`, both of which wrap the SAME `Arc<Mutex<Connection>>` that
/// `run_migration` owns. Rather than reach through the stores, we hand each
/// import function a `&Provenance` that holds its own clone of that `Arc`. This
/// keeps the dedup concern in one small, self-contained type, leaves the store
/// APIs untouched, and never deadlocks: `is_imported`/`record_imported` acquire
/// and release the lock around a single statement each, and are only called
/// *between* store calls (never while a store guard is held).
struct Provenance {
    conn: Arc<Mutex<Connection>>,
}

impl Provenance {
    fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| {
            warn!("migration provenance connection mutex was poisoned; recovering");
            poisoned.into_inner()
        })
    }

    /// True if `source_id` was already recorded as imported on a prior run.
    fn is_imported(&self, source_id: &str) -> Result<bool> {
        let conn = self.lock();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM letta_imported WHERE source_id = ?1)",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .context("failed to query letta_imported")?;
        Ok(exists)
    }

    /// Record `source_id` (of the given `kind`) as imported, stamping the
    /// current time. `INSERT OR IGNORE` keeps this safe even if a row id
    /// somehow appears twice within a single run.
    fn record_imported(&self, source_id: &str, kind: &str) -> Result<()> {
        let conn = self.lock();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR IGNORE INTO letta_imported (source_id, kind, imported_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![source_id, kind, now],
        )
        .context("failed to record letta_imported row")?;
        Ok(())
    }
}

/// Resolve a Letta-supplied timestamp into a value to persist.
///
/// - `Some(ts)` that parses as RFC 3339 → returned as-is (callers that store it
///   in the notes table get it normalized to canonical millis there; the
///   messages table stores it verbatim, which is acceptable for ordering).
/// - `Some(ts)` that does NOT parse → fall back to now() with a `warn!`.
/// - `None` (field absent in the export) → fall back to now() silently.
///
/// We validate leniently rather than rejecting: a migration should never abort
/// because one row has a malformed date.
fn resolve_letta_timestamp(raw: Option<&str>, context: &str) -> String {
    match raw {
        Some(ts) => {
            if chrono::DateTime::parse_from_rfc3339(ts).is_ok() {
                ts.to_string()
            } else {
                warn!(
                    raw = ts,
                    context, "unparseable Letta timestamp; falling back to now()"
                );
                chrono::Utc::now().to_rfc3339()
            }
        }
        None => chrono::Utc::now().to_rfc3339(),
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

    let memory_store = SqliteMemory::new(Arc::clone(&conn), core_md_path.to_path_buf())
        .context("failed to initialize memory store")?;

    // Initialize history schema (creates tables if needed).
    {
        let c = conn.lock().expect("mutex poisoned");
        history_schema::initialize(&c).context("failed to initialize history schema")?;

        // Migration-owned provenance table for idempotent re-imports (MIG-2).
        // Deliberately separate from the notes/messages schemas — no ALTER TABLE.
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS letta_imported (
                 source_id   TEXT PRIMARY KEY,
                 kind        TEXT NOT NULL,
                 imported_at TEXT NOT NULL
             );",
        )
        .context("failed to initialize letta_imported provenance table")?;
    }
    let history_store = HistoryStore::new(Arc::clone(&conn));
    let provenance = Provenance::new(Arc::clone(&conn));

    let mut stats = MigrationStats::default();

    // --- Core memory ---
    let core_path = source_dir.join("core_memory.json");
    if core_path.exists() {
        info!(path = %core_path.display(), "importing core memory");
        let data = std::fs::read_to_string(&core_path)
            .with_context(|| format!("failed to read {}", core_path.display()))?;
        let memory: LettaMemory = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", core_path.display()))?;
        import_core_memory(&memory_store, &provenance, &memory, &mut stats)?;
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
        import_archival_memory(&memory_store, &provenance, &passages, &mut stats)?;
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
        import_messages(&history_store, &provenance, &messages, &mut stats)?;
    } else {
        info!("no messages.json found, skipping conversation history import");
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Tag sanitization
// ---------------------------------------------------------------------------

/// Best-effort coercion of an arbitrary Letta string into a valid Borealis tag.
///
/// Letta tag/description strings are free-form (spaces, capitals, punctuation).
/// Borealis tags require `[a-z0-9._-]` segments separated by `/`. We:
/// 1. Lowercase the input.
/// 2. Replace whitespace runs with `-`.
/// 3. Drop any character that isn't allowed (or `/` for nesting).
/// 4. Collapse repeated slashes.
/// 5. Validate the result via `normalize_tag`.
///
/// Returns `None` (with a warning) if nothing salvageable remains.
fn sanitize_letta_tag(raw: &str) -> Option<String> {
    let lowered = raw.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut prev_dash = false;
    for c in lowered.chars() {
        let mapped = match c {
            'a'..='z' | '0'..='9' | '.' | '_' | '-' | '/' => Some(c),
            c if c.is_whitespace() => Some('-'),
            _ => None,
        };
        if let Some(ch) = mapped {
            // Collapse runs of dashes — both whitespace-derived and literal.
            // Two consecutive dashes anywhere in the input become one.
            if ch == '-' && prev_dash {
                continue;
            }
            prev_dash = ch == '-';
            out.push(ch);
        }
    }
    // Collapse `//` runs.
    while out.contains("//") {
        out = out.replace("//", "/");
    }
    // Strip leading/trailing dashes that would otherwise survive normalization
    // (e.g. whitespace-only input would become a literal `-` tag).
    let out = out.trim_matches('-').to_string();
    match normalize_tag(&out) {
        Ok(t) => Some(t),
        Err(e) => {
            warn!(raw, error = %e, "dropping unsalvageable Letta tag");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Core memory import
// ---------------------------------------------------------------------------

fn import_core_memory(
    store: &SqliteMemory,
    provenance: &Provenance,
    memory: &LettaMemory,
    stats: &mut MigrationStats,
) -> Result<()> {
    for block in &memory.blocks {
        let label = block.label.as_deref().unwrap_or("unknown");

        if label == "persona" {
            // Persona block → overwrite core.md. This is idempotent by nature
            // (it overwrites a single file), so it is NOT gated on provenance:
            // a re-run simply re-applies the latest persona text.
            store
                .update_note("core", &block.value)
                .context("failed to update core.md with persona block")?;
            stats.persona_updated = true;
            stats.core_blocks_imported += 1;
            info!(label, "persona block → core.md");
        } else {
            // Other blocks (human, custom) → note rows.
            //
            // Dedup on the Letta block id when present. Blocks with no id
            // (`id: None`) are imported unconditionally and NOT recorded — we
            // cannot dedup without a stable key. In practice Letta blocks carry
            // ids, so a re-run only re-imports id-less blocks (rare).
            if let Some(id) = block.id.as_deref() {
                if provenance.is_imported(id)? {
                    stats.core_blocks_skipped += 1;
                    info!(label, id, "core block already imported → skipping");
                    continue;
                }
            }

            let title = format!("Letta core: {label}");
            let mut tags: Vec<String> = ["letta-core", label]
                .iter()
                .filter_map(|t| sanitize_letta_tag(t))
                .collect();
            if let Some(ref desc) = block.description {
                // Truncate long descriptions for tag use
                if desc.len() <= 50 {
                    if let Some(t) = sanitize_letta_tag(desc) {
                        tags.push(t);
                    }
                }
            }
            store
                .create_note(&title, &block.value, &tags)
                .with_context(|| format!("failed to create note for block '{label}'"))?;
            if let Some(id) = block.id.as_deref() {
                provenance.record_imported(id, "core_block")?;
            }
            info!(label, "core block → note row");
            stats.core_blocks_imported += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Archival memory import
// ---------------------------------------------------------------------------

fn import_archival_memory(
    store: &SqliteMemory,
    provenance: &Provenance,
    passages: &[LettaPassage],
    stats: &mut MigrationStats,
) -> Result<()> {
    for (i, passage) in passages.iter().enumerate() {
        // Dedup on the Letta passage id when present. Passages with no id
        // (`id: None`) are imported unconditionally and NOT recorded — we
        // cannot dedup without a stable key, so a re-run would re-import such
        // rows. In practice Letta passages always carry ids.
        if let Some(id) = passage.id.as_deref() {
            if provenance.is_imported(id)? {
                stats.archival_notes_skipped += 1;
                continue;
            }
        }

        let title = format!(
            "Letta archival #{}",
            passage.id.as_deref().unwrap_or(&format!("{}", i + 1))
        );

        let mut tags: Vec<String> = sanitize_letta_tag("letta-archival").into_iter().collect();

        // Extract string tags from metadata if present
        if let Some(Value::Object(meta)) = &passage.metadata {
            if let Some(Value::Array(arr)) = meta.get("tags") {
                for v in arr {
                    if let Value::String(s) = v {
                        if let Some(t) = sanitize_letta_tag(s) {
                            tags.push(t);
                        }
                    }
                }
            }
        }

        // Preserve the original Letta timestamp; fall back to now() if absent
        // or unparseable.
        let created_at = resolve_letta_timestamp(passage.created_at.as_deref(), "archival passage");

        store
            .create_note_at(&title, &passage.text, &tags, &created_at)
            .with_context(|| format!("failed to create note for passage {}", i + 1))?;
        if let Some(id) = passage.id.as_deref() {
            provenance.record_imported(id, "passage")?;
        }
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
    provenance: &Provenance,
    messages: &[LettaMessageRaw],
    stats: &mut MigrationStats,
) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }

    let agent_hash = extract_agent_id(messages);
    let conv_id = ConversationId::Dm {
        channel_type: ChannelSource::Letta,
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
        // Dedup on the Letta message id when present. Messages with no id
        // (`id: None`) are imported unconditionally and NOT recorded — we
        // cannot dedup without a stable key, so a re-run would re-import such
        // rows. In practice Letta messages always carry ids.
        //
        // The skip happens BEFORE conversion and BEFORE the turn-tracking
        // update, so an already-imported message neither re-inserts nor
        // disturbs the step→turn grouping of the messages that follow it.
        if let Some(id) = msg.id.as_deref() {
            if provenance.is_imported(id)? {
                stats.messages_skipped_existing += 1;
                continue;
            }
        }

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

        // Preserve the original Letta timestamp; fall back to now() if absent
        // or unparseable.
        let created_at = resolve_letta_timestamp(msg.date.as_deref(), "message");

        let used_turn = history_store
            .append_message_at(&conv_id, &chat_msg, turn_id, &created_at)
            .context("failed to append migrated message")?;

        if let Some(id) = msg.id.as_deref() {
            provenance.record_imported(id, "message")?;
        }

        current_turn_id = Some(used_turn);
        current_step = msg.step_id.clone();
        stats.messages_imported += 1;
    }

    info!(
        imported = stats.messages_imported,
        skipped = stats.messages_skipped,
        skipped_existing = stats.messages_skipped_existing,
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
    use rusqlite::params;
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

    // --- sanitize_letta_tag direct unit tests ---

    #[test]
    fn sanitize_passes_already_valid_tag() {
        assert_eq!(sanitize_letta_tag("letta-archival").as_deref(), Some("letta-archival"));
        assert_eq!(sanitize_letta_tag("recipes/italian").as_deref(), Some("recipes/italian"));
    }

    #[test]
    fn sanitize_lowercases_and_replaces_spaces() {
        assert_eq!(
            sanitize_letta_tag("Human Profile").as_deref(),
            Some("human-profile")
        );
        assert_eq!(
            sanitize_letta_tag("CUSTOM_DATA").as_deref(),
            Some("custom_data")
        );
    }

    #[test]
    fn sanitize_drops_disallowed_characters() {
        assert_eq!(sanitize_letta_tag("block#3").as_deref(), Some("block3"));
        assert_eq!(sanitize_letta_tag("foo!bar?baz").as_deref(), Some("foobarbaz"));
        // Disallowed characters at the start and end of the input should be
        // dropped just as cleanly as those in the middle.
        assert_eq!(sanitize_letta_tag("#block").as_deref(), Some("block"));
        assert_eq!(sanitize_letta_tag("foo!").as_deref(), Some("foo"));
        assert_eq!(sanitize_letta_tag("?foo!").as_deref(), Some("foo"));
    }

    #[test]
    fn sanitize_handles_disallowed_char_between_slashes() {
        // `foo/!/bar` — the middle segment is a single disallowed char.
        // After dropping `!` we get `foo//bar`, which the slash-collapse
        // pass reduces to `foo/bar`.
        assert_eq!(
            sanitize_letta_tag("foo/!/bar").as_deref(),
            Some("foo/bar")
        );
    }

    #[test]
    fn sanitize_returns_none_for_slash_only_input() {
        assert_eq!(sanitize_letta_tag("///"), None);
    }

    #[test]
    fn sanitize_collapses_runs_of_dashes() {
        // Multiple spaces in a row should produce one dash, not many.
        assert_eq!(
            sanitize_letta_tag("foo    bar").as_deref(),
            Some("foo-bar")
        );
        // Literal `--` in input also collapses (documented behaviour).
        assert_eq!(
            sanitize_letta_tag("foo--bar").as_deref(),
            Some("foo-bar")
        );
    }

    #[test]
    fn sanitize_collapses_repeated_slashes() {
        assert_eq!(
            sanitize_letta_tag("foo//bar///baz").as_deref(),
            Some("foo/bar/baz")
        );
    }

    #[test]
    fn sanitize_strips_outer_dashes_from_whitespace_input() {
        // Whitespace-only input would otherwise become a literal `-` tag.
        assert_eq!(sanitize_letta_tag("   "), None);
        assert_eq!(sanitize_letta_tag(" foo "), Some("foo".to_string()));
    }

    #[test]
    fn sanitize_returns_none_for_empty_input() {
        assert_eq!(sanitize_letta_tag(""), None);
    }

    #[test]
    fn sanitize_returns_none_for_unsalvageable_input() {
        // Nothing in the allowed character set survives.
        assert_eq!(sanitize_letta_tag("!!!"), None);
        assert_eq!(sanitize_letta_tag("漢字"), None);
    }

    #[test]
    fn sanitize_respects_max_tag_depth() {
        // 9 segments should fail normalize_tag's depth check and return None.
        let too_deep = (1..=9).map(|i| i.to_string()).collect::<Vec<_>>().join("/");
        assert_eq!(sanitize_letta_tag(&too_deep), None);
        // 8 segments should pass.
        let just_ok = (1..=8).map(|i| i.to_string()).collect::<Vec<_>>().join("/");
        assert!(sanitize_letta_tag(&just_ok).is_some());
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
        let store = SqliteMemory::new(conn, core_md).unwrap();
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
        let store = SqliteMemory::new(conn, core_md).unwrap();
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

    // --- MIG-1: original Letta timestamps are preserved ---

    #[test]
    fn migrate_archival_preserves_letta_created_at() {
        let (source_dir, data_dir) = setup_test_env();
        let archival_json = serde_json::json!([
            {
                "id": "passage-ts",
                "text": "A passage with a known created_at.",
                "created_at": "2024-03-04T05:06:07Z"
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
        assert_eq!(stats.archival_notes_imported, 1);

        // The stored note's created_at must equal the Letta value (normalized to
        // canonical millis RFC3339), NOT "now".
        let conn = Connection::open(&db_path).unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let store = SqliteMemory::new(conn, core_md).unwrap();
        let page = store
            .list_notes_paginated(Some("letta-archival"), false, 10, 0)
            .unwrap();
        assert_eq!(page.notes.len(), 1);
        assert_eq!(page.notes[0].created_at, "2024-03-04T05:06:07.000Z");
    }

    #[test]
    fn migrate_archival_without_created_at_falls_back_to_now() {
        let (source_dir, data_dir) = setup_test_env();
        // No created_at field at all.
        let archival_json = serde_json::json!([
            { "id": "passage-no-ts", "text": "No timestamp here." }
        ]);
        std::fs::write(
            source_dir.path().join("archival_memory.json"),
            serde_json::to_string(&archival_json).unwrap(),
        )
        .unwrap();

        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        run_migration(source_dir.path(), &db_path, &core_md).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let store = SqliteMemory::new(conn, core_md).unwrap();
        let page = store
            .list_notes_paginated(Some("letta-archival"), false, 10, 0)
            .unwrap();
        assert_eq!(page.notes.len(), 1);
        // Fallback should produce a recent timestamp (this decade), not 2024.
        assert!(
            page.notes[0].created_at.starts_with("202"),
            "expected an RFC3339 fallback timestamp, got {}",
            page.notes[0].created_at
        );
        assert!(page.notes[0].created_at.as_str() >= "2026-");
    }

    #[test]
    fn migrate_messages_preserve_letta_date() {
        let (source_dir, data_dir) = setup_test_env();
        let messages_json = serde_json::json!([
            {
                "id": "message-ts-1",
                "date": "2023-07-08T09:10:11Z",
                "message_type": "user_message",
                "content": "Message with a known date.",
                "step_id": "step-x"
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
        assert_eq!(stats.messages_imported, 1);

        // Query the message's created_at directly from the DB.
        let conn = Connection::open(&db_path).unwrap();
        let created_at: String = conn
            .query_row(
                "SELECT created_at FROM messages WHERE content = ?1",
                params!["Message with a known date."],
                |row| row.get(0),
            )
            .unwrap();
        // The Letta `date` is stored verbatim by append_message_at.
        assert_eq!(created_at, "2023-07-08T09:10:11Z");
    }

    // --- MIG-2: idempotent re-import ---

    fn count_rows(db_path: &Path, table: &str) -> i64 {
        let conn = Connection::open(db_path).unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn migrate_is_idempotent_on_rerun() {
        let (source_dir, data_dir) = setup_test_env();
        let archival_json = serde_json::json!([
            { "id": "passage-1", "text": "First passage.", "created_at": "2025-01-01T00:00:00Z" },
            { "id": "passage-2", "text": "Second passage.", "created_at": "2025-01-02T00:00:00Z" }
        ]);
        std::fs::write(
            source_dir.path().join("archival_memory.json"),
            serde_json::to_string(&archival_json).unwrap(),
        )
        .unwrap();
        let messages_json = serde_json::json!([
            {
                "id": "message-1",
                "date": "2025-01-01T00:00:00Z",
                "message_type": "user_message",
                "content": "Hello",
                "step_id": "s1"
            },
            {
                "id": "message-2",
                "date": "2025-01-01T00:00:05Z",
                "message_type": "user_message",
                "content": "Again",
                "step_id": "s2"
            }
        ]);
        std::fs::write(
            source_dir.path().join("messages.json"),
            serde_json::to_string(&messages_json).unwrap(),
        )
        .unwrap();

        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        // First run imports everything.
        let s1 = run_migration(source_dir.path(), &db_path, &core_md).unwrap();
        assert_eq!(s1.archival_notes_imported, 2);
        assert_eq!(s1.messages_imported, 2);
        assert_eq!(s1.archival_notes_skipped, 0);
        assert_eq!(s1.messages_skipped_existing, 0);

        let notes_after_1 = count_rows(&db_path, "notes");
        let msgs_after_1 = count_rows(&db_path, "messages");
        assert_eq!(notes_after_1, 2);
        assert_eq!(msgs_after_1, 2);

        // Second run imports NOTHING new — everything is skipped as already imported.
        let s2 = run_migration(source_dir.path(), &db_path, &core_md).unwrap();
        assert_eq!(s2.archival_notes_imported, 0);
        assert_eq!(s2.messages_imported, 0);
        assert_eq!(s2.archival_notes_skipped, 2);
        assert_eq!(s2.messages_skipped_existing, 2);

        // Row counts must NOT have doubled.
        assert_eq!(count_rows(&db_path, "notes"), notes_after_1);
        assert_eq!(count_rows(&db_path, "messages"), msgs_after_1);
    }

    #[test]
    fn migrate_third_run_imports_only_new_rows() {
        let (source_dir, data_dir) = setup_test_env();
        let archival_json = serde_json::json!([
            { "id": "passage-1", "text": "First passage.", "created_at": "2025-01-01T00:00:00Z" }
        ]);
        std::fs::write(
            source_dir.path().join("archival_memory.json"),
            serde_json::to_string(&archival_json).unwrap(),
        )
        .unwrap();
        let messages_json = serde_json::json!([
            {
                "id": "message-1",
                "date": "2025-01-01T00:00:00Z",
                "message_type": "user_message",
                "content": "Hello",
                "step_id": "s1"
            }
        ]);
        std::fs::write(
            source_dir.path().join("messages.json"),
            serde_json::to_string(&messages_json).unwrap(),
        )
        .unwrap();

        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        // Run 1 and 2: same source.
        run_migration(source_dir.path(), &db_path, &core_md).unwrap();
        run_migration(source_dir.path(), &db_path, &core_md).unwrap();
        assert_eq!(count_rows(&db_path, "notes"), 1);
        assert_eq!(count_rows(&db_path, "messages"), 1);

        // Add ONE new passage and ONE new message to the source.
        let archival_json = serde_json::json!([
            { "id": "passage-1", "text": "First passage.", "created_at": "2025-01-01T00:00:00Z" },
            { "id": "passage-2", "text": "Brand new passage.", "created_at": "2025-06-01T00:00:00Z" }
        ]);
        std::fs::write(
            source_dir.path().join("archival_memory.json"),
            serde_json::to_string(&archival_json).unwrap(),
        )
        .unwrap();
        let messages_json = serde_json::json!([
            {
                "id": "message-1",
                "date": "2025-01-01T00:00:00Z",
                "message_type": "user_message",
                "content": "Hello",
                "step_id": "s1"
            },
            {
                "id": "message-2",
                "date": "2025-06-01T00:00:00Z",
                "message_type": "user_message",
                "content": "New message",
                "step_id": "s2"
            }
        ]);
        std::fs::write(
            source_dir.path().join("messages.json"),
            serde_json::to_string(&messages_json).unwrap(),
        )
        .unwrap();

        // Run 3: only the new rows should import.
        let s3 = run_migration(source_dir.path(), &db_path, &core_md).unwrap();
        assert_eq!(s3.archival_notes_imported, 1);
        assert_eq!(s3.messages_imported, 1);
        assert_eq!(s3.archival_notes_skipped, 1);
        assert_eq!(s3.messages_skipped_existing, 1);

        assert_eq!(count_rows(&db_path, "notes"), 2);
        assert_eq!(count_rows(&db_path, "messages"), 2);
    }

    #[test]
    fn migrate_core_blocks_are_idempotent_on_rerun() {
        let (source_dir, data_dir) = setup_test_env();
        let core_json = serde_json::json!({
            "blocks": [
                { "id": "block-persona", "value": "I am Aurora.", "label": "persona" },
                { "id": "block-human", "value": "The human is Tyto.", "label": "human" }
            ]
        });
        std::fs::write(
            source_dir.path().join("core_memory.json"),
            serde_json::to_string(&core_json).unwrap(),
        )
        .unwrap();

        let db_path = data_dir.path().join("test.db");
        let core_md = data_dir.path().join("core.md");

        let s1 = run_migration(source_dir.path(), &db_path, &core_md).unwrap();
        // persona overwrites core.md; human becomes a note.
        assert_eq!(s1.core_blocks_imported, 2);
        assert!(s1.persona_updated);
        let notes_after_1 = count_rows(&db_path, "notes");
        assert_eq!(notes_after_1, 1, "only the human block becomes a note row");

        // Re-run: the human note must not duplicate; persona is still overwritten.
        let s2 = run_migration(source_dir.path(), &db_path, &core_md).unwrap();
        assert!(s2.persona_updated, "persona overwrite is idempotent by nature");
        assert_eq!(
            s2.core_blocks_skipped, 1,
            "the human core block should be skipped as already imported"
        );
        assert_eq!(
            count_rows(&db_path, "notes"),
            notes_after_1,
            "re-running must not duplicate the human core note"
        );
    }
}
