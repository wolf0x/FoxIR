//! Deterministic orchestrator self-test (expert multi-agent template verification).
//!
//! Independent of the Manager LLM: drives the Orchestrator through the declared
//! workflow templates with the real configured model so sub-agent spawning is
//! guaranteed and observable. Invoked via `FoxIR.exe --orch-self-test
//! <sequential|parallel|loop> [n]`.

use crate::config::{OrchestrationLimits, OrchestrationTemplate};
use crate::managed::manager::ParallelSubtask;
use crate::managed::parallel::{ParallelEnv, run_loop_collect, run_template_collect};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    workspace_dir: &str,
    template: OrchestrationTemplate,
    n: usize,
    provider: std::sync::Arc<crate::model::openai::OpenAiProvider>,
    tools: std::sync::Arc<tokio::sync::RwLock<crate::tool::ToolRegistry>>,
    model: &str,
) -> Result<(), String> {
    let db_path = std::path::Path::new(workspace_dir).join("memory").join("memory.db");
    let ms = std::sync::Arc::new(
        crate::memory::MemoryStore::new(db_path.to_str().unwrap()).map_err(|e| e.to_string())?,
    );
    let env = ParallelEnv {
        provider: provider.clone(),
        tools: tools.clone(),
        working_dir: workspace_dir.to_string(),
        workspace_dir: workspace_dir.to_string(),
        model: model.to_string(),
        max_iterations: 8,
        context_window: 128000,
        max_inline_chars: 120000,
        tool_timeout_secs: 60,
        max_tool_retries: 1,
        two_tier_memory: false,
        limits: OrchestrationLimits::default(),
        memory_store: Some(ms),
        permissions: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::permission::default_permissions(),
        )),
        permission_pending: crate::permission::PermissionResolver::new().1,
        preauth_profile: None,
    };

    let root_id = format!("selftest-{}", template.as_str());
    let results = run_template_collect(&env, &subtasks(n, template.as_str()), template, &root_id, "selftest").await;
    emit(&template, &results, n, &root_id);
    Ok(())
}

/// Loop variant: bounded deep-dive rounds on a single role.
pub async fn run_loop(
    workspace_dir: &str,
    role: &str,
    rounds: usize,
    provider: std::sync::Arc<crate::model::openai::OpenAiProvider>,
    tools: std::sync::Arc<tokio::sync::RwLock<crate::tool::ToolRegistry>>,
    model: &str,
) -> Result<(), String> {
    let db_path = std::path::Path::new(workspace_dir).join("memory").join("memory.db");
    let ms = std::sync::Arc::new(
        crate::memory::MemoryStore::new(db_path.to_str().unwrap()).map_err(|e| e.to_string())?,
    );
    let env = ParallelEnv {
        provider: provider.clone(),
        tools: tools.clone(),
        working_dir: workspace_dir.to_string(),
        workspace_dir: workspace_dir.to_string(),
        model: model.to_string(),
        max_iterations: 8,
        context_window: 128000,
        max_inline_chars: 120000,
        tool_timeout_secs: 60,
        max_tool_retries: 1,
        two_tier_memory: false,
        limits: OrchestrationLimits::default(),
        memory_store: Some(ms),
        permissions: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::permission::default_permissions(),
        )),
        permission_pending: crate::permission::PermissionResolver::new().1,
        preauth_profile: None,
    };
    let root_id = format!("selftest-loop-{}", role);
    let results = run_loop_collect(&env, role, "continue bounded read-only deep-dive", rounds, &root_id, "selftest").await;
    emit(&OrchestrationTemplate::Parallel, &results, rounds, &root_id);
    Ok(())
}

fn subtasks(n: usize, tmpl: &str) -> Vec<ParallelSubtask> {
    let prefixes = ["port_scan", "log_parse", "persistence", "network", "process"];
    (0..n).map(|i| {
        let role = match prefixes.get(i) {
            Some(p) => format!("{}_{}", p, i + 1),
            None => format!("worker_{}", i + 1),
        };
        ParallelSubtask {
            role: role.clone(),
            task: format!(
                "Read-only {}: inspect role '{}' and report concise findings (no writes, no containment).",
                tmpl, role
            ),
        }
    }).collect()
}

fn emit(template: &OrchestrationTemplate, results: &[crate::context::SubAgentResult], n: usize, root: &str) {
    println!();
    println!("[orch-self-test] template={} requested_workers={} root_run={}", template.as_str(), n, root);
    for r in results {
        let summary: String = r.summary.chars().take(220).collect();
        println!(
            "  worker [{}] run_id={} status={:?} conf={:?} evid={} summary={}",
            r.role, r.run_id, r.status, r.confidence, r.evidence_refs.len(), summary
        );
    }
    let ok = results.iter().filter(|r| r.status == crate::context::SubAgentStatus::Ok).count();
    println!("[orch-self-test] completed={}/{} workers total={}", ok, n, results.len());
}
