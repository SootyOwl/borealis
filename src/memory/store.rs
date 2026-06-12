use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
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
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TagCount {
    pub tag: String,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaginatedNotes {
    pub notes: Vec<NoteSummary>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
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
    fn search_notes(
        &self,
        query: &str,
        limit: usize,
        tag_filter: Option<&str>,
        tag_exact: bool,
    ) -> MemoryResult<Vec<Note>>;
    fn list_notes(&self, tag_filter: Option<&str>) -> MemoryResult<Vec<Note>>;
    fn link_notes(&self, from_id: &str, to_id: &str, relation: &str) -> MemoryResult<Link>;
    fn get_links_for_note(&self, id: &str) -> MemoryResult<Vec<Link>>;
    fn tag_note(&self, id: &str, tags: &[String]) -> MemoryResult<Note>;
    fn load_core_persona(&self) -> MemoryResult<String>;
    fn list_notes_paginated(
        &self,
        tag_filter: Option<&str>,
        tag_exact: bool,
        limit: usize,
        offset: usize,
    ) -> MemoryResult<PaginatedNotes>;
    fn list_tags(&self, prefix: Option<&str>) -> MemoryResult<Vec<TagCount>>;
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
    ///
    /// A panic while holding the lock poisons the mutex, but the SQLite
    /// connection state itself is sound across a Rust panic (statement-level
    /// rollback applies), so we recover the guard rather than permanently
    /// bricking every store that shares this connection.
    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("memory store connection mutex was poisoned by a panic; recovering");
            poisoned.into_inner()
        })
    }

    fn init_schema(&self) -> MemoryResult<()> {
        let conn = self.lock_conn();

        // Step 1: create base tables and index.
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

        // Step 2: one-time backfill of legacy second-precision timestamps
        // (`...:SSZ`) to millisecond format (`...:SS.000Z`).
        //
        // This MUST run BEFORE the FTS5 triggers are created.  The UPDATE
        // statements here would fire `notes_au` if the trigger existed, causing
        // FTS5 to try to delete rows it has never indexed — which raises
        // SQLITE_CORRUPT_VTAB on the empty FTS5 index.  Doing the timestamp
        // backfill first avoids that.
        //
        // Idempotent: filtered to rows that don't already contain a `.`.
        conn.execute_batch(
            "UPDATE notes SET created_at = REPLACE(created_at, 'Z', '.000Z')
                 WHERE created_at NOT LIKE '%.%';
             UPDATE notes SET updated_at = REPLACE(updated_at, 'Z', '.000Z')
                 WHERE updated_at NOT LIKE '%.%';
             UPDATE notes SET deleted_at = REPLACE(deleted_at, 'Z', '.000Z')
                 WHERE deleted_at IS NOT NULL AND deleted_at NOT LIKE '%.%';",
        )?;

        // Step 3: create the FTS5 virtual table and sync triggers.
        //
        // These are created AFTER the timestamp backfill so that the backfill
        // UPDATEs don't fire `notes_au` against an empty (uninitialized) FTS5
        // index.
        //
        // We use a regular (full-content) FTS5 table rather than an
        // external-content table (`content='notes'`).  External-content tables
        // "pass through" SELECT COUNT(*) to the source table, which makes the
        // standard `EXISTS(SELECT 1 FROM notes_fts LIMIT 1)` backfill check
        // unreliable — it always returns true even when the FTS index is empty.
        // Storing the indexed text directly in notes_fts is simpler, idempotent,
        // and the storage overhead is acceptable given the note sizes in practice.
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
                 title,
                 content,
                 tokenize='porter unicode61'
             );

             CREATE TRIGGER IF NOT EXISTS notes_ai AFTER INSERT ON notes BEGIN
                 INSERT INTO notes_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
             END;

             CREATE TRIGGER IF NOT EXISTS notes_ad AFTER DELETE ON notes BEGIN
                 DELETE FROM notes_fts WHERE rowid = old.rowid;
             END;

             -- Normal update: re-index only when the note is still live.
             CREATE TRIGGER IF NOT EXISTS notes_au AFTER UPDATE ON notes
             WHEN new.deleted_at IS NULL
             BEGIN
                 DELETE FROM notes_fts WHERE rowid = old.rowid;
                 INSERT INTO notes_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
             END;

             -- Soft-delete: remove from the FTS index when the note transitions from live to deleted.
             CREATE TRIGGER IF NOT EXISTS notes_soft_delete AFTER UPDATE ON notes
             WHEN new.deleted_at IS NOT NULL AND old.deleted_at IS NULL
             BEGIN
                 DELETE FROM notes_fts WHERE rowid = new.rowid;
             END;",
        )?;

        // Step 4: idempotent FTS5 backfill.
        //
        // Populate the index from existing notes if the index is empty but the
        // notes table is not.  For a regular (full-content) FTS5 table,
        // `EXISTS(SELECT 1 FROM notes_fts LIMIT 1)` correctly detects whether
        // any notes have been indexed.  Cheap on subsequent boots — the SELECT
        // returns immediately when the index is already populated.
        let fts_populated: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM notes_fts LIMIT 1)", [], |row| {
                row.get(0)
            })?;
        let notes_present: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM notes LIMIT 1)", [], |row| {
                row.get(0)
            })?;
        if !fts_populated && notes_present {
            // Soft-deleted notes are excluded from the backfill: they would pollute
            // bm25 corpus statistics for content that is no longer accessible, and
            // the soft-delete trigger (notes_soft_delete) ensures they are removed
            // from the index going forward anyway.
            conn.execute(
                "INSERT INTO notes_fts(rowid, title, content)
                 SELECT rowid, title, content FROM notes WHERE deleted_at IS NULL",
                [],
            )?;
        }

        Ok(())
    }

    /// Generate a note ID like `note_a1b2c3d4` using a random u32.
    /// Checks for collisions and regenerates if needed.
    fn generate_id(&self) -> MemoryResult<String> {
        let conn = self.lock_conn();
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

    /// Current UTC timestamp in RFC 3339 format with millisecond precision
    /// (e.g. `2026-04-07T18:42:11.123Z`).
    ///
    /// Millisecond precision matters for `ORDER BY updated_at` stability:
    /// the previous second-resolution format made bulk-creates indistinguishable
    /// and forced a secondary sort by id everywhere. Format is also chosen to
    /// lex-sort identically to chronological order, so SQLite's text comparison
    /// gives correct ordering without any custom collation.
    fn now_iso() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
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

    /// Execute the FTS5 MATCH query for `search_notes`.
    ///
    /// On a FTS5 syntax error (e.g. stray `:` in the query string), this
    /// retries once with the query wrapped in double-quotes as a literal phrase.
    /// That way Aurora can search for `"foo:bar"` without knowing FTS5 grammar.
    ///
    /// FTS5 parse errors arrive as `rusqlite::Error::SqliteFailure` with
    /// `ffi::ErrorCode::Unknown` (SQLITE_ERROR, code 1).
    ///
    /// Safety argument: `execute_fts_query` issues a single, static FTS5 MATCH
    /// statement (`SELECT rowid FROM notes_fts WHERE notes_fts MATCH ?1 ...`).
    /// There is zero schema variability in that statement — the only input is the
    /// FTS5 query string bound as a parameter.  Therefore any `Unknown`-coded
    /// `SqliteFailure` returned from running it must originate from the FTS5 query
    /// parser, not from a structural DB problem (missing tables, corrupt schema,
    /// etc.).  Genuine structural errors would return different error codes
    /// (`CannotOpen`, `NotFound`, `Corrupt`, …) and are not masked here.
    fn run_fts_query(
        &self,
        conn: &Connection,
        query: &str,
        normalized_tag: &Option<String>,
        tag_exact: bool,
        limit: usize,
    ) -> MemoryResult<Vec<Note>> {
        match self.execute_fts_query(conn, query, normalized_tag, tag_exact, limit) {
            Ok(notes) => Ok(notes),
            Err(MemoryError::Database(rusqlite::Error::SqliteFailure(err, _)))
                if err.code == rusqlite::ffi::ErrorCode::Unknown =>
            {
                // FTS5 query parse error — retry as a literal phrase.  The FTS5 MATCH
                // statement above has no schema variability, so any Unknown-coded SQL
                // error from running it must originate from the FTS5 query parser.
                let phrase_query = format!("\"{}\"", query.replace('"', "\"\""));
                self.execute_fts_query(conn, &phrase_query, normalized_tag, tag_exact, limit)
            }
            Err(e) => Err(e),
        }
    }

    /// The actual SQL execution for `run_fts_query`.  Separated so the
    /// phrase-fallback can call it cleanly without recursion risk.
    ///
    /// `bm25()` can only be used in the `ORDER BY` of a query that has the FTS5
    /// table directly in `FROM` (not in a `JOIN`).  Soft-delete and tag filters
    /// are pushed into a `rowid IN (...)` subquery — SQLite evaluates that as a
    /// pre-sort filter, so `bm25(notes_fts)` still applies and `LIMIT` is
    /// honoured exactly.  An earlier version of this function ran an unscoped
    /// FTS match, overfetched 10× the limit, then post-filtered tags in Rust;
    /// that silently dropped results when untagged notes ranked ahead of tagged
    /// matches.
    ///
    /// The GLOB pattern `tag || '/*'` is used (not LIKE) for consistency with
    /// `list_notes` and `list_notes_paginated` — `_` is an allowed tag char and a
    /// LIKE wildcard; GLOB metacharacters (`*`, `?`, `[`) are all rejected by
    /// `normalize_tag`, so GLOB is safe.
    fn execute_fts_query(
        &self,
        conn: &Connection,
        fts_query: &str,
        normalized_tag: &Option<String>,
        tag_exact: bool,
        limit: usize,
    ) -> MemoryResult<Vec<Note>> {
        let rowids: Vec<i64> = match normalized_tag {
            None => {
                let mut stmt = conn.prepare(
                    "SELECT rowid FROM notes_fts
                     WHERE notes_fts MATCH ?1
                       AND rowid IN (SELECT rowid FROM notes WHERE deleted_at IS NULL)
                     ORDER BY bm25(notes_fts) LIMIT ?2",
                )?;
                stmt.query_map(params![fts_query, limit as i64], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            }
            Some(tag) if tag_exact => {
                let mut stmt = conn.prepare(
                    "SELECT rowid FROM notes_fts
                     WHERE notes_fts MATCH ?1
                       AND rowid IN (
                           SELECT n.rowid FROM notes n
                           JOIN tags t ON t.note_id = n.id
                           WHERE n.deleted_at IS NULL AND t.tag = ?2
                       )
                     ORDER BY bm25(notes_fts) LIMIT ?3",
                )?;
                stmt.query_map(params![fts_query, tag, limit as i64], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            }
            Some(tag) => {
                let glob_prefix = format!("{tag}/*");
                let mut stmt = conn.prepare(
                    "SELECT rowid FROM notes_fts
                     WHERE notes_fts MATCH ?1
                       AND rowid IN (
                           SELECT n.rowid FROM notes n
                           JOIN tags t ON t.note_id = n.id
                           WHERE n.deleted_at IS NULL AND (t.tag = ?2 OR t.tag GLOB ?3)
                       )
                     ORDER BY bm25(notes_fts) LIMIT ?4",
                )?;
                stmt.query_map(
                    params![fts_query, tag, glob_prefix, limit as i64],
                    |row| row.get(0),
                )?
                .collect::<Result<Vec<_>, _>>()?
            }
        };

        if rowids.is_empty() {
            return Ok(Vec::new());
        }

        // Load each note in bm25 order.  Soft-delete and tag filtering already
        // happened in the rowid query above; this loop just hydrates.
        let mut result = Vec::with_capacity(rowids.len());
        for rowid in rowids {
            let note: Option<Note> = conn
                .query_row(
                    "SELECT id, title, content, created_at, updated_at
                     FROM notes WHERE rowid = ?1",
                    params![rowid],
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
                .optional()
                .map_err(MemoryError::Database)?;
            if let Some(n) = note {
                result.push(n);
            }
        }

        Ok(result)
    }
}

impl Memory for SqliteMemory {
    fn create_note(&self, title: &str, content: &str, tags: &[String]) -> MemoryResult<Note> {
        let normalized = normalize_tags(tags)?;
        let id = self.generate_id()?;
        let now = Self::now_iso();
        let conn = self.lock_conn();

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

        let conn = self.lock_conn();
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
        let conn = self.lock_conn();

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
        let conn = self.lock_conn();

        let rows = conn.execute(
            "UPDATE notes SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![now, id],
        )?;

        if rows == 0 {
            return Err(MemoryError::NotFound(id.to_string()));
        }

        Ok(())
    }

    fn search_notes(
        &self,
        query: &str,
        limit: usize,
        tag_filter: Option<&str>,
        tag_exact: bool,
    ) -> MemoryResult<Vec<Note>> {
        // Normalize the tag filter if provided.
        let normalized_tag: Option<String> = match tag_filter {
            Some(tag) => Some(normalize_tag(tag)?),
            None => None,
        };

        // An empty or whitespace-only query has no meaningful results.  Returning
        // an empty vec is correct: it avoids the FTS5 phrase fallback synthesizing
        // `""`, which would match every document in the index.
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // No point running the FTS query + overfetch loop when the caller wants zero results.
        if limit == 0 {
            return Ok(Vec::new());
        }

        let conn = self.lock_conn();

        // Run the FTS5 query, falling back to a phrase-wrapped query on parse error.
        let notes = self.run_fts_query(&conn, query, &normalized_tag, tag_exact, limit)?;

        // Populate tags and links for each result.
        let mut result = Vec::with_capacity(notes.len());
        for note in notes {
            let tags = self.get_tags_for_note_locked(&conn, &note.id)?;
            let links = self.get_links_for_note_locked(&conn, &note.id)?;
            result.push(Note { tags, links, ..note });
        }

        Ok(result)
    }

    fn list_notes(&self, tag_filter: Option<&str>) -> MemoryResult<Vec<Note>> {
        let conn = self.lock_conn();

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

        let conn = self.lock_conn();

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
        let conn = self.lock_conn();
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
        let conn = self.lock_conn();
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

    fn list_notes_paginated(
        &self,
        tag_filter: Option<&str>,
        tag_exact: bool,
        limit: usize,
        offset: usize,
    ) -> MemoryResult<PaginatedNotes> {
        let conn = self.lock_conn();

        // Normalize the tag filter if provided.
        let normalized = match tag_filter {
            Some(tag) => Some(normalize_tag(tag)?),
            None => None,
        };

        // --- COUNT query ---
        let total: usize = match &normalized {
            None => {
                conn.query_row(
                    "SELECT COUNT(*) FROM notes WHERE deleted_at IS NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )? as usize
            }
            Some(tag) if tag_exact => {
                conn.query_row(
                    "SELECT COUNT(DISTINCT n.id)
                     FROM notes n
                     JOIN tags t ON n.id = t.note_id
                     WHERE n.deleted_at IS NULL
                       AND t.tag = ?1",
                    params![tag],
                    |row| row.get::<_, i64>(0),
                )? as usize
            }
            Some(tag) => {
                conn.query_row(
                    "SELECT COUNT(DISTINCT n.id)
                     FROM notes n
                     JOIN tags t ON n.id = t.note_id
                     WHERE n.deleted_at IS NULL
                       AND (t.tag = ?1 OR t.tag GLOB ?1 || '/*')",
                    params![tag],
                    |row| row.get::<_, i64>(0),
                )? as usize
            }
        };

        // --- PAGE query ---
        let summaries: Vec<NoteSummary> = if limit == 0 {
            Vec::new()
        } else {
            match &normalized {
                None => {
                    let mut stmt = conn.prepare(
                        "SELECT id, title, created_at, updated_at
                         FROM notes
                         WHERE deleted_at IS NULL
                         ORDER BY updated_at DESC, id ASC
                         LIMIT ?1 OFFSET ?2",
                    )?;
                    stmt.query_map(params![limit as i64, offset as i64], |row| {
                        Ok(NoteSummary {
                            id: row.get(0)?,
                            title: row.get(1)?,
                            tags: Vec::new(),
                            created_at: row.get(2)?,
                            updated_at: row.get(3)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                }
                Some(tag) if tag_exact => {
                    let mut stmt = conn.prepare(
                        "SELECT DISTINCT n.id, n.title, n.created_at, n.updated_at
                         FROM notes n
                         JOIN tags t ON n.id = t.note_id
                         WHERE n.deleted_at IS NULL
                           AND t.tag = ?1
                         ORDER BY n.updated_at DESC, n.id ASC
                         LIMIT ?2 OFFSET ?3",
                    )?;
                    stmt.query_map(params![tag, limit as i64, offset as i64], |row| {
                        Ok(NoteSummary {
                            id: row.get(0)?,
                            title: row.get(1)?,
                            tags: Vec::new(),
                            created_at: row.get(2)?,
                            updated_at: row.get(3)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                }
                Some(tag) => {
                    let mut stmt = conn.prepare(
                        "SELECT DISTINCT n.id, n.title, n.created_at, n.updated_at
                         FROM notes n
                         JOIN tags t ON n.id = t.note_id
                         WHERE n.deleted_at IS NULL
                           AND (t.tag = ?1 OR t.tag GLOB ?1 || '/*')
                         ORDER BY n.updated_at DESC, n.id ASC
                         LIMIT ?2 OFFSET ?3",
                    )?;
                    stmt.query_map(params![tag, limit as i64, offset as i64], |row| {
                        Ok(NoteSummary {
                            id: row.get(0)?,
                            title: row.get(1)?,
                            tags: Vec::new(),
                            created_at: row.get(2)?,
                            updated_at: row.get(3)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                }
            }
        };

        // Populate tags for each summary.
        let mut notes = Vec::with_capacity(summaries.len());
        for s in summaries {
            let tags = self.get_tags_for_note_locked(&conn, &s.id)?;
            notes.push(NoteSummary { tags, ..s });
        }

        Ok(PaginatedNotes {
            notes,
            total,
            offset,
            limit,
        })
    }

    fn list_tags(&self, prefix: Option<&str>) -> MemoryResult<Vec<TagCount>> {
        let conn = self.lock_conn();

        let normalized: Option<String> = match prefix {
            Some(p) => Some(normalize_tag(p)?),
            None => None,
        };

        let mut stmt = conn.prepare(
            "SELECT t.tag, COUNT(*) AS cnt
             FROM tags t
             JOIN notes n ON t.note_id = n.id
             WHERE n.deleted_at IS NULL
               AND (?1 IS NULL OR t.tag = ?1 OR t.tag GLOB ?1 || '/*')
             GROUP BY t.tag
             ORDER BY cnt DESC, t.tag ASC",
        )?;

        let counts = stmt
            .query_map(params![normalized], |row| {
                Ok(TagCount {
                    tag: row.get(0)?,
                    count: row.get::<_, i64>(1)? as usize,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(counts)
    }
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
    fn memory_store_recovers_from_poisoned_mutex() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# Test Core").unwrap();
        let store = SqliteMemory::new(Arc::clone(&conn), tmp.path().to_path_buf()).unwrap();

        // Poison the shared mutex by panicking while holding the lock.
        let poisoner = Arc::clone(&conn);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the shared connection mutex");
        })
        .join();
        assert!(conn.is_poisoned(), "mutex should be poisoned");

        // The store must keep working after the poisoning panic.
        let note = store
            .create_note("After poison", "still alive", &[])
            .expect("memory store should recover from a poisoned mutex");
        let read = store.read_note(&note.id).unwrap();
        assert_eq!(read.content, "still alive");
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

        let results = store.search_notes("Forgettable", 10, None, false).unwrap();
        assert!(results.is_empty());

        let err = store.read_note(&note.id).unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
    }

    #[test]
    fn search_by_title_and_content() {
        // FTS5 indexes title and content; tags are not in the FTS5 index.
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
        let results = store.search_notes("Rust", 10, None, false).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust Programming");

        // Search by content
        let results = store.search_notes("carbonara", 10, None, false).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Cooking Recipe");

        // FTS5 searches title and content only (not tags directly),
        // so "code" won't match via tag search.  Search by content keyword instead.
        let results = store.search_notes("Systems", 10, None, false).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust Programming");
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

        fn search_notes(
            &self,
            query: &str,
            limit: usize,
            tag_filter: Option<&str>,
            tag_exact: bool,
        ) -> MemoryResult<Vec<Note>> {
            // Normalize the tag filter if provided — propagates InvalidTag errors.
            let normalized_tag: Option<String> = match tag_filter {
                Some(tag) => Some(normalize_tag(tag)?),
                None => None,
            };

            // Parity with SqliteMemory: empty/whitespace query returns nothing.
            if query.trim().is_empty() {
                return Ok(Vec::new());
            }

            // Short-circuit: limit == 0 means caller wants nothing.
            if limit == 0 {
                return Ok(Vec::new());
            }

            let notes = self.notes.lock().unwrap();
            Ok(notes
                .iter()
                .filter(|n| {
                    // Substring match on title + content.
                    let text_match = n.title.contains(query) || n.content.contains(query);
                    if !text_match {
                        return false;
                    }
                    // Apply tag filter.
                    match &normalized_tag {
                        None => true,
                        Some(tag) if tag_exact => n.tags.iter().any(|t| t == tag),
                        Some(tag) => {
                            let prefix = format!("{tag}/");
                            n.tags.iter().any(|t| t == tag || t.starts_with(&prefix))
                        }
                    }
                })
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

        fn list_notes_paginated(
            &self,
            tag_filter: Option<&str>,
            tag_exact: bool,
            limit: usize,
            offset: usize,
        ) -> MemoryResult<PaginatedNotes> {
            let notes = self.notes.lock().unwrap();

            let normalized = match tag_filter {
                Some(tag) => Some(normalize_tag(tag)?),
                None => None,
            };

            let mut filtered: Vec<&Note> = notes
                .iter()
                .filter(|n| match &normalized {
                    None => true,
                    Some(tag) if tag_exact => n.tags.iter().any(|t| t == tag),
                    Some(tag) => {
                        let prefix = format!("{tag}/");
                        n.tags.iter().any(|t| t == tag || t.starts_with(&prefix))
                    }
                })
                .collect();

            // Stable sort: updated_at DESC, id ASC — mirrors the SQLite ORDER BY.
            filtered.sort_by(|a, b| {
                b.updated_at
                    .cmp(&a.updated_at)
                    .then(a.id.cmp(&b.id))
            });

            let total = filtered.len();

            let page: Vec<NoteSummary> = if limit == 0 {
                Vec::new()
            } else {
                filtered
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(|n| NoteSummary {
                        id: n.id.clone(),
                        title: n.title.clone(),
                        tags: n.tags.clone(),
                        created_at: n.created_at.clone(),
                        updated_at: n.updated_at.clone(),
                    })
                    .collect()
            };

            Ok(PaginatedNotes {
                notes: page,
                total,
                offset,
                limit,
            })
        }

        fn list_tags(&self, prefix: Option<&str>) -> MemoryResult<Vec<TagCount>> {
            let notes = self.notes.lock().unwrap();

            let normalized: Option<String> = match prefix {
                Some(p) => Some(normalize_tag(p)?),
                None => None,
            };

            let mut counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();

            for note in notes.iter() {
                for tag in &note.tags {
                    let include = match &normalized {
                        None => true,
                        Some(pfx) => {
                            let child_prefix = format!("{pfx}/");
                            tag == pfx || tag.starts_with(&child_prefix)
                        }
                    };
                    if include {
                        *counts.entry(tag.clone()).or_insert(0) += 1;
                    }
                }
            }

            let mut result: Vec<TagCount> = counts
                .into_iter()
                .map(|(tag, count)| TagCount { tag, count })
                .collect();

            // Sort: count desc, tag asc (for deterministic tie-breaking).
            result.sort_by(|a, b| b.count.cmp(&a.count).then(a.tag.cmp(&b.tag)));

            Ok(result)
        }
    }

    // -----------------------------------------------------------------------
    // Pagination tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_notes_paginated_returns_summary_projection() {
        let (_tmp, store) = test_store();
        let note = store
            .create_note("My Title", "My content", &["cooking".into()])
            .unwrap();

        let page = store
            .list_notes_paginated(None, false, 20, 0)
            .unwrap();

        assert_eq!(page.total, 1);
        assert_eq!(page.notes.len(), 1);
        assert_eq!(page.offset, 0, "envelope offset should echo the request");
        assert_eq!(page.limit, 20, "envelope limit should echo the request");
        let s = &page.notes[0];
        assert_eq!(s.id, note.id);
        assert_eq!(s.title, "My Title");
        assert_eq!(s.tags, vec!["cooking"]);
        assert!(!s.created_at.is_empty());
        assert!(!s.updated_at.is_empty());
    }

    #[test]
    fn list_notes_paginated_respects_offset_and_limit() {
        let (_tmp, store) = test_store();
        for i in 0..5 {
            // Sleep 1ms to ensure distinct updated_at ordering on systems
            // where the clock resolution is coarser than per-note creates.
            std::thread::sleep(std::time::Duration::from_millis(1));
            store
                .create_note(&format!("Note {i}"), "Content", &[])
                .unwrap();
        }

        let p0 = store.list_notes_paginated(None, false, 2, 0).unwrap();
        assert_eq!(p0.total, 5);
        assert_eq!(p0.notes.len(), 2);
        assert_eq!(p0.offset, 0);
        assert_eq!(p0.limit, 2);

        let p1 = store.list_notes_paginated(None, false, 2, 2).unwrap();
        assert_eq!(p1.total, 5);
        assert_eq!(p1.notes.len(), 2);
        assert_eq!(p1.offset, 2);
        assert_eq!(p1.limit, 2);

        let p2 = store.list_notes_paginated(None, false, 2, 4).unwrap();
        assert_eq!(p2.total, 5);
        assert_eq!(p2.notes.len(), 1);
        assert_eq!(p2.offset, 4);
        assert_eq!(p2.limit, 2);

        // No overlap between pages.
        let ids_p0: std::collections::HashSet<&str> = p0.notes.iter().map(|n| n.id.as_str()).collect();
        let ids_p1: std::collections::HashSet<&str> = p1.notes.iter().map(|n| n.id.as_str()).collect();
        let ids_p2: std::collections::HashSet<&str> = p2.notes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids_p0.is_disjoint(&ids_p1));
        assert!(ids_p0.is_disjoint(&ids_p2));
        assert!(ids_p1.is_disjoint(&ids_p2));
    }

    #[test]
    fn list_notes_paginated_offset_beyond_total_returns_empty() {
        let (_tmp, store) = test_store();
        for i in 0..5 {
            store.create_note(&format!("N{i}"), "C", &[]).unwrap();
        }

        let page = store.list_notes_paginated(None, false, 20, 10).unwrap();
        assert_eq!(page.total, 5);
        assert!(page.notes.is_empty());
    }

    #[test]
    fn list_notes_paginated_limit_zero() {
        let (_tmp, store) = test_store();
        store.create_note("N", "C", &[]).unwrap();

        let page = store.list_notes_paginated(None, false, 0, 0).unwrap();
        assert_eq!(page.total, 1);
        assert!(page.notes.is_empty());
        assert_eq!(page.limit, 0);
    }

    #[test]
    fn list_notes_paginated_with_prefix_filter() {
        let (_tmp, store) = test_store();
        store
            .create_note("Carbonara", "X", &["recipes/italian/carbonara".into()])
            .unwrap();
        store
            .create_note("Ratatouille", "X", &["recipes/french".into()])
            .unwrap();
        store
            .create_note("Unrelated", "X", &["recipe".into()])
            .unwrap();

        // Prefix match — "recipes" matches italian and french subtrees but not "recipe".
        let page = store
            .list_notes_paginated(Some("recipes"), false, 20, 0)
            .unwrap();
        assert_eq!(page.total, 2);

        // Exact match — only notes tagged exactly "recipes/italian/carbonara".
        let exact = store
            .list_notes_paginated(Some("recipes/italian/carbonara"), true, 20, 0)
            .unwrap();
        assert_eq!(exact.total, 1);
        assert_eq!(exact.notes[0].title, "Carbonara");

        // Exact match on parent should not pull in children.
        let exact_recipes = store
            .list_notes_paginated(Some("recipes"), true, 20, 0)
            .unwrap();
        assert_eq!(exact_recipes.total, 0);
    }

    #[test]
    fn list_notes_paginated_total_excludes_soft_deleted() {
        let (_tmp, store) = test_store();
        let n1 = store.create_note("Keep", "C", &[]).unwrap();
        let n2 = store.create_note("Delete", "C", &[]).unwrap();
        store.forget_note(&n2.id).unwrap();
        let _ = n1;

        let page = store.list_notes_paginated(None, false, 20, 0).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.notes.len(), 1);
        assert_eq!(page.notes[0].title, "Keep");
    }

    #[test]
    fn list_notes_paginated_propagates_invalid_tag_filter() {
        let (_tmp, store) = test_store();
        let err = store
            .list_notes_paginated(Some("bad tag"), false, 20, 0)
            .unwrap_err();
        assert!(matches!(err, MemoryError::InvalidTag(_)));
    }

    #[test]
    fn now_iso_has_millisecond_precision() {
        // Format: `YYYY-MM-DDTHH:MM:SS.mmmZ` — exactly 24 chars, with `.` at
        // position 19. This is what we need for stable lex-sort ordering
        // across the millisecond boundary.
        let ts = SqliteMemory::now_iso();
        assert_eq!(ts.len(), 24, "expected 24-char RFC3339 millis: {ts}");
        assert_eq!(&ts[19..20], ".", "expected `.` at position 19: {ts}");
        assert!(ts.ends_with('Z'), "expected trailing Z: {ts}");
    }

    #[test]
    fn legacy_second_precision_timestamps_are_backfilled_on_open() {
        // Simulate a legacy database with second-precision timestamps and
        // verify init_schema upgrades them to millisecond format on the next
        // open. The backfill is critical because lex-sort of mixed formats
        // is inconsistent (`.` < `Z`).
        let conn = Connection::open_in_memory().unwrap();
        // Manually create the legacy schema and insert a row with the old format.
        conn.execute_batch(
            "CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE TABLE tags (
                note_id TEXT NOT NULL REFERENCES notes(id),
                tag TEXT NOT NULL,
                PRIMARY KEY (note_id, tag)
            );
            CREATE TABLE links (
                from_id TEXT NOT NULL REFERENCES notes(id),
                to_id TEXT NOT NULL REFERENCES notes(id),
                relation TEXT NOT NULL,
                PRIMARY KEY (from_id, to_id)
            );
            INSERT INTO notes (id, title, content, created_at, updated_at, deleted_at)
            VALUES ('legacy_1', 'Old', 'X', '2025-01-15T12:00:00Z', '2025-01-15T12:00:00Z', NULL),
                   ('legacy_2', 'Deleted', 'X', '2025-01-15T12:00:00Z', '2025-01-15T12:00:01Z', '2025-01-15T12:00:02Z');",
        )
        .unwrap();

        // Now open via SqliteMemory, which runs init_schema and should backfill.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# core").unwrap();
        let store = SqliteMemory::new(
            Arc::new(Mutex::new(conn)),
            tmp.path().to_path_buf(),
        )
        .unwrap();

        let conn = store.lock_conn();
        let (created, updated, deleted): (String, String, Option<String>) = conn
            .query_row(
                "SELECT created_at, updated_at, deleted_at FROM notes WHERE id = 'legacy_2'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(created, "2025-01-15T12:00:00.000Z");
        assert_eq!(updated, "2025-01-15T12:00:01.000Z");
        assert_eq!(deleted, Some("2025-01-15T12:00:02.000Z".to_string()));

        // The non-deleted row's deleted_at must remain NULL.
        let null_deleted: Option<String> = conn
            .query_row(
                "SELECT deleted_at FROM notes WHERE id = 'legacy_1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(null_deleted, None);
    }

    #[test]
    fn list_notes_paginated_deterministic_order_when_timestamps_tie() {
        // The tiebreaker `ORDER BY id ASC` is load-bearing: any two notes that
        // share an `updated_at` value must still paginate in a stable order, or
        // rows can appear on multiple pages (or be skipped) between successive
        // LIMIT/OFFSET calls.  `now_iso()` is millisecond-precision, so genuine
        // collisions are rare under wall-clock — we force one here by writing
        // the same `updated_at` to every row.
        let (_tmp, store) = test_store();
        for i in 0..4 {
            store
                .create_note(&format!("Tie{i}"), "Content", &[])
                .unwrap();
        }
        {
            let conn = store.lock_conn();
            conn.execute(
                "UPDATE notes SET updated_at = '2026-04-07T12:00:00Z' WHERE deleted_at IS NULL",
                [],
            )
            .unwrap();
        }

        let p0 = store.list_notes_paginated(None, false, 2, 0).unwrap();
        assert_eq!(p0.total, 4);
        assert_eq!(p0.notes.len(), 2);

        let p1 = store.list_notes_paginated(None, false, 2, 2).unwrap();
        assert_eq!(p1.total, 4);
        assert_eq!(p1.notes.len(), 2);

        // Collect all IDs across both pages and verify no duplicates, no gaps.
        let mut all_ids: Vec<String> = p0
            .notes
            .iter()
            .chain(p1.notes.iter())
            .map(|n| n.id.clone())
            .collect();
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(all_ids.len(), 4, "expected 4 distinct IDs across both pages");

        // Calling the same pages again must return the same content — proves the
        // ordering is stable, not just non-overlapping by accident.
        let p0_again = store.list_notes_paginated(None, false, 2, 0).unwrap();
        let p1_again = store.list_notes_paginated(None, false, 2, 2).unwrap();
        let p0_ids: Vec<&str> = p0.notes.iter().map(|n| n.id.as_str()).collect();
        let p0_again_ids: Vec<&str> =
            p0_again.notes.iter().map(|n| n.id.as_str()).collect();
        let p1_ids: Vec<&str> = p1.notes.iter().map(|n| n.id.as_str()).collect();
        let p1_again_ids: Vec<&str> =
            p1_again.notes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(p0_ids, p0_again_ids);
        assert_eq!(p1_ids, p1_again_ids);
    }

    // -----------------------------------------------------------------------
    // list_tags tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_tags_returns_counts_sorted_by_count_desc() {
        let (_tmp, store) = test_store();
        // "common" appears 3 times, "rare" 1 time, "also-rare" 1 time.
        for _ in 0..3 {
            store
                .create_note("N", "C", &["common".into()])
                .unwrap();
        }
        store.create_note("N", "C", &["rare".into()]).unwrap();
        store.create_note("N", "C", &["also-rare".into()]).unwrap();

        let tags = store.list_tags(None).unwrap();
        assert_eq!(tags[0].tag, "common");
        assert_eq!(tags[0].count, 3);
        // Ties broken alphabetically: "also-rare" < "rare".
        assert_eq!(tags[1].tag, "also-rare");
        assert_eq!(tags[1].count, 1);
        assert_eq!(tags[2].tag, "rare");
        assert_eq!(tags[2].count, 1);
    }

    #[test]
    fn list_tags_excludes_soft_deleted() {
        let (_tmp, store) = test_store();
        let n = store
            .create_note("N", "C", &["mytag".into()])
            .unwrap();
        store.forget_note(&n.id).unwrap();

        let tags = store.list_tags(None).unwrap();
        assert!(tags.iter().all(|t| t.tag != "mytag"));
    }

    #[test]
    fn list_tags_with_prefix() {
        let (_tmp, store) = test_store();
        store
            .create_note("A", "C", &["recipe".into()])
            .unwrap();
        store
            .create_note("B", "C", &["recipes".into()])
            .unwrap();
        store
            .create_note("C", "C", &["recipes/italian".into()])
            .unwrap();

        let tags = store.list_tags(Some("recipes")).unwrap();
        let tag_names: Vec<&str> = tags.iter().map(|t| t.tag.as_str()).collect();
        // "recipe" must NOT appear — `recipes` prefix does not match `recipe`.
        assert!(!tag_names.contains(&"recipe"));
        assert!(tag_names.contains(&"recipes"));
        assert!(tag_names.contains(&"recipes/italian"));

        // Explicit count assertions for each expected tag.
        let recipes_tc = tags.iter().find(|t| t.tag == "recipes").unwrap();
        assert_eq!(recipes_tc.count, 1, "recipes should have count 1");
        let italian_tc = tags.iter().find(|t| t.tag == "recipes/italian").unwrap();
        assert_eq!(italian_tc.count, 1, "recipes/italian should have count 1");
    }

    #[test]
    fn list_tags_propagates_invalid_prefix() {
        let (_tmp, store) = test_store();
        let err = store.list_tags(Some("bad tag")).unwrap_err();
        assert!(matches!(err, MemoryError::InvalidTag(_)));
    }

    // -----------------------------------------------------------------------
    // FTS5 search tests
    // -----------------------------------------------------------------------

    #[test]
    fn search_finds_stemmed_forms() {
        let (_tmp, store) = test_store();
        store
            .create_note("Park note", "running through the park", &[])
            .unwrap();
        // "run" should match "running" via the porter stemmer.
        let results = store.search_notes("run", 10, None, false).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Park note");
    }

    #[test]
    fn search_phrase_query() {
        let (_tmp, store) = test_store();
        store
            .create_note("A", "oauth refresh token flow", &[])
            .unwrap();
        store
            .create_note("B", "refresh oauth process", &[])
            .unwrap();
        // Phrase query — only the first note has "oauth" immediately followed by "refresh".
        let results = store
            .search_notes("\"oauth refresh\"", 10, None, false)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "A");
    }

    #[test]
    fn search_boolean_query() {
        let (_tmp, store) = test_store();
        store
            .create_note("Both", "oauth and token present", &[])
            .unwrap();
        store
            .create_note("Only oauth", "oauth only here", &[])
            .unwrap();
        store
            .create_note("Only token", "token only here", &[])
            .unwrap();
        // Boolean AND — only note with both terms should be returned.
        let results = store
            .search_notes("oauth AND token", 10, None, false)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Both");
    }

    #[test]
    fn search_excludes_soft_deleted() {
        let (_tmp, store) = test_store();
        let note = store
            .create_note("Deletable", "unique content xyzzy", &[])
            .unwrap();
        let results = store.search_notes("xyzzy", 10, None, false).unwrap();
        assert_eq!(results.len(), 1);

        store.forget_note(&note.id).unwrap();
        let results = store.search_notes("xyzzy", 10, None, false).unwrap();
        assert!(results.is_empty(), "soft-deleted note must not appear in search");
    }

    #[test]
    fn search_with_prefix_tag_filter() {
        let (_tmp, store) = test_store();
        store
            .create_note("Italian", "pasta carbonara recipe", &["recipes/italian".into()])
            .unwrap();
        store
            .create_note("French", "ratatouille recipe", &["recipes/french".into()])
            .unwrap();
        store
            .create_note("Other", "recipe unrelated", &["other".into()])
            .unwrap();

        // Prefix match on "recipes" should return both Italian and French.
        let results = store
            .search_notes("recipe", 10, Some("recipes"), false)
            .unwrap();
        let titles: Vec<&str> = results.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(results.len(), 2);
        assert!(titles.contains(&"Italian"));
        assert!(titles.contains(&"French"));

        // Narrower prefix — only Italian.
        let results = store
            .search_notes("recipe", 10, Some("recipes/italian"), false)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Italian");
    }

    #[test]
    fn search_with_exact_tag_filter() {
        let (_tmp, store) = test_store();
        store
            .create_note("Italian", "pasta carbonara recipe", &["recipes/italian".into()])
            .unwrap();
        store
            .create_note("French", "ratatouille recipe", &["recipes/french".into()])
            .unwrap();

        // Exact match on parent tag must NOT pull in children.
        let results = store
            .search_notes("recipe", 10, Some("recipes"), true)
            .unwrap();
        assert!(
            results.is_empty(),
            "exact 'recipes' must not match 'recipes/italian' or 'recipes/french'"
        );

        // Exact match on a leaf tag works.
        let results = store
            .search_notes("recipe", 10, Some("recipes/italian"), true)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Italian");
    }

    #[test]
    fn search_limit_honored() {
        let (_tmp, store) = test_store();
        for i in 0..5 {
            store
                .create_note(&format!("Note {i}"), "common keyword here", &[])
                .unwrap();
        }
        let results = store.search_notes("common", 3, None, false).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn search_falls_back_to_phrase_on_malformed_query() {
        let (_tmp, store) = test_store();
        // A colon in the query triggers an FTS5 parse error.
        store
            .create_note("Colon note", "foo:bar baz content", &[])
            .unwrap();
        // This must not error — it should fall back to a phrase search.
        let results = store.search_notes("foo:bar", 10, None, false).unwrap();
        assert_eq!(
            results.len(),
            1,
            "phrase fallback must find the note containing 'foo:bar'"
        );
        assert_eq!(results[0].title, "Colon note");
    }

    #[test]
    fn search_propagates_invalid_tag_filter() {
        let (_tmp, store) = test_store();
        let err = store
            .search_notes("anything", 10, Some("bad tag"), false)
            .unwrap_err();
        assert!(matches!(err, MemoryError::InvalidTag(_)));
    }

    #[test]
    fn search_returns_empty_for_empty_query() {
        // Empty and whitespace-only queries must short-circuit at the entry
        // and return Ok(empty) — never reach the FTS5 layer where they could
        // synthesize a match-all phrase.
        let (_tmp, store) = test_store();
        store
            .create_note("Real note", "some real content", &[])
            .unwrap();

        // Empty string.
        let r = store.search_notes("", 10, None, false).unwrap();
        assert!(r.is_empty(), "empty query must return no results");

        // Whitespace only — spaces, tabs, newlines.
        let r = store.search_notes("   ", 10, None, false).unwrap();
        assert!(r.is_empty(), "all-spaces query must return no results");

        let r = store.search_notes("\t\n", 10, None, false).unwrap();
        assert!(r.is_empty(), "tab+newline query must return no results");

        // The mock impl must agree (parity).
        let mock = MockMemory::new();
        mock.create_note("Mock", "content", &[]).unwrap();
        let r = mock.search_notes("", 10, None, false).unwrap();
        assert!(r.is_empty(), "mock empty query must also return no results");
        let r = mock.search_notes("   ", 10, None, false).unwrap();
        assert!(r.is_empty(), "mock whitespace query must also return no results");
    }

    #[test]
    fn search_reflects_insert_update_delete() {
        let (_tmp, store) = test_store();
        let note = store
            .create_note("Trigger test", "unique phrase qwerty", &[])
            .unwrap();

        // Should be found after insert (notes_ai trigger).
        let results = store.search_notes("qwerty", 10, None, false).unwrap();
        assert_eq!(results.len(), 1);

        // Update content so the term no longer matches (notes_au trigger).
        store.update_note(&note.id, "completely different content").unwrap();
        let results = store.search_notes("qwerty", 10, None, false).unwrap();
        assert!(
            results.is_empty(),
            "FTS index must reflect the content update"
        );

        // The new term should now be findable.
        let results = store.search_notes("completely", 10, None, false).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn fts_index_backfilled_on_first_open() {
        // Simulate a legacy database that has notes but no FTS table/triggers.
        // Opening via SqliteMemory::new should backfill the index.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE TABLE tags (
                note_id TEXT NOT NULL REFERENCES notes(id),
                tag TEXT NOT NULL,
                PRIMARY KEY (note_id, tag)
            );
            CREATE TABLE links (
                from_id TEXT NOT NULL REFERENCES notes(id),
                to_id TEXT NOT NULL REFERENCES notes(id),
                relation TEXT NOT NULL,
                PRIMARY KEY (from_id, to_id)
            );
            INSERT INTO notes (id, title, content, created_at, updated_at)
            VALUES ('legacy_note_1', 'Legacy Title', 'legacy content zulu', '2025-01-15T12:00:00.000Z', '2025-01-15T12:00:00.000Z');",
        )
        .unwrap();

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# core").unwrap();
        let store = SqliteMemory::new(
            Arc::new(Mutex::new(conn)),
            tmp.path().to_path_buf(),
        )
        .unwrap();

        // The backfill must have indexed the pre-existing note.
        let results = store.search_notes("zulu", 10, None, false).unwrap();
        assert_eq!(
            results.len(),
            1,
            "FTS backfill must index pre-existing notes on first open"
        );
        assert_eq!(results[0].title, "Legacy Title");
    }

    // -----------------------------------------------------------------------
    // MockMemory search test
    // -----------------------------------------------------------------------

    #[test]
    fn mock_memory_search_with_tag_filter() {
        let memory: Arc<dyn Memory> = Arc::new(MockMemory::new());

        memory
            .create_note("Italian recipe", "pasta carbonara", &["recipes/italian".into()])
            .unwrap();
        memory
            .create_note("French recipe", "ratatouille", &["recipes/french".into()])
            .unwrap();
        memory
            .create_note("Unrelated", "something else entirely", &["other".into()])
            .unwrap();

        // Substring match only (no tag filter).
        let results = memory.search_notes("recipe", 10, None, false).unwrap();
        assert_eq!(results.len(), 2);

        // Prefix tag filter: "recipes" matches both recipe notes.
        let results = memory
            .search_notes("recipe", 10, Some("recipes"), false)
            .unwrap();
        assert_eq!(results.len(), 2);

        // Narrower prefix filter: only Italian.
        let results = memory
            .search_notes("recipe", 10, Some("recipes/italian"), false)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Italian recipe");

        // Exact tag filter on parent must NOT pull in children.
        let results = memory
            .search_notes("recipe", 10, Some("recipes"), true)
            .unwrap();
        assert!(results.is_empty());

        // Invalid tag filter propagates error.
        let err = memory
            .search_notes("recipe", 10, Some("bad!tag"), false)
            .unwrap_err();
        assert!(matches!(err, MemoryError::InvalidTag(_)));
    }

    // -----------------------------------------------------------------------
    // MockMemory new-method test
    // -----------------------------------------------------------------------

    #[test]
    fn mock_memory_paginated_and_tags_via_arc_dyn() {
        let memory: Arc<dyn Memory> = Arc::new(MockMemory::new());

        memory
            .create_note("A", "Ca", &["food/italian".into()])
            .unwrap();
        memory
            .create_note("B", "Cb", &["food/french".into()])
            .unwrap();
        memory
            .create_note("C", "Cc", &["food/italian".into()])
            .unwrap();

        // Pagination — all notes, page 0.
        let page = memory
            .list_notes_paginated(None, false, 10, 0)
            .unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.notes.len(), 3);

        // Pagination — limit 1.
        let p1 = memory
            .list_notes_paginated(None, false, 1, 0)
            .unwrap();
        assert_eq!(p1.total, 3);
        assert_eq!(p1.notes.len(), 1);

        // Pagination — offset 1 skips first note and forwards offset via dyn dispatch.
        let p_offset = memory
            .list_notes_paginated(None, false, 10, 1)
            .unwrap();
        assert_eq!(p_offset.total, 3);
        assert_eq!(p_offset.notes.len(), 2, "offset=1 should return 2 of 3 notes");
        // The first note (mock_0) must not appear on this page.
        assert!(
            !p_offset.notes.iter().any(|n| n.id == "mock_0"),
            "offset=1 should skip mock_0"
        );

        // Prefix filter: "food" matches all three.
        let pfood = memory
            .list_notes_paginated(Some("food"), false, 10, 0)
            .unwrap();
        assert_eq!(pfood.total, 3);

        // Prefix filter: "food/italian" matches 2.
        let pita = memory
            .list_notes_paginated(Some("food/italian"), false, 10, 0)
            .unwrap();
        assert_eq!(pita.total, 2);

        // Exact filter: "food/italian" matches 2.
        let exact = memory
            .list_notes_paginated(Some("food/italian"), true, 10, 0)
            .unwrap();
        assert_eq!(exact.total, 2);

        // Exact filter on parent: "food" must NOT pull in child-only tagged notes.
        // None of the seeded notes carries an exact "food" tag, so total must be 0.
        let exact_parent = memory
            .list_notes_paginated(Some("food"), true, 10, 0)
            .unwrap();
        assert_eq!(
            exact_parent.total, 0,
            "exact `food` must not match notes tagged only `food/italian` or `food/french`"
        );
        assert!(exact_parent.notes.is_empty());

        // list_tags — no prefix.
        let tags = memory.list_tags(None).unwrap();
        // "food/italian" has count 2, "food/french" has count 1.
        assert_eq!(tags[0].tag, "food/italian");
        assert_eq!(tags[0].count, 2);
        assert_eq!(tags[1].tag, "food/french");
        assert_eq!(tags[1].count, 1);

        // list_tags — with prefix.
        let food_tags = memory.list_tags(Some("food")).unwrap();
        assert_eq!(food_tags.len(), 2);

        // Invalid prefix.
        let err = memory.list_tags(Some("bad!tag")).unwrap_err();
        assert!(matches!(err, MemoryError::InvalidTag(_)));
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

        let found = memory.search_notes("New", 10, None, false).unwrap();
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

    // -----------------------------------------------------------------------
    // T1. Additional phrase fallback cases
    // -----------------------------------------------------------------------

    #[test]
    fn search_fallback_unmatched_quote() {
        let (_tmp, store) = test_store();
        store
            .create_note("Quote note", "unclosed phrase content here", &[])
            .unwrap();
        // An unclosed quote triggers an FTS5 parse error; fallback must find the note.
        let results = store
            .search_notes("unclosed \"phrase", 10, None, false)
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "phrase fallback must find note for unmatched-quote query"
        );
        assert_eq!(results[0].title, "Quote note");
    }

    #[test]
    fn search_fallback_trailing_boolean_operator() {
        let (_tmp, store) = test_store();
        store
            .create_note("Trailing AND note", "rust AND trailing content", &[])
            .unwrap();
        // A trailing boolean operator is a parse error; fallback should find the note.
        let results = store.search_notes("rust AND", 10, None, false).unwrap();
        assert_eq!(
            results.len(),
            1,
            "phrase fallback must find note for trailing-AND query"
        );
        assert_eq!(results[0].title, "Trailing AND note");
    }

    #[test]
    fn search_fallback_bare_not_operator() {
        let (_tmp, store) = test_store();
        store
            .create_note("NOT alone note", "NOT alone bare content", &[])
            .unwrap();
        // A bare unary NOT without an operand is a parse error; fallback should fire.
        let results = store.search_notes("NOT alone", 10, None, false).unwrap();
        assert_eq!(
            results.len(),
            1,
            "phrase fallback must find note for bare-NOT query"
        );
        assert_eq!(results[0].title, "NOT alone note");
    }

    // -----------------------------------------------------------------------
    // T2. bm25 ordering verification
    // -----------------------------------------------------------------------

    #[test]
    fn search_bm25_ordering() {
        let (_tmp, store) = test_store();
        // Note A has "rust" three times in the title — higher term frequency.
        store
            .create_note("Rust Rust Rust", "about programming", &[])
            .unwrap();
        // Note B has one mention in content only.
        store
            .create_note("Programming", "a single mention of rust", &[])
            .unwrap();

        let results = store.search_notes("rust", 10, None, false).unwrap();
        assert_eq!(results.len(), 2, "both notes should be returned");
        assert_eq!(
            results[0].title, "Rust Rust Rust",
            "the note with higher rust term frequency must rank first"
        );
    }

    // -----------------------------------------------------------------------
    // T3. Overfetch under tag-filter pressure
    // -----------------------------------------------------------------------

    #[test]
    fn search_returns_tagged_results_when_untagged_dominate_bm25() {
        let (_tmp, store) = test_store();

        // 200 untagged notes whose content repeats the keyword — high term
        // frequency, so they rank above the tagged notes in bm25.
        for i in 0..200 {
            store
                .create_note(
                    &format!("Loud {i}"),
                    "common common common common keyword",
                    &["other".to_string()],
                )
                .unwrap();
        }

        // 5 tagged notes that mention the keyword only once — low bm25, but
        // they're the only matches that satisfy the tag filter.
        let mut tagged_titles = Vec::new();
        for i in 0..5 {
            let title = format!("Quiet {i}");
            tagged_titles.push(title.clone());
            store
                .create_note(&title, "common", &["target/exact".to_string()])
                .unwrap();
        }

        // With limit=5 and tag prefix "target", the rowid subquery restricts
        // bm25 ranking to the 5 tagged notes — all five must come back.  Under
        // the pre-fix code that fetched the top (limit*10).max(50)=50 rowids
        // globally and post-filtered, none of the tagged notes would survive
        // (they all rank below the 200 loud ones).
        let results = store
            .search_notes("common", 5, Some("target"), false)
            .unwrap();
        assert_eq!(
            results.len(),
            5,
            "all five tagged notes must be returned even though untagged notes rank higher in bm25"
        );
        let result_titles: Vec<&str> = results.iter().map(|n| n.title.as_str()).collect();
        for expected in &tagged_titles {
            assert!(
                result_titles.contains(&expected.as_str()),
                "expected tagged note '{expected}' not found in results: {result_titles:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // T4. Soft-delete removes from FTS index (new trigger)
    // -----------------------------------------------------------------------

    #[test]
    fn soft_delete_removes_from_fts_index() {
        let (_tmp, store) = test_store();
        let note = store
            .create_note("FTS trigger test", "unique_fts_term_xyzzy", &[])
            .unwrap();

        // Note: porter stemmer may or may not match the underscored form with a hyphen
        // variant; we verify the trigger via direct SQL below rather than search count.

        // Soft-delete the note.
        store.forget_note(&note.id).unwrap();

        // After soft-delete, searching should not return the note.
        let results = store
            .search_notes("unique_fts_term_xyzzy", 10, None, false)
            .unwrap();
        assert!(
            results.is_empty(),
            "soft-deleted note must not appear in search"
        );

        // Verify directly: the FTS row must be gone.
        let conn = store.lock_conn();
        let fts_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes_fts WHERE rowid = (SELECT rowid FROM notes WHERE id = ?1)",
                params![note.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            fts_count, 0,
            "soft-delete should remove the note from notes_fts"
        );
    }

    // -----------------------------------------------------------------------
    // T5. Backfill excludes soft-deleted notes
    // -----------------------------------------------------------------------

    #[test]
    fn fts_backfill_excludes_soft_deleted() {
        // Simulate a legacy database with a live note AND a soft-deleted note.
        // The backfill (fix #5) must only index the live one.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE TABLE tags (
                note_id TEXT NOT NULL REFERENCES notes(id),
                tag TEXT NOT NULL,
                PRIMARY KEY (note_id, tag)
            );
            CREATE TABLE links (
                from_id TEXT NOT NULL REFERENCES notes(id),
                to_id TEXT NOT NULL REFERENCES notes(id),
                relation TEXT NOT NULL,
                PRIMARY KEY (from_id, to_id)
            );
            INSERT INTO notes (id, title, content, created_at, updated_at, deleted_at)
            VALUES
                ('live_1', 'Live Title', 'live content alpha', '2025-01-15T12:00:00.000Z', '2025-01-15T12:00:00.000Z', NULL),
                ('dead_1', 'Deleted Title', 'deleted content beta', '2025-01-15T12:00:00.000Z', '2025-01-15T12:00:01.000Z', '2025-01-15T12:00:02.000Z');",
        )
        .unwrap();

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# core").unwrap();
        let store = SqliteMemory::new(Arc::new(Mutex::new(conn)), tmp.path().to_path_buf()).unwrap();

        // The live note must be indexed.
        let live_results = store.search_notes("alpha", 10, None, false).unwrap();
        assert_eq!(
            live_results.len(),
            1,
            "live note must be found after backfill"
        );
        assert_eq!(live_results[0].title, "Live Title");

        // The soft-deleted note must NOT be indexed.
        let dead_results = store.search_notes("beta", 10, None, false).unwrap();
        assert!(
            dead_results.is_empty(),
            "soft-deleted note must not be backfilled into the FTS index"
        );
    }

    // -----------------------------------------------------------------------
    // T8. FTS5 prefix query syntax
    // -----------------------------------------------------------------------

    #[test]
    fn search_fts5_prefix_syntax() {
        let (_tmp, store) = test_store();
        store
            .create_note("Auth note", "authentication flow", &[])
            .unwrap();
        store
            .create_note("Authz note", "authorize access grant", &[])
            .unwrap();
        store
            .create_note("Unrelated", "completely different content", &[])
            .unwrap();

        // FTS5 prefix query — `auth*` must match "authentication" and "authorize".
        let results = store.search_notes("auth*", 10, None, false).unwrap();
        assert_eq!(
            results.len(),
            2,
            "prefix query auth* must match both 'authentication' and 'authorize'"
        );
        let titles: Vec<&str> = results.iter().map(|n| n.title.as_str()).collect();
        assert!(titles.contains(&"Auth note"));
        assert!(titles.contains(&"Authz note"));
    }
}
