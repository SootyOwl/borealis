use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing (respects RUST_LOG env var).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .init();

    let args: Vec<String> = std::env::args().collect();

    // Check for `migrate-letta` subcommand.
    if args.len() >= 2 && args[1] == "migrate-letta" {
        return run_migrate_letta(&args[2..]);
    }

    let settings = borealis::config::Settings::load().inspect_err(|e| {
        eprintln!("error: {e}");
    })?;

    info!(bot_name = %settings.bot.name, "configuration loaded");

    info!(
        providers.anthropic = settings.providers.anthropic.is_some(),
        providers.openai = settings.providers.openai.is_some(),
        channels.cli = settings.channels.cli.is_some(),
        channels.discord = settings.channels.discord.is_some(),
        database.path = %settings.database.path.display(),
        "borealis ready"
    );

    // Open the SQLite database connection (shared across memory + history stores).
    let db_conn = Arc::new(Mutex::new(
        rusqlite::Connection::open(&settings.database.path)
            .inspect_err(|e| tracing::error!(path = %settings.database.path.display(), "failed to open database: {e}"))?,
    ));
    {
        let conn = db_conn.lock().expect("mutex not poisoned at startup");
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;",
        )?;
    }
    info!(path = %settings.database.path.display(), "database opened with WAL mode");

    // Create the cancellation token — the single shutdown coordination primitive.
    let cancel = CancellationToken::new();

    // Spawn the signal handler that cancels the token on SIGINT/SIGTERM.
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        borealis::shutdown::wait_for_signal(signal_cancel).await;
    });

    // Future phases wire up channel adapters and the event bus here.
    // For now, wait for shutdown signal.
    cancel.cancelled().await;

    // Run the graceful shutdown sequence with 5s timeout.
    let drain = async {
        // Future phases: shut down event bus, channel adapters, etc.
        // e.g., event_bus.shutdown().await;
    };

    borealis::shutdown::run_shutdown(drain, Some(db_conn)).await;

    Ok(())
}

fn run_migrate_letta(args: &[String]) -> anyhow::Result<()> {
    let mut source: Option<PathBuf> = None;
    let mut db_path = PathBuf::from("memory/borealis.db");
    let mut core_md = PathBuf::from("memory/core.md");

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--source" => {
                i += 1;
                if i >= args.len() {
                    anyhow::bail!("--source requires a path argument");
                }
                source = Some(PathBuf::from(&args[i]));
            }
            "--db" => {
                i += 1;
                if i >= args.len() {
                    anyhow::bail!("--db requires a path argument");
                }
                db_path = PathBuf::from(&args[i]);
            }
            "--core-md" => {
                i += 1;
                if i >= args.len() {
                    anyhow::bail!("--core-md requires a path argument");
                }
                core_md = PathBuf::from(&args[i]);
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: borealis migrate-letta --source <path> [--db <path>] [--core-md <path>]"
                );
                eprintln!();
                eprintln!("Import data from Letta (MemGPT) JSON exports into Borealis.");
                eprintln!();
                eprintln!("Options:");
                eprintln!(
                    "  --source <path>   Directory containing Letta JSON export files (required)"
                );
                eprintln!("  --db <path>       SQLite database path (default: memory/borealis.db)");
                eprintln!("  --core-md <path>  Path to core.md (default: memory/core.md)");
                return Ok(());
            }
            other => {
                anyhow::bail!(
                    "unknown argument: {other}\nRun 'borealis migrate-letta --help' for usage"
                );
            }
        }
        i += 1;
    }

    let source = source.ok_or_else(|| {
        anyhow::anyhow!("--source is required\nRun 'borealis migrate-letta --help' for usage")
    })?;

    if !source.is_dir() {
        anyhow::bail!(
            "source path does not exist or is not a directory: {}",
            source.display()
        );
    }

    info!(source = %source.display(), db = %db_path.display(), core_md = %core_md.display(), "starting Letta migration");

    let stats = borealis::migrate::run_migration(&source, &db_path, &core_md)?;
    println!("{stats}");

    Ok(())
}
