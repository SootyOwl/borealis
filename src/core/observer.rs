use std::time::Duration;

use crate::core::event::InEvent;
use crate::providers::{ChatMessage, LlmResponse, ToolDef as ProviderToolDef};
use crate::tools::{ToolCall, ToolContext, ToolResult};

// ---------------------------------------------------------------------------
// Observer trait
// ---------------------------------------------------------------------------

/// Lifecycle observer for pipeline events.
///
/// All methods have default no-op implementations so that concrete observers
/// only need to override the hooks they care about.
pub trait Observer: Send + Sync {
    /// Called when an inbound event is received from a channel adapter.
    fn on_message_received(&self, _event: &InEvent) {}

    /// Called just before an LLM request is sent.
    fn on_llm_request(&self, _messages: &[ChatMessage], _tools: &[ProviderToolDef]) {}

    /// Called when an LLM response is received.
    fn on_llm_response(&self, _response: &LlmResponse, _duration: Duration) {}

    /// Called when a tool is about to be executed.
    fn on_tool_call(&self, _call: &ToolCall, _ctx: &ToolContext) {}

    /// Called after a tool finishes executing.
    fn on_tool_result(&self, _call: &ToolCall, _result: &ToolResult, _duration: Duration) {}

    /// Called when an error occurs in the pipeline.
    fn on_error(&self, _error: &anyhow::Error) {}
}

// ---------------------------------------------------------------------------
// ObserverRegistry
// ---------------------------------------------------------------------------

/// Registry that holds observers and broadcasts lifecycle notifications.
pub struct ObserverRegistry {
    observers: Vec<Box<dyn Observer>>,
}

impl ObserverRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            observers: Vec::new(),
        }
    }

    /// Register an observer.
    pub fn register(&mut self, observer: Box<dyn Observer>) {
        self.observers.push(observer);
    }

    /// Number of registered observers.
    pub fn len(&self) -> usize {
        self.observers.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.observers.is_empty()
    }

    // -- notification helpers -----------------------------------------------

    pub fn notify_message_received(&self, event: &InEvent) {
        for obs in &self.observers {
            obs.on_message_received(event);
        }
    }

    pub fn notify_llm_request(&self, messages: &[ChatMessage], tools: &[ProviderToolDef]) {
        for obs in &self.observers {
            obs.on_llm_request(messages, tools);
        }
    }

    pub fn notify_llm_response(&self, response: &LlmResponse, duration: Duration) {
        for obs in &self.observers {
            obs.on_llm_response(response, duration);
        }
    }

    pub fn notify_tool_call(&self, call: &ToolCall, ctx: &ToolContext) {
        for obs in &self.observers {
            obs.on_tool_call(call, ctx);
        }
    }

    pub fn notify_tool_result(&self, call: &ToolCall, result: &ToolResult, duration: Duration) {
        for obs in &self.observers {
            obs.on_tool_result(call, result, duration);
        }
    }

    pub fn notify_error(&self, error: &anyhow::Error) {
        for obs in &self.observers {
            obs.on_error(error);
        }
    }
}

impl Default for ObserverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Inventory-based observer registration
// ---------------------------------------------------------------------------

/// A self-registering observer factory.
///
/// Each observer module submits one of these via `inventory::submit!`.
/// At startup, `build_observer_registry()` iterates them to construct an
/// `ObserverRegistry` with all registered observers.
pub struct ObserverRegistration {
    /// Observer name (e.g. "tracing", "metrics").
    pub name: &'static str,
    /// Build an observer instance.
    pub build_fn: fn() -> Box<dyn Observer>,
}

inventory::collect!(ObserverRegistration);

/// Build an `ObserverRegistry` populated with all inventory-registered observers.
pub fn build_observer_registry() -> ObserverRegistry {
    let mut registry = ObserverRegistry::new();
    for reg in inventory::iter::<ObserverRegistration> {
        tracing::debug!(observer = reg.name, "registering observer");
        registry.register((reg.build_fn)());
    }
    registry
}

// ---------------------------------------------------------------------------
// TracingObserver
// ---------------------------------------------------------------------------

/// An [`Observer`] that emits structured log events via the `tracing` crate.
pub struct TracingObserver;

inventory::submit! {
    ObserverRegistration {
        name: "tracing",
        build_fn: || Box::new(TracingObserver),
    }
}

impl Observer for TracingObserver {
    fn on_llm_response(&self, response: &LlmResponse, duration: Duration) {
        tracing::info!(
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            tool_call_count = response.tool_calls.len(),
            duration_ms = duration.as_millis() as u64,
            "LLM response received"
        );
    }

    fn on_tool_result(&self, call: &ToolCall, result: &ToolResult, duration: Duration) {
        tracing::info!(
            tool_name = %call.name,
            is_error = result.is_error,
            duration_ms = duration.as_millis() as u64,
            "Tool execution completed"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A test observer that counts how many times each hook is called.
    struct CountingObserver {
        llm_response_count: Arc<AtomicUsize>,
        tool_result_count: Arc<AtomicUsize>,
        error_count: Arc<AtomicUsize>,
    }

    impl CountingObserver {
        fn new() -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let llm = Arc::new(AtomicUsize::new(0));
            let tool = Arc::new(AtomicUsize::new(0));
            let err = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    llm_response_count: Arc::clone(&llm),
                    tool_result_count: Arc::clone(&tool),
                    error_count: Arc::clone(&err),
                },
                llm,
                tool,
                err,
            )
        }
    }

    impl Observer for CountingObserver {
        fn on_llm_response(&self, _response: &LlmResponse, _duration: Duration) {
            self.llm_response_count.fetch_add(1, Ordering::SeqCst);
        }

        fn on_tool_result(&self, _call: &ToolCall, _result: &ToolResult, _duration: Duration) {
            self.tool_result_count.fetch_add(1, Ordering::SeqCst);
        }

        fn on_error(&self, _error: &anyhow::Error) {
            self.error_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn registry_starts_empty() {
        let reg = ObserverRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn register_and_len() {
        let mut reg = ObserverRegistry::new();
        let (obs, _, _, _) = CountingObserver::new();
        reg.register(Box::new(obs));
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    fn make_tool_call() -> ToolCall {
        ToolCall {
            id: "tc_1".into(),
            name: "test_tool".into(),
            arguments: serde_json::json!({}),
        }
    }

    fn make_tool_result() -> ToolResult {
        ToolResult {
            call_id: "tc_1".into(),
            content: serde_json::json!("ok"),
            is_error: false,
        }
    }

    fn make_llm_response() -> LlmResponse {
        LlmResponse {
            text: Some("hello".into()),
            tool_calls: vec![],
            usage: crate::providers::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }
    }

    #[test]
    fn notify_llm_response_reaches_observers() {
        let mut reg = ObserverRegistry::new();
        let (obs, llm_count, _, _) = CountingObserver::new();
        reg.register(Box::new(obs));

        let resp = make_llm_response();
        reg.notify_llm_response(&resp, Duration::from_millis(100));
        reg.notify_llm_response(&resp, Duration::from_millis(200));

        assert_eq!(llm_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn notify_tool_result_reaches_observers() {
        let mut reg = ObserverRegistry::new();
        let (obs, _, tool_count, _) = CountingObserver::new();
        reg.register(Box::new(obs));

        let call = make_tool_call();
        let result = make_tool_result();
        reg.notify_tool_result(&call, &result, Duration::from_millis(50));

        assert_eq!(tool_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn notify_error_reaches_observers() {
        let mut reg = ObserverRegistry::new();
        let (obs, _, _, err_count) = CountingObserver::new();
        reg.register(Box::new(obs));

        let err = anyhow::anyhow!("something went wrong");
        reg.notify_error(&err);

        assert_eq!(err_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn multiple_observers_all_notified() {
        let mut reg = ObserverRegistry::new();
        let (obs1, llm1, _, _) = CountingObserver::new();
        let (obs2, llm2, _, _) = CountingObserver::new();
        reg.register(Box::new(obs1));
        reg.register(Box::new(obs2));

        let resp = make_llm_response();
        reg.notify_llm_response(&resp, Duration::from_millis(100));

        assert_eq!(llm1.load(Ordering::SeqCst), 1);
        assert_eq!(llm2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn default_no_op_observer_compiles() {
        // A struct that implements Observer with all defaults — should compile fine.
        struct NoOpObserver;
        impl Observer for NoOpObserver {}

        let mut reg = ObserverRegistry::new();
        reg.register(Box::new(NoOpObserver));

        // Calling all notify methods should not panic.
        let resp = make_llm_response();
        let call = make_tool_call();
        let result = make_tool_result();
        let err = anyhow::anyhow!("test");

        reg.notify_llm_response(&resp, Duration::from_millis(1));
        reg.notify_tool_call(&call, &ToolContext {
            author_id: "u1".into(),
            conversation_id: "c1".into(),
            channel_source: "test".into(),
        });
        reg.notify_tool_result(&call, &result, Duration::from_millis(1));
        reg.notify_error(&err);
    }
}
