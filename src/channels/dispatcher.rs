//! Per-conversation worker dispatcher.
//!
//! Routes inbound events to isolated per-conversation workers so that a slow
//! conversation (e.g. long tool-call loop) does not block other conversations
//! on the same channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::core::event::{ConversationId, InEvent, OutEvent};
use crate::core::pipeline::PipelineRunner;

/// How long a worker may sit idle before eviction.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How often the eviction sweep runs.
const EVICTION_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Per-worker bounded channel capacity.
const WORKER_BUFFER: usize = 64;

/// Stores a timestamp as milliseconds since an arbitrary epoch (Instant-based).
///
/// We use the tokio `Instant` baseline so we can convert back and forth.
/// This avoids holding async locks inside DashMap guards.
struct ActivityTimestamp(AtomicU64);

impl ActivityTimestamp {
    fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    fn touch(&self, base: Instant) {
        let millis = Instant::now().duration_since(base).as_millis() as u64;
        self.0.store(millis, Ordering::Relaxed);
    }

    fn elapsed_since_last(&self, base: Instant) -> Duration {
        let last_millis = self.0.load(Ordering::Relaxed);
        let now_millis = Instant::now().duration_since(base).as_millis() as u64;
        Duration::from_millis(now_millis.saturating_sub(last_millis))
    }
}

/// Metadata for a live conversation worker.
struct WorkerHandle {
    tx: mpsc::Sender<InEvent>,
    last_activity: Arc<ActivityTimestamp>,
}

/// Dispatches inbound events to per-conversation workers.
///
/// Each unique `ConversationId` gets its own tokio task that sequentially
/// processes events through the pipeline. Workers that have been idle longer
/// than [`IDLE_TIMEOUT`] are cleaned up by a periodic eviction sweep.
pub struct ConversationDispatcher {
    workers: Arc<DashMap<ConversationId, WorkerHandle>>,
    pipeline: Arc<dyn PipelineRunner>,
    out_tx: mpsc::Sender<OutEvent>,
    cancel: CancellationToken,
    channel_name: String,
    /// Baseline instant for converting ActivityTimestamp values.
    epoch: Instant,
}

impl ConversationDispatcher {
    /// Create a new dispatcher. Also spawns the background eviction task.
    pub fn new(
        pipeline: Arc<dyn PipelineRunner>,
        out_tx: mpsc::Sender<OutEvent>,
        cancel: CancellationToken,
        channel_name: String,
    ) -> Arc<Self> {
        let dispatcher = Arc::new(Self {
            workers: Arc::new(DashMap::new()),
            pipeline,
            out_tx,
            cancel: cancel.clone(),
            channel_name,
            epoch: Instant::now(),
        });

        // Spawn eviction sweep.
        {
            let d = Arc::clone(&dispatcher);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                d.eviction_loop(cancel).await;
            });
        }

        dispatcher
    }

    /// Route an inbound event to its conversation's worker, spawning one if needed.
    pub async fn dispatch(&self, event: InEvent) {
        let conv_id = event.context.conversation_id.clone();

        // Fast path: worker already exists and channel is open.
        // Clone tx and activity out of the DashMap guard to avoid holding
        // the shard lock across the .await on send().
        let existing = self.workers.get(&conv_id).and_then(|handle| {
            if handle.tx.is_closed() {
                None
            } else {
                Some((handle.tx.clone(), Arc::clone(&handle.last_activity)))
            }
        });

        if let Some((tx, activity)) = existing {
            activity.touch(self.epoch);
            match tx.send(event).await {
                Ok(()) => return,
                Err(mpsc::error::SendError(event)) => {
                    // Worker is gone, fall through to spawn a new one with
                    // the recovered event.
                    self.workers.remove(&conv_id);
                    self.spawn_worker(conv_id, event).await;
                    return;
                }
            }
        }

        self.spawn_worker(conv_id, event).await;
    }

    /// Spawn a new worker for the given conversation and send it the first event.
    async fn spawn_worker(&self, conv_id: ConversationId, event: InEvent) {
        let (tx, rx) = mpsc::channel::<InEvent>(WORKER_BUFFER);
        let last_activity = Arc::new(ActivityTimestamp::new());
        last_activity.touch(self.epoch);

        let handle = WorkerHandle {
            tx: tx.clone(),
            last_activity,
        };
        self.workers.insert(conv_id.clone(), handle);

        let pipeline = Arc::clone(&self.pipeline);
        let out_tx = self.out_tx.clone();
        let cancel = self.cancel.clone();
        let channel_name = self.channel_name.clone();
        let worker_conv_id = conv_id.clone();

        tokio::spawn(async move {
            Self::worker_loop(worker_conv_id, rx, pipeline, out_tx, cancel, channel_name).await;
        });

        if tx.send(event).await.is_err() {
            warn!(channel = %self.channel_name, "failed to send to newly-spawned worker");
            self.workers.remove(&conv_id);
        }
    }

    /// The per-conversation worker loop.
    async fn worker_loop(
        conv_id: ConversationId,
        mut rx: mpsc::Receiver<InEvent>,
        pipeline: Arc<dyn PipelineRunner>,
        out_tx: mpsc::Sender<OutEvent>,
        cancel: CancellationToken,
        channel_name: String,
    ) {
        debug!(channel = %channel_name, conversation = %conv_id, "conversation worker started");

        loop {
            tokio::select! {
                msg = tokio::time::timeout(IDLE_TIMEOUT, rx.recv()) => {
                    match msg {
                        Ok(Some(event)) => {
                            match pipeline.process(&event).await {
                                Ok(out_event) => {
                                    if out_tx.send(out_event).await.is_err() {
                                        debug!(
                                            channel = %channel_name,
                                            conversation = %conv_id,
                                            "outbound channel closed, worker exiting"
                                        );
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        channel = %channel_name,
                                        conversation = %conv_id,
                                        "pipeline error: {e}"
                                    );
                                    let err_event = OutEvent {
                                        target: event.source.clone(),
                                        channel_id: event.context.channel_id.clone(),
                                        text: Some(
                                            "I'm having trouble thinking right now, try again in a moment."
                                                .into(),
                                        ),
                                        reply_to: Some(event.message.id.clone()),
                                    };
                                    let _ = out_tx.send(err_event).await;
                                }
                            }
                        }
                        Ok(None) => {
                            // Sender dropped (dispatcher removed us).
                            debug!(
                                channel = %channel_name,
                                conversation = %conv_id,
                                "worker receiver closed, exiting"
                            );
                            break;
                        }
                        Err(_) => {
                            // Idle timeout.
                            info!(
                                channel = %channel_name,
                                conversation = %conv_id,
                                "worker idle for {}s, exiting",
                                IDLE_TIMEOUT.as_secs()
                            );
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    debug!(
                        channel = %channel_name,
                        conversation = %conv_id,
                        "worker cancelled"
                    );
                    break;
                }
            }
        }
    }

    /// Periodic sweep that removes workers idle for longer than [`IDLE_TIMEOUT`].
    async fn eviction_loop(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(EVICTION_INTERVAL) => {
                    // Snapshot keys and idle durations without holding the
                    // DashMap iter guard across any await point.
                    let candidates: Vec<ConversationId> = self
                        .workers
                        .iter()
                        .filter(|entry| {
                            entry.value().last_activity.elapsed_since_last(self.epoch) > IDLE_TIMEOUT
                        })
                        .map(|entry| entry.key().clone())
                        .collect();

                    for conv_id in &candidates {
                        // Dropping the sender causes the worker to exit naturally.
                        self.workers.remove(conv_id);
                    }

                    if !candidates.is_empty() {
                        info!(
                            channel = %self.channel_name,
                            count = candidates.len(),
                            "evicted idle conversation workers"
                        );
                    }
                }
                _ = cancel.cancelled() => {
                    break;
                }
            }
        }
    }

    /// Returns the number of active conversation workers.
    #[cfg(test)]
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event::{
        Author, ChannelSource, ConversationId, InEvent, Message, MessageContext, MessageId,
        OutEvent,
    };
    use crate::core::pipeline::PipelineRunner;
    use anyhow::Result;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration as StdDuration;

    fn make_event(conv_id: ConversationId) -> InEvent {
        InEvent {
            source: ChannelSource::Cli,
            message: Message {
                id: MessageId("msg-1".into()),
                author: Author {
                    id: "user-1".into(),
                    display_name: "Tester".into(),
                },
                text: "hello".into(),
                timestamp: chrono::Utc::now(),
                mentions_bot: false,
            },
            context: MessageContext {
                conversation_id: conv_id,
                channel_id: "test".into(),
                reply_to: None,
            },
            tool_groups: None,
            completion_flag: None,
        }
    }

    /// Pipeline that echoes back instantly.
    struct EchoPipeline;

    impl PipelineRunner for EchoPipeline {
        fn process<'a>(
            &'a self,
            event: &'a InEvent,
        ) -> Pin<Box<dyn Future<Output = Result<OutEvent>> + Send + 'a>> {
            Box::pin(async move {
                Ok(OutEvent {
                    target: event.source.clone(),
                    channel_id: event.context.channel_id.clone(),
                    text: Some(format!("echo: {}", event.message.text)),
                    reply_to: Some(event.message.id.clone()),
                })
            })
        }
    }

    /// Pipeline that sleeps before responding, to test concurrency.
    struct SlowPipeline {
        delay: StdDuration,
        call_count: AtomicUsize,
    }

    impl SlowPipeline {
        fn new(delay: StdDuration) -> Self {
            Self {
                delay,
                call_count: AtomicUsize::new(0),
            }
        }
    }

    impl PipelineRunner for SlowPipeline {
        fn process<'a>(
            &'a self,
            event: &'a InEvent,
        ) -> Pin<Box<dyn Future<Output = Result<OutEvent>> + Send + 'a>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;
                Ok(OutEvent {
                    target: event.source.clone(),
                    channel_id: event.context.channel_id.clone(),
                    text: Some("done".into()),
                    reply_to: Some(event.message.id.clone()),
                })
            })
        }
    }

    #[tokio::test]
    async fn different_conversations_process_concurrently() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutEvent>(64);
        let cancel = CancellationToken::new();
        let pipeline: Arc<dyn PipelineRunner> =
            Arc::new(SlowPipeline::new(StdDuration::from_millis(100)));

        let dispatcher =
            ConversationDispatcher::new(pipeline, out_tx, cancel.clone(), "test".into());

        let conv_a = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "user-a".into(),
        };
        let conv_b = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "user-b".into(),
        };

        let start = tokio::time::Instant::now();

        // Dispatch two events to different conversations.
        dispatcher.dispatch(make_event(conv_a)).await;
        dispatcher.dispatch(make_event(conv_b)).await;

        // Both should complete concurrently, so total time ~100ms, not ~200ms.
        let _r1 = out_rx.recv().await.unwrap();
        let _r2 = out_rx.recv().await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < StdDuration::from_millis(180),
            "expected concurrent processing (~100ms) but took {elapsed:?}"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn slow_conversation_does_not_block_fast_one() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutEvent>(64);
        let cancel = CancellationToken::new();

        // Use a pipeline where each call sleeps — but since workers are isolated,
        // they should process in parallel.
        let pipeline: Arc<dyn PipelineRunner> =
            Arc::new(SlowPipeline::new(StdDuration::from_millis(200)));

        let dispatcher =
            ConversationDispatcher::new(pipeline, out_tx, cancel.clone(), "test".into());

        let slow_conv = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "slow-user".into(),
        };
        let fast_conv = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "fast-user".into(),
        };

        // Send to slow conversation first.
        dispatcher.dispatch(make_event(slow_conv)).await;
        // Then to fast conversation. Both get the same delay but process concurrently.
        tokio::time::sleep(StdDuration::from_millis(10)).await;
        dispatcher.dispatch(make_event(fast_conv)).await;

        // Fast conversation should not wait for slow conversation.
        let r1 = out_rx.recv().await.unwrap();
        let r2 = out_rx.recv().await.unwrap();
        // Both should arrive; we don't care about order.
        assert!(r1.text.is_some());
        assert!(r2.text.is_some());

        cancel.cancel();
    }

    #[tokio::test]
    async fn same_conversation_processes_sequentially() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutEvent>(64);
        let cancel = CancellationToken::new();
        let pipeline: Arc<dyn PipelineRunner> =
            Arc::new(SlowPipeline::new(StdDuration::from_millis(50)));

        let dispatcher =
            ConversationDispatcher::new(pipeline, out_tx, cancel.clone(), "test".into());

        let conv = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "user-1".into(),
        };

        let start = tokio::time::Instant::now();

        // Two events to same conversation — should be sequential.
        dispatcher.dispatch(make_event(conv.clone())).await;
        dispatcher.dispatch(make_event(conv)).await;

        let _r1 = out_rx.recv().await.unwrap();
        let _r2 = out_rx.recv().await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed >= StdDuration::from_millis(90),
            "expected sequential processing (>=100ms) but took {elapsed:?}"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn echo_pipeline_works_through_dispatcher() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutEvent>(64);
        let cancel = CancellationToken::new();
        let pipeline: Arc<dyn PipelineRunner> = Arc::new(EchoPipeline);

        let dispatcher =
            ConversationDispatcher::new(pipeline, out_tx, cancel.clone(), "test".into());

        let conv = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "user-1".into(),
        };

        dispatcher.dispatch(make_event(conv)).await;

        let result = tokio::time::timeout(StdDuration::from_secs(1), out_rx.recv())
            .await
            .expect("timeout")
            .expect("no event");

        assert_eq!(result.text, Some("echo: hello".into()));
        cancel.cancel();
    }
}
