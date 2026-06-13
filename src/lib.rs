pub mod channels;
pub mod config;
pub mod core;
pub mod history;
pub mod memory;
pub mod migrate;
pub mod providers;
pub mod scheduler;
pub mod security;
pub mod shutdown;
pub mod tools;
pub mod types;

/// Test-only helpers shared across the crate's unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests that read or mutate process-global environment
    /// variables. `std::env::set_var` is `unsafe` in edition 2024 because
    /// `setenv` may reallocate the environment block while another thread is in
    /// `getenv` — a data race regardless of which variable each touches. Holding
    /// this lock for the whole env-touching section of every such test makes
    /// those accesses mutually exclusive within the test binary.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the global env-test lock, recovering from poisoning (a panicking
    /// test must not brick every later env test).
    pub(crate) fn env_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}
