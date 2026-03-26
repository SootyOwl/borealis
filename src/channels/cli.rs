use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::channels::{Channel, ChannelRegistry};
use crate::config::Settings;
use crate::core::event::{
    Author, ChannelSource, ConversationId, InEvent, Message, MessageContext, MessageId, OutEvent,
};
use crate::core::pipeline::PipelineRunner;

/// Register the CLI adapter with the channel registry if enabled in config.
pub fn register(
    registry: &mut ChannelRegistry,
    settings: &Settings,
    pipeline: Arc<dyn PipelineRunner>,
    cancel: CancellationToken,
) {
    let enabled = settings.channels.cli.as_ref().is_some_and(|c| c.enabled);
    if !enabled {
        return;
    }

    let cli = Arc::new(CliAdapter::new(settings.bot.name.clone()));
    registry.register(cli, pipeline, cancel);
}

/// CLI adapter for development — reads lines from stdin, prints responses to stdout.
///
/// Tracing output goes to stderr, chat goes to stdout.
pub struct CliAdapter {
    /// The bot name, used for display and mention detection.
    bot_name: String,
}

impl CliAdapter {
    pub fn new(bot_name: String) -> Self {
        Self { bot_name }
    }

    fn make_in_event(&self, line: &str, seq: u64) -> InEvent {
        let mentions_bot = line.to_lowercase().contains(&self.bot_name.to_lowercase());

        InEvent {
            source: ChannelSource::Cli,
            message: Message {
                id: MessageId(format!("cli-{seq}")),
                author: Author {
                    id: "cli-user".into(),
                    display_name: "You".into(),
                },
                text: line.to_string(),
                timestamp: Utc::now(),
                mentions_bot,
            },
            context: MessageContext {
                conversation_id: ConversationId::Dm {
                    channel_type: ChannelSource::Cli,
                    user_id: "cli-user".into(),
                },
                channel_id: "cli".into(),
                reply_to: None,
            },
            tool_groups: None,
        }
    }
}

impl Channel for CliAdapter {
    fn name(&self) -> &str {
        "cli"
    }

    async fn run_inbound(self: Arc<Self>, tx: Sender<InEvent>) -> Result<()> {
        info!("CLI adapter inbound started — type messages below");

        let stdin = tokio::io::stdin();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();
        let mut seq: u64 = 0;

        loop {
            let line = match lines.next_line().await? {
                Some(line) => line,
                None => {
                    info!("stdin closed — CLI adapter shutting down");
                    break;
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if trimmed == "/quit" || trimmed == "/exit" {
                info!("CLI quit command received");
                break;
            }

            seq += 1;
            let event = self.make_in_event(trimmed, seq);
            debug!(seq, text = trimmed, "CLI inbound message");

            if tx.send(event).await.is_err() {
                info!("event bus closed — CLI inbound shutting down");
                break;
            }
        }

        Ok(())
    }

    async fn run_outbound(self: Arc<Self>, mut rx: Receiver<OutEvent>) -> Result<()> {
        info!("CLI adapter outbound started");

        let mut stdout = tokio::io::stdout();

        while let Some(event) = rx.recv().await {
            // Empty text = no reply (REQ-11 convention replacing NoReply directive)
            if let Some(text) = &event.text {
                if !text.is_empty() {
                    let output = format!("{}: {}\n", self.bot_name, text);
                    stdout.write_all(output.as_bytes()).await?;
                    stdout.flush().await?;
                }
            }
        }

        info!("CLI adapter outbound finished");
        Ok(())
    }
}
