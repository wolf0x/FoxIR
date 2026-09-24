//! Event system with metadata, inspired by ADK-RUST's Event structure.
//!
//! Events flow from agent → runner → server → client.
//! Each event carries identity metadata (id, timestamp, author, invocation_id).

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

/// A single event in the agent pipeline, carrying metadata.
/// Modeled after ADK-RUST's Event struct with id, timestamp, author, invocation_id.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
pub enum AgentEvent {
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(flatten)]
        meta: EventMeta,
        content: String,
    },

    #[serde(rename = "text")]
    TextDelta {
        #[serde(flatten)]
        meta: EventMeta,
        content: String,
    },

    #[serde(rename = "tool_call")]
    ToolCall {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        #[serde(rename = "call_id")]
        call_id: String,
        args: Value,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        #[serde(rename = "call_id")]
        call_id: String,
        result: Value,
    },

    /// Heartbeat / progress event sent during long-running tool execution.
    #[serde(rename = "progress")]
    Progress {
        #[serde(flatten)]
        meta: EventMeta,
        /// Name of the tool being executed
        tool_name: String,
        /// Human-readable status message
        message: String,
        /// Seconds elapsed since tool execution started
        elapsed_secs: u64,
    },

    #[serde(rename = "error")]
    Error {
        #[serde(flatten)]
        meta: EventMeta,
        message: String,
    },

    #[serde(rename = "permission_request")]
    PermissionRequest {
        #[serde(flatten)]
        meta: EventMeta,
        request_id: String,
        tool_name: String,
        category: String,
        args: Value,
        /// Plain-language one-line description of what executing the tool does,
        /// shown to the user when they are asked to approve/deny the action.
        #[serde(default)]
        explanation: String,
    },

    #[serde(rename = "permission_response")]
    PermissionResponse {
        #[serde(flatten)]
        meta: EventMeta,
        request_id: String,
        allowed: bool,
    },

    #[serde(rename = "done")]
    Done {
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Token usage statistics from an LLM API call.
    #[serde(rename = "usage")]
    Usage {
        #[serde(flatten)]
        meta: EventMeta,
        /// Model name used for this call.
        model: String,
        /// Input/prompt tokens.
        prompt_tokens: u64,
        /// Output/completion tokens.
        completion_tokens: u64,
        /// Total tokens (prompt + completion).
        total_tokens: u64,
        /// Prompt tokens served from the provider's prefix cache (cache hit).
        /// `None` = this endpoint does not report cache accounting; distinguish
        /// that from `Some(0)` = reported and nothing was hit.
        cached_tokens: Option<u64>,
    },

    /// A worker sub-agent has been dispatched (Expert orchestration, T6.5+).
    #[serde(rename = "subagent_spawned")]
    SubagentSpawned {
        #[serde(flatten)]
        meta: EventMeta,
        #[serde(rename = "run_id")]
        run_id: String,
        role: String,
    },

    /// A worker sub-agent reached a terminal non-failure status.
    #[serde(rename = "subagent_completed")]
    SubagentCompleted {
        #[serde(flatten)]
        meta: EventMeta,
        #[serde(rename = "run_id")]
        run_id: String,
        role: String,
        status: String,
        confidence: String,
        summary: String,
        #[serde(rename = "evidence_refs")]
        evidence_refs: Vec<String>,
    },

    /// A worker sub-agent failed / timed out / was cancelled.
    #[serde(rename = "subagent_failed")]
    SubagentFailed {
        #[serde(flatten)]
        meta: EventMeta,
        #[serde(rename = "run_id")]
        run_id: String,
        role: String,
        reason: String,
    },

    /// Per-worker token usage for the budget drawer.
    #[serde(rename = "budget_update")]
    BudgetUpdate {
        #[serde(flatten)]
        meta: EventMeta,
        #[serde(rename = "run_id")]
        run_id: String,
        role: String,
        tokens: u64,
    },

    /// The Manager published/updated the shared plan (task tree source).
    #[serde(rename = "plan_updated")]
    PlanUpdated {
        #[serde(flatten)]
        meta: EventMeta,
        plan: serde_json::Value,
    },
}

/// Event metadata — identity, timing, and provenance.
/// Modeled after ADK-RUST's Event fields.
#[derive(Debug, Clone, Serialize)]
pub struct EventMeta {
    /// Unique event ID (UUID v4).
    pub id: String,
    /// When the event was created.
    pub timestamp: DateTime<Utc>,
    /// The invocation this event belongs to.
    pub invocation_id: String,
    /// Who authored this event (agent name or "user").
    pub author: String,
}

impl EventMeta {
    pub fn new(invocation_id: &str, author: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            invocation_id: invocation_id.to_string(),
            author: author.to_string(),
        }
    }

    /// Create a minimal meta (for simple/internal events).
    pub fn minimal() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            invocation_id: String::new(),
            author: "system".to_string(),
        }
    }
}

impl AgentEvent {
    // --- Serialization ---

    pub fn to_ws_message(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    // --- Convenience constructors ---

    pub fn thinking(content: &str, invocation_id: &str, author: &str) -> Self {
        AgentEvent::Thinking {
            meta: EventMeta::new(invocation_id, author),
            content: content.to_string(),
        }
    }

    pub fn text(content: &str, invocation_id: &str, author: &str) -> Self {
        AgentEvent::TextDelta {
            meta: EventMeta::new(invocation_id, author),
            content: content.to_string(),
        }
    }

    pub fn tool_call(name: &str, call_id: &str, args: Value, invocation_id: &str, author: &str) -> Self {
        AgentEvent::ToolCall {
            meta: EventMeta::new(invocation_id, author),
            name: name.to_string(),
            call_id: call_id.to_string(),
            args,
        }
    }

    pub fn tool_result(name: &str, call_id: &str, result: Value, invocation_id: &str, author: &str) -> Self {
        AgentEvent::ToolResult {
            meta: EventMeta::new(invocation_id, author),
            name: name.to_string(),
            call_id: call_id.to_string(),
            result,
        }
    }

    pub fn progress(tool_name: &str, message: &str, elapsed_secs: u64, invocation_id: &str, author: &str) -> Self {
        AgentEvent::Progress {
            meta: EventMeta::new(invocation_id, author),
            tool_name: tool_name.to_string(),
            message: message.to_string(),
            elapsed_secs,
        }
    }

    pub fn error(message: &str, invocation_id: &str, author: &str) -> Self {
        AgentEvent::Error {
            meta: EventMeta::new(invocation_id, author),
            message: message.to_string(),
        }
    }

    pub fn permission_request(request_id: &str, tool_name: &str, category: &str, args: Value, explanation: &str, invocation_id: &str, author: &str) -> Self {
        AgentEvent::PermissionRequest {
            meta: EventMeta::new(invocation_id, author),
            request_id: request_id.to_string(),
            tool_name: tool_name.to_string(),
            category: category.to_string(),
            args,
            explanation: explanation.to_string(),
        }
    }

    pub fn permission_response(request_id: &str, allowed: bool, invocation_id: &str, author: &str) -> Self {
        AgentEvent::PermissionResponse {
            meta: EventMeta::new(invocation_id, author),
            request_id: request_id.to_string(),
            allowed,
        }
    }

    pub fn done(invocation_id: &str, author: &str) -> Self {
        AgentEvent::Done {
            meta: EventMeta::new(invocation_id, author),
        }
    }

    pub fn usage(model: &str, prompt_tokens: u64, completion_tokens: u64, total_tokens: u64, cached_tokens: Option<u64>, invocation_id: &str, author: &str) -> Self {
        AgentEvent::Usage {
            meta: EventMeta::new(invocation_id, author),
            model: model.to_string(),
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_tokens,
        }
    }

    pub fn subagent_spawned(run_id: &str, role: &str, invocation_id: &str, author: &str) -> Self {
        AgentEvent::SubagentSpawned {
            meta: EventMeta::new(invocation_id, author),
            run_id: run_id.to_string(),
            role: role.to_string(),
        }
    }

    pub fn subagent_completed(run_id: &str, role: &str, status: &str, confidence: &str, summary: &str, evidence_refs: Vec<String>, invocation_id: &str, author: &str) -> Self {
        AgentEvent::SubagentCompleted {
            meta: EventMeta::new(invocation_id, author),
            run_id: run_id.to_string(),
            role: role.to_string(),
            status: status.to_string(),
            confidence: confidence.to_string(),
            summary: summary.to_string(),
            evidence_refs,
        }
    }

    pub fn subagent_failed(run_id: &str, role: &str, reason: &str, invocation_id: &str, author: &str) -> Self {
        AgentEvent::SubagentFailed {
            meta: EventMeta::new(invocation_id, author),
            run_id: run_id.to_string(),
            role: role.to_string(),
            reason: reason.to_string(),
        }
    }

    pub fn budget_update(run_id: &str, role: &str, tokens: u64, invocation_id: &str, author: &str) -> Self {
        AgentEvent::BudgetUpdate {
            meta: EventMeta::new(invocation_id, author),
            run_id: run_id.to_string(),
            role: role.to_string(),
            tokens,
        }
    }

    pub fn plan_updated(plan: serde_json::Value, invocation_id: &str, author: &str) -> Self {
        AgentEvent::PlanUpdated {
            meta: EventMeta::new(invocation_id, author),
            plan,
        }
    }

    // --- Getters ---

    pub fn meta(&self) -> &EventMeta {
        match self {
            Self::Thinking { meta, .. }
            | Self::TextDelta { meta, .. }
            | Self::ToolCall { meta, .. }
            | Self::ToolResult { meta, .. }
            | Self::Progress { meta, .. }
            | Self::Error { meta, .. }
            | Self::PermissionRequest { meta, .. }
            | Self::PermissionResponse { meta, .. }
            | Self::Usage { meta, .. }
            | Self::SubagentSpawned { meta, .. }
            | Self::SubagentCompleted { meta, .. }
            | Self::SubagentFailed { meta, .. }
            | Self::BudgetUpdate { meta, .. }
            | Self::PlanUpdated { meta, .. }
            | Self::Done { meta } => meta,
        }
    }

    pub fn is_done(&self) -> bool {
        match self {
            Self::Done { .. } => true,
            _ => false,
        }
    }

    /// Get the text content if this is a TextDelta event.
    pub fn text_content(&self) -> Option<&str> {
        match self {
            Self::TextDelta { content, .. } => Some(content.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_and_plan_events_serialize_with_expected_ws_types() {
        let spawned = AgentEvent::subagent_spawned("sub-1", "port_scan", "root", "orchestrator");
        let v: Value = serde_json::from_str(&spawned.to_ws_message()).unwrap();
        assert_eq!(v["type"], "subagent_spawned");
        assert_eq!(v["run_id"], "sub-1");
        assert_eq!(v["role"], "port_scan");

        let done = AgentEvent::subagent_completed("sub-1", "port_scan", "completed", "High",
            "found open port 445", vec!["output/services.txt".to_string()], "root", "orchestrator");
        let v: Value = serde_json::from_str(&done.to_ws_message()).unwrap();
        assert_eq!(v["type"], "subagent_completed");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["confidence"], "High");
        assert_eq!(v["evidence_refs"][0], "output/services.txt");

        let budget = AgentEvent::budget_update("sub-1", "port_scan", 12345, "root", "orchestrator");
        let v: Value = serde_json::from_str(&budget.to_ws_message()).unwrap();
        assert_eq!(v["type"], "budget_update");
        assert_eq!(v["tokens"], 12345);

        let failed = AgentEvent::subagent_failed("sub-2", "log_parse", "timeout", "root", "orchestrator");
        let v: Value = serde_json::from_str(&failed.to_ws_message()).unwrap();
        assert_eq!(v["type"], "subagent_failed");
        assert_eq!(v["reason"], "timeout");

        let plan = AgentEvent::plan_updated(serde_json::json!({"round": 1, "subtask": "recon"}), "root", "manager");
        let v: Value = serde_json::from_str(&plan.to_ws_message()).unwrap();
        assert_eq!(v["type"], "plan_updated");
        assert_eq!(v["plan"]["subtask"], "recon");
    }
}
