//! Shared helpers for the FoxIR integration / gate suites (T6.8).
//! Linked by `tests/*.rs` via `mod common;`.

/// A canned Expert Manager raw output that declares parallel subtasks, used as a
/// deterministic "mock LLM" response for the manager-parsing gate.
pub fn canned_parallel_manager_output() -> &'static str {
    "Subtask: aggregate parallel recon\nSuccess Criteria: merged report\nExpected Evidence: output/report.txt\nParallel Subtasks:\n- port_scan | enumerate open TCP ports on 10.0.0.5\n- log_parse | parse recent security logs for anomalies\nRoute: continue\n"
}

/// A canned Manager output with no parallel section (legacy single-executor).
pub fn canned_serial_manager_output() -> &'static str {
    "Subtask: enumerate services\nSuccess Criteria: service list\nExpected Evidence: output/services.txt\nRoute: continue\n"
}
