mod lifecycle;

pub use lifecycle::MemoryLifecycle;
mod store;

use std::sync::{Arc, Mutex};

use crate::config::Settings;

pub use store::{Link, Memory, MemoryError, MemoryResult, Note, SqliteMemory};

// ---------------------------------------------------------------------------
// Inventory-based memory backend registration
// ---------------------------------------------------------------------------

/// Dependencies passed to memory backend factories at construction time.
pub struct MemoryDeps<'a> {
    pub settings: &'a Settings,
    pub db_conn: Arc<Mutex<rusqlite::Connection>>,
}

/// A self-registering memory backend factory.
///
/// Each memory backend module submits one of these via `inventory::submit!`.
/// At startup, `build_memory()` iterates them to construct the configured
/// backend.
pub struct MemoryRegistration {
    /// Backend name (e.g. "sqlite").
    pub name: &'static str,
    /// Build the memory backend from shared dependencies.
    pub build_fn: fn(&MemoryDeps) -> anyhow::Result<Arc<dyn Memory>>,
}

inventory::collect!(MemoryRegistration);

/// Build a memory backend from inventory-registered backends.
///
/// Looks up the backend named "sqlite" specifically rather than taking the
/// first registered backend.
pub fn build_memory(deps: MemoryDeps) -> anyhow::Result<Arc<dyn Memory>> {
    for reg in inventory::iter::<MemoryRegistration> {
        if reg.name == "sqlite" {
            return (reg.build_fn)(&deps);
        }
    }
    anyhow::bail!("no memory backend named 'sqlite' registered")
}
