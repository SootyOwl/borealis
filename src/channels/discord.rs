use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use poise::serenity_prelude as serenity;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::channels::modes::{AlwaysMode, DigestMode, MentionOnlyMode, ModeRouter, ResponseMode};
use crate::channels::{Channel, ChannelRegistry};
use crate::config::{DiscordChannelConfig, Settings};
use crate::core::event::{
    Author, ChannelSource, ConversationId, Directive, DirectiveKind, InEvent, Message,
    MessageContext, MessageId, OutEvent,
};
use crate::core::pipeline::PipelineRunner;

/// Register the Discord adapter with the channel registry if enabled in config.
pub fn register(
    registry: &mut ChannelRegistry,
    settings: &Settings,
    pipeline: Arc<dyn PipelineRunner>,
    cancel: CancellationToken,
) {
    let config = match &settings.channels.discord {
        Some(c) if c.enabled => c.clone(),
        _ => return,
    };

    // Build mode router from config groups.
    let mut modes: HashMap<String, Arc<dyn ResponseMode>> = HashMap::new();
    for group in &config.groups {
        let mode: Arc<dyn ResponseMode> = match group.response_mode.as_str() {
            "mention-only" => Arc::new(MentionOnlyMode),
            "digest" => {
                let interval = Duration::from_secs(group.digest_interval_min.unwrap_or(5) * 60);
                let debounce = Duration::from_secs(group.digest_debounce_min.unwrap_or(2) * 60);
                Arc::new(DigestMode::new(interval, debounce))
            }
            _ => Arc::new(AlwaysMode),
        };
        modes.insert(group.guild_id.clone(), mode);
    }
    let mode_router = Arc::new(ModeRouter::new(modes, Arc::new(AlwaysMode)));

    let discord = Arc::new(DiscordAdapter::new(
        config,
        mode_router,
        settings.bot.name.clone(),
    ));
    registry.register(discord, pipeline, cancel);
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
    http: Arc<tokio::sync::OnceCell<Arc<serenity::Http>>>,
    /// Cache, set after the framework connects.
    cache: Arc<tokio::sync::OnceCell<Arc<serenity::Cache>>>,
}

impl DiscordAdapter {
    pub fn new(
        config: DiscordChannelConfig,
        mode_router: Arc<ModeRouter>,
        bot_name: String,
    ) -> Self {
        Self {
            config,
            mode_router,
            bot_name,
            http: Arc::new(tokio::sync::OnceCell::new()),
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
    }
}

/// Determine the group ID for mode routing.
/// DMs use the user ID, guild channels use the channel ID.
fn group_id_for_message(msg: &serenity::Message) -> String {
    if msg.guild_id.is_none() {
        // DMs — route by user
        msg.author.id.to_string()
    } else {
        msg.channel_id.to_string()
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
                            let group_id = group_id_for_message(new_message);

                            // Route through mode
                            let dispatch_events =
                                data.mode_router.on_message(&group_id, in_event).await;

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

            // Send text response
            if let Some(text) = &event.text {
                // Truncate to Discord's 2000 char limit
                let text = if text.len() > 1997 {
                    format!("{}...", &text[..1997])
                } else {
                    text.clone()
                };

                if let Err(e) = channel.say(&http, &text).await {
                    error!(error = %e, "failed to send Discord message");
                }
            }

            // Handle directives
            for directive in &event.directives {
                match directive {
                    Directive::NoReply => {
                        debug!("Discord: NoReply directive");
                    }
                    Directive::React { emoji, message_id } => {
                        if let Some(msg_id) = message_id
                            .as_ref()
                            .and_then(|id| id.parse::<u64>().ok())
                            .or_else(|| event.reply_to.as_ref()?.0.parse::<u64>().ok())
                        {
                            let reaction = serenity::ReactionType::Unicode(emoji.clone());
                            if let Err(e) = http
                                .create_reaction(channel_id.into(), msg_id.into(), &reaction)
                                .await
                            {
                                warn!(error = %e, emoji, "failed to add reaction");
                            }
                        } else {
                            debug!(
                                emoji,
                                "React directive without target message ID — skipping"
                            );
                        }
                    }
                    Directive::Send {
                        channel: target_channel,
                        text,
                        ..
                    } => {
                        if let Ok(target_id) = target_channel.parse::<u64>() {
                            let target = serenity::ChannelId::new(target_id);
                            if let Err(e) = target.say(&http, text).await {
                                error!(error = %e, target = target_channel, "failed to send cross-channel message");
                            }
                        } else {
                            warn!(
                                target = target_channel,
                                "Send directive with non-numeric channel — skipping"
                            );
                        }
                    }
                    Directive::Voice { .. } | Directive::SendFile { .. } => {
                        debug!(directive = ?directive, "unsupported Discord directive — skipping");
                    }
                }
            }
        }

        info!("Discord adapter outbound finished");
        Ok(())
    }

    fn supported_directives(&self) -> Vec<DirectiveKind> {
        vec![
            DirectiveKind::NoReply,
            DirectiveKind::React,
            DirectiveKind::Send,
        ]
    }
}
