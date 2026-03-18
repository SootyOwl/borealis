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
    let memory_store = borealis::memory::MemoryStore::new(
        Arc::clone(&db_conn),
        settings.bot.core_persona_path.clone(),
    )?;
    info!("memory store initialized");

    // Create the tool registry with memory tools.
    let mut tool_registry = borealis::tools::ToolRegistry::new();
    borealis::tools::register_memory_tools(&mut tool_registry, memory_store.clone());
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
    let pipeline: Arc<dyn PipelineRunner> = build_pipeline(
        &settings,
        Arc::clone(&history_store),
        Arc::clone(&tool_registry),
        memory_store,
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

    // Wire up the CLI adapter if enabled.
    let cli_enabled = settings.channels.cli.as_ref().is_some_and(|c| c.enabled);

    if cli_enabled {
        let (in_tx, mut in_rx) = tokio::sync::mpsc::channel(256);
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(256);

        let cli = Arc::new(borealis::channels::cli::CliAdapter::new(
            settings.bot.name.clone(),
        ));

        // Spawn CLI inbound (stdin reader).
        let cli_in = cli.clone();
        let cancel_in = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                result = borealis::channels::Channel::run_inbound(cli_in, in_tx) => {
                    if let Err(e) = result {
                        tracing::error!("CLI inbound error: {e}");
                    }
                }
                _ = cancel_in.cancelled() => {
                    tracing::debug!("CLI inbound cancelled");
                }
            }
        });

        // Spawn CLI outbound (stdout writer).
        let cli_out = cli.clone();
        let cancel_out = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                result = borealis::channels::Channel::run_outbound(cli_out, out_rx) => {
                    if let Err(e) = result {
                        tracing::error!("CLI outbound error: {e}");
                    }
                }
                _ = cancel_out.cancelled() => {
                    tracing::debug!("CLI outbound cancelled");
                }
            }
        });

        // Spawn the message processing loop.
        let pipeline_clone = pipeline.clone();
        let cancel_loop = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = in_rx.recv() => {
                        match pipeline_clone.process(&event).await {
                            Ok(out_event) => {
                                if out_tx.send(out_event).await.is_err() {
                                    tracing::debug!("outbound channel closed");
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::error!("pipeline error: {e}");
                                // Send an error message back to the user.
                                let err_event = borealis::core::event::OutEvent {
                                    target: event.source.clone(),
                                    channel_id: event.context.channel_id.clone(),
                                    text: Some("I'm having trouble thinking right now, try again in a moment.".into()),
                                    directives: vec![],
                                    reply_to: Some(event.message.id.clone()),
                                };
                                let _ = out_tx.send(err_event).await;
                            }
                        }
                    }
                    _ = cancel_loop.cancelled() => {
                        tracing::debug!("processing loop cancelled");
                        break;
                    }
                }
            }
        });

        info!(
            "CLI adapter running — type a message to chat with {}",
            settings.bot.name
        );
    } else {
        info!("no channel adapters enabled — waiting for shutdown signal");
    }

    // Wait for shutdown signal.
    cancel.cancelled().await;

    // Run the graceful shutdown sequence with 5s timeout.
    let drain = async {};
    borealis::shutdown::run_shutdown(drain, Some(db_conn)).await;

    // Force exit — the tokio stdin reader holds the process alive because
    // its blocking read doesn't respect cancellation.
    std::process::exit(0);
}

fn build_pipeline(
    settings: &borealis::config::Settings,
    history_store: Arc<borealis::history::store::HistoryStore>,
    tool_registry: Arc<borealis::tools::ToolRegistry>,
    memory_store: borealis::memory::MemoryStore,
) -> anyhow::Result<Arc<dyn PipelineRunner>> {
    let sys_path = &settings.bot.system_prompt_path;
    let persona_path = &settings.bot.core_persona_path;
    let compaction_config = settings.bot.compaction.clone();
    let compaction_state = Arc::new(borealis::history::compaction::CompactionState::new());

    // Prefer Anthropic if configured, otherwise fall back to OpenAI-compatible.
    if let Some(ref anthropic) = settings.providers.anthropic {
        let api_key = anthropic
            .api_key_env
            .as_ref()
            .and_then(|env| std::env::var(env).ok())
            .unwrap_or_default();

        let config = borealis::providers::ProviderConfig {
            api_key,
            base_url: anthropic.base_url.clone(),
            model: anthropic.model.clone(),
            timeout_secs: anthropic.timeout_secs,
            max_retries: anthropic.max_retries,
        };

        let provider = Arc::new(borealis::providers::anthropic::AnthropicProvider::new(
            config,
        )?);
        info!(model = %anthropic.model, "using Anthropic provider");

        let pipeline_config = borealis::core::pipeline::PipelineConfig {
            model_max_tokens: anthropic.max_history_tokens,
            response_reserve: 1024,
        };

        let pipeline = borealis::core::pipeline::Pipeline::new(
            provider,
            sys_path,
            persona_path,
            history_store,
            tool_registry,
            memory_store,
            compaction_config,
            compaction_state,
            pipeline_config,
        )?;
        return Ok(Arc::new(pipeline));
    }

    if let Some(ref openai) = settings.providers.openai {
        let api_key = openai
            .api_key_env
            .as_ref()
            .and_then(|env| std::env::var(env).ok())
            .unwrap_or_default();

        let config = borealis::providers::ProviderConfig {
            api_key,
            base_url: openai.base_url.clone(),
            model: openai.model.clone(),
            timeout_secs: openai.timeout_secs,
            max_retries: openai.max_retries,
        };

        let provider = Arc::new(borealis::providers::openai::OpenAiProvider::new(config)?);
        info!(model = %openai.model, "using OpenAI-compatible provider");

        let pipeline_config = borealis::core::pipeline::PipelineConfig {
            model_max_tokens: openai.max_history_tokens,
            response_reserve: 1024,
        };

        let pipeline = borealis::core::pipeline::Pipeline::new(
            provider,
            sys_path,
            persona_path,
            history_store,
            tool_registry,
            memory_store,
            compaction_config,
            compaction_state,
            pipeline_config,
        )?;
        return Ok(Arc::new(pipeline));
    }

    anyhow::bail!(
        "no LLM provider configured — add [providers.openai] or [providers.anthropic] to config"
    )
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
