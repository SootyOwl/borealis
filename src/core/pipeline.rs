use std::future::Future;
use std::path::{Path, PathBuf};
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
use crate::providers::{Provider, RequestConfig, StopReason};
use crate::security::{AuthorizationResult, Security};
use crate::tools::{ToolContext, ToolRegistry, ToolResult};
use crate::types::{ChatMessage, ConversationId, ConversationMode, estimate_tokens};

/// Maximum number of tool-call → LLM round-trips before we stop looping.
const MAX_TOOL_ITERATIONS: usize = 10;

/// Marker appended to responses that hit the max output token limit, so
/// neither the user nor the persisted history mistakes a truncated response
/// for a complete turn.
const TRUNCATION_MARKER: &str = "[response truncated: hit max output tokens]";

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
///
/// The system prompt is read once at construction (it changes rarely and
/// requires a restart-equivalent to redeploy).  The core persona is re-read
/// from disk on every invocation so edits to `memory/core.md` take effect
/// without restarting the bot.
pub struct Pipeline<P: Provider + 'static> {
    provider: Arc<P>,
    system_prompt: String,
    core_persona_path: PathBuf,
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

        // Core persona is read lazily per invocation (see `load_core_persona`)
        // so edits to memory/core.md take effect without restarting the bot.
        // We probe the file here only to surface a one-shot startup warning if
        // the configured path is wrong; the runtime path stores the PathBuf.
        if !core_persona_path.exists() {
            debug!(path = %core_persona_path.display(), "core persona file not found at startup; will retry per invocation");
        }

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
            core_persona_path = %core_persona_path.display(),
            tool_count = deps.tool_registry.tool_count(),
            provider = provider.name(),
            "pipeline initialized"
        );

        Ok(Self {
            provider,
            system_prompt,
            core_persona_path: core_persona_path.to_path_buf(),
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

    /// Read the core persona from disk.  Runs on the blocking pool so the file
    /// IO doesn't park a runtime worker.  A missing file is treated as an empty
    /// persona (with a warning) rather than a fatal error — the bot stays up
    /// even if `memory/core.md` is temporarily removed during editing.
    async fn load_core_persona(&self) -> String {
        let path = self.core_persona_path.clone();
        match tokio::task::spawn_blocking(move || std::fs::read_to_string(&path)).await {
            Ok(Ok(contents)) => contents,
            Ok(Err(e)) => {
                warn!(
                    path = %self.core_persona_path.display(),
                    error = %e,
                    "failed to read core persona, continuing with empty persona"
                );
                String::new()
            }
            Err(e) => {
                warn!(error = %e, "core persona load task panicked, continuing with empty persona");
                String::new()
            }
        }
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
        let (turns, summary_text, summary_token_overhead) =
            self.load_history_turns(&conv_id).await?;

        // Retrieve relevant memories based on user message.
        let retrieved_memories = self.retrieve_memories(&event.message.text).await;

        // Read the core persona fresh from disk for this invocation so edits to
        // memory/core.md take effect without a restart.
        let core_persona = self.load_core_persona().await;

        // Build the context budget.
        let system_tokens = estimate_tokens(&self.system_prompt);
        let persona_tokens = estimate_tokens(&core_persona);
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
        // Fixed, non-evictable context cost: tool-definition tokens plus the
        // pinned compaction-summary tokens (the summary is counted once here,
        // never as an evictable turn).
        let fixed_overhead = estimate_tokens(&tool_defs_json) + summary_token_overhead;

        let budget = ContextBudget::new(
            self.pipeline_config.model_max_tokens,
            self.pipeline_config.response_reserve,
            system_tokens,
            persona_tokens,
            fixed_overhead,
        );

        let selection = budget.select_turns(&turns);
        if !selection.evicted.is_empty() {
            debug!(
                evicted = selection.evicted.len(),
                "evicted oldest turns from context window"
            );
        }

        // Assemble the full message array. The compaction summary (if any) is
        // pinned as fixed context via `summary_text`, never as an evictable turn.
        let included_turns: Vec<Turn> = selection.included.iter().map(|t| (*t).clone()).collect();
        let assembled = budget.assemble(
            &self.system_prompt,
            &core_persona,
            &included_turns,
            &retrieved_memories,
            summary_text.as_deref(),
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

        // In-flight buffer: messages appended during THIS turn's tool loop
        // (assistant-with-tool-calls and tool_results). On 400-recovery the
        // base prompt is rebuilt from scratch from `included_turns`, which would
        // otherwise drop these in-flight messages and make the model re-issue
        // already-executed, side-effectful tool calls. We replay this buffer
        // onto every rebuilt prompt. Using an explicit buffer (not a slice index
        // into provider_messages) keeps it correct across multiple recoveries,
        // where the rebuilt base length differs from the original.
        let mut inflight: Vec<ChatMessage> = Vec::new();

        let config = RequestConfig {
            temperature: self.pipeline_config.temperature,
            max_tokens: self.pipeline_config.max_response_tokens.map(|n| n as u32),
            ..Default::default()
        };

        // Determine whether this event is system-originated (bypasses authorization).
        let is_system_event = event.source == ChannelSource::Scheduler;

        // Tool execution loop.
        let mut iterations = 0;
        let mut response = loop {
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
                    &core_persona,
                    summary_text.as_deref(),
                    &inflight,
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

            // Add assistant message to provider messages (same type now), and to
            // the in-flight buffer so it survives a 400-recovery rebuild.
            provider_messages.push(assistant_msg.clone());
            inflight.push(assistant_msg);

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

                        provider_messages.push(tool_msg.clone());
                        inflight.push(tool_msg);
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

                // Add to provider messages, and to the in-flight buffer so it
                // survives a 400-recovery rebuild.
                provider_messages.push(tool_msg.clone());
                inflight.push(tool_msg);
            }

            iterations += 1;
        };

        // The response was cut off mid-generation by the output token limit.
        // Mark it visibly so neither the user nor the persisted history
        // mistakes it for a complete turn.
        if response.stop_reason == StopReason::MaxTokens {
            warn!(
                max_response_tokens = ?self.pipeline_config.max_response_tokens,
                "LLM response hit max output tokens; appending truncation marker"
            );
            match response.text.as_mut() {
                Some(text) => {
                    text.push_str("\n\n");
                    text.push_str(TRUNCATION_MARKER);
                }
                None => response.text = Some(TRUNCATION_MARKER.to_string()),
            }
        }

        // Persist the final assistant response to history.
        //
        // The loop only persists assistant messages for iterations that CONTINUE
        // (it appends the assistant-with-tool-calls message before executing the
        // tools and looping again). The response that BREAKS the loop is never
        // persisted inside the loop — this is true for BOTH exit paths:
        //   - normal exit: `response.tool_calls` is empty;
        //   - max-iterations exit: `response.tool_calls` is still non-empty, yet
        //     the text was already delivered to the user via `build_out_event`,
        //     so it must be persisted here or history diverges from what the user
        //     saw.
        //
        // We persist a PLAIN assistant message (`ChatMessage::assistant`), dropping
        // any unexecuted tool_calls: persisting tool_calls without matching
        // tool_results would orphan the tool_use and make the provider reject the
        // next turn with HTTP 400. `response_text` is captured AFTER the MaxTokens
        // truncation marker is appended above, and `build_out_event` reads the same
        // `response.text`, so persisted text == delivered text.
        let response_text = response.text.clone().unwrap_or_default();
        if !response_text.is_empty() {
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
    #[allow(clippy::too_many_arguments)] // distinct context pieces, no clear grouping
    async fn call_llm_with_400_recovery(
        &self,
        provider_messages: &mut Vec<ChatMessage>,
        tool_defs: &[crate::tools::ToolDef],
        config: &RequestConfig,
        included_turns: &[Turn],
        retrieved_memories: &[String],
        channel_context: &str,
        core_persona: &str,
        summary: Option<&str>,
        inflight: &[ChatMessage],
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
            core_persona,
            reduced_turns,
            retrieved_memories,
            summary,
        );
        if let Some(first) = retry_messages.first_mut() {
            first.content.push_str(channel_context);
        }
        // Replay in-flight tool-loop messages so the recovery request still
        // carries the assistant-with-tool-calls / tool_result pairs appended
        // during this turn; otherwise the model re-issues executed tool calls.
        retry_messages.extend_from_slice(inflight);

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
            core_persona,
            minimal_turns,
            &[], // no memories
            summary,
        );
        if let Some(first) = minimal_messages.first_mut() {
            first.content.push_str(channel_context);
        }
        // Replay in-flight tool-loop messages (see the first-retry rebuild).
        minimal_messages.extend_from_slice(inflight);

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
    /// Returns `(turns, summary_text, summary_token_overhead)`:
    /// - `turns`: the real conversation turns (no synthetic summary turn).
    /// - `summary_text`: the compaction summary text to render as fixed,
    ///   non-evictable context (`None` if no summary).
    /// - `summary_token_overhead`: the summary's token estimate, counted ONCE as
    ///   fixed overhead (0 if no summary). The summary is deliberately NOT a Turn
    ///   so `select_turns` neither double-counts it nor evicts it.
    ///
    /// The summary and the messages-after-boundary are read under a single lock
    /// via `load_summary_and_messages`, so a concurrent compaction commit can
    /// never be observed half-applied.
    ///
    /// All SQLite I/O runs on the blocking thread pool via `spawn_blocking`.
    async fn load_history_turns(
        &self,
        conv_id: &ConversationId,
    ) -> Result<(Vec<Turn>, Option<String>, usize)> {
        let store = Arc::clone(&self.history_store);
        let cid = conv_id.clone();

        tokio::task::spawn_blocking(move || {
            let (summary, messages) = store.load_summary_and_messages(&cid)?;

            let summary_text = summary.as_ref().map(|s| s.summary_text.clone());
            let summary_overhead = summary.as_ref().map_or(0, |s| s.token_estimate);

            // Build turns from stored messages (group by turn_id). The summary
            // is NOT represented as a turn — it is pinned separately as fixed
            // context so it is counted once and never evicted.
            let mut turns: Vec<Turn> = Vec::new();

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

            Ok((turns, summary_text, summary_overhead))
        })
        .await
        .map_err(|e| anyhow::anyhow!("task join error: {e}"))?
    }

    /// Search memory for notes relevant to the user's message.
    ///
    /// Runs on the blocking thread pool since `Memory::search_notes` does SQLite I/O.
    ///
    /// The user message is sanitized for FTS5: metacharacters are stripped and
    /// bare operator keywords are dropped, but the surviving tokens are passed
    /// through so FTS5's tokenizer + Porter stemmer can do their normal work.
    /// FTS5's default is implicit AND, so a multi-word user message matches
    /// notes containing all of the surviving tokens — narrow but predictable.
    /// See `crate::memory::sanitize_for_fts` for the exact rules.
    async fn retrieve_memories(&self, query: &str) -> Vec<String> {
        let sanitized = crate::memory::sanitize_for_fts(query);
        if sanitized.is_empty() {
            // Nothing salvageable from the user message — skip retrieval rather
            // than handing FTS5 an empty query that would match-all.
            return vec![];
        }
        let mem = Arc::clone(&self.memory_store);
        let result =
            tokio::task::spawn_blocking(move || mem.search_notes(&sanitized, 5, None, false)).await;
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
    use crate::providers::{LlmResponse, StopReason, TokenUsage};
    use crate::tools::{ToolCall, ToolDef};
    use crate::security::Security;
    use std::sync::Mutex;

    /// A mock provider that returns a configurable sequence of responses and
    /// records the messages it was called with.
    struct MockProvider {
        responses: Mutex<Vec<LlmResponse>>,
        captured: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                captured: Mutex::new(Vec::new()),
            }
        }

        fn captured(&self) -> Vec<Vec<ChatMessage>> {
            self.captured.lock().unwrap().clone()
        }
    }

    impl crate::providers::Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn chat(
            &self,
            messages: Vec<ChatMessage>,
            _tools: &[ToolDef],
            _config: &crate::providers::RequestConfig,
        ) -> anyhow::Result<LlmResponse> {
            self.captured.lock().unwrap().push(messages);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(LlmResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                    usage: TokenUsage::default(),
                    stop_reason: StopReason::EndTurn,
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
                guild_id: None,
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
                stop_reason: StopReason::ToolUse,
            },
            LlmResponse {
                text: Some("I was denied.".into()),
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
                stop_reason: StopReason::EndTurn,
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
                stop_reason: StopReason::ToolUse,
            },
            LlmResponse {
                text: Some("Done.".into()),
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
                stop_reason: StopReason::EndTurn,
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
                stop_reason: StopReason::ToolUse,
            },
            LlmResponse {
                text: Some("System done.".into()),
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
                stop_reason: StopReason::EndTurn,
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
                stop_reason: StopReason::EndTurn,
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
                        guild_id: None,
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
                stop_reason: StopReason::EndTurn,
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
    async fn core_persona_is_reloaded_each_invocation() {
        // Build a pipeline pointing at a tempfile, then change the file
        // between invocations and confirm the new contents flow through to
        // the provider on the next call.
        let provider = Arc::new(MockProvider::new(vec![]));
        let db = Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap()));
        {
            let conn = db.lock().unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
                .unwrap();
        }
        crate::history::schema::initialize(&db.lock().unwrap()).unwrap();
        let history_store = Arc::new(crate::history::store::HistoryStore::new(Arc::clone(&db)));

        let tmp_persona = std::env::temp_dir().join("borealis_test_reload_core.md");
        std::fs::write(&tmp_persona, "PERSONA_VERSION_ONE").unwrap();
        let memory_store: Arc<dyn crate::memory::Memory> = Arc::new(
            crate::memory::SqliteMemory::new(Arc::clone(&db), tmp_persona.clone()).unwrap(),
        );

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
        let pipeline = Pipeline::new(Arc::clone(&provider), sys_path, &tmp_persona, deps).unwrap();

        // First call: persona should be VERSION_ONE.
        let event = make_test_event(ChannelSource::Cli, "user_reload");
        pipeline.process_impl(&event).await.unwrap();
        let captured = provider.captured();
        assert!(
            captured.last().unwrap().iter().any(|m| m.content.contains("PERSONA_VERSION_ONE")),
            "first invocation should see PERSONA_VERSION_ONE in the prompt",
        );

        // Overwrite the persona file. No restart, no Pipeline rebuild.
        std::fs::write(&tmp_persona, "PERSONA_VERSION_TWO").unwrap();

        // Second call: persona should be VERSION_TWO.
        pipeline.process_impl(&event).await.unwrap();
        let captured = provider.captured();
        let second_call = captured.last().unwrap();
        assert!(
            second_call.iter().any(|m| m.content.contains("PERSONA_VERSION_TWO")),
            "second invocation should see PERSONA_VERSION_TWO after the file was rewritten",
        );
        assert!(
            !second_call.iter().any(|m| m.content.contains("PERSONA_VERSION_ONE")),
            "second invocation must NOT carry the stale PERSONA_VERSION_ONE content",
        );

        let _ = std::fs::remove_file(&tmp_persona);
    }

    #[tokio::test]
    async fn max_tokens_response_is_marked_truncated_and_persisted_with_marker() {
        // Build a pipeline keeping a handle on the history store so we can
        // verify what gets persisted.
        let truncated = LlmResponse {
            text: Some("This reply was cut off mid-sen".into()),
            tool_calls: vec![],
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 1024,
            },
            stop_reason: StopReason::MaxTokens,
        };
        let provider = Arc::new(MockProvider::new(vec![truncated]));

        let db = Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap()));
        {
            let conn = db.lock().unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
                .unwrap();
        }
        crate::history::schema::initialize(&db.lock().unwrap()).unwrap();
        let history_store = Arc::new(crate::history::store::HistoryStore::new(Arc::clone(&db)));

        let tmp_persona = std::env::temp_dir().join("borealis_test_trunc_core.md");
        std::fs::write(&tmp_persona, "test persona").unwrap();
        let memory_store: Arc<dyn crate::memory::Memory> = Arc::new(
            crate::memory::SqliteMemory::new(Arc::clone(&db), tmp_persona.clone()).unwrap(),
        );

        let deps = PipelineDeps {
            history_store: Arc::clone(&history_store),
            tool_registry: Arc::new(crate::tools::ToolRegistry::new()),
            memory_store,
            security: make_test_security(),
            observers: Arc::new(ObserverRegistry::new()),
            compaction_config: crate::config::CompactionConfig::default(),
            compaction_state: Arc::new(crate::history::compaction::CompactionState::new()),
            pipeline_config: PipelineConfig::default(),
            llm_semaphore: Arc::new(Semaphore::new(4)),
        };

        let sys_path = std::path::Path::new("/nonexistent/system_prompt.md");
        let pipeline = Pipeline::new(provider, sys_path, &tmp_persona, deps).unwrap();

        let event = make_test_event(ChannelSource::Cli, "trunc_user");
        let result = pipeline.process_impl(&event).await.unwrap();

        // The delivered text carries the visible truncation marker.
        let text = result.text.expect("response text");
        assert!(text.starts_with("This reply was cut off mid-sen"));
        assert!(
            text.contains("[response truncated: hit max output tokens]"),
            "delivered text should carry the truncation marker: {text}"
        );

        // The persisted assistant turn also carries the marker, so history
        // doesn't record the truncated reply as a clean turn.
        let conv_id = crate::core::event::ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "trunc_user".into(),
        };
        let messages = history_store.load_messages(&conv_id).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.role == crate::types::Role::Assistant)
            .expect("assistant message persisted");
        assert!(
            assistant
                .content
                .contains("[response truncated: hit max output tokens]"),
            "persisted assistant message should carry the marker: {}",
            assistant.content
        );

        let _ = std::fs::remove_file(&tmp_persona);
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

    // -----------------------------------------------------------------------
    // BUD-1 / CORE-2 / CORE-3 — scripted mock that mixes success responses and
    // HTTP 400 errors, capturing the full message array on every call.
    // -----------------------------------------------------------------------

    /// A scripted outcome for `ScriptedMockProvider`.
    enum Scripted {
        /// Return this response.
        Ok(LlmResponse),
        /// Return an HTTP 400 error.
        Http400,
    }

    /// A mock provider that plays a scripted sequence of outcomes and records
    /// the full message array it was called with on every call. Once the script
    /// is exhausted it returns a plain "done" response.
    struct ScriptedMockProvider {
        script: Mutex<Vec<Scripted>>,
        captured: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl ScriptedMockProvider {
        fn new(script: Vec<Scripted>) -> Self {
            Self {
                script: Mutex::new(script),
                captured: Mutex::new(Vec::new()),
            }
        }

        fn captured(&self) -> Vec<Vec<ChatMessage>> {
            self.captured.lock().unwrap().clone()
        }
    }

    impl crate::providers::Provider for ScriptedMockProvider {
        fn name(&self) -> &str {
            "scripted-mock"
        }

        async fn chat(
            &self,
            messages: Vec<ChatMessage>,
            _tools: &[ToolDef],
            _config: &crate::providers::RequestConfig,
        ) -> anyhow::Result<LlmResponse> {
            self.captured.lock().unwrap().push(messages);
            let next = {
                let mut script = self.script.lock().unwrap();
                if script.is_empty() {
                    None
                } else {
                    Some(script.remove(0))
                }
            };
            match next {
                Some(Scripted::Ok(r)) => Ok(r),
                Some(Scripted::Http400) => Err(crate::providers::retry::RetryError::HttpStatus {
                    status: 400,
                    body: "context too large".into(),
                }
                .into()),
                None => Ok(LlmResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                    usage: TokenUsage::default(),
                    stop_reason: StopReason::EndTurn,
                }),
            }
        }

        fn estimate_tokens(&self, text: &str) -> usize {
            text.len() / 4
        }
    }

    /// Build a pipeline around a `ScriptedMockProvider`, returning both the
    /// pipeline and a handle on the history store for persistence assertions.
    fn make_scripted_pipeline(
        provider: Arc<ScriptedMockProvider>,
        persona_tag: &str,
    ) -> (Pipeline<ScriptedMockProvider>, Arc<crate::history::store::HistoryStore>) {
        let db = Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap()));
        {
            let conn = db.lock().unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
                .unwrap();
        }
        crate::history::schema::initialize(&db.lock().unwrap()).unwrap();
        let history_store = Arc::new(crate::history::store::HistoryStore::new(Arc::clone(&db)));

        let tmp_persona = std::env::temp_dir().join(format!("borealis_test_{persona_tag}_core.md"));
        std::fs::write(&tmp_persona, "test persona").unwrap();
        let memory_store: Arc<dyn crate::memory::Memory> = Arc::new(
            crate::memory::SqliteMemory::new(Arc::clone(&db), tmp_persona.clone()).unwrap(),
        );

        let deps = PipelineDeps {
            history_store: Arc::clone(&history_store),
            tool_registry: Arc::new(crate::tools::ToolRegistry::new()),
            memory_store,
            security: make_test_security(),
            observers: Arc::new(ObserverRegistry::new()),
            compaction_config: crate::config::CompactionConfig::default(),
            compaction_state: Arc::new(crate::history::compaction::CompactionState::new()),
            pipeline_config: PipelineConfig::default(),
            llm_semaphore: Arc::new(Semaphore::new(4)),
        };

        let sys_path = std::path::Path::new("/nonexistent/system_prompt.md");
        let pipeline = Pipeline::new(provider, sys_path, &tmp_persona, deps).unwrap();
        (pipeline, history_store)
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "bash_exec".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
        }
    }

    // --- BUD-1: compaction summary is pinned, counted once, never evicted ---

    #[tokio::test]
    async fn compaction_summary_is_pinned_in_prompt_after_persona() {
        // Seed a conversation with a compaction summary and one real turn, then
        // process a message. The assembled prompt must carry the summary as a
        // single system message placed right after the persona and before the
        // surviving history — i.e. it is pinned, not a turn.
        let provider = Arc::new(ScriptedMockProvider::new(vec![]));
        let (pipeline, store) = make_scripted_pipeline(Arc::clone(&provider), "budsummary");

        let conv_id = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "summary_user".into(),
        };
        store
            .ensure_conversation(&conv_id, crate::types::ConversationMode::Shared)
            .unwrap();
        // A pre-existing real turn that survives compaction (seq after boundary).
        let turn = store
            .append_message(&conv_id, &ChatMessage::user("earlier message"), None)
            .unwrap();
        store
            .append_message(&conv_id, &ChatMessage::assistant("earlier reply"), Some(&turn))
            .unwrap();
        // Save a summary whose boundary is BEFORE the surviving turn (seq 0), so
        // the surviving turn is still loaded as history.
        store
            .save_summary(&conv_id, "SUMMARY_MARKER_TEXT", 0, 7)
            .unwrap();

        let event = make_test_event(ChannelSource::Cli, "summary_user");
        pipeline.process_impl(&event).await.unwrap();

        let captured = provider.captured();
        let prompt = captured.last().expect("at least one LLM call");

        // The summary appears exactly once, as a system message.
        let summary_indices: Vec<usize> = prompt
            .iter()
            .enumerate()
            .filter(|(_, m)| m.content.contains("## Conversation Summary")
                && m.content.contains("SUMMARY_MARKER_TEXT"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            summary_indices.len(),
            1,
            "summary must appear exactly once (not double-counted/duplicated)"
        );
        let summary_idx = summary_indices[0];

        // It sits right after the persona/system message (index 0).
        assert_eq!(prompt[0].role, crate::types::Role::System);
        assert_eq!(summary_idx, 1, "summary must be pinned right after persona");
        assert_eq!(prompt[summary_idx].role, crate::types::Role::System);

        // The surviving real history follows the summary.
        assert!(
            prompt[summary_idx + 1..]
                .iter()
                .any(|m| m.content.contains("earlier message")),
            "surviving history must follow the pinned summary"
        );
    }

    /// Like `make_scripted_pipeline` but with a caller-supplied `PipelineConfig`,
    /// so a test can tune the token budget.
    fn make_scripted_pipeline_with_config(
        provider: Arc<ScriptedMockProvider>,
        persona_tag: &str,
        pipeline_config: PipelineConfig,
    ) -> (Pipeline<ScriptedMockProvider>, Arc<crate::history::store::HistoryStore>) {
        let db = Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap()));
        {
            let conn = db.lock().unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
                .unwrap();
        }
        crate::history::schema::initialize(&db.lock().unwrap()).unwrap();
        let history_store = Arc::new(crate::history::store::HistoryStore::new(Arc::clone(&db)));

        let tmp_persona = std::env::temp_dir().join(format!("borealis_test_{persona_tag}_core.md"));
        std::fs::write(&tmp_persona, "test persona").unwrap();
        let memory_store: Arc<dyn crate::memory::Memory> = Arc::new(
            crate::memory::SqliteMemory::new(Arc::clone(&db), tmp_persona.clone()).unwrap(),
        );

        let deps = PipelineDeps {
            history_store: Arc::clone(&history_store),
            tool_registry: Arc::new(crate::tools::ToolRegistry::new()),
            memory_store,
            security: make_test_security(),
            observers: Arc::new(ObserverRegistry::new()),
            compaction_config: crate::config::CompactionConfig::default(),
            compaction_state: Arc::new(crate::history::compaction::CompactionState::new()),
            pipeline_config,
            llm_semaphore: Arc::new(Semaphore::new(4)),
        };

        let sys_path = std::path::Path::new("/nonexistent/system_prompt.md");
        let pipeline = Pipeline::new(provider, sys_path, &tmp_persona, deps).unwrap();
        (pipeline, history_store)
    }

    #[tokio::test]
    async fn budget_summary_overhead_evicts_older_turn() {
        // BUD-1 (budget half): the pinned summary's token estimate must be
        // subtracted from `available_for_history`. We tune the budget so an older
        // turn fits ONLY when the summary overhead is NOT counted; with the
        // overhead, the older turn is pushed out of the window and evicted.
        //
        // The summary's `compacted_up_to` is 0 so the seeded turn (seq > 0)
        // still loads as evictable history, and the new incoming message becomes
        // the (never-evicted) last turn.
        let provider = Arc::new(ScriptedMockProvider::new(vec![]));

        // Empty tool registry → tool_defs serialize to "[]" (0 tokens). Replicate
        // the production fixed-overhead formula so the boundary is exact.
        let tool_defs_json = "[]";
        let fixed = estimate_tokens(&default_system_prompt())
            + estimate_tokens("test persona")
            + estimate_tokens(tool_defs_json);

        let summary_overhead = 100usize;
        // The new incoming user message persisted by process_impl is
        // "Tester: hello" (see make_test_event) → its own turn.
        let new_turn_tokens = estimate_tokens("Tester: hello");

        // Choose a budget and turn sizes so that:
        //   available_without_summary       = old + new + 50   (older turn fits)
        //   available_with_summary (= -100) = old + new - 50   (older turn evicted)
        let response_reserve = 0usize;
        // Make the older turn comfortably large so its eviction is unambiguous.
        let old_turn_tokens = 400usize;
        // model_max chosen so available_without_summary = old + new + 50.
        let model_max_tokens =
            response_reserve + fixed + old_turn_tokens + new_turn_tokens + 50;

        let pipeline_config = PipelineConfig {
            model_max_tokens,
            response_reserve,
            ..PipelineConfig::default()
        };
        let (pipeline, store) = make_scripted_pipeline_with_config(
            Arc::clone(&provider),
            "budevict",
            pipeline_config,
        );

        let conv_id = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "evict_user".into(),
        };
        store
            .ensure_conversation(&conv_id, crate::types::ConversationMode::Shared)
            .unwrap();

        // Seed an older turn sized to exactly `old_turn_tokens`. A single user
        // message of 4*N chars yields token_estimate == N (estimate = len/4).
        let old_content = "a".repeat(old_turn_tokens * 4);
        let unique_marker = "OLDER_TURN_MARKER_xyzzy";
        let old_content = format!("{unique_marker}{}", &old_content[unique_marker.len()..]);
        assert_eq!(
            estimate_tokens(&old_content),
            old_turn_tokens,
            "test setup: older turn must be exactly old_turn_tokens"
        );
        store
            .append_message(&conv_id, &ChatMessage::user(&old_content), None)
            .unwrap();

        // Pin a summary with a large token estimate.
        store
            .save_summary(&conv_id, "EVICT_SUMMARY_TEXT", 0, summary_overhead)
            .unwrap();

        let event = make_test_event(ChannelSource::Cli, "evict_user");
        pipeline.process_impl(&event).await.unwrap();

        let captured = provider.captured();
        let prompt = captured.last().expect("at least one LLM call");

        // The summary IS present (pinned).
        assert!(
            prompt
                .iter()
                .any(|m| m.content.contains("## Conversation Summary")
                    && m.content.contains("EVICT_SUMMARY_TEXT")),
            "pinned summary must be present in the assembled prompt"
        );
        // The older turn was EVICTED because the summary overhead shrank the
        // history budget below old + new.
        assert!(
            !prompt.iter().any(|m| m.content.contains(unique_marker)),
            "older turn must be evicted once the summary overhead is counted"
        );
    }

    #[tokio::test]
    async fn compaction_summary_survives_400_recovery() {
        // CORE-2/BUD-1 interaction: even when 400-recovery rebuilds the prompt
        // from scratch, the pinned summary must still be present.
        let provider = Arc::new(ScriptedMockProvider::new(vec![
            Scripted::Http400,
            Scripted::Ok(LlmResponse {
                text: Some("recovered".into()),
                tool_calls: vec![],
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            }),
        ]));
        let (pipeline, store) = make_scripted_pipeline(Arc::clone(&provider), "budrecover");

        let conv_id = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "recover_user".into(),
        };
        store
            .ensure_conversation(&conv_id, crate::types::ConversationMode::Shared)
            .unwrap();
        store
            .save_summary(&conv_id, "PINNED_SUMMARY", 0, 5)
            .unwrap();

        let event = make_test_event(ChannelSource::Cli, "recover_user");
        let result = pipeline.process_impl(&event).await.unwrap();
        assert_eq!(result.text, Some("recovered".into()));

        let captured = provider.captured();
        assert_eq!(captured.len(), 2, "expected original + recovery call");
        // The recovery (2nd) call's rebuilt prompt must still carry the summary.
        assert!(
            captured[1]
                .iter()
                .any(|m| m.content.contains("## Conversation Summary")
                    && m.content.contains("PINNED_SUMMARY")),
            "pinned summary must survive the 400-recovery rebuild"
        );
    }

    // --- CORE-2: in-flight tool-loop messages survive 400-recovery ---

    #[tokio::test]
    async fn inflight_tool_messages_preserved_across_400_recovery() {
        // Call 1: assistant issues a tool call (loop executes it, appends the
        //         assistant-with-tool-calls + tool_result to provider_messages).
        // Call 2: HTTP 400 (mid tool loop).
        // Call 3 (recovery): succeeds.
        // The recovery request must still contain the in-flight tool_use /
        // tool_result pair, not just the reduced base history.
        let provider = Arc::new(ScriptedMockProvider::new(vec![
            Scripted::Ok(LlmResponse {
                text: Some("calling a tool".into()),
                tool_calls: vec![tool_call("tc_inflight")],
                usage: TokenUsage::default(),
                stop_reason: StopReason::ToolUse,
            }),
            Scripted::Http400,
            Scripted::Ok(LlmResponse {
                text: Some("after recovery".into()),
                tool_calls: vec![],
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            }),
        ]));
        let (pipeline, _store) = make_scripted_pipeline(Arc::clone(&provider), "inflight");

        let event = make_test_event(ChannelSource::Cli, "inflight_user");
        let result = pipeline.process_impl(&event).await.unwrap();
        assert_eq!(result.text, Some("after recovery".into()));

        let captured = provider.captured();
        assert_eq!(
            captured.len(),
            3,
            "expected 3 calls: tool-call, 400, recovery"
        );
        let recovery = &captured[2];

        // The recovery request retains the in-flight assistant-with-tool-calls.
        assert!(
            recovery.iter().any(|m| m.role == crate::types::Role::Assistant
                && m.tool_calls.iter().any(|tc| tc.id == "tc_inflight")),
            "recovery request must retain the in-flight assistant tool_call message"
        );
        // ...and the matching tool_result (so tool_use is not orphaned).
        assert!(
            recovery
                .iter()
                .any(|m| m.role == crate::types::Role::Tool
                    && m.tool_call_id.as_deref() == Some("tc_inflight")),
            "recovery request must retain the matching tool_result message"
        );
    }

    // --- CORE-3: final text persisted once on max-iterations break ---

    #[tokio::test]
    async fn max_iterations_break_persists_final_text_as_plain_assistant() {
        // Script the provider to ALWAYS return a tool call (text + tool_calls),
        // forcing the loop to hit MAX_TOOL_ITERATIONS and break with a final
        // response that still has non-empty tool_calls.
        let mut script = Vec::new();
        for i in 0..(MAX_TOOL_ITERATIONS + 2) {
            script.push(Scripted::Ok(LlmResponse {
                text: Some(format!("iteration {i} text")),
                tool_calls: vec![tool_call(&format!("tc_{i}"))],
                usage: TokenUsage::default(),
                stop_reason: StopReason::ToolUse,
            }));
        }
        let provider = Arc::new(ScriptedMockProvider::new(script));
        let (pipeline, store) = make_scripted_pipeline(Arc::clone(&provider), "maxiter");

        let event = make_test_event(ChannelSource::Cli, "maxiter_user");
        let result = pipeline.process_impl(&event).await.unwrap();

        // The breaking response is the one returned at iteration MAX_TOOL_ITERATIONS.
        let breaking_text = format!("iteration {MAX_TOOL_ITERATIONS} text");
        // Delivered text matches the breaking response.
        assert_eq!(result.text, Some(breaking_text.clone()));

        let conv_id = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "maxiter_user".into(),
        };
        let messages = store.load_messages(&conv_id).unwrap();

        // The breaking text must be persisted exactly once, as a PLAIN assistant
        // message (no tool_calls), matching the delivered out-event text.
        let plain_matches: Vec<_> = messages
            .iter()
            .filter(|m| m.role == crate::types::Role::Assistant
                && m.content == breaking_text
                && m.tool_calls.is_empty())
            .collect();
        assert_eq!(
            plain_matches.len(),
            1,
            "breaking response text must be persisted exactly once as a plain assistant message"
        );

        // And it must NOT also be persisted as an assistant-with-tool-calls
        // message (that would orphan the tool_use on the next turn).
        assert!(
            !messages.iter().any(|m| m.role == crate::types::Role::Assistant
                && m.content == breaking_text
                && !m.tool_calls.is_empty()),
            "breaking response must not be persisted with its unexecuted tool_calls"
        );
    }
}
