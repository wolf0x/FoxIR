//! Orchestration tools (SDD v1.5 \u00a77.3 / Step 2a).
//!
//! These 7 tools hand the manager agent real sub-agent control: spawn a read-only
//! worker, wait for/collect its result, list/cancel children, persist the shared
//! plan, and read a worker's log. They resolve the current run's [`Orchestrator`]
//! from the process-global registry keyed by the root invocation id. Workers have
//! `can_spawn = false` and a different invocation id, so they can never reach the
//! orchestrator (no deep nesting in Phase 0).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolRegistry};
use crate::agent::orchestration::get_orchestrator;
use crate::context::{SubAgentSpec, ToolContext};
use crate::error::{AgentError, AgentResult};

/// The seven orchestration tool names (must match `ALL_ORCH` in llm_agent.rs).
pub const ORCH_TOOL_NAMES: [&str; 7] = [
    "spawn_subagent",
    "wait_subagent",
    "list_subagents",
    "cancel_subagent",
    "get_subagent_result",
    "update_plan",
    "read_subagent_log",
];

/// Register all orchestration tools.
pub fn register_orchestration_tools(registry: &mut ToolRegistry) {
    registry.register(Arc::new(SpawnSubagentTool));
    registry.register(Arc::new(WaitSubagentTool));
    registry.register(Arc::new(ListSubagentsTool));
    registry.register(Arc::new(CancelSubagentTool));
    registry.register(Arc::new(GetSubagentResultTool));
    registry.register(Arc::new(UpdatePlanTool));
    registry.register(Arc::new(ReadSubagentLogTool));
}

/// Execution-time double gate (SDD v1.5 §7.3): every orchestration tool must
/// refuse non-spawning contexts and any depth >= 1 caller at its first line,
/// independently of the orchestrator-registry fallback in `orch()`.
fn gate(ctx: &ToolContext) -> AgentResult<()> {
    if !ctx.can_spawn {
        return Err(AgentError::not_available_in_mode(ctx.mode));
    }
    if ctx.depth >= 1 {
        return Err(AgentError::depth_limit());
    }
    Ok(())
}

fn orch(ctx: &ToolContext) -> AgentResult<Arc<crate::agent::orchestration::Orchestrator>> {
    get_orchestrator(&ctx.base.base.invocation_id)
        .ok_or_else(|| AgentError::not_available_in_mode(ctx.mode))
}

pub struct SpawnSubagentTool;
#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str { "spawn_subagent" }
    fn description(&self) -> &str {
        "Spawn a read-only worker sub-agent that runs a delegated task in its own session and returns a structured SubAgentResult. Call with {role, prompt, [tools_allowlist], [allow_write], [allow_exec], [model], [max_iterations], [timeout]}. Then wait_subagent for the result."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {
            "role": { "type": "string" },
            "prompt": { "type": "string" },
            "tools_allowlist": { "type": "array", "items": { "type": "string" } },
            "allow_write": { "type": "boolean" },
            "allow_exec": { "type": "boolean" },
            "model": { "type": "string" },
            "max_iterations": { "type": "integer" },
            "timeout": { "type": "integer", "description": "Per-worker wall-clock timeout in seconds (default 300)" }
        }, "required": ["role", "prompt"] })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        gate(ctx)?;
        let o = orch(ctx)?;
        let spec: SubAgentSpec = serde_json::from_value(args)
            .map_err(|e| AgentError::agent(format!("spawn_subagent: bad args: {e}")))?;
        // P4 (§6.4): read-only workers are admitted with no extra gate; write/exec
        // workers (allow_write || allow_exec) require explicit user authorization
        // (or a matching pre-authorization profile) before spawning. Denied =>
        // structured rejection (no panic), suggesting a read-only re-spawn.
        if spec.allow_write || spec.allow_exec {
            if !o.request_spawn_authorization(&spec).await {
                return Ok(json!({
                    "error": "Write/exec worker authorization denied by user",
                    "status": "rejected",
                    "suggestion": "Re-spawn with allow_write=false for read-only access"
                }));
            }
        }
        // Step 2b: allow_write / allow_exec workers are admitted (§7.10 layer 2)
        // and forced serial by the Orchestrator's write_gate.
        let run_id = o.spawn(&spec, ctx.depth, &ctx.base.base.invocation_id, &ctx.base.base.session_id, &ctx.base.base.agent_name).await?;
        Ok(json!({ "run_id": run_id, "status": "spawned" }))
    }
}

pub struct WaitSubagentTool;
#[async_trait]
impl Tool for WaitSubagentTool {
    fn name(&self) -> &str { "wait_subagent" }
    fn description(&self) -> &str { "Block until a previously spawned sub-agent reaches a terminal state and return its full SubAgentResult. Args: {run_id}." }
    fn parameters_schema(&self) -> Value { json!({ "type": "object", "properties": { "run_id": { "type": "string" } }, "required": ["run_id"] }) }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        gate(ctx)?;
        let o = orch(ctx)?;
        let run_id = args.get("run_id").and_then(|v| v.as_str()).ok_or_else(|| AgentError::agent("wait_subagent: missing run_id"))?;
        let r = o.wait(run_id).await?;
        Ok(serde_json::to_value(r).unwrap_or(json!({})))
    }
}

pub struct ListSubagentsTool;
#[async_trait]
impl Tool for ListSubagentsTool {
    fn name(&self) -> &str { "list_subagents" }
    fn description(&self) -> &str { "List all sub-agents spawned by this run with their role and status." }
    fn parameters_schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        gate(ctx)?;
        let o = orch(ctx)?;
        let items: Vec<Value> = o.list().into_iter()
            .map(|(rid, role, st)| json!({ "run_id": rid, "role": role, "status": format!("{:?}", st) }))
            .collect();
        Ok(json!({ "subagents": items }))
    }
}

pub struct CancelSubagentTool;
#[async_trait]
impl Tool for CancelSubagentTool {
    fn name(&self) -> &str { "cancel_subagent" }
    fn description(&self) -> &str { "Request cancellation of a running sub-agent. Args: {run_id}." }
    fn parameters_schema(&self) -> Value { json!({ "type": "object", "properties": { "run_id": { "type": "string" } }, "required": ["run_id"] }) }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        gate(ctx)?;
        let o = orch(ctx)?;
        let run_id = args.get("run_id").and_then(|v| v.as_str()).ok_or_else(|| AgentError::agent("cancel_subagent: missing run_id"))?;
        Ok(json!({ "cancelled": o.cancel(run_id) }))
    }
}

pub struct GetSubagentResultTool;
#[async_trait]
impl Tool for GetSubagentResultTool {
    fn name(&self) -> &str { "get_subagent_result" }
    fn description(&self) -> &str { "Non-blocking fetch of a finished sub-agent's result. Args: {run_id}." }
    fn parameters_schema(&self) -> Value { json!({ "type": "object", "properties": { "run_id": { "type": "string" } }, "required": ["run_id"] }) }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        gate(ctx)?;
        let o = orch(ctx)?;
        let run_id = args.get("run_id").and_then(|v| v.as_str()).ok_or_else(|| AgentError::agent("get_subagent_result: missing run_id"))?;
        match o.get_result(run_id) {
            Some(r) => Ok(serde_json::to_value(r).unwrap_or(json!({}))),
            None => Ok(json!({ "run_id": run_id, "status": "not_finished" })),
        }
    }
}

pub struct UpdatePlanTool;
#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &str { "update_plan" }
    fn description(&self) -> &str { "Publish or update the shared execution plan for this run. Args: {plan: <any>}." }
    fn parameters_schema(&self) -> Value { json!({ "type": "object", "properties": { "plan": {} }, "required": ["plan"] }) }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        gate(ctx)?;
        let o = orch(ctx)?;
        o.update_plan(args.get("plan").cloned().unwrap_or(json!(null)));
        Ok(json!({ "ok": true }))
    }
}

pub struct ReadSubagentLogTool;
#[async_trait]
impl Tool for ReadSubagentLogTool {
    fn name(&self) -> &str { "read_subagent_log" }
    fn description(&self) -> &str { "Read the captured log lines of a sub-agent. Args: {run_id}." }
    fn parameters_schema(&self) -> Value { json!({ "type": "object", "properties": { "run_id": { "type": "string" } }, "required": ["run_id"] }) }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        gate(ctx)?;
        let o = orch(ctx)?;
        let run_id = args.get("run_id").and_then(|v| v.as_str()).ok_or_else(|| AgentError::agent("read_subagent_log: missing run_id"))?;
        Ok(json!({ "run_id": run_id, "log": o.read_log(run_id) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_orchestration_tools_register_under_all_orch_names() {
        let mut reg = ToolRegistry::new();
        register_orchestration_tools(&mut reg);
        for n in ORCH_TOOL_NAMES {
            assert!(reg.get(n).is_some(), "missing orchestration tool {}", n);
        }
        assert_eq!(reg.len(), ORCH_TOOL_NAMES.len());
    }

    /// §7.3 double gate: even with `can_spawn = true`, a depth >= 1 caller must
    /// be refused with the depth-limit error at the tool's first line.
    #[tokio::test]
    async fn orchestration_tools_refuse_depth_one_even_with_can_spawn() {
        let mut ctx = crate::context::ToolContext::simple(".".to_string(), ".".to_string());
        ctx.can_spawn = true;
        ctx.depth = 1;
        let err = ListSubagentsTool.execute(json!({}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("depth"), "got {err}");
        // And a plain non-spawning (Instant) context is refused with the mode error.
        let ctx2 = crate::context::ToolContext::simple(".".to_string(), ".".to_string());
        let err2 = ListSubagentsTool.execute(json!({}), &ctx2).await.unwrap_err();
        assert!(err2.to_string().contains("unavailable"), "got {err2}");
    }

    /// Without a registered orchestrator for this invocation, orchestration
    /// tools must refuse with `not_available_in_mode` (G-sub-not-main guard).
    #[tokio::test]
    async fn orchestration_tools_unavailable_without_orchestrator() {
        let ctx = crate::context::ToolContext::simple(".".to_string(), ".".to_string());
        let tool = SpawnSubagentTool;
        let err = tool.execute(json!({"role": "worker", "prompt": "x"}), &ctx).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unavailable") || msg.contains("not available") || msg.contains("mode"), "got {msg}");
    }

    /// A schema-compliant call with only `role`/`prompt` must deserialize into
    /// a read-only worker instead of failing with "missing field" (the P0 serde
    /// blocker). All optional fields now carry safe defaults.
    #[test]
    fn subagent_spec_partial_json_uses_safe_defaults() {
        let spec: SubAgentSpec = serde_json::from_value(json!({
            "role": "probe",
            "prompt": "collect"
        }))
        .expect("partial spec must deserialize with defaults");
        assert_eq!(spec.role, "probe");
        assert_eq!(spec.prompt, "collect");
        assert!(spec.tools_allowlist.is_empty());
        assert!(!spec.allow_write);
        assert!(!spec.allow_exec);
        assert!(spec.skills.is_empty());
    }
}
