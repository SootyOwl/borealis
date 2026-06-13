use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::sync::Semaphore;
use tracing::info;

use crate::config::{ProviderEntry, Settings};
use crate::core::observer::build_observer_registry;
use crate::core::pipeline::{PipelineDeps, PipelineRunner};
use crate::history::compaction::CompactionState;
use crate::history::store::HistoryStore;
use crate::memory::Memory;
use crate::providers::{ProviderConfig, ProviderRegistration};
use crate::security::Security;
use crate::tools::ToolRegistry;

/// A resolved provider entry with configuration ready for construction.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    /// Provider name (e.g., "anthropic", "openai").
    pub name: String,
    /// Resolved provider configuration (API key already fetched from env).
    pub config: ProviderConfig,
    /// Maximum tokens the model supports for history.
    pub max_history_tokens: usize,
    /// Sampling temperature for this provider (None = use the API default).
    pub temperature: Option<f32>,
}

/// Resolve a `ProviderEntry` from config into a `ResolvedProvider`.
///
/// The API key is resolved from the environment variable named in `api_key_env`.
/// If the env var is missing or empty, the key defaults to an empty string
/// (valid for local providers like Ollama that don't require authentication).
fn resolve_entry(name: &str, entry: &ProviderEntry) -> ResolvedProvider {
    let api_key = entry
        .api_key_env
        .as_ref()
        .and_then(|env| std::env::var(env).ok())
        .unwrap_or_default();

    ResolvedProvider {
        name: name.to_string(),
        config: ProviderConfig {
            api_key,
            base_url: entry.base_url.clone(),
            model: entry.model.clone(),
            timeout_secs: entry.timeout_secs,
            max_retries: entry.max_retries,
        },
        max_history_tokens: entry.max_history_tokens,
        temperature: entry.temperature,
    }
}

/// Return all configured providers in priority order.
///
/// Ordering, with duplicates removed so each provider appears once:
///   1. `bot.default_provider`, if set and present in the configured entries.
///   2. `anthropic`, if configured.
///   3. `openai`, if configured.
///   4. Any remaining configured providers, sorted by name.
///
/// With `default_provider` unset and only `anthropic`/`openai` configured this
/// reproduces the historical behavior exactly (anthropic first, then openai).
///
/// Only providers present in the config are included. This does not validate
/// that the provider can actually be constructed (e.g., API key validity); that
/// happens at construction time.
pub fn resolve_configured_providers(settings: &Settings) -> Vec<ResolvedProvider> {
    let entries = &settings.providers.entries;
    let mut order: Vec<&str> = Vec::new();

    if let Some(ref name) = settings.bot.default_provider
        && entries.contains_key(name)
    {
        order.push(name.as_str());
    }
    for preferred in ["anthropic", "openai"] {
        if entries.contains_key(preferred) && !order.contains(&preferred) {
            order.push(preferred);
        }
    }
    let mut remaining: Vec<&str> = entries
        .keys()
        .map(String::as_str)
        .filter(|k| !order.contains(k))
        .collect();
    remaining.sort_unstable();
    order.extend(remaining);

    order
        .into_iter()
        .map(|name| resolve_entry(name, &entries[name]))
        .collect()
}

/// Resolve a specific provider by name from settings.
///
/// Returns `None` if the named provider is not configured.
pub fn resolve_named_provider(name: &str, settings: &Settings) -> Option<ResolvedProvider> {
    settings
        .providers
        .entries
        .get(name)
        .map(|e| resolve_entry(name, e))
}

/// Construct a concrete provider and wrap it in a `Pipeline`, returning the
/// object-safe `PipelineRunner`.
///
/// Looks up the provider by name in the inventory of `ProviderRegistration`
/// entries. Each provider module self-registers via `inventory::submit!`, so
/// adding a new provider requires no changes here.
fn build_pipeline_for_provider(
    resolved: &ResolvedProvider,
    sys_path: &Path,
    persona_path: &Path,
    deps: PipelineDeps,
) -> Result<Arc<dyn PipelineRunner>> {
    for reg in inventory::iter::<ProviderRegistration> {
        if reg.name == resolved.name {
            info!(provider = reg.name, model = %resolved.config.model, "building provider pipeline");
            return (reg.build_pipeline_fn)(
                resolved.config.clone(),
                sys_path,
                persona_path,
                deps,
            );
        }
    }
    bail!("unknown provider type: {}", resolved.name);
}

/// Build a `PipelineRunner` from the primary configured provider.
///
/// The primary is chosen by [`resolve_configured_providers`]: `bot.default_provider`
/// if set, otherwise anthropic, then openai, then any other configured provider.
/// Returns an error if no providers are configured.
pub fn build_pipeline(
    settings: &Settings,
    history_store: Arc<HistoryStore>,
    tool_registry: Arc<ToolRegistry>,
    memory_store: Arc<dyn Memory>,
    security: Arc<Security>,
) -> Result<Arc<dyn PipelineRunner>> {
    let providers = resolve_configured_providers(settings);

    if providers.is_empty() {
        bail!(
            "no LLM provider configured — add [providers.openai] or [providers.anthropic] to config"
        );
    }

    let sys_path = &settings.bot.system_prompt_path;
    let persona_path = &settings.bot.core_persona_path;
    let compaction_config = settings.bot.compaction.clone();
    let compaction_state = Arc::new(CompactionState::new());

    // Build observer registry from inventory-registered observers.
    let observers = Arc::new(build_observer_registry());

    // Use the first configured provider.
    let resolved = &providers[0];

    let pipeline_config = crate::core::pipeline::PipelineConfig {
        model_max_tokens: resolved.max_history_tokens,
        response_reserve: 1024,
        temperature: resolved.temperature,
        max_response_tokens: Some(settings.bot.max_response_tokens),
    };

    let llm_semaphore = Arc::new(Semaphore::new(settings.bot.max_concurrent_llm));

    let deps = PipelineDeps {
        history_store,
        tool_registry,
        memory_store,
        security,
        observers,
        compaction_config,
        compaction_state,
        pipeline_config,
        llm_semaphore,
    };

    build_pipeline_for_provider(resolved, sys_path, persona_path, deps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_entry() -> ProviderEntry {
        ProviderEntry {
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-4-20250514".into(),
            api_key_env: None,
            timeout_secs: 60,
            max_retries: 3,
            max_history_tokens: 8192,
            temperature: Some(0.7),
        }
    }

    fn openai_entry() -> ProviderEntry {
        ProviderEntry {
            base_url: "http://localhost:11434/v1".into(),
            model: "llama3".into(),
            api_key_env: None,
            timeout_secs: 60,
            max_retries: 3,
            max_history_tokens: 4096,
            temperature: Some(0.3),
        }
    }

    fn make_bot_config() -> crate::config::BotConfig {
        use crate::config::*;
        BotConfig {
            name: "Test".into(),
            default_provider: None,
            system_prompt_path: "config/system_prompt.md".into(),
            core_persona_path: "memory/core.md".into(),
            compaction: CompactionConfig::default(),
            max_concurrent_llm: 4,
            max_response_tokens: 1024,
        }
    }

    fn make_settings_with(entries: Vec<(&str, ProviderEntry)>) -> Settings {
        use crate::config::*;
        Settings {
            bot: make_bot_config(),
            providers: ProvidersConfig {
                entries: entries
                    .into_iter()
                    .map(|(name, entry)| (name.to_string(), entry))
                    .collect(),
            },
            channels: ChannelsConfig::default(),
            database: DatabaseConfig::default(),
            rate_limit: RateLimitConfig::default(),
            scheduler: SchedulerConfig::default(),
            tools: ToolsConfig::default(),
        }
    }

    fn make_settings_both() -> Settings {
        make_settings_with(vec![
            ("anthropic", anthropic_entry()),
            ("openai", openai_entry()),
        ])
    }

    fn make_settings_openai_only() -> Settings {
        make_settings_with(vec![("openai", openai_entry())])
    }

    fn make_settings_none() -> Settings {
        make_settings_with(vec![])
    }

    #[test]
    fn resolve_configured_providers_priority_order() {
        let settings = make_settings_both();
        let providers = resolve_configured_providers(&settings);
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].name, "anthropic");
        assert_eq!(providers[1].name, "openai");
    }

    #[test]
    fn default_provider_takes_precedence_over_anthropic() {
        // PROV-8: bot.default_provider = "gemini" puts gemini first even when
        // anthropic and openai are also configured.
        let mut settings = make_settings_with(vec![
            ("anthropic", anthropic_entry()),
            ("openai", openai_entry()),
            (
                "gemini",
                ProviderEntry {
                    base_url: "https://generativelanguage.googleapis.com".into(),
                    model: "gemini-1.5-pro".into(),
                    api_key_env: None,
                    timeout_secs: 60,
                    max_retries: 3,
                    max_history_tokens: 8192,
                    temperature: Some(0.5),
                },
            ),
        ]);
        settings.bot.default_provider = Some("gemini".into());

        let providers = resolve_configured_providers(&settings);
        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].name, "gemini");
        // The remaining two keep the historical anthropic-then-openai order.
        assert_eq!(providers[1].name, "anthropic");
        assert_eq!(providers[2].name, "openai");
    }

    #[test]
    fn default_provider_unset_keeps_anthropic_first() {
        // With default_provider unset, a custom provider sorts after the
        // hardcoded anthropic/openai preference.
        let mut settings = make_settings_with(vec![
            ("anthropic", anthropic_entry()),
            ("openai", openai_entry()),
            (
                "gemini",
                ProviderEntry {
                    base_url: "https://generativelanguage.googleapis.com".into(),
                    model: "gemini-1.5-pro".into(),
                    api_key_env: None,
                    timeout_secs: 60,
                    max_retries: 3,
                    max_history_tokens: 8192,
                    temperature: Some(0.5),
                },
            ),
        ]);
        settings.bot.default_provider = None;

        let providers = resolve_configured_providers(&settings);
        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].name, "anthropic");
        assert_eq!(providers[1].name, "openai");
        assert_eq!(providers[2].name, "gemini");
    }

    #[test]
    fn resolve_configured_providers_single() {
        let settings = make_settings_openai_only();
        let providers = resolve_configured_providers(&settings);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "openai");
        assert_eq!(providers[0].config.model, "llama3");
        assert_eq!(providers[0].max_history_tokens, 4096);
    }

    #[test]
    fn resolve_configured_providers_empty() {
        let settings = make_settings_none();
        let providers = resolve_configured_providers(&settings);
        assert!(providers.is_empty());
    }

    #[test]
    fn resolve_named_provider_found() {
        let settings = make_settings_both();
        let resolved = resolve_named_provider("openai", &settings);
        assert!(resolved.is_some());
        let r = resolved.unwrap();
        assert_eq!(r.name, "openai");
        assert_eq!(r.config.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn resolve_named_provider_not_found() {
        let settings = make_settings_both();
        assert!(resolve_named_provider("gemini", &settings).is_none());
    }

    #[test]
    fn resolve_named_custom_provider_found() {
        // PROV-8: a custom provider name is resolvable once it is in the config
        // map — this was impossible with the old hardcoded match arms.
        let settings = make_settings_with(vec![(
            "gemini",
            ProviderEntry {
                base_url: "https://generativelanguage.googleapis.com".into(),
                model: "gemini-1.5-pro".into(),
                api_key_env: None,
                timeout_secs: 60,
                max_retries: 3,
                max_history_tokens: 8192,
                temperature: Some(0.5),
            },
        )]);
        let resolved = resolve_named_provider("gemini", &settings);
        assert!(resolved.is_some(), "custom provider should resolve");
        let r = resolved.unwrap();
        assert_eq!(r.name, "gemini");
        assert_eq!(r.config.model, "gemini-1.5-pro");
        assert_eq!(r.config.base_url, "https://generativelanguage.googleapis.com");
    }

    #[test]
    fn resolve_named_provider_not_configured() {
        let settings = make_settings_openai_only();
        assert!(resolve_named_provider("anthropic", &settings).is_none());
    }

    #[test]
    fn resolve_entry_defaults_empty_api_key() {
        let entry = ProviderEntry {
            base_url: "http://localhost".into(),
            model: "test".into(),
            api_key_env: Some("NONEXISTENT_KEY_FOR_TEST_12345".into()),
            timeout_secs: 30,
            max_retries: 2,
            max_history_tokens: 2048,
            temperature: Some(0.7),
        };
        let resolved = resolve_entry("test", &entry);
        assert_eq!(resolved.config.api_key, "");
    }

    #[test]
    fn resolve_entry_carries_temperature() {
        let entry = ProviderEntry {
            base_url: "http://localhost".into(),
            model: "test".into(),
            api_key_env: None,
            timeout_secs: 30,
            max_retries: 2,
            max_history_tokens: 2048,
            temperature: Some(0.2),
        };
        let resolved = resolve_entry("test", &entry);
        assert_eq!(resolved.temperature, Some(0.2));

        let entry_none = ProviderEntry {
            temperature: None,
            ..entry
        };
        let resolved_none = resolve_entry("test", &entry_none);
        assert_eq!(resolved_none.temperature, None);
    }

    #[test]
    fn resolved_providers_carry_per_provider_temperature() {
        let settings = make_settings_both();
        let providers = resolve_configured_providers(&settings);
        assert_eq!(providers[0].temperature, Some(0.7));
        assert_eq!(providers[1].temperature, Some(0.3));
    }
}
