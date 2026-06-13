pub mod events;

use anyhow::{Context, Result};
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

    /// Wait for all event-runner tasks to finish, then return.
    ///
    /// Each `ScheduledEventRunner::run` loop exits promptly once the shared
    /// `CancellationToken` is cancelled (its `tokio::select!`s are `biased` with
    /// the cancel branch first), so the caller is expected to have cancelled the
    /// token already — otherwise this awaits until the runners' next sleep wakes.
    /// Mirrors `ChannelRegistry::await_shutdown`: join each handle, warn on a
    /// join error (a panicked runner) rather than propagating.
    ///
    /// Note: this only joins the runners. The event-feed loop that consumes the
    /// runners' events and drives the pipeline is owned and drained by the caller
    /// (see `main.rs`), because it depends on the pipeline which the scheduler
    /// does not hold.
    pub async fn shutdown(self) {
        for handle in self.handles {
            if let Err(e) = handle.await {
                warn!("scheduler runner join error: {e}");
            }
        }
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

    /// Build a scheduler whose runners share `cancel`, so a test can cancel them
    /// and observe `shutdown()` returning promptly. The mpsc receiver is dropped
    /// — runners in these tests use a long interval and never fire within the
    /// test window, so they never `send`.
    fn build_scheduler_with_cancel(
        events: Vec<SchedulerEventConfig>,
        timezone: &str,
        cancel: CancellationToken,
    ) -> Result<Scheduler> {
        let (tx, _rx) = mpsc::channel(8);
        Scheduler::new(
            SchedulerConfig {
                timezone: timezone.into(),
                events,
            },
            tx,
            cancel,
        )
    }

    /// LIFE: the shutdown gap fix — a started scheduler whose token is cancelled
    /// must join its runner tasks and return promptly (not hang). This guards the
    /// drain that lets an in-flight scheduled event finish before process exit.
    #[tokio::test]
    async fn shutdown_returns_after_cancel() {
        let cancel = CancellationToken::new();
        let mut ev = event("tick", "recurring");
        ev.interval = Some("60s".into()); // long interval: runner is asleep
        let mut scheduler =
            build_scheduler_with_cancel(vec![ev], "UTC", cancel.clone()).expect("valid scheduler");
        scheduler.start();

        // Cancel first so the runners' `biased` selects take the cancel branch.
        cancel.cancel();

        // shutdown() must return well within the 5s force-exit backstop.
        tokio::time::timeout(std::time::Duration::from_secs(2), scheduler.shutdown())
            .await
            .expect("scheduler.shutdown() should return promptly after cancel");
    }

    /// A scheduler with zero events has no runners; shutdown() is an immediate
    /// no-op and must still return.
    #[tokio::test]
    async fn shutdown_with_no_events_is_noop() {
        let cancel = CancellationToken::new();
        let mut scheduler =
            build_scheduler_with_cancel(vec![], "UTC", cancel.clone()).expect("valid scheduler");
        scheduler.start();
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), scheduler.shutdown())
            .await
            .expect("empty scheduler.shutdown() should return immediately");
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
