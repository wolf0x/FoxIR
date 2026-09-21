//! Skill routing & usage telemetry for the agentskills.io adoption.
//!
//! Lightweight atomic counters persisted to <skills>/.metrics.json. No new
//! dependency; enough signal to compare loading / follow / call-success rates
//! between router modes without a real embedding layer.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

static FILE: OnceLock<PathBuf> = OnceLock::new();
static CATALOG_TURNS: AtomicU64 = AtomicU64::new(0);
static READ_CALLS: AtomicU64 = AtomicU64::new(0);
static READ_FAILURES: AtomicU64 = AtomicU64::new(0);
static LOAD_FAILURES: AtomicU64 = AtomicU64::new(0);
static IMPROVEMENTS: AtomicU64 = AtomicU64::new(0);

fn bump(c: &AtomicU64) { c.fetch_add(1, Ordering::Relaxed); }
fn get(c: &AtomicU64) -> u64 { c.load(Ordering::Relaxed) }

fn set(c: &AtomicU64, v: &Value, key: &str) {
    if let Some(n) = v.get(key).and_then(|x| x.as_u64()) {
        c.store(n, Ordering::Relaxed);
    }
}

/// Initialize the metrics file (idempotent); loads any persisted counters.
pub fn init(path: PathBuf) {
    if FILE.set(path).is_ok() {
        if let Some(p) = FILE.get() {
            if let Ok(txt) = std::fs::read_to_string(p) {
                if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                    set(&CATALOG_TURNS, &v, "catalog_turns");
                    set(&READ_CALLS, &v, "read_skill_calls");
                    set(&READ_FAILURES, &v, "read_skill_failures");
                    set(&LOAD_FAILURES, &v, "load_failures");
                    set(&IMPROVEMENTS, &v, "improvements");
                }
            }
        }
    }
}

pub fn record_catalog_turn() { bump(&CATALOG_TURNS); }
pub fn record_read_call() { bump(&READ_CALLS); }
pub fn record_read_failure() { bump(&READ_FAILURES); }
pub fn record_load_failure() { bump(&LOAD_FAILURES); }
pub fn record_improvement() { bump(&IMPROVEMENTS); }

/// Persist counters to the metrics file (best-effort).
pub fn save() {
    let Some(p) = FILE.get() else { return; };
    if let Some(parent) = p.parent() {
        if std::fs::create_dir_all(parent).is_err() { return; }
    }
    let _ = std::fs::write(p, snapshot().to_string());
}

/// Current counters + router mode as JSON.
pub fn snapshot() -> Value {
    json!({
        "catalog_turns": get(&CATALOG_TURNS),
        "read_skill_calls": get(&READ_CALLS),
        "read_skill_failures": get(&READ_FAILURES),
        "load_failures": get(&LOAD_FAILURES),
        "improvements": get(&IMPROVEMENTS),
        "router_mode": "llm",
    })
}
