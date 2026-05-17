# Memory: Pagination, Nested Tags, FTS5 Search

**Date:** 2026-04-07
**Status:** Approved
**Supersedes:** [2026-04-06-memory-pagination-tags-design.md](2026-04-06-memory-pagination-tags-design.md)
**Motivation:** Aurora needs to (a) browse her imported Letta archive without nuking her context, (b) keep tags consistent as the corpus grows, and (c) actually find things. The current `LIKE`-based `memory_search` and unpaginated `memory_list` don't scale past a few hundred notes. Inspired by mempalace's finding that structural scoping alone yields a +34% retrieval boost — we get the same benefit without adopting their wing/hall/room vocabulary, just by treating tags as Obsidian-style paths.

This spec lands three changes together because they share trait signatures, tool params, and tests:

1. **Pagination + summary projection** on `memory_list`, plus a new `memory_tags` tool.
2. **Nested tag namespaces** (`recipes/italian/carbonara`) with prefix-by-default semantics.
3. **FTS5 full-text search** replacing the `LIKE` path in `memory_search`.

## Design principles

- **No tag schema migration.** Hierarchy is a *convention* over the existing `tags.tag TEXT` column. Slash-delimited, lowercase, no leading/trailing/empty segments.
- **Prefix is the primary tag query mode.** `recipes` matches `recipes`, `recipes/italian`, `recipes/italian/carbonara`. Exact-match is an opt-in escape hatch.
- **FTS5 is mandatory, not bolted-on.** `search_notes` is rewritten to use it; the old `LIKE` path is removed.
- **LLM-friendly responses.** Flat lists with counts beat nested trees. Summaries (no content/links) for browsing. Aurora drills down with a follow-up call.

---

## Part 1 — Pagination & summary projection

### `memory_list` — paginated, summary-only

**Current:** returns all matching `Note` objects (full content + links), optionally filtered by tag. No pagination.

**New:** returns paginated `NoteSummary` objects (no content, no links) wrapped in a `PaginatedNotes` envelope.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `tag` | string | no | — | Filter by tag (prefix-match by default — see Part 2) |
| `tag_exact` | bool | no | false | If true, `tag` must match exactly |
| `limit` | integer | no | 20 | Max notes per page |
| `offset` | integer | no | 0 | Number of notes to skip |

**Response shape:**

```json
{
  "notes": [
    {
      "id": "note_a1b2c3d4",
      "title": "Letta archival #passage-abc",
      "tags": ["letta-archival"],
      "created_at": "2025-01-15T12:00:00Z",
      "updated_at": "2025-01-15T12:00:00Z"
    }
  ],
  "total": 247,
  "offset": 0,
  "limit": 20
}
```

Use `memory_read(id)` to fetch full content for a specific note.

### `memory_tags` — new tool

Lists distinct tags with usage counts, sorted by count descending.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `prefix` | string | no | — | If set, only return tags equal to `prefix` or starting with `prefix/` |

**Response shape:**

```json
[
  {"tag": "letta-archival", "count": 247},
  {"tag": "recipes/italian", "count": 12},
  {"tag": "recipes/french", "count": 8}
]
```

Only counts non-deleted notes. Counts are per-distinct-tag (no rollup) — Aurora can sum them herself if she wants a namespace total.

### Types (`src/memory/store.rs`)

```rust
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
```

### `Memory` trait additions

```rust
fn list_notes_paginated(
    &self,
    tag_filter: Option<&str>,
    tag_exact: bool,
    limit: usize,
    offset: usize,
) -> MemoryResult<PaginatedNotes>;

fn list_tags(&self, prefix: Option<&str>) -> MemoryResult<Vec<TagCount>>;
```

Existing `list_notes` stays on the trait (used by migration code) but its `tag_filter` switches to prefix semantics — see Part 2.

### `SqliteMemory` implementation

**`list_notes_paginated`:** two queries — `COUNT(*)` for total, then `SELECT id, title, created_at, updated_at FROM notes ... LIMIT ? OFFSET ?` (no content column). Tags populated per-note via the existing helper.

**`list_tags`:**
```sql
SELECT t.tag, COUNT(*) AS cnt
FROM tags t
JOIN notes n ON t.note_id = n.id
WHERE n.deleted_at IS NULL
  AND (?1 IS NULL OR t.tag = ?1 OR t.tag GLOB ?1 || '/*')
GROUP BY t.tag
ORDER BY cnt DESC;
```

GLOB (not LIKE) is used everywhere prefix tags are matched. `_` is an allowed tag character but is a LIKE wildcard, so LIKE would falsely match tags like `co_de/inner` against the prefix `code`. GLOB metacharacters (`*`, `?`, `[`) are all rejected by `normalize_tag`, so the user-supplied prefix can be safely concatenated with `/*`.

---

## Part 2 — Nested tag namespaces

### Format

- Slash-delimited path segments: `parent/child/leaf`.
- Lowercase only. Inputs are silently lowercased on write — no error.
- Segments must be non-empty (`a//b` rejected). Leading/trailing slashes stripped.
- Allowed segment characters: `[a-z0-9._-]`. Anything else → `MemoryError::InvalidTag`.
- Maximum depth: 8 segments (sanity bound).

Normalization happens in one place: a `normalize_tag(&str) -> MemoryResult<String>` helper in `src/memory/store.rs`, called from `create_note` and `tag_note` before insert.

### Query semantics

`tag_filter` arguments across the API switch from exact-match to **prefix-match by default**:

- `tag_filter = Some("recipes")` matches `recipes`, `recipes/italian`, `recipes/italian/carbonara`.
- SQL form: `tag = ?1 OR tag GLOB ?1 || '/*'` — the `|| '/*'` ensures `recipe` does *not* match `recipes`. GLOB rather than LIKE so the `_` allowed in tag segments is not interpreted as a wildcard.
- `tag_exact: true` on `list_notes_paginated` and `MemorySearch` forces exact-match.

**Behavior change called out:** this is a breaking change for any caller that relied on exact-match `list_notes(tag_filter)`. Notes with single-segment tags (`person`, `preference`) keep working unchanged because exact-match is a subset of prefix-match. Update tool descriptions accordingly so the LLM uses the new semantics correctly.

---

## Part 3 — FTS5 search

`rusqlite = "0.38"` with the `bundled` feature already includes FTS5 — no Cargo change needed.

### Schema

New virtual table, created alongside `notes`/`tags`/`links` in `SqliteMemory::new`:

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
    title,
    content,
    tokenize='porter unicode61'
);
```

Regular (full-content) FTS5 table — the indexed text is stored inside `notes_fts` itself. `porter unicode61` gives English stemming + unicode folding.

> **Deviation from original draft:** an earlier version of this spec proposed an
> external-content table (`content='notes'`). That was rejected during
> implementation because external-content tables transparently delegate
> `SELECT COUNT(*)` and `EXISTS` to the source `notes` table, which made the
> obvious `EXISTS(SELECT 1 FROM notes_fts ...)` backfill check return true even
> when the FTS index was actually empty. Storing the text in `notes_fts` is
> simpler, idempotent, and the storage overhead is acceptable at the note sizes
> we expect.

### Triggers

Sync triggers on `notes`. The update trigger is split so that re-tokenization happens only when the note is still live, and a dedicated soft-delete trigger removes the row from the FTS index when `deleted_at` transitions from NULL to non-NULL.

```sql
CREATE TRIGGER IF NOT EXISTS notes_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
END;

CREATE TRIGGER IF NOT EXISTS notes_ad AFTER DELETE ON notes BEGIN
    DELETE FROM notes_fts WHERE rowid = old.rowid;
END;

CREATE TRIGGER IF NOT EXISTS notes_au AFTER UPDATE ON notes
WHEN new.deleted_at IS NULL
BEGIN
    DELETE FROM notes_fts WHERE rowid = old.rowid;
    INSERT INTO notes_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
END;

CREATE TRIGGER IF NOT EXISTS notes_soft_delete AFTER UPDATE ON notes
WHEN new.deleted_at IS NOT NULL AND old.deleted_at IS NULL
BEGIN
    DELETE FROM notes_fts WHERE rowid = new.rowid;
END;
```

Soft-deleted notes are removed from the FTS index by `notes_soft_delete`, so they don't pollute bm25 corpus statistics with content that's no longer reachable. Hard-deletes also clear the FTS row via `notes_ad`.

### Backfill

On startup, after creating the virtual table and triggers, check whether the index is populated:

```sql
SELECT EXISTS(SELECT 1 FROM notes_fts LIMIT 1);
```

If the index is empty and `notes` is non-empty, populate it from the live notes (excluding soft-deleted rows so they don't get re-indexed):

```sql
INSERT INTO notes_fts(rowid, title, content)
SELECT rowid, title, content FROM notes WHERE deleted_at IS NULL;
```

Idempotent and cheap on subsequent boots.

### `search_notes` rewrite

```rust
fn search_notes(
    &self,
    query: &str,
    limit: usize,
    tag_filter: Option<&str>,
    tag_exact: bool,
) -> MemoryResult<Vec<Note>>;
```

Implementation: two-step rowid query, since `bm25(notes_fts)` requires `notes_fts` directly in `FROM` (not behind a JOIN). The soft-delete and tag filters are pushed into a `rowid IN (...)` subquery so they apply before sorting and `LIMIT` is honoured exactly.

```sql
-- No tag filter:
SELECT rowid FROM notes_fts
WHERE notes_fts MATCH ?1
  AND rowid IN (SELECT rowid FROM notes WHERE deleted_at IS NULL)
ORDER BY bm25(notes_fts) LIMIT ?2;

-- Prefix tag filter:
SELECT rowid FROM notes_fts
WHERE notes_fts MATCH ?1
  AND rowid IN (
      SELECT n.rowid FROM notes n
      JOIN tags t ON t.note_id = n.id
      WHERE n.deleted_at IS NULL
        AND (t.tag = ?2 OR t.tag GLOB ?3)
  )
ORDER BY bm25(notes_fts) LIMIT ?4;
```

(The `tag_exact: true` variant drops the `OR t.tag GLOB ?3` clause.) Each rowid is then looked up in `notes` in relevance order to hydrate the returned `Note`. `tags` and `links` are left empty on the returned `Note` to keep search responses small; callers who need them follow up with `memory_read`.

**Query passthrough.** The user's query string is passed directly to FTS5 MATCH. This gives Aurora prefix (`auth*`), phrase (`"oauth refresh"`), and boolean (`oauth AND token`) for free. If MATCH fails to parse (FTS5 syntax error on stray `:` etc.), retry once with the query wrapped in double quotes as a phrase before surfacing the error — Aurora shouldn't have to know FTS5 grammar to do a literal search.

**Sanitization in the pipeline.** Free-form user messages are sanitized via `crate::memory::sanitize_for_fts` before being handed to `search_notes` from the conversation pipeline (FTS5 metacharacters stripped, bare operator keywords dropped, length-capped). Tool-driven `memory_search` calls do *not* go through the sanitizer — Aurora-the-LLM is trusted to use FTS5 syntax intentionally.

### Tool changes

`MemorySearch` (`src/tools/memory_tools.rs`) gains optional `tag` and `tag_exact` parameters mirroring `memory_list`. Description should mention prefix semantics and FTS5 syntax support.

---

## Tool changes summary (`src/tools/memory_tools.rs`)

- **`MemoryList`:** add `limit`, `offset`, `tag_exact` params. Call `list_notes_paginated`. Returns `PaginatedNotes`.
- **`MemorySearch`:** add `tag`, `tag_exact` params. Calls the new `search_notes` signature.
- **`MemoryTags`:** new tool. Optional `prefix` param. Calls `list_tags`. Register in `register_memory_tools` (bumps tool count from 9 to 10).

## `MockMemory` updates

- `list_notes_paginated` (slice + count from in-memory vec, prefix-aware)
- `list_tags(prefix)` (count from in-memory vec, prefix-aware)
- `search_notes` with new signature: substring match on title+content, prefix match on tags
- `create_note` / `tag_note`: apply `normalize_tag`

## Removed, not deprecated

The `LIKE`-based search branch in `search_notes` is deleted. Search is read-only, no migration risk.

## Implementation order

1. `normalize_tag` helper + tests.
2. Apply normalization in `create_note` and `tag_note`. Update existing tests that pass mixed-case tags.
3. New types (`NoteSummary`, `TagCount`, `PaginatedNotes`) and trait additions.
4. `SqliteMemory::list_notes_paginated` and `list_tags(prefix)`.
5. Switch `list_notes` tag filtering to prefix semantics.
6. FTS5 virtual table, triggers, and backfill in `SqliteMemory::new`.
7. Rewrite `search_notes` against FTS5 with `tag_filter` + `tag_exact`.
8. `MockMemory` updates to match.
9. Tool wiring: `MemoryList`, `MemorySearch`, new `MemoryTags`.
10. Tests (see below).

## Tests

**Tag normalization**
- Lowercases mixed case silently.
- Strips leading/trailing slashes.
- Rejects empty segments (`a//b`), bad chars, depth > 8.

**Pagination**
- Correct slice at various offsets; total count accuracy; offset beyond range returns empty; `limit=0` edge case.

**Tag listing**
- Counts correct; excludes soft-deleted notes; sorted by count desc.
- `prefix` filter returns only matching tags; `recipe` does not match `recipes`.

**Prefix tag filter (list + search)**
- `recipes` matches `recipes/italian/carbonara`; `recipe` does not match `recipes`.
- `tag_exact: true` forces exact match.

**FTS5 search**
- Stemmed forms (`running` finds `run`).
- Phrase queries (`"oauth refresh"`).
- Boolean queries (`oauth AND token`).
- Soft-deleted notes excluded.
- `tag_filter` scopes results (both prefix and exact modes).
- `limit` honored; bm25 ordering.
- Malformed query (`foo:bar`) falls back to phrase search instead of erroring.

**FTS5 sync**
- Insert/update/delete on `notes` reflected immediately in search results.

**Backfill**
- Wiping `notes_fts` and reopening the store rebuilds the index.

**`MockMemory`**
- All new behaviors mirrored so trait-based callers stay testable.

## Out of scope

- Rollup counts in `memory_tags` (Aurora can sum).
- Tree-shaped tag responses.
- Migration of existing uppercase tags in production DBs (Aurora's store is small enough to re-tag manually if needed).
- Semantic / vector search. FTS5 + prefix scoping is enough for now; revisit if recall actually proves insufficient in practice.
