//! Mock-LLM driven manager gate (T6.8): feed canned Manager outputs (acting as a
//! deterministic mock LLM) through the manager plan parser and assert the
//! parallel/serial routing decision — the same signal that gates Expert-mode
//! sub-agent dispatch in the ManagedRunner.

mod common;

use FoxIR::managed::manager::parse_manager_plan;

#[test]
fn mock_llm_parallel_output_routes_to_subtasks() {
    // This is the exact text a real Manager LLM is expected to emit for a
    // parallel-recon plan; here it is mocked deterministically.
    let plan = parse_manager_plan(common::canned_parallel_manager_output());
    assert_eq!(plan.parallel_subtasks.len(), 2, "mock LLM must declare 2 subtasks");
    assert_eq!(plan.parallel_subtasks[0].role, "port_scan");
    assert_eq!(plan.parallel_subtasks[1].role, "log_parse");
}

#[test]
fn mock_llm_serial_output_keeps_legacy_single_executor() {
    // No `Parallel Subtasks:` section -> empty (legacy single-Executor path).
    let plan = parse_manager_plan(common::canned_serial_manager_output());
    assert!(plan.parallel_subtasks.is_empty(),
        "legacy plan must not spawn parallel sub-agents");
    assert_eq!(plan.subtask, "enumerate services");
}
