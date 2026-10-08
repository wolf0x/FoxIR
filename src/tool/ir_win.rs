//! Windows IR / forensics tool family — hot/cold tiering single source of truth.
//!
//! The Windows IR tools are registered individually in
//! [`ToolRegistry::build_default`](super::ToolRegistry::build_default) (unlike the
//! Linux family, which is derived from `linux_ir_category_tools()`), so the cold
//! set is declared explicitly here. Delivery is gated at runtime by the
//! `win_ir_tools` Settings switch (see `apply_win_ir_gate` in the agent): when the
//! switch is off, every tool named below is demoted from the always-on tool payload
//! to on-demand loading via `load_tool_schema` — still fully callable, just not
//! paid for on every turn.
//!
//! Tiering criteria (audited tool-by-tool):
//! 1. Target is the live host (hot) vs. a specific artifact file the user must feed
//!    (cold).
//! 2. Is it a near-every-turn collection/analysis backbone (hot) or not (cold).
//! 3. A one-shot terminal call is not worth per-turn residency (ir_scan, ir_report).
//!
//! **Resident hot (7):** `ir_process`, `ir_network`, `ir_account`, `ir_persistence`,
//! `ir_eventlog`, `ir_file`, `ir_analyzer`.
//! **Demoted cold (17):** the rest of the Windows IR/AV family — listed below.
//!
//! Boundary rulings:
//! - `ir_eventlog` stays hot (queries the live event service) and is distinct from
//!   its cold offline-file twin `ir_evtx_parse`.
//! - `ir_report` / `ir_scan` go cold (one-shot orchestrator / terminal).
//! - `ir_timeline` / `malware_scan` go cold (deep-hunt phase / need a located sample).
//!
//! Pipeline direction stays safe: `ir_analyzer` (the analysis hub) stays hot, so
//! `ir_report` / `ir_attackpath` — which depend on its findings — still connect
//! correctly after being pulled back via `load_tool_schema`.

/// Windows IR/forensics tools demoted to on-demand loading when the `win_ir_tools`
/// switch is off (the default). Large / task-specific / one-shot terminal calls.
/// Kept in sync with [`WIN_IR_HOT_TOOLS`] — the two must partition the 24 registered
/// Windows IR/AV tools with no overlap (asserted in tests).
pub const WIN_IR_COLD_TOOLS: &[&str] = &[
    "ir_scan",
    "ir_report",
    "ir_case",
    "ir_usn",
    "ir_evtx_parse",
    "ir_memdump",
    "ir_log_parse",
    "ir_artifacts",
    "ir_vss",
    "malware_deep",
    "ir_weblog_scan",
    "ir_timeline",
    "ir_attackpath",
    "ir_eml",
    "malware_scan",
    "ir_pcap_analyze",
    "ir_driver",
];

/// Windows IR/forensics tools that stay resident in every request (core triage
/// backbone — near-every-turn collection/analysis with zero round-trip latency).
/// Exposed so tests can assert the hot/cold partition is complete and disjoint.
pub const WIN_IR_HOT_TOOLS: &[&str] = &[
    "ir_process",
    "ir_network",
    "ir_account",
    "ir_persistence",
    "ir_eventlog",
    "ir_file",
    "ir_analyzer",
];

/// Whether `name` belongs to the demoted cold set (checked by the agent's tool-
/// delivery gate against the `win_ir_tools` Settings switch).
pub fn is_win_ir_cold(name: &str) -> bool {
    WIN_IR_COLD_TOOLS.contains(&name)
}
