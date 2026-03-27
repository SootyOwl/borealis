use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::config::CompactionConfig;
use crate::core::event::{ChannelSource, InEvent, OutEvent};
use crate::core::observer::ObserverRegistry;
use crate::history::budget::{ContextBudget, Turn};
use crate::history::compaction::{CompactionService, CompactionState};
use crate::history::store::HistoryStore;
use crate::memory::Memory;
use crate::providers::retry::RetryError;
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
    pub temperature: Option<f32>,
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
    pub llm_semaphore: Arc<Semaphore>,
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
    llm_semaphore: Arc<Semaphore>,
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
            llm_semaphore: deps.llm_semaphore,
        })
    }

    async fn process_impl(&self, event: &InEvent) -> Result<OutEvent> {
        // Observer: message received
        self.observers.notify_message_received(event);

        let conv_id = event.context.conversation_id.clone();

        // Ensure conversation exists in the store.
        {
            let store = Arc::clone(&self.history_store);
            let cid = conv_id.clone();
            tokio::task::spawn_blocking(move || {
                store.ensure_conversation(&cid, ConversationMode::Shared)
            })
            .await
            .map_err(|e| anyhow::anyhow!("task join error: {e}"))??;
        }

        // Build the user message and persist it.
        let user_text = format!(
            "{}: {}",
            event.message.author.display_name, event.message.text
        );
        let user_msg = ChatMessage::user(&user_text);
        let turn_id = {
            let store = Arc::clone(&self.history_store);
            let cid = conv_id.clone();
            let msg = user_msg.clone();
            tokio::task::spawn_blocking(move || store.append_message(&cid, &msg, None))
                .await
                .map_err(|e| anyhow::anyhow!("task join error: {e}"))?
                .context("failed to append user message")?
        };

        // Load conversation history (with compaction summary support).
        let (turns, summary_token_overhead) = self.load_history_turns(&conv_id).await?;

        // Retrieve relevant memories based on user message.
        let retrieved_memories = self.retrieve_memories(&event.message.text).await;

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
            temperature: self.pipeline_config.temperature,
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

            let (response, llm_duration) = self
                .call_llm_with_400_recovery(
                    &mut provider_messages,
                    &tool_defs,
                    &config,
                    &included_turns,
                    &retrieved_memories,
                    &channel_context,
                )
                .await?;

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
            {
                let store = Arc::clone(&self.history_store);
                let cid = conv_id.clone();
                let msg = assistant_msg.clone();
                let tid = turn_id.clone();
                tokio::task::spawn_blocking(move || {
                    store.append_message(&cid, &msg, Some(&tid))
                })
                .await
                .map_err(|e| anyhow::anyhow!("task join error: {e}"))??;
            }

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
                        {
                            let store = Arc::clone(&self.history_store);
                            let cid = conv_id.clone();
                            let msg = tool_msg.clone();
                            let tid = turn_id.clone();
                            tokio::task::spawn_blocking(move || {
                                store.append_message(&cid, &msg, Some(&tid))
                            })
                            .await
                            .map_err(|e| anyhow::anyhow!("task join error: {e}"))??;
                        }

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
                {
                    let store = Arc::clone(&self.history_store);
                    let cid = conv_id.clone();
                    let msg = tool_msg.clone();
                    let tid = turn_id.clone();
                    tokio::task::spawn_blocking(move || {
                        store.append_message(&cid, &msg, Some(&tid))
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("task join error: {e}"))??;
                }

                // Add to provider messages.
                provider_messages.push(tool_msg);
            }

            iterations += 1;
        };

        // Persist the final assistant response to history.
        // Only persist if the loop exited because tool_calls was empty (normal exit).
        // If it exited due to max iterations, the assistant message was already persisted
        // inside the loop, so persisting again would create a duplicate.
        let response_text = response.text.clone().unwrap_or_default();
        if !response_text.is_empty() && response.tool_calls.is_empty() {
            let final_msg = ChatMessage::assistant(&response_text);
            let store = Arc::clone(&self.history_store);
            let cid = conv_id.clone();
            tokio::task::spawn_blocking(move || {
                store.append_message(&cid, &final_msg, Some(&turn_id))
            })
            .await
            .map_err(|e| anyhow::anyhow!("task join error: {e}"))??;
        }

        // Check if compaction should be triggered.
        let history_tokens = {
            let store = Arc::clone(&self.history_store);
            let cid = conv_id.clone();
            tokio::task::spawn_blocking(move || store.total_history_tokens(&cid))
                .await
                .map_err(|e| anyhow::anyhow!("task join error: {e}"))?
                .unwrap_or(0)
        };
        let history_budget = budget.available_for_history();
        self.compaction_service
            .maybe_trigger(&conv_id, history_tokens, history_budget);

        self.build_out_event(event, response)
    }

    /// Call the LLM provider with 400-status recovery.
    ///
    /// On HTTP 400 (context too large, invalid request, etc.):
    /// - First retry: evict oldest half of non-fixed turns and retry.
    /// - Second failure: fall back to system prompt + core persona + current message only.
    async fn call_llm_with_400_recovery(
        &self,
        provider_messages: &mut Vec<ChatMessage>,
        tool_defs: &[crate::tools::ToolDef],
        config: &RequestConfig,
        included_turns: &[Turn],
        retrieved_memories: &[String],
        channel_context: &str,
    ) -> Result<(crate::providers::LlmResponse, std::time::Duration)> {
        let _permit = self
            .llm_semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("LLM semaphore closed"))?;

        let llm_start = Instant::now();
        let response = self
            .provider
            .chat(provider_messages.clone(), tool_defs, config)
            .await;
        let llm_duration = llm_start.elapsed();

        drop(_permit);

        match response {
            Ok(r) => return Ok((r, llm_duration)),
            Err(e) if Self::is_http_400(&e) => {
                warn!("LLM returned HTTP 400, retrying with fewer messages");
            }
            Err(e) => {
                self.observers.notify_error(&e);
                return Err(e);
            }
        }

        // --- First retry: evict oldest half of non-fixed turns ---
        let half = included_turns.len() / 2;
        let reduced_turns = if half > 0 {
            &included_turns[half..]
        } else {
            // Only one turn or empty — skip to minimal fallback.
            &included_turns[included_turns.len().saturating_sub(1)..]
        };

        let mut retry_messages = ContextBudget::assemble_static(
            &self.system_prompt,
            &self.core_persona,
            reduced_turns,
            retrieved_memories,
        );
        if let Some(first) = retry_messages.first_mut() {
            first.content.push_str(channel_context);
        }

        let _permit = self
            .llm_semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("LLM semaphore closed"))?;

        let llm_start = Instant::now();
        let response = self
            .provider
            .chat(retry_messages.clone(), tool_defs, config)
            .await;
        let llm_duration = llm_start.elapsed();

        drop(_permit);

        match response {
            Ok(r) => {
                *provider_messages = retry_messages;
                return Ok((r, llm_duration));
            }
            Err(e) if Self::is_http_400(&e) => {
                warn!("LLM returned HTTP 400 again, falling back to minimal context");
            }
            Err(e) => {
                self.observers.notify_error(&e);
                return Err(e);
            }
        }

        // --- Second retry: system prompt + core persona + current message only ---
        let last_turn = included_turns.last();
        let minimal_turns = match last_turn {
            Some(t) => std::slice::from_ref(t),
            None => &[],
        };

        let mut minimal_messages = ContextBudget::assemble_static(
            &self.system_prompt,
            &self.core_persona,
            minimal_turns,
            &[], // no memories
        );
        if let Some(first) = minimal_messages.first_mut() {
            first.content.push_str(channel_context);
        }

        let _permit = self
            .llm_semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("LLM semaphore closed"))?;

        let llm_start = Instant::now();
        let response = self
            .provider
            .chat(minimal_messages.clone(), tool_defs, config)
            .await;
        let llm_duration = llm_start.elapsed();

        drop(_permit);

        match response {
            Ok(r) => {
                *provider_messages = minimal_messages;
                Ok((r, llm_duration))
            }
            Err(e) => {
                self.observers.notify_error(&e);
                Err(e)
            }
        }
    }

    /// Check if an error is an HTTP 400 from the provider.
    fn is_http_400(err: &anyhow::Error) -> bool {
        err.downcast_ref::<RetryError>()
            .and_then(|re| re.status_code())
            .is_some_and(|s| s == 400)
    }

    /// Load conversation history as turns, incorporating any compaction summary.
    ///
    /// All SQLite I/O runs on the blocking thread pool via `spawn_blocking`.
    async fn load_history_turns(&self, conv_id: &ConversationId) -> Result<(Vec<Turn>, usize)> {
        let store = Arc::clone(&self.history_store);
        let cid = conv_id.clone();

        tokio::task::spawn_blocking(move || {
            let summary = store.load_summary(&cid)?;

            let messages = match &summary {
                Some(s) => store.load_messages_after(&cid, s.compacted_up_to)?,
                None => store.load_messages(&cid)?,
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
        })
        .await
        .map_err(|e| anyhow::anyhow!("task join error: {e}"))?
    }

    /// Search memory for notes relevant to the user's message.
    ///
    /// Runs on the blocking thread pool since `Memory::search_notes` does SQLite I/O.
    async fn retrieve_memories(&self, query: &str) -> Vec<String> {
        let mem = Arc::clone(&self.memory_store);
        let q = query.to_owned();
        let result = tokio::task::spawn_blocking(move || mem.search_notes(&q, 5)).await;
        match result {
            Ok(Ok(notes)) => notes
                .into_iter()
                .map(|n: crate::memory::Note| format!("**{}**: {}", n.title, n.content))
                .collect(),
            Ok(Err(e)) => {
                warn!(error = %e, "memory retrieval failed, continuing without memories");
                vec![]
            }
            Err(e) => {
                warn!(error = %e, "memory retrieval task panicked, continuing without memories");
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
            completion_flag: None,
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
            llm_semaphore: Arc::new(Semaphore::new(4)),
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

    /// A mock provider that tracks peak concurrency via an atomic counter.
    struct ConcurrencyTrackingProvider {
        active: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    impl ConcurrencyTrackingProvider {
        fn new() -> Self {
            Self {
                active: std::sync::atomic::AtomicUsize::new(0),
                peak: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn peak(&self) -> usize {
            self.peak.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl crate::providers::Provider for ConcurrencyTrackingProvider {
        fn name(&self) -> &str {
            "concurrency-mock"
        }

        async fn chat(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: &[ToolDef],
            _config: &crate::providers::RequestConfig,
        ) -> anyhow::Result<LlmResponse> {
            let prev = self.active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let current = prev + 1;
            // Update peak if this is a new high water mark.
            self.peak.fetch_max(current, std::sync::atomic::Ordering::SeqCst);

            // Hold the "slot" for a bit so concurrent calls overlap.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            self.active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

            Ok(LlmResponse {
                text: Some("ok".into()),
                tool_calls: vec![],
                usage: TokenUsage::default(),
            })
        }

        fn estimate_tokens(&self, text: &str) -> usize {
            text.len() / 4
        }
    }

    fn make_semaphore_test_pipeline(
        provider: Arc<ConcurrencyTrackingProvider>,
        permits: usize,
    ) -> Pipeline<ConcurrencyTrackingProvider> {
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

        let tmp_persona = std::env::temp_dir().join("borealis_test_sem_core.md");
        std::fs::write(&tmp_persona, "test persona").unwrap();
        let memory_store: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(Arc::clone(&db), tmp_persona.clone()).unwrap());

        let tool_registry = Arc::new(crate::tools::ToolRegistry::new());
        let observers = Arc::new(ObserverRegistry::new());
        let security = make_test_security();

        let deps = PipelineDeps {
            history_store,
            tool_registry,
            memory_store,
            security,
            observers,
            compaction_config: crate::config::CompactionConfig::default(),
            compaction_state: Arc::new(crate::history::compaction::CompactionState::new()),
            pipeline_config: PipelineConfig::default(),
            llm_semaphore: Arc::new(Semaphore::new(permits)),
        };

        let sys_path = std::path::Path::new("/nonexistent/system_prompt.md");
        let persona_path = &tmp_persona;

        Pipeline::new(provider, sys_path, persona_path, deps).unwrap()
    }

    #[tokio::test]
    async fn semaphore_limits_concurrent_llm_calls() {
        let provider = Arc::new(ConcurrencyTrackingProvider::new());
        let pipeline = Arc::new(make_semaphore_test_pipeline(Arc::clone(&provider), 2));

        // Spawn 3 concurrent pipeline calls with permits=2.
        // Each call uses a unique conversation id to avoid history conflicts.
        let mut handles = Vec::new();
        for i in 0..3 {
            let p = Arc::clone(&pipeline);
            handles.push(tokio::spawn(async move {
                let event = InEvent {
                    source: ChannelSource::Cli,
                    message: Message {
                        id: MessageId(format!("msg-sem-{i}")),
                        author: Author {
                            id: format!("user-{i}"),
                            display_name: "Tester".into(),
                        },
                        text: "hello".into(),
                        timestamp: chrono::Utc::now(),
                        mentions_bot: false,
                    },
                    context: MessageContext {
                        conversation_id: ConversationId::Dm {
                            channel_type: ChannelSource::Cli,
                            user_id: format!("user-{i}"),
                        },
                        channel_id: format!("chan-{i}"),
                        reply_to: None,
                    },
                    tool_groups: None,
                    completion_flag: None,
                };
                p.process_impl(&event).await.unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // Peak concurrency should be at most 2 (the semaphore limit).
        assert!(
            provider.peak() <= 2,
            "expected peak concurrency <= 2, got {}",
            provider.peak()
        );
    }

    /// A mock provider that returns HTTP 400 a configurable number of times,
    /// then succeeds. Also records the message count of each call.
    struct Http400MockProvider {
        failures_remaining: Mutex<usize>,
        call_message_counts: Mutex<Vec<usize>>,
    }

    impl Http400MockProvider {
        fn new(fail_count: usize) -> Self {
            Self {
                failures_remaining: Mutex::new(fail_count),
                call_message_counts: Mutex::new(Vec::new()),
            }
        }

        fn message_counts(&self) -> Vec<usize> {
            self.call_message_counts.lock().unwrap().clone()
        }
    }

    impl crate::providers::Provider for Http400MockProvider {
        fn name(&self) -> &str {
            "http400-mock"
        }

        async fn chat(
            &self,
            messages: Vec<ChatMessage>,
            _tools: &[ToolDef],
            _config: &crate::providers::RequestConfig,
        ) -> anyhow::Result<LlmResponse> {
            self.call_message_counts.lock().unwrap().push(messages.len());

            let mut remaining = self.failures_remaining.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                drop(remaining);
                return Err(crate::providers::retry::RetryError::HttpStatus {
                    status: 400,
                    body: "context too large".into(),
                }.into());
            }

            Ok(LlmResponse {
                text: Some("recovered".into()),
                tool_calls: vec![],
                usage: TokenUsage::default(),
            })
        }

        fn estimate_tokens(&self, text: &str) -> usize {
            text.len() / 4
        }
    }

    fn make_400_test_pipeline(
        provider: Arc<Http400MockProvider>,
    ) -> Pipeline<Http400MockProvider> {
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

        let tmp_persona = std::env::temp_dir().join("borealis_test_400_core.md");
        std::fs::write(&tmp_persona, "test persona").unwrap();
        let memory_store: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(Arc::clone(&db), tmp_persona.clone()).unwrap());

        let tool_registry = Arc::new(crate::tools::ToolRegistry::new());
        let observers = Arc::new(ObserverRegistry::new());
        let security = make_test_security();

        let deps = PipelineDeps {
            history_store,
            tool_registry,
            memory_store,
            security,
            observers,
            compaction_config: crate::config::CompactionConfig::default(),
            compaction_state: Arc::new(crate::history::compaction::CompactionState::new()),
            pipeline_config: PipelineConfig::default(),
            llm_semaphore: Arc::new(Semaphore::new(4)),
        };

        let sys_path = std::path::Path::new("/nonexistent/system_prompt.md");
        let persona_path = &tmp_persona;

        Pipeline::new(provider, sys_path, persona_path, deps).unwrap()
    }

    #[tokio::test]
    async fn http_400_recovery_retries_with_fewer_messages() {
        // Provider fails once with 400, then succeeds on retry with fewer messages.
        let provider = Arc::new(Http400MockProvider::new(1));
        let pipeline = make_400_test_pipeline(Arc::clone(&provider));
        let event = make_test_event(ChannelSource::Cli, "user1");

        let result = pipeline.process_impl(&event).await.unwrap();
        assert_eq!(result.text, Some("recovered".into()));

        let counts = provider.message_counts();
        assert_eq!(counts.len(), 2, "expected 2 LLM calls (original + retry)");
        // The retry should have fewer or equal messages.
        assert!(
            counts[1] <= counts[0],
            "retry should have <= messages: first={}, second={}",
            counts[0],
            counts[1]
        );
    }

    #[tokio::test]
    async fn http_400_recovery_falls_back_to_minimal() {
        // Provider fails twice with 400, then succeeds on minimal fallback.
        let provider = Arc::new(Http400MockProvider::new(2));
        let pipeline = make_400_test_pipeline(Arc::clone(&provider));
        let event = make_test_event(ChannelSource::Cli, "user2");

        let result = pipeline.process_impl(&event).await.unwrap();
        assert_eq!(result.text, Some("recovered".into()));

        let counts = provider.message_counts();
        assert_eq!(counts.len(), 3, "expected 3 LLM calls (original + retry + minimal)");
        // The minimal fallback should have the fewest messages.
        assert!(
            counts[2] <= counts[1],
            "minimal should have <= messages than retry: retry={}, minimal={}",
            counts[1],
            counts[2]
        );
    }
}
