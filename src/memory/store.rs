use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("note not found: {0}")]
    NotFound(String),
    #[error("cannot use reserved ID 'core' for note operations")]
    ReservedId,
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("core.md I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type MemoryResult<T> = Result<T, MemoryError>;

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

    fn init_schema(&self) -> MemoryResult<()> {
        let conn = self.conn.lock().expect("mutex poisoned");
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;

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
             );",
        )?;
        Ok(())
    }

    /// Generate a note ID like `note_a1b2c3d4` using a random u32.
    /// Checks for collisions and regenerates if needed.
    fn generate_id(&self) -> MemoryResult<String> {
        let conn = self.conn.lock().expect("mutex poisoned");
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
        let id = self.generate_id()?;
        let now = Self::now_iso();
        let conn = self.conn.lock().expect("mutex poisoned");

        conn.execute(
            "INSERT INTO notes (id, title, content, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, title, content, now, now],
        )?;

        for tag in tags {
            conn.execute(
                "INSERT INTO tags (note_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )?;
        }

        Ok(Note {
            id,
            title: title.to_string(),
            content: content.to_string(),
            tags: tags.to_vec(),
            links: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    fn read_note(&self, id: &str) -> MemoryResult<Note> {
        if id == "core" {
            return self.read_core();
        }

        let conn = self.conn.lock().expect("mutex poisoned");
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
        let conn = self.conn.lock().expect("mutex poisoned");

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
        let conn = self.conn.lock().expect("mutex poisoned");

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
        let conn = self.conn.lock().expect("mutex poisoned");
        let pattern = format!("%{}%", query);

        let mut stmt = conn.prepare(
            "SELECT DISTINCT n.id, n.title, n.content, n.created_at, n.updated_at
             FROM notes n
             LEFT JOIN tags t ON n.id = t.note_id
             WHERE n.deleted_at IS NULL
               AND (n.title LIKE ?1 OR n.content LIKE ?1 OR t.tag LIKE ?1)
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

        // Populate tags for each note
        let mut result = Vec::with_capacity(notes.len());
        for note in notes {
            let tags = self.get_tags_for_note_locked(&conn, &note.id)?;
            result.push(Note { tags, ..note });
        }

        Ok(result)
    }

    fn list_notes(&self, tag_filter: Option<&str>) -> MemoryResult<Vec<Note>> {
        let conn = self.conn.lock().expect("mutex poisoned");

        let notes = if let Some(tag) = tag_filter {
            let mut stmt = conn.prepare(
                "SELECT n.id, n.title, n.content, n.created_at, n.updated_at
                 FROM notes n
                 JOIN tags t ON n.id = t.note_id
                 WHERE n.deleted_at IS NULL AND t.tag = ?1
                 ORDER BY n.updated_at DESC",
            )?;
            stmt.query_map(params![tag], |row| {
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
            result.push(Note { tags, ..note });
        }

        Ok(result)
    }

    fn link_notes(&self, from_id: &str, to_id: &str, relation: &str) -> MemoryResult<Link> {
        if from_id == "core" || to_id == "core" {
            return Err(MemoryError::ReservedId);
        }

        let conn = self.conn.lock().expect("mutex poisoned");

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
        let conn = self.conn.lock().expect("mutex poisoned");
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

        let conn = self.conn.lock().expect("mutex poisoned");
        self.assert_note_exists_locked(&conn, id)?;

        // Replace all tags
        conn.execute("DELETE FROM tags WHERE note_id = ?1", params![id])?;
        for tag in tags {
            conn.execute(
                "INSERT INTO tags (note_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )?;
        }

        let now = Self::now_iso();
        conn.execute(
            "UPDATE notes SET updated_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;

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

    fn test_store() -> SqliteMemory {
        let conn = Connection::open_in_memory().unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let tmp = std::env::temp_dir().join("test_core.md");
        std::fs::write(&tmp, "# Test Core\nI am a test persona.").unwrap();
        SqliteMemory::new(conn, tmp).unwrap()
    }

    #[test]
    fn create_and_read_note() {
        let store = test_store();
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
        let store = test_store();
        let note = store.create_note("Title", "Old content", &[]).unwrap();
        let updated = store.update_note(&note.id, "New content").unwrap();
        assert_eq!(updated.content, "New content");
        assert_eq!(updated.title, "Title");
    }

    #[test]
    fn forget_note_excludes_from_search() {
        let store = test_store();
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
        let store = test_store();
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
        let store = test_store();
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
        let store = test_store();
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
    fn list_with_tag_filter() {
        let store = test_store();
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
        let store = test_store();

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
        let store = test_store();
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
        let store = test_store();

        let err = store.read_note("note_nonexist").unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));

        let err = store.update_note("note_nonexist", "content").unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));

        let err = store.forget_note("note_nonexist").unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
    }
}
