mod channel_tools;
mod computer_tools;
mod history_tools;
mod memory_tools;
mod web_tools;

pub use channel_tools::register_discord_channel_tools;
pub use computer_tools::register_computer_tools;
pub use history_tools::register_history_tools;
pub use memory_tools::register_memory_tools;
pub use web_tools::register_web_tools;

use std::collections::HashMap;

/// Identifies a functional group of tools.
///
/// Used for per-event tool filtering: scheduler events can restrict which
/// tool groups are available via the `tools` config field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolGroup {
    Memory,
    Computer,
    Web,
    Channel,
}

impl ToolGroup {
    /// Parse a tool group from a string (case-insensitive).
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "memory" => Some(Self::Memory),
            "computer" => Some(Self::Computer),
            "web" => Some(Self::Web),
            "channel" => Some(Self::Channel),
            _ => None,
        }
    }
}

/// Describes a tool that the LLM can call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A tool call from the LLM.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Result of executing a tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: serde_json::Value,
    pub is_error: bool,
}

/// Context passed to tools for authorization and routing.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub author_id: String,
    pub conversation_id: String,
    pub channel_source: String,
}

/// Trait for tools. Rust 2024 edition — native async fn in traits.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDef;
    fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> impl std::future::Future<Output = ToolResult> + Send;
}

/// Registry that maps tool names to instances, with optional group tagging.
pub struct ToolRegistry {
    handlers: HashMap<String, Box<dyn ErasedTool>>,
    /// Maps tool names to their group, if registered with one.
    groups: HashMap<String, ToolGroup>,
}

/// Object-safe wrapper around Tool to allow dynamic dispatch.
trait ErasedTool: Send + Sync {
    fn definition(&self) -> ToolDef;
    fn execute_boxed<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>>;
}

impl<T: Tool> ErasedTool for T {
    fn definition(&self) -> ToolDef {
        Tool::definition(self)
    }

    fn execute_boxed<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(Tool::execute(self, args, ctx))
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            groups: HashMap::new(),
        }
    }

    pub fn register<T: Tool + 'static>(&mut self, handler: T) {
        self.handlers
            .insert(handler.name().to_string(), Box::new(handler));
    }

    /// Register a tool and tag it with a group for filtering.
    pub fn register_with_group<T: Tool + 'static>(&mut self, handler: T, group: ToolGroup) {
        let name = handler.name().to_string();
        self.groups.insert(name.clone(), group);
        self.handlers.insert(name, Box::new(handler));
    }

    pub fn definitions(&self) -> Vec<ToolDef> {
        self.handlers.values().map(|h| h.definition()).collect()
    }

    /// Return definitions filtered to only include tools from the specified groups.
    /// Tools registered without a group are excluded.
    pub fn definitions_for_groups(&self, groups: &[ToolGroup]) -> Vec<ToolDef> {
        self.handlers
            .iter()
            .filter(|(name, _)| {
                self.groups
                    .get(name.as_str())
                    .is_some_and(|g| groups.contains(g))
            })
            .map(|(_, h)| h.definition())
            .collect()
    }

    pub async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        match self.handlers.get(&call.name) {
            Some(handler) => handler.execute_boxed(call.arguments.clone(), ctx).await,
            None => ToolResult {
                call_id: call.id.clone(),
                content: serde_json::json!({
                    "error": format!("unknown tool: {}", call.name)
                }),
                is_error: true,
            },
        }
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    pub fn tool_count(&self) -> usize {
        self.handlers.len()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn definition(&self) -> ToolDef {
            ToolDef {
                name: "echo".to_string(),
                description: "Echoes input".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
                    }
                }),
            }
        }

        async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: "test".to_string(),
                content: args,
                is_error: false,
            }
        }
    }

    #[test]
    fn registry_register_and_lookup() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);

        assert!(registry.has_tool("echo"));
        assert!(!registry.has_tool("nonexistent"));
        assert_eq!(registry.tool_count(), 1);
        assert_eq!(registry.definitions().len(), 1);
        assert_eq!(registry.definitions()[0].name, "echo");
    }

    #[tokio::test]
    async fn registry_execute_known_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);

        let call = ToolCall {
            id: "call_1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"text": "hello"}),
        };
        let ctx = ToolContext {
            author_id: "user1".to_string(),
            conversation_id: "conv1".to_string(),
            channel_source: "cli".to_string(),
        };

        let result = registry.execute(&call, &ctx).await;
        assert!(!result.is_error);
        assert_eq!(result.content, serde_json::json!({"text": "hello"}));
    }

    #[tokio::test]
    async fn registry_execute_unknown_tool() {
        let registry = ToolRegistry::new();
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "nonexistent".to_string(),
            arguments: serde_json::json!({}),
        };
        let ctx = ToolContext {
            author_id: "user1".to_string(),
            conversation_id: "conv1".to_string(),
            channel_source: "cli".to_string(),
        };

        let result = registry.execute(&call, &ctx).await;
        assert!(result.is_error);
    }

    /// A second tool for group filtering tests.
    struct PingTool;

    impl Tool for PingTool {
        fn name(&self) -> &str {
            "ping"
        }

        fn definition(&self) -> ToolDef {
            ToolDef {
                name: "ping".to_string(),
                description: "Returns pong".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: "test".to_string(),
                content: serde_json::json!("pong"),
                is_error: false,
            }
        }
    }

    #[test]
    fn definitions_for_groups_filters_correctly() {
        let mut registry = ToolRegistry::new();
        registry.register_with_group(EchoTool, ToolGroup::Memory);
        registry.register_with_group(PingTool, ToolGroup::Computer);

        // Request only Memory group — should get echo but not ping.
        let memory_defs = registry.definitions_for_groups(&[ToolGroup::Memory]);
        assert_eq!(memory_defs.len(), 1);
        assert_eq!(memory_defs[0].name, "echo");

        // Request only Computer group — should get ping but not echo.
        let computer_defs = registry.definitions_for_groups(&[ToolGroup::Computer]);
        assert_eq!(computer_defs.len(), 1);
        assert_eq!(computer_defs[0].name, "ping");

        // Request both — should get both.
        let both_defs =
            registry.definitions_for_groups(&[ToolGroup::Memory, ToolGroup::Computer]);
        assert_eq!(both_defs.len(), 2);

        // Request a group with no tools — should get nothing.
        let web_defs = registry.definitions_for_groups(&[ToolGroup::Web]);
        assert!(web_defs.is_empty());

        // Empty group list — should get nothing.
        let empty_defs = registry.definitions_for_groups(&[]);
        assert!(empty_defs.is_empty());
    }

    #[test]
    fn definitions_for_groups_excludes_ungrouped_tools() {
        let mut registry = ToolRegistry::new();
        // Register without a group.
        registry.register(EchoTool);
        // Register with a group.
        registry.register_with_group(PingTool, ToolGroup::Computer);

        // definitions() returns all tools.
        assert_eq!(registry.definitions().len(), 2);

        // definitions_for_groups returns only grouped tools.
        let computer_defs = registry.definitions_for_groups(&[ToolGroup::Computer]);
        assert_eq!(computer_defs.len(), 1);
        assert_eq!(computer_defs[0].name, "ping");

        // Ungrouped echo is excluded from group filtering.
        let all_groups = registry.definitions_for_groups(&[
            ToolGroup::Memory,
            ToolGroup::Computer,
            ToolGroup::Web,
            ToolGroup::Channel,
        ]);
        assert_eq!(all_groups.len(), 1);
        assert_eq!(all_groups[0].name, "ping");
    }
}
