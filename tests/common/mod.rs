//! Shared helpers for the FoxIR integration / gate suites (T6.8).
//! Linked by `tests/*.rs` via `mod common;`.

/// A canned Manager output used as a deterministic "mock LLM" response for the
/// manager-parsing gate.
pub fn canned_serial_manager_output() -> &'static str {
    "Subtask: enumerate services\nSuccess Criteria: service list\nExpected Evidence: output/services.txt\nRoute: continue\n"
}
