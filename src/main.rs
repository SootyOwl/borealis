use tracing::info;

fn main() -> anyhow::Result<()> {
    // Initialize tracing (respects RUST_LOG env var).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .init();

    let settings = borealis::config::Settings::load().inspect_err(|e| {
        eprintln!("error: {e}");
    })?;

    info!(bot_name = %settings.bot.name, "configuration loaded");

    // Future phases wire up the runtime here.
    // For now, print a summary to confirm config loaded correctly.
    info!(
        providers.anthropic = settings.providers.anthropic.is_some(),
        providers.openai = settings.providers.openai.is_some(),
        channels.cli = settings.channels.cli.is_some(),
        channels.discord = settings.channels.discord.is_some(),
        database.path = %settings.database.path.display(),
        "borealis ready"
    );

    Ok(())
}
