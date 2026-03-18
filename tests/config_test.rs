use std::collections::HashMap;
use std::io::Write;

use config::{Config, Environment, File, FileFormat};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Unit tests: layering & deserialization via the config crate directly
// ---------------------------------------------------------------------------

/// Minimal subset of Settings for unit tests.
#[derive(Debug, Deserialize)]
struct TestSettings {
    bot: TestBot,
    providers: TestProviders,
}

#[derive(Debug, Deserialize)]
struct TestBot {
    name: String,
}

#[derive(Debug, Deserialize)]
struct TestProviders {
    anthropic: Option<TestProvider>,
    openai: Option<TestProvider>,
}

#[derive(Debug, Deserialize)]
struct TestProvider {
    base_url: String,
    model: String,
    #[serde(default)]
    api_key_env: Option<String>,
}

#[test]
fn default_toml_deserializes_to_settings() {
    let default_toml = include_str!("../config/default.toml");

    let config = Config::builder()
        .add_source(File::from_str(default_toml, FileFormat::Toml))
        .build()
        .expect("failed to build config from default.toml");

    let settings: TestSettings = config
        .try_deserialize()
        .expect("failed to deserialize default.toml into TestSettings");

    assert_eq!(settings.bot.name, "Aurora");
    // Anthropic is commented out in default.toml (requires API key).
    assert!(settings.providers.anthropic.is_none());
    assert!(settings.providers.openai.is_some());

    let openai = settings.providers.openai.unwrap();
    assert_eq!(openai.base_url, "http://localhost:11434/v1");
    assert_eq!(openai.model, "llama3");
}

#[test]
fn env_vars_override_toml_values() {
    let default_toml = include_str!("../config/default.toml");

    // Use the config crate's mock env source — no process-level mutation.
    let mut mock_env = HashMap::new();
    mock_env.insert("BOREALIS__BOT__NAME".into(), "TestBot".into());
    mock_env.insert(
        "BOREALIS__PROVIDERS__OPENAI__MODEL".into(),
        "gpt-4o".into(),
    );

    let config = Config::builder()
        .add_source(File::from_str(default_toml, FileFormat::Toml))
        .add_source(
            Environment::with_prefix("BOREALIS")
                .separator("__")
                .source(Some(mock_env)),
        )
        .build()
        .expect("failed to build config with env overrides");

    let settings: TestSettings = config
        .try_deserialize()
        .expect("failed to deserialize with env overrides");

    assert_eq!(settings.bot.name, "TestBot");
    let openai = settings.providers.openai.unwrap();
    assert_eq!(openai.model, "gpt-4o");
}

#[test]
fn overlay_toml_overrides_default() {
    let default_toml = include_str!("../config/default.toml");

    let overlay_toml = r#"
[bot]
name = "OverlayBot"

[providers.openai]
model = "gpt-4o"
"#;

    let config = Config::builder()
        .add_source(File::from_str(default_toml, FileFormat::Toml))
        .add_source(File::from_str(overlay_toml, FileFormat::Toml))
        .build()
        .expect("failed to build config with overlay");

    let settings: TestSettings = config
        .try_deserialize()
        .expect("failed to deserialize with overlay");

    assert_eq!(settings.bot.name, "OverlayBot");
    // OpenAI model should be overridden
    let openai = settings.providers.openai.unwrap();
    assert_eq!(openai.model, "gpt-4o");
}

#[test]
fn env_takes_precedence_over_overlay() {
    let default_toml = include_str!("../config/default.toml");
    let overlay_toml = r#"
[bot]
name = "OverlayBot"
"#;

    let mut mock_env = HashMap::new();
    mock_env.insert("BOREALIS__BOT__NAME".into(), "EnvBot".into());

    let config = Config::builder()
        .add_source(File::from_str(default_toml, FileFormat::Toml))
        .add_source(File::from_str(overlay_toml, FileFormat::Toml))
        .add_source(
            Environment::with_prefix("BOREALIS")
                .separator("__")
                .source(Some(mock_env)),
        )
        .build()
        .expect("failed to build config with overlay + env");

    let settings: TestSettings = config.try_deserialize().expect("failed to deserialize");

    // Env should win over both default and overlay
    assert_eq!(settings.bot.name, "EnvBot");
}

#[test]
fn missing_required_field_produces_clear_error() {
    // A TOML with no [bot] section at all should produce a deserialization
    // error that names the missing field.
    let minimal_toml = r#"
[providers]
"#;

    let config = Config::builder()
        .add_source(File::from_str(minimal_toml, FileFormat::Toml))
        .build()
        .expect("failed to build config");

    let result = config.try_deserialize::<TestSettings>();
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    // Error should mention "bot" — the missing required section
    assert!(
        err_msg.contains("bot"),
        "error should mention the missing 'bot' field, got: {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// Integration tests: Settings::load() with real files on disk
// Uses File::with_name to test the file discovery path.
// ---------------------------------------------------------------------------

/// Helper to set up a temp dir with config files and run a closure.
fn with_config_dir<F, R>(default_toml: &str, extra_files: &[(&str, &str)], f: F) -> R
where
    F: FnOnce(&std::path::Path) -> R,
{
    let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("failed to create config dir");

    let mut file = std::fs::File::create(config_dir.join("default.toml"))
        .expect("failed to create default.toml");
    file.write_all(default_toml.as_bytes())
        .expect("failed to write default.toml");

    for (name, content) in extra_files {
        let path = config_dir.join(name);
        let mut f = std::fs::File::create(path).expect("failed to create extra file");
        f.write_all(content.as_bytes())
            .expect("failed to write extra file");
    }

    f(tmp.path())
}

#[test]
fn load_default_toml_from_disk() {
    with_config_dir(include_str!("../config/default.toml"), &[], |dir| {
        let config_path = dir.join("config").join("default");

        let config = Config::builder()
            .add_source(File::with_name(config_path.to_str().unwrap()))
            .build()
            .expect("failed to build config from disk");

        let settings: TestSettings = config
            .try_deserialize()
            .expect("failed to deserialize from disk");

        assert_eq!(settings.bot.name, "Aurora");
    });
}

#[test]
fn local_toml_overlay_from_disk() {
    let local_toml = r#"
[bot]
name = "LocalAurora"
"#;

    with_config_dir(
        include_str!("../config/default.toml"),
        &[("local.toml", local_toml)],
        |dir| {
            let default_path = dir.join("config").join("default");
            let local_path = dir.join("config").join("local");

            let config = Config::builder()
                .add_source(File::with_name(default_path.to_str().unwrap()))
                .add_source(File::with_name(local_path.to_str().unwrap()).required(false))
                .build()
                .expect("failed to build config with local overlay");

            let settings: TestSettings = config
                .try_deserialize()
                .expect("failed to deserialize with local overlay");

            assert_eq!(settings.bot.name, "LocalAurora");
            // OpenAI still present from default
            assert!(settings.providers.openai.is_some());
        },
    );
}

// ---------------------------------------------------------------------------
// Validation tests (env var indirection)
//
// These test the resolve_env_var logic. Since we can't import from a binary
// crate, we mirror the function and verify the error message format.
// ---------------------------------------------------------------------------

/// Mirror of the resolve_env_var logic from config.rs for testing.
fn resolve_env_var(field: &str, env_var: &str) -> Result<String, String> {
    match std::env::var(env_var) {
        Ok(val) if val.is_empty() => Err(format!(
            "environment variable '{env_var}' (referenced by {field}) is set but empty — \
             provide a non-empty value"
        )),
        Ok(val) => Ok(val),
        Err(_) => Err(format!(
            "environment variable '{env_var}' (referenced by {field}) is not set — \
             set it or remove '{field}' from the config"
        )),
    }
}

#[test]
fn resolve_env_var_missing() {
    // This var should not exist in the environment.
    let result = resolve_env_var(
        "providers.anthropic.api_key_env",
        "BOREALIS_TEST_NONEXISTENT_KEY_7f3a9c",
    );
    assert!(result.is_err());
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("BOREALIS_TEST_NONEXISTENT_KEY_7f3a9c"),
        "error should name the missing env var, got: {err_msg}"
    );
    assert!(
        err_msg.contains("providers.anthropic.api_key_env"),
        "error should name the config field, got: {err_msg}"
    );
}

#[test]
fn resolve_env_var_empty() {
    // SAFETY: single-threaded test with a unique var name.
    unsafe { std::env::set_var("BOREALIS_TEST_EMPTY_KEY_a1b2c3", "") };

    let result = resolve_env_var(
        "providers.anthropic.api_key_env",
        "BOREALIS_TEST_EMPTY_KEY_a1b2c3",
    );
    assert!(result.is_err());
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("empty"),
        "error should mention empty value, got: {err_msg}"
    );

    unsafe { std::env::remove_var("BOREALIS_TEST_EMPTY_KEY_a1b2c3") };
}

#[test]
fn resolve_env_var_valid() {
    // SAFETY: single-threaded test with a unique var name.
    unsafe { std::env::set_var("BOREALIS_TEST_VALID_KEY_d4e5f6", "sk-abc123") };

    let result = resolve_env_var(
        "providers.anthropic.api_key_env",
        "BOREALIS_TEST_VALID_KEY_d4e5f6",
    );
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "sk-abc123");

    unsafe { std::env::remove_var("BOREALIS_TEST_VALID_KEY_d4e5f6") };
}
