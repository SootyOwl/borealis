use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{NaiveTime, TimeDelta, Utc};
use croner::Cron;
use rand::Rng;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::SchedulerEventConfig;
use crate::core::event::{
    Author, ChannelSource, ConversationId, InEvent, Message, MessageContext, MessageId,
};

/// Parse a human-friendly duration string like "30m", "2h", "90s".
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration string"));
    }
    let (num_str, suffix) = s.split_at(s.len() - 1);
    let value: u64 = num_str
        .parse()
        .map_err(|_| anyhow!("invalid duration: {s}"))?;
    match suffix {
        "s" => Ok(Duration::from_secs(value)),
        "m" => Ok(Duration::from_secs(value * 60)),
        "h" => Ok(Duration::from_secs(value * 3600)),
        _ => Err(anyhow!("unknown duration suffix in '{s}', expected s/m/h")),
    }
}

/// Parse "HH:MM-HH:MM" into (start, end) NaiveTime pair.
pub fn parse_active_hours(s: &str) -> Result<(NaiveTime, NaiveTime)> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 2 {
        return Err(anyhow!(
            "invalid active_hours format: '{s}', expected 'HH:MM-HH:MM'"
        ));
    }
    let start = NaiveTime::parse_from_str(parts[0], "%H:%M")
        .map_err(|e| anyhow!("invalid start time '{}': {e}", parts[0]))?;
    let end = NaiveTime::parse_from_str(parts[1], "%H:%M")
        .map_err(|e| anyhow!("invalid end time '{}': {e}", parts[1]))?;
    Ok((start, end))
}

/// Check if a time falls within [start, end] (inclusive).
/// Does NOT handle overnight ranges (e.g., 23:00-06:00).
pub fn is_within_active_hours(time: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    time >= start && time <= end
}

/// Compute a random jitter in the range [-max_jitter, +max_jitter].
/// Returns a chrono::TimeDelta for use with DateTime arithmetic.
pub fn compute_jitter(max_jitter: Duration) -> TimeDelta {
    if max_jitter.is_zero() {
        return TimeDelta::zero();
    }
    let max_secs = max_jitter.as_secs() as i64;
    let jitter_secs = rand::thread_rng().gen_range(-max_secs..=max_secs);
    TimeDelta::seconds(jitter_secs)
}

/// Replace template variables in a prompt string.
pub fn substitute_template(
    template: &str,
    time: &str,
    timezone: &str,
    interval: Option<&str>,
) -> String {
    let mut result = template
        .replace("{time}", time)
        .replace("{timezone}", timezone);
    if let Some(interval) = interval {
        result = result.replace("{interval}", interval);
    }
    result
}

// ---------------------------------------------------------------------------
// ScheduledEventRunner
// ---------------------------------------------------------------------------

/// The type of scheduling for an event.
enum ScheduleType {
    Recurring { interval: Duration },
    Cron { cron: Box<Cron> },
}

/// Runs a single scheduled event in an async loop.
pub struct ScheduledEventRunner {
    config: SchedulerEventConfig,
    timezone: String,
    schedule: ScheduleType,
    jitter: Option<Duration>,
    active_hours: Option<(NaiveTime, NaiveTime)>,
    processing: Arc<AtomicBool>,
    event_tx: mpsc::Sender<InEvent>,
    cancel: CancellationToken,
}

impl ScheduledEventRunner {
    /// Create a new runner from config. Validates the event config eagerly.
    pub fn new(
        config: SchedulerEventConfig,
        timezone: String,
        event_tx: mpsc::Sender<InEvent>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let schedule = match config.event_type.as_str() {
            "recurring" => {
                let interval_str = config.interval.as_deref().ok_or_else(|| {
                    anyhow!("recurring event '{}' missing 'interval'", config.name)
                })?;
                ScheduleType::Recurring {
                    interval: parse_duration(interval_str)?,
                }
            }
            "cron" => {
                let schedule_str = config
                    .schedule
                    .as_deref()
                    .ok_or_else(|| anyhow!("cron event '{}' missing 'schedule'", config.name))?;
                let cron = Cron::from_str(schedule_str)
                    .map_err(|e| anyhow!("invalid cron expression for '{}': {e}", config.name))?;
                ScheduleType::Cron {
                    cron: Box::new(cron),
                }
            }
            other => {
                return Err(anyhow!(
                    "unknown event type '{other}' for '{}'",
                    config.name
                ));
            }
        };

        let jitter = config.jitter.as_deref().map(parse_duration).transpose()?;

        let active_hours = config
            .active_hours
            .as_deref()
            .map(parse_active_hours)
            .transpose()?;

        Ok(Self {
            config,
            timezone,
            schedule,
            jitter,
            active_hours,
            processing: Arc::new(AtomicBool::new(false)),
            event_tx,
            cancel,
        })
    }

    /// Run the event loop. This is spawned as a tokio task.
    pub async fn run(self) {
        let name = &self.config.name;
        info!(event = %name, "scheduler event started");

        match &self.schedule {
            ScheduleType::Recurring { interval } => self.run_recurring(*interval).await,
            ScheduleType::Cron { .. } => self.run_cron().await,
        }

        info!(event = %name, "scheduler event stopped");
    }

    async fn run_recurring(&self, interval: Duration) {
        let name = &self.config.name;

        // First fire: wait one full interval from startup
        debug!(event = %name, ?interval, "waiting initial interval before first fire");
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }

        loop {
            self.try_fire().await;

            // Compute next sleep = interval + jitter
            let jitter_delta = self.jitter.map(compute_jitter).unwrap_or(TimeDelta::zero());
            let base = TimeDelta::from_std(interval).unwrap_or(TimeDelta::zero());
            let next_wait = base + jitter_delta;
            let next_wait = next_wait.to_std().unwrap_or(interval);

            debug!(event = %name, ?next_wait, "sleeping until next fire");
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(next_wait) => {}
            }
        }
    }

    async fn run_cron(&self) {
        let name = &self.config.name;
        let cron = match &self.schedule {
            ScheduleType::Cron { cron } => cron,
            _ => unreachable!(),
        };

        loop {
            let now = Utc::now();
            let next = match cron.find_next_occurrence(&now, false) {
                Ok(next) => next,
                Err(e) => {
                    warn!(event = %name, "failed to compute next cron occurrence: {e}");
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                    }
                    continue;
                }
            };

            let mut wait_until = next;

            // Apply jitter
            if let Some(max_jitter) = self.jitter {
                let jitter_delta = compute_jitter(max_jitter);
                wait_until += jitter_delta;
                // If jitter pushed us into the past, fire now
                if wait_until < Utc::now() {
                    wait_until = Utc::now();
                }
            }

            let sleep_duration = (wait_until - Utc::now()).to_std().unwrap_or(Duration::ZERO);

            debug!(event = %name, ?sleep_duration, next = %wait_until, "sleeping until next cron fire");
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(sleep_duration) => {}
            }

            self.try_fire().await;
        }
    }

    async fn try_fire(&self) {
        let name = &self.config.name;
        let now = Utc::now();

        // Active hours check — convert UTC to configured timezone
        if let Some((start, end)) = self.active_hours {
            let local_time = if let Ok(tz) = self.timezone.parse::<chrono_tz::Tz>() {
                now.with_timezone(&tz).time()
            } else {
                now.time()
            };
            if !is_within_active_hours(local_time, start, end) {
                info!(event = %name, time = %local_time, "skipping — outside active hours");
                return;
            }
        }

        // Overlap prevention
        if self.processing.swap(true, Ordering::SeqCst) {
            warn!(event = %name, "skipping — previous event still processing");
            return;
        }

        let processing = Arc::clone(&self.processing);
        let interval_str = self.config.interval.as_deref();
        let prompt = substitute_template(
            &self.config.prompt,
            &now.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            &self.timezone,
            interval_str,
        );

        let event = InEvent {
            source: ChannelSource::Scheduler,
            message: Message {
                id: MessageId(format!("sched-{}-{}", name, uuid::Uuid::new_v4())),
                author: Author {
                    id: "scheduler".into(),
                    display_name: "Scheduler".into(),
                },
                text: prompt,
                timestamp: now,
                mentions_bot: true,
            },
            context: MessageContext {
                conversation_id: ConversationId::System {
                    event_name: name.clone(),
                },
                channel_id: format!("scheduler:{name}"),
                reply_to: None,
            },
            tool_groups: self.config.tools.clone(),
            // Pass the processing flag so the consumer can clear it when done.
            completion_flag: Some(Arc::clone(&processing)),
        };

        if let Err(e) = self.event_tx.send(event).await {
            warn!(event = %name, "failed to send scheduler event: {e}");
            processing.store(false, Ordering::SeqCst);
            return;
        }

        debug!(event = %name, "scheduler event fired — flag held until processing completes");
    }
}
