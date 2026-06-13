//! Per-conversation worker dispatcher.
//!
//! Routes inbound events to isolated per-conversation workers so that a slow
//! conversation (e.g. long tool-call loop) does not block other conversations
//! on the same channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
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
    /// Set while the worker is inside `pipeline.process`. A busy worker is never
    /// evicted, so a single long call (one slow tool loop exceeding
    /// `IDLE_TIMEOUT`) can't be reaped mid-flight — which would otherwise let
    /// `dispatch` spawn a second worker and process the conversation twice.
    busy: Arc<AtomicBool>,
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
    /// How long a worker may sit idle before eviction. Defaults to
    /// [`IDLE_TIMEOUT`]; overridable in tests for deterministic eviction.
    idle_timeout: Duration,
    /// Tracks spawned per-conversation worker tasks so shutdown can wait for
    /// in-flight processing to finish (see [`ConversationDispatcher::drain`]).
    tracker: TaskTracker,
}

impl ConversationDispatcher {
    /// Create a new dispatcher. Also spawns the background eviction task.
    pub fn new(
        pipeline: Arc<dyn PipelineRunner>,
        out_tx: mpsc::Sender<OutEvent>,
        cancel: CancellationToken,
        channel_name: String,
    ) -> Arc<Self> {
        Self::new_with_idle_timeout(pipeline, out_tx, cancel, channel_name, IDLE_TIMEOUT)
    }

    /// Like [`new`](Self::new) but with an explicit idle timeout. The public
    /// constructor uses [`IDLE_TIMEOUT`]; tests use a tiny value to exercise the
    /// eviction sweep deterministically.
    fn new_with_idle_timeout(
        pipeline: Arc<dyn PipelineRunner>,
        out_tx: mpsc::Sender<OutEvent>,
        cancel: CancellationToken,
        channel_name: String,
        idle_timeout: Duration,
    ) -> Arc<Self> {
        let dispatcher = Arc::new(Self {
            workers: Arc::new(DashMap::new()),
            pipeline,
            out_tx,
            cancel: cancel.clone(),
            channel_name,
            epoch: Instant::now(),
            idle_timeout,
            tracker: TaskTracker::new(),
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
    ///
    /// Uses a non-blocking `try_send` so a single saturated conversation can
    /// never stall the dispatcher loop (and thus every other conversation).
    /// When a worker's buffer is full the message is load-shed (dropped with a
    /// warning) rather than blocking.
    pub async fn dispatch(&self, event: InEvent) {
        let conv_id = event.context.conversation_id.clone();

        // Fast path: worker already exists and channel is open.
        // Clone tx and activity out of the DashMap guard so we don't hold the
        // shard lock while sending.
        let existing = self.workers.get(&conv_id).and_then(|handle| {
            if handle.tx.is_closed() {
                None
            } else {
                Some((handle.tx.clone(), Arc::clone(&handle.last_activity)))
            }
        });

        if let Some((tx, activity)) = existing {
            activity.touch(self.epoch);
            match tx.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Closed(event)) => {
                    // Worker is gone — spawn a fresh one with the recovered event.
                    self.workers.remove(&conv_id);
                    self.spawn_worker(conv_id, event).await;
                }
                Err(mpsc::error::TrySendError::Full(_event)) => {
                    // Conversation is saturated. Load-shed: drop the message
                    // rather than block the dispatcher loop (which would back up
                    // every other conversation behind this one).
                    warn!(
                        channel = %self.channel_name,
                        conversation = %conv_id,
                        "per-conversation buffer full, dropping message"
                    );
                }
            }
            return;
        }

        self.spawn_worker(conv_id, event).await;
    }

    /// Spawn a new worker for the given conversation and send it the first event.
    async fn spawn_worker(&self, conv_id: ConversationId, event: InEvent) {
        let (tx, rx) = mpsc::channel::<InEvent>(WORKER_BUFFER);
        let last_activity = Arc::new(ActivityTimestamp::new());
        last_activity.touch(self.epoch);
        let busy = Arc::new(AtomicBool::new(false));

        let handle = WorkerHandle {
            tx: tx.clone(),
            last_activity: Arc::clone(&last_activity),
            busy: Arc::clone(&busy),
        };
        self.workers.insert(conv_id.clone(), handle);

        let pipeline = Arc::clone(&self.pipeline);
        let out_tx = self.out_tx.clone();
        let cancel = self.cancel.clone();
        let channel_name = self.channel_name.clone();
        let worker_conv_id = conv_id.clone();
        let epoch = self.epoch;
        let idle_timeout = self.idle_timeout;

        // Spawn via the TaskTracker so shutdown can wait for in-flight work.
        self.tracker.spawn(async move {
            Self::worker_loop(
                worker_conv_id,
                rx,
                pipeline,
                out_tx,
                cancel,
                channel_name,
                last_activity,
                busy,
                epoch,
                idle_timeout,
            )
            .await;
        });

        if tx.send(event).await.is_err() {
            warn!(channel = %self.channel_name, "failed to send to newly-spawned worker");
            self.workers.remove(&conv_id);
        }
    }

    /// The per-conversation worker loop.
    #[allow(clippy::too_many_arguments)]
    async fn worker_loop(
        conv_id: ConversationId,
        mut rx: mpsc::Receiver<InEvent>,
        pipeline: Arc<dyn PipelineRunner>,
        out_tx: mpsc::Sender<OutEvent>,
        cancel: CancellationToken,
        channel_name: String,
        last_activity: Arc<ActivityTimestamp>,
        busy: Arc<AtomicBool>,
        epoch: Instant,
        idle_timeout: Duration,
    ) {
        debug!(channel = %channel_name, conversation = %conv_id, "conversation worker started");

        loop {
            tokio::select! {
                msg = tokio::time::timeout(idle_timeout, rx.recv()) => {
                    match msg {
                        Ok(Some(event)) => {
                            // Mark busy across the whole call so the eviction sweep
                            // can't reap this worker even if a single `process`
                            // outlives IDLE_TIMEOUT; refresh activity afterwards so
                            // a worker between backlog items also stays fresh.
                            // Together these stop dispatch() from ever spawning a
                            // duplicate worker for this conversation.
                            busy.store(true, Ordering::Relaxed);
                            let result = pipeline.process(&event).await;
                            last_activity.touch(epoch);
                            busy.store(false, Ordering::Relaxed);
                            match result {
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
                                idle_timeout.as_secs()
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
                    let evicted = self.evict_idle();
                    if evicted > 0 {
                        info!(
                            channel = %self.channel_name,
                            count = evicted,
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

    /// Remove workers idle longer than `idle_timeout`, returning the count.
    ///
    /// A worker currently inside `pipeline.process` (`busy`) is never evicted,
    /// even if a single call outlives the timeout. The predicate is re-checked
    /// atomically under the shard lock via `remove_if`, so a worker that became
    /// busy or received an event between the snapshot and the removal is left
    /// alone — otherwise `dispatch` could spawn a duplicate worker for it and
    /// process the conversation concurrently.
    fn evict_idle(&self) -> usize {
        let idle = |w: &WorkerHandle| {
            !w.busy.load(Ordering::Relaxed)
                && w.last_activity.elapsed_since_last(self.epoch) > self.idle_timeout
        };

        // Snapshot candidate keys without holding the iter guard across removals.
        let candidates: Vec<ConversationId> = self
            .workers
            .iter()
            .filter(|entry| idle(entry.value()))
            .map(|entry| entry.key().clone())
            .collect();

        let mut evicted = 0usize;
        for conv_id in &candidates {
            // Dropping the sender (on removal) causes the worker to exit naturally.
            if self.workers.remove_if(conv_id, |_, w| idle(w)).is_some() {
                evicted += 1;
            }
        }
        evicted
    }

    /// Wait for all in-flight conversation workers to finish.
    ///
    /// Closes the task tracker (so no new worker is accepted) and awaits every
    /// spawned worker. Workers exit promptly once the dispatcher's
    /// [`CancellationToken`] is cancelled, so callers should cancel first.
    /// This lets shutdown wait for an in-flight LLM call / history write to
    /// complete instead of the process exiting mid-work.
    pub async fn drain(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }

    /// Returns the number of active conversation workers.
    #[cfg(test)]
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Returns how long ago a conversation's worker last touched its activity
    /// timestamp, or `None` if no worker exists. Used to verify the
    /// touch-after-process path (CORE-6) without mutating production timeouts.
    #[cfg(test)]
    pub fn elapsed_since_activity(&self, conv_id: &ConversationId) -> Option<Duration> {
        self.workers
            .get(conv_id)
            .map(|h| h.last_activity.elapsed_since_last(self.epoch))
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
                guild_id: None,
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

    /// Pipeline whose first call blocks until `release` is triggered, used to
    /// deterministically hold a worker "in flight" without racing wall-clock
    /// sleeps. Counts how many calls actually completed.
    struct GatedPipeline {
        release: tokio::sync::Notify,
        started: tokio::sync::Notify,
        completed: AtomicUsize,
    }

    impl GatedPipeline {
        fn new() -> Self {
            Self {
                release: tokio::sync::Notify::new(),
                started: tokio::sync::Notify::new(),
                completed: AtomicUsize::new(0),
            }
        }
    }

    impl PipelineRunner for GatedPipeline {
        fn process<'a>(
            &'a self,
            event: &'a InEvent,
        ) -> Pin<Box<dyn Future<Output = Result<OutEvent>> + Send + 'a>> {
            Box::pin(async move {
                // Signal that processing has begun, then block until released.
                self.started.notify_one();
                self.release.notified().await;
                self.completed.fetch_add(1, Ordering::SeqCst);
                Ok(OutEvent {
                    target: event.source.clone(),
                    channel_id: event.context.channel_id.clone(),
                    text: Some("released".into()),
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

    /// CORE-6: a worker refreshes `last_activity` *after* processing each event,
    /// so an idle-based eviction sweep won't remove a worker that is actively
    /// draining a backlog (which would let dispatch spawn a duplicate worker for
    /// the same conversation).
    ///
    /// We prove the touch-after-process path two ways:
    /// 1. The worker's activity timestamp is fresh *after* the processing time
    ///    has elapsed — not stale from the dispatch-time touch only.
    /// 2. A single worker handles the whole backlog (no duplicate spawned).
    #[tokio::test]
    async fn worker_touches_activity_after_processing() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutEvent>(64);
        let cancel = CancellationToken::new();
        // 40ms per event; with 3 events the worker is busy ~120ms total.
        let slow_pipeline = Arc::new(SlowPipeline::new(StdDuration::from_millis(40)));
        let pipeline: Arc<dyn PipelineRunner> = slow_pipeline.clone();

        let dispatcher =
            ConversationDispatcher::new(pipeline, out_tx, cancel.clone(), "test".into());

        let conv = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "backlog-user".into(),
        };

        // Queue a backlog of events to the same conversation. All touches at
        // dispatch time happen up front, before the worker does its slow work.
        for _ in 0..3 {
            dispatcher.dispatch(make_event(conv.clone())).await;
        }

        // Drain all 3 outputs.
        for _ in 0..3 {
            tokio::time::timeout(StdDuration::from_secs(2), out_rx.recv())
                .await
                .expect("timeout waiting for output")
                .expect("worker produced no output");
        }

        // After ~120ms of processing, the activity timestamp must be recent
        // (touched right after the last process call), NOT ~120ms+ stale as it
        // would be if only dispatch() touched it before the work ran.
        let elapsed = dispatcher
            .elapsed_since_activity(&conv)
            .expect("worker should still exist");
        assert!(
            elapsed < StdDuration::from_millis(40),
            "worker must touch activity after processing; elapsed={elapsed:?} \
             implies the timestamp went stale during the backlog"
        );

        // Exactly one worker handled the whole backlog — no duplicate spawned.
        assert_eq!(
            dispatcher.worker_count(),
            1,
            "a single worker must handle the whole backlog (no duplicate)"
        );
        assert_eq!(
            slow_pipeline.call_count.load(Ordering::SeqCst),
            3,
            "all backlog events processed by one worker"
        );

        cancel.cancel();
    }

    /// CORE-7: a full per-conversation buffer on conversation A must not block
    /// dispatch to conversation B. We saturate A (worker stuck on a gated
    /// pipeline + buffer full), then dispatch to B and assert B is processed
    /// promptly.
    #[tokio::test]
    async fn full_buffer_on_one_conversation_does_not_block_another() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutEvent>(256);
        let cancel = CancellationToken::new();
        let gated = Arc::new(GatedPipeline::new());
        let pipeline: Arc<dyn PipelineRunner> = gated.clone();

        let dispatcher =
            ConversationDispatcher::new(pipeline, out_tx, cancel.clone(), "test".into());

        let conv_a = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "saturated-a".into(),
        };
        let conv_b = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "fast-b".into(),
        };

        // First event to A: worker spawns and immediately blocks in the gated
        // pipeline. Wait until it has actually started processing.
        dispatcher.dispatch(make_event(conv_a.clone())).await;
        gated.started.notified().await;

        // Now flood A well past WORKER_BUFFER (64). With try_send + load-shed,
        // none of these block the dispatcher; the surplus is dropped.
        for _ in 0..(WORKER_BUFFER + 50) {
            dispatcher.dispatch(make_event(conv_a.clone())).await;
        }

        // Dispatch to B; its worker should spawn and start independently even
        // though A is fully saturated and blocked.
        dispatcher.dispatch(make_event(conv_b.clone())).await;

        // B's gated worker should start promptly (proves dispatch to B was not
        // blocked behind A's full buffer). The GatedPipeline notifies `started`
        // for every call; A's in-flight call already consumed one notification
        // above, and B's first call produces the next one.
        tokio::time::timeout(StdDuration::from_secs(2), gated.started.notified())
            .await
            .expect("conversation B was blocked behind saturated A");

        // Release everything so both workers can drain and exit cleanly.
        for _ in 0..(WORKER_BUFFER * 4) {
            gated.release.notify_one();
        }
        // Sanity: at least one output is produced once released.
        let _ = tokio::time::timeout(StdDuration::from_secs(2), out_rx.recv()).await;

        cancel.cancel();
    }

    /// CORE-6: a worker that is mid-`pipeline.process` must NOT be evicted, even
    /// if the call outlives the idle timeout — otherwise dispatch would spawn a
    /// second worker and process the same conversation concurrently. We pin a
    /// worker in a gated (blocked) call with a ~zero idle timeout and assert the
    /// eviction sweep leaves it alone while busy, then reaps it once idle.
    #[tokio::test]
    async fn busy_worker_survives_eviction_until_idle() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutEvent>(64);
        let cancel = CancellationToken::new();
        let gated = Arc::new(GatedPipeline::new());
        let pipeline: Arc<dyn PipelineRunner> = gated.clone();

        // Idle timeout of zero: an unblocked worker is immediately "idle", so the
        // ONLY thing preventing eviction here is the busy guard.
        let dispatcher = ConversationDispatcher::new_with_idle_timeout(
            pipeline,
            out_tx,
            cancel.clone(),
            "test".into(),
            StdDuration::ZERO,
        );

        let conv = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "busy-user".into(),
        };

        dispatcher.dispatch(make_event(conv.clone())).await;
        // Wait until the worker is actually inside the (blocked) pipeline call.
        gated.started.notified().await;

        // Despite the zero idle timeout, the busy worker must survive eviction.
        assert_eq!(
            dispatcher.evict_idle(),
            0,
            "a worker mid-process must not be evicted"
        );
        assert_eq!(dispatcher.worker_count(), 1);

        // Release and let the call complete, clearing busy.
        gated.release.notify_one();
        let _ = out_rx.recv().await.expect("released output");
        // Ensure measurable idle time elapses (millisecond-resolution timestamp).
        tokio::time::sleep(StdDuration::from_millis(5)).await;

        // Now genuinely idle → the sweep reaps it.
        assert_eq!(
            dispatcher.evict_idle(),
            1,
            "an idle worker should be evicted"
        );

        cancel.cancel();
    }

    /// LIFE-7: an in-flight worker must finish its current pipeline call when
    /// shutdown drains, rather than being dropped. We dispatch to a gated
    /// worker, cancel, then in parallel release the gate and call `drain`, and
    /// assert the OutEvent was produced.
    #[tokio::test]
    async fn drain_waits_for_in_flight_worker() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutEvent>(64);
        let cancel = CancellationToken::new();
        let gated = Arc::new(GatedPipeline::new());
        let pipeline: Arc<dyn PipelineRunner> = gated.clone();

        let dispatcher =
            ConversationDispatcher::new(pipeline, out_tx, cancel.clone(), "test".into());

        let conv = ConversationId::Dm {
            channel_type: ChannelSource::Cli,
            user_id: "inflight-user".into(),
        };

        dispatcher.dispatch(make_event(conv)).await;
        // Wait until the worker is actually mid-process.
        gated.started.notified().await;

        // Cancel: a naive shutdown would now drop the worker mid-call.
        cancel.cancel();

        // Release the gate shortly after starting the drain, then drain.
        let gated_release = gated.clone();
        let releaser = tokio::spawn(async move {
            tokio::time::sleep(StdDuration::from_millis(20)).await;
            gated_release.release.notify_one();
        });

        // drain() must wait for the in-flight call to complete.
        dispatcher.drain().await;
        releaser.await.unwrap();

        // The completed call must have produced its OutEvent.
        assert_eq!(
            gated.completed.load(Ordering::SeqCst),
            1,
            "in-flight pipeline call must complete during drain, not be dropped"
        );
        let out = out_rx.try_recv().expect("OutEvent should have been produced");
        assert_eq!(out.text, Some("released".into()));
    }
}
