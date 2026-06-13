pub mod events;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

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
            // Fail fast: an invalid event (bad cron, unknown type, missing or
            // sub-second interval, bad timezone) must surface at startup rather
            // than being silently dropped — a typo should never quietly disable
            // a scheduled event.
            let runner = ScheduledEventRunner::new(
                event_config,
                config.timezone.clone(),
                event_tx.clone(),
                cancel.clone(),
            )
            .with_context(|| format!("invalid scheduler event '{name}'"))?;
            runners.push(runner);
        }

        info!(count = runners.len(), "scheduler initialized");

        Ok(Self {
            runners,
            handles: Vec::new(),
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SchedulerEventConfig;

    fn event(name: &str, event_type: &str) -> SchedulerEventConfig {
        SchedulerEventConfig {
            name: name.into(),
            event_type: event_type.into(),
            interval: None,
            schedule: None,
            jitter: None,
            active_hours: None,
            prompt: "p".into(),
            tools: None,
        }
    }

    /// Build a scheduler and return the error string. `Scheduler` is not
    /// `Debug`, so `.unwrap_err()` won't compile — extract the error directly.
    fn scheduler_err(events: Vec<SchedulerEventConfig>, timezone: &str) -> String {
        match build_scheduler(events, timezone) {
            Ok(_) => panic!("expected Scheduler::new to fail"),
            Err(e) => e.to_string(),
        }
    }

    fn scheduler_ok(events: Vec<SchedulerEventConfig>, timezone: &str) -> bool {
        build_scheduler(events, timezone).is_ok()
    }

    fn build_scheduler(events: Vec<SchedulerEventConfig>, timezone: &str) -> Result<Scheduler> {
        let (tx, _rx) = mpsc::channel(1);
        Scheduler::new(
            SchedulerConfig {
                timezone: timezone.into(),
                events,
            },
            tx,
            CancellationToken::new(),
        )
    }

    #[test]
    fn new_propagates_invalid_cron_event() {
        // LIFE-10: a bad cron expression must fail fast, not be warn-skipped.
        let mut ev = event("daily", "cron");
        ev.schedule = Some("not a cron".into());
        let err = scheduler_err(vec![ev], "UTC");
        assert!(err.contains("daily"), "error should name the event: {err}");
    }

    #[test]
    fn new_propagates_unknown_event_type() {
        let ev = event("weird", "sometimes");
        let err = scheduler_err(vec![ev], "UTC");
        assert!(err.contains("weird"), "error should name the event: {err}");
    }

    #[test]
    fn new_propagates_invalid_timezone() {
        // LIFE-3 + LIFE-10: a bad scheduler timezone fails fast.
        let mut ev = event("daily", "cron");
        ev.schedule = Some("0 22 * * *".into());
        let err = scheduler_err(vec![ev], "Bad/Zone");
        assert!(err.contains("daily"), "error should name the event: {err}");
    }

    #[test]
    fn new_succeeds_with_valid_events() {
        let mut ev = event("daily", "cron");
        ev.schedule = Some("0 22 * * *".into());
        assert!(scheduler_ok(vec![ev], "Europe/London"));
    }
}
