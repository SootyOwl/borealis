use rusqlite::{Connection, Result};

/// Initialize the database schema.
///
/// Sets recommended PRAGMAs and creates tables + indexes if they do not
/// already exist. Safe to call multiple times (idempotent).
pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA busy_timeout = 5000;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS conversations (
            id              TEXT PRIMARY KEY,
            mode            TEXT NOT NULL,
            created_at      TEXT NOT NULL,
            last_active_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS messages (
            id              TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL REFERENCES conversations(id),
            turn_id         TEXT NOT NULL,
            seq             INTEGER NOT NULL,
            role            TEXT NOT NULL,
            content         TEXT NOT NULL,
            tool_call_id    TEXT,
            tool_calls      TEXT,
            token_estimate  INTEGER NOT NULL,
            created_at      TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_messages_conv_seq
            ON messages(conversation_id, seq);

        CREATE INDEX IF NOT EXISTS idx_messages_conv_turn
            ON messages(conversation_id, turn_id, seq);

        CREATE TABLE IF NOT EXISTS conversation_summaries (
            conversation_id TEXT PRIMARY KEY REFERENCES conversations(id),
            summary_text    TEXT NOT NULL,
            compacted_up_to INTEGER NOT NULL,
            token_estimate  INTEGER NOT NULL,
            created_at      TEXT NOT NULL
        );
        ",
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_memory_db() -> Connection {
        Connection::open_in_memory().expect("failed to open in-memory database")
    }

    #[test]
    fn schema_initializes_on_fresh_db() {
        let conn = open_memory_db();
        initialize(&conn).expect("initialize should succeed on a fresh db");

        // Verify conversations table exists by querying it.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
            .expect("conversations table should exist");
        assert_eq!(count, 0);

        // Verify messages table exists by querying it.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .expect("messages table should exist");
        assert_eq!(count, 0);
    }

    #[test]
    fn schema_is_idempotent() {
        let conn = open_memory_db();
        initialize(&conn).expect("first initialize should succeed");
        initialize(&conn).expect("second initialize should also succeed without error");
    }
}
