use std::sync::Arc;

use crate::memory::{MEMORY_SEARCH_MAX_QUERY_LEN, Memory};
use crate::tools::{
    Tool, ToolContext, ToolDef, ToolDeps, ToolGroup, ToolRegistry, ToolResult,
    error_result, get_str, get_string_array, ok_result,
};

/// Maximum number of notes returned per `memory_list` page.
const MEMORY_LIST_MAX_LIMIT: usize = 100;

/// Maximum number of notes returned by `memory_search` per call.
const MEMORY_SEARCH_MAX_LIMIT: usize = 100;

fn register(registry: &mut ToolRegistry, deps: &ToolDeps) {
    register_memory_tools(registry, Arc::clone(&deps.memory_store));
}

inventory::submit! {
    crate::tools::ToolRegistration {
        name: "memory",
        register_fn: register,
    }
}

/// Register all 10 memory tools into the given registry.
pub fn register_memory_tools(registry: &mut ToolRegistry, store: Arc<dyn Memory>) {
    registry.register_with_group(MemoryCreate(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(MemorySearch(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(MemoryRead(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(MemoryUpdate(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(MemoryLink(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(MemoryTag(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(MemoryForget(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(MemoryLinks(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(MemoryList(Arc::clone(&store)), ToolGroup::Memory);
    registry.register_with_group(MemoryTags(store), ToolGroup::Memory);
}

// --- memory_create ---

struct MemoryCreate(Arc<dyn Memory>);

impl Tool for MemoryCreate {
    fn name(&self) -> &str {
        "memory_create"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_create".to_string(),
            description: "Create a new memory note with a title, content, and optional tags."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Title of the note"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content of the note"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tags to categorize the note. Tags are slash-delimited nested paths (e.g. `recipes/italian/carbonara`) and must use only `[a-z0-9._-]` per segment. Inputs are silently lowercased; spaces and other characters are rejected. Maximum depth is 8 segments."
                    }
                },
                "required": ["title", "content"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let title = match get_str(&args, "title") {
            Some(t) => t,
            None => return error_result(call_id, "missing required field: title"),
        };
        let content = match get_str(&args, "content") {
            Some(c) => c,
            None => return error_result(call_id, "missing required field: content"),
        };
        let tags = match get_string_array(&args, "tags") {
            Ok(t) => t,
            Err(e) => return error_result(call_id, &e),
        };

        let store = self.0.clone();
        let title = title.to_string();
        let content = content.to_string();

        match tokio::task::spawn_blocking(move || store.create_note(&title, &content, &tags)).await
        {
            Ok(Ok(note)) => ok_result(call_id, match serde_json::to_value(&note) {
                    Ok(v) => v,
                    Err(e) => return error_result(call_id, &format!("serialization error: {e}")),
                }),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- memory_search ---

struct MemorySearch(Arc<dyn Memory>);

impl Tool for MemorySearch {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_search".to_string(),
            description: "Search memory notes using FTS5 full-text search. Supports prefix queries (`auth*`), phrase queries (`\"oauth refresh\"`), and boolean operators (`oauth AND token`). Results are ordered by bm25 relevance. The optional `tag` filter is a PREFIX match by default: `recipes` matches `recipes`, `recipes/italian`, and all descendants. Set `tag_exact: true` to match only the exact tag.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "FTS5 search query. Supports prefix (`auth*`), phrase (`\"oauth refresh\"`), and boolean (`oauth AND token`) syntax. Literal strings with special characters are handled automatically.",
                        "maxLength": 1024
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results (default: 10). Silently clamped to 100.",
                        "minimum": 0,
                        "maximum": 100
                    },
                    "tag": {
                        "type": "string",
                        "description": "Optional tag filter. Prefix-match by default (matches the tag and all descendants). Slash-delimited nested path; lowercase `[a-z0-9._-]` per segment."
                    },
                    "tag_exact": {
                        "type": "boolean",
                        "description": "If true, `tag` must match exactly — no prefix expansion. Default: false."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let query = match get_str(&args, "query") {
            Some(q) => q.to_string(),
            None => return error_result(call_id, "missing required field: query"),
        };
        if query.len() > MEMORY_SEARCH_MAX_QUERY_LEN {
            return error_result(call_id, "query too long: max 1024 bytes");
        }
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(MEMORY_SEARCH_MAX_LIMIT as u64) as usize;
        let tag = get_str(&args, "tag").map(String::from);
        let tag_exact = args.get("tag_exact").and_then(|v| v.as_bool()).unwrap_or(false);

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || {
            store.search_notes(&query, limit, tag.as_deref(), tag_exact)
        })
        .await
        {
            Ok(Ok(notes)) => ok_result(call_id, match serde_json::to_value(&notes) {
                    Ok(v) => v,
                    Err(e) => return error_result(call_id, &format!("serialization error: {e}")),
                }),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- memory_read ---

struct MemoryRead(Arc<dyn Memory>);

impl Tool for MemoryRead {
    fn name(&self) -> &str {
        "memory_read"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_read".to_string(),
            description: "Read a memory note by its ID. Use id 'core' to read the core persona."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Note ID (e.g. 'note_a1b2c3d4' or 'core')"
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let id = match get_str(&args, "id") {
            Some(i) => i.to_string(),
            None => return error_result(call_id, "missing required field: id"),
        };

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.read_note(&id)).await {
            Ok(Ok(note)) => ok_result(call_id, match serde_json::to_value(&note) {
                    Ok(v) => v,
                    Err(e) => return error_result(call_id, &format!("serialization error: {e}")),
                }),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- memory_update ---

struct MemoryUpdate(Arc<dyn Memory>);

impl Tool for MemoryUpdate {
    fn name(&self) -> &str {
        "memory_update"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_update".to_string(),
            description:
                "Update the content of a memory note. Use id 'core' to update the core persona."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Note ID to update"
                    },
                    "content": {
                        "type": "string",
                        "description": "New content for the note"
                    }
                },
                "required": ["id", "content"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let id = match get_str(&args, "id") {
            Some(i) => i.to_string(),
            None => return error_result(call_id, "missing required field: id"),
        };
        let content = match get_str(&args, "content") {
            Some(c) => c.to_string(),
            None => return error_result(call_id, "missing required field: content"),
        };

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.update_note(&id, &content)).await {
            Ok(Ok(note)) => ok_result(call_id, match serde_json::to_value(&note) {
                    Ok(v) => v,
                    Err(e) => return error_result(call_id, &format!("serialization error: {e}")),
                }),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- memory_link ---

struct MemoryLink(Arc<dyn Memory>);

impl Tool for MemoryLink {
    fn name(&self) -> &str {
        "memory_link"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_link".to_string(),
            description: "Create a directional link from one note to another with a named relation. The link can be retrieved from either side via memory_links, but storage is one-way — call twice with from/to swapped if you want a symmetric relationship.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "from": {
                        "type": "string",
                        "description": "Source note ID"
                    },
                    "to": {
                        "type": "string",
                        "description": "Target note ID"
                    },
                    "relation": {
                        "type": "string",
                        "description": "Type of relationship (e.g. 'related_to', 'contradicts')"
                    }
                },
                "required": ["from", "to", "relation"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let from = match get_str(&args, "from") {
            Some(f) => f.to_string(),
            None => return error_result(call_id, "missing required field: from"),
        };
        let to = match get_str(&args, "to") {
            Some(t) => t.to_string(),
            None => return error_result(call_id, "missing required field: to"),
        };
        let relation = match get_str(&args, "relation") {
            Some(r) => r.to_string(),
            None => return error_result(call_id, "missing required field: relation"),
        };

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.link_notes(&from, &to, &relation)).await {
            Ok(Ok(link)) => ok_result(call_id, match serde_json::to_value(&link) {
                    Ok(v) => v,
                    Err(e) => return error_result(call_id, &format!("serialization error: {e}")),
                }),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- memory_tag ---

struct MemoryTag(Arc<dyn Memory>);

impl Tool for MemoryTag {
    fn name(&self) -> &str {
        "memory_tag"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_tag".to_string(),
            description: "Replace the tags on a note. Pass an empty array `[]` to clear all tags from the note.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Note ID to tag"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "New set of tags for the note. Tags are slash-delimited nested paths (e.g. `recipes/italian/carbonara`) and must use only `[a-z0-9._-]` per segment. Inputs are silently lowercased; spaces and other characters are rejected. Maximum depth is 8 segments."
                    }
                },
                "required": ["id", "tags"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let id = match get_str(&args, "id") {
            Some(i) => i.to_string(),
            None => return error_result(call_id, "missing required field: id"),
        };
        // The `tags` field is required (the schema marks it so) but its value
        // may be an empty array — that's the documented way to clear all tags
        // from a note. Distinguish "missing field" from "field present, empty".
        if !args.get("tags").is_some_and(|v| v.is_array()) {
            return error_result(
                call_id,
                "missing required field: tags (must be an array; pass [] to clear)",
            );
        }
        // Strict parse: a malformed `tags: [123]` must error rather than fall
        // through to the empty-array clear-all path.
        let tags = match get_string_array(&args, "tags") {
            Ok(t) => t,
            Err(e) => return error_result(call_id, &e),
        };

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.tag_note(&id, &tags)).await {
            Ok(Ok(note)) => ok_result(call_id, match serde_json::to_value(&note) {
                    Ok(v) => v,
                    Err(e) => return error_result(call_id, &format!("serialization error: {e}")),
                }),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- memory_forget ---

struct MemoryForget(Arc<dyn Memory>);

impl Tool for MemoryForget {
    fn name(&self) -> &str {
        "memory_forget"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_forget".to_string(),
            description: "Soft-delete a note (marks as deleted, excluded from search and listing)."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Note ID to forget"
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let id = match get_str(&args, "id") {
            Some(i) => i.to_string(),
            None => return error_result(call_id, "missing required field: id"),
        };

        let store = self.0.clone();
        let id_clone = id.clone();
        match tokio::task::spawn_blocking(move || store.forget_note(&id_clone)).await {
            Ok(Ok(())) => ok_result(
                call_id,
                serde_json::json!({ "status": "forgotten", "id": id }),
            ),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- memory_links ---

struct MemoryLinks(Arc<dyn Memory>);

impl Tool for MemoryLinks {
    fn name(&self) -> &str {
        "memory_links"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_links".to_string(),
            description: "List all links from/to a given note.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Note ID to query links for"
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let id = match get_str(&args, "id") {
            Some(i) => i.to_string(),
            None => return error_result(call_id, "missing required field: id"),
        };

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.get_links_for_note(&id)).await {
            Ok(Ok(links)) => ok_result(call_id, match serde_json::to_value(&links) {
                    Ok(v) => v,
                    Err(e) => return error_result(call_id, &format!("serialization error: {e}")),
                }),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- memory_list ---

struct MemoryList(Arc<dyn Memory>);

impl Tool for MemoryList {
    fn name(&self) -> &str {
        "memory_list"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_list".to_string(),
            description: "List memory notes as paginated summaries (no content). Use `memory_read(id)` to fetch the full content of a specific note. The `tag` filter is a PREFIX match by default: `recipes` matches `recipes`, `recipes/italian`, and `recipes/italian/carbonara`. Set `tag_exact: true` to match only the exact tag with no descendants.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "tag": {
                        "type": "string",
                        "description": "Optional tag filter. Prefix-match by default (matches the tag and all descendants). Slash-delimited nested path; lowercase `[a-z0-9._-]` per segment."
                    },
                    "tag_exact": {
                        "type": "boolean",
                        "description": "If true, `tag` must match exactly — no prefix expansion. Default: false."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 100,
                        "description": "Maximum number of notes per page. Default: 20. Values above 100 are clamped silently to 100."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Number of notes to skip (for pagination). Default: 0. Large offsets simply return empty pages."
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let tag = get_str(&args, "tag").map(String::from);
        let tag_exact = args.get("tag_exact").and_then(|v| v.as_bool()).unwrap_or(false);
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20)
            .min(MEMORY_LIST_MAX_LIMIT as u64) as usize;
        // Clamp to i64::MAX before casting: rusqlite binds `usize` parameters as
        // i64, so a u64 value above i64::MAX would wrap to a negative offset and
        // SQLite would treat that as invalid rather than the documented
        // "returns empty pages" behaviour.
        let offset = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(i64::MAX as u64) as usize;

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || {
            store.list_notes_paginated(tag.as_deref(), tag_exact, limit, offset)
        })
        .await
        {
            Ok(Ok(paginated)) => ok_result(call_id, match serde_json::to_value(&paginated) {
                    Ok(v) => v,
                    Err(e) => return error_result(call_id, &format!("serialization error: {e}")),
                }),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}

// --- memory_tags ---

struct MemoryTags(Arc<dyn Memory>);

impl Tool for MemoryTags {
    fn name(&self) -> &str {
        "memory_tags"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_tags".to_string(),
            description: "List all distinct tags with usage counts. Optionally filter to a tag namespace by passing `prefix`. Counts are per-distinct-tag with no rollup — sum them yourself if you want a namespace total.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prefix": {
                        "type": "string",
                        "description": "Optional tag prefix. Returns only tags equal to `prefix` or starting with `prefix/`. Lowercase `[a-z0-9._-]` per segment, slash-delimited."
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.call_id;
        let prefix = get_str(&args, "prefix").map(String::from);

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.list_tags(prefix.as_deref())).await {
            Ok(Ok(tags)) => ok_result(call_id, match serde_json::to_value(&tags) {
                    Ok(v) => v,
                    Err(e) => return error_result(call_id, &format!("serialization error: {e}")),
                }),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}
