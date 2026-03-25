pub mod cli;
pub mod discord;
pub mod modes;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::core::event::{DirectiveKind, InEvent, OutEvent};
use crate::core::pipeline::PipelineRunner;

/// A channel adapter that bridges a platform (Discord, CLI, etc.) with the core event bus.
///
/// Split into two async methods so inbound and outbound run as separate tasks.
/// If `run_outbound` panics, the supervisor can restart it without affecting inbound.
#[allow(dead_code)] // Methods used by task supervisor (REQ-9)
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

    /// Which directive kinds this channel supports.
    fn supported_directives(&self) -> Vec<DirectiveKind>;
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

        // Spawn the message processing loop.
        let processing_handle = {
            let name = name.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Some(event) = in_rx.recv() => {
                            match pipeline.process(&event).await {
                                Ok(out_event) => {
                                    if out_tx.send(out_event).await.is_err() {
                                        tracing::debug!(channel = %name, "outbound channel closed");
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!(channel = %name, "pipeline error: {e}");
                                    let err_event = crate::core::event::OutEvent {
                                        target: event.source.clone(),
                                        channel_id: event.context.channel_id.clone(),
                                        text: Some("I'm having trouble thinking right now, try again in a moment.".into()),
                                        directives: vec![],
                                        reply_to: Some(event.message.id.clone()),
                                    };
                                    let _ = out_tx.send(err_event).await;
                                }
                            }
                        }
                        _ = cancel.cancelled() => {
                            tracing::debug!(channel = %name, "processing loop cancelled");
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
            };
            let _ = tx.send(event).await;
            Ok(())
        }

        async fn run_outbound(self: Arc<Self>, mut rx: Receiver<OutEvent>) -> Result<()> {
            while let Some(_event) = rx.recv().await {}
            Ok(())
        }

        fn supported_directives(&self) -> Vec<DirectiveKind> {
            vec![]
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
                    directives: vec![],
                    reply_to: Some(event.message.id.clone()),
                })
            })
        }
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
        );
        registry.register(
            Arc::new(MockChannel {
                name: "test-2".into(),
            }),
            pipeline,
            cancel.clone(),
        );

        assert_eq!(registry.channel_count(), 2);
        assert_eq!(registry.channel_names(), vec!["test-1", "test-2"]);

        // Give a moment for mock inbound to send + processing to echo, then cancel.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        registry.await_shutdown().await;
    }
}
