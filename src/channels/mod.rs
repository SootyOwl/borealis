pub mod cli;
pub mod discord;
pub mod dispatcher;
pub mod modes;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::config::Settings;
use crate::core::event::{ChannelSource, InEvent, OutEvent};
use crate::core::pipeline::PipelineRunner;
use crate::security::Security;
use crate::tools::DiscordHttpHandle;

use self::dispatcher::ConversationDispatcher;

/// Runtime dependencies available to channel registration functions.
pub struct ChannelDeps<'a> {
    pub settings: &'a Settings,
    pub pipeline: Arc<dyn PipelineRunner>,
    pub cancel: CancellationToken,
    pub security: Arc<Security>,
    pub discord_http: DiscordHttpHandle,
}

/// A self-registering channel adapter.
///
/// Each channel module submits one of these via `inventory::submit!`. They are
/// collected and executed by [`register_all_channels`].
pub struct ChannelRegistration {
    pub name: &'static str,
    pub register_fn: fn(&mut ChannelRegistry, &ChannelDeps<'_>),
}

inventory::collect!(ChannelRegistration);

/// Register all channel adapters discovered via `inventory`.
///
/// Each channel module submits a [`ChannelRegistration`] at link time. This
/// function iterates them and calls each registration function with the shared deps.
pub fn register_all_channels(
    settings: &Settings,
    pipeline: Arc<dyn PipelineRunner>,
    cancel: CancellationToken,
    security: Arc<Security>,
    discord_http: DiscordHttpHandle,
) -> ChannelRegistry {
    let deps = ChannelDeps {
        settings,
        pipeline,
        cancel,
        security,
        discord_http,
    };
    let mut registry = ChannelRegistry::new();
    for reg in inventory::iter::<ChannelRegistration> {
        tracing::debug!(channel = reg.name, "registering channel");
        (reg.register_fn)(&mut registry, &deps);
    }
    registry
}

/// A channel adapter that bridges a platform (Discord, CLI, etc.) with the core event bus.
///
/// Split into two async methods so inbound and outbound run as separate tasks.
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;

    /// Listen for inbound messages and send `InEvent`s to the core.
    fn run_inbound(
        self: Arc<Self>,
        tx: Sender<InEvent>,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// Consume outbound events and dispatch them to the platform.
    fn run_outbound(
        self: Arc<Self>,
        rx: Receiver<OutEvent>,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// Default bounded channel capacity for per-channel mpsc channels.
const CHANNEL_BUFFER: usize = 256;

/// A spawn coordinator that replaces hardcoded channel wiring in `main.rs`.
///
/// Each registered channel gets its own inbound, outbound, and processing loop
/// spawned automatically. Adding a new channel means implementing the [`Channel`]
/// trait and calling [`ChannelRegistry::register`].
pub struct ChannelRegistry {
    handles: Vec<(String, Vec<JoinHandle<()>>)>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    /// Register a channel adapter and spawn its inbound, outbound, and processing tasks.
    ///
    /// This is generic over the concrete channel type — no trait object or erased wrapper
    /// needed since channels are registered at startup, not dynamically.
    pub fn register<C: Channel + 'static>(
        &mut self,
        channel: Arc<C>,
        pipeline: Arc<dyn PipelineRunner>,
        cancel: CancellationToken,
        security: Option<Arc<Security>>,
    ) {
        let name = channel.name().to_string();
        let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<InEvent>(CHANNEL_BUFFER);
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<OutEvent>(CHANNEL_BUFFER);

        // Spawn inbound task.
        let inbound_handle = {
            let ch = channel.clone();
            let cancel = cancel.clone();
            let name = name.clone();
            tokio::spawn(async move {
                tokio::select! {
                    result = Channel::run_inbound(ch, in_tx) => {
                        if let Err(e) = result {
                            error!(channel = %name, "inbound error: {e}");
                        }
                    }
                    _ = cancel.cancelled() => {
                        tracing::debug!(channel = %name, "inbound cancelled");
                    }
                }
            })
        };

        // Spawn outbound task.
        let outbound_handle = {
            let ch = channel.clone();
            let cancel = cancel.clone();
            let name = name.clone();
            tokio::spawn(async move {
                tokio::select! {
                    result = Channel::run_outbound(ch, out_rx) => {
                        if let Err(e) = result {
                            error!(channel = %name, "outbound error: {e}");
                        }
                    }
                    _ = cancel.cancelled() => {
                        tracing::debug!(channel = %name, "outbound cancelled");
                    }
                }
            })
        };

        // Spawn the dispatcher loop that routes events to per-conversation workers.
        let processing_handle = {
            let name = name.clone();
            let cancel = cancel.clone();
            let dispatcher =
                ConversationDispatcher::new(pipeline, out_tx, cancel.clone(), name.clone());
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Some(event) = in_rx.recv() => {
                            // Scheduler events bypass rate limiting.
                            if event.source != ChannelSource::Scheduler {
                                if let Some(ref sec) = security {
                                    let result = sec.rate_limiter.check(
                                        &event.message.author.id,
                                        None,
                                    );
                                    if result != crate::security::RateLimitResult::Allowed {
                                        tracing::warn!(
                                            channel = %name,
                                            user_id = %event.message.author.id,
                                            result = ?result,
                                            "message rate-limited, dropping"
                                        );
                                        continue;
                                    }
                                }
                            }
                            dispatcher.dispatch(event).await;
                        }
                        _ = cancel.cancelled() => {
                            tracing::debug!(channel = %name, "dispatcher loop cancelled");
                            break;
                        }
                    }
                }
            })
        };

        info!(channel = %name, "channel registered and tasks spawned");
        self.handles.push((
            name,
            vec![inbound_handle, outbound_handle, processing_handle],
        ));
    }

    /// Returns the number of registered channels.
    pub fn channel_count(&self) -> usize {
        self.handles.len()
    }

    /// Returns the names of all registered channels.
    pub fn channel_names(&self) -> Vec<&str> {
        self.handles.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// Wait for all channel tasks to complete (typically via cancellation).
    pub async fn await_shutdown(self) {
        for (name, handles) in self.handles {
            for handle in handles {
                if let Err(e) = handle.await {
                    error!(channel = %name, "task join error: {e}");
                }
            }
        }
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event::InEvent;

    /// A minimal test channel that sends one event then exits.
    struct MockChannel {
        name: String,
    }

    impl Channel for MockChannel {
        fn name(&self) -> &str {
            &self.name
        }

        async fn run_inbound(self: Arc<Self>, tx: Sender<InEvent>) -> Result<()> {
            let event = InEvent {
                source: crate::core::event::ChannelSource::Cli,
                message: crate::core::event::Message {
                    id: crate::core::event::MessageId("mock-1".into()),
                    author: crate::core::event::Author {
                        id: "mock-user".into(),
                        display_name: "Mock".into(),
                    },
                    text: "hello".into(),
                    timestamp: chrono::Utc::now(),
                    mentions_bot: false,
                },
                context: crate::core::event::MessageContext {
                    conversation_id: crate::core::event::ConversationId::Dm {
                        channel_type: crate::core::event::ChannelSource::Cli,
                        user_id: "mock-user".into(),
                    },
                    channel_id: "mock".into(),
                    reply_to: None,
                },
                tool_groups: None,
                completion_flag: None,
            };
            let _ = tx.send(event).await;
            Ok(())
        }

        async fn run_outbound(self: Arc<Self>, mut rx: Receiver<OutEvent>) -> Result<()> {
            while let Some(_event) = rx.recv().await {}
            Ok(())
        }
    }

    /// A mock pipeline that echoes back an OutEvent for each InEvent.
    struct EchoPipeline;

    impl PipelineRunner for EchoPipeline {
        fn process<'a>(
            &'a self,
            event: &'a InEvent,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<OutEvent>> + Send + 'a>>
        {
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

    /// Helper to create a Security instance with a tight rate limit for testing.
    fn make_test_security(capacity: u32) -> Arc<Security> {
        let config = crate::config::RateLimitConfig {
            per_user: crate::config::TokenBucketConfig {
                capacity,
                refill_secs: 60,
            },
            global: crate::config::GlobalTokenBucketConfig {
                capacity: 100,
                refill_secs: 60,
            },
            allowed_users: vec!["allowed-user".into()],
            allowed_guilds: vec![],
        };
        let tmp = std::env::temp_dir().join("borealis_test_ratelimit");
        let _ = std::fs::create_dir_all(&tmp);
        Arc::new(Security::new(&config, tmp, std::iter::empty::<String>()))
    }

    fn make_test_event(user_id: &str, source: crate::core::event::ChannelSource) -> InEvent {
        InEvent {
            source: source.clone(),
            message: crate::core::event::Message {
                id: crate::core::event::MessageId("test-msg".into()),
                author: crate::core::event::Author {
                    id: user_id.into(),
                    display_name: "Test".into(),
                },
                text: "hello".into(),
                timestamp: chrono::Utc::now(),
                mentions_bot: false,
            },
            context: crate::core::event::MessageContext {
                conversation_id: crate::core::event::ConversationId::Dm {
                    channel_type: source,
                    user_id: user_id.into(),
                },
                channel_id: "test".into(),
                reply_to: None,
            },
            tool_groups: None,
            completion_flag: None,
        }
    }

    #[tokio::test]
    async fn rate_limited_user_messages_dropped() {
        let cancel = CancellationToken::new();
        let pipeline: Arc<dyn PipelineRunner> = Arc::new(EchoPipeline);
        let security = make_test_security(2); // capacity=2

        let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<InEvent>(64);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<OutEvent>(64);

        let sec = Some(security);
        let dispatcher = ConversationDispatcher::new(
            pipeline,
            out_tx,
            cancel.clone(),
            "test".into(),
        );

        // Simulate the dispatcher loop inline.
        let handle = tokio::spawn(async move {
            while let Some(event) = in_rx.recv().await {
                if event.source != ChannelSource::Scheduler {
                    if let Some(ref s) = sec {
                        let result = s.rate_limiter.check(&event.message.author.id, None);
                        if result != crate::security::RateLimitResult::Allowed {
                            continue;
                        }
                    }
                }
                dispatcher.dispatch(event).await;
            }
        });

        // Send 4 messages — only first 2 should pass (capacity=2).
        for _ in 0..4 {
            in_tx
                .send(make_test_event("ratelimited-user", ChannelSource::Cli))
                .await
                .unwrap();
        }

        // Collect results with a timeout.
        let mut received = 0;
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                out_rx.recv(),
            )
            .await
            {
                Ok(Some(_)) => received += 1,
                _ => break,
            }
        }

        assert_eq!(received, 2, "only 2 of 4 messages should pass rate limit");

        drop(in_tx);
        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn scheduler_events_bypass_rate_limit() {
        let cancel = CancellationToken::new();
        let pipeline: Arc<dyn PipelineRunner> = Arc::new(EchoPipeline);
        let security = make_test_security(1); // capacity=1

        let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<InEvent>(64);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<OutEvent>(64);

        let sec = Some(security);
        let dispatcher = ConversationDispatcher::new(
            pipeline,
            out_tx,
            cancel.clone(),
            "test".into(),
        );

        let handle = tokio::spawn(async move {
            while let Some(event) = in_rx.recv().await {
                if event.source != ChannelSource::Scheduler {
                    if let Some(ref s) = sec {
                        let result = s.rate_limiter.check(&event.message.author.id, None);
                        if result != crate::security::RateLimitResult::Allowed {
                            continue;
                        }
                    }
                }
                dispatcher.dispatch(event).await;
            }
        });

        // Send 3 scheduler events — all should bypass rate limiting.
        for _ in 0..3 {
            in_tx
                .send(make_test_event("system", ChannelSource::Scheduler))
                .await
                .unwrap();
        }

        let mut received = 0;
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                out_rx.recv(),
            )
            .await
            {
                Ok(Some(_)) => received += 1,
                _ => break,
            }
        }

        assert_eq!(received, 3, "all scheduler events should bypass rate limit");

        drop(in_tx);
        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_register_and_channel_count() {
        let cancel = CancellationToken::new();
        let pipeline: Arc<dyn PipelineRunner> = Arc::new(EchoPipeline);

        let mut registry = ChannelRegistry::new();
        registry.register(
            Arc::new(MockChannel {
                name: "test-1".into(),
            }),
            pipeline.clone(),
            cancel.clone(),
            None,
        );
        registry.register(
            Arc::new(MockChannel {
                name: "test-2".into(),
            }),
            pipeline,
            cancel.clone(),
            None,
        );

        assert_eq!(registry.channel_count(), 2);
        assert_eq!(registry.channel_names(), vec!["test-1", "test-2"]);

        // Give a moment for mock inbound to send + processing to echo, then cancel.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        registry.await_shutdown().await;
    }
}
