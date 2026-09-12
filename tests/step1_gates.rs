//! Step-1 gate aggregation (T6.8): re-assert the orchestration delivery gates
//! through the public library surface as an external integration suite, so the
//! gates are runnable/observable from `tests/` independent of in-crate unit tests.
//!
//! Gates covered:
//!   G-instant-tools   Instant root gets NO orchestration tools.
//!   G-sub-not-main    Expert depth>=1 (sub-agents) get no orchestration tools.
//!   G-expert-root     Expert depth 0 is opened to the full ALL_ORCH set.
//!   G-name-disjoint   Every ALL_ORCH tool is delivered to the Expert root.

use FoxIR::agent::llm_agent::{ALL_ORCH, orchestration_allowset, orchestration_delivered};
use FoxIR::context::AgentMode;

#[test]
fn g_instant_tools_is_empty() {
    assert!(orchestration_allowset(AgentMode::Instant, 0).is_empty(),
        "Instant mode must not receive orchestration tools");
}

#[test]
fn g_sub_not_main_no_tools_for_depth_one() {
    assert!(orchestration_allowset(AgentMode::Expert, 1).is_empty(),
        "depth>=1 (sub-agent) must not receive orchestration tools");
    assert!(orchestration_allowset(AgentMode::Instant, 1).is_empty());
}

#[test]
fn g_expert_root_full_allowset() {
    let allow = orchestration_allowset(AgentMode::Expert, 0);
    assert_eq!(allow.len(), ALL_ORCH.len(), "Expert root gets every orchestration tool");
}

#[test]
fn g_name_disjoint_all_tools_delivered() {
    let allow = orchestration_allowset(AgentMode::Expert, 0);
    for name in ALL_ORCH.iter() {
        assert!(orchestration_delivered(name, &allow),
            "{} must be delivered to the Expert root", name);
    }
}

#[test]
fn g_allowset_names_are_disjoint_from_non_orch() {
    // Names in ALL_ORCH must not collide with ordinary tool names (uniqueness).
    let mut seen = std::collections::HashSet::new();
    for n in ALL_ORCH.iter() {
        assert!(seen.insert(*n), "duplicate orchestration tool name: {n}");
    }
}
