use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::{debug, trace};

use crate::core::event::InEvent;

/// A boxed future used throughout the mode API for dyn-compatibility.
type ModeFuture<'a> = Pin<Box<dyn std::future::Future<Output = Vec<InEvent>> + Send + 'a>>;

/// Controls when messages are dispatched to the core pipeline.
///
/// Modes are adapter-independent — any channel adapter can use any mode.
/// A `ModeRouter` maps group IDs to mode instances per adapter.
///
/// Uses boxed futures for dyn-compatibility (required by `ModeRouter`).
pub trait ResponseMode: Send + Sync {
    /// Called when a new message arrives. Returns messages to dispatch now, if any.
    fn on_message(&self, event: InEvent) -> ModeFuture<'_>;

    /// Called periodically (e.g., every second). Returns buffered messages ready for dispatch.
    fn on_tick(&self) -> ModeFuture<'_>;
}

/// Dispatches every message immediately.
pub struct AlwaysMode;

impl ResponseMode for AlwaysMode {
    fn on_message(&self, event: InEvent) -> ModeFuture<'_> {
        Box::pin(async move { vec![event] })
    }

    fn on_tick(&self) -> ModeFuture<'_> {
        Box::pin(async { vec![] })
    }
}

/// Dispatches only messages that mention Aurora. Drops everything else.
pub struct MentionOnlyMode;

impl ResponseMode for MentionOnlyMode {
    fn on_message(&self, event: InEvent) -> ModeFuture<'_> {
        Box::pin(async move {
            if event.message.mentions_bot {
                vec![event]
            } else {
                trace!(
                    author = %event.message.author.display_name,
                    "dropping non-mention message in mention-only mode"
                );
                vec![]
            }
        })
    }

    fn on_tick(&self) -> ModeFuture<'_> {
        Box::pin(async { vec![] })
    }
}

/// Buffers messages and dispatches them as a batch on interval or debounce.
///
/// - `digest_interval`: max time between dispatches (fires even if messages keep arriving)
/// - `digest_debounce`: silence period after last message before dispatching
/// - @mentions bypass the buffer and dispatch immediately
pub struct DigestMode {
    digest_interval: Duration,
    digest_debounce: Duration,
    state: Mutex<DigestState>,
}

struct DigestState {
    buffer: Vec<InEvent>,
    last_message_at: Option<Instant>,
    last_dispatch_at: Instant,
}

impl DigestMode {
    pub fn new(digest_interval: Duration, digest_debounce: Duration) -> Self {
        Self {
            digest_interval,
            digest_debounce,
            state: Mutex::new(DigestState {
                buffer: Vec::new(),
                last_message_at: None,
                last_dispatch_at: Instant::now(),
            }),
        }
    }

    /// Drain the buffer and reset timers.
    async fn flush(&self) -> Vec<InEvent> {
        let mut state = self.state.lock().await;
        if state.buffer.is_empty() {
            return vec![];
        }
        let events = std::mem::take(&mut state.buffer);
        state.last_message_at = None;
        state.last_dispatch_at = Instant::now();
        debug!(count = events.len(), "digest mode flushing buffer");
        events
    }
}

impl ResponseMode for DigestMode {
    fn on_message(&self, event: InEvent) -> ModeFuture<'_> {
        Box::pin(async move {
            // @mentions bypass the buffer for immediate processing
            if event.message.mentions_bot {
                debug!(
                    author = %event.message.author.display_name,
                    "mention in digest mode — bypassing buffer"
                );
                return vec![event];
            }

            let mut state = self.state.lock().await;
            state.last_message_at = Some(Instant::now());
            state.buffer.push(event);
            vec![]
        })
    }

    fn on_tick(&self) -> ModeFuture<'_> {
        Box::pin(async move {
            let should_flush = {
                let state = self.state.lock().await;
                if state.buffer.is_empty() {
                    return vec![];
                }

                let now = Instant::now();

                // Interval trigger: enough time since last dispatch
                let interval_elapsed =
                    now.duration_since(state.last_dispatch_at) >= self.digest_interval;

                // Debounce trigger: enough silence since last message
                let debounce_elapsed = state
                    .last_message_at
                    .is_some_and(|t| now.duration_since(t) >= self.digest_debounce);

                interval_elapsed || debounce_elapsed
            };

            if should_flush {
                self.flush().await
            } else {
                vec![]
            }
        })
    }
}

/// Routes messages to the appropriate `ResponseMode` based on group ID.
///
/// Each adapter gets one `ModeRouter`. It maps group IDs to mode instances.
/// A wildcard `"*"` entry serves as the default for unconfigured groups.
pub struct ModeRouter {
    modes: HashMap<String, Arc<dyn ResponseMode>>,
    default_mode: Arc<dyn ResponseMode>,
}

impl ModeRouter {
    pub fn new(
        modes: HashMap<String, Arc<dyn ResponseMode>>,
        default_mode: Arc<dyn ResponseMode>,
    ) -> Self {
        Self {
            modes,
            default_mode,
        }
    }

    /// Get the mode for a given group ID, falling back to the default.
    fn mode_for(&self, group_id: &str) -> &Arc<dyn ResponseMode> {
        self.modes.get(group_id).unwrap_or(&self.default_mode)
    }

    /// Route a message through its group's mode.
    pub async fn on_message(&self, group_id: &str, event: InEvent) -> Vec<InEvent> {
        self.mode_for(group_id).on_message(event).await
    }

    /// Tick all modes, collecting any ready events.
    pub async fn on_tick(&self) -> Vec<InEvent> {
        let mut events = Vec::new();
        for mode in self.modes.values() {
            events.extend(mode.on_tick().await);
        }
        // Also tick the default mode (it may have buffered messages for unconfigured groups)
        events.extend(self.default_mode.on_tick().await);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event::{
        Author, ChannelSource, ConversationId, Message, MessageContext, MessageId,
    };
    use chrono::Utc;

    fn make_event(mentions_bot: bool, group_id: &str) -> InEvent {
        InEvent {
            source: ChannelSource::Discord,
            message: Message {
                id: MessageId("msg1".into()),
                author: Author {
                    id: "user1".into(),
                    display_name: "TestUser".into(),
                },
                text: "hello".into(),
                timestamp: Utc::now(),
                mentions_bot,
            },
            context: MessageContext {
                conversation_id: ConversationId::Group {
                    channel_type: ChannelSource::Discord,
                    group_id: group_id.into(),
                },
                channel_id: group_id.into(),
                reply_to: None,
            },
            tool_groups: None,
            completion_flag: None,
        }
    }

    #[tokio::test]
    async fn always_mode_dispatches_immediately() {
        let mode = AlwaysMode;
        let events = mode.on_message(make_event(false, "general")).await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn always_mode_tick_is_noop() {
        let mode = AlwaysMode;
        let events = mode.on_tick().await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn mention_only_drops_non_mentions() {
        let mode = MentionOnlyMode;
        let events = mode.on_message(make_event(false, "general")).await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn mention_only_dispatches_mentions() {
        let mode = MentionOnlyMode;
        let events = mode.on_message(make_event(true, "general")).await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn digest_buffers_non_mentions() {
        let mode = DigestMode::new(Duration::from_secs(60), Duration::from_secs(10));
        let events = mode.on_message(make_event(false, "general")).await;
        assert!(events.is_empty());

        // Buffer should have one message
        let state = mode.state.lock().await;
        assert_eq!(state.buffer.len(), 1);
    }

    #[tokio::test]
    async fn digest_bypasses_buffer_on_mention() {
        let mode = DigestMode::new(Duration::from_secs(60), Duration::from_secs(10));
        let events = mode.on_message(make_event(true, "general")).await;
        assert_eq!(events.len(), 1);

        // Buffer should be empty — mention bypassed it
        let state = mode.state.lock().await;
        assert!(state.buffer.is_empty());
    }

    #[tokio::test]
    async fn digest_tick_empty_buffer_returns_nothing() {
        let mode = DigestMode::new(Duration::from_secs(60), Duration::from_secs(10));
        let events = mode.on_tick().await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn digest_flushes_on_debounce() {
        let mode = DigestMode::new(
            Duration::from_secs(3600), // long interval — won't trigger
            Duration::from_millis(50), // short debounce
        );

        // Buffer a message
        mode.on_message(make_event(false, "general")).await;

        // Wait for debounce
        tokio::time::sleep(Duration::from_millis(60)).await;

        let events = mode.on_tick().await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn digest_flushes_on_interval() {
        let mode = DigestMode::new(
            Duration::from_millis(50), // short interval
            Duration::from_secs(3600), // long debounce — won't trigger
        );

        // Set last_dispatch_at to the past
        {
            let mut state = mode.state.lock().await;
            state.last_dispatch_at = Instant::now() - Duration::from_millis(100);
        }

        // Buffer a message
        mode.on_message(make_event(false, "general")).await;

        let events = mode.on_tick().await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn mode_router_routes_to_correct_mode() {
        let mut modes: HashMap<String, Arc<dyn ResponseMode>> = HashMap::new();
        modes.insert("quiet".into(), Arc::new(MentionOnlyMode));

        let router = ModeRouter::new(modes, Arc::new(AlwaysMode));

        // "quiet" group uses mention-only: non-mention dropped
        let events = router.on_message("quiet", make_event(false, "quiet")).await;
        assert!(events.is_empty());

        // Unknown group uses default (always): dispatched
        let events = router.on_message("other", make_event(false, "other")).await;
        assert_eq!(events.len(), 1);
    }
}
