# Feature: Aurora Memory Enhancements

## Summary

Four improvements to Aurora's memory system, driven by Aurora's own feedback on the design doc. Each addresses a concrete limitation she identified from her experience with the Letta memory system.

## Requirements

- REQ-1: **Diff-based memory editing** — **Create** a new `memory_edit(id, old_text, new_text)` tool that performs find-and-replace on note content (including core.md). Prevents hallucination drift when Aurora only needs to change one line — she doesn't have to rewrite the entire note. Empty `old_text` = append. Empty `new_text` = delete the matched text. First occurrence only. `memory_update` remains available for full rewrites. **Change** Pipeline to re-read core.md on each message instead of caching at startup, so edits to core.md take effect immediately.

- REQ-2: **Working memory (pin/unpin)** — **Create** `memory_pin(id)` and `memory_unpin(id)` tools that attach/detach notes to the current conversation's context. Pinned notes are injected into the system prompt (after core.md, before history) for the duration of the conversation. **Create** a new `pinned_notes` table for per-conversation storage. Pinned notes count toward the context budget — enforced at prompt assembly time (not pin-time, since tools lack budget access). Max 10 pinned notes per conversation. **Modify** `memory_forget` to auto-unpin forgotten notes. Pinned notes always reflect current content (not snapshots). Conversations are per-channel on Discord, per-user for DMs.

- REQ-3: **Link metadata in memory_read** — Verify `memory_read` returns links end-to-end with direction and relation in the JSON output. Fix if not working. (Existing code already populates `Note.links` — this is a verification task, not new development.)

- REQ-4: **FTS5 full-text search** — **Replace** LIKE-based `search_notes` with FTS5. **Create** `notes_fts` virtual table mirroring `notes.title` and `notes.content`. Search uses BM25 ranking. **Create** triggers on `notes` to keep the FTS index in sync (INSERT, UPDATE including soft-deletes, DELETE). **Add** input sanitisation for FTS5 queries. **Modify** rusqlite dependency to enable FTS5 compilation. Rebuild index on first run for existing data.

## Acceptance Criteria

- [ ] AC-1: `memory_edit("core", "I like cats", "I like cats and dogs")` modifies only the matched text in core.md. The rest of the file is untouched. (REQ-1)
- [ ] AC-2: `memory_edit("core", "", "\n\nI enjoy painting.")` appends to core.md without altering existing content. (REQ-1)
- [ ] AC-3: `memory_edit("note_abc", "old section", "")` deletes the matched text from the note. (REQ-1)
- [ ] AC-4: `memory_edit("core", "nonexistent text", "replacement")` returns an error — old_text not found. (REQ-1)
- [ ] AC-5: After `memory_edit("core", ...)`, the next LLM call uses the updated core persona content (pipeline re-reads from disk, not cached). (REQ-1)
- [ ] AC-6: `memory_pin("note_abc123")` causes that note's content to appear in the system prompt for all subsequent messages in the same conversation. `memory_unpin("note_abc123")` removes it. Pinned notes survive across messages but not across conversations. (REQ-2)
- [ ] AC-7: Pinned notes count toward the context budget. If pinned notes exceed the available budget at assembly time, the oldest-pinned are evicted with a warning log. (REQ-2)
- [ ] AC-8: `memory_forget("note_abc123")` on a pinned note auto-unpins it. (REQ-2)
- [ ] AC-9: Max 10 pinned notes per conversation. Pinning an 11th returns an error. (REQ-2)
- [ ] AC-10: `memory_read("note_abc123")` returns note content AND links (both incoming and outgoing) with relation names and direction. (REQ-3)
- [ ] AC-11: `memory_search("painting")` returns results ranked by BM25 relevance via FTS5. A note containing "I enjoy painting landscapes" is found even when searching for "paint" (prefix matching via `paint*` works). (REQ-4)
- [ ] AC-12: Creating, updating, editing, or forgetting a note automatically updates the FTS5 index. No manual sync needed. (REQ-4)
- [ ] AC-13: Search input containing FTS5 special characters (quotes, `*`, `AND`, `OR`) is sanitised and does not cause query errors. (REQ-4)

## Phases

### Phase 1: memory_edit + link metadata verification (REQ-1, REQ-3)
Small, independent changes. Create `memory_edit` tool. Verify `memory_read` link output.

### Phase 1.5 (prerequisite for Phase 2): FTS5 feature verification
**Before** Phase 2 starts: verify FTS5 is available in rusqlite by testing in a minimal project. Change `Cargo.toml` feature flags as needed. Confirm `CREATE VIRTUAL TABLE ... USING fts5(...)` works at runtime. This gates Phase 2.

### Phase 2 & 3 (parallel, after Phase 1): FTS5 search + Working memory pin/unpin
These are independent and can run in parallel.
- **Phase 2: FTS5 search (REQ-4)** — Schema addition, trigger-based sync, query sanitisation.
- **Phase 3: Working memory pin/unpin (REQ-2)** — New table, new tools, pipeline modification, budget accounting.

## Implementation Notes

All notes below describe **changes to make** to the existing codebase. The current code does not have these features.

### REQ-1: memory_edit

**Create:**
- New `MemoryEdit` tool struct in `src/tools/memory_tools.rs`
- Parameters: `id`, `old_text`, `new_text`
- For regular notes: load content via `read_note`, find `old_text` via `str::find` (raw substring match), replace with `new_text`, save via `update_note`
- For `id="core"`: same flow but via existing `load_core_persona` / `update_core` path
- If `old_text` is empty: append `new_text` to the end of the content
- If `new_text` is empty: delete the matched `old_text` from the content
- If `old_text` not found: return tool error "text not found in note"
- If `old_text` appears multiple times: replace only the first occurrence
- Register in `ToolGroup::Memory`

**Modify:**
- `src/main.rs`: add `"memory_edit"` to the restricted tools list (alongside `memory_create`, `memory_update`, etc.)
- `src/core/pipeline.rs`: Pipeline currently loads `core.md` once at startup into `self.core_persona` (line 112-122) and never re-reads it. **Change** `process_impl` to call `self.memory_store.load_core_persona()` on each invocation instead of using the cached `self.core_persona` field. This ensures edits to core.md take effect on the next message. The file I/O cost is negligible compared to LLM API call latency. The `core_persona` field on the Pipeline struct can be removed or kept as a fallback.

**Concurrency:** Safe — all DB ops and file I/O are serialized via `Arc<Mutex<Connection>>` and `spawn_blocking`.

### REQ-2: Working memory

**Create:**
- New SQLite table in `src/memory/store.rs` `init_schema()`:
  ```sql
  CREATE TABLE IF NOT EXISTS pinned_notes (
      conversation_id TEXT NOT NULL,
      note_id TEXT NOT NULL,
      pinned_at TEXT NOT NULL,
      PRIMARY KEY (conversation_id, note_id)
  )
  ```
- New `Memory` trait methods in `src/memory/store.rs`:
  - `pin_note(conv_id: &str, note_id: &str) -> MemoryResult<()>`
  - `unpin_note(conv_id: &str, note_id: &str) -> MemoryResult<()>`
  - `get_pinned_notes(conv_id: &str) -> MemoryResult<Vec<(Note, String)>>` — returns notes with `pinned_at` timestamps (needed for eviction ordering)
- Implement these in `SqliteMemory`. `pin_note` checks count < 10 before inserting.
- New `MemoryPin` and `MemoryUnpin` tool structs in `src/tools/memory_tools.rs`. `conversation_id` comes from `ToolContext`.

**Modify:**
- `src/memory/store.rs` `forget_note()`: after soft-deleting, **add** `DELETE FROM pinned_notes WHERE note_id = ?` to auto-unpin from all conversations.
- `src/history/budget.rs` `assemble()`: **add** `pinned_notes: &[Note]` parameter. Inject pinned notes as a labeled section (`## Pinned Notes\n\n### {title}\n{content}`) into the system message, after core persona. No eviction logic here — `assemble()` receives only budget-safe notes. Update all existing `assemble()` call sites to pass the new parameter (pass empty slice `&[]` for calls that don't have pinned context, e.g. compaction, 400-recovery fallback).
- `src/core/pipeline.rs` `process_impl()`: **add** pinned notes loading and budget enforcement **before** turn selection:
  1. Call `self.memory_store.get_pinned_notes(&conv_id)` via `spawn_blocking`
  2. Estimate tokens for each pinned note
  3. If total pinned tokens exceed available budget (after subtracting system prompt, core persona, tool defs, response reserve), evict oldest-pinned (by `pinned_at` timestamp) until they fit. Log `warn!` for each evicted note.
  4. Subtract remaining pinned tokens from available budget before calling `select_turns()`
  5. Pass only budget-safe pinned notes to `assemble()`
  - Pinned notes have higher priority than old history turns but lower than the most recent turn
- `src/main.rs`: **add** `"memory_pin"` and `"memory_unpin"` to the restricted tools list.

### REQ-3: Link metadata

**Verify** (not create):
- `src/memory/store.rs` `read_note()` already calls `get_links_for_note_locked()` and populates `Note.links` with direction and relation fields.
- `src/tools/memory_tools.rs` `MemoryRead` serialises the `Note` struct via `serde_json::to_value`, which should include links.
- **Add** a test in `tests/memory_test.rs` that calls `memory_read` on a note with links and asserts the JSON output contains `links` with `direction` and `relation` fields.

### REQ-4: FTS5

**Modify:**
- `Cargo.toml`: **change** rusqlite features. The `bundled` feature compiles SQLite from source but does not enable FTS5 by default. **Verify** the correct approach before implementation — likely need to add `"bundled-full"` or set `SQLITE_ENABLE_FTS5` via a build script. **Test** FTS5 availability in a minimal project before committing to the approach.

**Create:**
- FTS5 virtual table in `src/memory/store.rs` `init_schema()`:
  ```sql
  CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(title, content, content=notes, content_rowid=rowid)
  ```
  Note: this must be a separate `conn.execute()` call, not part of `execute_batch()` (virtual tables may not work in batch mode — **verify**).
- **Create** triggers in `init_schema()` (valid SQLite syntax):
  ```sql
  CREATE TRIGGER IF NOT EXISTS notes_fts_insert AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
  END;

  CREATE TRIGGER IF NOT EXISTS notes_fts_update AFTER UPDATE ON notes BEGIN
    DELETE FROM notes_fts WHERE rowid = old.rowid;
    INSERT INTO notes_fts(rowid, title, content)
      SELECT new.rowid, new.title, new.content WHERE new.deleted_at IS NULL;
  END;

  CREATE TRIGGER IF NOT EXISTS notes_fts_delete AFTER DELETE ON notes BEGIN
    DELETE FROM notes_fts WHERE rowid = old.rowid;
  END;
  ```
  The UPDATE trigger handles both content changes and soft-deletes: it always removes the old entry, then only re-inserts if `deleted_at IS NULL`.
- Populate existing notes on first run: `INSERT INTO notes_fts(notes_fts) VALUES('rebuild')` — runs after table creation. `CREATE IF NOT EXISTS` ensures this only happens once.

**Modify:**
- `src/memory/store.rs` `search_notes()`: **replace** LIKE-based query with FTS5 query:
  ```sql
  SELECT n.id, n.title, n.content, n.created_at, n.updated_at
  FROM notes n
  JOIN notes_fts f ON n.rowid = f.rowid
  WHERE n.deleted_at IS NULL AND notes_fts MATCH ?
  ORDER BY f.rank
  ```
- **Add** query sanitisation function: strip all characters except alphanumeric, spaces, and underscores from user input before passing to FTS5 MATCH. This prevents FTS5 syntax injection (operators like `AND`, `OR`, `NOT`, `*`, `-`, quotes, parentheses are stripped). Prefix matching is applied programmatically by appending `*` to the sanitised query.
