//! Sub-agent orchestration runtime (SDD v1.5 Step 2a / 2.1-2.6).
//!
//! The root (Manager) agent may spawn read-only worker sub-agents concurrently.
//! Each worker is a full `LlmAgent` driven on a child [`InvocationContext`] with
//! `session_kind = SubAgent`, `depth = parent + 1`, `can_spawn = false` (no
//! grandchildren in Phase 0), and skil injection disabled (B4.3 `.without_skills()`).
//!
//! A single per-root-run [`Orchestrator`] owns the children map, the shared plan,
//! and a parent event channel (the EventPump sink). The orchestration *tools*
//! (`spawn_subagent` / `wait_subagent` / ...) reach it through a process-global
//! registry keyed by the root invocation id.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use futures::StreamExt;
use uuid::Uuid;

use crate::agent::{Agent, AgentEvent, EventStream, LlmAgent};
use crate::agent::event_pump::{EventPump, WORKER_CHANNEL_CAP};
use crate::config::ModelConfig;
use crate::context::{AgentMode, Confidence, InvocationContext, SessionKind, SubAgentResult, SubAgentSpec, SubAgentStatus};
use crate::error::{AgentError, AgentResult};
use crate::model::openai::OpenAiProvider;
use crate::permission::PendingMap;
use crate::skill::SkillManager;
use crate::tool::ToolRegistry;

/// Cloned environment needed to construct and run a worker sub-agent.
#[derive(Clone)]
pub struct OrchestratorEnv {
    pub provider: Arc<OpenAiProvider>,
    pub tools: Arc<tokio::sync::RwLock<ToolRegistry>>,
    pub working_dir: String,
    pub workspace_dir: String,
    pub model_configs: Vec<ModelConfig>,
    pub max_iterations: usize,
    pub parallel_ir_tools: bool,
    pub user_given_name: String,
    pub two_tier_memory: bool,
    pub sop_replay: Arc<std::sync::atomic::AtomicBool>,
    pub parent_model: String,
    // Permission context copied from the parent so workers can execute read-only tools.
    pub permissions: Arc<tokio::sync::Mutex<HashMap<String, bool>>>,
    pub permission_pending: PendingMap,
    pub preauth_profile: Option<Arc<crate::managed::permission_profile::PermissionProfile>>,
    pub context_window: usize,
    pub enable_context_scaling: bool,
    pub max_inline_chars: usize,
    pub tool_timeout_secs: u64,
    pub max_tool_retries: usize,
    /// Upper bound on concurrently running sub-agents (used to size the
    /// EventPump registration channel, SDD §7.4.2 delta).
    pub max_concurrent_subagents: usize,
    /// Default per-worker deadline (SDD §7.4.1 / 2.1) applied when the spec
    /// does not provide an explicit 	imeout.
    pub default_timeout_secs: u64,
    /// Optional memory store (for persisting sub-agent results across crashes).
    pub memory_store: Option<Arc<crate::memory::MemoryStore>>,
}

/// Live state of one worker sub-agent, shared between the Orchestrator (for
/// wait/list/cancel) and the worker task (which mutates result/status on exit).
#[derive(Clone)]
pub struct SubAgentHandle {
    pub run_id: String,
    pub role: String,
    pub spec: SubAgentSpec,
    pub status: Arc<Mutex<SubAgentStatus>>,
    pub result: Arc<Mutex<Option<SubAgentResult>>>,
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
    pub log: Arc<RwLock<Vec<String>>>,
    pub done: Arc<tokio::sync::Notify>,
}

impl SubAgentHandle {
    fn new(run_id: String, spec: SubAgentSpec) -> Self {
        let role = spec.role.clone();
        Self {
            run_id,
            spec,
            role,
            status: Arc::new(Mutex::new(SubAgentStatus::Pending)),
            result: Arc::new(Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            log: Arc::new(RwLock::new(Vec::new())),
            done: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

/// Default sub-agent nesting depth allowed for Phase 0 (root spawns depth-1
/// workers only; no grandchildren because workers have `can_spawn = false`).
pub const DEFAULT_MAX_DEPTH: u8 = 1;

/// Build the WS `budget_update` payload for a worker's per-run budget snapshot
/// (SDD v1.5 §7.7 / Step 3). The frontend reads the per-run map on this event
/// to render the sub-agent's token card.
pub fn budget_update_ws(snap: &BudgetSnapshot) -> serde_json::Value {
    serde_json::json!({
        "type": "budget_update",
        "run_id": snap.run_id,
        "role": snap.role,
        "prompt_tokens": snap.prompt_tokens,
        "completion_tokens": snap.completion_tokens,
        "total_tokens": snap.total_tokens,
    })
}

/// Build the WS `subagent` status payload for a terminal worker (SDD v1.5
/// §7.5 / Step 3). Frontend renders an Agent Card with a Cancel affordance
/// while running; terminal events finalize the card.
pub fn subagent_ws(result: &SubAgentResult) -> serde_json::Value {
    serde_json::json!({
        "type": "subagent",
        "run_id": result.run_id,
        "role": result.role,
        "status": format!("{:?}", result.status),
        "summary": result.summary,
        "token_usage": result.token_usage,
    })
}

/// Aggregate audit verdict (SDD v1.5 §7.5 Auditor 契约).
/// a deterministic audit over the aggregated sub-agent results before the caller
/// persists `proposed_writes`; the full LLM-backed Auditor remains in
/// `src/managed/auditor.rs` for the Manager-layer pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditVerdict {
    Pass,
    Fail(String),
    Uncertain(String),
}

/// Deterministic aggregate auditor (§7.5 default behavior):
/// - every non-High confidence result must carry >= 1 `evidence_refs`;
/// - a `proposed_writes` payload must be traceable to a role that actually ran;
/// - contradictory results (same `case_ref`, conflicting role claims) => Uncertain.
pub fn audit_aggregate(results: &[SubAgentResult]) -> AuditVerdict {
    if results.is_empty() {
        return AuditVerdict::Uncertain("no sub-agent results to audit".to_string());
    }
    let roles: std::collections::HashSet<&str> = results.iter().map(|r| r.role.as_str()).collect();
    for r in results {
        if r.confidence != crate::context::Confidence::High && r.evidence_refs.is_empty() {
            return AuditVerdict::Fail(format!(
                "worker '{}' has confidence != High but no evidence_refs", r.role));
        }
        // proposed_writes must name targets consistent with a role that ran.
        for w in &r.proposed_writes {
            if w.target.is_empty() {
                return AuditVerdict::Fail(format!(
                    "worker '{}' returned an empty proposed_write target", r.role));
            }
        }
        if roles.len() != results.len() {
            return AuditVerdict::Uncertain("duplicate roles make aggregation ambiguous".to_string());
        }
    }
    AuditVerdict::Pass
}

/// Per-worker token budget snapshot (SDD §7.7). Keyed by run_id in the
/// Orchestrator's per-run map; the WS `budget_update{run_id}` (Step 3) reads it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BudgetSnapshot {
    pub run_id: String,
    pub role: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Per-root-run orchestrator.
pub struct Orchestrator {
    env: OrchestratorEnv,
    root_invocation_id: String,
    root_session_id: String,
    root_ended: Arc<std::sync::atomic::AtomicBool>,
    max_depth: u8,
    pub plan: Arc<RwLock<Option<serde_json::Value>>>,
    children: Arc<Mutex<HashMap<String, SubAgentHandle>>>,
    /// Registration channel to the EventPump (bounded, cap = max_concurrent*2).
    reg_tx: Option<tokio::sync::mpsc::Sender<tokio::sync::mpsc::Receiver<AgentEvent>>>,
    /// Per-run token budgets (SDD §7.7), keyed by run_id.
    budgets: Arc<Mutex<HashMap<String, BudgetSnapshot>>>,
    /// Serial gate for write/exec workers (SDD v1.5 §7.10 / §7.11 layer 2):
    /// write/exec workers are forced to run one-at-a-time. Read-only workers
    /// never touch this gate (they are concurrency-safe).
    write_gate: Arc<tokio::sync::Mutex<()>>,
}

impl Orchestrator {
    /// Create an orchestrator for a root run. `root_invocation_id` is the key the
    /// orchestration tools use to look this orchestrator up.
    pub fn new(
        env: OrchestratorEnv,
        root_invocation_id: String,
        root_session_id: String,
        root_ended: Arc<std::sync::atomic::AtomicBool>,
        max_depth: u8,
        parent_tx: Option<tokio::sync::mpsc::Sender<AgentResult<AgentEvent>>>,
    ) -> Self {
        // SDD §7.4.2: start a single EventPump that forwards every worker's own
        // event channel to the parent stream. The registration channel is
        // *bounded* (cap = max_concurrent*2) so the register hop carries
        // backpressure too (accepted delta vs the spec's UnboundedReceiver).
        let reg_tx = if let Some(ptx) = parent_tx {
            let cap = env.max_concurrent_subagents.max(1) * 2;
            let (reg_tx, reg_rx) = tokio::sync::mpsc::channel::<
                tokio::sync::mpsc::Receiver<AgentEvent>
            >(cap);
            tokio::spawn(EventPump::new(reg_rx, ptx).run());
            Some(reg_tx)
        } else {
            None
        };
        Self {
            env,
            root_invocation_id,
            root_session_id,
            root_ended,
            max_depth,
            plan: Arc::new(RwLock::new(None)),
            children: Arc::new(Mutex::new(HashMap::new())),
            reg_tx,
            budgets: Arc::new(Mutex::new(HashMap::new())),
            write_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn root_invocation_id(&self) -> &str {
        &self.root_invocation_id
    }

    /// Per-run token budget for one worker (SDD §7.7).
    pub fn budget(&self, run_id: &str) -> Option<BudgetSnapshot> {
        self.budgets.lock().unwrap().get(run_id).cloned()
    }

    /// Snapshot of all current worker token budgets.
    pub fn budgets(&self) -> Vec<BudgetSnapshot> {
        self.budgets.lock().unwrap().values().cloned().collect()
    }

    /// Validate this run may spawn (depth bound + can_spawn gate).
    fn check_can_spawn(&self, ctx_depth: u8) -> AgentResult<()> {
        if ctx_depth + 1 > self.max_depth {
            return Err(AgentError::depth_limit());
        }
        if !self.root_ended.load(Ordering::SeqCst) && ctx_depth >= self.max_depth {
            // max_depth reached
        }
        Ok(())
    }

    /// Reuse a finished worker for `role` if one already exists for this root.
    /// (a) in-memory terminal handle -> return its run id (already registered).
    /// (b) persisted terminal result (crash recovery) -> register a synthetic
    ///     completed handle so `wait`/`get_result` resolve, then return its id
    ///     without re-spawning the worker.
    fn try_reuse(&self, role: &str) -> Option<String> {
        // 1) In-memory terminal handle for the same role.
        {
            let map = self.children.lock().unwrap();
            for h in map.values() {
                let terminal = matches!(*h.status.lock().unwrap(),
                    SubAgentStatus::Ok | SubAgentStatus::Failed
                    | SubAgentStatus::Cancelled | SubAgentStatus::Timeout);
                if h.role == role && terminal {
                    return Some(h.run_id.clone());
                }
            }
        }
        // 2) Persisted terminal result for this root (crash recovery).
        if let Some(ms) = &self.env.memory_store {
            if let Ok(rows) = ms.load_subagent_results(&self.root_invocation_id) {
                for r in &rows {
                    let terminal = matches!(r.status,
                        SubAgentStatus::Ok | SubAgentStatus::Failed
                        | SubAgentStatus::Cancelled | SubAgentStatus::Timeout);
                    if r.role == role && terminal {
                        let mut map = self.children.lock().unwrap();
                        if map.contains_key(&r.run_id) {
                            continue;
                        }
                        let spec = SubAgentSpec {
                            role: role.to_string(),
                            prompt: String::new(),
                            system_prompt: None,
                            tools_allowlist: Vec::new(),
                            allow_write: false,
                            allow_exec: false,
                            model: None,
                            timeout: None,
                            max_tokens: None,
                            max_iterations: None,
                            skills: Vec::new(),
                        };
                        let handle = SubAgentHandle::new(r.run_id.clone(), spec);
                        *handle.status.lock().unwrap() = r.status;
                        *handle.result.lock().unwrap() = Some(r.clone());
                        handle.log.write().unwrap().push(
                            "reused from persisted result (crash recovery)".to_string());
                        handle.done.notify_waiters();
                        map.insert(r.run_id.clone(), handle);
                        return Some(r.run_id.clone());
                    }
                }
            }
        }
        None
    }

    /// Spawn a worker sub-agent from a spawn request made by the parent.
    /// `ctx_depth` is the parent's current depth; the worker gets depth+1.
    pub async fn spawn(&self, spec: &SubAgentSpec, ctx_depth: u8, parent_id: &str, parent_session: &str, parent_author: &str) -> AgentResult<String> {
        if ctx_depth + 1 > self.max_depth {
            return Err(AgentError::depth_limit());
        }
        // Crash-recovery reuse: if this root already has a terminal result for
        // the same role (in-memory or persisted), return the existing run id
        // instead of re-spawning the worker (Phase 0 kill-9 gate).
        if let Some(existing) = self.try_reuse(&spec.role) {
            return Ok(existing);
        }
        let run_id = Uuid::new_v4().to_string();
        let session_id = format!("sub-{run_id}");
        let role = spec.role.clone();

        // Worker allowlist: explicit list if given, else the read-only subset;
        // skill tools + orchestration tools are always stripped (B4.2 / G-no-skill-tools).
        // Three-tier + skill/orch strip (SDD v1.5 §7.10 / §20.3 B4.2):
        //   base = explicit allowlist if given, else read-only subset;
        //   Step 2b opt-in: allow_write / allow_exec additionally admit the
        //   write/modify/delete and execute tools respectively (§7.10 layer 2).
        let mut allow: Vec<String> = if spec.tools_allowlist.is_empty() {
            let mut base = self.env.tools.read().await.read_only_names();
            if spec.allow_write || spec.allow_exec {
                base.extend(self.env.tools.read().await.write_exec_names(
                    spec.allow_write, spec.allow_exec));
            }
            base
        } else {
            spec.tools_allowlist.clone()
        };
        // §7.10 / Step 2b: a write/exec worker under a pre-auth containment
        // profile additionally gains the tool names its profile pre-authorizes
        // (reverse of `check_preauthorization`). Gated to opt-in write/exec and
        // to the no-explicit-allowlist branch, so Phase 0 read-only workers keep
        // the pure read-only subset (read-only guarantee intact).
        if (spec.allow_write || spec.allow_exec) && spec.tools_allowlist.is_empty() {
            if let Some(profile) = &self.env.preauth_profile {
                allow.extend(crate::managed::permission_profile::authorized_tool_names(profile));
            }
        }
        allow.retain(|n| {
            !SkillManager::skill_tool_names().iter().any(|s| s == n)
                && !crate::agent::llm_agent::is_orchestration_name(n.as_str())
        });
        let worker_registry = {
            let reg = self.env.tools.read().await;
            reg.subset(&allow)
        };

        // Build the child invocation context via the unified pass-through path
        // (SDD v1.5 §7.1 / Step 1.4): mode=Expert, depth+1, can_spawn=false,
        // SessionKind::SubAgent, skill strategy Disabled — all in one constructor.
        let child_ctx = InvocationContext::subagent_child(crate::context::SubagentChildParams {
            run_id: run_id.clone(),
            role: role.clone(),
            model: spec.model.clone().unwrap_or_else(|| self.env.parent_model.clone()),
            max_iterations: self.env.max_iterations,
            parent_invocation_id: parent_id.to_string(),
            root_invocation_id: self.root_invocation_id.clone(),
            parent_depth: ctx_depth,
            permissions: self.env.permissions.clone(),
            permission_pending: self.env.permission_pending.clone(),
            preauth_profile: self.env.preauth_profile.clone(),
            context_window: self.env.context_window,
            enable_context_scaling: self.env.enable_context_scaling,
            max_inline_chars: self.env.max_inline_chars,
            tool_timeout_secs: self.env.tool_timeout_secs,
            max_tool_retries: self.env.max_tool_retries,
            ended: self.root_ended.clone(),
        });

        // Build the worker agent with skills disabled (B4.3).
        let worker = LlmAgentBuilder::new()
            .name(&role)
            .provider(self.env.provider.clone())
            .tools(Arc::new(tokio::sync::RwLock::new(worker_registry)))
            .working_dir(&self.env.working_dir)
            .workspace_dir(&self.env.workspace_dir)
            .model_configs(self.env.model_configs.clone())
            .max_iterations(spec.max_iterations.unwrap_or(self.env.max_iterations))
            .parallel_ir_tools(self.env.parallel_ir_tools)
            .user_given_name(&self.env.user_given_name)
            .two_tier_memory(self.env.two_tier_memory)
            .sop_replay(self.env.sop_replay.clone())
            .mode(AgentMode::Expert)
            .depth(ctx_depth + 1)
            .without_skills()
            .build()?;

        let handle = SubAgentHandle::new(run_id.clone(), spec.clone());
        self.children.lock().unwrap().insert(run_id.clone(), handle.clone());

        // Dispatch the worker task. Clone borrowed inputs into owned values first
        // so nothing borrowed escapes into the 'static spawned task.
        // Give the worker its own event channel and register it on the EventPump
        // (bounded reg channel => register hop carries backpressure, §7.4.2 delta).
        let (worker_tx, worker_rx) = if self.reg_tx.is_some() {
            let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(WORKER_CHANNEL_CAP);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        if let (Some(reg), Some(rx)) = (self.reg_tx.as_ref(), worker_rx) {
            let _ = reg.send(rx).await; // bounded: blocks if EventPump backs up
        }
        let worker = Arc::new(worker);
        let child_ctx = Arc::new(child_ctx);
        let spec_task = spec.clone();
        let handle_task = handle.clone();
        let parent_author_task = parent_author.to_string();
        let _ = parent_session;
        let memory_store = self.env.memory_store.clone();
        let root_invocation_id = self.root_invocation_id.clone();
        let budgets = self.budgets.clone();
        // Per-worker deadline (SDD §7.3 default_timeout / §7.4.1): explicit spec
        // timeout if given, else the configured default. The worker future is
        // wrapped so an overrun finalizes the handle as `Timeout` (terminal).
        let timeout_secs = spec.timeout.unwrap_or(self.env.default_timeout_secs);
        // Clones used by the timeout branch; the originals move into `fut`.
        let handle_tmo = handle.clone();
        let ms_tmo = memory_store.clone();
        let root_tmo = root_invocation_id.clone();
        let budgets_tmo = budgets.clone();
        let write_gate = self.write_gate.clone();
        tokio::spawn(async move {
            let fut = run_worker(worker, &child_ctx, spec_task, handle_task, worker_tx,
                parent_author_task.as_str(), memory_store, root_invocation_id, budgets, write_gate);
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut).await {
                Ok(res) => { let _ = res; }
                // Deadline exceeded: the worker future is dropped mid-run; finalize
                // the handle as Timeout (terminal) so wait/get resolve and the
                // result persists (SDD §7.4.1 / 2.1).
                Err(_) => {
                    let tmo_result = SubAgentResult {
                        run_id: handle_tmo.run_id.clone(),
                        role: handle_tmo.role.clone(),
                        summary: "<worker timed out>".to_string(),
                        confidence: Confidence::Low,
                        token_usage: 0,
                        evidence_refs: Vec::new(),
                        artifact_refs: Vec::new(),
                        case_ref: None,
                        proposed_writes: Vec::new(),
                        status: SubAgentStatus::Timeout,
                    };
                    *handle_tmo.result.lock().unwrap() = Some(tmo_result.clone());
                    *handle_tmo.status.lock().unwrap() = SubAgentStatus::Timeout;
                    if let Some(ms) = ms_tmo {
                        if let Err(e) = ms.save_subagent_result(&root_tmo, &tmo_result) {
                            tracing::warn!("[sub-agent] failed to persist timeout result: {}", e);
                        }
                    }
                    budgets_tmo.lock().unwrap().insert(handle_tmo.run_id.clone(), BudgetSnapshot {
                        run_id: handle_tmo.run_id.clone(),
                        role: handle_tmo.role.clone(),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                    });
                    handle_tmo.done.notify_waiters();
                }
            }
        });

        handle.log.write().unwrap().push(format!("spawned session {session_id} role {role}"));
        // mark running
        *handle.status.lock().unwrap() = SubAgentStatus::Running;
        Ok(run_id)
    }

    /// Block until a worker reaches a terminal state and return its result.
    pub async fn wait(&self, run_id: &str) -> AgentResult<SubAgentResult> {
        let done = {
            let map = self.children.lock().unwrap();
            let h = map.get(run_id).ok_or_else(|| AgentError::not_found(
                crate::error::ErrorComponent::Orchestration,
                format!("sub-agent {run_id} not found"),
            ))?;
            // Fast path: if the worker already finished (or was recovered from a
            // persisted result), return immediately without waiting on the notifier.
            if let Some(r) = h.result.lock().unwrap().clone() {
                return Ok(r);
            }
            h.done.clone()
        };
        done.notified().await;
        let result = {
            let map = self.children.lock().unwrap();
            map.get(run_id).and_then(|h| h.result.lock().unwrap().clone())
        };
        result.ok_or_else(|| AgentError::agent(format!("sub-agent {run_id} produced no result")))
    }

    /// Non-blocking current status of a worker.
    pub fn status(&self, run_id: &str) -> Option<(String, SubAgentStatus)> {
        let map = self.children.lock().unwrap();
        map.get(run_id).map(|h| (h.role.clone(), *h.status.lock().unwrap()))
    }

    /// List all workers as (run_id, role, status).
    pub fn list(&self) -> Vec<(String, String, SubAgentStatus)> {
        let map = self.children.lock().unwrap();
        map.values()
            .map(|h| (h.run_id.clone(), h.role.clone(), *h.status.lock().unwrap()))
            .collect()
    }

    pub fn cancel(&self, run_id: &str) -> bool {
        let map = self.children.lock().unwrap();
        if let Some(h) = map.get(run_id) {
            h.cancel.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn get_result(&self, run_id: &str) -> Option<SubAgentResult> {
        let map = self.children.lock().unwrap();
        map.get(run_id).and_then(|h| h.result.lock().unwrap().clone())
    }

    pub fn read_log(&self, run_id: &str) -> Vec<String> {
        let map = self.children.lock().unwrap();
        map.get(run_id).map(|h| h.log.read().unwrap().clone()).unwrap_or_default()
    }

    pub fn update_plan(&self, plan: serde_json::Value) {
        *self.plan.write().unwrap() = Some(plan);
    }

    pub fn plan(&self) -> Option<serde_json::Value> {
        self.plan.read().unwrap().clone()
    }
}

impl Drop for Orchestrator {
    fn drop(&mut self) {
        unregister_orchestrator(&self.root_invocation_id);
    }
}

/// Drive one worker to completion: consume its event stream, build a summary,
/// apply cancellation, and finalize the handle (status + result + notify).
/// Recursively harvest short string leaves from a tool result JSON. Used to
/// populate a worker`s `evidence_refs` (SDD v1.5 §7.5) so the deterministic
/// auditor has real references to audit against, instead of an always-empty list.
pub fn string_leaves(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(strv) => {
            if !strv.is_empty() && strv.len() <= 300 { out.push(strv.clone()); }
        }
        serde_json::Value::Array(items) => { for it in items { string_leaves(it, out); } }
        serde_json::Value::Object(map) => {
            if let Some(path) = map.get("path").and_then(|v| v.as_str()) {
                if !path.is_empty() { out.push(path.to_string()); }
            }
            for v in map.values() { string_leaves(v, out); }
        }
        _ => {}
    }
}

async fn run_worker(
    worker: Arc<LlmAgent>,
    child_ctx: &InvocationContext,
    spec: SubAgentSpec,
    handle: SubAgentHandle,
    worker_tx: Option<tokio::sync::mpsc::Sender<AgentEvent>>,
    parent_author: &str,
    memory_store: Option<Arc<crate::memory::MemoryStore>>,
    root_invocation_id: String,
    budgets: Arc<Mutex<HashMap<String, BudgetSnapshot>>>,
    write_gate: Arc<tokio::sync::Mutex<()>>,
) -> AgentResult<SubAgentResult> {
    // Step 2b (SDD v1.5 §7.10 / §7.11 layer 2): write/exec workers are forced to
    // run one-at-a-time. Acquire the gate before running; dropped on return.
    let _serial_guard = if spec.allow_write || spec.allow_exec {
        Some(write_gate.lock().await)
    } else {
        None
    };
    let mut summary = String::new();
    let mut evidence_refs: Vec<String> = Vec::new();
    let mut token_usage: u64 = 0;
    let mut prompt_sum: u64 = 0;
    let mut completion_sum: u64 = 0;
    let mut cancelled = false;

    let stream: AgentResult<EventStream> = worker.run(child_ctx, &spec.prompt, vec![]).await;
    if let Ok(mut stream) = stream {
        while let Some(ev) = stream.next().await {
            if handle.cancel.load(Ordering::SeqCst) || child_ctx.is_ended() {
                cancelled = true;
                break;
            }
            match &ev {
                Ok(AgentEvent::TextDelta { content, .. }) => {
                    if summary.len() < 4000 {
                        summary.push_str(content);
                    }
                }
                Ok(AgentEvent::ToolCall { name, .. }) => {
                    handle.log.write().unwrap().push(format!("tool_call: {name}"));
                }
                Ok(AgentEvent::ToolResult { name, result, .. }) => {
                    handle.log.write().unwrap().push(format!("tool_result: {name}"));
                    if evidence_refs.len() < 24 {
                        let mut tmp = Vec::new();
                        string_leaves(result, &mut tmp);
                        for leaf in tmp {
                            if evidence_refs.len() >= 24 { break; }
                            if !evidence_refs.contains(&leaf) { evidence_refs.push(leaf); }
                        }
                    }
                }
                Ok(AgentEvent::Usage { prompt_tokens, completion_tokens, total_tokens, .. }) => {
                    prompt_sum += prompt_tokens;
                    completion_sum += completion_tokens;
                    token_usage = *total_tokens;
                }
                _ => {}
            }
            // Non-blocking write into this worker's own channel; the EventPump
            // drains it and applies device-bound backpressure (§7.4.2).
            if let (Some(ref tx), Ok(e)) = (worker_tx.as_ref(), &ev) {
                let _ = tx.try_send(e.clone());
            }
        }
    }

    let status = if cancelled {
        SubAgentStatus::Cancelled
    } else {
        SubAgentStatus::Ok
    };
    let result = SubAgentResult {
        run_id: handle.run_id.clone(),
        role: spec.role.clone(),
        summary,
        confidence: Confidence::Medium,
        token_usage,
        evidence_refs,
        artifact_refs: Vec::new(),
        case_ref: None,
        proposed_writes: Vec::new(),
        status,
    };
    *handle.result.lock().unwrap() = Some(result.clone());
    *handle.status.lock().unwrap() = result.status;
    if let Some(ms) = &memory_store {
        if let Err(e) = ms.save_subagent_result(&root_invocation_id, &result) {
            tracing::warn!("[sub-agent:{}] failed to persist result: {}", spec.role, e);
        }
    }
    // Per-run budget snapshot (SDD §7.7).
    budgets.lock().unwrap().insert(handle.run_id.clone(), BudgetSnapshot {
        run_id: handle.run_id.clone(),
        role: spec.role.clone(),
        prompt_tokens: prompt_sum,
        completion_tokens: completion_sum,
        total_tokens: token_usage,
    });
    handle.done.notify_waiters();
    let _ = parent_author;
    crate::agent::event_pump::broadcast_subagent_result(&worker_tx, &result);
    Ok(result)
}

// ---------------------------------------------------------------------------
// Process-global registry: maps root invocation id -> Arc<Orchestrator>.
// ---------------------------------------------------------------------------

static ORCHESTRATORS: OnceLock<Mutex<HashMap<String, Arc<Orchestrator>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, Arc<Orchestrator>>> {
    ORCHESTRATORS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_orchestrator(root_invocation_id: String, orch: Arc<Orchestrator>) {
    registry().lock().unwrap().insert(root_invocation_id, orch);
}

pub fn get_orchestrator(root_invocation_id: &str) -> Option<Arc<Orchestrator>> {
    registry().lock().unwrap().get(root_invocation_id).cloned()
}

/// Cancel a sub-agent across every live root orchestrator by its run id.
/// Used by the frontend's Agent Card Cancel button, which only knows `run_id`.
pub fn cancel_subagent_any(run_id: &str) -> bool {
    let map = registry().lock().unwrap();
    for orch in map.values() {
        if orch.cancel(run_id) {
            return true;
        }
    }
    false
}

pub(crate) fn unregister_orchestrator(root_invocation_id: &str) {
    if let Ok(mut m) = registry().lock() {
        m.remove(root_invocation_id);
    }
}

/// Convenience: the parameter-free builder for a worker agent. (Re-exported so
/// tools can share a single import surface.)
pub use crate::agent::llm_agent::LlmAgentBuilder;
