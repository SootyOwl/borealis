use borealis::memory::{Memory, SqliteMemory};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

#[test]
fn pagination_offset_beyond_signed_sqlite_range_returns_empty() {
    let store = SqliteMemory::new(
        Arc::new(Mutex::new(Connection::open_in_memory().unwrap())),
        std::path::PathBuf::from("unused-core-persona"),
    )
    .unwrap();
    store
        .create_note("Seed", "content", &["topic".into(), "topic/child".into()])
        .unwrap();
    // The public storage API accepts usize, and must not wrap it into a
    // negative SQLite OFFSET (which SQLite interprets as zero).
    for (tag, exact) in [(None, false), (Some("topic"), true), (Some("topic"), false)] {
        let page = store
            .list_notes_paginated(tag, exact, 10, usize::MAX)
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.offset, usize::MAX);
        assert!(
            page.notes.is_empty(),
            "offset beyond total must return an empty page: tag={tag:?}, exact={exact}"
        );
    }
}
