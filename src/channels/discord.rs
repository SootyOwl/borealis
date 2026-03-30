use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use poise::serenity_prelude as serenity;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::channels::modes::{ConfigModeFactory, ModeFactory, ModeRouter, ResponseMode};
use crate::channels::{Channel, ChannelRegistration, ChannelRegistry};
use crate::config::{DiscordChannelConfig, Settings};
use crate::core::event::{
    Author, ChannelSource, ConversationId, InEvent, Message, MessageContext, MessageId, OutEvent,
};
use crate::core::pipeline::PipelineRunner;

use crate::tools::DiscordHttpHandle;

// Auto-registration via inventory
inventory::submit! {
    ChannelRegistration {
        name: "discord",
        register_fn: |registry, deps| {
            register(registry, deps.settings, deps.pipeline.clone(), deps.cancel.clone(), deps.security.clone(), Arc::clone(&deps.discord_http));
        },
    }
}

/// Register the Discord adapter with the channel registry if enabled in config.
pub fn register(
    registry: &mut ChannelRegistry,
    settings: &Settings,
    pipeline: Arc<dyn PipelineRunner>,
    cancel: CancellationToken,
    security: Arc<crate::security::Security>,
    discord_http: DiscordHttpHandle,
) {
    let config = match &settings.channels.discord {
        Some(c) if c.enabled => c.clone(),
        _ => return,
    };

    // Build mode router from config groups + per-channel overrides.
    // Guild factories create per-channel mode instances on the fly.
    let mut channel_modes: HashMap<String, Arc<dyn ResponseMode>> = HashMap::new();
    let mut guild_factories: HashMap<String, Arc<dyn ModeFactory>> = HashMap::new();
    for group in &config.groups {
        // Guild factory — used to create modes for channels not explicitly configured.
        let factory = Arc::new(ConfigModeFactory::new(
            &group.response_mode,
            group.digest_interval_min,
            group.digest_debounce_min,
        ));
        debug!(guild = %group.guild_id, mode = %group.response_mode, "registered guild mode");
        guild_factories.insert(group.guild_id.clone(), factory as Arc<dyn ModeFactory>);

        // Per-channel overrides — each gets its own mode instance.
        for ch in &group.channels {
            let mode_name = ch.response_mode.as_deref().unwrap_or(&group.response_mode);
            let ch_factory = ConfigModeFactory::new(
                mode_name,
                ch.digest_interval_min.or(group.digest_interval_min),
                ch.digest_debounce_min.or(group.digest_debounce_min),
            );
            debug!(
                guild = %group.guild_id,
                channel = %ch.channel_id,
                mode = mode_name,
                "registered channel mode override"
            );
            channel_modes.insert(ch.channel_id.clone(), ch_factory.create());
        }
    }
    let default_factory = Arc::new(ConfigModeFactory::new("mention-only", None, None));
    let mode_router = Arc::new(ModeRouter::new(channel_modes, guild_factories, default_factory));

    let discord = Arc::new(DiscordAdapter::new(
        config,
        mode_router,
        settings.bot.name.clone(),
        discord_http,
    ));
    registry.register(discord, pipeline, cancel, Some(security));
}

/// Shared state available inside poise's event handler.
struct BotData {
    event_tx: Sender<InEvent>,
    mode_router: Arc<ModeRouter>,
    bot_user_id: serenity::UserId,
}

type PoiseError = Box<dyn std::error::Error + Send + Sync>;

/// Discord adapter using poise 0.6.1 (built on serenity 0.12.4).
///
/// Inbound: poise's `event_handler` captures `FullEvent::Message`, converts to `InEvent`,
/// routes through the `ModeRouter`, and sends to the event bus.
///
/// Outbound: a separate task consumes `OutEvent`s from its dedicated mpsc receiver
/// and sends messages via serenity's HTTP client.
pub struct DiscordAdapter {
    config: DiscordChannelConfig,
    mode_router: Arc<ModeRouter>,
    #[allow(dead_code)] // Used for mention detection in future enhancements
    bot_name: String,
    /// Serenity HTTP client, set after the framework connects.
    /// Shared with channel tools so they can call Discord API directly.
    http: DiscordHttpHandle,
    /// Cache, set after the framework connects.
    cache: Arc<tokio::sync::OnceCell<Arc<serenity::Cache>>>,
}

impl DiscordAdapter {
    pub fn new(
        config: DiscordChannelConfig,
        mode_router: Arc<ModeRouter>,
        bot_name: String,
        discord_http: DiscordHttpHandle,
    ) -> Self {
        Self {
            config,
            mode_router,
            bot_name,
            http: discord_http,
            cache: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Resolve the Discord bot token from the configured environment variable.
    fn resolve_token(&self) -> Result<String> {
        std::env::var(&self.config.token_env)
            .with_context(|| format!("Discord token env var '{}' not set", self.config.token_env))
    }
}

/// Convert a serenity Message into our InEvent, detecting bot mentions.
fn serenity_message_to_in_event(msg: &serenity::Message, bot_user_id: serenity::UserId) -> InEvent {
    let mentions_bot = msg.mentions.iter().any(|u| u.id == bot_user_id);

    let is_dm = msg.guild_id.is_none();

    let conversation_id = if is_dm {
        ConversationId::Dm {
            channel_type: ChannelSource::Discord,
            user_id: msg.author.id.to_string(),
        }
    } else {
        ConversationId::Group {
            channel_type: ChannelSource::Discord,
            group_id: msg.channel_id.to_string(),
        }
    };

    InEvent {
        source: ChannelSource::Discord,
        message: Message {
            id: MessageId(msg.id.to_string()),
            author: Author {
                id: msg.author.id.to_string(),
                display_name: msg
                    .member
                    .as_ref()
                    .and_then(|m| m.nick.clone())
                    .unwrap_or_else(|| msg.author.name.clone()),
            },
            text: msg.content.clone(),
            timestamp: Utc::now(),
            mentions_bot,
        },
        context: MessageContext {
            conversation_id,
            channel_id: msg.channel_id.to_string(),
            reply_to: msg
                .referenced_message
                .as_ref()
                .map(|m| MessageId(m.id.to_string())),
        },
        tool_groups: None,
        completion_flag: None,
    }
}

/// Determine the group ID for mode routing.
/// DMs use the user ID, guild channels use the guild ID (matching mode router keys).
fn group_id_for_message(msg: &serenity::Message) -> String {
    match msg.guild_id {
        Some(guild_id) => guild_id.to_string(),
        None => msg.author.id.to_string(),
    }
}

impl Channel for DiscordAdapter {
    fn name(&self) -> &str {
        "discord"
    }

    async fn run_inbound(self: Arc<Self>, tx: Sender<InEvent>) -> Result<()> {
        info!("Discord adapter starting");

        let token = self.resolve_token()?;
        let intents = serenity::GatewayIntents::GUILD_MESSAGES
            | serenity::GatewayIntents::DIRECT_MESSAGES
            | serenity::GatewayIntents::MESSAGE_CONTENT;

        let mode_router = Arc::clone(&self.mode_router);
        let http_cell = Arc::clone(&self.http);
        let cache_cell = Arc::clone(&self.cache);
        let event_tx = tx.clone();

        // Spawn a tick task for digest mode polling
        let tick_router = Arc::clone(&self.mode_router);
        let tick_tx = tx;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                let events = tick_router.on_tick().await;
                for event in events {
                    if tick_tx.send(event).await.is_err() {
                        debug!("event bus closed — digest tick task exiting");
                        return;
                    }
                }
            }
        });

        let framework = poise::Framework::builder()
            .setup(
                move |ctx, ready, _framework: &poise::Framework<_, PoiseError>| {
                    Box::pin(async move {
                        info!(
                            bot_name = %ready.user.name,
                            "Discord bot connected"
                        );

                        // Store HTTP and cache for outbound use
                        let _ = http_cell.set(Arc::clone(&ctx.http));
                        let _ = cache_cell.set(ctx.cache.clone());

                        Ok(BotData {
                            event_tx,
                            mode_router,
                            bot_user_id: ready.user.id,
                        })
                    })
                },
            )
            .options(poise::FrameworkOptions {
                event_handler: |ctx, event, _framework, data| {
                    Box::pin(async move {
                        if let serenity::FullEvent::Message { new_message } = event {
                            // Ignore messages from the bot itself
                            if new_message.author.id == data.bot_user_id {
                                return Ok(());
                            }

                            // Ignore bot messages
                            if new_message.author.bot {
                                return Ok(());
                            }

                            let _ = ctx; // available if needed for fetching member info etc.
                            let in_event =
                                serenity_message_to_in_event(new_message, data.bot_user_id);
                            let channel_id = new_message.channel_id.to_string();
                            let guild_id = group_id_for_message(new_message);

                            // Route through mode (channel override → guild default → global default)
                            let dispatch_events = data
                                .mode_router
                                .on_message(&channel_id, &guild_id, in_event)
                                .await;

                            for event in dispatch_events {
                                if data.event_tx.send(event).await.is_err() {
                                    warn!("event bus closed — dropping Discord message");
                                    break;
                                }
                            }
                        }

                        Ok(())
                    })
                },
                // Disable poise's built-in command prefix handling — we use
                // the event_handler for all message routing, not poise commands.
                prefix_options: poise::PrefixFrameworkOptions {
                    mention_as_prefix: false,
                    ..Default::default()
                },
                ..Default::default()
            })
            .build();

        let mut client = serenity::ClientBuilder::new(token, intents)
            .framework(framework)
            .await
            .context("failed to create Discord client")?;

        client
            .start()
            .await
            .context("Discord client connection error")?;

        Ok(())
    }

    async fn run_outbound(self: Arc<Self>, mut rx: Receiver<OutEvent>) -> Result<()> {
        info!("Discord adapter outbound waiting for connection");

        // Wait for the HTTP client to be available (set during framework setup)
        let http = loop {
            if let Some(http) = self.http.get() {
                break Arc::clone(http);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };

        info!("Discord adapter outbound started");

        while let Some(event) = rx.recv().await {
            let channel_id: u64 = match event.channel_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    error!(channel_id = %event.channel_id, error = %e, "invalid channel ID");
                    continue;
                }
            };

            let channel = serenity::ChannelId::new(channel_id);

            // Send text response (empty text = no reply, per REQ-11 convention)
            if let Some(text) = &event.text {
                if !text.is_empty() {
                    // Truncate to Discord's 2000 char limit (safe for multi-byte UTF-8)
                    let text = if text.len() > 1997 {
                        let end = (0..=1997).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
                        format!("{}...", &text[..end])
                    } else {
                        text.clone()
                    };

                    if let Err(e) = channel.say(&http, &text).await {
                        error!(error = %e, "failed to send Discord message");
                    }
                }
            }
        }

        info!("Discord adapter outbound finished");
        Ok(())
    }
}
