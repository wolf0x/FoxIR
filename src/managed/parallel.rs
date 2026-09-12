//! Managed parallel collection layer (SDD v1.5 §7.8.1 / T5.5).
//!
//! Bridges the Manager's declared [`ParallelSubtask`]s into the Orchestrator:
//! each subtask runs as an independent depth-1 read-only worker
//! (`can_spawn=false` — no grandchildren, anti-fragmentation), results are
//! aggregated, and the deterministic auditor (`audit_aggregate`) is applied.
//!
//! Single-writer contract: this layer only *collects* read-only worker results
//! and returns them to the caller (the ManagedRunner Manager is the sole writer
//! that folds them into the TaskContract). No TaskContract mutation happens here.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::agent::orchestration::{Orchestrator, OrchestratorEnv, DEFAULT_MAX_DEPTH};
use crate::config::OrchestrationLimits;
use crate::context::{SubAgentResult, SubAgentSpec, SubAgentStatus};
use crate::permission::PendingMap;
use crate::managed::manager::ParallelSubtask;
use crate::managed::permission_profile::PermissionProfile;
use crate::memory::MemoryStore;
use crate::model::openai::OpenAiProvider;
use crate::tool::ToolRegistry;

/// Everything the parallel layer needs to construct an Orchestrator. Supplied by
/// the ManagedRunner at the Executor-round insertion point.
pub struct ParallelEnv {
    pub provider: Arc<OpenAiProvider>,
    pub tools: Arc<tokio::sync::RwLock<ToolRegistry>>,
    pub working_dir: String,
    pub workspace_dir: String,
    /// Parent (manager/root) model — the default for workers lacking an override.
    pub model: String,
    pub max_iterations: usize,
    pub context_window: usize,
    pub max_inline_chars: usize,
    pub tool_timeout_secs: u64,
    pub max_tool_retries: usize,
    pub two_tier_memory: bool,
    pub limits: OrchestrationLimits,
    pub memory_store: Option<Arc<MemoryStore>>,
    pub permissions: Arc<tokio::sync::Mutex<std::collections::HashMap<String, bool>>>,
    pub permission_pending: PendingMap,
    pub preauth_profile: Option<Arc<PermissionProfile>>,
}

impl ParallelEnv {
    fn orchestrator(&self, root_id: &str, root_session: &str) -> Orchestrator {
        let env = OrchestratorEnv {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            working_dir: self.working_dir.clone(),
            workspace_dir: self.workspace_dir.clone(),
            model_configs: Vec::new(),
            max_iterations: self.max_iterations,
            parallel_ir_tools: false,
            user_given_name: String::new(),
            two_tier_memory: self.two_tier_memory,
            sop_replay: Arc::new(AtomicBool::new(false)),
            parent_model: self.model.clone(),
            permissions: self.permissions.clone(),
            permission_pending: self.permission_pending.clone(),
            preauth_profile: self.preauth_profile.clone(),
            context_window: self.context_window,
            enable_context_scaling: true,
            max_inline_chars: self.max_inline_chars,
            tool_timeout_secs: self.tool_timeout_secs,
            max_tool_retries: self.max_tool_retries,
            max_concurrent_subagents: self.limits.max_concurrent_subagents.max(1),
            default_timeout_secs: self.limits.default_timeout_secs,
            memory_store: self.memory_store.clone(),
        };
        // parent_tx = None: this layer's workers are transient collectors, their
        // events are not forwarded to the UI (the caller surfaces a condensed brief).
        Orchestrator::new(env, root_id.to_string(), root_session.to_string(),
            Arc::new(AtomicBool::new(false)), DEFAULT_MAX_DEPTH, None)
    }
}

fn spec_for(ps: &ParallelSubtask, timeout: u64) -> SubAgentSpec {
    SubAgentSpec {
        role: ps.role.clone(),
        prompt: ps.task.clone(),
        system_prompt: None,
        // Empty allowlist + no write/exec => read-only tool subset (orchestration
        // and skill tools stripped by Orchestrator::spawn). Anti-fragmentation:
        // the worker is depth 1 with can_spawn=false (enforced by subagent_child).
        tools_allowlist: Vec::new(),
        allow_write: false,
        allow_exec: false,
        model: None,
        timeout: Some(timeout),
        max_tokens: None,
        max_iterations: None,
        skills: Vec::new(),
    }
}

/// Run the declared parallel read-only subtasks concurrently and return their
/// aggregated results. Empty input returns an empty vec (no-op — the legacy
/// single-Executor path is fully preserved).
pub async fn run_parallel_collect(
    env: &ParallelEnv,
    subtasks: &[ParallelSubtask],
    root_id: &str,
    root_session: &str,
) -> Vec<SubAgentResult> {
    if subtasks.is_empty() {
        return Vec::new();
    }
    let orch = env.orchestrator(root_id, root_session);
    let mut run_ids = Vec::new();
    for ps in subtasks {
        let spec = spec_for(ps, env.limits.default_timeout_secs);
        if let Ok(rid) = orch.spawn(&spec, 0, root_id, root_session, "manager").await {
            run_ids.push(rid);
        }
    }
    let mut results = Vec::new();
    for rid in run_ids {
        if let Ok(r) = orch.wait(&rid).await {
            results.push(r);
        }
    }
    results
}

/// Dispatch the declared parallel subtasks through the shared workflow template
/// selected by `OrchestrationLimits.template` (SDD v1.5 §10 / T6.3). Sequential
/// runs the workers one at a time (fail-fast); Parallel spawns all then
/// aggregates. Empty subtasks => empty result (legacy single-Executor path kept).
pub async fn run_template_collect(
    env: &ParallelEnv,
    subtasks: &[ParallelSubtask],
    template: crate::config::OrchestrationTemplate,
    root_id: &str,
    root_session: &str,
) -> Vec<SubAgentResult> {
    use crate::agent::workflow::{parallel, sequential, WorkflowStep};
    if subtasks.is_empty() {
        return Vec::new();
    }
    let steps: Vec<WorkflowStep> = subtasks.iter().map(|ps| {
        WorkflowStep::new(ps.role.clone(), ps.task.clone())
            .with_timeout(Some(env.limits.default_timeout_secs))
    }).collect();
    let orch = env.orchestrator(root_id, root_session);
    match template {
        crate::config::OrchestrationTemplate::Sequential => {
            sequential(&orch, root_id, &steps).await
        }
        crate::config::OrchestrationTemplate::Parallel => {
            parallel(&orch, root_id, &steps).await
        }
    }
}

/// Loop template entry (SDD v1.5 §10): iterate `max_rounds` deep-dive rounds on a
/// single role until the predicate is met or rounds are exhausted. Exposed for
/// the deterministic CLI self-test; the ManagedRunner's outer loop already
/// embodies Loop across rounds.
pub async fn run_loop_collect(
    env: &ParallelEnv,
    role: &str,
    plan_prompt: &str,
    max_rounds: usize,
    root_id: &str,
    root_session: &str,
) -> Vec<SubAgentResult> {
    use crate::agent::workflow::{loop_until};
    if max_rounds == 0 {
        return Vec::new();
    }
    let orch = env.orchestrator(root_id, root_session);
    loop_until(&orch, root_id, role, plan_prompt, max_rounds, |_| false).await
}

/// Single-writer collect disposition driven by the deterministic auditor
/// (SDD v1.5 §7.5 "Auditor 先于落盘"). The Manager uses this to decide whether
/// the aggregated worker results are promoted into verified/persisted state
/// (Pass) or degraded (Fail/Uncertain — partial results + divergence note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectDisposition {
    Promote,
    Degrade(String),
}

/// Evaluate the aggregated results with `audit_aggregate` and map to a
/// single-writer disposition.
pub fn collect_disposition(results: &[SubAgentResult]) -> CollectDisposition {
    use crate::agent::orchestration::{audit_aggregate, AuditVerdict};
    match audit_aggregate(results) {
        AuditVerdict::Pass => CollectDisposition::Promote,
        AuditVerdict::Fail(reason) | AuditVerdict::Uncertain(reason) => CollectDisposition::Degrade(reason),
    }
}

/// Render a compact, Manager-consumable brief of the collected worker results,
/// plus the deterministic auditor verdict. Callers (the single-writer Manager)
/// decide how to fold this into the TaskContract.
pub fn render_collect_brief(results: &[SubAgentResult]) -> String {
    use crate::agent::orchestration::audit_aggregate;
    if results.is_empty() {
        return String::new();
    }
    let verdict = audit_aggregate(results);
    let mut out = String::from("\n\n[Parallel Collect](#)\n");
    if let crate::agent::orchestration::AuditVerdict::Pass = verdict {
        out.push_str("Audit: Pass\n");
    } else {
        out.push_str("Audit: apply caution — not fully Pass\n");
    }
    for r in results {
        let st = match r.status { SubAgentStatus::Ok => "ok", _ => "non-ok" };
        let summary: String = r.summary.chars().take(400).collect();
        out.push_str(&format!("- [{}/{}] {}: {}\n", r.role, st, r.run_id, summary));
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Confidence, ProposedWrite, SubAgentResult, SubAgentStatus};

    fn res(role: &str, conf: Confidence, evidence: usize) -> SubAgentResult {
        SubAgentResult {
            run_id: format!("id-{role}"),
            role: role.to_string(),
            summary: format!("found {}", role),
            confidence: conf,
            token_usage: 0,
            evidence_refs: (0..evidence).map(|i| format!("ev{i}")).collect(),
            artifact_refs: Vec::new(),
            case_ref: None,
            proposed_writes: Vec::new(),
            status: SubAgentStatus::Ok,
        }
    }

    #[test]
    fn spec_for_is_read_only() {
        let ps = ParallelSubtask { role: "scan".into(), task: "do scan".into() };
        let sp = spec_for(&ps, 30);
        assert_eq!(sp.role, "scan");
        assert_eq!(sp.prompt, "do scan");
        assert!(!sp.allow_write && !sp.allow_exec, "parallel workers are read-only");
        assert!(sp.tools_allowlist.is_empty(), "empty allowlist => read-only subset");
        assert_eq!(sp.timeout, Some(30));
    }

    #[test]
    fn render_brief_empty_is_empty() {
        assert!(render_collect_brief(&[]).is_empty());
    }

    #[test]
    fn render_brief_aggregates_worker_lines_and_passes_clean_audit() {
        let results = vec![
            res("a", Confidence::Medium, 1),
            res("b", Confidence::High, 2),
        ];
        let brief = render_collect_brief(&results);
        assert!(brief.contains("[a/ok]"), "brief lists role+status: {brief}");
        assert!(brief.contains("[b/ok]"));
        assert!(brief.contains("Audit: Pass"), "all medium+ w/ evidence => Pass: {brief}");
    }

    #[test]
    fn render_brief_flags_low_confidence_without_evidence() {
        let results = vec![res("c", Confidence::Low, 0)];
        let brief = render_collect_brief(&results);
        assert!(brief.contains("apply caution"), "low/no-evidence must not Pass: {brief}");
    }

    #[test]
    fn collect_disposition_promotes_evidence_backed_results() {
        // High needs no evidence; medium must carry >=1 evidence -> Pass.
        let results = vec![
            res("a", Confidence::High, 0),
            res("b", Confidence::Medium, 1),
        ];
        assert_eq!(collect_disposition(&results), CollectDisposition::Promote);
    }

    #[test]
    fn collect_disposition_degrades_without_evidence() {
        let results = vec![res("c", Confidence::Medium, 0)];
        assert!(matches!(collect_disposition(&results), CollectDisposition::Degrade(_)));
    }

    #[test]
    fn collect_disposition_degrades_empty_proposed_write_target() {
        let mut r = res("d", Confidence::Medium, 1);
        r.proposed_writes = vec![ProposedWrite {
            kind: "file_write".into(),
            target: String::new(),
            payload: serde_json::json!(null),
        }];
        assert!(matches!(collect_disposition(&[r]), CollectDisposition::Degrade(_)));
    }

    #[test]
    fn collect_disposition_degrades_empty_results() {
        assert!(matches!(collect_disposition(&[]), CollectDisposition::Degrade(_)));
    }
}



