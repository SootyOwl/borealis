use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Unique identifier for a supervised task.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Configuration for the circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Maximum number of restarts allowed within the window before tripping.
    pub max_restarts: usize,
    /// Sliding time window for counting restarts.
    pub window: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            window: Duration::from_secs(60),
        }
    }
}

/// Tracks restart frequency to prevent crash loops.
///
/// Records restart timestamps in a sliding window. When the number of restarts
/// within the window reaches `max_restarts`, the breaker trips and no further
/// restarts are allowed.
struct CircuitBreaker {
    config: CircuitBreakerConfig,
    restart_times: VecDeque<Instant>,
    tripped: bool,
}

impl CircuitBreaker {
    fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            restart_times: VecDeque::new(),
            tripped: false,
        }
    }

    /// Check whether a restart is allowed. If allowed, records the restart
    /// timestamp. Returns `true` if the restart should proceed.
    fn allow_restart(&mut self) -> bool {
        if self.tripped {
            return false;
        }

        let now = Instant::now();

        // Prune entries outside the sliding window
        while self
            .restart_times
            .front()
            .is_some_and(|&t| now.duration_since(t) > self.config.window)
        {
            self.restart_times.pop_front();
        }

        if self.restart_times.len() >= self.config.max_restarts {
            self.tripped = true;
            return false;
        }

        self.restart_times.push_back(now);
        true
    }

    /// Number of restarts recorded within the current window.
    fn restart_count(&self) -> usize {
        self.restart_times.len()
    }

    /// Whether the circuit breaker has tripped.
    fn is_tripped(&self) -> bool {
        self.tripped
    }
}

/// A factory function that spawns a task into a `JoinSet`.
///
/// Called once on initial registration and again on each restart. The closure
/// should create any fresh resources (e.g., channel pairs) and spawn the task
/// future. Returns the `tokio::task::Id` for tracking.
pub type SpawnFn = Box<dyn Fn(&mut JoinSet<Result<()>>) -> tokio::task::Id + Send>;

/// Internal entry for a supervised task.
struct TaskEntry {
    label: String,
    spawn_fn: SpawnFn,
    circuit_breaker: CircuitBreaker,
}

/// Supervises a set of tasks with automatic restart and circuit breaking.
///
/// When a supervised task exits with an error or panics, the supervisor
/// restarts it by calling the registered `SpawnFn` with a fresh `JoinSet`
/// slot. A per-task circuit breaker stops restarts if failures exceed the
/// configured threshold (default: 5 restarts in 60 seconds).
///
/// Clean exits (`Ok(())`) do not trigger restarts — they indicate the task
/// completed its work normally.
pub struct TaskSupervisor {
    join_set: JoinSet<Result<()>>,
    tasks: HashMap<TaskId, TaskEntry>,
    /// Maps tokio runtime task IDs back to our TaskId for identifying tasks
    /// that exit (including panics, where the return value is lost).
    tokio_to_task: HashMap<tokio::task::Id, TaskId>,
    cancel: CancellationToken,
}

impl TaskSupervisor {
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            join_set: JoinSet::new(),
            tasks: HashMap::new(),
            tokio_to_task: HashMap::new(),
            cancel,
        }
    }

    /// Register and spawn a supervised task.
    ///
    /// The `spawn_fn` is called immediately to start the task, and stored
    /// for future restarts. Each invocation of `spawn_fn` should create
    /// fresh resources (channels, connections) as needed.
    pub fn supervise(
        &mut self,
        id: TaskId,
        label: impl Into<String>,
        config: CircuitBreakerConfig,
        spawn_fn: SpawnFn,
    ) {
        let label = label.into();
        let tokio_id = spawn_fn(&mut self.join_set);
        info!(task = %id, label = %label, "supervised task spawned");
        self.tokio_to_task.insert(tokio_id, id.clone());
        self.tasks.insert(
            id,
            TaskEntry {
                label,
                spawn_fn,
                circuit_breaker: CircuitBreaker::new(config),
            },
        );
    }

    /// Run the supervisor loop until cancellation or all tasks exit.
    ///
    /// Monitors the `JoinSet` for task completions. On error or panic,
    /// attempts to restart the task (subject to circuit breaker limits).
    /// On cancellation, shuts down all tasks gracefully.
    pub async fn run(&mut self) {
        loop {
            tokio::select! {
                biased;

                _ = self.cancel.cancelled() => {
                    info!("supervisor: cancellation received, shutting down all tasks");
                    self.join_set.shutdown().await;
                    break;
                }

                result = self.join_set.join_next_with_id() => {
                    match result {
                        Some(Ok((tokio_id, task_result))) => {
                            if let Some(task_id) = self.tokio_to_task.remove(&tokio_id) {
                                self.handle_exit(task_id, task_result);
                            } else {
                                warn!(tokio_task_id = ?tokio_id, "unknown task exited");
                            }
                        }
                        Some(Err(join_error)) => {
                            let tokio_id = join_error.id();
                            if let Some(task_id) = self.tokio_to_task.remove(&tokio_id) {
                                if join_error.is_panic() {
                                    error!(task = %task_id, "supervised task panicked");
                                    self.handle_exit(
                                        task_id,
                                        Err(anyhow::anyhow!("task panicked")),
                                    );
                                } else {
                                    info!(task = %task_id, "supervised task was cancelled");
                                }
                            } else {
                                warn!("unknown task exited with JoinError: {join_error}");
                            }
                        }
                        None => {
                            info!("supervisor: all supervised tasks have exited");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Handle a task exit, restarting if appropriate.
    fn handle_exit(&mut self, task_id: TaskId, result: Result<()>) {
        // First pass: inspect result and update circuit breaker.
        // This block borrows self.tasks mutably then releases it.
        let should_restart = {
            let Some(entry) = self.tasks.get_mut(&task_id) else {
                warn!(task = %task_id, "no entry found for exited task");
                return;
            };

            match &result {
                Ok(()) => {
                    info!(
                        task = %task_id,
                        label = %entry.label,
                        "supervised task exited cleanly"
                    );
                    false
                }
                Err(e) => {
                    warn!(
                        task = %task_id,
                        label = %entry.label,
                        error = %e,
                        "supervised task failed"
                    );

                    if entry.circuit_breaker.allow_restart() {
                        true
                    } else {
                        error!(
                            task = %task_id,
                            label = %entry.label,
                            max_restarts = entry.circuit_breaker.config.max_restarts,
                            window_secs = entry.circuit_breaker.config.window.as_secs(),
                            "circuit breaker tripped — stopping retries"
                        );
                        false
                    }
                }
            }
        };

        if !should_restart {
            return;
        }

        // Second pass: respawn. Split borrows on struct fields so we can
        // read tasks while mutating join_set and tokio_to_task.
        let tasks = &self.tasks;
        let join_set = &mut self.join_set;
        let tokio_to_task = &mut self.tokio_to_task;

        if let Some(entry) = tasks.get(&task_id) {
            info!(
                task = %task_id,
                label = %entry.label,
                restart = entry.circuit_breaker.restart_count(),
                "restarting supervised task"
            );
            let tokio_id = (entry.spawn_fn)(join_set);
            tokio_to_task.insert(tokio_id, task_id);
        }
    }

    /// Number of registered supervised tasks (including stopped ones).
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    /// Number of currently running tasks in the `JoinSet`.
    pub fn active_count(&self) -> usize {
        self.join_set.len()
    }

    /// Whether a specific task's circuit breaker has tripped.
    pub fn is_tripped(&self, id: &TaskId) -> bool {
        self.tasks
            .get(id)
            .is_some_and(|e| e.circuit_breaker.is_tripped())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Helper: creates a SpawnFn that spawns a task returning Ok(()) after a brief delay.
    fn clean_exit_spawn(delay_ms: u64) -> SpawnFn {
        Box::new(move |join_set: &mut JoinSet<Result<()>>| {
            let handle = join_set.spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                Ok(())
            });
            handle.id()
        })
    }

    /// Helper: creates a SpawnFn that fails N times then succeeds.
    /// Uses an atomic counter shared across invocations of the closure.
    fn fail_then_succeed_spawn(fail_count: u32, delay_ms: u64) -> SpawnFn {
        let counter = Arc::new(AtomicU32::new(0));
        Box::new(move |join_set: &mut JoinSet<Result<()>>| {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            let should_fail = call < fail_count;
            let handle = join_set.spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                if should_fail {
                    Err(anyhow::anyhow!("failure #{}", call + 1))
                } else {
                    Ok(())
                }
            });
            handle.id()
        })
    }

    /// Helper: creates a SpawnFn that increments a counter each time it's called,
    /// always returning an error.
    fn counting_error_spawn(counter: Arc<AtomicU32>, delay_ms: u64) -> SpawnFn {
        Box::new(move |join_set: &mut JoinSet<Result<()>>| {
            let count = counter.fetch_add(1, Ordering::SeqCst);
            let handle = join_set.spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                Err(anyhow::anyhow!("failure #{}", count + 1))
            });
            handle.id()
        })
    }

    #[tokio::test]
    async fn clean_exit_no_restart() {
        let cancel = CancellationToken::new();
        let mut supervisor = TaskSupervisor::new(cancel.clone());

        supervisor.supervise(
            TaskId("test-clean".into()),
            "clean task",
            CircuitBreakerConfig::default(),
            clean_exit_spawn(10),
        );

        assert_eq!(supervisor.task_count(), 1);
        assert_eq!(supervisor.active_count(), 1);

        // Supervisor runs until all tasks exit
        tokio::time::timeout(Duration::from_secs(2), supervisor.run())
            .await
            .expect("supervisor should exit when all tasks complete");

        // Task exited cleanly — no restart, JoinSet is empty
        assert_eq!(supervisor.active_count(), 0);
        assert!(!supervisor.is_tripped(&TaskId("test-clean".into())));
    }

    #[tokio::test]
    async fn error_triggers_restart_then_succeeds() {
        let cancel = CancellationToken::new();
        let mut supervisor = TaskSupervisor::new(cancel.clone());

        // Fail twice, then succeed
        supervisor.supervise(
            TaskId("test-error".into()),
            "error-then-ok task",
            CircuitBreakerConfig::default(),
            fail_then_succeed_spawn(2, 10),
        );

        tokio::time::timeout(Duration::from_secs(2), supervisor.run())
            .await
            .expect("supervisor should exit after task succeeds");

        assert_eq!(supervisor.active_count(), 0);
        assert!(!supervisor.is_tripped(&TaskId("test-error".into())));
    }

    #[tokio::test]
    async fn panic_triggers_restart_then_succeeds() {
        let cancel = CancellationToken::new();
        let mut supervisor = TaskSupervisor::new(cancel.clone());

        // Use a counter: first call panics, second call succeeds
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = Arc::clone(&counter);
        let spawn_fn: SpawnFn = Box::new(move |join_set: &mut JoinSet<Result<()>>| {
            let call = counter_clone.fetch_add(1, Ordering::SeqCst);
            let handle = join_set.spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if call == 0 {
                    panic!("simulated panic on first call");
                }
                Ok(())
            });
            handle.id()
        });

        supervisor.supervise(
            TaskId("test-panic".into()),
            "panic-then-ok task",
            CircuitBreakerConfig::default(),
            spawn_fn,
        );

        tokio::time::timeout(Duration::from_secs(2), supervisor.run())
            .await
            .expect("supervisor should exit after task succeeds");

        assert_eq!(supervisor.active_count(), 0);
        // Counter should be 2: initial spawn + 1 restart
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn circuit_breaker_trips_after_max_restarts() {
        let cancel = CancellationToken::new();
        let mut supervisor = TaskSupervisor::new(cancel.clone());

        let spawn_counter = Arc::new(AtomicU32::new(0));
        let task_id = TaskId("test-breaker".into());

        supervisor.supervise(
            task_id.clone(),
            "always-failing task",
            CircuitBreakerConfig {
                max_restarts: 5,
                window: Duration::from_secs(60),
            },
            counting_error_spawn(Arc::clone(&spawn_counter), 10),
        );

        tokio::time::timeout(Duration::from_secs(5), supervisor.run())
            .await
            .expect("supervisor should exit after circuit breaker trips");

        // 1 initial spawn + 5 restarts = 6 total spawns
        assert_eq!(spawn_counter.load(Ordering::SeqCst), 6);
        assert!(supervisor.is_tripped(&task_id));
        assert_eq!(supervisor.active_count(), 0);
    }

    #[tokio::test]
    async fn cancellation_shuts_down_all_tasks() {
        let cancel = CancellationToken::new();
        let mut supervisor = TaskSupervisor::new(cancel.clone());

        // Spawn tasks that run indefinitely
        for i in 0..3 {
            let spawn_fn: SpawnFn = Box::new(move |join_set: &mut JoinSet<Result<()>>| {
                let handle = join_set.spawn(async move {
                    // Run forever until cancelled
                    loop {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                });
                handle.id()
            });

            supervisor.supervise(
                TaskId(format!("long-{i}")),
                format!("long-running task {i}"),
                CircuitBreakerConfig::default(),
                spawn_fn,
            );
        }

        assert_eq!(supervisor.active_count(), 3);

        // Cancel after a brief delay
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        tokio::time::timeout(Duration::from_secs(2), supervisor.run())
            .await
            .expect("supervisor should exit after cancellation");

        assert_eq!(supervisor.active_count(), 0);
    }

    #[tokio::test]
    async fn multiple_tasks_independent_circuit_breakers() {
        let cancel = CancellationToken::new();
        let mut supervisor = TaskSupervisor::new(cancel.clone());

        let failing_counter = Arc::new(AtomicU32::new(0));
        let failing_id = TaskId("always-failing".into());
        let clean_id = TaskId("clean-exit".into());

        // Task that always fails
        supervisor.supervise(
            failing_id.clone(),
            "always-failing",
            CircuitBreakerConfig {
                max_restarts: 3,
                window: Duration::from_secs(60),
            },
            counting_error_spawn(Arc::clone(&failing_counter), 10),
        );

        // Task that exits cleanly
        supervisor.supervise(
            clean_id.clone(),
            "clean-exit",
            CircuitBreakerConfig::default(),
            clean_exit_spawn(10),
        );

        tokio::time::timeout(Duration::from_secs(3), supervisor.run())
            .await
            .expect("supervisor should exit");

        // Failing task: 1 initial + 3 restarts = 4 spawns
        assert_eq!(failing_counter.load(Ordering::SeqCst), 4);
        assert!(supervisor.is_tripped(&failing_id));
        assert!(!supervisor.is_tripped(&clean_id));
    }

    #[tokio::test]
    async fn circuit_breaker_window_expiry() {
        // Test that restarts outside the window don't count toward the limit.
        // Use a very short window so we can test expiry quickly.
        let cancel = CancellationToken::new();
        let mut supervisor = TaskSupervisor::new(cancel.clone());

        let spawn_counter = Arc::new(AtomicU32::new(0));
        let counter_clone = Arc::clone(&spawn_counter);

        // Fail 3 times quickly, then wait for window to expire, then fail again.
        // With max_restarts=3 and a 100ms window, the first 3 restarts fill the
        // window. If the 4th failure happens after the window expires, it should
        // be allowed (window has reset).
        //
        // We simulate this by having the task sleep longer after the 3rd restart,
        // so the old entries expire.
        let spawn_fn: SpawnFn = Box::new(move |join_set: &mut JoinSet<Result<()>>| {
            let call = counter_clone.fetch_add(1, Ordering::SeqCst);
            let handle = join_set.spawn(async move {
                if call < 3 {
                    // First 3 calls: fail quickly
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    Err(anyhow::anyhow!("quick failure #{}", call + 1))
                } else if call == 3 {
                    // 4th call: wait for window to expire, then fail
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Err(anyhow::anyhow!("delayed failure"))
                } else {
                    // 5th call: succeed
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    Ok(())
                }
            });
            handle.id()
        });

        supervisor.supervise(
            TaskId("window-test".into()),
            "window-expiry task",
            CircuitBreakerConfig {
                max_restarts: 3,
                window: Duration::from_millis(100),
            },
            spawn_fn,
        );

        tokio::time::timeout(Duration::from_secs(3), supervisor.run())
            .await
            .expect("supervisor should exit after task succeeds");

        // Should have spawned 6 times: initial + 3 quick restarts + 1 delayed restart + 1 success
        // Actually: initial(0) fails → restart(1) fails → restart(2) fails → restart(3) sleeps 200ms
        // (window entries from 0,1,2 expire) → restart(3) fails → restart(4) succeeds
        // So counter = 6: calls 0,1,2,3,4,5
        // Wait, let me retrace:
        // Spawn 0 (initial): call=0, quick fail → allow_restart: window=[], len=0 < 3, record → restart
        // Spawn 1 (restart 1): call=1, quick fail → allow_restart: window=[t0], prune (none), len=1 < 3, record → restart
        // Spawn 2 (restart 2): call=2, quick fail → allow_restart: window=[t0,t1], prune (none), len=2 < 3, record → restart
        // Spawn 3 (restart 3): call=3, sleeps 200ms, fail → allow_restart: window=[t0,t1,t2], prune all (200ms > 100ms), len=0 < 3, record → restart
        // Spawn 4 (restart 4): call=4, succeeds → clean exit
        // Total: 5 spawns, counter=5
        assert_eq!(spawn_counter.load(Ordering::SeqCst), 5);
        assert!(!supervisor.is_tripped(&TaskId("window-test".into())));
    }

    #[test]
    fn circuit_breaker_unit_allows_up_to_max() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig {
            max_restarts: 3,
            window: Duration::from_secs(60),
        });

        assert!(cb.allow_restart()); // restart 1
        assert!(cb.allow_restart()); // restart 2
        assert!(cb.allow_restart()); // restart 3
        assert!(!cb.allow_restart()); // 4th — tripped
        assert!(!cb.allow_restart()); // still tripped
        assert!(cb.is_tripped());
        assert_eq!(cb.restart_count(), 3);
    }

    #[test]
    fn circuit_breaker_unit_prunes_old_entries() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig {
            max_restarts: 2,
            window: Duration::from_millis(50),
        });

        assert!(cb.allow_restart()); // restart 1
        assert!(cb.allow_restart()); // restart 2
        // Window is full, next would trip... but let's wait for expiry
        std::thread::sleep(Duration::from_millis(60));
        assert!(cb.allow_restart()); // old entries pruned, allowed
        assert!(!cb.is_tripped());
    }
}
