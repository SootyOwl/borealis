# Memory Pagination & Tag Listing

**Date:** 2026-04-06
**Status:** Superseded by [2026-04-07-fts5-and-nested-tags.md](2026-04-07-fts5-and-nested-tags.md)
**Motivation:** Enable Aurora to curate imported Letta memories by browsing them in manageable batches and maintaining consistent tagging.

## Changes

### 1. `memory_list` — add pagination, return summaries

**Current behavior:** Returns all matching `Note` objects (with full content and links), optionally filtered by tag. No pagination.

**New behavior:** Returns paginated `NoteSummary` objects (no content, no links) wrapped in a `PaginatedNotes` envelope.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `tag` | string | no | — | Filter by exact tag match |
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

Use `memory_read(id)` to get full content for a specific note.

### 2. `memory_tags` — new tool

Lists all distinct tags with usage counts, sorted by count descending.

**Parameters:** None.

**Response shape:**

```json
[
  {"tag": "letta-archival", "count": 247},
  {"tag": "person", "count": 12},
  {"tag": "preference", "count": 8}
]
```

Only counts non-deleted notes.

## Implementation

### New types (`src/memory/store.rs`)

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
    limit: usize,
    offset: usize,
) -> MemoryResult<PaginatedNotes>;

fn list_tags(&self) -> MemoryResult<Vec<TagCount>>;
```

Existing `list_notes` remains on the trait (used by migration code).

### `SqliteMemory` implementation

**`list_notes_paginated`:** Two queries — a `COUNT(*)` for total, then a `SELECT ... LIMIT ? OFFSET ?` for the page. Tags populated per-note as in existing code. Select only id, title, created_at, updated_at (no content column).

**`list_tags`:**
```sql
SELECT t.tag, COUNT(*) as cnt
FROM tags t
JOIN notes n ON t.note_id = n.id
WHERE n.deleted_at IS NULL
GROUP BY t.tag
ORDER BY cnt DESC
```

### Tool changes (`src/tools/memory_tools.rs`)

**`MemoryList`:** Add `limit` and `offset` params to definition. Call `list_notes_paginated` instead of `list_notes`. Serialize `PaginatedNotes` as response.

**`MemoryTags`:** New tool struct. No params. Calls `list_tags`. Register in `register_memory_tools` (bumps tool count from 9 to 10).

### `MockMemory` updates

Implement `list_notes_paginated` (slice + count from in-memory vec) and `list_tags` (count from in-memory vec).

### Tests

- Pagination: correct slice at various offsets, total count accuracy, offset beyond range returns empty, limit=0 edge case
- Tag listing: correct counts, excludes deleted notes, sorted by count descending
- MockMemory: exercises new trait methods through `Arc<dyn Memory>`
