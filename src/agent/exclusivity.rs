//! Exclusive tool-access registry (SDD v1.5 Step 2a / 2.5).
//!
//! Many tools are safe to run concurrently because they are stateless or
//! read-only (`Shared`). A few guard long-lived stateful resources (a single
//! CDP browser session, one WinRM connection) and must never race another tool
//! touching the same resource. Those tools opt into `Exclusive`, and the agent
//! holds a per-name tokio lock across the whole tool execution.

use std::collections::HashMap;
use std::sync::Arc;

/// Granularity of exclusive access a tool requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusivity {
    /// Safe to run concurrently with any other tool.
    Shared,
    /// Requires exclusive access to underlying state; no other tool may run
    /// while this tool holds its lock slot.
    Exclusive,
}

/// Per-name registry of exclusive locks. Tools that declare `Exclusive` acquire
/// the lock for their own slot; all other tools share and never touch the locks.
#[derive(Default)]
pub struct ExclusivityRegistry {
    locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ExclusivityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the named lock slot exclusively, creating it on first use
    /// (SDD v1.5 §7.4.3). Async: waits until the slot is free and returns an
    /// owned guard that releases on drop, so the caller needs no extra Arc.
    pub async fn acquire(&self, name: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .locks
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        lock.lock_owned().await
    }
}

/// Process-global registry shared by all agent executions on this process.
pub fn global() -> &'static ExclusivityRegistry {
    static REG: std::sync::OnceLock<ExclusivityRegistry> = std::sync::OnceLock::new();
    REG.get_or_init(ExclusivityRegistry::new)
}
