mod store;

use std::sync::Arc;

use crate::config::Settings;

pub use store::{Link, Memory, MemoryError, MemoryResult, Note, SqliteMemory};

// ---------------------------------------------------------------------------
// Inventory-based memory backend registration
// ---------------------------------------------------------------------------

/// A self-registering memory backend factory.
///
/// Each memory backend module submits one of these via `inventory::submit!`.
/// At startup, `build_memory()` iterates them to construct the configured
/// backend.
pub struct MemoryRegistration {
    /// Backend name (e.g. "sqlite").
    pub name: &'static str,
    /// Build the memory backend from application settings.
    pub build_fn: fn(&Settings) -> anyhow::Result<Arc<dyn Memory>>,
}

inventory::collect!(MemoryRegistration);

/// Build a memory backend from inventory-registered backends.
///
/// Currently there is only one backend ("sqlite"), so this simply uses the
/// first registered backend. When a config selector is added later, this
/// function will look up the configured backend by name.
pub fn build_memory(settings: &Settings) -> anyhow::Result<Arc<dyn Memory>> {
    for reg in inventory::iter::<MemoryRegistration> {
        tracing::debug!(memory_backend = reg.name, "registering memory backend");
        return (reg.build_fn)(settings);
    }
    anyhow::bail!("no memory backend registered via inventory")
}
