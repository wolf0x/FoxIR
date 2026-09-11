//! Workflow templates (SDD v1.5 §10): Sequential / Parallel / Loop.
//!
//! These are plain orchestration-helper functions built on the [`Orchestrator`]
//! spawn/wait primitives — they do **not** introduce a new Agent type. Each
//! template expresses an `OrchestrationPlan` task tree and drives it with the
//! existing sub-agent runtime. Roles must be unique per run so result keys are
//! unambiguous (spawn de-dups by role).

use futures::future::join_all;

use crate::agent::orchestration::Orchestrator;
use crate::context::{SubAgentResult, SubAgentSpec, SubAgentStatus};

/// A single planned step in a workflow: run `role` with `prompt`.
#[derive(Debug, Clone)]
pub struct WorkflowStep {
    pub role: String,
    pub prompt: String,
    pub max_iterations: Option<usize>,
    /// Per-step worker deadline (seconds). `None` = use the orchestrator default.
    /// Lets a template test (or a caller) force a step to overrun and fail fast.
    pub timeout: Option<u64>,
}

impl WorkflowStep {
    pub fn new(role: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self { role: role.into(), prompt: prompt.into(), max_iterations: None, timeout: None }
    }

    /// Set an explicit per-step deadline (seconds); `None` restores orchestrator default.
    pub fn with_timeout(mut self, timeout: Option<u64>) -> Self {
        self.timeout = timeout;
        self
    }

    fn into_spec(&self) -> SubAgentSpec {
        SubAgentSpec {
            role: self.role.clone(),
            prompt: self.prompt.clone(),
            system_prompt: None,
            tools_allowlist: Vec::new(),
            allow_write: false,
            allow_exec: false,
            model: None,
            timeout: self.timeout,
            max_tokens: None,
            max_iterations: self.max_iterations,
            skills: Vec::new(),
        }
    }
}

/// Sequential template (§10): run steps one after another (research → analyze
/// → summarize). Fails fast: if any step does not finish `Ok`, returns the
/// partial vector with that step non-Ok so the caller can decide.
pub async fn sequential(
    orch: &Orchestrator,
    root: &str,
    steps: &[WorkflowStep],
) -> Vec<SubAgentResult> {
    let mut out = Vec::new();
    for s in steps {
        let run_id = orch.spawn(&s.into_spec(), 0, root, "sess", "parent").await.unwrap();
        let res = orch.wait(&run_id).await.unwrap();
        out.push(res.clone());
        if res.status != SubAgentStatus::Ok {
            break;
        }
    }
    out
}

/// Parallel template (§10): run steps concurrently (multi-angle checks) and
/// aggregate on completion.
pub async fn parallel(
    orch: &Orchestrator,
    root: &str,
    steps: &[WorkflowStep],
) -> Vec<SubAgentResult> {
    let mut ids = Vec::new();
    for s in steps {
        let run_id = orch.spawn(&s.into_spec(), 0, root, "sess", "parent").await.unwrap();
        ids.push(run_id);
    }
    join_all(ids.iter().map(|id| orch.wait(id))).await
        .into_iter()
        .map(|r| r.unwrap())
        .collect()
}

/// Loop template (§10): iterate `max_rounds` times, re-planning via
/// `update_plan` between rounds, and stop early when `stop(&results)` returns
/// true or no new Ok result is produced in a round. Each round runs one worker.
pub async fn loop_until<F>(
    orch: &Orchestrator,
    root: &str,
    role: &str,
    plan_prompt: &str,
    max_rounds: usize,
    mut stop: F,
) -> Vec<SubAgentResult>
where
    F: FnMut(&[SubAgentResult]) -> bool,
{
    let mut results = Vec::new();
    for round in 0..max_rounds {
        let step = WorkflowStep {
            role: format!("{role}-r{round}"),
            prompt: format!("{plan_prompt} (round {})", round + 1),
            max_iterations: None,
            timeout: None,
        };
        orch.update_plan(serde_json::json!({ "round": round + 1, "role": role }));
        let run_id = orch.spawn(&step.into_spec(), 0, root, "sess", "parent").await.unwrap();
        let res = orch.wait(&run_id).await.unwrap();
        let is_ok = res.status == SubAgentStatus::Ok;
        results.push(res.clone());
        if stop(&results) || !is_ok {
            break;
        }
    }
    results
}
