use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use tracing::info;

use borealis::core::pipeline::PipelineRunner;

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

    // Initialize the history schema.
    {
        let conn = db_conn.lock().expect("mutex not poisoned");
        borealis::history::schema::initialize(&conn)?;
    }
    info!("history schema initialized");

    // Create the history store.
    let history_store = Arc::new(borealis::history::store::HistoryStore::new(Arc::clone(
        &db_conn,
    )));

    // Create the memory store.
    let memory_store: Arc<dyn borealis::memory::Memory> =
        Arc::new(borealis::memory::SqliteMemory::new(
            Arc::clone(&db_conn),
            settings.bot.core_persona_path.clone(),
        )?);
    info!("memory store initialized");

    // Create the security module.
    let security = Arc::new(borealis::security::Security::new(
        &settings.rate_limit,
        settings.tools.computer_use.sandbox_root.clone(),
        settings.rate_limit.allowed_users.clone(),
    ));
    info!("security module initialized");

    // Create the tool registry with memory and history tools.
    let mut tool_registry = borealis::tools::ToolRegistry::new();
    borealis::tools::register_memory_tools(&mut tool_registry, Arc::clone(&memory_store));
    borealis::tools::register_history_tools(&mut tool_registry, Arc::clone(&history_store));
    let tool_registry = Arc::new(tool_registry);
    info!(
        tool_count = tool_registry.tool_count(),
        "tool registry initialized"
    );

    // Create the cancellation token — the single shutdown coordination primitive.
    let cancel = CancellationToken::new();

    // Spawn the signal handler that cancels the token on SIGINT/SIGTERM.
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        borealis::shutdown::wait_for_signal(signal_cancel).await;
    });

    // Build the LLM provider and pipeline from config.
    let pipeline: Arc<dyn PipelineRunner> = borealis::providers::registry::build_pipeline(
        &settings,
        Arc::clone(&history_store),
        Arc::clone(&tool_registry),
        memory_store,
        Arc::clone(&security),
    )?;

    // Spawn the scheduler if events are configured.
    if !settings.scheduler.events.is_empty() {
        let (sched_tx, mut sched_rx) = tokio::sync::mpsc::channel(256);
        let mut scheduler = borealis::scheduler::Scheduler::new(
            settings.scheduler.clone(),
            sched_tx,
            cancel.clone(),
        )?;
        scheduler.start();

        // Spawn a task that feeds scheduler events into the pipeline.
        let pipeline_sched = pipeline.clone();
        let cancel_sched = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = sched_rx.recv() => {
                        match pipeline_sched.process(&event).await {
                            Ok(_out_event) => {
                                tracing::debug!("scheduler event processed");
                            }
                            Err(e) => {
                                tracing::error!("scheduler pipeline error: {e}");
                            }
                        }
                    }
                    _ = cancel_sched.cancelled() => {
                        tracing::debug!("scheduler processing loop cancelled");
                        break;
                    }
                }
            }
        });

        info!(
            events = settings.scheduler.events.len(),
            "scheduler spawned"
        );
    }

    // Register channel adapters via the channel registry.
    let mut channels = borealis::channels::ChannelRegistry::new();
    borealis::channels::cli::register(&mut channels, &settings, pipeline.clone(), cancel.clone());
    borealis::channels::discord::register(
        &mut channels,
        &settings,
        pipeline.clone(),
        cancel.clone(),
    );

    if channels.channel_count() > 0 {
        info!(
            channels = ?channels.channel_names(),
            "channel registry ready"
        );
    } else {
        info!("no channel adapters enabled — waiting for shutdown signal");
    }

    // Wait for shutdown signal.
    cancel.cancelled().await;

    // Run the graceful shutdown sequence with 5s timeout.
    let drain = channels.await_shutdown();
    borealis::shutdown::run_shutdown(drain, Some(db_conn)).await;

    // Force exit — the tokio stdin reader holds the process alive because
    // its blocking read doesn't respect cancellation.
    std::process::exit(0);
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
