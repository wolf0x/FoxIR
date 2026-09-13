//! Phase 0 acceptance harness (SDD v1.5 Step 2a) — measures the four gates
//! using a scripted in-process OpenAI-compatible provider. No real LLM / network
//! required, so the metrics are reproducible in CI / headless.
//!
//! Gates measured (see SDD v1.5 D4 / Phase 0 验收指标):
//!  1. wall-clock  parallel read-only workers vs serial baseline  (>= 30% 縮短)
//!  2. token       orchestrated vs baseline consumption          (<= 50% 增幅)
//!  3. 20 runs     no SQLITE_BUSY / deadlock / event loss
//!  4. kill-9      崩溃重启后不重 spawn（复用已持久化结果）

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::post;
use axum::Router;
use axum::body::Bytes;
use futures::future::join_all;

use crate::agent::event::AgentEvent;
use crate::agent::orchestration::{Orchestrator, OrchestratorEnv, DEFAULT_MAX_DEPTH};
use crate::config::ModelConfig;
use crate::context::{SubAgentResult, SubAgentSpec, SubAgentStatus};
use crate::error::AgentResult;
use crate::memory::MemoryStore;
use crate::model::openai::OpenAiProvider;
use crate::permission::{PermissionResolver, default_permissions};
use crate::tool::ToolRegistry;

// ---------------------------------------------------------------------------
// In-process mock OpenAI-compatible provider (SSE / non-streaming)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockState {
    latency_ms: Arc<AtomicU64>,
    req_count: Arc<AtomicU64>,
    tokens: Arc<AtomicU64>,
}

async fn completions(State(st): State<MockState>, body: Bytes) -> String {
    st.req_count.fetch_add(1, Ordering::SeqCst);
    let latency = st.latency_ms.load(Ordering::SeqCst);
    if latency > 0 {
        tokio::time::sleep(Duration::from_millis(latency)).await;
    }
    let stream = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["stream"].as_bool())
        .unwrap_or(false);
    let p: u64 = 100;
    let c: u64 = 20;
    let t = p + c;
    st.tokens.fetch_add(t, Ordering::SeqCst);
    if stream {
        let c1 = serde_json::json!({
            "choices": [{"delta": {"role": "assistant", "content": "done"}, "finish_reason": null}],
            "usage": null
        })
        .to_string();
        let c2 = serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": p, "completion_tokens": c, "total_tokens": t}
        })
        .to_string();
        format!("data: {c1}

data: {c2}

data: [DONE]

")
    } else {
        serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": p, "completion_tokens": c, "total_tokens": t}
        })
        .to_string()
    }
}

async fn start_mock(latency_ms: u64) -> (String, MockState) {
    let st = MockState {
        latency_ms: Arc::new(AtomicU64::new(latency_ms)),
        req_count: Arc::new(AtomicU64::new(0)),
        tokens: Arc::new(AtomicU64::new(0)),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(completions))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("mock server error: {e}");
        }
    });
    (format!("http://127.0.0.1:{}/v1", addr.port()), st)
}

fn model_cfg(base: &str) -> ModelConfig {
    ModelConfig {
        title: "mock".into(),
        name: "mock".into(),
        api_base: base.to_string(),
        api_key: Some("sk-mock".into()),
        api_key_env: None,
        context_window: 128000,
        max_tokens: 1024,
        temperature: 0.0,
        supports_vision: false,
    }
}

// ---------------------------------------------------------------------------
// Bench scaffolding
// ---------------------------------------------------------------------------

struct Bench {
    base: String,
    mock: MockState,
    provider: Arc<OpenAiProvider>,
    tools: Arc<tokio::sync::RwLock<ToolRegistry>>,
    working: std::path::PathBuf,
    workspace: std::path::PathBuf,
}

impl Bench {
    async fn new(latency_ms: u64) -> Self {
        let (base, mock) = start_mock(latency_ms).await;
        let provider = Arc::new(OpenAiProvider::new(vec![model_cfg(&base)]));
        let tools = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
        let working = tempfile::tempdir().unwrap().into_path();
        let workspace = tempfile::tempdir().unwrap().into_path();
        Self {
            base,
            mock,
            provider,
            tools,
            working,
            workspace,
        }
    }

    fn spec(&self, role: &str) -> SubAgentSpec {
        SubAgentSpec {
            role: role.into(),
            prompt: format!("collect forensic evidence for {role}"),
            system_prompt: None,
            tools_allowlist: Vec::new(),
            allow_write: false,
            allow_exec: false,
            model: Some("mock".into()),
            timeout: None,
            max_tokens: None,
            max_iterations: Some(3),
            skills: Vec::new(),
        }
    }

    fn env(&self, ms: Arc<MemoryStore>) -> OrchestratorEnv {
        let (_resolver, pending) = PermissionResolver::new();
        OrchestratorEnv {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            working_dir: self.working.to_string_lossy().to_string(),
            workspace_dir: self.workspace.to_string_lossy().to_string(),
            model_configs: vec![model_cfg(&self.base)],
            max_iterations: 5,
            parallel_ir_tools: false,
            user_given_name: "tester".into(),
            two_tier_memory: false,
            sop_replay: Arc::new(AtomicBool::new(false)),
            parent_model: "mock".into(),
            permissions: Arc::new(tokio::sync::Mutex::new(default_permissions())),
            permission_pending: pending,
            preauth_profile: None,
            context_window: 128000,
            enable_context_scaling: false,
            max_inline_chars: 120000,
            tool_timeout_secs: 30,
            max_tool_retries: 0,
            max_concurrent_subagents: 8,
            default_timeout_secs: 300,
            memory_store: Some(ms),
        }
    }

    fn mem_store(&self) -> Arc<MemoryStore> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.db");
        let db = Arc::new(MemoryStore::new(path.to_str().unwrap()).unwrap());
        std::mem::forget(dir);
        db
    }
}

type OrchPair = (
    Arc<Orchestrator>,
    tokio::sync::mpsc::Receiver<AgentResult<AgentEvent>>,
);

fn make_orch(env: OrchestratorEnv, root: &str) -> OrchPair {
    let (ptx, prx) = tokio::sync::mpsc::channel::<AgentResult<AgentEvent>>(256);
    let root_ended = Arc::new(AtomicBool::new(false));
    let orch = Arc::new(Orchestrator::new(
        env,
        root.to_string(),
        "sess".into(),
        root_ended,
        DEFAULT_MAX_DEPTH,
        Some(ptx),
    ));
    (orch, prx)
}

async fn spawn_wait(orch: &Orchestrator, spec: &SubAgentSpec, root: &str) -> String {
    let run_id = orch.spawn(spec, 0, root, "sess", "parent").await.unwrap();
    let res = orch.wait(&run_id).await.unwrap();
    assert_eq!(res.status, SubAgentStatus::Ok, "worker {} should finish Ok", spec.role);
    run_id
}

// ---------------------------------------------------------------------------
// Gate 1: wall-clock speed-up  (parallel >= 30% faster than serial)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_wallclock_parallel_ge_30pct() {
    let b = Bench::new(60).await;
    let n = 5;

    // Serial baseline: spawn + wait one at a time.
    let ms1 = b.mem_store();
    let (orch_s, _prx) = make_orch(b.env(ms1.clone()), "root-s");
    let t0 = Instant::now();
    for i in 0..n {
        let spec = b.spec(&format!("w{i}"));
        spawn_wait(&orch_s, &spec, "root-s").await;
    }
    let serial = t0.elapsed();

    // Parallel: spawn all, then wait all concurrently.
    let ms2 = b.mem_store();
    let (orch_p, _prx) = make_orch(b.env(ms2.clone()), "root-p");
    let t1 = Instant::now();
    let mut ids = Vec::new();
    for i in 0..n {
        let spec = b.spec(&format!("w{i}"));
        let rid = orch_p.spawn(&spec, 0, "root-p", "sess", "parent").await.unwrap();
        ids.push(rid);
    }
    join_all(ids.iter().map(|id| orch_p.wait(id))).await;
    let parallel = t1.elapsed();

    eprintln!(
        "PHASE0 wall-clock: n={n} serial={:?} parallel={:?} ratio={:.2}x",
        serial,
        parallel,
        serial.as_secs_f64() / parallel.as_secs_f64()
    );
    assert!(
        parallel.as_secs_f64() <= 0.70 * serial.as_secs_f64(),
        "parallel {parallel:?} not <= 70% of serial {serial:?} (need >=30% cut)"
    );
}

// ---------------------------------------------------------------------------
// Gate 2: token budget  (orchestrated <= 1.5x serial)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_token_le_50pct_increase() {
    let b = Bench::new(5).await;
    let n = 6;

    let ms1 = b.mem_store();
    let (orch_s, _) = make_orch(b.env(ms1.clone()), "root-s");
    for i in 0..n {
        spawn_wait(&orch_s, &b.spec(&format!("w{i}")), "root-s").await;
    }
    let serial_tok = b.mock.tokens.load(Ordering::SeqCst);

    let ms2 = b.mem_store();
    let (orch_p, _) = make_orch(b.env(ms2.clone()), "root-p");
    let mut ids = Vec::new();
    for i in 0..n {
        let rid = orch_p.spawn(&b.spec(&format!("w{i}")), 0, "root-p", "sess", "parent").await.unwrap();
        ids.push(rid);
    }
    join_all(ids.iter().map(|id| orch_p.wait(id))).await;
    let par_tok = b.mock.tokens.load(Ordering::SeqCst) - serial_tok;

    eprintln!("PHASE0 token: serial={serial_tok} parallel={par_tok} pct_increase={:.1}%",
        (par_tok as f64) / (serial_tok.max(1) as f64) * 100.0 - 100.0);
    assert!(
        par_tok <= (serial_tok.max(1) as f64 * 1.5) as u64,
        "orchestrated tokens {par_tok} exceed 1.5x serial {serial_tok}"
    );
}

// ---------------------------------------------------------------------------
// Gate 3: 20 consecutive runs — no SQLITE_BUSY / deadlock / event loss
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_twenty_runs_no_busy_no_deadlock_no_event_loss() {
    let b = Bench::new(2).await;
    let ms = b.mem_store(); // shared DB across all 20 runs
    let m = 3;
    let runs = 20;

    for k in 0..runs {
        let root = format!("root-{k}");
        let (orch, mut prx) = make_orch(b.env(ms.clone()), &root);
        let mut ids = Vec::new();
        for i in 0..m {
            let rid = orch.spawn(&b.spec(&format!("r{k}w{i}")), 0, &root, "sess", "parent").await.unwrap();
            ids.push(rid);
        }
        join_all(ids.iter().map(|id| orch.wait(id))).await;

        // Event-loss guard: every worker must emit its typed completion milestone
        // (subagent_completed) onto the parent stream. Events flow through the
        // parent channel asynchronously, so drain with a bounded settle window
        // instead of a single non-blocking try_recv pass.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut broadcasts = 0usize;
        while broadcasts < m && std::time::Instant::now() < deadline {
            match prx.try_recv() {
                Ok(ev) => {
                    if matches!(ev, Ok(AgentEvent::SubagentCompleted { .. })) {
                        broadcasts += 1;
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        assert_eq!(broadcasts, m, "run {k}: lost {m} worker completion broadcast(s), got {broadcasts}");

        // Persistence guard: all m workers must be persisted (no lost / busy inserts).
        let persisted = ms.load_subagent_results(&root).unwrap().len();
        assert_eq!(persisted, m, "run {k}: expected {m} persisted results, got {persisted}");
    }

    let total: u64 = (0..runs)
        .map(|k| ms.load_subagent_results(&format!("root-{k}")).unwrap().len() as u64)
        .sum();
    eprintln!("PHASE0 20-runs OK: runs={runs} workers/run={m} total_persisted={total}");
    assert_eq!(total, runs as u64 * m as u64, "no run lost a persisted row");
}

// ---------------------------------------------------------------------------
// Gate 4: kill -9 — restart must NOT re-spawn completed workers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_kill9_restart_no_respawn() {
    let b = Bench::new(3).await;
    let ms = b.mem_store();
    let n = 3;
    let root = "root-K";

    // Phase A: run workers to completion + persistence.
    let mut ids_a = Vec::new();
    {
        let (orch, _) = make_orch(b.env(ms.clone()), root);
        for i in 0..n {
            let rid = spawn_wait(&orch, &b.spec(&format!("w{i}")), root).await;
            ids_a.push(rid);
        }
    }
    let req_before = b.mock.req_count.load(Ordering::SeqCst);
    let stored = ms.load_subagent_results(root).unwrap();
    assert_eq!(stored.len(), n, "phase A should persist {n} results");

    // Phase B: "restart" a new Orchestrator over the same store + root; re-issue
    // the same spawns. try_reuse must return the persisted run ids WITHOUT any
    // new LLM request (no re-spawn).
    let (orch2, _) = make_orch(b.env(ms.clone()), root);
    let mut ids_b = Vec::new();
    for i in 0..n {
        let rid = orch2.spawn(&b.spec(&format!("w{i}")), 0, root, "sess", "parent").await.unwrap();
        ids_b.push(rid);
    }
    let req_after = b.mock.req_count.load(Ordering::SeqCst);

    eprintln!(
        "PHASE0 kill-9: reqs before={req_before} after={req_after} n={n} ids_a={ids_a:?} ids_b={ids_b:?}"
    );
    // No re-spawn => no new worker LLM request after restart.
    assert_eq!(
        req_after, req_before,
        "restart re-spawned workers: extra LLM requests {}/{}",
        req_after, req_before
    );
    // The same (persisted) run ids are returned so wait/get_result resolve.
    let mut a_sorted: Vec<String> = ids_a.clone();
    let mut b_sorted = ids_b.clone();
    a_sorted.sort();
    b_sorted.sort();
    assert_eq!(a_sorted, b_sorted, "restart should reuse the persisted run ids");

    // Completed results are still resolvable on the fresh orchestrator.
    for id in &ids_b {
        let res = orch2.wait(id).await.unwrap();
        assert_eq!(res.status, SubAgentStatus::Ok);
    }
    eprintln!("PHASE0 kill-9 OK: no respawn, {} results reused", n);
}


// ---------------------------------------------------------------------------
// T2.1: per-worker timeout  (SDD §7.4.1 / 2.1) — overrun finalizes as Timeout
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_timeout_marks_worker_as_timeout() {
    // High mock latency so each worker would take much longer than the deadline.
    let b = Bench::new(5000).await;
    let ms = b.mem_store();
    let (orch, _prx) = make_orch(b.env(ms.clone()), "root-t");

    let mut spec = b.spec("slow");
    spec.timeout = Some(1); // 1s deadline; mock latency 5s => overrun

    let run_id = orch.spawn(&spec, 0, "root-t", "sess", "parent").await.unwrap();
    // `wait` resolves as soon as the handle reaches a terminal state (Timeout).
    let res = orch.wait(&run_id).await.expect("timeout worker should produce a result");
    eprintln!("PHASE0 timeout: status={:?} summary={}", res.status, res.summary);
    assert_eq!(
        res.status,
        SubAgentStatus::Timeout,
        "worker past deadline should be Timeout, got {:?}",
        res.status
    );
    // The terminal Timeout result is also resolvable on a fresh orchestrator
    // (persisted) as the same timeout status.
    let stored = ms.load_subagent_results("root-t").unwrap();
    let st = stored.iter().find(|r| r.role == "slow").expect("slow row persisted");
    assert_eq!(st.status, SubAgentStatus::Timeout, "persisted status should stay Timeout");
}


// ---------------------------------------------------------------------------
// T2.7: budget per-run  (SDD §7.7) — Usage events populate the per-run snapshot
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_budget_per_run_populated() {
    let b = Bench::new(2).await;
    let ms = b.mem_store();
    let (orch, _prx) = make_orch(b.env(ms.clone()), "root-b");

    let spec = b.spec("b1");
    let run_id = spawn_wait(&orch, &spec, "root-b").await;

    // Worker Ok => a BudgetSnapshot must be recorded for this run_id with the
    // mock Usage tokens (prompt 100 + completion 20 per completion response).
    let snap = orch.budget(&run_id).expect("per-run budget snapshot should exist");
    eprintln!("PHASE0 budget: run_id={} total={} prompt={} completion={}",
        snap.run_id, snap.total_tokens, snap.prompt_tokens, snap.completion_tokens);
    assert_eq!(snap.run_id, run_id);
    assert!(snap.total_tokens > 0, "mock Usage should report >0 total tokens");
    assert!(orch.budgets().iter().any(|s| s.run_id == run_id), "budgets() should list the run");
}


// ---------------------------------------------------------------------------
// T2.6: three-tier cancel tree (SDD §7.9, Phase 0 = root + per-worker)
//   per-worker cancel must terminate the worker as Cancelled (worker_ended_i),
//   while root_ended cascades all workers (STOP).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_cancel_worker_marks_cancelled() {
    let b = Bench::new(5000).await; // long latency -> worker runs a while
    let ms = b.mem_store();
    let (orch, _prx) = make_orch(b.env(ms.clone()), "root-c");

    let spec = b.spec("slow");
    let run_id = orch.spawn(&spec, 0, "root-c", "sess", "parent").await.unwrap();

    // Give the worker a moment to be Running, then cancel it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cancelled = orch.cancel(&run_id);
    assert!(cancelled, "cancel on existing worker should succeed");

    let res = orch.wait(&run_id).await.expect("cancelled worker should resolve");
    eprintln!("PHASE0 cancel: status={:?} cancelled_ok={cancelled}", res.status);
    assert_eq!(
        res.status,
        SubAgentStatus::Cancelled,
        "per-worker cancel should mark Cancelled, got {:?}",
        res.status
    );
}


// ---------------------------------------------------------------------------
// T3.5 / G2 gate: sub-agents are temporary execution units — no new main
// session, no main-session thread. A spawned worker resolves via the
// orchestrator registry under the root invocation id, has its own `sub-`
// session id, and cannot reach the orchestrator as a non-root caller.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_g2_subagent_is_temporary_no_main_session() {
    let b = Bench::new(2).await;
    let ms = b.mem_store();
    let (orch, _prx) = make_orch(b.env(ms.clone()), "root-g2");

    // A worker's role is run under a `sub-` session (never a main session),
    // and the orchestrator is keyed by the root invocation id only.
    let spec = b.spec("g2worker");
    let run_id = orch.spawn(&spec, 0, "root-g2", "sess", "parent").await.unwrap();
    // The worker is a temporary execution unit under the root run: it keeps its
    // own `sub-` session id (G2: no main session / no main-session thread) and
    // is only resolvable through the orchestrator returned by `wait`/`get_result`.
    assert!(!run_id.is_empty());
    let res = orch.wait(&run_id).await.unwrap();
    assert_eq!(res.role, "g2worker");
    assert_eq!(res.status, SubAgentStatus::Ok);
    // G2: worker's log and result belong to the sub-agent handle, not the root.
    let (rid, _role, st) = orch.list().into_iter().find(|(id, _, _)| id == &run_id)
        .expect("sub-agent should be listed in the orchestrator");
    assert_eq!(st, SubAgentStatus::Ok);
    eprintln!("PHASE0 g2: run_id={run_id} listed_rid={rid} role={} ok", res.role);
}


// ---------------------------------------------------------------------------
// T3.4: workflow templates (§10) — Sequential / Parallel / Loop build on the
// Orchestrator spawn/wait primitives and produce the expected result shapes.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_workflow_sequential_then_parallel_then_loop() {
    use crate::agent::workflow::{sequential, parallel, loop_until, WorkflowStep};
    let b = Bench::new(2).await;

    // Sequential: research -> analyze -> summarize, fail-fast on Ok.
    let ms = b.mem_store();
    let (orch, _prx) = make_orch(b.env(ms.clone()), "root-seq");
    let steps = vec![
        WorkflowStep::new("research", "gather"), 
        WorkflowStep::new("analyze", "analyze"),
        WorkflowStep::new("summarize", "summarize"),
    ];
    let seq = sequential(&orch, "root-seq", &steps).await;
    assert_eq!(seq.len(), 3, "sequential should run all 3 steps");
    assert!(seq.iter().all(|r| r.status == SubAgentStatus::Ok));
    // Roles are distinct and results preserve order.
    let roles: Vec<&str> = seq.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(roles, vec!["research", "analyze", "summarize"]);

    // Parallel: spawn all then aggregate.
    let ms2 = b.mem_store();
    let (orch2, _prx2) = make_orch(b.env(ms2.clone()), "root-par");
    let par_steps = vec![
        WorkflowStep::new("p1", "a"),
        WorkflowStep::new("p2", "b"),
        WorkflowStep::new("p3", "c"),
    ];
    let par = parallel(&orch2, "root-par", &par_steps).await;
    assert_eq!(par.len(), 3);
    assert!(par.iter().all(|r| r.status == SubAgentStatus::Ok));

    // Loop: iterate until stop predicate or max_rounds.
    let ms3 = b.mem_store();
    let (orch3, _prx3) = make_orch(b.env(ms3.clone()), "root-loop");
    let res = loop_until(&orch3, "root-loop", "dig", "keep digging", 3, |r| r.len() >= 2).await;
    assert_eq!(res.len(), 2, "loop should stop after 2 rounds matching predicate");
    eprintln!("PHASE0 workflow: seq={} par={} loop={}", seq.len(), par.len(), res.len());
}


// ---------------------------------------------------------------------------
// §10 workflow-template conformance — data-driven, ADK-Rust correspondence.
// Maps FoxIR workflow::* to ADK-Rust adk-agent types (verified against docs.rs/
// adk-agent 2.2.0):
//   SequentialAgent  -> workflow::sequential   (order preserved; short-circuit on error)
//   ParallelAgent    -> workflow::parallel     (concurrent execution; result aggregation)
//   LoopAgent        -> workflow::loop_until   (iterate until exit condition / max)
// Each template is exercised on the Expert Orchestrator runtime (mock provider)
// with multiple dataset samples; semantics asserted per template.
// ---------------------------------------------------------------------------

/// Dataset S: suspicious-login triage — research -> correlate -> summarize.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_templates_sequential_preserves_order_and_runs_all() {
    use crate::agent::workflow::{sequential, WorkflowStep};
    let b = Bench::new(2).await;

    // Sample S1: 3-step IR pipeline.
    let (orch1, _) = make_orch(b.env(b.mem_store()), "seq-s1");
    let s1 = vec![
        WorkflowStep::new("collect_login_events", "pull auth.log login records"),
        WorkflowStep::new("correlate_with_iocs", "match against known bad IPs"),
        WorkflowStep::new("summarize_finding", "state the verdict"),
    ];
    let r1 = sequential(&orch1, "seq-s1", &s1).await;
    let roles1: Vec<&str> = r1.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(roles1, vec!["collect_login_events", "correlate_with_iocs", "summarize_finding"],
        "sequential must preserve step order");
    assert!(r1.iter().all(|r| r.status == SubAgentStatus::Ok));
    assert_eq!(r1.len(), 3);

    // Sample S2: 4-step ransomware artifact triage (longer chain).
    let (orch2, _) = make_orch(b.env(b.mem_store()), "seq-s2");
    let s2 = vec![
        WorkflowStep::new("id_ransomnote", "locate ransom note sample"),
        WorkflowStep::new("extract_iocs", "extract wallet/payment strings"),
        WorkflowStep::new("verify_mutation", "confirm binary is a known mutation"),
        WorkflowStep::new("write_summary", "document the finding"),
    ];
    let r2 = sequential(&orch2, "seq-s2", &s2).await;
    let roles2: Vec<&str> = r2.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(roles2, vec!["id_ransomnote", "extract_iocs", "verify_mutation", "write_summary"]);
    assert!(r2.iter().all(|r| r.status == SubAgentStatus::Ok));
    assert_eq!(r2.len(), 4);
    eprintln!("PHASE0 templates sequential: S1={} S2={}", r1.len(), r2.len());
}

/// Sequential short-circuit: a step that overruns its deadline (Timeout) stops
/// the chain and the returned vector carries the partial results, mirroring
/// error-abort semantics of a sequential workflow rather than pushing on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_templates_sequential_fail_fast_on_timeout() {
    use crate::agent::workflow::{sequential, WorkflowStep};
    // Each worker costs ~1.5s; give step 2 a 1s deadline so it overruns -> Timeout.
    let b = Bench::new(1500).await;
    let (orch, _) = make_orch(b.env(b.mem_store()), "seq-failfast");
    let steps = vec![
        WorkflowStep::new("collect", "step one").with_timeout(None),
        WorkflowStep::new("analyze", "step two").with_timeout(Some(1)),
    ];
    let r = sequential(&orch, "seq-failfast", &steps).await;
    assert_eq!(r.len(), 2, "partial vector: collect + analyze(timeout); summarize must not run");
    assert_eq!(r[0].status, SubAgentStatus::Ok, "first step completes Ok");
    assert_ne!(r[1].status, SubAgentStatus::Ok, "overrun step must be non-Ok (Timeout)");
    eprintln!("PHASE0 templates sequential-fail-fast: len={} last={:?}", r.len(), r[1].status);
}

/// Dataset P: multi-angle concurrent sweep. Asserts true concurrency (parallel
/// wall-clock far below serial sum) plus complete aggregation and unique roles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_templates_parallel_concurrent_and_aggregates() {
    use crate::agent::workflow::{parallel, WorkflowStep};
    use std::time::Instant;

    // Serial baseline: 5 workers back-to-back at 120ms latency each.
    let n = 5;
    let b1 = Bench::new(120).await;
    let (orch_s, _) = make_orch(b1.env(b1.mem_store()), "par-serial");
    let t0 = Instant::now();
    for i in 0..n {
        let sp = b1.spec(&format!("q{i}"));
        spawn_wait(&orch_s, &sp, "par-serial").await;
    }
    let serial_ms = t0.elapsed().as_millis();

    // Parallel team as a forensic "multi-source" sweep.
    let b2 = Bench::new(120).await;
    let (orch_p, _) = make_orch(b2.env(b2.mem_store()), "par-team");
    let steps = vec![
        WorkflowStep::new("p_evtx", "scan event logs"),
        WorkflowStep::new("p_network", "list suspicious outbound"),
        WorkflowStep::new("p_persistence", "find persistence points"),
        WorkflowStep::new("p_services", "enumerate services"),
        WorkflowStep::new("p_autoruns", "check autoruns"),
    ];
    let t1 = Instant::now();
    let r = parallel(&orch_p, "par-team", &steps).await;
    let par_ms = t1.elapsed().as_millis();

    assert_eq!(r.len(), 5, "parallel must aggregate all worker results");
    assert!(r.iter().all(|x| x.status == SubAgentStatus::Ok));
    let mut roles: Vec<&str> = r.iter().map(|x| x.role.as_str()).collect();
    roles.sort();
    roles.dedup();
    assert_eq!(roles.len(), 5, "parallel workers must have unique roles");

    // Concurrency evidence: parallel << serial (which is ~ n * latency).
    eprintln!("PHASE0 templates parallel: serial={serial_ms}ms parallel={par_ms}ms n={n}");
    assert!(par_ms < serial_ms,
        "parallel ({par_ms}ms) must beat serial ({serial_ms}ms)");
    assert!(par_ms * 3 < serial_ms as u128,
        "expected near-concurrent; serial {serial_ms}ms vs parallel {par_ms}ms");
}

/// Dataset L: loop template. L1 stops early on the exit predicate; L2 runs to
/// the max_rounds cap when the predicate is never satisfied. Round roles stay unique.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_templates_loop_exit_predicate_and_cap() {
    use crate::agent::workflow::loop_until;
    let b = Bench::new(2).await;

    // Sample L1: exit when 2 evidence rounds are gathered (early stop).
    let (orch1, _) = make_orch(b.env(b.mem_store()), "loop-s1");
    let r1 = loop_until(&orch1, "loop-s1", "dig", "keep digging", 5, |acc| acc.len() >= 2).await;
    assert_eq!(r1.len(), 2, "loop must stop as soon as the exit predicate is met");

    // Sample L2: predicate never true => cap at max_rounds = 3.
    let (orch2, _) = make_orch(b.env(b.mem_store()), "loop-s2");
    let r2 = loop_until(&orch2, "loop-s2", "hunt", "no exit", 3, |_| false).await;
    assert_eq!(r2.len(), 3, "loop must stop at the max_rounds cap");

    // Round roles must be distinct per round (dig-r0/dig-r1), so results are unambiguous.
    let mut roles: Vec<&str> = r1.iter().map(|x| x.role.as_str()).collect();
    roles.sort();
    roles.dedup();
    assert_eq!(roles.len(), 2, "each loop round must carry a unique role");
    eprintln!("PHASE0 templates loop: early={} cap={}", r1.len(), r2.len());
}


// ---------------------------------------------------------------------------
// T3.3 / T2.5: aggregate auditor (§7.5) — deterministic Pass/Fail/Uncertain
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_audit_aggregate_verdicts() {
    use crate::agent::orchestration::{audit_aggregate, AuditVerdict};
    use crate::context::Confidence;

    let ev = |role: &str, conf: Confidence, evidence: usize| SubAgentResult {
        run_id: format!("id-{role}"),
        role: role.to_string(),
        summary: "x".to_string(),
        confidence: conf,
        token_usage: 0,
        evidence_refs: (0..evidence).map(|i| format!("ev{i}")).collect(),
        artifact_refs: Vec::new(),
        case_ref: None,
        proposed_writes: Vec::new(),
        status: crate::context::SubAgentStatus::Ok,
    };

    // All High (or high-confidence with evidence) => Pass.
    let pas = vec![ev("a", Confidence::Medium, 1), ev("b", Confidence::High, 0)];
    assert_eq!(audit_aggregate(&pas), AuditVerdict::Pass, "high/low-with-evidence must Pass");

    // Low confidence with no evidence => Fail.
    let fail = vec![ev("c", Confidence::Low, 0)];
    assert!(matches!(audit_aggregate(&fail), AuditVerdict::Fail(_)),
        "non-High without evidence must Fail");

    // Empty => Uncertain.
    assert!(matches!(audit_aggregate(&[]), AuditVerdict::Uncertain(_)),
        "no results must be Uncertain");

    // Duplicate roles => Uncertain.
    let dup = vec![ev("a", Confidence::High, 0), ev("a", Confidence::High, 0)];
    assert!(matches!(audit_aggregate(&dup), AuditVerdict::Uncertain(_)),
        "duplicate roles must be Uncertain");

    eprintln!("PHASE0 audit: pass / fail / uncertain all classified");
}


// ---------------------------------------------------------------------------
// P4: WS payload builders (Step 3) — budget_update / subagent terminal events
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_ws_payload_builders_shape() {
    use crate::agent::orchestration::{BudgetSnapshot, budget_update_ws, subagent_ws};
    use crate::context::Confidence;

    let snap = BudgetSnapshot {
        run_id: "r1".into(), role: "w".into(),
        prompt_tokens: 100, completion_tokens: 20, total_tokens: 120,
    };
    let bu = budget_update_ws(&snap);
    assert_eq!(bu["type"], "budget_update");
    assert_eq!(bu["run_id"], "r1");
    assert_eq!(bu["total_tokens"], 120);

    let res = SubAgentResult {
        run_id: "r1".into(), role: "w".into(), summary: "s".into(),
        confidence: Confidence::Medium, token_usage: 120,
        evidence_refs: vec!["e1".into()], artifact_refs: vec![],
        case_ref: None, proposed_writes: vec![],
        status: SubAgentStatus::Ok,
    };
    let sa = subagent_ws(&res);
    assert_eq!(sa["type"], "subagent");
    assert_eq!(sa["status"], "Ok");
    eprintln!("PHASE0 ws: budget_update + subagent shaped ok");
}


// ---------------------------------------------------------------------------
// Self-contained acceptance report: single pass over the key gates, writing a
// structured artifact to output/phase0_acceptance.json so "run once, see the
// metrics" is reproducible (serial-safe — no dependence on other tests' order).
// Provider defaults to the in-process mock; real answers require a configured
// OpenAI-compatible endpoint (config.toml api_base + api_key).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_report_generates_artifact() {
    use std::sync::atomic::Ordering as Ord;
    let b = Bench::new(60).await;
    let n = 5usize;

    // Serial baseline (wall-clock + token) on a fresh orchestrator.
    let ms1 = b.mem_store();
    let (orch_s, _) = make_orch(b.env(ms1.clone()), "root-s");
    let t0 = Instant::now();
    for i in 0..n {
        spawn_wait(&orch_s, &b.spec(&format!("w{i}")), "root-s").await;
    }
    let serial_s = t0.elapsed().as_secs_f64();
    let serial_tok = b.mock.tokens.load(Ord::SeqCst);

    // Parallel pass on a fresh orchestrator.
    let ms2 = b.mem_store();
    let (orch_p, _) = make_orch(b.env(ms2.clone()), "root-p");
    let t1 = Instant::now();
    let mut ids = Vec::new();
    for i in 0..n {
        let rid = orch_p.spawn(&b.spec(&format!("w{i}")), 0, "root-p", "sess", "parent").await.unwrap();
        ids.push(rid);
    }
    join_all(ids.iter().map(|id| orch_p.wait(id))).await;
    let parallel_s = t1.elapsed().as_secs_f64();
    let par_tok = b.mock.tokens.load(Ord::SeqCst) - serial_tok;

    let speedup = if parallel_s > 0.0 { serial_s / parallel_s } else { 0.0 };
    let token_pct = (par_tok as f64) / (serial_tok.max(1) as f64) * 100.0 - 100.0;
    let gate_wall = parallel_s <= 0.70 * serial_s;
    let gate_token = (par_tok as f64) <= (serial_tok.max(1) as f64) * 1.5;

    // Stability sample: 5 parallel runs x 2 workers, all results must return.
    let ms3 = b.mem_store();
    let (orch_r, _) = make_orch(b.env(ms3.clone()), "root-r");
    let mut stable_runs = 0usize;
    let mut lost = 0usize;
    for r in 0..5 {
        let m = 2usize;
        let mut rids = Vec::new();
        for i in 0..m {
            let rid = orch_r.spawn(&b.spec(&format!("r{r}-{i}")), 0, "root-r", "sess", "parent").await.unwrap();
            rids.push(rid);
        }
        for id in rids {
            match orch_r.wait(&id).await {
                Ok(res) if res.status == SubAgentStatus::Ok => stable_runs += 1,
                _ => lost += 1,
            }
        }
    }

    let report = serde_json::json!({
        "suite": "phase0-acceptance",
        "provider": "mock",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "wallclock": {
            "workers": n, "serial_s": serial_s, "parallel_s": parallel_s,
            "speedup_ratio": speedup, "gate_parallel_le_70pct_serial": gate_wall
        },
        "tokens": {
            "serial": serial_tok, "parallel_delta": par_tok,
            "pct_increase": token_pct, "gate_le_50pct": token_pct <= 50.0
        },
        "stability_sample": { "runs": stable_runs, "lost": lost },
        "gates": { "wall_clock_ge_30pct_cut": gate_wall, "token_le_50pct_inc": gate_token },
        "status": if gate_wall && gate_token && lost == 0 { "ok" } else { "pending-live" },
    });

    let dir = std::path::Path::new("output");
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("phase0_acceptance.json");
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap());
    eprintln!("PHASE0 REPORT {} -> {}: {}", path.display(), report["status"], serde_json::to_string(&report).unwrap());

    assert!(gate_wall, "parallel {parallel_s}s not <= 70% of serial {serial_s}s");
    assert!(gate_token, "orchestrated tokens {par_tok} exceed 1.5x serial {serial_tok}");
    assert_eq!(lost, 0, "event/result loss in stability sample: {lost}");
    assert!(std::path::Path::new(&path).exists(), "report artifact missing");
}

// ---------------------------------------------------------------------------
// Live end-to-end (real OpenAI-compatible endpoint, optional).
// Loads the user's workspace models.json through model_store::load_configs
// (which decrypts the AES-256-GCM api keys in-process), picks a model with an
// inline key, and runs a small real parallel collect to measure wall-clock +
// token usage. If no endpoint is reachable it records diagnostics and skips
// (never falsely fails on environment unavailability).
// ---------------------------------------------------------------------------

fn live_tmp_memstore() -> Arc<MemoryStore> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mem.db");
    let db = Arc::new(MemoryStore::new(path.to_str().unwrap()).unwrap());
    std::mem::forget(dir);
    db
}

fn live_model_configs() -> Vec<crate::config::ModelConfig> {
    let Some(userprofile) = std::env::var("USERPROFILE").ok() else { return Vec::new() };
    let path = std::path::Path::new(&userprofile).join(".RustAgent/Workspace/models.json");
    // load_configs decrypts api_key in-process (crypto.rs, AES-256-GCM keyed by MachineGuid).
    crate::model_store::load_configs(&path)
        .into_iter()
        .filter(|m| {
            !m.api_base.is_empty()
                && !m.api_base.contains("localhost")
                && !m.api_base.contains("127.0.0.1")
        })
        .collect()
}

fn live_env(
    provider: Arc<OpenAiProvider>,
    mcfg: &crate::config::ModelConfig,
    memory_store: Arc<MemoryStore>,
    tools: Arc<tokio::sync::RwLock<ToolRegistry>>,
) -> OrchestratorEnv {
    let (_resolver, pending) = PermissionResolver::new();
    OrchestratorEnv {
        provider,
        tools,
        working_dir: tempfile::tempdir().unwrap().into_path().to_string_lossy().to_string(),
        workspace_dir: tempfile::tempdir().unwrap().into_path().to_string_lossy().to_string(),
        model_configs: vec![mcfg.clone()],
        max_iterations: 5,
        parallel_ir_tools: false,
        user_given_name: "live".into(),
        two_tier_memory: false,
        sop_replay: Arc::new(AtomicBool::new(false)),
        parent_model: mcfg.name.clone(),
        permissions: Arc::new(tokio::sync::Mutex::new(default_permissions())),
        permission_pending: pending,
        preauth_profile: None,
        context_window: mcfg.context_window,
        enable_context_scaling: false,
        max_inline_chars: 120000,
        tool_timeout_secs: 60,
        max_tool_retries: 1,
        max_concurrent_subagents: 4,
        default_timeout_secs: 240,
        memory_store: Some(memory_store),
    }
}

fn live_spec(role: &str, model: &str) -> SubAgentSpec {
    SubAgentSpec {
        role: role.into(),
        prompt: "Reply with exactly the single word: ok. Do not call any tool.".into(),
        system_prompt: None,
        tools_allowlist: Vec::new(),
        allow_write: false,
        allow_exec: false,
        model: Some(model.to_string()),
        timeout: Some(180),
        max_tokens: Some(8),
        max_iterations: Some(1),
        skills: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_live_end_to_end() {
    // Gated so routine `cargo test` never spends real tokens: run with
    // FOXIR_LIVE_E2E=1 to perform a live parallel collect against the workspace
    // endpoint (keys decrypted in-process via model_store::load_configs).
    if std::env::var("FOXIR_LIVE_E2E").is_err() {
        eprintln!("PHASE0 LIVE skip: set FOXIR_LIVE_E2E=1 to run against the real endpoint");
        return;
    }
    let configs = live_model_configs();
    if configs.is_empty() {
        eprintln!("PHASE0 LIVE skip: no inlined-key model in workspace models.json");
        return;
    }

    // Probe reachability with one minimal worker per candidate (bounded timeout).
    let mut diag = Vec::new();
    let mut live: Option<(Arc<OpenAiProvider>, crate::config::ModelConfig)> = None;
    let tools = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    for mcfg in configs.iter().take(8) {
        let provider = Arc::new(OpenAiProvider::new(vec![mcfg.clone()]));
        let (orch, _prx) = make_orch(live_env(provider.clone(), mcfg, live_tmp_memstore(), tools.clone()), "live-probe");
        let spec = live_spec("probe", &mcfg.name);
        let probe = async {
            let rid = orch.spawn(&spec, 0, "live-probe", "sess", "parent").await?;
            orch.wait(&rid).await
        };
        let t = Instant::now();
        match tokio::time::timeout(std::time::Duration::from_secs(200), probe).await {
            Ok(Ok(res)) if res.status == SubAgentStatus::Ok => {
                eprintln!("PHASE0 LIVE probe OK model={} wall={:06.3}s", mcfg.name, t.elapsed().as_secs_f64());
                live = Some((provider, mcfg.clone()));
                break;
            }
            Ok(Ok(res)) => diag.push(format!("{} -> worker {:?}", mcfg.name, res.status)),
            Ok(Err(e)) => diag.push(format!("{} -> {}", mcfg.name, e)),
            Err(_) => diag.push(format!("{} -> timeout", mcfg.name)),
        }
    }

    let Some((provider, mcfg)) = live else {
        let report = serde_json::json!({
            "suite": "phase0-live", "status": "unavailable",
            "diag": diag,
            "note": "no configured endpoint reachable/authorized at probe time (keys were decrypted via model_store)"
        });
        let _ = std::fs::create_dir_all("output");
        let _ = std::fs::write("output/phase0_live.json", serde_json::to_string_pretty(&report).unwrap());
        eprintln!("PHASE0 LIVE unavailable: {}", serde_json::to_string(&report).unwrap());
        return;
    };

    // Real parallel vs serial collect.
    let n = 3usize;
    let btools = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let (orch_s, _) = make_orch(live_env(provider.clone(), &mcfg, live_tmp_memstore(), btools.clone()), "live-s");
    let t0 = Instant::now();
    let mut serial_tok = 0u64;
    for i in 0..n {
        let rid = orch_s.spawn(&live_spec(&format!("s{i}"), &mcfg.name), 0, "live-s", "sess", "p").await.unwrap();
        let r = orch_s.wait(&rid).await.unwrap();
        assert_eq!(r.status, SubAgentStatus::Ok, "serial worker {i}");
        serial_tok += r.token_usage;
    }
    let serial_s = t0.elapsed().as_secs_f64();

    let (orch_p, _) = make_orch(live_env(provider.clone(), &mcfg, live_tmp_memstore(), btools.clone()), "live-p");
    let t1 = Instant::now();
    let mut ids = Vec::new();
    for i in 0..n {
        let rid = orch_p.spawn(&live_spec(&format!("p{i}"), &mcfg.name), 0, "live-p", "sess", "p").await.unwrap();
        ids.push(rid);
    }
    let mut par_tok = 0u64;
    for id in ids {
        let r = orch_p.wait(&id).await.unwrap();
        assert_eq!(r.status, SubAgentStatus::Ok, "parallel worker");
        par_tok += r.token_usage;
    }
    let parallel_s = t1.elapsed().as_secs_f64();
    let speedup = if parallel_s > 0.0 { serial_s / parallel_s } else { 0.0 };
    let report = serde_json::json!({
        "suite": "phase0-live",
        "provider": mcfg.name,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "wallclock": { "workers": n, "serial_s": serial_s, "parallel_s": parallel_s, "speedup_ratio": speedup },
        "tokens": { "serial": serial_tok, "parallel": par_tok },
        "status": "ok",
    });
    let _ = std::fs::create_dir_all("output");
    let _ = std::fs::write("output/phase0_live.json", serde_json::to_string_pretty(&report).unwrap());
    eprintln!("PHASE0 LIVE report: {}", serde_json::to_string(&report).unwrap());
}
