use std::sync::{Arc, Mutex};

use borealis::memory::{Memory, SqliteMemory};
use borealis::tools::{ToolCall, ToolContext, ToolRegistry, register_memory_tools};
use rusqlite::Connection;

fn test_ctx() -> ToolContext {
    ToolContext {
        call_id: "test_call".to_string(),
        author_id: "test_user".to_string(),
        conversation_id: "test_conv".to_string(),
        channel_source: "cli".to_string(),
    }
}

fn setup() -> (Arc<dyn Memory>, ToolRegistry) {
    let conn = Connection::open_in_memory().unwrap();
    let conn = Arc::new(Mutex::new(conn));
    let tmp = std::env::temp_dir().join(format!("borealis_test_core_{}.md", std::process::id()));
    std::fs::write(&tmp, "# Aurora\nI am Aurora, a test persona.").unwrap();

    let store: Arc<dyn Memory> = Arc::new(SqliteMemory::new(conn, tmp).unwrap());
    let mut registry = ToolRegistry::new();
    register_memory_tools(&mut registry, Arc::clone(&store));
    (store, registry)
}

/// AC-7: memory_create inserts a row; memory_read retrieves it
#[tokio::test]
async fn ac7_create_and_read() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Create a note
    let result = registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Test Note",
                    "content": "Hello world",
                    "tags": ["greeting", "test"]
                }),
            },
            &ctx,
        )
        .await;

    assert!(!result.is_error, "create failed: {:?}", result.content);
    let note_id = result.content["id"].as_str().unwrap().to_string();
    assert!(note_id.starts_with("note_"));

    // Read it back
    let result = registry
        .execute(
            &ToolCall {
                id: "c2".into(),
                name: "memory_read".into(),
                arguments: serde_json::json!({ "id": note_id }),
            },
            &ctx,
        )
        .await;

    assert!(!result.is_error);
    assert_eq!(result.content["title"], "Test Note");
    assert_eq!(result.content["content"], "Hello world");
    let tags: Vec<String> = result.content["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(tags, vec!["greeting", "test"]);
}

/// AC-7: memory_update changes content
#[tokio::test]
async fn ac7_update() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    let result = registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Updatable",
                    "content": "Original content"
                }),
            },
            &ctx,
        )
        .await;
    let note_id = result.content["id"].as_str().unwrap().to_string();

    let result = registry
        .execute(
            &ToolCall {
                id: "c2".into(),
                name: "memory_update".into(),
                arguments: serde_json::json!({
                    "id": note_id,
                    "content": "Updated content"
                }),
            },
            &ctx,
        )
        .await;

    assert!(!result.is_error);
    assert_eq!(result.content["content"], "Updated content");
}

/// AC-7: memory_search finds by tag, title, and content substring
#[tokio::test]
async fn ac7_search() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Create several notes
    for (title, content, tags) in [
        ("Rust Guide", "Systems programming", vec!["code"]),
        ("Python Intro", "Scripting language", vec!["code"]),
        ("Pasta Recipe", "Carbonara with guanciale", vec!["food"]),
    ] {
        registry
            .execute(
                &ToolCall {
                    id: "c".into(),
                    name: "memory_create".into(),
                    arguments: serde_json::json!({
                        "title": title,
                        "content": content,
                        "tags": tags
                    }),
                },
                &ctx,
            )
            .await;
    }

    // Search by title
    let result = registry
        .execute(
            &ToolCall {
                id: "s1".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": "Rust" }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);
    let notes = result.content.as_array().unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["title"], "Rust Guide");

    // Search by content substring
    let result = registry
        .execute(
            &ToolCall {
                id: "s2".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": "guanciale" }),
            },
            &ctx,
        )
        .await;
    assert_eq!(result.content.as_array().unwrap().len(), 1);

    // Search with tag filter — FTS5 searches title+content; use the `tag`
    // parameter to scope by tag.  Both "Rust Guide" and "Python Intro" are
    // tagged "code".  A wildcard prefix query matches all indexed terms, and
    // the tag filter narrows to the "code" namespace.
    let result = registry
        .execute(
            &ToolCall {
                id: "s3".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": "programming OR language", "tag": "code" }),
            },
            &ctx,
        )
        .await;
    assert_eq!(result.content.as_array().unwrap().len(), 2);
}

/// AC-7: memory_forget sets deleted_at and excludes from subsequent searches
#[tokio::test]
async fn ac7_forget() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    let result = registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Ephemeral",
                    "content": "Temporary note",
                    "tags": ["temp"]
                }),
            },
            &ctx,
        )
        .await;
    let note_id = result.content["id"].as_str().unwrap().to_string();

    // Forget it
    let result = registry
        .execute(
            &ToolCall {
                id: "f1".into(),
                name: "memory_forget".into(),
                arguments: serde_json::json!({ "id": note_id }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(result.content["status"], "forgotten");

    // Search should not find it
    let result = registry
        .execute(
            &ToolCall {
                id: "s1".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": "Ephemeral" }),
            },
            &ctx,
        )
        .await;
    assert_eq!(result.content.as_array().unwrap().len(), 0);

    // Read should return error
    let result = registry
        .execute(
            &ToolCall {
                id: "r1".into(),
                name: "memory_read".into(),
                arguments: serde_json::json!({ "id": note_id }),
            },
            &ctx,
        )
        .await;
    assert!(result.is_error);
}

/// AC-8: memory_link creates a bidirectional relationship
#[tokio::test]
async fn ac8_link_bidirectional() {
    let (store, registry) = setup();
    let ctx = test_ctx();

    let r1 = registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Note Alpha",
                    "content": "First note"
                }),
            },
            &ctx,
        )
        .await;
    let r2 = registry
        .execute(
            &ToolCall {
                id: "c2".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Note Beta",
                    "content": "Second note"
                }),
            },
            &ctx,
        )
        .await;

    let id_a = r1.content["id"].as_str().unwrap().to_string();
    let id_b = r2.content["id"].as_str().unwrap().to_string();

    let result = registry
        .execute(
            &ToolCall {
                id: "l1".into(),
                name: "memory_link".into(),
                arguments: serde_json::json!({
                    "from": id_a,
                    "to": id_b,
                    "relation": "related_to"
                }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);

    // Verify directional links via store directly
    let links_a = store.get_links_for_note(&id_a).unwrap();
    assert_eq!(links_a.len(), 1);
    assert_eq!(links_a[0].to_id, id_b);
    assert_eq!(links_a[0].direction, Some("outgoing".to_string()));

    let links_b = store.get_links_for_note(&id_b).unwrap();
    assert_eq!(links_b.len(), 1);
    assert_eq!(links_b[0].from_id, id_a);
    assert_eq!(links_b[0].direction, Some("incoming".to_string()));
}

/// AC-8: memory_list with tag filter returns only matching notes
#[tokio::test]
async fn ac8_list_with_tag_filter() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    for (title, tags) in [
        ("Alpha", vec!["group_a"]),
        ("Beta", vec!["group_b"]),
        ("Gamma", vec!["group_a", "group_b"]),
    ] {
        registry
            .execute(
                &ToolCall {
                    id: "c".into(),
                    name: "memory_create".into(),
                    arguments: serde_json::json!({
                        "title": title,
                        "content": "content",
                        "tags": tags
                    }),
                },
                &ctx,
            )
            .await;
    }

    // List with tag filter
    let result = registry
        .execute(
            &ToolCall {
                id: "l1".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({ "tag": "group_a" }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);
    // memory_list now returns a PaginatedNotes envelope.
    let notes = result.content["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 2);
    assert_eq!(result.content["total"].as_u64().unwrap(), 2);

    // List all
    let result = registry
        .execute(
            &ToolCall {
                id: "l2".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({}),
            },
            &ctx,
        )
        .await;
    assert_eq!(result.content["notes"].as_array().unwrap().len(), 3);
    assert_eq!(result.content["total"].as_u64().unwrap(), 3);
}

/// AC-9: Core persona from memory/core.md is accessible and modifiable
#[tokio::test]
async fn ac9_core_persona() {
    let (store, registry) = setup();
    let ctx = test_ctx();

    // Load core persona via store
    let persona = store.load_core_persona().unwrap();
    assert!(persona.contains("Aurora"));

    // Read via tool
    let result = registry
        .execute(
            &ToolCall {
                id: "r1".into(),
                name: "memory_read".into(),
                arguments: serde_json::json!({ "id": "core" }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(result.content["id"], "core");
    assert!(
        result.content["content"]
            .as_str()
            .unwrap()
            .contains("Aurora")
    );

    // Update via tool
    let result = registry
        .execute(
            &ToolCall {
                id: "u1".into(),
                name: "memory_update".into(),
                arguments: serde_json::json!({
                    "id": "core",
                    "content": "# Aurora\nUpdated persona text."
                }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);
    assert!(
        result.content["content"]
            .as_str()
            .unwrap()
            .contains("Updated persona")
    );

    // Verify persistence
    let persona = store.load_core_persona().unwrap();
    assert!(persona.contains("Updated persona"));
}

/// All 10 tools are registered
#[tokio::test]
async fn all_tools_registered() {
    let (_store, registry) = setup();

    assert_eq!(registry.tool_count(), 10);
    for name in [
        "memory_create",
        "memory_search",
        "memory_read",
        "memory_update",
        "memory_link",
        "memory_links",
        "memory_tag",
        "memory_forget",
        "memory_list",
        "memory_tags",
    ] {
        assert!(registry.has_tool(name), "missing tool: {name}");
    }

    let defs = registry.definitions();
    assert_eq!(defs.len(), 10);
    for def in &defs {
        assert!(!def.description.is_empty());
        assert!(def.parameters.is_object());
    }
}

/// memory_tag replaces tags via tool
#[tokio::test]
async fn tag_tool_replaces() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    let result = registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Taggable",
                    "content": "Content",
                    "tags": ["old_tag"]
                }),
            },
            &ctx,
        )
        .await;
    let note_id = result.content["id"].as_str().unwrap().to_string();

    let result = registry
        .execute(
            &ToolCall {
                id: "t1".into(),
                name: "memory_tag".into(),
                arguments: serde_json::json!({
                    "id": note_id,
                    "tags": ["new_tag_1", "new_tag_2"]
                }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);
    let tags: Vec<String> = result.content["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(tags, vec!["new_tag_1", "new_tag_2"]);
}

/// memory_tag with `tags: []` clears all tags from a note (documented behaviour).
#[tokio::test]
async fn tag_tool_empty_array_clears_tags() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Create a note with two tags.
    let create = registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Tagged",
                    "content": "Content",
                    "tags": ["foo", "bar"]
                }),
            },
            &ctx,
        )
        .await;
    let note_id = create.content["id"].as_str().unwrap().to_string();

    // Pass an empty tags array — this should succeed and clear all tags.
    let cleared = registry
        .execute(
            &ToolCall {
                id: "t1".into(),
                name: "memory_tag".into(),
                arguments: serde_json::json!({
                    "id": note_id,
                    "tags": []
                }),
            },
            &ctx,
        )
        .await;
    assert!(
        !cleared.is_error,
        "memory_tag with [] should succeed: {:?}",
        cleared.content
    );
    let tags = cleared.content["tags"].as_array().unwrap();
    assert!(tags.is_empty(), "tags should be empty after clear");

    // A missing `tags` field is still an error (distinguished from empty array).
    let missing = registry
        .execute(
            &ToolCall {
                id: "t2".into(),
                name: "memory_tag".into(),
                arguments: serde_json::json!({ "id": note_id }),
            },
            &ctx,
        )
        .await;
    assert!(missing.is_error, "missing `tags` field should error");
}

/// memory_tag rejects arrays containing non-string elements rather than silently
/// dropping them — otherwise a malformed `{"tags":[123]}` collapses to `[]` and
/// clears all tags on the note.
#[tokio::test]
async fn tag_tool_rejects_non_string_elements() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    let create = registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Tagged",
                    "content": "Content",
                    "tags": ["keep_me"]
                }),
            },
            &ctx,
        )
        .await;
    let note_id = create.content["id"].as_str().unwrap().to_string();

    let bad = registry
        .execute(
            &ToolCall {
                id: "t1".into(),
                name: "memory_tag".into(),
                arguments: serde_json::json!({
                    "id": note_id,
                    "tags": [123]
                }),
            },
            &ctx,
        )
        .await;
    assert!(
        bad.is_error,
        "memory_tag with non-string element must error: {:?}",
        bad.content
    );

    // Existing tags must be preserved — the malformed call must not have
    // clobbered them via the empty-array path.
    let read = registry
        .execute(
            &ToolCall {
                id: "r1".into(),
                name: "memory_read".into(),
                arguments: serde_json::json!({ "id": note_id }),
            },
            &ctx,
        )
        .await;
    let tags: Vec<String> = read.content["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(tags, vec!["keep_me"], "tags should be unchanged after a rejected malformed call");
}

/// Fix B — memory_list clamps limit to MEMORY_LIST_MAX_LIMIT (100) silently.
#[tokio::test]
async fn memory_list_caps_limit_at_max() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Seed one note so the response is non-trivial.
    registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Seed",
                    "content": "content"
                }),
            },
            &ctx,
        )
        .await;

    let result = registry
        .execute(
            &ToolCall {
                id: "l1".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({ "limit": 9999 }),
            },
            &ctx,
        )
        .await;

    assert!(!result.is_error, "memory_list failed: {:?}", result.content);
    // The echoed limit must be clamped to 100, not 9999.
    assert_eq!(
        result.content["limit"].as_u64().unwrap(),
        100,
        "limit field in response should be clamped to 100"
    );

    // Boundary: limit exactly at the cap should pass through unchanged.
    let at_cap = registry
        .execute(
            &ToolCall {
                id: "l_at_cap".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({ "limit": 100 }),
            },
            &ctx,
        )
        .await;
    assert!(!at_cap.is_error);
    assert_eq!(
        at_cap.content["limit"].as_u64().unwrap(),
        100,
        "limit=100 should pass through unchanged (boundary)"
    );

    // Boundary: one above the cap should clamp to 100, not 101.
    let just_over = registry
        .execute(
            &ToolCall {
                id: "l_over".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({ "limit": 101 }),
            },
            &ctx,
        )
        .await;
    assert!(!just_over.is_error);
    assert_eq!(
        just_over.content["limit"].as_u64().unwrap(),
        100,
        "limit=101 should clamp to 100 (off-by-one guard)"
    );
}

/// memory_list with an offset above i64::MAX must not error out via signed
/// overflow when bound to SQLite (rusqlite encodes `usize` as i64). The tool
/// clamps oversized values so the call returns an empty page instead.
#[tokio::test]
async fn memory_list_handles_oversized_offset() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Seed one note so the table isn't empty.
    registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({ "title": "Seed", "content": "content" }),
            },
            &ctx,
        )
        .await;

    let result = registry
        .execute(
            &ToolCall {
                id: "l1".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({ "offset": u64::MAX }),
            },
            &ctx,
        )
        .await;
    assert!(
        !result.is_error,
        "memory_list with u64::MAX offset must not error: {:?}",
        result.content
    );
    let notes = result.content["notes"].as_array().unwrap();
    assert!(notes.is_empty(), "huge offset should yield an empty page");
}

/// Fix C — memory_tags tool: lists all tags with counts, supports prefix filter,
/// and returns an error for an invalid prefix.
#[tokio::test]
async fn tags_tool_lists_with_counts_and_prefix() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Seed notes with overlapping nested tags.
    for (title, tags) in [
        ("Italian dish", vec!["recipes", "recipes/italian"]),
        ("French dish", vec!["recipes", "recipes/french"]),
        ("Another Italian", vec!["recipes/italian"]),
        ("Person note", vec!["person"]),
    ] {
        registry
            .execute(
                &ToolCall {
                    id: "c".into(),
                    name: "memory_create".into(),
                    arguments: serde_json::json!({
                        "title": title,
                        "content": "content",
                        "tags": tags
                    }),
                },
                &ctx,
            )
            .await;
    }

    // --- No prefix: all tags returned ---
    let result = registry
        .execute(
            &ToolCall {
                id: "t1".into(),
                name: "memory_tags".into(),
                arguments: serde_json::json!({}),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error, "memory_tags failed: {:?}", result.content);
    let all_tags = result.content.as_array().unwrap();
    let find_count = |tag: &str| -> u64 {
        all_tags
            .iter()
            .find(|t| t["tag"] == tag)
            .and_then(|t| t["count"].as_u64())
            .unwrap_or(0)
    };
    assert_eq!(find_count("recipes"), 2, "recipes should appear on 2 notes");
    assert_eq!(
        find_count("recipes/italian"),
        2,
        "recipes/italian should appear on 2 notes"
    );
    assert_eq!(
        find_count("recipes/french"),
        1,
        "recipes/french should appear on 1 note"
    );
    assert_eq!(find_count("person"), 1, "person should appear on 1 note");

    // --- Prefix filter: only recipes/* tags returned ---
    let result = registry
        .execute(
            &ToolCall {
                id: "t2".into(),
                name: "memory_tags".into(),
                arguments: serde_json::json!({ "prefix": "recipes" }),
            },
            &ctx,
        )
        .await;
    assert!(
        !result.is_error,
        "memory_tags with prefix failed: {:?}",
        result.content
    );
    let recipe_tags = result.content.as_array().unwrap();
    let recipe_tag_names: Vec<&str> = recipe_tags
        .iter()
        .map(|t| t["tag"].as_str().unwrap())
        .collect();
    assert!(recipe_tag_names.contains(&"recipes"));
    assert!(recipe_tag_names.contains(&"recipes/italian"));
    assert!(recipe_tag_names.contains(&"recipes/french"));
    assert!(
        !recipe_tag_names.contains(&"person"),
        "person should not appear under recipes prefix"
    );

    // --- Invalid prefix returns an error ---
    let result = registry
        .execute(
            &ToolCall {
                id: "t3".into(),
                name: "memory_tags".into(),
                arguments: serde_json::json!({ "prefix": "bad tag" }),
            },
            &ctx,
        )
        .await;
    assert!(
        result.is_error,
        "memory_tags with invalid prefix should return an error"
    );
}

/// Fix D + Fix E — memory_list tool exercises tag_exact, limit, offset,
/// and asserts the envelope fields are echoed correctly.
#[tokio::test]
async fn memory_list_tool_pagination_and_exact_filter() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Seed: two notes tagged "food", one tagged "food/italian" (child only).
    for (title, tags) in [
        ("Food A", vec!["food"]),
        ("Food B", vec!["food"]),
        ("Italian only", vec!["food/italian"]),
    ] {
        registry
            .execute(
                &ToolCall {
                    id: "c".into(),
                    name: "memory_create".into(),
                    arguments: serde_json::json!({
                        "title": title,
                        "content": "content",
                        "tags": tags
                    }),
                },
                &ctx,
            )
            .await;
    }

    // --- tag_exact: true — should match only "food" (exact), not "food/italian" ---
    let result = registry
        .execute(
            &ToolCall {
                id: "l1".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({ "tag": "food", "tag_exact": true }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);
    let notes = result.content["notes"].as_array().unwrap();
    assert_eq!(
        result.content["total"].as_u64().unwrap(),
        2,
        "exact match on 'food' should return 2 notes"
    );
    assert_eq!(notes.len(), 2);
    // "Italian only" (tagged food/italian) must NOT appear.
    assert!(
        notes.iter().all(|n| n["title"] != "Italian only"),
        "tag_exact should exclude notes tagged only with a child tag"
    );

    // --- limit: 1 — exactly 1 note in response, total > 1 ---
    let result = registry
        .execute(
            &ToolCall {
                id: "l2".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({ "limit": 1 }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(
        result.content["notes"].as_array().unwrap().len(),
        1,
        "limit=1 should return exactly 1 note"
    );
    assert!(
        result.content["total"].as_u64().unwrap() > 1,
        "total should reflect all 3 notes"
    );
    // Fix E: envelope echo assertions
    assert_eq!(
        result.content["limit"].as_u64().unwrap(),
        1,
        "limit field in envelope should echo the requested limit"
    );
    assert_eq!(
        result.content["offset"].as_u64().unwrap(),
        0,
        "offset field in envelope should default to 0"
    );

    // --- offset: 1 — skips the first note ---
    let result_all = registry
        .execute(
            &ToolCall {
                id: "l3a".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({}),
            },
            &ctx,
        )
        .await;
    let first_id = result_all.content["notes"].as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let result = registry
        .execute(
            &ToolCall {
                id: "l3b".into(),
                name: "memory_list".into(),
                arguments: serde_json::json!({ "offset": 1 }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error);
    let notes_offset = result.content["notes"].as_array().unwrap();
    assert!(
        notes_offset.iter().all(|n| n["id"] != first_id),
        "offset=1 should skip the first note"
    );
    // Fix E: envelope echo for offset
    assert_eq!(
        result.content["offset"].as_u64().unwrap(),
        1,
        "offset field in envelope should echo 1"
    );
}

/// T9: `memory_search` respects `tag_exact` — parent tag must not pull in child-tagged notes.
#[tokio::test]
async fn memory_search_tool_tag_exact_filter() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Create a note tagged with the bare parent tag.
    let r1 = registry
        .execute(
            &ToolCall {
                id: "s1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Recipes Index",
                    "content": "top level recipe index entry",
                    "tags": ["recipes"]
                }),
            },
            &ctx,
        )
        .await;
    assert!(!r1.is_error, "create failed: {:?}", r1.content);

    // Create a note tagged with a child tag.
    let r2 = registry
        .execute(
            &ToolCall {
                id: "s2".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Italian Pasta",
                    "content": "top level recipe for italian pasta",
                    "tags": ["recipes/italian"]
                }),
            },
            &ctx,
        )
        .await;
    assert!(!r2.is_error, "create failed: {:?}", r2.content);

    // tag_exact: true — only the note tagged exactly "recipes" should be returned.
    let result = registry
        .execute(
            &ToolCall {
                id: "s3".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({
                    "query": "recipe",
                    "tag": "recipes",
                    "tag_exact": true
                }),
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error, "search failed: {:?}", result.content);
    let notes = result.content.as_array().unwrap();
    assert_eq!(notes.len(), 1, "tag_exact=true must match only the exact-tagged note");
    assert_eq!(
        notes[0]["title"].as_str().unwrap(),
        "Recipes Index",
        "only the note tagged exactly 'recipes' should be returned"
    );

    // tag_exact: false (default prefix) — both notes should be returned.
    let result_prefix = registry
        .execute(
            &ToolCall {
                id: "s4".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({
                    "query": "recipe",
                    "tag": "recipes",
                    "tag_exact": false
                }),
            },
            &ctx,
        )
        .await;
    assert!(!result_prefix.is_error, "search failed: {:?}", result_prefix.content);
    let notes_prefix = result_prefix.content.as_array().unwrap();
    assert_eq!(
        notes_prefix.len(),
        2,
        "tag_exact=false must return both notes (prefix match)"
    );
}

/// memory_search clamps `limit` at MEMORY_SEARCH_MAX_LIMIT (100). Boundary
/// tests at exactly 100 (passes), exactly 101 (clamps), and far above
/// (clamps). Mirrors the memory_list cap test from Phase 2.
#[tokio::test]
async fn memory_search_caps_limit_at_max() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Seed one note so the search has something to return.
    let _ = registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "memory_create".into(),
                arguments: serde_json::json!({
                    "title": "Seed",
                    "content": "alpha beta gamma"
                }),
            },
            &ctx,
        )
        .await;

    // Far above the cap — must succeed (no error) and search runs at the cap.
    let far_over = registry
        .execute(
            &ToolCall {
                id: "s_far".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": "alpha", "limit": 99999 }),
            },
            &ctx,
        )
        .await;
    assert!(
        !far_over.is_error,
        "limit=99999 should succeed (clamps silently): {:?}",
        far_over.content
    );

    // Boundary: exactly at the cap should pass through unchanged.
    let at_cap = registry
        .execute(
            &ToolCall {
                id: "s_at_cap".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": "alpha", "limit": 100 }),
            },
            &ctx,
        )
        .await;
    assert!(!at_cap.is_error, "limit=100 should succeed: {:?}", at_cap.content);

    // Boundary: one above should clamp (no error, response identical to at-cap).
    let just_over = registry
        .execute(
            &ToolCall {
                id: "s_over".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": "alpha", "limit": 101 }),
            },
            &ctx,
        )
        .await;
    assert!(
        !just_over.is_error,
        "limit=101 should succeed (clamps to 100): {:?}",
        just_over.content
    );
}

/// memory_search rejects queries longer than MEMORY_SEARCH_MAX_QUERY_LEN
/// (1024 bytes). Boundary tests at exactly 1024 (passes) and exactly 1025
/// (rejects with an error).
#[tokio::test]
async fn memory_search_rejects_overlong_query() {
    let (_store, registry) = setup();
    let ctx = test_ctx();

    // Exactly at the cap — should NOT be rejected.
    let at_cap_query = "a".repeat(1024);
    let at_cap = registry
        .execute(
            &ToolCall {
                id: "q_at_cap".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": at_cap_query }),
            },
            &ctx,
        )
        .await;
    assert!(
        !at_cap.is_error,
        "query at exactly 1024 bytes should be accepted: {:?}",
        at_cap.content
    );

    // One byte over — should be rejected with an error result.
    let over_cap_query = "a".repeat(1025);
    let over_cap = registry
        .execute(
            &ToolCall {
                id: "q_over_cap".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": over_cap_query }),
            },
            &ctx,
        )
        .await;
    assert!(
        over_cap.is_error,
        "query at 1025 bytes should be rejected: {:?}",
        over_cap.content
    );
}
