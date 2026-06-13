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

/// Discord's per-message character limit (Unicode scalar values, not bytes).
const DISCORD_MESSAGE_LIMIT: usize = 2000;

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
        cancel.clone(),
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
    /// Process-wide cancellation token, so background tasks (e.g. the digest
    /// tick loop) can observe shutdown and stop instead of leaking.
    cancel: CancellationToken,
}

impl DiscordAdapter {
    pub fn new(
        config: DiscordChannelConfig,
        mode_router: Arc<ModeRouter>,
        bot_name: String,
        discord_http: DiscordHttpHandle,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            mode_router,
            bot_name,
            http: discord_http,
            cache: Arc::new(tokio::sync::OnceCell::new()),
            cancel,
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
            guild_id: msg.guild_id.map(|g| g.to_string()),
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

/// Split a reply into consecutive chunks, each at most `limit` Unicode scalar
/// values (chars) long, for Discord's 2000-CHARACTER message limit.
///
/// Discord counts characters, not bytes, so we measure in `char`s and never
/// split a multi-byte char. When the next chunk would exceed `limit`, we prefer
/// to break at a newline or whitespace boundary reasonably close to the limit
/// (to avoid cutting mid-line/mid-word); otherwise we hard-split at exactly
/// `limit` chars. Never panics on multi-byte content.
fn split_message(text: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        // Degenerate guard — return the whole text as a single chunk rather than
        // looping forever. Callers always pass a positive limit (2000).
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        };
    }

    let mut chunks = Vec::new();
    // Track remaining text by char index using (char_index, byte_index) pairs.
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let total = chars.len();
    let mut start = 0; // char index into `chars`

    while start < total {
        let remaining = total - start;
        if remaining <= limit {
            // The rest fits in one chunk.
            let byte_start = chars[start].0;
            chunks.push(text[byte_start..].to_string());
            break;
        }

        // Hard end is the furthest we can take without exceeding the char limit.
        let hard_end = start + limit; // char index (exclusive)

        // Look for a nicer break point at or before `hard_end`. We search
        // backwards for a newline first, then any whitespace, but only accept a
        // break that isn't too far back (keep chunks reasonably full).
        let min_break = start + limit / 2; // don't break before halfway
        let mut break_at = None;
        // Whether `end` points AT a single whitespace char to drop at the seam.
        // Only the whitespace branch sets this; the newline branch breaks AFTER
        // the newline (keeping it) so the following char must be preserved.
        let mut drop_seam_ws = false;

        // Prefer a newline boundary: break AFTER the newline char (it stays in
        // the chunk; the next char begins the following chunk unchanged).
        for i in (start..hard_end).rev() {
            if chars[i].1 == '\n' {
                if i + 1 > min_break {
                    break_at = Some(i + 1);
                }
                break;
            }
        }

        // Fall back to a whitespace boundary: break BEFORE the whitespace char,
        // dropping that single boundary whitespace at the chunk seam.
        if break_at.is_none() {
            for i in (start..hard_end).rev() {
                if chars[i].1.is_whitespace() {
                    if i > min_break {
                        break_at = Some(i);
                        drop_seam_ws = true;
                    }
                    break;
                }
            }
        }

        let end = break_at.unwrap_or(hard_end); // char index (exclusive)
        let byte_start = chars[start].0;
        let chunk: String = if end >= total {
            text[byte_start..].to_string()
        } else {
            let byte_end = chars[end].0;
            text[byte_start..byte_end].to_string()
        };
        chunks.push(chunk);

        // Advance past the chunk. For a whitespace-boundary break, also skip the
        // single seam whitespace char so it isn't reproduced at the next chunk's
        // start. Newline and hard-split breaks drop nothing.
        start = if drop_seam_ws { end + 1 } else { end };
    }

    chunks
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
        let tick_cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    // Observe cancellation so this task stops on shutdown instead
                    // of ticking forever and leaking (it also holds an event Sender).
                    _ = tick_cancel.cancelled() => {
                        debug!("cancellation received — digest tick task exiting");
                        return;
                    }
                    _ = interval.tick() => {
                        let events = tick_router.on_tick().await;
                        for event in events {
                            if tick_tx.send(event).await.is_err() {
                                debug!("event bus closed — digest tick task exiting");
                                return;
                            }
                        }
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

                            // DMs are 1:1 conversations and always get a response.
                            // Mode routing (mention-only / digest) is a guild/channel
                            // concept — and a DM can't @-mention via guild membership —
                            // so bypass the router entirely and dispatch directly.
                            // Otherwise the default mention-only mode would silently
                            // drop every DM (no reply, no history).
                            let dispatch_events = if new_message.guild_id.is_none() {
                                vec![in_event]
                            } else {
                                let channel_id = new_message.channel_id.to_string();
                                let guild_id = group_id_for_message(new_message);

                                // Route through mode (channel override → guild default → global default)
                                data.mode_router
                                    .on_message(&channel_id, &guild_id, in_event)
                                    .await
                            };

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
                    // Discord caps messages at 2000 CHARACTERS (not bytes). Split
                    // long replies into ordered chunks instead of truncating so we
                    // never lose content or chop a multi-byte char.
                    for chunk in split_message(text, DISCORD_MESSAGE_LIMIT) {
                        if let Err(e) = channel.say(&http, &chunk).await {
                            // Stop sending the rest of this reply on the first
                            // failure rather than spinning on a broken channel.
                            error!(error = %e, "failed to send Discord message; aborting remaining chunks");
                            break;
                        }
                    }
                }
            }
        }

        info!("Discord adapter outbound finished");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{DISCORD_MESSAGE_LIMIT, split_message};

    /// Every chunk must respect the char limit and never split a char.
    fn assert_within_limit(chunks: &[String], limit: usize) {
        for chunk in chunks {
            assert!(
                chunk.chars().count() <= limit,
                "chunk exceeds limit: {} chars > {limit}",
                chunk.chars().count()
            );
        }
    }

    #[test]
    fn ascii_under_limit_is_single_chunk() {
        let text = "hello world";
        let chunks = split_message(text, 2000);
        assert_eq!(chunks, vec!["hello world".to_string()]);
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(split_message("", 2000).is_empty());
    }

    #[test]
    fn exactly_at_limit_is_single_chunk() {
        let text = "a".repeat(2000);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chars().count(), 2000);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn one_over_limit_splits_into_two() {
        // 2001 'a's with no whitespace forces a hard split at the limit.
        let text = "a".repeat(2001);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_within_limit(&chunks, 2000);
        assert_eq!(chunks[0].chars().count(), 2000);
        assert_eq!(chunks[1].chars().count(), 1);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn hard_split_no_whitespace_preserves_content() {
        let text = "x".repeat(5000);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 3);
        assert_within_limit(&chunks, 2000);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn prefers_newline_boundary_near_limit() {
        // Fill most of a chunk, put a newline near the end, then more text.
        // Expect the first chunk to end at the newline (newline kept), and the
        // second chunk to start with the following text.
        let mut text = "a".repeat(1990);
        text.push('\n');
        text.push_str(&"b".repeat(100));
        let chunks = split_message(&text, 2000);
        assert!(chunks.len() >= 2);
        assert_within_limit(&chunks, 2000);
        // First chunk ends with the newline boundary.
        assert!(chunks[0].ends_with('\n'));
        // No content lost.
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn prefers_whitespace_boundary_and_drops_seam_space() {
        // 1995 'a's, a space, then a long run of 'b's. The break should happen
        // at the space; that single seam space is dropped (not duplicated).
        let mut text = "a".repeat(1995);
        text.push(' ');
        text.push_str(&"b".repeat(100));
        let chunks = split_message(&text, 2000);
        assert!(chunks.len() >= 2);
        assert_within_limit(&chunks, 2000);
        // The seam space is dropped, so concatenation is missing exactly one space.
        assert_eq!(chunks.concat(), text.replacen(' ', "", 1));
        // First chunk is the run of 'a's with no trailing space.
        assert_eq!(chunks[0], "a".repeat(1995));
    }

    #[test]
    fn newline_break_preserves_following_indent_whitespace() {
        // Break on a newline whose NEXT char is whitespace (an indented
        // continuation line, very common in formatted replies). The newline
        // branch must keep that following whitespace — only the whitespace
        // branch drops a seam char. Regression test for the seam-skip bug.
        let mut text = "a".repeat(1990);
        text.push('\n');
        text.push(' '); // leading indent of the continued line
        text.push_str(&"b".repeat(100));
        let chunks = split_message(&text, 2000);
        assert!(chunks.len() >= 2);
        assert_within_limit(&chunks, 2000);
        // Nothing dropped: the indent space survives at the start of chunk 1.
        assert_eq!(chunks.concat(), text);
        assert!(chunks[0].ends_with('\n'));
        assert!(chunks[1].starts_with(' '));
    }

    #[test]
    fn whitespace_too_early_falls_back_to_hard_split() {
        // A space at the very start (before the halfway point) shouldn't be used
        // as a break — we hard-split at the limit instead to keep chunks full.
        let mut text = String::from("a b");
        text.push_str(&"c".repeat(3000));
        let chunks = split_message(&text, 2000);
        assert_within_limit(&chunks, 2000);
        assert_eq!(chunks[0].chars().count(), 2000);
        // No content dropped (hard split, no seam whitespace removed).
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn cjk_content_counts_chars_not_bytes() {
        // Each CJK char is 3 bytes in UTF-8. 2001 of them is ~6003 bytes but only
        // 2001 chars, so it must split into 2 char-bounded chunks, not 4 byte ones.
        let text = "中".repeat(2001);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_within_limit(&chunks, 2000);
        assert_eq!(chunks[0].chars().count(), 2000);
        assert_eq!(chunks[1].chars().count(), 1);
        assert_eq!(chunks.concat(), text);
        // Every chunk is valid UTF-8 (String guarantees it) — no char was split.
    }

    #[test]
    fn emoji_content_not_split_mid_char() {
        // Multi-byte emoji (4 bytes each). 2500 of them => splits, each chunk
        // valid and within the char limit; no scalar value is cut.
        let text = "\u{1F600}".repeat(2500); // grinning face
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_within_limit(&chunks, 2000);
        assert_eq!(chunks.concat(), text);
        // Each chunk must contain whole emoji only.
        for chunk in &chunks {
            assert!(chunk.chars().all(|c| c == '\u{1F600}'));
        }
    }

    #[test]
    fn mixed_multibyte_with_newlines_splits_cleanly() {
        // Build a long mixed-script message with periodic newlines.
        let line = "héllo 世界 \u{1F389} ";
        let text = line.repeat(500); // well over 2000 chars
        let chunks = split_message(&text, DISCORD_MESSAGE_LIMIT);
        assert!(chunks.len() >= 2);
        assert_within_limit(&chunks, DISCORD_MESSAGE_LIMIT);
        // Concatenation must reconstruct the original modulo dropped seam spaces.
        // We can't assert exact equality because whitespace seams may be dropped,
        // but no non-whitespace char may be lost: compare with whitespace stripped.
        let strip_ws = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(strip_ws(&chunks.concat()), strip_ws(&text));
    }

    #[test]
    fn limit_zero_returns_whole_text() {
        // Degenerate guard: must not loop forever.
        assert_eq!(split_message("abc", 0), vec!["abc".to_string()]);
        assert!(split_message("", 0).is_empty());
    }
}
