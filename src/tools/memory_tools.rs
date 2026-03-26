use std::sync::Arc;

use crate::memory::Memory;
use crate::tools::{Tool, ToolContext, ToolDef, ToolDeps, ToolRegistry, ToolResult};

fn register(registry: &mut ToolRegistry, deps: &ToolDeps) {
    register_memory_tools(registry, Arc::clone(&deps.memory_store));
}

inventory::submit! {
    crate::tools::ToolRegistration {
        name: "memory",
        register_fn: register,
    }
}

/// Register all 9 memory tools into the given registry.
pub fn register_memory_tools(registry: &mut ToolRegistry, store: Arc<dyn Memory>) {
    registry.register(MemoryCreate(Arc::clone(&store)));
    registry.register(MemorySearch(Arc::clone(&store)));
    registry.register(MemoryRead(Arc::clone(&store)));
    registry.register(MemoryUpdate(Arc::clone(&store)));
    registry.register(MemoryLink(Arc::clone(&store)));
    registry.register(MemoryTag(Arc::clone(&store)));
    registry.register(MemoryForget(Arc::clone(&store)));
    registry.register(MemoryLinks(Arc::clone(&store)));
    registry.register(MemoryList(store));
}

fn error_result(call_id: &str, msg: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: serde_json::json!({ "error": msg }),
        is_error: true,
    }
}

fn ok_result(call_id: &str, value: serde_json::Value) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: value,
        is_error: false,
    }
}

fn get_str<'a>(args: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    args.get(field).and_then(|v| v.as_str())
}

fn get_string_array(args: &serde_json::Value, field: &str) -> Vec<String> {
    args.get(field)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
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
                        "description": "Tags to categorize the note"
                    }
                },
                "required": ["title", "content"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let title = match get_str(&args, "title") {
            Some(t) => t,
            None => return error_result(call_id, "missing required field: title"),
        };
        let content = match get_str(&args, "content") {
            Some(c) => c,
            None => return error_result(call_id, "missing required field: content"),
        };
        let tags = get_string_array(&args, "tags");

        let store = self.0.clone();
        let title = title.to_string();
        let content = content.to_string();

        match tokio::task::spawn_blocking(move || store.create_note(&title, &content, &tags)).await
        {
            Ok(Ok(note)) => ok_result(call_id, serde_json::to_value(note).unwrap()),
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
            description: "Search memory notes by title, content, or tags.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query to match against title, content, and tags"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results (default: 10)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let query = match get_str(&args, "query") {
            Some(q) => q.to_string(),
            None => return error_result(call_id, "missing required field: query"),
        };
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.search_notes(&query, limit)).await {
            Ok(Ok(notes)) => ok_result(call_id, serde_json::to_value(notes).unwrap()),
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
        let call_id = &ctx.conversation_id;
        let id = match get_str(&args, "id") {
            Some(i) => i.to_string(),
            None => return error_result(call_id, "missing required field: id"),
        };

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.read_note(&id)).await {
            Ok(Ok(note)) => ok_result(call_id, serde_json::to_value(note).unwrap()),
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
        let call_id = &ctx.conversation_id;
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
            Ok(Ok(note)) => ok_result(call_id, serde_json::to_value(note).unwrap()),
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
            description: "Create a bidirectional link between two notes.".to_string(),
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
        let call_id = &ctx.conversation_id;
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
            Ok(Ok(link)) => ok_result(call_id, serde_json::to_value(link).unwrap()),
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
            description: "Update the tags on a note (replaces existing tags).".to_string(),
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
                        "description": "New set of tags for the note"
                    }
                },
                "required": ["id", "tags"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let id = match get_str(&args, "id") {
            Some(i) => i.to_string(),
            None => return error_result(call_id, "missing required field: id"),
        };
        let tags = get_string_array(&args, "tags");
        if tags.is_empty() {
            return error_result(call_id, "missing required field: tags");
        }

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.tag_note(&id, &tags)).await {
            Ok(Ok(note)) => ok_result(call_id, serde_json::to_value(note).unwrap()),
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
        let call_id = &ctx.conversation_id;
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
        let call_id = &ctx.conversation_id;
        let id = match get_str(&args, "id") {
            Some(i) => i.to_string(),
            None => return error_result(call_id, "missing required field: id"),
        };

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.get_links_for_note(&id)).await {
            Ok(Ok(links)) => ok_result(call_id, serde_json::to_value(links).unwrap()),
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
            description: "List memory notes, optionally filtered by tag.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "tag": {
                        "type": "string",
                        "description": "Optional tag to filter by"
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let call_id = &ctx.conversation_id;
        let tag = get_str(&args, "tag").map(String::from);

        let store = self.0.clone();
        match tokio::task::spawn_blocking(move || store.list_notes(tag.as_deref())).await {
            Ok(Ok(notes)) => ok_result(call_id, serde_json::to_value(notes).unwrap()),
            Ok(Err(e)) => error_result(call_id, &e.to_string()),
            Err(e) => error_result(call_id, &format!("task join error: {e}")),
        }
    }
}
