//! Context hierarchy inspired by ADK-RUST.
//!
//! Provides identity and environment data that flows through the agent execution pipeline.
//!
//! Hierarchy: ReadonlyContext → CallbackContext → ToolContext
//!            ReadonlyContext → InvocationContext

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::model::ChatMessage;
use crate::permission::PendingMap;
use crate::checkpoint::ATaskCheckpointer;

/// Agent operational mode. Orchestration is only available in Instant mode (depth 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    #[default]
    Instant,
    Expert,
}

/// Classifies a session: main user session, cron-triggered, or a sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionKind {
    #[default]
    Main,
    Cron,
    SubAgent,
}

impl SessionKind {
    /// Infer the kind from a session id prefix (`sub-` / `cron-` / otherwise Main).
    pub fn from_session_id(session_id: &str) -> Self {
        if session_id.starts_with("sub-") {
            Self::SubAgent
        } else if session_id.starts_with("cron-") {
            Self::Cron
        } else {
            Self::Main
        }
    }
}

/// Terminal lifecycle status of a sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SubAgentStatus {
    Pending,
    Running,
    Ok,
    Failed,
    Cancelled,
    Timeout,
}

/// Heuristic confidence for a sub-agent result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// A single pending write intent returned by a worker; executed by the Manager
/// single-writer. Kept generic for Step 1 (concrete kinds wired in Step 2a).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProposedWrite {
    pub kind: String,
    pub target: String,
    pub payload: serde_json::Value,
}

/// Structured result returned by a sub-agent (the only thing Orchestrator reads).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubAgentResult {
    pub run_id: String,
    pub role: String,
    pub summary: String,
    pub confidence: Confidence,
    pub token_usage: u64,
    pub evidence_refs: Vec<String>,
    pub artifact_refs: Vec<String>,
    pub case_ref: Option<String>,
    pub proposed_writes: Vec<ProposedWrite>,
    pub status: SubAgentStatus,
}

/// Spec passed to `spawn_subagent` describing a worker run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubAgentSpec {
    pub role: String,
    pub prompt: String,
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools_allowlist: Vec<String>,
    #[serde(default)]
    pub allow_write: bool,
    #[serde(default)]
    pub allow_exec: bool,
    pub model: Option<String>,
    pub timeout: Option<u64>,
    pub max_tokens: Option<usize>,
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub skills: Vec<String>,
}

/// Base identity context — immutable, passed through the entire pipeline.
/// Modeled after ADK-RUST's ReadonlyContext.
#[derive(Debug, Clone)]
pub struct ReadonlyContext {
    pub invocation_id: String,
    pub agent_name: String,
    pub session_id: String,
    pub created_at: DateTime<Utc>,
}

impl ReadonlyContext {
    pub fn new(invocation_id: String, agent_name: String, session_id: String) -> Self {
        Self {
            invocation_id,
            agent_name,
            session_id,
            created_at: Utc::now(),
        }
    }
}

/// Extended context for callbacks — adds mutable shared state.
/// Modeled after ADK-RUST's CallbackContext.
#[derive(Debug, Clone)]
pub struct CallbackContext {
    pub base: ReadonlyContext,
    pub shared_state: HashMap<String, Value>,
}

impl CallbackContext {
    pub fn new(base: ReadonlyContext) -> Self {
        Self {
            base,
            shared_state: HashMap::new(),
        }
    }

    pub fn get_state(&self, key: &str) -> Option<&Value> {
        self.shared_state.get(key)
    }

    pub fn set_state(&mut self, key: String, value: Value) {
        self.shared_state.insert(key, value);
    }
}

/// Context passed to tool execution.
/// Modeled after ADK-RUST's ToolContext.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub base: CallbackContext,
    pub function_call_id: String,
    pub working_dir: String,
    pub workspace_dir: String,
    pub mode: AgentMode,
    pub depth: u8,
    pub can_spawn: bool,
    pub run_id: Option<String>,
    /// Override for the artifact/output directory. Empty → defaults to
    /// `workspace_dir/output`. Used by Expert mode to write each round's
    /// artifacts directly into `managed/<contract>/round_NNN/`.
    output_dir: String,
    /// Optional progress channel for long-running tools to report status.
    /// Messages sent here are forwarded to the frontend as `progress` events.
    progress_tx: Option<tokio::sync::mpsc::Sender<String>>,
    /// Model context window in tokens (used for context-scaled inline limits).
    pub context_window: usize,
    /// Whether per-tool inline result caps scale with the model's context window.
    pub enable_context_scaling: bool,
    /// Absolute protection cap (chars) for how much of a single tool result or
    /// inline body is injected into the model context.
    pub max_inline_chars: usize,
}

impl ToolContext {
    pub fn new(base: CallbackContext, function_call_id: String, working_dir: String, workspace_dir: String) -> Self {
        Self {
            base,
            function_call_id,
            working_dir,
            workspace_dir,
            output_dir: String::new(),
            progress_tx: None,
            context_window: 128000,
            enable_context_scaling: true,
            max_inline_chars: 120_000,
            mode: AgentMode::Instant,
            depth: 0,
            can_spawn: false,
            run_id: None,
        }
    }

    /// Create a minimal ToolContext for simple use cases.
    pub fn simple(working_dir: String, workspace_dir: String) -> Self {
        let ctx = ReadonlyContext::new(
            String::new(),
            String::new(),
            String::new(),
        );
        let cb_ctx = CallbackContext::new(ctx);
        Self {
            base: cb_ctx,
            function_call_id: String::new(),
            working_dir,
            workspace_dir,
            output_dir: String::new(),
            progress_tx: None,
            context_window: 128000,
            enable_context_scaling: true,
            max_inline_chars: 120_000,
            mode: AgentMode::Instant,
            depth: 0,
            can_spawn: false,
            run_id: None,
        }
    }

    /// Set context-scaled inline-limit parameters for this tool invocation.
    pub fn with_inline_limits(mut self, context_window: usize, enabled: bool, max_inline_chars: usize) -> Self {
        self.context_window = context_window;
        self.enable_context_scaling = enabled;
        self.max_inline_chars = max_inline_chars;
        self
    }

    /// Override the artifact/output directory for this tool invocation.
    /// When empty, tools fall back to `workspace_dir/output`.
    pub fn with_output_dir(mut self, dir: String) -> Self {
        self.output_dir = dir;
        self
    }

    /// Effective output directory for artifacts. Falls back to
    /// `workspace_dir/output` when no per-invocation override is set.
    pub fn output_dir(&self) -> String {
        if self.output_dir.is_empty() {
            format!("{}/output", self.workspace_dir)
        } else {
            self.output_dir.clone()
        }
    }

    /// Create a ToolContext with a progress reporting channel.
    pub fn with_progress(mut self, tx: tokio::sync::mpsc::Sender<String>) -> Self {
        self.progress_tx = Some(tx);
        self
    }


    pub fn report_progress(&self, message: &str) {
        if let Some(ref tx) = self.progress_tx {
            let _ = tx.try_send(message.to_string());
        }
    }

    /// Compute an effective inline-content cap for a tool with a legacy
    /// conservative default. When context scaling is enabled, the legacy limit
    /// is raised proportionally to the model's context window (relative to a
    /// 128k token baseline) and bounded by `max_inline_chars`. When disabled,
    /// the legacy default is used unchanged.
    pub fn inline_limit(&self, legacy_default: usize) -> usize {
        effective_inline_limit(legacy_default, self.context_window, self.enable_context_scaling, self.max_inline_chars)
    }
}

/// Effective inline-content cap for a tool with a `legacy_default` conservative
/// limit. When context scaling is enabled, the limit is raised proportionally to
/// the model's `context_window` (relative to a 128k baseline) and bounded by the
/// absolute `max_inline_chars` protection cap; otherwise the legacy default is
/// used unchanged. Shared by ToolContext and the agent's per-result history cap
/// so all inline limits scale consistently.
pub fn effective_inline_limit(
    legacy_default: usize,
    context_window: usize,
    enabled: bool,
    max_inline_chars: usize,
) -> usize {
    if !enabled {
        return legacy_default;
    }
    let factor = (context_window as f64 / 128_000.0).max(1.0);
    let scaled = (legacy_default as f64 * factor).ceil() as usize;
    scaled
        .min(max_inline_chars.max(legacy_default))
        .max(legacy_default)
}

/// Context for an entire agent invocation.
/// Modeled after ADK-RUST's InvocationContext.
#[derive(Debug)]
pub struct InvocationContext {
    pub base: ReadonlyContext,
    pub agent_name: String,
    pub model_name: String,
    pub fallback_model: Option<String>,
    pub max_iterations: usize,
    pub rabbit_hole_threshold: usize,
    /// When true, trailing tool calls that exactly duplicate calls already
    /// executed this session are dropped if the model has produced a final
    /// text answer (guarded by config).
    pub trim_redundant_tool_calls: bool,
    /// When true, the agent pre-retrieves relevant knowledge pointers each
    /// user turn and injects them so it answers from stored knowledge without
    /// having to remember to call knowledge_search first (default on).
    pub knowledge_pre_retrieval: bool,
    /// Independent SOP replay switch (default on). Unlike knowledge_pre_retrieval,
    /// toggling knowledge off does not disable SOP replay.
    pub sop_replay: bool,
    pub budget_dashboard: bool,
    /// Shared budget snapshot sink: the agent writes its real measured context
    /// budget here each build; `/api/budget` reads it for the Dashboard page.
    pub budget_sink: Option<std::sync::Arc<std::sync::Mutex<Option<crate::context_arbiter::BudgetReport>>>>,
    /// Model context window size in tokens
    pub context_window: usize,
    /// Context usage threshold percentage (e.g. 80 = trim at 80%)
    pub context_window_threshold: usize,
    /// When true, per-tool inline result caps scale with the model's context window.
    pub enable_context_scaling: bool,
    /// Absolute protection cap (chars) for how much of a single tool result or
    /// inline body is injected into the model context.
    pub max_inline_chars: usize,
    /// Skill catalog listing strategy used when building the system prompt.
    pub skill_listing_strategy: crate::skill::SkillListingStrategy,
    /// Max chars of a single hot skill body inlined into the prompt.
    pub skill_max_inline_chars: usize,
    /// Max number of cold skills listed (name:desc) in the skill catalog.
    pub skill_catalog_max: usize,
    /// Top-K fuzzy-matched skills inlined (hot) per turn.
    pub skill_hot_top_k: usize,
    /// Tool execution timeout in seconds
    pub tool_timeout_secs: u64,
    /// Per-TODO-item timeout in seconds: an item left 'in_progress' longer than
    /// this is auto-marked 'skipped' by the watchdog so the task can move on.
    pub todo_item_timeout_secs: u64,
    /// Maximum automatic retries for retryable tool failures
    pub max_tool_retries: usize,
    pub conversation_history: Vec<ChatMessage>,
    pub shared_state: HashMap<String, Value>,
    /// Permission settings (category -> allowed)
    pub permissions: Arc<Mutex<HashMap<String, bool>>>,
    /// Shared pending map for permission requests
    pub permission_pending: PendingMap,
    /// History restored from a checkpoint (resume mode — skips adding user message).
    pub resume_history: Option<Vec<ChatMessage>>,
    /// Starting iteration when resuming from a checkpoint.
    pub resume_iteration: Option<usize>,
    /// Checkpoint ID for save/delete operations during this invocation.
    pub checkpoint_id: Option<String>,
    /// Checkpointer for persisting task state.
    pub checkpointer: Option<ATaskCheckpointer>,
    /// Path to the JSONL event log file for this run.
    /// When set, the agent logs all state changes to this file for crash recovery.
    pub event_log_path: Option<std::path::PathBuf>,
    /// Pre-authorization profile for managed (unattended) task execution.
    /// When set, matching tool calls bypass the human permission gate (Phase 6).
    pub preauth_profile: Option<std::sync::Arc<crate::managed::permission_profile::PermissionProfile>>,
    /// Optional per-invocation artifact/output directory override (Expert rounds).
    pub tool_output_dir: Option<String>,
    pub mode: AgentMode,
    pub depth: u8,
    pub can_spawn: bool,
    pub root_invocation_id: Option<String>,
    pub parent_invocation_id: Option<String>,
    pub session_kind: SessionKind,
    ended: Arc<AtomicBool>,
}

/// Parameters for the unified sub-agent child-context pass-through path
/// (SDD v1.5 §7.1 / Step 1.4). Everything the worker inherits from the parent
/// run travels through this one struct instead of ad-hoc field copies.
pub struct SubagentChildParams {
    pub run_id: String,
    pub role: String,
    pub model: String,
    pub max_iterations: usize,
    pub parent_invocation_id: String,
    pub root_invocation_id: String,
    pub parent_depth: u8,
    pub permissions: Arc<Mutex<HashMap<String, bool>>>,
    pub permission_pending: PendingMap,
    pub preauth_profile: Option<std::sync::Arc<crate::managed::permission_profile::PermissionProfile>>,
    pub context_window: usize,
    pub enable_context_scaling: bool,
    pub max_inline_chars: usize,
    pub tool_timeout_secs: u64,
    pub max_tool_retries: usize,
    /// Cancellation flag the worker observes (root-ended or per-worker ended).
    pub ended: Arc<AtomicBool>,
}

impl InvocationContext {
    /// Unified pass-through constructor for a sub-agent worker context
    /// (SDD v1.5 §7.4.1): `mode = Expert`, `depth = parent + 1`,
    /// `can_spawn = false`, `session_kind = SubAgent`, strategy-level skill
    /// shutdown (`Disabled`), and the §7.6 isolation semantics (fresh history,
    /// no inherited scratchpad — `new()` defaults already provide those).
    pub fn subagent_child(p: SubagentChildParams) -> Self {
        let session_id = format!("sub-{}", p.run_id);
        let base = ReadonlyContext::new(p.run_id.clone(), p.role.clone(), session_id);
        let mut ctx = Self::new(base, p.role, p.model, p.max_iterations);
        ctx.session_kind = SessionKind::SubAgent;
        ctx.mode = AgentMode::Expert;
        debug_assert_eq!(ctx.mode, AgentMode::Expert, "§7.4.1 child mode must be Expert");
        ctx.depth = p.parent_depth + 1;
        ctx.can_spawn = false;
        ctx.root_invocation_id = Some(p.root_invocation_id);
        ctx.parent_invocation_id = Some(p.parent_invocation_id);
        ctx.permissions = p.permissions;
        ctx.permission_pending = p.permission_pending;
        ctx.preauth_profile = p.preauth_profile;
        ctx.context_window = p.context_window;
        ctx.enable_context_scaling = p.enable_context_scaling;
        ctx.max_inline_chars = p.max_inline_chars;
        ctx.tool_timeout_secs = p.tool_timeout_secs;
        ctx.max_tool_retries = p.max_tool_retries;
        ctx.skill_listing_strategy = crate::skill::SkillListingStrategy::Disabled;
        ctx.set_ended(p.ended);
        ctx
    }

    pub fn new(
        base: ReadonlyContext,
        agent_name: String,
        model_name: String,
        max_iterations: usize,
    ) -> Self {
        let (resolver, pending) = crate::permission::PermissionResolver::new();
        let _ = resolver; // resolver is used by server, stored separately
        Self {
            base,
            agent_name,
            model_name,
            fallback_model: None,
            max_iterations,
            rabbit_hole_threshold: 5,
            trim_redundant_tool_calls: false, // Off by default; opt in via Settings to avoid dropping legitimate follow-up tool calls
            knowledge_pre_retrieval: true,
            sop_replay: true,
            budget_dashboard: true,
            budget_sink: None,
            context_window: 128000,
            context_window_threshold: 80,
            enable_context_scaling: true,
            max_inline_chars: 120_000,
            skill_listing_strategy: crate::skill::SkillListingStrategy::Query,
            skill_max_inline_chars: 6000,
            skill_catalog_max: 40,
            skill_hot_top_k: 3,
            tool_timeout_secs: 300,
            todo_item_timeout_secs: 600,
            max_tool_retries: 2,
            conversation_history: Vec::new(),
            shared_state: HashMap::new(),
            permissions: Arc::new(Mutex::new(crate::permission::default_permissions())),
            permission_pending: pending,
            resume_history: None,
            resume_iteration: None,
            checkpoint_id: None,
            checkpointer: None,
            event_log_path: None,
            preauth_profile: None,
            tool_output_dir: None,
            mode: AgentMode::Instant,
            depth: 0,
            can_spawn: false,
            root_invocation_id: None,
            parent_invocation_id: None,
            session_kind: SessionKind::Main,
            ended: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set permissions for tool execution.
    pub fn with_permissions(mut self, permissions: Arc<Mutex<HashMap<String, bool>>>) -> Self {
        self.permissions = permissions;
        self
    }

    /// Set the shared pending map for permission resolution.
    pub fn with_permission_pending(mut self, pending: PendingMap) -> Self {
        self.permission_pending = pending;
        self
    }

    /// Set conversation history for multi-turn context.
    pub fn with_history(mut self, history: Vec<ChatMessage>) -> Self {
        self.conversation_history = history;
        self
    }

    /// Set fallback model name.
    pub fn with_fallback_model(mut self, model: Option<String>) -> Self {
        self.fallback_model = model;
        self
    }

    /// Set rabbit hole detection threshold.
    pub fn with_rabbit_hole_threshold(mut self, threshold: usize) -> Self {
        self.rabbit_hole_threshold = threshold;
        self
    }

    /// Enable/disable dropping redundant trailing tool calls after a final
    /// text answer.
    pub fn with_trim_redundant_tool_calls(mut self, v: bool) -> Self {
        self.trim_redundant_tool_calls = v;
        self
    }

    /// Enable/disable per-turn knowledge pre-retrieval pointer injection.
    pub fn with_knowledge_pre_retrieval(mut self, v: bool) -> Self {
        self.knowledge_pre_retrieval = v;
        self
    }

    /// Independently enable/disable SOP replay.
    pub fn with_sop_replay(mut self, v: bool) -> Self {
        self.sop_replay = v;
        self
    }

    /// Enable/disable the unified context budget (Finite Brain) dashboard: when
    /// on, the agent's system prompt carries a CONTEXT BUDGET self-awareness block.
    pub fn with_budget_dashboard(mut self, v: bool) -> Self {
        self.budget_dashboard = v;
        self
    }

    /// Set the shared budget snapshot sink (writes location for measured reports).
    pub fn with_budget_sink(mut self, v: Option<std::sync::Arc<std::sync::Mutex<Option<crate::context_arbiter::BudgetReport>>>>) -> Self {
        self.budget_sink = v;
        self
    }

    /// Set context window size in tokens.
    pub fn with_context_window(mut self, tokens: usize) -> Self {
        self.context_window = tokens;
        self
    }

    /// Set context window usage threshold percentage.
    pub fn with_context_window_threshold(mut self, percent: usize) -> Self {
        self.context_window_threshold = percent;
        self
    }
    /// Enable/disable context-window-scaled inline result caps.
    pub fn with_enable_context_scaling(mut self, v: bool) -> Self {
        self.enable_context_scaling = v;
        self
    }
    /// Set the absolute protection cap (chars) for single-result inline content.
    /// Set the absolute protection cap (chars) for single-result inline content.
    pub fn with_max_inline_chars(mut self, chars: usize) -> Self {
        self.max_inline_chars = chars;
        self
    }

    /// Set the skill catalog listing strategy for the system prompt.
    pub fn with_skill_listing_strategy(mut self, s: crate::skill::SkillListingStrategy) -> Self {
        self.skill_listing_strategy = s;
        self
    }

    /// Set the max chars of a single hot skill body inlined into the prompt.
    pub fn with_skill_max_inline_chars(mut self, chars: usize) -> Self {
        self.skill_max_inline_chars = chars;
        self
    }

    /// Set the max number of cold skills listed in the catalog.
    pub fn with_skill_catalog_max(mut self, n: usize) -> Self {
        self.skill_catalog_max = n;
        self
    }

    /// Set the top-K fuzzy-matched skills inlined (hot) per turn.
    pub fn with_skill_hot_top_k(mut self, k: usize) -> Self {
        self.skill_hot_top_k = k;
        self
    }

    /// Set tool execution timeout in seconds.
    pub fn with_tool_timeout_secs(mut self, secs: u64) -> Self {
        self.tool_timeout_secs = secs;
        self
    }

    /// Set per-TODO-item timeout in seconds. Items left 'in_progress' longer
    /// than this are auto-marked 'skipped' so the task can advance.
    pub fn with_todo_item_timeout_secs(mut self, secs: u64) -> Self {
        self.todo_item_timeout_secs = secs;
        self
    }

    /// Set maximum automatic retries for retryable tool failures.
    pub fn with_max_tool_retries(mut self, retries: usize) -> Self {
        self.max_tool_retries = retries;
        self
    }

    /// Enable sub-agent orchestration spawning for this run (SDD v1.5 2.x).
    /// Gated at runtime by `mode == Instant && depth == 0` in the agent loop.
    pub fn with_can_spawn(mut self, enabled: bool) -> Self {
        self.can_spawn = enabled;
        self
    }

    /// Set the operational mode for this run. The shared runner is always Instant;
    /// Expert mode is used by ManagedRunner (separate code path). SDD v1.5 H2.
    pub fn with_mode(mut self, mode: AgentMode) -> Self {
        self.mode = mode;
        self
    }


    pub fn with_resume_state(mut self, history: Vec<ChatMessage>, start_iteration: usize) -> Self {
        self.resume_history = Some(history);
        self.resume_iteration = Some(start_iteration);
        self
    }

    /// Set the checkpoint ID for this invocation.
    pub fn with_checkpoint_id(mut self, id: String) -> Self {
        self.checkpoint_id = Some(id);
        self
    }

    /// Set the checkpointer for persisting task state.
    pub fn with_checkpointer(mut self, cp: ATaskCheckpointer) -> Self {
        self.checkpointer = Some(cp);
        self
    }

    /// Set the path for the JSONL event log.
    pub fn with_event_log_path(mut self, path: std::path::PathBuf) -> Self {
        self.event_log_path = Some(path);
        self
    }

    /// Set a pre-authorization profile for managed (unattended) task execution.
    pub fn with_preauth_profile(
        mut self,
        profile: Option<std::sync::Arc<crate::managed::permission_profile::PermissionProfile>>,
    ) -> Self {
        self.preauth_profile = profile;
        self
    }

    /// Set an optional artifact/output directory override for this invocation.
    pub fn with_tool_output_dir(mut self, dir: Option<String>) -> Self {
        self.tool_output_dir = dir;
        self
    }

    /// Signal that the invocation should end (e.g., ExitLoopTool called).
    pub fn end_invocation(&self) {
        self.ended.store(true, Ordering::SeqCst);
    }

    /// Check if the invocation has been signaled to end.
    pub fn set_ended(&mut self, ended: Arc<AtomicBool>) {
        self.ended = ended;
    }

    pub fn ended_flag(&self) -> Arc<AtomicBool> {
        self.ended.clone()
    }

    /// Worker-authorized tool names derived from this invocation's
    /// `preauth_profile` (Step 2a/2b): the write/exec tools the profile
    /// pre-authorizes, on top of the read-only base. Empty when no profile or
    /// the profile grants no recognized tool set. Reverse of
    /// `permission_profile::check_preauthorization` (SDD v1.5 §7.10).
    pub fn authorized_tools(&self) -> Vec<String> {
        match &self.preauth_profile {
            Some(profile) => crate::managed::permission_profile::authorized_tool_names(profile),
            None => Vec::new(),
        }
    }

    pub fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ReadonlyContext {
        ReadonlyContext::new("inv-1".into(), "agent".into(), "sess".into())
    }

    #[test]
    fn can_spawn_defaults_to_false_and_setter_toggles() {
        let ctx = InvocationContext::new(base(), "agent".into(), "m".into(), 5);
        assert!(!ctx.can_spawn);
        let ctx = ctx.with_can_spawn(true);
        assert!(ctx.can_spawn);
        let ctx = ctx.with_can_spawn(false);
        assert!(!ctx.can_spawn);
    }

    #[test]
    fn root_and_parent_invocation_roundtrip() {
        let mut ctx = InvocationContext::new(base(), "agent".into(), "m".into(), 5);
        ctx.root_invocation_id = Some("root-1".into());
        ctx.parent_invocation_id = Some("parent-1".into());
        assert_eq!(ctx.root_invocation_id.as_deref(), Some("root-1"));
        assert_eq!(ctx.parent_invocation_id.as_deref(), Some("parent-1"));
    }

    #[test]
    fn authorized_tools_empty_without_profile_and_derived_with_containment() {
        use crate::managed::permission_profile::{PermissionProfile, PreauthorizedAction};
        let plain = InvocationContext::new(base(), "agent".into(), "m".into(), 5);
        assert!(plain.authorized_tools().is_empty(), "no profile -> no authorized tools");

        let mut profile = PermissionProfile::new("t".into());
        profile.authorize(PreauthorizedAction::KillProcess);
        profile.authorize(PreauthorizedAction::RemovePersistence);
        let ctx = InvocationContext::new(base(), "agent".into(), "m".into(), 5)
            .with_preauth_profile(Some(std::sync::Arc::new(profile)));
        let mut names = ctx.authorized_tools();
        names.sort();
        assert_eq!(names, vec!["ir_persistence", "shell_exec", "sys_process"]);
    }

    #[test]
    fn subagent_child_keeps_skills_disabled_read_only() {
        use crate::context::SubagentChildParams;
        use std::sync::atomic::AtomicBool;
        use crate::permission::PermissionResolver;
        let ctx = InvocationContext::subagent_child(SubagentChildParams {
            run_id: "r1".into(),
            role: "w".into(),
            model: "mock".into(),
            max_iterations: 5,
            parent_invocation_id: "p".into(),
            root_invocation_id: "root".into(),
            parent_depth: 0,
            permissions: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            permission_pending: PermissionResolver::new().1,
            preauth_profile: None,
            context_window: 128000,
            enable_context_scaling: true,
            max_inline_chars: 120000,
            tool_timeout_secs: 60,
            max_tool_retries: 0,
            ended: std::sync::Arc::new(AtomicBool::new(false)),
        });
        // §20.3 B4.3 / §7.6: worker stays strategy-level skill-off by construction.
        assert_eq!(ctx.skill_listing_strategy, crate::skill::SkillListingStrategy::Disabled);
        assert_eq!(ctx.mode, crate::context::AgentMode::Expert);
        assert!(!ctx.can_spawn, "worker must be read-only (no grandchildren)");
        assert_eq!(ctx.session_kind, crate::context::SessionKind::SubAgent);
    }
}
