pub mod events;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::SchedulerConfig;
use crate::core::event::InEvent;
use events::ScheduledEventRunner;

/// Config-driven scheduler that manages scheduled event runners.
pub struct Scheduler {
    runners: Vec<ScheduledEventRunner>,
    handles: Vec<JoinHandle<()>>,
}

impl Scheduler {
    /// Create a new scheduler from config. Validates all events eagerly.
    pub fn new(
        config: SchedulerConfig,
        event_tx: mpsc::Sender<InEvent>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let mut runners = Vec::with_capacity(config.events.len());
        for event_config in config.events {
            let name = event_config.name.clone();
            match ScheduledEventRunner::new(
                event_config,
                config.timezone.clone(),
                event_tx.clone(),
                cancel.clone(),
            ) {
                Ok(runner) => runners.push(runner),
                Err(e) => {
                    warn!(event = %name, "skipping invalid scheduler event: {e}");
                }
            }
        }

        info!(count = runners.len(), "scheduler initialized");

        Ok(Self {
            runners,
            handles: Vec::new(),
        })
    }

    /// Returns the number of configured events.
    pub fn event_count(&self) -> usize {
        self.runners.len() + self.handles.len()
    }

    /// Start all event runners as background tasks.
    pub fn start(&mut self) {
        let runners: Vec<_> = self.runners.drain(..).collect();
        for runner in runners {
            let handle = tokio::spawn(runner.run());
            self.handles.push(handle);
        }
        info!(tasks = self.handles.len(), "scheduler started");
    }
}
