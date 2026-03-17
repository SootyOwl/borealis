use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::event::{ConversationId, InEvent, OutEvent};

/// Configuration for the event bus.
#[derive(Debug, Clone)]
pub struct EventBusConfig {
    /// Capacity of bounded mpsc channels per conversation worker.
    pub channel_capacity: usize,
    /// Maximum number of concurrent LLM requests.
    pub max_llm_concurrency: usize,
    /// Duration after which an idle conversation worker is evicted.
    pub idle_timeout: Duration,
    /// How often to check for idle workers.
    pub eviction_interval: Duration,
}

impl Default for EventBusConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 256,
            max_llm_concurrency: 4,
            idle_timeout: Duration::from_secs(30 * 60), // 30 minutes
            eviction_interval: Duration::from_secs(60), // check every minute
        }
    }
}

/// Metadata tracked per conversation worker.
struct WorkerEntry {
    /// Sender half to dispatch events to this worker.
    tx: mpsc::Sender<InEvent>,
    /// When this worker last received an event.
    last_active: Instant,
    /// Handle to the spawned worker task.
    _handle: JoinHandle<()>,
}

/// A handler function that processes an inbound event and produces an optional outbound event.
///
/// This is the extension point where the core pipeline will be wired in.
/// The semaphore permit should be acquired inside the handler when making LLM calls.
pub type EventHandler =
    Arc<dyn Fn(InEvent, Arc<Semaphore>) -> tokio::task::JoinHandle<Option<OutEvent>> + Send + Sync>;

/// Tokio-based event bus with bounded mpsc channels.
///
/// Per-conversation sequential workers are dispatched via a `DashMap`.
/// Idle workers are evicted after a configurable timeout.
/// Global LLM concurrency is limited by a `tokio::sync::Semaphore`.
pub struct EventBus {
    config: EventBusConfig,
    /// Per-conversation worker registry.
    workers: Arc<DashMap<ConversationId, WorkerEntry>>,
    /// Global LLM concurrency semaphore.
    llm_semaphore: Arc<Semaphore>,
    /// Handler that processes each inbound event.
    handler: EventHandler,
    /// Sender for outbound events (consumed by channel adapters).
    out_tx: mpsc::Sender<OutEvent>,
    /// Cancellation token for graceful shutdown.
    cancel: CancellationToken,
    /// Handle to the eviction task.
    eviction_handle: Option<JoinHandle<()>>,
}

impl EventBus {
    /// Creates a new event bus and returns it along with the outbound event receiver.
    ///
    /// The `handler` is called for each inbound event within the conversation's
    /// sequential worker. It receives the event and a reference to the LLM semaphore
    /// so it can acquire a permit before making LLM calls.
    pub fn new(
        config: EventBusConfig,
        handler: EventHandler,
        cancel: CancellationToken,
    ) -> (Self, mpsc::Receiver<OutEvent>) {
        let (out_tx, out_rx) = mpsc::channel(config.channel_capacity);
        let llm_semaphore = Arc::new(Semaphore::new(config.max_llm_concurrency));
        let workers: Arc<DashMap<ConversationId, WorkerEntry>> = Arc::new(DashMap::new());

        let mut bus = Self {
            config,
            workers,
            llm_semaphore,
            handler,
            out_tx,
            cancel,
            eviction_handle: None,
        };

        bus.start_eviction_task();
        (bus, out_rx)
    }

    /// Dispatches an inbound event to the appropriate conversation worker.
    ///
    /// If no worker exists for the conversation, one is spawned.
    /// Events are processed sequentially within each conversation.
    pub async fn dispatch(&self, event: InEvent) -> Result<(), EventBusError> {
        let conversation_id = event.context.conversation_id.clone();

        // Update last_active or create a new worker
        if let Some(mut entry) = self.workers.get_mut(&conversation_id) {
            entry.last_active = Instant::now();
            entry
                .tx
                .send(event)
                .await
                .map_err(|_| EventBusError::WorkerGone(conversation_id.clone()))?;
        } else {
            let (tx, rx) = mpsc::channel(self.config.channel_capacity);
            tx.send(event)
                .await
                .map_err(|_| EventBusError::ChannelFull(conversation_id.clone()))?;

            let handle = self.spawn_worker(conversation_id.clone(), rx);

            self.workers.insert(
                conversation_id,
                WorkerEntry {
                    tx,
                    last_active: Instant::now(),
                    _handle: handle,
                },
            );
        }

        Ok(())
    }

    /// Spawns a sequential worker for a conversation.
    ///
    /// The worker processes events one at a time, calling the handler for each.
    /// When the channel closes (sender dropped) or cancellation fires, the worker exits.
    fn spawn_worker(
        &self,
        conversation_id: ConversationId,
        mut rx: mpsc::Receiver<InEvent>,
    ) -> JoinHandle<()> {
        let handler = Arc::clone(&self.handler);
        let semaphore = Arc::clone(&self.llm_semaphore);
        let out_tx = self.out_tx.clone();
        let cancel = self.cancel.clone();
        let workers = Arc::clone(&self.workers);
        let conv_id = conversation_id.clone();

        tokio::spawn(async move {
            debug!(conversation = %conv_id, "conversation worker started");

            loop {
                tokio::select! {
                    biased;

                    _ = cancel.cancelled() => {
                        debug!(conversation = %conv_id, "conversation worker shutting down");
                        break;
                    }

                    msg = rx.recv() => {
                        match msg {
                            Some(event) => {
                                let handle = handler(event, Arc::clone(&semaphore));
                                match handle.await {
                                    Ok(Some(out_event)) => {
                                        if let Err(e) = out_tx.send(out_event).await {
                                            warn!(
                                                conversation = %conv_id,
                                                "failed to send outbound event: {e}"
                                            );
                                            break;
                                        }
                                    }
                                    Ok(None) => {
                                        // Handler produced no output (e.g., NoReply directive)
                                    }
                                    Err(e) => {
                                        warn!(
                                            conversation = %conv_id,
                                            "handler task panicked: {e}"
                                        );
                                    }
                                }
                            }
                            None => {
                                debug!(conversation = %conv_id, "worker channel closed");
                                break;
                            }
                        }
                    }
                }
            }

            // Clean up our entry from the workers map
            workers.remove(&conv_id);
            debug!(conversation = %conv_id, "conversation worker stopped");
        })
    }

    /// Starts the background idle eviction task.
    fn start_eviction_task(&mut self) {
        let workers = Arc::clone(&self.workers);
        let idle_timeout = self.config.idle_timeout;
        let interval = self.config.eviction_interval;
        let cancel = self.cancel.clone();

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;

                    _ = cancel.cancelled() => {
                        debug!("eviction task shutting down");
                        break;
                    }

                    _ = ticker.tick() => {
                        let now = Instant::now();
                        let mut evicted = Vec::new();

                        workers.retain(|id, entry| {
                            if now.duration_since(entry.last_active) > idle_timeout {
                                evicted.push(id.clone());
                                false
                            } else {
                                true
                            }
                        });

                        for id in &evicted {
                            info!(conversation = %id, "evicted idle conversation worker");
                        }

                        if !evicted.is_empty() {
                            debug!(count = evicted.len(), "idle eviction sweep complete");
                        }
                    }
                }
            }
        });

        self.eviction_handle = Some(handle);
    }

    /// Returns the number of active conversation workers.
    pub fn active_workers(&self) -> usize {
        self.workers.len()
    }

    /// Returns the number of available LLM permits.
    pub fn available_llm_permits(&self) -> usize {
        self.llm_semaphore.available_permits()
    }

    /// Initiates graceful shutdown of the event bus.
    ///
    /// Cancels all workers and the eviction task, then waits for the eviction
    /// task to complete.
    pub async fn shutdown(mut self) {
        info!("event bus shutting down");
        self.cancel.cancel();

        if let Some(handle) = self.eviction_handle.take() {
            let _ = handle.await;
        }

        // Workers will exit via cancellation token and remove themselves from the map.
        // Give them a moment to clean up.
        tokio::time::sleep(Duration::from_millis(50)).await;
        info!(
            remaining = self.workers.len(),
            "event bus shutdown complete"
        );
    }
}

/// Errors that can occur during event bus operations.
#[derive(Debug, thiserror::Error)]
pub enum EventBusError {
    #[error("conversation worker gone for {0}")]
    WorkerGone(ConversationId),

    #[error("channel full for conversation {0}")]
    ChannelFull(ConversationId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn make_event(conv: &str, content: &str) -> InEvent {
        InEvent {
            source: ChannelSource::Cli,
            message: Message {
                id: MessageId(format!("test-{}", uuid::Uuid::new_v4())),
                author: Author {
                    id: "user1".into(),
                    display_name: "Test User".into(),
                },
                text: content.to_string(),
                timestamp: chrono::Utc::now(),
                mentions_bot: false,
            },
            context: MessageContext {
                conversation_id: ConversationId::Dm {
                    channel_type: ChannelSource::Cli,
                    user_id: conv.into(),
                },
                channel_id: "cli".into(),
                reply_to: None,
            },
        }
    }

    fn echo_handler() -> EventHandler {
        Arc::new(|event: InEvent, _semaphore: Arc<Semaphore>| {
            tokio::spawn(async move {
                Some(OutEvent {
                    target: event.source.clone(),
                    channel_id: event.context.channel_id.clone(),
                    text: Some(event.message.text.clone()),
                    directives: vec![],
                    reply_to: None,
                })
            })
        })
    }

    #[tokio::test]
    async fn single_message_round_trip() {
        let cancel = CancellationToken::new();
        let (bus, mut out_rx) =
            EventBus::new(EventBusConfig::default(), echo_handler(), cancel.clone());

        let event = make_event("conv1", "hello");
        bus.dispatch(event).await.unwrap();

        let out = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .expect("timeout")
            .expect("no output");

        assert_eq!(out.text, Some("hello".to_string()));

        bus.shutdown().await;
    }

    #[tokio::test]
    async fn idle_eviction() {
        let cancel = CancellationToken::new();
        let config = EventBusConfig {
            idle_timeout: Duration::from_millis(100),
            eviction_interval: Duration::from_millis(50),
            ..Default::default()
        };
        let (bus, mut _out_rx) = EventBus::new(config, echo_handler(), cancel.clone());

        bus.dispatch(make_event("conv1", "hello"))
            .await
            .unwrap();

        // Worker should exist now
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(bus.active_workers() >= 1);

        // Wait for idle timeout + eviction interval
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(
            bus.active_workers(),
            0,
            "idle worker should have been evicted"
        );

        bus.shutdown().await;
    }

    #[tokio::test]
    async fn graceful_shutdown_stops_workers() {
        let cancel = CancellationToken::new();
        let (bus, mut _out_rx) =
            EventBus::new(EventBusConfig::default(), echo_handler(), cancel.clone());

        // Spawn a few workers
        for i in 0..3 {
            bus.dispatch(make_event(&format!("conv{i}"), "msg"))
                .await
                .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Shutdown should complete without hanging
        let shutdown_result = tokio::time::timeout(Duration::from_secs(5), bus.shutdown()).await;

        assert!(shutdown_result.is_ok(), "shutdown timed out");
    }

    #[tokio::test]
    async fn llm_semaphore_limits_concurrency() {
        let concurrent = Arc::new(AtomicU64::new(0));
        let max_concurrent = Arc::new(AtomicU64::new(0));

        let conc = Arc::clone(&concurrent);
        let max_conc = Arc::clone(&max_concurrent);
        let handler: EventHandler = Arc::new(move |event: InEvent, sem: Arc<Semaphore>| {
            let conc = Arc::clone(&conc);
            let max_conc = Arc::clone(&max_conc);
            tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();

                let current = conc.fetch_add(1, Ordering::SeqCst) + 1;
                max_conc.fetch_max(current, Ordering::SeqCst);

                tokio::time::sleep(Duration::from_millis(50)).await;

                conc.fetch_sub(1, Ordering::SeqCst);

                Some(OutEvent {
                    target: event.source.clone(),
                    channel_id: event.context.channel_id.clone(),
                    text: Some(event.message.text.clone()),
                    directives: vec![],
                    reply_to: None,
                })
            })
        });

        let cancel = CancellationToken::new();
        let config = EventBusConfig {
            max_llm_concurrency: 2,
            ..Default::default()
        };
        let (bus, mut out_rx) = EventBus::new(config, handler, cancel.clone());

        // Send 6 messages across 6 different conversations (so they run in parallel)
        for i in 0..6 {
            bus.dispatch(make_event(&format!("conv{i}"), "msg"))
                .await
                .unwrap();
        }

        for _ in 0..6 {
            tokio::time::timeout(Duration::from_secs(10), out_rx.recv())
                .await
                .expect("timeout")
                .expect("no output");
        }

        assert!(
            max_concurrent.load(Ordering::SeqCst) <= 2,
            "concurrency exceeded semaphore limit: {}",
            max_concurrent.load(Ordering::SeqCst)
        );

        bus.shutdown().await;
    }
}
