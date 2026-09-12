//! Mock-LLM driven manager gate (T6.8): feed canned Manager outputs (acting as a
//! deterministic mock LLM) through the manager plan parser and assert the parsed
//! round objective — the same parser that drives the ManagedRunner.

mod common;

use FoxIR::managed::manager::parse_manager_plan;

#[test]
fn mock_llm_output_parses_round_objective() {
    // A canned Manager output is parsed deterministically into a ManagerPlan.
    let plan = parse_manager_plan(common::canned_serial_manager_output());
    assert_eq!(plan.subtask, "enumerate services");
}
