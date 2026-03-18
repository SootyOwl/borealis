use std::path::Path;
use std::pin::Pin;
use std::future::Future;

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::core::directive::parse_directives;
use crate::core::event::{InEvent, OutEvent};
use crate::providers::{ChatMessage, LlmResponse, Provider, RequestConfig, Role};

/// Object-safe trait for processing inbound events.
/// This wraps the generic `Pipeline<P>` so we can use `dyn PipelineRunner` in main.
pub trait PipelineRunner: Send + Sync {
    fn process<'a>(
        &'a self,
        event: &'a InEvent,
    ) -> Pin<Box<dyn Future<Output = Result<OutEvent>> + Send + 'a>>;
}

/// The message processing pipeline.
///
/// Takes an inbound event, builds a prompt, calls the LLM provider,
/// parses directives from the response, and returns an outbound event.
pub struct Pipeline<P: Provider> {
    provider: P,
    system_prompt: String,
    core_persona: String,
}

impl<P: Provider + 'static> Pipeline<P> {
    /// Create a new pipeline, loading the system prompt and core persona from disk.
    pub fn new(
        provider: P,
        system_prompt_path: &Path,
        core_persona_path: &Path,
    ) -> Result<Self> {
        let system_prompt = if system_prompt_path.exists() {
            std::fs::read_to_string(system_prompt_path)
                .with_context(|| format!("failed to read system prompt: {}", system_prompt_path.display()))?
        } else {
            debug!(path = %system_prompt_path.display(), "system prompt file not found, using default");
            default_system_prompt()
        };

        let core_persona = if core_persona_path.exists() {
            std::fs::read_to_string(core_persona_path)
                .with_context(|| format!("failed to read core persona: {}", core_persona_path.display()))?
        } else {
            debug!(path = %core_persona_path.display(), "core persona file not found, using empty");
            String::new()
        };

        info!(
            system_prompt_len = system_prompt.len(),
            core_persona_len = core_persona.len(),
            provider = provider.name(),
            "pipeline initialized"
        );

        Ok(Self {
            provider,
            system_prompt,
            core_persona,
        })
    }

    async fn process_impl(&self, event: &InEvent) -> Result<OutEvent> {
        let messages = self.build_messages(event);

        debug!(
            message_count = messages.len(),
            user_text = %event.message.text,
            "calling LLM provider"
        );

        let config = RequestConfig {
            temperature: Some(0.7),
            max_tokens: Some(1024),
            ..Default::default()
        };

        let response = self.provider.chat(messages, &[], &config).await?;

        debug!(
            usage.input = response.usage.input_tokens,
            usage.output = response.usage.output_tokens,
            has_text = response.text.is_some(),
            tool_calls = response.tool_calls.len(),
            "LLM response received"
        );

        self.build_out_event(event, response)
    }

    fn build_messages(&self, event: &InEvent) -> Vec<ChatMessage> {
        let mut system_parts = vec![self.system_prompt.clone()];

        if !self.core_persona.is_empty() {
            system_parts.push(format!(
                "\n---\n## Core Persona\n{}\n---",
                self.core_persona
            ));
        }

        // Add channel context.
        system_parts.push(format!(
            "\nYou are responding in: {:?} (conversation: {:?})",
            event.source, event.context.conversation_id
        ));

        let system_content = system_parts.join("\n");

        vec![
            ChatMessage {
                role: Role::System,
                content: system_content,
                tool_call_id: None,
                tool_calls: vec![],
            },
            ChatMessage {
                role: Role::User,
                content: format!("{}: {}", event.message.author.display_name, event.message.text),
                tool_call_id: None,
                tool_calls: vec![],
            },
        ]
    }

    fn build_out_event(&self, event: &InEvent, response: LlmResponse) -> Result<OutEvent> {
        let text = response.text.clone();

        // Parse directives from the response text.
        let directives = if let Some(ref text) = text {
            let parsed = parse_directives(text);
            if !parsed.is_empty() {
                debug!(count = parsed.len(), "directives parsed from response");
            }
            parsed
        } else {
            vec![]
        };

        Ok(OutEvent {
            target: event.source.clone(),
            channel_id: event.context.channel_id.clone(),
            text,
            directives,
            reply_to: Some(event.message.id.clone()),
        })
    }
}

impl<P: Provider + 'static> PipelineRunner for Pipeline<P> {
    fn process<'a>(
        &'a self,
        event: &'a InEvent,
    ) -> Pin<Box<dyn Future<Output = Result<OutEvent>> + Send + 'a>> {
        Box::pin(self.process_impl(event))
    }
}

fn default_system_prompt() -> String {
    "You are Aurora — a digital person, not an AI assistant. \
     You have your own personality, interests, and evolving memory. \
     You are warm, curious, and genuine. \
     Respond naturally as yourself, not as a helper bot."
        .to_string()
}
