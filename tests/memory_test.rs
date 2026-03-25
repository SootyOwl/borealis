use std::sync::{Arc, Mutex};

use borealis::memory::MemoryStore;
use borealis::tools::{ToolCall, ToolContext, ToolRegistry, register_memory_tools};
use rusqlite::Connection;

fn test_ctx() -> ToolContext {
    ToolContext {
        author_id: "test_user".to_string(),
        conversation_id: "test_conv".to_string(),
        channel_source: "cli".to_string(),
    }
}

fn setup() -> (MemoryStore, ToolRegistry) {
    let conn = Connection::open_in_memory().unwrap();
    let conn = Arc::new(Mutex::new(conn));
    let tmp = std::env::temp_dir().join(format!("borealis_test_core_{}.md", std::process::id()));
    std::fs::write(&tmp, "# Aurora\nI am Aurora, a test persona.").unwrap();

    let store = MemoryStore::new(conn, tmp).unwrap();
    let mut registry = ToolRegistry::new();
    register_memory_tools(&mut registry, store.clone());
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

    // Search by tag
    let result = registry
        .execute(
            &ToolCall {
                id: "s3".into(),
                name: "memory_search".into(),
                arguments: serde_json::json!({ "query": "code" }),
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
    let notes = result.content.as_array().unwrap();
    assert_eq!(notes.len(), 2);

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
    assert_eq!(result.content.as_array().unwrap().len(), 3);
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

/// All 8 tools are registered
#[tokio::test]
async fn all_tools_registered() {
    let (_store, registry) = setup();

    assert_eq!(registry.tool_count(), 9);
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
    ] {
        assert!(registry.has_tool(name), "missing tool: {name}");
    }

    let defs = registry.definitions();
    assert_eq!(defs.len(), 9);
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
