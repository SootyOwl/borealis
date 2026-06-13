use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, NaiveTime, TimeDelta, Utc};
use chrono_tz::Tz;
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
    // Split on the last *char* boundary (not byte) so a multi-byte final
    // character (e.g. a pasted Cyrillic/Greek letter) does not panic.
    let last_char = s
        .chars()
        .last()
        .ok_or_else(|| anyhow!("empty duration string"))?;
    let num_str = &s[..s.len() - last_char.len_utf8()];
    let value: u64 = num_str
        .parse()
        .map_err(|_| anyhow!("invalid duration: {s}"))?;
    match last_char {
        's' => Ok(Duration::from_secs(value)),
        'm' => Ok(Duration::from_secs(value * 60)),
        'h' => Ok(Duration::from_secs(value * 3600)),
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
/// Handles overnight ranges (e.g., 22:00-06:00) via wrap-around.
pub fn is_within_active_hours(time: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start <= end {
        // Normal range: e.g. 06:00-23:00
        time >= start && time <= end
    } else {
        // Overnight range: e.g. 22:00-06:00
        time >= start || time <= end
    }
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

/// Compute the next cron occurrence strictly after `anchor`, evaluating the
/// schedule in timezone `tz`, and return it as a UTC instant.
///
/// `anchor` is a UTC instant; it is converted into `tz` so croner evaluates
/// the schedule in local wall-clock time (e.g. "0 22 * * *" -> 22:00 local).
/// The result is converted back to UTC for sleep-duration math.
///
/// Anchoring the search at the *scheduled* occurrence (rather than `now`) is
/// what prevents the symmetric-jitter double-fire: `find_next_occurrence` with
/// `inclusive = false` always returns a strictly-later occurrence than its
/// anchor, so advancing the anchor to the just-fired occurrence guarantees the
/// search moves past it regardless of jitter sign.
pub fn next_after(cron: &Cron, anchor: DateTime<Utc>, tz: Tz) -> Result<DateTime<Utc>> {
    let local_anchor = anchor.with_timezone(&tz);
    let next_local = cron
        .find_next_occurrence(&local_anchor, false)
        .map_err(|e| anyhow!("failed to compute next cron occurrence: {e}"))?;
    Ok(next_local.with_timezone(&Utc))
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
    /// The configured timezone string (used for `{timezone}` template rendering).
    timezone: String,
    /// The timezone parsed once at construction. All scheduling and time
    /// rendering uses this — no per-call re-parsing, no silent UTC fallback.
    tz: Tz,
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
        // Parse the timezone once, failing fast on an invalid string instead of
        // silently degrading to UTC at every call site.
        let tz = timezone.parse::<Tz>().map_err(|e| {
            anyhow!(
                "invalid scheduler timezone '{timezone}' for event '{}': {e}",
                config.name
            )
        })?;

        let schedule = match config.event_type.as_str() {
            "recurring" => {
                let interval_str = config.interval.as_deref().ok_or_else(|| {
                    anyhow!("recurring event '{}' missing 'interval'", config.name)
                })?;
                let interval = parse_duration(interval_str)?;
                // Reject a zero / sub-second interval: run_recurring would become
                // a CPU hot loop (sleep(ZERO) returns immediately and try_fire
                // spins).
                if interval < Duration::from_secs(1) {
                    return Err(anyhow!(
                        "recurring event '{}' interval must be >= 1s",
                        config.name
                    ));
                }
                ScheduleType::Recurring { interval }
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
            tz,
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

        // The anchor for the next-occurrence search. We start at "now" and then
        // always advance it to the *unjittered* scheduled occurrence we just
        // fired, so the following search advances strictly past it regardless of
        // jitter sign. This eliminates the symmetric-jitter double-fire and skip.
        let mut search_anchor = Utc::now();

        loop {
            let next = match next_after(cron, search_anchor, self.tz) {
                Ok(next) => next,
                Err(e) => {
                    warn!(event = %name, "{e}");
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                    }
                    continue;
                }
            };

            // Apply jitter for spread. Jitter may be +/-, but clamp the fire
            // time so we never sleep "into the past".
            let mut fire_at = next;
            if let Some(max_jitter) = self.jitter {
                fire_at += compute_jitter(max_jitter);
            }
            let now = Utc::now();
            if fire_at < now {
                fire_at = now;
            }

            let sleep_duration = (fire_at - now).to_std().unwrap_or(Duration::ZERO);

            debug!(event = %name, ?sleep_duration, fire_at = %fire_at, scheduled = %next, "sleeping until next cron fire");
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(sleep_duration) => {}
            }

            self.try_fire().await;

            // Advance the anchor PAST the just-fired scheduled occurrence so the
            // next search always returns a later occurrence (never re-fires the
            // same one, even when jitter fired us early).
            search_anchor = next;
        }
    }

    async fn try_fire(&self) {
        let name = &self.config.name;
        let now = Utc::now();

        // Active hours check — convert UTC to the configured timezone (parsed
        // once at construction, no per-call fallback).
        if let Some((start, end)) = self.active_hours {
            let local_time = now.with_timezone(&self.tz).time();
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
        // Render {time} in the configured timezone (with the zone abbreviation
        // for clarity), consistent with the {timezone} placeholder.
        let local_now = now.with_timezone(&self.tz);
        let prompt = substitute_template(
            &self.config.prompt,
            &local_now.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
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
                guild_id: None,
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_hours_normal_range() {
        let start = NaiveTime::from_hms_opt(6, 0, 0).unwrap();
        let end = NaiveTime::from_hms_opt(23, 0, 0).unwrap();

        // Inside
        assert!(is_within_active_hours(
            NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
            start,
            end,
        ));
        // At boundaries
        assert!(is_within_active_hours(start, start, end));
        assert!(is_within_active_hours(end, start, end));
        // Outside
        assert!(!is_within_active_hours(
            NaiveTime::from_hms_opt(5, 0, 0).unwrap(),
            start,
            end,
        ));
        assert!(!is_within_active_hours(
            NaiveTime::from_hms_opt(23, 30, 0).unwrap(),
            start,
            end,
        ));
    }

    #[test]
    fn active_hours_overnight_range() {
        let start = NaiveTime::from_hms_opt(22, 0, 0).unwrap();
        let end = NaiveTime::from_hms_opt(6, 0, 0).unwrap();

        // Inside — late evening
        assert!(is_within_active_hours(
            NaiveTime::from_hms_opt(23, 0, 0).unwrap(),
            start,
            end,
        ));
        // Inside — early morning
        assert!(is_within_active_hours(
            NaiveTime::from_hms_opt(3, 0, 0).unwrap(),
            start,
            end,
        ));
        // At boundaries
        assert!(is_within_active_hours(start, start, end));
        assert!(is_within_active_hours(end, start, end));
        // Outside — midday
        assert!(!is_within_active_hours(
            NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
            start,
            end,
        ));
        // Outside — just after end
        assert!(!is_within_active_hours(
            NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
            start,
            end,
        ));
    }

    // -- parse_duration -----------------------------------------------------

    #[test]
    fn parse_duration_valid_suffixes() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn parse_duration_multibyte_suffix_is_err_not_panic() {
        // LIFE-9: a multi-byte final char (Cyrillic "с", Greek "ς") must not
        // panic on a non-char-boundary split — it should return an Err.
        assert!(parse_duration("30с").is_err()); // Cyrillic es
        assert!(parse_duration("5ς").is_err()); // Greek final sigma
        assert!(parse_duration("ч").is_err()); // single multi-byte char
    }

    #[test]
    fn parse_duration_unknown_suffix_is_err() {
        let err = parse_duration("10x").unwrap_err().to_string();
        assert!(err.contains("unknown duration suffix"), "got: {err}");
    }

    // -- helpers for ScheduledEventRunner::new ------------------------------

    fn base_event(name: &str, event_type: &str) -> SchedulerEventConfig {
        SchedulerEventConfig {
            name: name.into(),
            event_type: event_type.into(),
            interval: None,
            schedule: None,
            jitter: None,
            active_hours: None,
            prompt: "test prompt".into(),
            tools: None,
        }
    }

    fn dummy_runner_args() -> (mpsc::Sender<InEvent>, CancellationToken) {
        let (tx, _rx) = mpsc::channel(1);
        (tx, CancellationToken::new())
    }

    /// Construct a runner and return the error string (Ok is discarded).
    /// `ScheduledEventRunner` is not `Debug`, so `.unwrap_err()` won't compile.
    fn new_err(cfg: SchedulerEventConfig, tz: &str) -> String {
        let (tx, cancel) = dummy_runner_args();
        match ScheduledEventRunner::new(cfg, tz.into(), tx, cancel) {
            Ok(_) => panic!("expected ScheduledEventRunner::new to fail"),
            Err(e) => e.to_string(),
        }
    }

    /// Construct a runner and return whether it succeeded (Ok discarded).
    fn new_ok(cfg: SchedulerEventConfig, tz: &str) -> bool {
        let (tx, cancel) = dummy_runner_args();
        ScheduledEventRunner::new(cfg, tz.into(), tx, cancel).is_ok()
    }

    // -- LIFE-8: zero / sub-second recurring interval rejected --------------

    #[test]
    fn new_rejects_zero_recurring_interval() {
        let mut cfg = base_event("hot", "recurring");
        cfg.interval = Some("0s".into());
        let err = new_err(cfg, "UTC");
        assert!(err.contains("must be >= 1s"), "got: {err}");
    }

    #[test]
    fn new_rejects_zero_minute_recurring_interval() {
        let mut cfg = base_event("hot", "recurring");
        cfg.interval = Some("0m".into());
        let err = new_err(cfg, "UTC");
        assert!(err.contains("must be >= 1s"), "got: {err}");
    }

    #[test]
    fn new_accepts_one_second_recurring_interval() {
        let mut cfg = base_event("ok", "recurring");
        cfg.interval = Some("1s".into());
        assert!(new_ok(cfg, "UTC"));
    }

    // -- LIFE-3: invalid timezone rejected at construction ------------------

    #[test]
    fn new_rejects_invalid_timezone() {
        let mut cfg = base_event("daily", "cron");
        cfg.schedule = Some("0 22 * * *".into());
        let err = new_err(cfg, "Not/AZone");
        assert!(err.contains("invalid scheduler timezone"), "got: {err}");
        assert!(err.contains("Not/AZone"), "got: {err}");
    }

    #[test]
    fn new_accepts_valid_named_timezone() {
        let mut cfg = base_event("daily", "cron");
        cfg.schedule = Some("0 22 * * *".into());
        assert!(new_ok(cfg, "Europe/London"));
    }

    // -- LIFE-2: cron evaluated in the configured timezone ------------------

    #[test]
    fn next_after_resolves_local_hour_in_tz() {
        // "0 22 * * *" should resolve to 22:00 *local* time in a non-UTC zone,
        // not 22:00 UTC.
        let cron = Cron::from_str("0 22 * * *").unwrap();
        let tz: Tz = "America/New_York".parse().unwrap();
        // Anchor at a fixed UTC instant; the result, viewed in tz, must be 22:00.
        let anchor = Utc::now();
        let next_utc = next_after(&cron, anchor, tz).unwrap();
        let local = next_utc.with_timezone(&tz);
        use chrono::Timelike;
        assert_eq!(local.hour(), 22, "next occurrence local hour");
        assert_eq!(local.minute(), 0);
    }

    // -- LIFE-1: anti-double-fire — anchoring at `next` skips it -------------

    #[test]
    fn next_after_anchored_at_prior_occurrence_strictly_advances() {
        // Anchoring the search at the previously-fired occurrence must yield a
        // strictly-later occurrence — never the same one again (the bug was
        // that anchoring at `now < next` returned `next` repeatedly when
        // negative jitter fired early).
        let cron = Cron::from_str("0 22 * * *").unwrap();
        let tz: Tz = "UTC".parse().unwrap();
        let anchor = Utc::now();
        let first = next_after(&cron, anchor, tz).unwrap();
        // Advance the anchor to the occurrence we just "fired".
        let second = next_after(&cron, first, tz).unwrap();
        assert!(
            second > first,
            "second occurrence ({second}) must be strictly after first ({first})"
        );
        // For a daily schedule the gap is exactly 24h.
        assert_eq!(second - first, TimeDelta::hours(24));
    }
}
