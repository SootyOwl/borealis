use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
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

/// Factory that creates a `ResponseMode` instance. Used by `ModeRouter` to
/// lazily create per-channel mode instances from guild defaults.
pub trait ModeFactory: Send + Sync {
    fn create(&self) -> Arc<dyn ResponseMode>;
}

/// A factory that always creates the same kind of mode from stored config.
pub struct ConfigModeFactory {
    mode_name: String,
    digest_interval: Duration,
    digest_debounce: Duration,
}

impl ConfigModeFactory {
    pub fn new(mode_name: &str, interval_min: Option<u64>, debounce_min: Option<u64>) -> Self {
        Self {
            mode_name: mode_name.to_string(),
            digest_interval: Duration::from_secs(interval_min.unwrap_or(5) * 60),
            digest_debounce: Duration::from_secs(debounce_min.unwrap_or(2) * 60),
        }
    }
}

impl ModeFactory for ConfigModeFactory {
    fn create(&self) -> Arc<dyn ResponseMode> {
        match self.mode_name.as_str() {
            "mention-only" => Arc::new(MentionOnlyMode),
            "digest" => Arc::new(DigestMode::new(self.digest_interval, self.digest_debounce)),
            _ => Arc::new(AlwaysMode),
        }
    }
}

/// Routes messages to the appropriate `ResponseMode` based on channel/guild ID.
///
/// Lookup order: channel_id → guild_id → global default.
///
/// Channels not explicitly configured get a lazily-created mode instance
/// from their guild's factory, ensuring each channel has its own independent
/// mode state (important for DigestMode's per-channel buffers).
pub struct ModeRouter {
    /// Explicitly configured modes (channel overrides + guild defaults used
    /// as templates for the factory only).
    channel_modes: DashMap<String, Arc<dyn ResponseMode>>,
    /// Factories keyed by guild_id, used to create per-channel modes on the fly.
    guild_factories: HashMap<String, Arc<dyn ModeFactory>>,
    /// Global default factory for channels in unconfigured guilds.
    default_factory: Arc<dyn ModeFactory>,
}

impl ModeRouter {
    pub fn new(
        channel_modes: HashMap<String, Arc<dyn ResponseMode>>,
        guild_factories: HashMap<String, Arc<dyn ModeFactory>>,
        default_factory: Arc<dyn ModeFactory>,
    ) -> Self {
        Self {
            channel_modes: channel_modes.into_iter().collect(),
            guild_factories,
            default_factory,
        }
    }

    /// Get or create the mode for a channel.
    fn mode_for(&self, channel_id: &str, guild_id: &str) -> Arc<dyn ResponseMode> {
        // Check if this channel already has a mode (explicit or previously created).
        if let Some(mode) = self.channel_modes.get(channel_id) {
            return Arc::clone(mode.value());
        }

        // Create a new mode from the guild factory (or global default).
        let factory = self
            .guild_factories
            .get(guild_id)
            .unwrap_or(&self.default_factory);
        let mode = factory.create();
        self.channel_modes
            .insert(channel_id.to_string(), Arc::clone(&mode));
        debug!(channel = channel_id, guild = guild_id, "created mode for new channel");
        mode
    }

    /// Route a message through its mode. Checks channel_id first, then
    /// creates from guild factory if needed.
    pub async fn on_message(
        &self,
        channel_id: &str,
        guild_id: &str,
        event: InEvent,
    ) -> Vec<InEvent> {
        self.mode_for(channel_id, guild_id).on_message(event).await
    }

    /// Tick all active modes, collecting any ready events.
    pub async fn on_tick(&self) -> Vec<InEvent> {
        // Snapshot the mode Arcs to avoid holding DashMap locks across .await.
        let modes: Vec<Arc<dyn ResponseMode>> = self
            .channel_modes
            .iter()
            .map(|entry| Arc::clone(entry.value()))
            .collect();

        let mut events = Vec::new();
        for mode in &modes {
            events.extend(mode.on_tick().await);
        }
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

    /// Helper: build a simple factory for tests.
    struct FixedModeFactory(Arc<dyn ResponseMode>);
    impl ModeFactory for FixedModeFactory {
        fn create(&self) -> Arc<dyn ResponseMode> {
            // For stateless modes we can clone the Arc; for DigestMode tests
            // each call should create a new instance.
            Arc::clone(&self.0)
        }
    }

    fn fixed_factory(mode: impl ResponseMode + 'static) -> Arc<dyn ModeFactory> {
        Arc::new(FixedModeFactory(Arc::new(mode)))
    }

    #[tokio::test]
    async fn mode_router_routes_to_correct_mode() {
        // Guild "quiet" uses mention-only
        let mut guild_factories: HashMap<String, Arc<dyn ModeFactory>> = HashMap::new();
        guild_factories.insert("quiet".into(), fixed_factory(MentionOnlyMode));

        let router = ModeRouter::new(
            HashMap::new(),
            guild_factories,
            fixed_factory(AlwaysMode),
        );

        // Channel in "quiet" guild: mention-only (non-mention dropped)
        let events = router
            .on_message("chan1", "quiet", make_event(false, "quiet"))
            .await;
        assert!(events.is_empty());

        // Channel in unknown guild: default (always, dispatched)
        let events = router
            .on_message("chan2", "other", make_event(false, "other"))
            .await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn mode_router_channel_override_takes_priority() {
        // Guild default: mention-only
        let mut guild_factories: HashMap<String, Arc<dyn ModeFactory>> = HashMap::new();
        guild_factories.insert("guild1".into(), fixed_factory(MentionOnlyMode));

        // Channel override: always
        let mut channel_modes: HashMap<String, Arc<dyn ResponseMode>> = HashMap::new();
        channel_modes.insert("chan-override".into(), Arc::new(AlwaysMode));

        let router = ModeRouter::new(
            channel_modes,
            guild_factories,
            fixed_factory(MentionOnlyMode),
        );

        // Overridden channel: dispatched (always mode wins)
        let events = router
            .on_message("chan-override", "guild1", make_event(false, "chan-override"))
            .await;
        assert_eq!(events.len(), 1);

        // Non-overridden channel: lazily created from guild factory (mention-only, dropped)
        let events = router
            .on_message("chan-other", "guild1", make_event(false, "chan-other"))
            .await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn mode_router_creates_independent_digest_per_channel() {
        // Guild uses digest mode — each channel should get its own buffer
        let mut guild_factories: HashMap<String, Arc<dyn ModeFactory>> = HashMap::new();
        guild_factories.insert(
            "guild1".into(),
            Arc::new(ConfigModeFactory::new("digest", Some(60), Some(5))),
        );

        let router = ModeRouter::new(
            HashMap::new(),
            guild_factories,
            fixed_factory(AlwaysMode),
        );

        // Send message to chan-a
        let events = router
            .on_message("chan-a", "guild1", make_event(false, "chan-a"))
            .await;
        assert!(events.is_empty()); // buffered

        // Send message to chan-b
        let events = router
            .on_message("chan-b", "guild1", make_event(false, "chan-b"))
            .await;
        assert!(events.is_empty()); // buffered in separate instance

        // Verify they have independent modes (2 entries in channel_modes)
        assert_eq!(router.channel_modes.len(), 2);
    }
}
