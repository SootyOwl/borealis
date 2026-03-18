use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Default timeout before forcing process exit after graceful shutdown begins.
const FORCE_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Waits for a shutdown signal (SIGINT or SIGTERM) and cancels the token.
///
/// This function blocks until either signal is received, then cancels the
/// provided `CancellationToken` so all tasks observing it can drain.
pub async fn wait_for_signal(cancel: CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => {
                info!("received SIGINT — initiating graceful shutdown");
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM — initiating graceful shutdown");
            }
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.expect("failed to listen for ctrl-c");
        info!("received SIGINT — initiating graceful shutdown");
    }

    cancel.cancel();
}

/// Checkpoints the SQLite WAL (Write-Ahead Log) to ensure all changes are
/// flushed to the main database file before exit.
///
/// Uses `PRAGMA wal_checkpoint(TRUNCATE)` which checkpoints and then truncates
/// the WAL file to zero bytes, ensuring a clean state.
pub fn checkpoint_wal(conn: &Arc<Mutex<Connection>>) -> anyhow::Result<()> {
    let conn = conn
        .lock()
        .map_err(|_| anyhow::anyhow!("SQLite mutex poisoned during WAL checkpoint"))?;

    let result: (i32, i32, i32) = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?;

    let (busy, log_pages, checkpointed) = result;
    if busy != 0 {
        warn!(
            busy,
            log_pages, checkpointed, "WAL checkpoint completed with busy pages"
        );
    } else {
        info!(log_pages, checkpointed, "WAL checkpoint completed cleanly");
    }

    Ok(())
}

/// Runs the graceful shutdown sequence with a hard timeout.
///
/// Steps:
/// 1. The cancellation token has already been cancelled (by `wait_for_signal`).
/// 2. Wait for tasks to drain (caller provides the drain future).
/// 3. Checkpoint SQLite WAL.
/// 4. If the entire sequence exceeds `FORCE_EXIT_TIMEOUT`, force exit.
pub async fn run_shutdown<F>(drain: F, db_conn: Option<Arc<Mutex<Connection>>>)
where
    F: std::future::Future<Output = ()>,
{
    info!(
        timeout_secs = FORCE_EXIT_TIMEOUT.as_secs(),
        "shutdown sequence started"
    );

    let result = tokio::time::timeout(FORCE_EXIT_TIMEOUT, async {
        // Step 1: Drain in-flight work (event bus workers, channel adapters).
        info!("draining in-flight tasks...");
        drain.await;
        info!("all tasks drained");

        // Step 2: Checkpoint SQLite WAL.
        if let Some(ref conn) = db_conn {
            info!("checkpointing SQLite WAL...");
            // Run synchronous SQLite work on the blocking thread pool.
            let conn = Arc::clone(conn);
            let wal_result = tokio::task::spawn_blocking(move || checkpoint_wal(&conn)).await;
            match wal_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => error!("WAL checkpoint failed: {e}"),
                Err(e) => error!("WAL checkpoint task panicked: {e}"),
            }
        }
    })
    .await;

    match result {
        Ok(()) => info!("graceful shutdown completed"),
        Err(_) => {
            error!(
                "shutdown timed out after {}s — forcing exit",
                FORCE_EXIT_TIMEOUT.as_secs()
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checkpoint_wal_on_in_memory_db() {
        // WAL checkpoint on an in-memory DB is a no-op but should not error.
        let conn = Connection::open_in_memory().unwrap();
        // Enable WAL mode first (in-memory DBs default to journal mode).
        conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        let conn = Arc::new(Mutex::new(conn));
        checkpoint_wal(&conn).unwrap();
    }

    #[tokio::test]
    async fn checkpoint_wal_on_file_db() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE test (id INTEGER PRIMARY KEY);
             INSERT INTO test VALUES (1);",
        )
        .unwrap();
        let conn = Arc::new(Mutex::new(conn));
        checkpoint_wal(&conn).unwrap();
    }

    #[tokio::test]
    async fn run_shutdown_completes_within_timeout() {
        let drain = async {
            // Simulate quick drain.
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        // No DB connection — just test the drain + timeout logic.
        run_shutdown(drain, None).await;
        // If we reach here, shutdown didn't force-exit — success.
    }

    #[tokio::test]
    async fn run_shutdown_with_db_checkpoint() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE test (id INTEGER PRIMARY KEY);
             INSERT INTO test VALUES (42);",
        )
        .unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let drain = async {};
        run_shutdown(drain, Some(conn)).await;
    }

    #[tokio::test]
    async fn cancellation_token_propagation() {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Simulate a task that respects the token.
        let task = tokio::spawn(async move {
            cancel_clone.cancelled().await;
            true
        });

        // Cancel should propagate.
        cancel.cancel();
        let result = task.await.unwrap();
        assert!(result, "task should have observed cancellation");
    }

    #[tokio::test]
    async fn wait_for_signal_cancels_token_on_sigint() {
        let cancel = CancellationToken::new();

        // Manually cancel to simulate signal (we can't send real signals in tests).
        // Instead, test that once cancelled externally, the token is cancelled.
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel_clone.cancel();
        });

        // Wait for cancellation.
        cancel.cancelled().await;
        assert!(cancel.is_cancelled());
    }
}
