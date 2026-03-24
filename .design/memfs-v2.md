# Feature: MemFS v2 — jj-backed Markdown Memory with SQLite Index

## Summary

Replace Borealis's SQLite-primary memory system with a **jj-backed markdown filesystem** ("MemFS") where the source of truth is versioned markdown files with YAML frontmatter, and SQLite serves as a derived, rebuildable read index. This gives Aurora human-readable, diffable, versionable memory that supports concurrent writes natively — without the complexity of git worktrees or merge conflicts.

This is a post-MVP architecture change. The current MVP memory system (SQLite notes/tags/links tables + 8 `memory_*` tool handlers) works and should remain operational during the transition.

## Motivation

### Why the current system is limiting

The MVP memory system stores notes as rows in SQLite. This works for structured CRUD but has fundamental limitations:

1. **Opaque storage** — Memory content lives in database rows. Humans can't browse, edit, or diff Aurora's memories without SQL tooling. There's no natural way to version-control what Aurora knows.

2. **Flat structure** — Notes are a flat list with tags. There's no hierarchy, no progressive disclosure, no way for Aurora to organize her knowledge spatially (by topic, by importance, by recency).

3. **No versioning** — When Aurora updates a note, the old content is lost. There's no history of how her understanding evolved. Soft-delete (`deleted_at`) preserves forgotten notes but nothing else.

4. **Single-writer bottleneck** — `Arc<Mutex<Connection>>` means all memory operations are serialized. Fine for a single agent, but blocks any future where multiple processes (reflection agents, memory consolidation, subagents) write concurrently.

5. **No human-agent collaboration** — A human can't open Aurora's memory in a text editor, make a correction, and have her pick it up. The database is a black box.

### Why markdown + jj

**Markdown files** are the natural unit of knowledge for LLMs. They're what goes into system prompts. Storing memory as markdown means the storage format *is* the consumption format — no serialization layer, no lossy conversion.

**Jujutsu (jj)** is a version control system designed around the idea that conflicts are data, not emergencies. Its key properties for our use case:

- **Library-first**: `jj-lib` (0.39.0, crates.io, Apache-2.0) is the canonical implementation. The CLI is a thin wrapper. We link the library directly — no subprocess shelling.
- **Concurrent writes without coordination**: Multiple changes can exist simultaneously. No lock files, no index.lock races. Each writer creates a change; jj handles the DAG.
- **Conflicts are materialized, not fatal**: If two writers modify the same file, the result is a conflict marker *in the file* — which can be resolved later, automatically, or by the agent itself.
- **Automatic rebasing**: When the main line moves forward, outstanding changes are automatically rebased. No manual merge workflow.
- **Operation log**: Every mutation to the repo is recorded. Full undo/redo at the repository level, not just file level.
- **`SimpleBackend`**: jj has a native storage backend that doesn't require git. Lighter weight, no `.git` directory, no git compatibility concerns.

### Prior Art

**Letta Context Repositories** (February 2026) — Letta rebuilt their memory system around git-backed markdown files with frontmatter descriptions. Progressive disclosure via file hierarchy. Memory swarms using git worktrees for concurrent writes. This validates the general approach, but their use of git worktrees is clunkier than what jj offers natively.

**Claude Code's auto-memory** — This very conversation uses file-based memory with YAML frontmatter (`name`, `description`, `type`) and a `MEMORY.md` index. It works well for human-agent collaboration. Our design borrows this pattern directly.

**mindgraph-rs** (0.6.1, docs.rs) — A typed knowledge graph with 48 node types across 6 semantic layers, confidence/salience scoring, provenance tracking, contradiction detection, memory decay. Backed by CozoDB. The library itself is too heavy and too tightly coupled to its graph DB, but several ideas are worth incorporating: **confidence scores** on memories, **provenance** (who said/observed what, when), **salience decay** over time, and **contradiction detection** between memories.

## Architecture

### Data Flow

```
                    ┌─────────────────┐
                    │   Aurora (LLM)  │
                    │                 │
                    │  system prompt  │◄── file tree + frontmatter (always loaded)
                    │  + tool calls   │◄── full file content (on demand via tools)
                    └────┬───────┬────┘
                         │       │
                    write│       │read
                         ▼       ▼
               ┌─────────────────────────┐
               │     MemoryStore trait    │
               │                         │
               │  write_file()           │──► jj transaction ──► commit
               │  read_file()            │◄── jj tree lookup
               │  search()               │◄── SQLite FTS5 index
               │  list_tree()            │◄── jj tree walk
               │  get_history()          │◄── jj log
               └─────────────────────────┘
                         │           │
                ┌────────┘           └────────┐
                ▼                              ▼
    ┌───────────────────┐          ┌───────────────────┐
    │   jj repository   │          │   SQLite index    │
    │   (SimpleBackend) │          │   (.memindex.db)  │
    │                   │          │                   │
    │  memory/          │ ──sync──►│  files (path,     │
    │    system/        │          │    frontmatter,   │
    │    people/        │          │    content_hash)  │
    │    projects/      │          │  fts5 (search)    │
    │    reflections/   │          │  tags             │
    │    ...            │          │  links            │
    └───────────────────┘          └───────────────────┘
```

### File Format

Every memory file is markdown with YAML frontmatter:

```markdown
---
title: "Tyto's communication style"
tags: [people, preferences]
confidence: 0.85
salience: 0.9
provenance: conversation
created: 2026-03-20T14:30:00Z
updated: 2026-03-24T09:15:00Z
links:
  - target: people/tyto.md
    relation: describes
  - target: projects/borealis.md
    relation: context_for
---

Tyto prefers concise responses without trailing summaries. They're a Rust
learner with deep interests in VCS and agent architectures. Direct
communication style — says what they mean, expects the same back.

When explaining technical concepts, frame in terms of systems they already
know (Discord bots, self-hosting, jj).
```

### Directory Structure

```
memory/
├── system/                    # Always fully loaded into system prompt
│   └── core.md                # Aurora's core persona (migrated from current core.md)
├── people/                    # People Aurora knows
│   └── tyto.md
├── projects/                  # Ongoing projects and context
│   └── borealis.md
├── knowledge/                 # Learned facts and observations
│   ├── rust-patterns.md
│   └── discord-etiquette.md
├── reflections/               # Aurora's self-observations and growth
│   └── 2026-03-reflection.md
├── conversations/             # Distilled conversation insights (not raw logs)
│   └── ...
└── .memindex.db               # SQLite index (gitignored by jj)
```

**Progressive disclosure rules:**
1. The file tree structure (paths + directory names) is always visible in the system prompt
2. Frontmatter (`title`, `tags`, `confidence`, one-line summary) from each file is always visible
3. Full file content is loaded only for `system/` files and on-demand via tool calls
4. This gives Aurora a "table of contents" of everything she knows without consuming the full token budget

### jj-lib Integration

```rust
use jj_lib::workspace::Workspace;
use jj_lib::repo::Repo;
use jj_lib::transaction::Transaction;

pub struct JjMemoryBackend {
    workspace: Workspace,
    index: SqliteIndex,
}

impl JjMemoryBackend {
    /// Initialize or open the memory repository
    pub fn open(path: &Path) -> Result<Self> {
        // Open existing workspace or init with SimpleBackend
        // Build/verify SQLite index
    }

    /// Write a memory file within a jj transaction
    pub fn write_file(&self, path: &str, content: &str) -> Result<()> {
        // 1. Start jj transaction
        // 2. Write file to tree
        // 3. Commit with descriptive message
        // 4. Update SQLite index
    }

    /// Read a file from the current jj tree
    pub fn read_file(&self, path: &str) -> Result<String> {
        // Read from working copy or committed tree
    }

    /// Full-text search via SQLite FTS5 index
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        // Query FTS5 index, return file paths + snippets + scores
    }

    /// Rebuild the entire SQLite index from jj tree
    pub fn rebuild_index(&self) -> Result<()> {
        // Walk the jj tree, parse all markdown files,
        // populate files/fts5/tags/links tables
    }

    /// Get the history of changes to a specific file
    pub fn file_history(&self, path: &str) -> Result<Vec<ChangeRecord>> {
        // Walk jj log filtered to changes touching this path
    }
}
```

### SQLite Index Schema

The index is derived and rebuildable. If deleted, `rebuild_index()` reconstructs it from the jj tree.

```sql
-- File metadata extracted from frontmatter
CREATE TABLE files (
    path            TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    content_hash    TEXT NOT NULL,      -- detect stale index entries
    confidence      REAL DEFAULT 1.0,
    salience        REAL DEFAULT 1.0,
    provenance      TEXT,               -- 'conversation', 'reflection', 'manual', 'migration'
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- Full-text search over file content
CREATE VIRTUAL TABLE fts USING fts5(
    path,
    title,
    content,
    tags,
    content='files',
    content_rowid='rowid'
);

-- Tag index for fast filtering
CREATE TABLE tags (
    path            TEXT NOT NULL REFERENCES files(path),
    tag             TEXT NOT NULL,
    PRIMARY KEY (path, tag)
);
CREATE INDEX idx_tags_tag ON tags(tag);

-- Link index for graph queries
CREATE TABLE links (
    from_path       TEXT NOT NULL REFERENCES files(path),
    to_path         TEXT NOT NULL REFERENCES files(path),
    relation        TEXT NOT NULL,
    PRIMARY KEY (from_path, to_path)
);
```

## Tool Interface

### Option A: Evolved memory tools (recommended for transition)

Keep the `memory_*` tool pattern but operate on files instead of DB rows. This maintains backward compatibility with Aurora's learned tool usage while the backend changes underneath.

| Current Tool | MemFS v2 Equivalent | Change |
|---|---|---|
| `memory_create` | Creates a new `.md` file with frontmatter, commits to jj | File path derived from title + category |
| `memory_read` | Reads file content from jj tree | Returns full markdown including frontmatter |
| `memory_update` | Writes updated content, commits to jj | Preserves frontmatter, updates `updated` timestamp |
| `memory_search` | Queries SQLite FTS5 index | Returns file paths + snippets + relevance scores |
| `memory_list` | Walks jj tree, optionally filtered by tag/directory | Returns tree structure with frontmatter summaries |
| `memory_link` | Adds `links` entry to both files' frontmatter | Bidirectional, committed as single jj transaction |
| `memory_tag` | Updates `tags` array in file frontmatter | Committed, index updated |
| `memory_forget` | Removes file from jj tree, commits | Recoverable via jj undo / operation log |
| `memory_links` | Queries link index | New tool (fixes issue #38) |
| `memory_history` | Queries jj log for file | New tool — shows how a memory evolved |
| `memory_tree` | Returns directory tree with frontmatter | New tool — progressive disclosure navigation |

### Option B: Computer use tools (longer term)

Add general-purpose filesystem tools that Aurora can use for memory and other tasks:

| Tool | Description |
|---|---|
| `file_read` | Read a file from the memory filesystem |
| `file_write` | Write/create a file (auto-commits to jj) |
| `file_list` | List directory contents with optional recursion |
| `file_search` | Full-text search across all files (FTS5) |
| `file_move` | Move/rename a file (jj tracks the rename) |
| `file_delete` | Remove a file (recoverable via jj) |
| `file_history` | Show change history for a file |
| `file_diff` | Show what changed between two versions |
| `bash_exec` | Execute a shell command (sandboxed, allowlisted) |

Option B is more general but requires Aurora to learn filesystem conventions rather than using purpose-built memory tools. The recommended path is to ship Option A first, then add Option B alongside it, and let Aurora naturally migrate to whichever she prefers.

## Concurrent Memory Access

### Memory Reflection (sleep-time compute)

A background process that periodically reviews recent conversation history and distills important information into memory files:

```
Main agent thread                    Reflection subagent
      │                                     │
      │  (conversation happening)           │
      │                                     │
      │──── trigger reflection ────────────►│
      │                                     │
      │  (continues responding)             │ create jj change
      │                                     │ read conversation history
      │                                     │ write/update memory files
      │                                     │ commit change
      │                                     │
      │◄──── jj auto-rebases ──────────────│
      │                                     │
      │  (next request sees new memories)   │
```

In jj, the reflection agent simply creates a new change. There's no lock contention, no worktree setup, no merge step. When the main agent next reads the tree, jj has already rebased if needed. If both modified the same file, the conflict is visible in the file content and can be resolved.

### Memory Defragmentation

Over time, memories accumulate and become disorganized. A periodic defragmentation process:

1. Creates a jj change
2. Reads all memory files
3. Reorganizes: splits large files, merges duplicates, restructures hierarchy
4. Commits with a descriptive message
5. Rebuilds the SQLite index

Because jj tracks file identity across renames, the history of a memory survives reorganization.

## Migration Path

### Phase 1: Dual-write (non-breaking)

- Initialize a jj repository in `memory/`
- On every `memory_create`/`memory_update`/`memory_tag`, also write a corresponding `.md` file and commit to jj
- SQLite remains the read path
- Existing memories exported to markdown files via one-time migration script
- Aurora doesn't know anything changed

### Phase 2: Index flip

- SQLite index rebuilt from jj tree (not from notes table)
- Read path switches to jj tree + SQLite FTS5 index
- Old notes/tags/links tables become unused
- Tool behavior unchanged from Aurora's perspective

### Phase 3: New capabilities

- Add `memory_history`, `memory_tree` tools
- Enable progressive disclosure in system prompt
- Add confidence/salience/provenance fields
- Enable reflection subagent
- Optionally add Option B computer use tools

### One-time migration script

```
borealis migrate-memfs [--source memory/borealis.db] [--target memory/]
```

For each note in the current SQLite database:
1. Derive a file path from title + tags (e.g., a note titled "Rust ownership" with tag "knowledge" → `knowledge/rust-ownership.md`)
2. Generate frontmatter from note metadata (created_at, updated_at, tags)
3. Write the markdown file
4. Create links entries in frontmatter for any existing links
5. Commit to jj with message "migrate: {title}"
6. Migrate `core.md` into `system/core.md`

## Key Decisions

### Why jj SimpleBackend, not GitBackend?

Git compatibility adds complexity (`.git` directory, git objects, index file) without benefit for our use case. Nobody needs to `git clone` Aurora's memories. SimpleBackend is lighter, faster, and avoids the git dependency entirely. If we later want to push memories to a remote for backup, we can add that via jj's native remote support or a simple file sync.

### Why SQLite index instead of pure filesystem search?

`grep` over markdown files works for small memory sets but doesn't scale, and doesn't support structured queries (find all memories with confidence < 0.5, all memories tagged "people" updated in the last week, etc.). SQLite FTS5 gives us sub-millisecond full-text search with ranking. The index is derived and disposable — the jj tree is the source of truth.

### Why not CozoDB (like mindgraph)?

CozoDB is a Datalog-based graph database. Powerful for graph queries but:
- Heavy dependency with limited ecosystem
- Unfamiliar query language (Datalog vs SQL)
- Our graph needs are simple (bidirectional links with relation types)
- SQLite handles our query patterns with standard SQL
- We can add graph traversal as application logic over the links table

### Why keep memory_* tools instead of going straight to computer use?

Aurora has learned the `memory_*` tool interface. Ripping it out and replacing with raw `file_read`/`file_write` would break her mental model. Better to evolve the existing tools to use the new backend (she won't notice), then add computer use tools alongside, and let her naturally adopt whichever she prefers. The tools also provide a security boundary — they can enforce invariants (valid frontmatter, index updates) that raw file writes would bypass.

## Open Questions

1. **Index sync strategy** — Should the SQLite index update synchronously on every jj commit (simpler, slight write latency), or asynchronously via a watcher (faster writes, eventual consistency)? MVP: synchronous. Optimize later if needed.

2. **Conflict resolution** — When a reflection agent and the main agent modify the same file, who resolves the conflict? Options: (a) main agent resolves on next read, (b) automatic last-writer-wins, (c) dedicated conflict resolution step. Leaning toward (a) — let Aurora see the conflict markers and decide.

3. **Memory budget** — How many files before the tree + frontmatter listing exceeds the system prompt budget? Need to test with real usage. Likely need a "hot" set (recently accessed, high salience) that's always loaded and a "cold" set that's only in the FTS5 index.

4. **jj-lib stability** — jj-lib is pre-1.0 (currently 0.39.0) with frequent breaking changes. We'd need to pin to a specific version and handle API churn on upgrades. Acceptable for a self-hosted project; risky for a library. Monitor the 1.0 roadmap.

5. **Binary size impact** — jj-lib pulls in ~37 dependencies. Need to benchmark the binary size increase (currently 15MB release). If it's prohibitive, the SimpleBackend subset might be extractable, or we could implement a minimal VCS layer ourselves (content-addressable store + DAG of changes — simpler than full jj but captures the key benefits).

6. **Salience decay** — How should confidence and salience scores change over time? Options: (a) explicit decay function (halve salience every N days without access), (b) LLM-driven re-evaluation during reflection, (c) access-count-based (frequently read memories stay salient). Likely a combination.

## Out of Scope

- Embedding-based semantic search (separate enhancement, compatible with this architecture)
- Multi-agent memory sharing across different Aurora instances
- Real-time sync to a remote repository
- Web UI for memory browsing (though the markdown files *are* browsable in any editor)
- Memory encryption at rest
