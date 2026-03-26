use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::CompactionConfig;
use crate::core::event::{ChannelSource, InEvent, OutEvent};
use crate::core::observer::ObserverRegistry;
use crate::history::budget::{ContextBudget, Turn};
use crate::history::compaction::{CompactionService, CompactionState};
use crate::history::store::HistoryStore;
use crate::memory::Memory;
use crate::providers::{Provider, RequestConfig};
use crate::security::{AuthorizationResult, Security};
use crate::tools::{ToolContext, ToolRegistry, ToolResult};
use crate::types::{ChatMessage, ConversationId, ConversationMode, estimate_tokens};

/// Maximum number of tool-call → LLM round-trips before we stop looping.
const MAX_TOOL_ITERATIONS: usize = 10;

/// Object-safe trait for processing inbound events.
/// This wraps the generic `Pipeline<P>` so we can use `dyn PipelineRunner` in main.
pub trait PipelineRunner: Send + Sync {
    fn process<'a>(
        &'a self,
        event: &'a InEvent,
    ) -> Pin<Box<dyn Future<Output = Result<OutEvent>> + Send + 'a>>;
}

/// Configuration for the pipeline's context window budget and generation params.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Maximum tokens the model supports.
    pub model_max_tokens: usize,
    /// Tokens reserved for the model's response.
    pub response_reserve: usize,
    /// Sampling temperature sent to the provider.
    pub temperature: Option<f64>,
    /// Maximum tokens the model may generate per response.
    pub max_response_tokens: Option<usize>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            model_max_tokens: 8192,
            response_reserve: 1024,
            temperature: Some(0.7),
            max_response_tokens: Some(1024),
        }
    }
}

/// Bundled dependencies for pipeline construction, avoiding parameter explosion.
pub struct PipelineDeps {
    pub history_store: Arc<HistoryStore>,
    pub tool_registry: Arc<ToolRegistry>,
    pub memory_store: Arc<dyn Memory>,
    pub security: Arc<Security>,
    pub observers: Arc<ObserverRegistry>,
    pub compaction_config: CompactionConfig,
    pub compaction_state: Arc<CompactionState>,
    pub pipeline_config: PipelineConfig,
}

/// The message processing pipeline.
///
/// Takes an inbound event, loads conversation history, builds a prompt with
/// budget-aware turn selection, calls the LLM provider, executes any tool
/// calls in a loop, persists history, and returns an outbound event.
pub struct Pipeline<P: Provider + 'static> {
    provider: Arc<P>,
    system_prompt: String,
    core_persona: String,
    history_store: Arc<HistoryStore>,
    tool_registry: Arc<ToolRegistry>,
    memory_store: Arc<dyn Memory>,
    security: Arc<Security>,
    observers: Arc<ObserverRegistry>,
    compaction_service: CompactionService<P>,
    pipeline_config: PipelineConfig,
}

impl<P: Provider + 'static> Pipeline<P> {
    /// Create a new pipeline with all dependencies.
    pub fn new(
        provider: Arc<P>,
        system_prompt_path: &Path,
        core_persona_path: &Path,
        deps: PipelineDeps,
    ) -> Result<Self> {
        let system_prompt = if system_prompt_path.exists() {
            std::fs::read_to_string(system_prompt_path).with_context(|| {
                format!(
                    "failed to read system prompt: {}",
                    system_prompt_path.display()
                )
            })?
        } else {
            debug!(path = %system_prompt_path.display(), "system prompt file not found, using default");
            default_system_prompt()
        };

        let core_persona = if core_persona_path.exists() {
            std::fs::read_to_string(core_persona_path).with_context(|| {
                format!(
                    "failed to read core persona: {}",
                    core_persona_path.display()
                )
            })?
        } else {
            debug!(path = %core_persona_path.display(), "core persona file not found, using empty");
            String::new()
        };

        let compaction_prompt = if deps.compaction_config.summary_prompt_path.exists() {
            std::fs::read_to_string(&deps.compaction_config.summary_prompt_path)
                .unwrap_or_else(|_| default_compaction_prompt())
        } else {
            default_compaction_prompt()
        };

        let compaction_service = CompactionService::new(
            Arc::clone(&deps.history_store),
            Arc::clone(&provider),
            deps.compaction_config,
            deps.compaction_state,
            compaction_prompt,
        );

        info!(
            system_prompt_len = system_prompt.len(),
            core_persona_len = core_persona.len(),
            tool_count = deps.tool_registry.tool_count(),
            provider = provider.name(),
            "pipeline initialized"
        );

        Ok(Self {
            provider,
            system_prompt,
            core_persona,
            history_store: deps.history_store,
            tool_registry: deps.tool_registry,
            memory_store: deps.memory_store,
            security: deps.security,
            observers: deps.observers,
            compaction_service,
            pipeline_config: deps.pipeline_config,
        })
    }

    async fn process_impl(&self, event: &InEvent) -> Result<OutEvent> {
        // Observer: message received
        self.observers.notify_message_received(event);

        let conv_id = event.context.conversation_id.clone();

        // Ensure conversation exists in the store.
        self.history_store
            .ensure_conversation(&conv_id, ConversationMode::Shared)
            .context("failed to ensure conversation")?;

        // Build the user message and persist it.
        let user_text = format!(
            "{}: {}",
            event.message.author.display_name, event.message.text
        );
        let user_msg = ChatMessage::user(&user_text);
        let turn_id = self
            .history_store
            .append_message(&conv_id, &user_msg, None)
            .context("failed to append user message")?;

        // Load conversation history (with compaction summary support).
        let (turns, summary_token_overhead) = self.load_history_turns(&conv_id)?;

        // Retrieve relevant memories based on user message.
        let retrieved_memories = self.retrieve_memories(&event.message.text);

        // Build the context budget.
        let system_tokens = estimate_tokens(&self.system_prompt);
        let persona_tokens = estimate_tokens(&self.core_persona);
        let tool_defs = if let Some(ref group_names) = event.tool_groups {
            let groups: Vec<crate::tools::ToolGroup> = group_names
                .iter()
                .filter_map(|name| crate::tools::ToolGroup::from_str_opt(name))
                .collect();
            if groups.is_empty() {
                self.tool_registry.definitions()
            } else {
                self.tool_registry.definitions_for_groups(&groups)
            }
        } else {
            self.tool_registry.definitions()
        };
        let tool_defs_json = serde_json::to_string(&tool_defs).unwrap_or_default();
        let tool_def_tokens = estimate_tokens(&tool_defs_json) + summary_token_overhead;

        let budget = ContextBudget::new(
            self.pipeline_config.model_max_tokens,
            self.pipeline_config.response_reserve,
            system_tokens,
            persona_tokens,
            tool_def_tokens,
        );

        let selection = budget.select_turns(&turns);
        if !selection.evicted.is_empty() {
            debug!(
                evicted = selection.evicted.len(),
                "evicted oldest turns from context window"
            );
        }

        // Assemble the full message array.
        let included_turns: Vec<Turn> = selection.included.iter().map(|t| (*t).clone()).collect();
        let assembled = budget.assemble(
            &self.system_prompt,
            &self.core_persona,
            &included_turns,
            &retrieved_memories,
        );

        // Add channel context to system prompt.
        let channel_context = format!(
            "\nYou are responding in: {:?} (conversation: {:?})",
            event.source, event.context.conversation_id
        );

        // Inject channel context into first message (types are now unified, no conversion needed).
        let mut provider_messages = assembled;
        if let Some(first) = provider_messages.first_mut() {
            first.content.push_str(&channel_context);
        }

        let config = RequestConfig {
            temperature: self.pipeline_config.temperature.map(|t| t as f32),
            max_tokens: self.pipeline_config.max_response_tokens.map(|n| n as u32),
            ..Default::default()
        };

        // Determine whether this event is system-originated (bypasses authorization).
        let is_system_event = event.source == ChannelSource::Scheduler;

        // Tool execution loop.
        let mut iterations = 0;
        let response = loop {
            debug!(
                message_count = provider_messages.len(),
                iteration = iterations,
                "calling LLM provider"
            );

            // Observer: LLM request
            self.observers
                .notify_llm_request(&provider_messages, &tool_defs);

            let llm_start = Instant::now();
            let response = self
                .provider
                .chat(provider_messages.clone(), &tool_defs, &config)
                .await;
            let llm_duration = llm_start.elapsed();

            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    self.observers.notify_error(&e);
                    return Err(e);
                }
            };

            // Observer: LLM response
            self.observers
                .notify_llm_response(&response, llm_duration);

            debug!(
                usage.input = response.usage.input_tokens,
                usage.output = response.usage.output_tokens,
                has_text = response.text.is_some(),
                tool_calls = response.tool_calls.len(),
                "LLM response received"
            );

            if response.tool_calls.is_empty() {
                break response;
            }

            if iterations >= MAX_TOOL_ITERATIONS {
                warn!(
                    "tool execution loop hit max iterations ({MAX_TOOL_ITERATIONS}), stopping"
                );
                break response;
            }

            // Append assistant message with tool calls to history and provider messages.
            let assistant_text = response.text.clone().unwrap_or_default();

            let assistant_msg = ChatMessage::assistant_with_tool_calls(
                &assistant_text,
                response.tool_calls.clone(),
            );
            self.history_store
                .append_message(&conv_id, &assistant_msg, Some(&turn_id))
                .context("failed to append assistant tool-call message")?;

            // Add assistant message to provider messages (same type now).
            provider_messages.push(assistant_msg);

            // Execute each tool call and collect results.
            for tc in &response.tool_calls {
                let tool_ctx = ToolContext {
                    call_id: tc.id.clone(),
                    author_id: event.message.author.id.clone(),
                    conversation_id: conv_id.to_string(),
                    channel_source: event.source.to_string(),
                };
                // Observer: tool call
                self.observers.notify_tool_call(tc, &tool_ctx);

                // Security: check authorization (system events bypass)
                if !is_system_event {
                    if let AuthorizationResult::Denied {
                        tool_name,
                        user_id,
                    } = self
                        .security
                        .check_authorization(&tc.name, &event.message.author.id)
                    {
                        let denied_result = ToolResult {
                            call_id: tc.id.clone(),
                            content: serde_json::json!({
                                "error": format!(
                                    "authorization denied: user '{}' is not allowed to call '{}'",
                                    user_id, tool_name
                                )
                            }),
                            is_error: true,
                        };
                        let result_content =
                            serde_json::to_string(&denied_result.content).unwrap_or_default();

                        // Observer: tool result (authorization denied)
                        self.observers.notify_tool_result(
                            tc,
                            &denied_result,
                            std::time::Duration::ZERO,
                        );

                        let tool_msg = ChatMessage::tool_result(&tc.id, &result_content);
                        self.history_store
                            .append_message(&conv_id, &tool_msg, Some(&turn_id))
                            .context("failed to append tool result")?;

                        provider_messages.push(tool_msg);
                        continue;
                    }
                }

                let tool_start = Instant::now();
                let result = self.tool_registry.execute(tc, &tool_ctx).await;
                let tool_duration = tool_start.elapsed();

                // Observer: tool result
                self.observers
                    .notify_tool_result(tc, &result, tool_duration);

                let result_content = serde_json::to_string(&result.content).unwrap_or_default();

                // Persist tool result to history.
                let tool_msg = ChatMessage::tool_result(&tc.id, &result_content);
                self.history_store
                    .append_message(&conv_id, &tool_msg, Some(&turn_id))
                    .context("failed to append tool result")?;

                // Add to provider messages.
                provider_messages.push(tool_msg);
            }

            iterations += 1;
        };

        // Persist the final assistant response to history.
        let response_text = response.text.clone().unwrap_or_default();
        if !response_text.is_empty() {
            let final_msg = ChatMessage::assistant(&response_text);
            self.history_store
                .append_message(&conv_id, &final_msg, Some(&turn_id))
                .context("failed to append final assistant message")?;
        }

        // Check if compaction should be triggered.
        let history_tokens = self
            .history_store
            .total_history_tokens(&conv_id)
            .unwrap_or(0);
        let history_budget = budget.available_for_history();
        self.compaction_service
            .maybe_trigger(&conv_id, history_tokens, history_budget);

        self.build_out_event(event, response)
    }

    /// Load conversation history as turns, incorporating any compaction summary.
    fn load_history_turns(&self, conv_id: &ConversationId) -> Result<(Vec<Turn>, usize)> {
        let summary = self.history_store.load_summary(conv_id)?;

        let messages = match &summary {
            Some(s) => self
                .history_store
                .load_messages_after(conv_id, s.compacted_up_to)?,
            None => self.history_store.load_messages(conv_id)?,
        };

        // Build turns from stored messages (group by turn_id).
        let mut turns: Vec<Turn> = Vec::new();
        let mut summary_overhead = 0usize;

        // If we have a summary, create a synthetic turn for it.
        if let Some(ref s) = summary {
            turns.push(Turn {
                turn_id: "__summary__".to_string(),
                messages: vec![ChatMessage::system(format!(
                    "## Conversation Summary\n\n{}",
                    s.summary_text
                ))],
                total_tokens: s.token_estimate,
            });
            summary_overhead = s.token_estimate;
        }

        // Group messages by turn_id.
        let mut current_turn_id: Option<String> = None;
        let mut current_messages: Vec<ChatMessage> = Vec::new();
        let mut current_tokens: usize = 0;

        for stored in &messages {
            if current_turn_id.as_deref() != Some(&stored.turn_id) {
                // Flush previous turn.
                if let Some(tid) = current_turn_id.take() {
                    turns.push(Turn {
                        turn_id: tid,
                        messages: std::mem::take(&mut current_messages),
                        total_tokens: current_tokens,
                    });
                    current_tokens = 0;
                }
                current_turn_id = Some(stored.turn_id.clone());
            }
            current_messages.push(stored.to_chat_message());
            current_tokens += stored.token_estimate;
        }

        // Flush last turn.
        if let Some(tid) = current_turn_id {
            turns.push(Turn {
                turn_id: tid,
                messages: current_messages,
                total_tokens: current_tokens,
            });
        }

        Ok((turns, summary_overhead))
    }

    /// Search memory for notes relevant to the user's message.
    fn retrieve_memories(&self, query: &str) -> Vec<String> {
        match self.memory_store.search_notes(query, 5) {
            Ok(notes) => notes
                .into_iter()
                .map(|n: crate::memory::Note| format!("**{}**: {}", n.title, n.content))
                .collect(),
            Err(e) => {
                warn!(error = %e, "memory retrieval failed, continuing without memories");
                vec![]
            }
        }
    }

    fn build_out_event(
        &self,
        event: &InEvent,
        response: crate::providers::LlmResponse,
    ) -> Result<OutEvent> {
        Ok(OutEvent {
            target: event.source.clone(),
            channel_id: event.context.channel_id.clone(),
            text: response.text.clone(),
            reply_to: Some(event.message.id.clone()),
        })
    }
}

impl<P: Provider + 'static> PipelineRunner for Pipeline<P> {
    fn process<'a>(
        &'a self,
        event: &'a InEvent,
    ) -> Pin<Box<dyn Future<Output = Result<OutEvent>> + Send + 'a>> {
        Box::pin(self.process_impl(event))
    }
}

fn default_system_prompt() -> String {
    "You are Aurora — a digital person, not an AI assistant. \
     You have your own personality, interests, and evolving memory. \
     Respond naturally as yourself. Your core persona is in memory/core.md."
        .to_string()
}

fn default_compaction_prompt() -> String {
    "Summarize the following conversation, preserving key facts, decisions, \
     emotional context, and any commitments made. Be concise but thorough."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event::{
        Author, ChannelSource, ConversationId, InEvent, Message, MessageContext, MessageId,
    };
    use crate::core::observer::ObserverRegistry;
    use crate::providers::{LlmResponse, TokenUsage};
    use crate::tools::{ToolCall, ToolDef};
    use crate::security::Security;
    use std::sync::Mutex;

    /// A mock provider that returns a configurable sequence of responses.
    struct MockProvider {
        responses: Mutex<Vec<LlmResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl crate::providers::Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn chat(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: &[ToolDef],
            _config: &crate::providers::RequestConfig,
        ) -> anyhow::Result<LlmResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(LlmResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                    usage: TokenUsage::default(),
                })
            } else {
                Ok(responses.remove(0))
            }
        }

        fn estimate_tokens(&self, text: &str) -> usize {
            text.len() / 4
        }
    }

    fn make_test_event(source: ChannelSource, author_id: &str) -> InEvent {
        InEvent {
            source: source.clone(),
            message: Message {
                id: MessageId("msg-1".into()),
                author: Author {
                    id: author_id.into(),
                    display_name: "Tester".into(),
                },
                text: "hello".into(),
                timestamp: chrono::Utc::now(),
                mentions_bot: false,
            },
            context: MessageContext {
                conversation_id: ConversationId::Dm {
                    channel_type: source,
                    user_id: author_id.into(),
                },
                channel_id: "test-chan".into(),
                reply_to: None,
            },
            tool_groups: None,
        }
    }

    fn make_test_security() -> Arc<Security> {
        let config = crate::config::RateLimitConfig::default();
        let tmp = std::env::temp_dir().join("borealis_test_pipeline");
        let _ = std::fs::create_dir_all(&tmp);
        let mut security = Security::new(&config, tmp, ["admin".to_string()]);
        security.register_restricted("bash_exec");
        Arc::new(security)
    }

    fn make_test_pipeline(
        responses: Vec<LlmResponse>,
        security: Arc<Security>,
    ) -> Pipeline<MockProvider> {
        let provider = Arc::new(MockProvider::new(responses));
        let db = Arc::new(Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        ));
        {
            let conn = db.lock().unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;",
            )
            .unwrap();
        }
        crate::history::schema::initialize(&db.lock().unwrap()).unwrap();
        let history_store = Arc::new(crate::history::store::HistoryStore::new(Arc::clone(&db)));

        let tmp_persona = std::env::temp_dir().join("borealis_test_core.md");
        std::fs::write(&tmp_persona, "test persona").unwrap();
        let memory_store: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(Arc::clone(&db), tmp_persona.clone()).unwrap());

        let tool_registry = Arc::new(crate::tools::ToolRegistry::new());
        let observers = Arc::new(ObserverRegistry::new());

        let deps = PipelineDeps {
            history_store,
            tool_registry,
            memory_store,
            security,
            observers,
            compaction_config: crate::config::CompactionConfig::default(),
            compaction_state: Arc::new(crate::history::compaction::CompactionState::new()),
            pipeline_config: PipelineConfig::default(),
        };

        let sys_path = std::path::Path::new("/nonexistent/system_prompt.md");
        let persona_path = &tmp_persona;

        Pipeline::new(provider, sys_path, persona_path, deps).unwrap()
    }

    #[tokio::test]
    async fn authorization_denies_restricted_tool_for_unauthorized_user() {
        let security = make_test_security();

        // Provider returns a response with a tool call to bash_exec, then a final text response.
        let responses = vec![
            LlmResponse {
                text: Some("Let me run that.".into()),
                tool_calls: vec![ToolCall {
                    id: "tc_1".into(),
                    name: "bash_exec".into(),
                    arguments: serde_json::json!({"command": "echo hi"}),
                }],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            },
            LlmResponse {
                text: Some("I was denied.".into()),
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
            },
        ];

        let pipeline = make_test_pipeline(responses, security);
        let event = make_test_event(ChannelSource::Cli, "random_user");

        let result = pipeline.process_impl(&event).await.unwrap();
        assert_eq!(result.text, Some("I was denied.".into()));
    }

    #[tokio::test]
    async fn authorization_allows_restricted_tool_for_authorized_user() {
        let security = make_test_security();

        // Provider returns a tool call then a final text.
        // For authorized user, the tool will be executed (it won't exist in registry,
        // so it returns "unknown tool" — but the point is it wasn't denied by authorization).
        let responses = vec![
            LlmResponse {
                text: Some("Running.".into()),
                tool_calls: vec![ToolCall {
                    id: "tc_1".into(),
                    name: "bash_exec".into(),
                    arguments: serde_json::json!({"command": "echo hi"}),
                }],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            },
            LlmResponse {
                text: Some("Done.".into()),
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
            },
        ];

        let pipeline = make_test_pipeline(responses, security);
        let event = make_test_event(ChannelSource::Cli, "admin");

        let result = pipeline.process_impl(&event).await.unwrap();
        // Should reach final response (tool was allowed but not found, then LLM responded)
        assert_eq!(result.text, Some("Done.".into()));
    }

    #[tokio::test]
    async fn scheduler_events_bypass_authorization() {
        let security = make_test_security();

        let responses = vec![
            LlmResponse {
                text: Some("System task.".into()),
                tool_calls: vec![ToolCall {
                    id: "tc_1".into(),
                    name: "bash_exec".into(),
                    arguments: serde_json::json!({"command": "echo hi"}),
                }],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            },
            LlmResponse {
                text: Some("System done.".into()),
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
            },
        ];

        let pipeline = make_test_pipeline(responses, security);
        // Use Scheduler source — even with "random_user" it should bypass authorization.
        let event = make_test_event(ChannelSource::Scheduler, "random_user");

        let result = pipeline.process_impl(&event).await.unwrap();
        assert_eq!(result.text, Some("System done.".into()));
    }
}
