use std::env;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(#[from] config::ConfigError),

    #[error(
        "environment variable '{env_var}' (referenced by {field}) is not set — \
         set it or remove '{field}' from the config"
    )]
    MissingEnvVar { field: String, env_var: String },

    #[error(
        "environment variable '{env_var}' (referenced by {field}) is set but empty — \
         provide a non-empty value"
    )]
    EmptyEnvVar { field: String, env_var: String },

    #[error("invalid configuration: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Top-level Settings
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub bot: BotConfig,
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
}

// ---------------------------------------------------------------------------
// Bot
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BotConfig {
    pub name: String,
    #[serde(default = "default_system_prompt_path")]
    pub system_prompt_path: PathBuf,
    #[serde(default = "default_core_persona_path")]
    pub core_persona_path: PathBuf,
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// Maximum number of concurrent LLM API calls (default: 4).
    #[serde(default = "default_max_concurrent_llm")]
    pub max_concurrent_llm: usize,
}

fn default_max_concurrent_llm() -> usize {
    4
}

fn default_system_prompt_path() -> PathBuf {
    PathBuf::from("config/system_prompt.md")
}

fn default_core_persona_path() -> PathBuf {
    PathBuf::from("memory/core.md")
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompactionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_compaction_threshold")]
    pub threshold: f64,
    #[serde(default = "default_compaction_model")]
    pub compaction_model: String,
    #[serde(default = "default_summary_prompt_path")]
    pub summary_prompt_path: PathBuf,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: default_compaction_threshold(),
            compaction_model: default_compaction_model(),
            summary_prompt_path: default_summary_prompt_path(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_compaction_threshold() -> f64 {
    0.75
}

fn default_compaction_model() -> String {
    "default".into()
}

fn default_summary_prompt_path() -> PathBuf {
    PathBuf::from("config/compaction_prompt.md")
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub anthropic: Option<ProviderEntry>,
    #[serde(default)]
    pub openai: Option<ProviderEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ProviderEntry {
    pub base_url: String,
    pub model: String,
    /// Name of the environment variable that holds the API key.
    /// The actual key is resolved at validation time.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_max_history_tokens")]
    pub max_history_tokens: usize,
}

fn default_timeout_secs() -> u64 {
    60
}

fn default_max_retries() -> u32 {
    3
}

fn default_max_history_tokens() -> usize {
    8192
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub cli: Option<CliChannelConfig>,
    #[serde(default)]
    pub discord: Option<DiscordChannelConfig>,
}

#[derive(Debug, Deserialize)]
pub struct CliChannelConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscordChannelConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Name of the environment variable that holds the Discord bot token.
    pub token_env: String,
    #[serde(default)]
    pub groups: Vec<DiscordGroupConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscordGroupConfig {
    pub guild_id: String,
    #[serde(default = "default_response_mode")]
    pub response_mode: String,
    #[serde(default)]
    pub digest_interval_min: Option<u64>,
    #[serde(default)]
    pub digest_debounce_min: Option<u64>,
}

fn default_response_mode() -> String {
    "mention-only".into()
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_database_path")]
    pub path: PathBuf,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: default_database_path(),
        }
    }
}

fn default_database_path() -> PathBuf {
    PathBuf::from("memory/borealis.db")
}

// ---------------------------------------------------------------------------
// Rate Limiting
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub per_user: TokenBucketConfig,
    #[serde(default)]
    pub global: GlobalTokenBucketConfig,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allowed_guilds: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenBucketConfig {
    #[serde(default = "default_user_bucket_capacity")]
    pub capacity: u32,
    #[serde(default = "default_user_refill_secs")]
    pub refill_secs: u64,
}

impl Default for TokenBucketConfig {
    fn default() -> Self {
        Self {
            capacity: default_user_bucket_capacity(),
            refill_secs: default_user_refill_secs(),
        }
    }
}

fn default_user_bucket_capacity() -> u32 {
    10
}

fn default_user_refill_secs() -> u64 {
    6
}

#[derive(Debug, Deserialize)]
pub struct GlobalTokenBucketConfig {
    #[serde(default = "default_global_bucket_capacity")]
    pub capacity: u32,
    #[serde(default = "default_global_refill_secs")]
    pub refill_secs: u64,
}

impl Default for GlobalTokenBucketConfig {
    fn default() -> Self {
        Self {
            capacity: default_global_bucket_capacity(),
            refill_secs: default_global_refill_secs(),
        }
    }
}

fn default_global_bucket_capacity() -> u32 {
    30
}

fn default_global_refill_secs() -> u64 {
    2
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct ToolsConfig {
    #[serde(default)]
    pub computer_use: ComputerUseConfig,
    #[serde(default)]
    pub web: WebToolsConfig,
}

#[derive(Debug, Deserialize)]
pub struct ComputerUseConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_sandbox_root")]
    pub sandbox_root: PathBuf,
    /// Empty list means all commands are allowed.
    #[serde(default)]
    pub command_allowlist: Vec<String>,
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u64,
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sandbox_root: default_sandbox_root(),
            command_allowlist: Vec::new(),
            command_timeout_secs: default_command_timeout_secs(),
        }
    }
}

fn default_sandbox_root() -> PathBuf {
    PathBuf::from(".")
}

fn default_command_timeout_secs() -> u64 {
    30
}

#[derive(Debug, Deserialize)]
pub struct WebToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Name of the environment variable holding the Jina API key (optional).
    /// Without a key, requests are rate-limited by IP.
    #[serde(default)]
    pub jina_api_key_env: Option<String>,
    /// Maximum response body size in bytes (default: 50 KiB).
    #[serde(default = "default_max_fetch_bytes")]
    pub max_fetch_bytes: usize,
}

impl Default for WebToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            jina_api_key_env: None,
            max_fetch_bytes: default_max_fetch_bytes(),
        }
    }
}

fn default_max_fetch_bytes() -> usize {
    51200
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub events: Vec<SchedulerEventConfig>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
            events: Vec::new(),
        }
    }
}

fn default_timezone() -> String {
    "UTC".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerEventConfig {
    pub name: String,
    /// "recurring" or "cron"
    #[serde(rename = "type")]
    pub event_type: String,
    /// For recurring events: interval like "30m", "1h", "90s"
    #[serde(default)]
    pub interval: Option<String>,
    /// For cron events: cron expression like "0 22 * * *"
    #[serde(default)]
    pub schedule: Option<String>,
    /// Jitter range like "5m", "30s"
    #[serde(default)]
    pub jitter: Option<String>,
    /// Active hours range like "06:00-23:00" (interpreted in configured timezone)
    #[serde(default)]
    pub active_hours: Option<String>,
    /// Prompt template with {time}, {timezone}, {interval} placeholders
    pub prompt: String,
    /// Optional list of tool groups available for this event.
    /// When omitted, all enabled tool groups are available.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Loading & Validation
// ---------------------------------------------------------------------------

impl Settings {
    /// Load settings from layered TOML config files + environment variables.
    ///
    /// Layer order (later overrides earlier):
    /// 1. `config/default.toml`          — base defaults
    /// 2. `config/{run_mode}.toml`       — environment-specific (optional)
    /// 3. `config/local.toml`            — local developer overrides (optional)
    /// 4. `BOREALIS__*` env vars         — runtime overrides
    pub fn load() -> Result<Self, ConfigError> {
        let run_mode = env::var("BOREALIS_RUN_MODE").unwrap_or_else(|_| "development".into());

        let config = config::Config::builder()
            .add_source(config::File::with_name("config/default"))
            .add_source(config::File::with_name(&format!("config/{run_mode}")).required(false))
            .add_source(config::File::with_name("config/local").required(false))
            .add_source(
                config::Environment::with_prefix("BOREALIS")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        let settings: Settings = config.try_deserialize()?;
        settings.validate()?;
        Ok(settings)
    }

    /// Validate resolved settings — checks that referenced env vars are set
    /// and contain non-empty values.
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(ref anthropic) = self.providers.anthropic
            && let Some(ref key_env) = anthropic.api_key_env
        {
            resolve_env_var("providers.anthropic.api_key_env", key_env)?;
        }
        if let Some(ref openai) = self.providers.openai
            && let Some(ref key_env) = openai.api_key_env
        {
            resolve_env_var("providers.openai.api_key_env", key_env)?;
        }
        if let Some(ref discord) = self.channels.discord
            && discord.enabled
        {
            resolve_env_var("channels.discord.token_env", &discord.token_env)?;
        }
        if self.tools.web.enabled {
            if let Some(ref key_env) = self.tools.web.jina_api_key_env {
                resolve_env_var("tools.web.jina_api_key_env", key_env)?;
            }
        }
        if self.bot.max_concurrent_llm == 0 {
            return Err(ConfigError::Validation(
                "bot.max_concurrent_llm must be > 0".into(),
            ));
        }
        if self.rate_limit.per_user.refill_secs == 0 {
            return Err(ConfigError::Validation(
                "rate_limit.per_user.refill_secs must be > 0".into(),
            ));
        }
        if self.rate_limit.global.refill_secs == 0 {
            return Err(ConfigError::Validation(
                "rate_limit.global.refill_secs must be > 0".into(),
            ));
        }
        if self.rate_limit.per_user.capacity == 0 {
            return Err(ConfigError::Validation(
                "rate_limit.per_user.capacity must be > 0".into(),
            ));
        }
        if self.rate_limit.global.capacity == 0 {
            return Err(ConfigError::Validation(
                "rate_limit.global.capacity must be > 0".into(),
            ));
        }
        Ok(())
    }
}

/// Resolve an environment variable by name, returning an error that names
/// both the config field and the missing env var.
fn resolve_env_var(field: &str, env_var: &str) -> Result<String, ConfigError> {
    match env::var(env_var) {
        Ok(val) if val.is_empty() => Err(ConfigError::EmptyEnvVar {
            field: field.to_owned(),
            env_var: env_var.to_owned(),
        }),
        Ok(val) => Ok(val),
        Err(_) => Err(ConfigError::MissingEnvVar {
            field: field.to_owned(),
            env_var: env_var.to_owned(),
        }),
    }
}

/// Convenience function for runtime lookup of an env var that was already
/// validated at startup. Panics if called before validation.
pub fn get_secret(env_var: &str) -> String {
    env::var(env_var).unwrap_or_else(|_| {
        panic!(
            "BUG: env var '{env_var}' should have been validated at startup — \
             this is a programming error"
        )
    })
}
