//! Phase 0 end-to-end smoke (T6.8): exercised through the public library API so
//! it runs under `cargo test` for the lib target. Covers the Phase 0 signal that
//! matters for Expert multi-agent landing — template selection and the durable
//! plan truth-source (TaskContract <-> update_plan sync, T6.5).

use FoxIR::config::OrchestrationTemplate;
use FoxIR::managed::task_contract::TaskContract;

#[test]
fn phase0_template_default_and_parse() {
    // $10 default is Parallel; unknown falls back to Parallel (fail-safe).
    assert_eq!(OrchestrationTemplate::default(), OrchestrationTemplate::Parallel);
    assert_eq!(OrchestrationTemplate::parse("sequential"), OrchestrationTemplate::Sequential);
    assert_eq!(OrchestrationTemplate::parse("parallel"), OrchestrationTemplate::Parallel);
    assert_eq!(OrchestrationTemplate::parse("bogus"), OrchestrationTemplate::Parallel);
    assert_eq!(OrchestrationTemplate::Sequential.as_str(), "sequential");
}

#[test]
fn phase0_contract_plan_sync_roundtrip() {
    // T6.5: the durable plan truth-source persists and restores through the
    // contract (contract <-> update_plan path) without data loss.
    let mut c = TaskContract::new("e2e-1".into(), "task".into(), "scope".into(), 5);
    assert_eq!(c.orchestrator_plan, None, "fresh contract has no plan");
    let plan = serde_json::json!({ "subtask": "recon", "remaining_work": [] });
    c.set_orchestrator_plan(&plan);
    let back = TaskContract::from_json(&c.to_json().unwrap()).unwrap();
    assert_eq!(back.orchestrator_plan_value(), Some(plan), "plan survives contract round-trip");
}
