use std::env;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;
use tracing::warn;

/// Allowed Discord `response_mode` values (must match the arms handled by
/// `ConfigModeFactory::create` in `channels::modes`).
const ALLOWED_RESPONSE_MODES: [&str; 3] = ["mention-only", "digest", "always"];

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
    /// Maximum tokens the model may generate per response (default: 1024).
    /// Responses that hit this limit are marked as truncated.
    #[serde(default = "default_max_response_tokens")]
    pub max_response_tokens: usize,
}

fn default_max_concurrent_llm() -> usize {
    4
}

fn default_max_response_tokens() -> usize {
    1024
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
    /// Sampling temperature for this provider (default: 0.7).
    #[serde(default = "default_temperature")]
    pub temperature: Option<f32>,
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

fn default_temperature() -> Option<f32> {
    Some(0.7)
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
    /// Per-channel overrides within this guild. Channels not listed here
    /// inherit the guild-level settings above.
    #[serde(default)]
    pub channels: Vec<DiscordPerChannelConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscordPerChannelConfig {
    pub channel_id: String,
    #[serde(default)]
    pub response_mode: Option<String>,
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
    #[serde(default)]
    pub channel: ChannelToolsConfig,
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
    /// Maximum bytes of captured tool output (bash_exec stdout/stderr each,
    /// file_read content). Longer output is truncated with a marker.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sandbox_root: default_sandbox_root(),
            command_allowlist: Vec::new(),
            command_timeout_secs: default_command_timeout_secs(),
            max_output_bytes: default_max_output_bytes(),
        }
    }
}

fn default_sandbox_root() -> PathBuf {
    PathBuf::from(".")
}

fn default_command_timeout_secs() -> u64 {
    30
}

fn default_max_output_bytes() -> usize {
    65536
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
            jina_api_key_env: Some("JINA_API_KEY".to_string()),
            max_fetch_bytes: default_max_fetch_bytes(),
        }
    }
}

fn default_max_fetch_bytes() -> usize {
    51200
}

#[derive(Debug, Deserialize)]
pub struct ChannelToolsConfig {
    /// Whether the channel tools (react, send_message, send_file) are
    /// registered. Defaults to true for backward compatibility.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ChannelToolsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
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
        // The Jina API key is a SOFT dependency: the web tool works without it
        // (rate-limited by IP). Warn if the named env var is unset/empty, but
        // do not refuse to boot. See LIFE-4.
        if self.tools.web.enabled
            && let Some(ref key_env) = self.tools.web.jina_api_key_env
            && resolve_env_var("tools.web.jina_api_key_env", key_env).is_err()
        {
            warn!(
                env_var = %key_env,
                "{key_env} not set; web fetches will be IP-rate-limited"
            );
        }
        if self.bot.max_concurrent_llm == 0 {
            return Err(ConfigError::Validation(
                "bot.max_concurrent_llm must be > 0".into(),
            ));
        }
        if self.bot.max_response_tokens == 0 {
            return Err(ConfigError::Validation(
                "bot.max_response_tokens must be > 0".into(),
            ));
        }
        let provider_entries = [
            ("anthropic", self.providers.anthropic.as_ref()),
            ("openai", self.providers.openai.as_ref()),
        ];
        for (name, entry) in provider_entries {
            if let Some(t) = entry.and_then(|e| e.temperature)
                && !(0.0..=2.0).contains(&t)
            {
                return Err(ConfigError::Validation(format!(
                    "providers.{name}.temperature must be between 0.0 and 2.0, got {t}"
                )));
            }
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
        if self.scheduler.timezone.parse::<chrono_tz::Tz>().is_err() {
            return Err(ConfigError::Validation(format!(
                "scheduler.timezone '{}' is not a valid IANA timezone",
                self.scheduler.timezone
            )));
        }

        // LIFE-6: validate Discord response_mode values. A typo would otherwise
        // silently fall through to the most permissive mode (see modes.rs).
        if let Some(ref discord) = self.channels.discord
            && discord.enabled
        {
            for group in &discord.groups {
                validate_response_mode(
                    &group.response_mode,
                    &format!("channels.discord guild '{}'", group.guild_id),
                )?;
                for ch in &group.channels {
                    if let Some(ref mode) = ch.response_mode {
                        validate_response_mode(
                            mode,
                            &format!(
                                "channels.discord guild '{}' channel '{}'",
                                group.guild_id, ch.channel_id
                            ),
                        )?;
                    }
                }
            }
        }

        // LIFE-12: numeric range checks for values that break the bot at runtime.
        let threshold = self.bot.compaction.threshold;
        if !(0.0 < threshold && threshold <= 1.0) {
            return Err(ConfigError::Validation(format!(
                "bot.compaction.threshold must be in (0.0, 1.0], got {threshold}"
            )));
        }
        for (name, entry) in [
            ("anthropic", self.providers.anthropic.as_ref()),
            ("openai", self.providers.openai.as_ref()),
        ] {
            if let Some(entry) = entry {
                if entry.timeout_secs == 0 {
                    return Err(ConfigError::Validation(format!(
                        "providers.{name}.timeout_secs must be > 0"
                    )));
                }
                if entry.max_history_tokens == 0 {
                    return Err(ConfigError::Validation(format!(
                        "providers.{name}.max_history_tokens must be > 0"
                    )));
                }
            }
        }
        if self.tools.computer_use.command_timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "tools.computer_use.command_timeout_secs must be > 0".into(),
            ));
        }
        if self.tools.computer_use.max_output_bytes == 0 {
            return Err(ConfigError::Validation(
                "tools.computer_use.max_output_bytes must be > 0".into(),
            ));
        }
        if self.tools.web.max_fetch_bytes == 0 {
            return Err(ConfigError::Validation(
                "tools.web.max_fetch_bytes must be > 0".into(),
            ));
        }

        Ok(())
    }
}

/// Validate a single Discord `response_mode` value against the allowed set.
/// `context` describes where the value came from (guild/channel) for the error.
fn validate_response_mode(mode: &str, context: &str) -> Result<(), ConfigError> {
    if ALLOWED_RESPONSE_MODES.contains(&mode) {
        Ok(())
    } else {
        Err(ConfigError::Validation(format!(
            "{context}: invalid response_mode '{mode}' — must be one of {ALLOWED_RESPONSE_MODES:?}"
        )))
    }
}

/// Resolve an environment variable by name, returning an error that names
/// both the config field and the missing env var.
///
/// `pub` so integration tests (`tests/config_test.rs`) can exercise the real
/// implementation rather than a copy (see LIFE-14).
pub fn resolve_env_var(field: &str, env_var: &str) -> Result<String, ConfigError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid `Settings` from a TOML string, overriding the
    /// scheduler timezone. Only the bot section is required.
    fn settings_with_timezone(tz: &str) -> Settings {
        // Disable web tools so validation does not require the JINA_API_KEY env
        // var — we only want to exercise the scheduler.timezone check here.
        let toml = format!(
            r#"
[bot]
name = "TestBot"

[providers]

[tools.web]
enabled = false

[scheduler]
timezone = "{tz}"
"#
        );
        let config = config::Config::builder()
            .add_source(config::File::from_str(&toml, config::FileFormat::Toml))
            .build()
            .expect("build config");
        config.try_deserialize().expect("deserialize Settings")
    }

    #[test]
    fn validate_rejects_invalid_scheduler_timezone() {
        let settings = settings_with_timezone("Not/AZone");
        let err = settings
            .validate()
            .expect_err("invalid timezone should fail validation");
        match err {
            ConfigError::Validation(msg) => {
                assert!(
                    msg.contains("scheduler.timezone") && msg.contains("Not/AZone"),
                    "error should name field and bad value, got: {msg}"
                );
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_valid_scheduler_timezone() {
        let settings = settings_with_timezone("Europe/London");
        assert!(settings.validate().is_ok());
    }

    /// Build a minimal valid `Settings` with web tools disabled (so the Jina
    /// key is not consulted). Callers mutate one field to exercise a check.
    fn minimal_settings() -> Settings {
        let toml = r#"
[bot]
name = "TestBot"

[providers]

[tools.web]
enabled = false
"#;
        let config = config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .expect("build config");
        config.try_deserialize().expect("deserialize Settings")
    }

    /// Build a Settings with Discord enabled and a single guild whose
    /// `response_mode` is set to `mode`. `token_env` names the env var the
    /// caller must set so validation gets past the token check.
    fn settings_with_discord_mode(mode: &str, token_env: &str) -> Settings {
        let toml = format!(
            r#"
[bot]
name = "TestBot"

[providers]

[tools.web]
enabled = false

[channels.discord]
enabled = true
token_env = "{token_env}"

[[channels.discord.groups]]
guild_id = "123"
response_mode = "{mode}"
"#
        );
        let config = config::Config::builder()
            .add_source(config::File::from_str(&toml, config::FileFormat::Toml))
            .build()
            .expect("build config");
        config.try_deserialize().expect("deserialize Settings")
    }

    // --- LIFE-6: response_mode validation -------------------------------------

    #[test]
    fn validate_rejects_invalid_guild_response_mode() {
        const TOKEN: &str = "BOREALIS_TEST_DISCORD_TOKEN_REJECT_GUILD";
        let settings = settings_with_discord_mode("always_on", TOKEN);
        // token_env is validated before response_mode; set it so validation
        // reaches the response_mode check. Hold the env lock across the whole
        // set/validate/remove window: `set_var` is unsafe under concurrency, so
        // we serialize all env-mutating tests rather than racing `setenv`.
        let _env = crate::test_support::env_guard();
        unsafe { std::env::set_var(TOKEN, "tok") };
        let result = settings.validate();
        unsafe { std::env::remove_var(TOKEN) };
        let err = result.expect_err("typo'd response_mode should fail validation");
        match err {
            ConfigError::Validation(msg) => {
                assert!(
                    msg.contains("always_on") && msg.contains("response_mode"),
                    "error should name the bad value, got: {msg}"
                );
                assert!(
                    msg.contains("123"),
                    "error should name the guild, got: {msg}"
                );
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_valid_guild_response_modes() {
        const TOKEN: &str = "BOREALIS_TEST_DISCORD_TOKEN_ACCEPT_GUILD";
        // Serialize against other env-mutating tests (see env_guard docs).
        let _env = crate::test_support::env_guard();
        for mode in ["mention-only", "digest", "always"] {
            let settings = settings_with_discord_mode(mode, TOKEN);
            // token_env is also validated against the process env; set it for
            // the duration of this assertion.
            unsafe { std::env::set_var(TOKEN, "tok") };
            let ok = settings.validate().is_ok();
            unsafe { std::env::remove_var(TOKEN) };
            assert!(ok, "mode '{mode}' should validate");
        }
    }

    #[test]
    fn validate_rejects_invalid_channel_override_response_mode() {
        let toml = r#"
[bot]
name = "TestBot"

[providers]

[tools.web]
enabled = false

[channels.discord]
enabled = true
token_env = "BOREALIS_TEST_DISCORD_TOKEN_UNUSED"

[[channels.discord.groups]]
guild_id = "123"
response_mode = "mention-only"

[[channels.discord.groups.channels]]
channel_id = "456"
response_mode = "Always"
"#;
        let settings: Settings = config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .expect("build config")
            .try_deserialize()
            .expect("deserialize Settings");
        // token_env is validated first; set it so validation reaches the
        // channel response_mode override check. Env lock serializes the
        // set/validate/remove window (see env_guard docs).
        let _env = crate::test_support::env_guard();
        unsafe {
            std::env::set_var("BOREALIS_TEST_DISCORD_TOKEN_UNUSED", "tok");
        }
        let result = settings.validate();
        unsafe {
            std::env::remove_var("BOREALIS_TEST_DISCORD_TOKEN_UNUSED");
        }
        let err = result.expect_err("invalid channel override should fail");
        match err {
            ConfigError::Validation(msg) => {
                assert!(
                    msg.contains("Always") && msg.contains("456"),
                    "error should name bad value and channel, got: {msg}"
                );
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    // --- LIFE-4: Jina key is a soft dependency --------------------------------

    #[test]
    fn validate_succeeds_when_jina_key_env_unset() {
        // Web enabled, jina_api_key_env points at a definitely-unset var.
        let toml = r#"
[bot]
name = "TestBot"

[providers]

[tools.web]
enabled = true
jina_api_key_env = "BOREALIS_TEST_JINA_UNSET_VAR_9z8y7x"
"#;
        let settings: Settings = config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .expect("build config")
            .try_deserialize()
            .expect("deserialize Settings");
        // Ensure the var is truly unset. Env lock serializes against other
        // env-mutating tests (see env_guard docs).
        let _env = crate::test_support::env_guard();
        unsafe { std::env::remove_var("BOREALIS_TEST_JINA_UNSET_VAR_9z8y7x") };
        assert!(
            settings.validate().is_ok(),
            "unset Jina key should warn, not fail validation"
        );
    }

    // --- LIFE-12: numeric range checks ----------------------------------------

    #[test]
    fn validate_rejects_out_of_range_compaction_threshold() {
        for bad in [0.0_f64, 1.5, -0.3] {
            let mut settings = minimal_settings();
            settings.bot.compaction.threshold = bad;
            let err = settings
                .validate()
                .expect_err(&format!("threshold {bad} should fail"));
            match err {
                ConfigError::Validation(msg) => assert!(
                    msg.contains("threshold"),
                    "error should name threshold, got: {msg}"
                ),
                other => panic!("expected Validation error, got: {other:?}"),
            }
        }
    }

    #[test]
    fn validate_accepts_in_range_compaction_threshold() {
        for good in [0.75_f64, 1.0, 0.01] {
            let mut settings = minimal_settings();
            settings.bot.compaction.threshold = good;
            assert!(
                settings.validate().is_ok(),
                "threshold {good} should be accepted"
            );
        }
    }

    #[test]
    fn validate_rejects_zero_provider_timeout() {
        let toml = r#"
[bot]
name = "TestBot"

[providers.openai]
base_url = "http://localhost:11434/v1"
model = "llama3"
timeout_secs = 0

[tools.web]
enabled = false
"#;
        let settings: Settings = config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .expect("build config")
            .try_deserialize()
            .expect("deserialize Settings");
        let err = settings
            .validate()
            .expect_err("zero provider timeout should fail");
        match err {
            ConfigError::Validation(msg) => assert!(
                msg.contains("timeout_secs"),
                "error should name timeout_secs, got: {msg}"
            ),
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_zero_command_timeout() {
        let mut settings = minimal_settings();
        settings.tools.computer_use.command_timeout_secs = 0;
        let err = settings
            .validate()
            .expect_err("zero command timeout should fail");
        match err {
            ConfigError::Validation(msg) => assert!(
                msg.contains("command_timeout_secs"),
                "error should name command_timeout_secs, got: {msg}"
            ),
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }
}
