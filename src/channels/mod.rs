pub mod cli;
pub mod discord;
pub mod modes;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::core::event::{DirectiveKind, InEvent, OutEvent};

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
