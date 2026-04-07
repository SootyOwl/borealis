use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Inventory self-registration
// ---------------------------------------------------------------------------

inventory::submit! {
    crate::memory::MemoryRegistration {
        name: "sqlite",
        build_fn: |deps| {
            let memory = SqliteMemory::new(
                Arc::clone(&deps.db_conn),
                deps.settings.bot.core_persona_path.clone(),
            )?;
            Ok(Arc::new(memory))
        },
    }
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("note not found: {0}")]
    NotFound(String),
    #[error("cannot use reserved ID 'core' for note operations")]
    ReservedId,
    #[error("invalid tag: {0}")]
    InvalidTag(String),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("core.md I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("lock poisoned")]
    LockPoisoned,
}

pub type MemoryResult<T> = Result<T, MemoryError>;

/// Maximum depth (number of slash-separated segments) for a nested tag.
const MAX_TAG_DEPTH: usize = 8;

/// Normalize a tag string into Borealis's canonical nested-tag form.
///
/// Rules (see `docs/superpowers/specs/2026-04-07-fts5-and-nested-tags.md`):
/// - Slash-delimited path segments: `parent/child/leaf`.
/// - Lowercased silently — `Recipes/Italian` becomes `recipes/italian`.
/// - Leading and trailing slashes stripped.
/// - Empty segments (`a//b`) are rejected.
/// - Allowed segment chars: `[a-z0-9._-]`.
/// - Maximum depth: [`MAX_TAG_DEPTH`] segments.
///
/// Returns the normalized tag, or [`MemoryError::InvalidTag`] with a human-readable reason.
pub(crate) fn normalize_tag(input: &str) -> MemoryResult<String> {
    let lowered = input.to_lowercase();
    let trimmed = lowered.trim_matches('/');
    if trimmed.is_empty() || trimmed.chars().all(char::is_whitespace) {
        return Err(MemoryError::InvalidTag(format!(
            "empty or whitespace-only tag: {input:?}"
        )));
    }

    let segments: Vec<&str> = trimmed.split('/').collect();
    if segments.len() > MAX_TAG_DEPTH {
        return Err(MemoryError::InvalidTag(format!(
            "tag exceeds max depth of {MAX_TAG_DEPTH} segments: {input:?}"
        )));
    }

    for seg in &segments {
        if seg.is_empty() {
            return Err(MemoryError::InvalidTag(format!(
                "empty segment in tag: {input:?}"
            )));
        }
        if !seg
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return Err(MemoryError::InvalidTag(format!(
                "invalid character in tag segment {seg:?} (allowed: a-z 0-9 . _ -)"
            )));
        }
    }

    Ok(segments.join("/"))
}

/// Normalize and deduplicate a slice of tags, returning the first error encountered.
///
/// Deduplication preserves first-occurrence order, so the returned `Vec` is a
/// faithful description of what will actually land in the `tags` table — callers
/// who inspect `Note.tags` after `create_note`/`tag_note` see exactly what was stored.
fn normalize_tags(tags: &[String]) -> MemoryResult<Vec<String>> {
    let mut out = Vec::with_capacity(tags.len());
    for raw in tags {
        let norm = normalize_tag(raw)?;
        if !out.contains(&norm) {
            out.push(norm);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Link {
    pub from_id: String,
    pub to_id: String,
    pub relation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
}

/// Object-safe trait for memory storage backends.
///
/// All methods are synchronous — callers are responsible for wrapping in
/// `tokio::task::spawn_blocking` when used in async contexts. Requires
/// `Send + Sync` so implementations can be shared via `Arc<dyn Memory>`.
pub trait Memory: Send + Sync {
    fn create_note(&self, title: &str, content: &str, tags: &[String]) -> MemoryResult<Note>;
    fn read_note(&self, id: &str) -> MemoryResult<Note>;
    fn update_note(&self, id: &str, content: &str) -> MemoryResult<Note>;
    fn forget_note(&self, id: &str) -> MemoryResult<()>;
    fn search_notes(&self, query: &str, limit: usize) -> MemoryResult<Vec<Note>>;
    fn list_notes(&self, tag_filter: Option<&str>) -> MemoryResult<Vec<Note>>;
    fn link_notes(&self, from_id: &str, to_id: &str, relation: &str) -> MemoryResult<Link>;
    fn get_links_for_note(&self, id: &str) -> MemoryResult<Vec<Link>>;
    fn tag_note(&self, id: &str, tags: &[String]) -> MemoryResult<Note>;
    fn load_core_persona(&self) -> MemoryResult<String>;
}

/// SQLite-backed memory store for notes, tags, and links.
///
/// Receives an `Arc<Mutex<Connection>>` — it does not create or manage
/// the connection. All operations are synchronous (caller is responsible
/// for wrapping in `spawn_blocking`).
#[derive(Clone)]
pub struct SqliteMemory {
    conn: Arc<Mutex<Connection>>,
    core_md_path: std::path::PathBuf,
}

impl SqliteMemory {
    pub fn new(
        conn: Arc<Mutex<Connection>>,
        core_md_path: std::path::PathBuf,
    ) -> MemoryResult<Self> {
        let store = Self { conn, core_md_path };
        store.init_schema()?;
        Ok(store)
    }

    /// Acquire the database connection, recovering from mutex poisoning.
    fn lock_conn(&self) -> MemoryResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| MemoryError::LockPoisoned)
    }

    fn init_schema(&self) -> MemoryResult<()> {
        let conn = self.lock_conn()?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;

             CREATE TABLE IF NOT EXISTS notes (
                 id          TEXT PRIMARY KEY,
                 title       TEXT NOT NULL,
                 content     TEXT NOT NULL,
                 created_at  TEXT NOT NULL,
                 updated_at  TEXT NOT NULL,
                 deleted_at  TEXT
             );

             CREATE TABLE IF NOT EXISTS tags (
                 note_id     TEXT NOT NULL REFERENCES notes(id),
                 tag         TEXT NOT NULL,
                 PRIMARY KEY (note_id, tag)
             );

             CREATE TABLE IF NOT EXISTS links (
                 from_id     TEXT NOT NULL REFERENCES notes(id),
                 to_id       TEXT NOT NULL REFERENCES notes(id),
                 relation    TEXT NOT NULL,
                 PRIMARY KEY (from_id, to_id)
             );

             CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(tag);",
        )?;
        Ok(())
    }

    /// Generate a note ID like `note_a1b2c3d4` using a random u32.
    /// Checks for collisions and regenerates if needed.
    fn generate_id(&self) -> MemoryResult<String> {
        let conn = self.lock_conn()?;
        loop {
            let id = format!("note_{:08x}", rand::random::<u32>());
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM notes WHERE id = ?1)",
                params![id],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(id);
            }
        }
    }

    fn now_iso() -> String {
        // Simple UTC timestamp without chrono dependency
        let duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch");
        let secs = duration.as_secs();
        // Convert to rough ISO 8601 — good enough for ordering and display.
        // For production, swap to chrono or time crate.
        let days_since_epoch = secs / 86400;
        let time_of_day = secs % 86400;
        let hours = time_of_day / 3600;
        let minutes = (time_of_day % 3600) / 60;
        let seconds = time_of_day % 60;

        // Calculate year/month/day from days since epoch (1970-01-01)
        let (year, month, day) = days_to_ymd(days_since_epoch);

        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            year, month, day, hours, minutes, seconds
        )
    }

    fn read_core(&self) -> MemoryResult<Note> {
        let content = std::fs::read_to_string(&self.core_md_path)?;
        let now = Self::now_iso();
        Ok(Note {
            id: "core".to_string(),
            title: "Core Persona".to_string(),
            content,
            tags: vec!["persona".to_string()],
            links: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    fn update_core(&self, content: &str) -> MemoryResult<Note> {
        std::fs::write(&self.core_md_path, content)?;
        self.read_core()
    }

    fn get_tags_for_note_locked(
        &self,
        conn: &Connection,
        note_id: &str,
    ) -> MemoryResult<Vec<String>> {
        let mut stmt = conn.prepare("SELECT tag FROM tags WHERE note_id = ?1 ORDER BY tag")?;
        let tags = stmt
            .query_map(params![note_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(tags)
    }

    fn get_links_for_note_locked(
        &self,
        conn: &Connection,
        note_id: &str,
    ) -> MemoryResult<Vec<Link>> {
        let mut stmt = conn.prepare(
            "SELECT from_id, to_id, relation, 'outgoing' AS direction FROM links WHERE from_id = ?1
             UNION ALL
             SELECT from_id, to_id, relation, 'incoming' AS direction FROM links WHERE to_id = ?1",
        )?;
        let links = stmt
            .query_map(params![note_id], |row| {
                Ok(Link {
                    from_id: row.get(0)?,
                    to_id: row.get(1)?,
                    relation: row.get(2)?,
                    direction: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(links)
    }

    fn assert_note_exists_locked(&self, conn: &Connection, id: &str) -> MemoryResult<()> {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM notes WHERE id = ?1 AND deleted_at IS NULL)",
            params![id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(MemoryError::NotFound(id.to_string()));
        }
        Ok(())
    }
}

impl Memory for SqliteMemory {
    fn create_note(&self, title: &str, content: &str, tags: &[String]) -> MemoryResult<Note> {
        let normalized = normalize_tags(tags)?;
        let id = self.generate_id()?;
        let now = Self::now_iso();
        let conn = self.lock_conn()?;

        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO notes (id, title, content, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, title, content, now, now],
        )?;

        for tag in &normalized {
            tx.execute(
                "INSERT OR IGNORE INTO tags (note_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )?;
        }
        tx.commit()?;

        Ok(Note {
            id,
            title: title.to_string(),
            content: content.to_string(),
            tags: normalized,
            links: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    fn read_note(&self, id: &str) -> MemoryResult<Note> {
        if id == "core" {
            return self.read_core();
        }

        let conn = self.lock_conn()?;
        let note = conn
            .query_row(
                "SELECT id, title, content, created_at, updated_at FROM notes WHERE id = ?1 AND deleted_at IS NULL",
                params![id],
                |row| {
                    Ok(Note {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        content: row.get(2)?,
                        tags: Vec::new(),
                        links: Vec::new(),
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => MemoryError::NotFound(id.to_string()),
                other => MemoryError::Database(other),
            })?;

        let tags = self.get_tags_for_note_locked(&conn, &note.id)?;
        let links = self.get_links_for_note_locked(&conn, &note.id)?;
        Ok(Note {
            tags,
            links,
            ..note
        })
    }

    fn update_note(&self, id: &str, content: &str) -> MemoryResult<Note> {
        if id == "core" {
            return self.update_core(content);
        }

        let now = Self::now_iso();
        let conn = self.lock_conn()?;

        let rows = conn.execute(
            "UPDATE notes SET content = ?1, updated_at = ?2 WHERE id = ?3 AND deleted_at IS NULL",
            params![content, now, id],
        )?;

        if rows == 0 {
            return Err(MemoryError::NotFound(id.to_string()));
        }

        drop(conn);
        self.read_note(id)
    }

    fn forget_note(&self, id: &str) -> MemoryResult<()> {
        if id == "core" {
            return Err(MemoryError::ReservedId);
        }

        let now = Self::now_iso();
        let conn = self.lock_conn()?;

        let rows = conn.execute(
            "UPDATE notes SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![now, id],
        )?;

        if rows == 0 {
            return Err(MemoryError::NotFound(id.to_string()));
        }

        Ok(())
    }

    fn search_notes(&self, query: &str, limit: usize) -> MemoryResult<Vec<Note>> {
        let conn = self.lock_conn()?;
        let escaped = query.replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("%{escaped}%");

        let mut stmt = conn.prepare(
            "SELECT DISTINCT n.id, n.title, n.content, n.created_at, n.updated_at
             FROM notes n
             LEFT JOIN tags t ON n.id = t.note_id
             WHERE n.deleted_at IS NULL
               AND (n.title LIKE ?1 ESCAPE '\\' OR n.content LIKE ?1 ESCAPE '\\' OR t.tag LIKE ?1 ESCAPE '\\')
             ORDER BY n.updated_at DESC
             LIMIT ?2",
        )?;

        let notes: Vec<Note> = stmt
            .query_map(params![pattern, limit as i64], |row| {
                Ok(Note {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    content: row.get(2)?,
                    tags: Vec::new(),
                    links: Vec::new(),
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Populate tags and links for each note
        let mut result = Vec::with_capacity(notes.len());
        for note in notes {
            let tags = self.get_tags_for_note_locked(&conn, &note.id)?;
            let links = self.get_links_for_note_locked(&conn, &note.id)?;
            result.push(Note { tags, links, ..note });
        }

        Ok(result)
    }

    fn list_notes(&self, tag_filter: Option<&str>) -> MemoryResult<Vec<Note>> {
        let conn = self.lock_conn()?;

        let notes = if let Some(tag) = tag_filter {
            let normalized = normalize_tag(tag)?;
            // Prefix match: `recipes` matches `recipes`, `recipes/italian`, etc.
            // The `|| '/*'` ensures `recipe` does NOT match `recipes`.
            //
            // GLOB (not LIKE) is deliberate: GLOB metacharacters are `*`, `?`, `[`,
            // none of which can appear in a tag normalized through `normalize_tag`
            // (allowlist is `[a-z0-9._-]` per segment + `/` separator). LIKE would
            // require escaping `_`, which IS in the allowlist.
            //
            // DISTINCT collapses multi-tag joins so a note with both `recipes` and
            // `recipes/italian` doesn't appear twice when filtered by `recipes`.
            let mut stmt = conn.prepare(
                "SELECT DISTINCT n.id, n.title, n.content, n.created_at, n.updated_at
                 FROM notes n
                 JOIN tags t ON n.id = t.note_id
                 WHERE n.deleted_at IS NULL
                   AND (t.tag = ?1 OR t.tag GLOB ?1 || '/*')
                 ORDER BY n.updated_at DESC",
            )?;
            stmt.query_map(params![normalized], |row| {
                Ok(Note {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    content: row.get(2)?,
                    tags: Vec::new(),
                    links: Vec::new(),
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, title, content, created_at, updated_at
                 FROM notes
                 WHERE deleted_at IS NULL
                 ORDER BY updated_at DESC",
            )?;
            stmt.query_map([], |row| {
                Ok(Note {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    content: row.get(2)?,
                    tags: Vec::new(),
                    links: Vec::new(),
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        let mut result = Vec::with_capacity(notes.len());
        for note in notes {
            let tags = self.get_tags_for_note_locked(&conn, &note.id)?;
            let links = self.get_links_for_note_locked(&conn, &note.id)?;
            result.push(Note { tags, links, ..note });
        }

        Ok(result)
    }

    fn link_notes(&self, from_id: &str, to_id: &str, relation: &str) -> MemoryResult<Link> {
        if from_id == "core" || to_id == "core" {
            return Err(MemoryError::ReservedId);
        }

        let conn = self.lock_conn()?;

        // Verify both notes exist and are not deleted
        self.assert_note_exists_locked(&conn, from_id)?;
        self.assert_note_exists_locked(&conn, to_id)?;

        // Insert forward link only (directional)
        conn.execute(
            "INSERT OR REPLACE INTO links (from_id, to_id, relation) VALUES (?1, ?2, ?3)",
            params![from_id, to_id, relation],
        )?;

        Ok(Link {
            from_id: from_id.to_string(),
            to_id: to_id.to_string(),
            relation: relation.to_string(),
            direction: None,
        })
    }

    fn get_links_for_note(&self, id: &str) -> MemoryResult<Vec<Link>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT from_id, to_id, relation, 'outgoing' AS direction FROM links WHERE from_id = ?1
             UNION ALL
             SELECT from_id, to_id, relation, 'incoming' AS direction FROM links WHERE to_id = ?1",
        )?;
        let links = stmt
            .query_map(params![id], |row| {
                Ok(Link {
                    from_id: row.get(0)?,
                    to_id: row.get(1)?,
                    relation: row.get(2)?,
                    direction: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(links)
    }

    fn tag_note(&self, id: &str, tags: &[String]) -> MemoryResult<Note> {
        if id == "core" {
            return Err(MemoryError::ReservedId);
        }

        let normalized = normalize_tags(tags)?;
        let conn = self.lock_conn()?;
        self.assert_note_exists_locked(&conn, id)?;

        // Replace all tags atomically
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM tags WHERE note_id = ?1", params![id])?;
        for tag in &normalized {
            tx.execute(
                "INSERT OR IGNORE INTO tags (note_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )?;
        }

        let now = Self::now_iso();
        tx.execute(
            "UPDATE notes SET updated_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        tx.commit()?;

        drop(conn);
        self.read_note(id)
    }

    fn load_core_persona(&self) -> MemoryResult<String> {
        std::fs::read_to_string(&self.core_md_path).map_err(MemoryError::Io)
    }
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's civil_from_days
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::NamedTempFile, SqliteMemory) {
        let conn = Connection::open_in_memory().unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let tmp_path = tmp.path().to_path_buf();
        std::fs::write(&tmp_path, "# Test Core\nI am a test persona.").unwrap();
        let store = SqliteMemory::new(conn, tmp_path).unwrap();
        (tmp, store)
    }

    #[test]
    fn create_and_read_note() {
        let (_tmp, store) = test_store();
        let note = store
            .create_note(
                "Test Title",
                "Test content",
                &["tag1".into(), "tag2".into()],
            )
            .unwrap();

        assert!(note.id.starts_with("note_"));
        assert_eq!(note.title, "Test Title");
        assert_eq!(note.content, "Test content");
        assert_eq!(note.tags, vec!["tag1", "tag2"]);

        let read = store.read_note(&note.id).unwrap();
        assert_eq!(read.id, note.id);
        assert_eq!(read.title, "Test Title");
        assert_eq!(read.content, "Test content");
        assert_eq!(read.tags, vec!["tag1", "tag2"]);
    }

    #[test]
    fn update_note() {
        let (_tmp, store) = test_store();
        let note = store.create_note("Title", "Old content", &[]).unwrap();
        let updated = store.update_note(&note.id, "New content").unwrap();
        assert_eq!(updated.content, "New content");
        assert_eq!(updated.title, "Title");
    }

    #[test]
    fn forget_note_excludes_from_search() {
        let (_tmp, store) = test_store();
        let note = store
            .create_note("Forgettable", "Some content", &["temp".into()])
            .unwrap();

        store.forget_note(&note.id).unwrap();

        let results = store.search_notes("Forgettable", 10).unwrap();
        assert!(results.is_empty());

        let err = store.read_note(&note.id).unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
    }

    #[test]
    fn search_by_title_content_tag() {
        let (_tmp, store) = test_store();
        store
            .create_note("Rust Programming", "Systems language", &["code".into()])
            .unwrap();
        store
            .create_note("Python Guide", "Scripting language", &["code".into()])
            .unwrap();
        store
            .create_note("Cooking Recipe", "Pasta carbonara", &["food".into()])
            .unwrap();

        // Search by title
        let results = store.search_notes("Rust", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust Programming");

        // Search by content
        let results = store.search_notes("carbonara", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Cooking Recipe");

        // Search by tag
        let results = store.search_notes("code", 10).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn link_notes_directional() {
        let (_tmp, store) = test_store();
        let a = store.create_note("Note A", "Content A", &[]).unwrap();
        let b = store.create_note("Note B", "Content B", &[]).unwrap();

        store.link_notes(&a.id, &b.id, "related_to").unwrap();

        // A sees an outgoing link to B
        let links_a = store.get_links_for_note(&a.id).unwrap();
        assert_eq!(links_a.len(), 1);
        assert_eq!(links_a[0].from_id, a.id);
        assert_eq!(links_a[0].to_id, b.id);
        assert_eq!(links_a[0].relation, "related_to");
        assert_eq!(links_a[0].direction, Some("outgoing".to_string()));

        // B sees an incoming link from A
        let links_b = store.get_links_for_note(&b.id).unwrap();
        assert_eq!(links_b.len(), 1);
        assert_eq!(links_b[0].from_id, a.id);
        assert_eq!(links_b[0].to_id, b.id);
        assert_eq!(links_b[0].direction, Some("incoming".to_string()));
    }

    #[test]
    fn tag_note_replaces_tags() {
        let (_tmp, store) = test_store();
        let note = store
            .create_note("Tagged", "Content", &["old".into()])
            .unwrap();

        let updated = store
            .tag_note(&note.id, &["new1".into(), "new2".into()])
            .unwrap();
        assert_eq!(updated.tags, vec!["new1", "new2"]);

        // Old tag is gone
        let results = store.list_notes(Some("old")).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn normalize_tag_lowercases_silently() {
        assert_eq!(normalize_tag("Recipes").unwrap(), "recipes");
        assert_eq!(
            normalize_tag("Recipes/Italian/CARBONARA").unwrap(),
            "recipes/italian/carbonara"
        );
    }

    #[test]
    fn normalize_tag_strips_outer_slashes() {
        assert_eq!(normalize_tag("/recipes/").unwrap(), "recipes");
        assert_eq!(
            normalize_tag("///recipes/italian///").unwrap(),
            "recipes/italian"
        );
    }

    #[test]
    fn normalize_tag_rejects_empty_segments() {
        assert!(matches!(
            normalize_tag("a//b"),
            Err(MemoryError::InvalidTag(_))
        ));
    }

    #[test]
    fn normalize_tag_rejects_empty_input() {
        assert!(matches!(normalize_tag(""), Err(MemoryError::InvalidTag(_))));
        assert!(matches!(
            normalize_tag("///"),
            Err(MemoryError::InvalidTag(_))
        ));
    }

    #[test]
    fn normalize_tag_rejects_bad_characters() {
        for bad in ["has space", "has!bang", "has:colon", "has?q", "ümlaut"] {
            assert!(
                matches!(normalize_tag(bad), Err(MemoryError::InvalidTag(_))),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn normalize_tag_allows_dots_underscores_dashes_digits() {
        assert_eq!(normalize_tag("v1.2.3").unwrap(), "v1.2.3");
        assert_eq!(normalize_tag("snake_case").unwrap(), "snake_case");
        assert_eq!(normalize_tag("kebab-case").unwrap(), "kebab-case");
        assert_eq!(normalize_tag("2026-04-07").unwrap(), "2026-04-07");
    }

    #[test]
    fn normalize_tag_enforces_max_depth() {
        // 8 segments is OK.
        let ok = (1..=8).map(|i| i.to_string()).collect::<Vec<_>>().join("/");
        assert!(normalize_tag(&ok).is_ok());
        // 9 segments is not.
        let bad = (1..=9).map(|i| i.to_string()).collect::<Vec<_>>().join("/");
        assert!(matches!(
            normalize_tag(&bad),
            Err(MemoryError::InvalidTag(_))
        ));
    }

    #[test]
    fn create_note_normalizes_tags_silently() {
        let (_tmp, store) = test_store();
        let note = store
            .create_note("T", "C", &["Recipes/Italian".into(), "PERSON".into()])
            .unwrap();
        assert_eq!(note.tags, vec!["recipes/italian", "person"]);
    }

    #[test]
    fn create_note_rejects_invalid_tag() {
        let (_tmp, store) = test_store();
        let err = store
            .create_note("T", "C", &["bad tag".into()])
            .unwrap_err();
        assert!(matches!(err, MemoryError::InvalidTag(_)));
    }

    #[test]
    fn list_notes_prefix_match() {
        let (_tmp, store) = test_store();
        store
            .create_note("Carbonara", "X", &["recipes/italian/carbonara".into()])
            .unwrap();
        store
            .create_note("Ratatouille", "X", &["recipes/french/ratatouille".into()])
            .unwrap();
        store
            .create_note("Recipes index", "X", &["recipes".into()])
            .unwrap();
        store
            .create_note("Unrelated", "X", &["recipe".into()])
            .unwrap();

        // `recipes` matches itself, italian/*, french/*. NOT `recipe` (different word).
        let hits = store.list_notes(Some("recipes")).unwrap();
        let titles: std::collections::HashSet<&str> =
            hits.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(hits.len(), 3);
        assert!(titles.contains("Carbonara"));
        assert!(titles.contains("Ratatouille"));
        assert!(titles.contains("Recipes index"));
        assert!(!titles.contains("Unrelated"));

        // Drilling down narrows the result.
        let italian = store.list_notes(Some("recipes/italian")).unwrap();
        assert_eq!(italian.len(), 1);
        assert_eq!(italian[0].title, "Carbonara");

        // Tag filter is normalized too — uppercase input still works.
        let hits = store.list_notes(Some("Recipes")).unwrap();
        let titles: std::collections::HashSet<&str> =
            hits.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(hits.len(), 3);
        assert!(titles.contains("Carbonara"));
        assert!(!titles.contains("Unrelated"));
    }

    #[test]
    fn list_notes_prefix_distinct_collapses_multi_tag_match() {
        // A note tagged with BOTH a parent and child of the filter must
        // appear exactly once, not twice. The `SELECT DISTINCT` in the
        // production query is the only thing keeping this honest.
        let (_tmp, store) = test_store();
        store
            .create_note(
                "Recipe with redundant tags",
                "X",
                &["recipes".into(), "recipes/italian".into()],
            )
            .unwrap();
        store
            .create_note("Other italian", "X", &["recipes/italian".into()])
            .unwrap();

        let hits = store.list_notes(Some("recipes")).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "expected DISTINCT to dedupe the multi-tag note"
        );
    }

    #[test]
    fn list_notes_prefix_does_not_treat_underscore_as_wildcard() {
        // Regression: underscores are allowed in tag segments AND are LIKE
        // wildcards. The query must use GLOB (or escaped LIKE) so that
        // `foo_bar` matches only `foo_bar/...`, not `fooXbar/...`.
        let (_tmp, store) = test_store();
        store
            .create_note("Real", "X", &["foo_bar/child".into()])
            .unwrap();
        store
            .create_note("Wildcard impostor", "X", &["fooxbar/child".into()])
            .unwrap();

        let hits = store.list_notes(Some("foo_bar")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Real");
    }

    #[test]
    fn list_notes_propagates_invalid_tag_filter() {
        let (_tmp, store) = test_store();
        let err = store.list_notes(Some("bad tag")).unwrap_err();
        assert!(matches!(err, MemoryError::InvalidTag(_)));
    }

    #[test]
    fn create_note_deduplicates_normalized_tags() {
        let (_tmp, store) = test_store();
        // Both inputs normalize to "recipes/italian" — the returned Note
        // must reflect what's actually in the DB (one row), not what was passed in.
        let note = store
            .create_note(
                "T",
                "C",
                &["Recipes/Italian".into(), "recipes/italian".into()],
            )
            .unwrap();
        assert_eq!(note.tags, vec!["recipes/italian"]);

        let read = store.read_note(&note.id).unwrap();
        assert_eq!(read.tags, note.tags);
    }

    #[test]
    fn tag_note_rejects_invalid_tag_and_preserves_existing_tags() {
        let (_tmp, store) = test_store();
        let note = store
            .create_note("T", "C", &["original".into()])
            .unwrap();

        let err = store
            .tag_note(&note.id, &["valid".into(), "bad tag".into()])
            .unwrap_err();
        assert!(matches!(err, MemoryError::InvalidTag(_)));

        // Validation must run BEFORE the DELETE, so the original tags survive.
        let read = store.read_note(&note.id).unwrap();
        assert_eq!(read.tags, vec!["original"]);
    }

    #[test]
    fn tag_note_rejects_core() {
        let (_tmp, store) = test_store();
        let err = store.tag_note("core", &["x".into()]).unwrap_err();
        assert!(matches!(err, MemoryError::ReservedId));
    }

    #[test]
    fn list_notes_prefix_does_not_match_unrelated_word() {
        let (_tmp, store) = test_store();
        store
            .create_note("Singular", "X", &["recipe".into()])
            .unwrap();
        store
            .create_note("Plural", "X", &["recipes".into()])
            .unwrap();
        // `recipe` should NOT match `recipes`.
        let hits = store.list_notes(Some("recipe")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Singular");
    }

    #[test]
    fn list_with_tag_filter() {
        let (_tmp, store) = test_store();
        store
            .create_note("A", "Content", &["alpha".into()])
            .unwrap();
        store.create_note("B", "Content", &["beta".into()]).unwrap();
        store
            .create_note("C", "Content", &["alpha".into(), "beta".into()])
            .unwrap();

        let alpha = store.list_notes(Some("alpha")).unwrap();
        assert_eq!(alpha.len(), 2);

        let all = store.list_notes(None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn core_persona_read_and_update() {
        let (_tmp, store) = test_store();

        let core = store.read_note("core").unwrap();
        assert_eq!(core.id, "core");
        assert!(core.content.contains("Test Core"));

        let updated = store
            .update_note("core", "# Updated Core\nNew persona.")
            .unwrap();
        assert!(updated.content.contains("Updated Core"));

        // Cannot forget core
        let err = store.forget_note("core").unwrap_err();
        assert!(matches!(err, MemoryError::ReservedId));
    }

    #[test]
    fn read_note_includes_links() {
        let (_tmp, store) = test_store();
        let a = store.create_note("Note A", "Content A", &[]).unwrap();
        let b = store.create_note("Note B", "Content B", &[]).unwrap();

        store.link_notes(&a.id, &b.id, "related_to").unwrap();

        // A has an outgoing link to B
        let read = store.read_note(&a.id).unwrap();
        assert_eq!(read.links.len(), 1);
        assert_eq!(read.links[0].to_id, b.id);
        assert_eq!(read.links[0].relation, "related_to");
        assert_eq!(read.links[0].direction, Some("outgoing".to_string()));

        // B has an incoming link from A
        let read_b = store.read_note(&b.id).unwrap();
        assert_eq!(read_b.links.len(), 1);
        assert_eq!(read_b.links[0].from_id, a.id);
        assert_eq!(read_b.links[0].direction, Some("incoming".to_string()));

        // Verify notes without links have empty vec
        let c = store.create_note("Note C", "Content C", &[]).unwrap();
        let read_c = store.read_note(&c.id).unwrap();
        assert!(read_c.links.is_empty());
    }

    #[test]
    fn not_found_errors() {
        let (_tmp, store) = test_store();

        let err = store.read_note("note_nonexist").unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));

        let err = store.update_note("note_nonexist", "content").unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));

        let err = store.forget_note("note_nonexist").unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
    }

    /// A mock memory implementation to verify the Memory trait is object-safe
    /// and can be used as `Arc<dyn Memory>`.
    struct MockMemory {
        notes: std::sync::Mutex<Vec<Note>>,
        links: std::sync::Mutex<Vec<Link>>,
    }

    impl MockMemory {
        fn new() -> Self {
            Self {
                notes: std::sync::Mutex::new(Vec::new()),
                links: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Memory for MockMemory {
        fn create_note(&self, title: &str, content: &str, tags: &[String]) -> MemoryResult<Note> {
            let normalized = normalize_tags(tags)?;
            let note = Note {
                id: format!("mock_{}", self.notes.lock().unwrap().len()),
                title: title.to_string(),
                content: content.to_string(),
                tags: normalized,
                links: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
            };
            self.notes.lock().unwrap().push(note.clone());
            Ok(note)
        }

        fn read_note(&self, id: &str) -> MemoryResult<Note> {
            self.notes
                .lock()
                .unwrap()
                .iter()
                .find(|n| n.id == id)
                .cloned()
                .ok_or_else(|| MemoryError::NotFound(id.to_string()))
        }

        fn update_note(&self, id: &str, content: &str) -> MemoryResult<Note> {
            let mut notes = self.notes.lock().unwrap();
            let note = notes
                .iter_mut()
                .find(|n| n.id == id)
                .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
            note.content = content.to_string();
            Ok(note.clone())
        }

        fn forget_note(&self, id: &str) -> MemoryResult<()> {
            if id == "core" {
                return Err(MemoryError::ReservedId);
            }
            let mut notes = self.notes.lock().unwrap();
            let pos = notes
                .iter()
                .position(|n| n.id == id)
                .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
            notes.remove(pos);
            Ok(())
        }

        fn search_notes(&self, query: &str, limit: usize) -> MemoryResult<Vec<Note>> {
            let notes = self.notes.lock().unwrap();
            Ok(notes
                .iter()
                .filter(|n| n.title.contains(query) || n.content.contains(query))
                .take(limit)
                .cloned()
                .collect())
        }

        fn list_notes(&self, tag_filter: Option<&str>) -> MemoryResult<Vec<Note>> {
            let notes = self.notes.lock().unwrap();
            Ok(match tag_filter {
                Some(tag) => {
                    let needle = normalize_tag(tag)?;
                    let prefix = format!("{needle}/");
                    notes
                        .iter()
                        .filter(|n| {
                            n.tags
                                .iter()
                                .any(|t| t == &needle || t.starts_with(&prefix))
                        })
                        .cloned()
                        .collect()
                }
                None => notes.clone(),
            })
        }

        fn link_notes(&self, from_id: &str, to_id: &str, relation: &str) -> MemoryResult<Link> {
            if from_id == "core" || to_id == "core" {
                return Err(MemoryError::ReservedId);
            }
            let notes = self.notes.lock().unwrap();
            if notes.iter().find(|n| n.id == from_id).is_none() {
                return Err(MemoryError::NotFound(from_id.to_string()));
            }
            if notes.iter().find(|n| n.id == to_id).is_none() {
                return Err(MemoryError::NotFound(to_id.to_string()));
            }
            drop(notes);
            let link = Link {
                from_id: from_id.to_string(),
                to_id: to_id.to_string(),
                relation: relation.to_string(),
                direction: None,
            };
            self.links.lock().unwrap().push(link.clone());
            Ok(link)
        }

        fn get_links_for_note(&self, id: &str) -> MemoryResult<Vec<Link>> {
            let links = self.links.lock().unwrap();
            Ok(links
                .iter()
                .filter_map(|l| {
                    if l.from_id == id {
                        Some(Link {
                            direction: Some("outgoing".to_string()),
                            ..l.clone()
                        })
                    } else if l.to_id == id {
                        Some(Link {
                            direction: Some("incoming".to_string()),
                            ..l.clone()
                        })
                    } else {
                        None
                    }
                })
                .collect())
        }

        fn tag_note(&self, id: &str, tags: &[String]) -> MemoryResult<Note> {
            if id == "core" {
                return Err(MemoryError::ReservedId);
            }
            let normalized = normalize_tags(tags)?;
            let mut notes = self.notes.lock().unwrap();
            let note = notes
                .iter_mut()
                .find(|n| n.id == id)
                .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
            note.tags = normalized;
            Ok(note.clone())
        }

        fn load_core_persona(&self) -> MemoryResult<String> {
            Ok("Mock persona".to_string())
        }
    }

    #[test]
    fn mock_memory_is_object_safe_and_usable_as_arc_dyn() {
        // Verify the Memory trait is object-safe by constructing Arc<dyn Memory>.
        let memory: Arc<dyn Memory> = Arc::new(MockMemory::new());

        // Exercise the trait through the dyn reference.
        let note = memory
            .create_note("Test", "Content", &["tag1".into()])
            .unwrap();
        assert_eq!(note.title, "Test");
        assert_eq!(note.id, "mock_0");

        let read = memory.read_note(&note.id).unwrap();
        assert_eq!(read.content, "Content");

        let updated = memory.update_note(&note.id, "New content").unwrap();
        assert_eq!(updated.content, "New content");

        let found = memory.search_notes("New", 10).unwrap();
        assert_eq!(found.len(), 1);

        let listed = memory.list_notes(Some("tag1")).unwrap();
        assert_eq!(listed.len(), 1);

        let tagged = memory
            .tag_note(&note.id, &["new_tag".into()])
            .unwrap();
        assert_eq!(tagged.tags, vec!["new_tag"]);

        let persona = memory.load_core_persona().unwrap();
        assert_eq!(persona, "Mock persona");

        memory.forget_note(&note.id).unwrap();
        assert!(memory.read_note(&note.id).is_err());
    }
}
