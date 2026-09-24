use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    response::{IntoResponse, Response},
    routing::{get, post, put, delete, patch},
    Json, Router,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::agent::AgentEvent;
use crate::log::ConversationLogger;
use crate::memory::MemoryStore;
use crate::model::ChatMessage;
use crate::permission::{PermissionResolver, PendingMap};
use crate::heartbeat::Heartbeat;
use crate::runner::Runner;
use crate::runner::ResumeState;
use crate::scheduler::{Scheduler, CronTask};
use crate::skill::SkillManager;
use crate::config::McpServerConfig;
use crate::external_tools::ExternalToolsManager;
use crate::tool::mcp_client::McpClientManager;
use crate::tool::ToolRegistry;
use crate::web::StaticServer;
use crate::model::openai::OpenAiProvider;

/// Reduce the distilled handoff to a single concise line (<= 100 chars) for the
/// Expert continue prompt, instead of showing the full raw findings dump.
/// Picks the latest instruction, the original task, and the first descriptive
/// finding line, then flattens + truncates to one line.
/// Does the user message carry an actual task to run, or is it only a mode-switch /
/// filler command (e.g. "go", "continues", "hello")? Used to decide whether a
/// "Start new round" Expert choice should dispatch immediately or ask for the task.
fn is_concrete_task(input: &str) -> bool {
    let t = input.trim();
    if t.is_empty() {
        return false;
    }
    let lower = t.to_lowercase();
    let fillers = [
        "go", "hello", "hi", "hey", "ok", "okay", "yes", "yep", "retry", "again",
        "new", "start", "开", "新", "开始", "继续", "继续吧", "好的", "好", "嗯", "在",
    ];
    if fillers.iter().any(|f| lower == *f) {
        return false;
    }
    if lower.starts_with("continue") || lower.starts_with("continu") || lower.starts_with("接着") {
        return false;
    }
    // Too short to be a concrete instruction.
    if t.chars().count() < 6 {
        return false;
    }
    true
}
/// Build a compact per-round summary ("Round 1 - <summary>; Round 2 - <summary>…")
/// from an existing TaskContract JSON. Each line is capped at 50 chars (one sentence).
fn rounds_summary_from_contract(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return String::new(); };
    let Some(notes) = v.get("manager_notes").and_then(|n| n.as_array()) else { return String::new(); };
    let mut pairs: Vec<(u32, String)> = Vec::new();
    for note in notes {
        let s = note.as_str().unwrap_or("").trim();
        if let Some(rest) = s.strip_prefix("Round ") {
            if let Some(col) = rest.find(':') {
                if let Ok(n) = rest[..col].trim().parse::<u32>() {
                    let body = rest[col + 1..].trim();
                    if !body.is_empty() {
                        pairs.push((n, body.chars().take(50).collect::<String>()));
                    }
                }
            }
        }
    }
    // Keep only the most recent rounds.
    if pairs.len() > 10 {
        pairs = pairs[pairs.len() - 10..].to_vec();
    }
    pairs.iter().map(|(n, s)| format!("Round {} - {}", n, s)).collect::<Vec<_>>().join("; ")
}
fn compress_handoff_summary(handoff: &str) -> String {
    const MAX: usize = 100;
    let mut original = String::new();
    let mut latest = String::new();
    let mut gist = String::new();
    let mut in_findings = false;
    for line in handoff.lines() {
        let l = line.trim();
        if let Some(r) = l.strip_prefix("Original task:") {
            original = r.trim().to_string();
            continue;
        }
        if let Some(r) = l.strip_prefix("Latest instruction:") {
            latest = r.trim().to_string();
            continue;
        }
        if l.starts_with("Prior findings") || l.starts_with("Prior") {
            in_findings = true;
            continue;
        }
        if in_findings && gist.is_empty()
            && !l.is_empty() && !l.starts_with('#') && !l.starts_with('|')
            && !l.starts_with('-') && !l.starts_with('*') && !l.starts_with('`') && !l.starts_with("[") {
            gist = l.to_string();
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if !latest.is_empty() && latest != original {
        parts.push(latest.clone());
    }
    if !original.is_empty() {
        parts.push(original.clone());
    }
    if !gist.is_empty() {
        parts.push(gist.clone());
    }
    let joined = parts.join(" | ");
    let flat: String = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        return flat;
    }
    let cut: String = flat.chars().take(MAX.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}


/// Type alias for the broadcast channel used to push notifications to all WS clients.
pub type NotifyTx = tokio::sync::broadcast::Sender<String>;

/// Shared WebSocket send half, cloneable handle for one connection.
pub type WsSink = Arc<Mutex<futures::stream::SplitSink<WebSocket, Message>>>;

/// Swappable sink held by an in-flight run. The run writes through this slot
/// instead of a concrete connection, so a browser refresh / sleep / network
/// drop **detaches** the run (events are no longer forwarded) instead of
/// cancelling it, and a reconnecting client can re-attach its new sink and
/// watch the same run continue live. Cancellation stays an explicit user
/// action (STOP -> per-session cancel flag), never a side effect of transport.
pub type SinkSlot = Arc<std::sync::Mutex<Option<WsSink>>>;

/// Outcome of forwarding one event through a [`SinkSlot`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Forward {
    /// Delivered to the client.
    Delivered,
    /// Send stalled; the message was dropped but the connection is still usable.
    Dropped,
    /// The connection is gone. The slot is now empty: the run keeps executing,
    /// detached, until a client re-attaches or the user presses STOP.
    Detached,
    /// No client attached (already detached).
    NoClient,
}

/// Forward one serialized event through the session's current sink, if any.
/// Never fails a run: every failure mode degrades to a dropped message.
async fn slot_forward(slot: &SinkSlot, msg: String) -> Forward {
    let sink = slot.lock().unwrap().clone();
    let Some(sink) = sink else { return Forward::NoClient };
    match ws_send_bounded(&sink, msg).await {
        WsSendOutcome::Sent => Forward::Delivered,
        WsSendOutcome::Dropped => Forward::Dropped,
        WsSendOutcome::Closed => {
            *slot.lock().unwrap() = None;
            Forward::Detached
        }
    }
}

/// Per-session running state for parallel multi-session execution (形态 B).
/// Each active session gets its own cancel flag so stopping/starting one session
/// never touches another. Registered while a session's task is in flight and
/// removed when it completes.
#[derive(Default)]
pub struct SessionRunState {
    pub cancel: Arc<AtomicBool>,
    pub running: bool,
    /// Current WS sink for this in-flight run (empty while detached).
    pub sink: SinkSlot,
}

pub struct AppState {
    pub runner: Arc<Runner>,
    pub skill_manager: Arc<SkillManager>,
    pub mcp_manager: Arc<Mutex<McpClientManager>>,
    /// Shared tool registry — wrapped in RwLock so MCP handlers can register/unregister tools dynamically
    pub tools: Arc<tokio::sync::RwLock<ToolRegistry>>,
    pub logger: Arc<ConversationLogger>,
    pub memory_store: Arc<MemoryStore>,
    pub external_tools: Arc<Mutex<ExternalToolsManager>>,
    pub password: String,
    /// Shared mutable model configs (shared with OpenAiProvider for runtime CRUD)
    pub model_configs: Arc<tokio::sync::RwLock<Vec<crate::config::ModelConfig>>>,
    /// Path to models.json persistence file
    pub model_store_path: String,
    pub max_iterations: Arc<AtomicUsize>,
    pub rabbit_hole_threshold: Arc<AtomicUsize>,
    pub trim_redundant_tool_calls: Arc<AtomicBool>,
    pub knowledge_pre_retrieval: Arc<AtomicBool>,
    /// 独立 SOP 回放开关（默认开；与 knowledge_pre_retrieval 解耦）。
    pub sop_replay: Arc<AtomicBool>,
    /// 统一上下文预算仪表盘（有限脑）开关（默认开）。
    pub budget_dashboard: Arc<AtomicBool>,
    /// Linux 取证工具族全量载入开关（默认关 = 降为按需载入）。与 agent 共享同一原子。
    pub linux_ir_tools: Arc<AtomicBool>,
    /// browser_cdp 无头开关（默认开）。关掉后浏览器可见，用于完成一次登录。
    /// 与 BrowserSession 共享同一原子，切换不需重启。
    pub browser_headless: Arc<AtomicBool>,
    /// browser_cdp 显式指定的浏览器路径，空串 = 自动探测。与 BrowserSession 共享。
    pub browser_executable: Arc<std::sync::RwLock<String>>,
    /// Web Browser 能力开关（Tools 页）。false = browser_cdp 已从注册表注销。
    pub browser_enabled: Arc<AtomicBool>,
    /// 浏览器会话本体：Tools 页重新启用时要拿回同一个会话（同一份持久 profile），
    /// 不能另建一个。ir_report 也已经持有同一个。
    pub browser_session: Arc<crate::tool::browser_cdp::BrowserSession>,
    /// 有限脑实测预算快照：agent 每次组装上下文时写入，`/api/budget` 读取。
    pub context_budget: Arc<std::sync::Mutex<Option<crate::context_arbiter::BudgetReport>>>,
    /// 发生过 task-matched SKILL 驱动的会话集合（SOP 蒸馏门控）。
    pub skill_used_sessions: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// 深层记忆注入开关（默认开）。
    pub two_tier_memory: Arc<AtomicBool>,
    /// 会话结束后 Debrief（案例/SOP 固化）开关。
    pub debrief_enabled: Arc<AtomicBool>,
    /// 每会话工具调用日志（权威口径：事件流 ToolResult），
    /// 供 Debrief 门控与作者化使用。每会话上限 200 条。
    pub session_tool_log: Arc<Mutex<std::collections::HashMap<String, Vec<(String, bool)>>>>,
    pub enable_context_scaling: Arc<AtomicBool>,
    pub max_inline_chars: Arc<AtomicUsize>,
    pub skill_listing_strategy: Arc<AtomicUsize>,
    pub skill_max_inline_chars: Arc<AtomicUsize>,
    pub skill_catalog_max: Arc<AtomicUsize>,
    pub skill_hot_top_k: Arc<AtomicUsize>,
    pub context_window_threshold: Arc<AtomicUsize>,
    pub tool_timeout_secs: Arc<AtomicUsize>,
    pub max_tool_retries: Arc<AtomicUsize>,
    /// Expert mode settings (used when managed=true) — AtomicUsize so settings
    /// can be hot-reloaded from the UI without restarting the process.
    pub expert_max_iterations: Arc<AtomicUsize>,
    pub expert_tool_timeout_secs: Arc<AtomicUsize>,
    pub expert_max_tool_retries: Arc<AtomicUsize>,
    pub expert_max_managed_rounds: Arc<AtomicUsize>,
    /// Sub-agent orchestration limits (concurrency/timeout) sourced from
    /// `config.agent.modes.expert`; used by the ManagedRunner parallel collect.
    pub orchestration_limits: Arc<crate::config::OrchestrationLimits>,
    /// Per-session conversation history for multi-turn context
    pub sessions: Arc<Mutex<std::collections::HashMap<String, Vec<ChatMessage>>>>,
    /// Lightweight session registry for the multi-session navigation UI
    /// (titles/timestamps/soft-delete), persisted to session_index.json.
    pub session_index: Arc<crate::session::SessionIndex>,
    /// Per-session last memory-context (daily summary) injection epoch, so a
    /// long-lived open session refreshes its 7-day overview instead of going stale.
    pub memory_ctx_at: Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
    /// Permission settings (category -> allowed), shared across connections
    pub permissions: Arc<Mutex<std::collections::HashMap<String, bool>>>,
    /// Resolver for pending permission requests
    pub permission_resolver: PermissionResolver,
    /// Shared pending map for permission requests
    pub permission_pending: PendingMap,
    /// Per-session Expert-mode task cancellation flags (session_id -> flag).
    /// Each managed run gets its OWN flag so a subsequent chat message (which
    /// resets the connection-level `cancelled`) cannot un-cancel a task that is
    /// still winding down — preventing two managed loops on the same contract.
    pub expert_tasks: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>>,
    /// Parallel multi-session concurrency cap (default 5). Hot-reloadable from
    /// the Settings panel via `agent_settings_save_handler`.
    pub session_max: Arc<AtomicUsize>,
    /// Per-session running-state registry for parallel execution.
    pub session_runs: Arc<std::sync::Mutex<std::collections::HashMap<String, SessionRunState>>>,
    /// CRON task scheduler
    pub scheduler: Arc<Mutex<Scheduler>>,
    /// Broadcast channel for push notifications (sys_remind, etc.)
    pub notify_tx: NotifyTx,
    /// Agent workspace directory (where AGENTS.md, SOUL.md, TOOLS.md live)
    pub workspace_dir: String,
    /// LLM provider for end-of-session knowledge distillation
    pub provider: Arc<OpenAiProvider>,
    /// Whether Computer Use (GUI control) tools are enabled
    pub computer_use_enabled: Arc<AtomicBool>,
    /// Whether to use LLM to simulate human intervention when Expert mode is blocked
    pub human_intervention_enabled: Arc<AtomicBool>,
    /// Whether the background heartbeat (proactive HEARTBEAT.md checks) is enabled
    pub heartbeat_enabled: Arc<AtomicBool>,
    /// Primary model name (from config.toml) — RwLock for hot-reload from UI
    pub primary_model: Arc<std::sync::RwLock<Option<String>>>,
    /// Fallback model name (from config.toml) — RwLock for hot-reload from UI
    pub fallback_model: Arc<std::sync::RwLock<Option<String>>>,
    /// Expert-mode per-role model overrides (Manager/Auditor/Executor) and their
    /// optional fallbacks. Hot-reloadable from Settings UI.
    pub expert_role_models: Arc<std::sync::RwLock<crate::config::RoleModelsConfig>>,
    /// Timezone offset in hours (from config.toml) — RwLock for hot-reload from UI
    pub timezone_offset: Arc<std::sync::RwLock<i8>>,
}

impl AppState {
    /// Try to reserve a parallel-execution slot for `session_id`, respecting the
    /// `session_max` cap. Also installs a fresh per-session cancel flag.
    /// Returns `None` when the cap is reached (caller should reject the run) or
    /// `Some(cancel)` when the slot was acquired.
    pub fn session_slot_acquire(&self, session_id: &str) -> Option<Arc<AtomicBool>> {
        let max = self.session_max.load(Ordering::SeqCst);
        {
            // Existing run: reuse its cancel flag (mark running if it had settled).
            let mut runs = self.session_runs.lock().unwrap();
            if let Some(st) = runs.get_mut(session_id) {
                if !st.running {
                    st.running = true;
                    st.cancel.store(false, Ordering::SeqCst);
                    debrief_forget(session_id);
                }
                return Some(st.cancel.clone());
            }
        }
        // New session: enforce the concurrency cap before inserting.
        let mut runs = self.session_runs.lock().unwrap();
        let active: usize = runs.values().filter(|s| s.running).count();
        if active >= max {
            return None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        debrief_forget(session_id);
        runs.insert(session_id.to_string(), SessionRunState { cancel: cancel.clone(), running: true, sink: SinkSlot::default() });
        Some(cancel)
    }

    /// Mark a session run as finished and remove it from the registry.
    pub fn session_slot_release(&self, session_id: &str) {
        if let Ok(mut runs) = self.session_runs.lock() {
            runs.remove(session_id);
        }
    }

    /// Request cancellation of a specific session's in-flight run.
    /// Returns whether a running task was found for this session.
    pub fn session_request_cancel(&self, session_id: &str) -> bool {
        if let Ok(runs) = self.session_runs.lock() {
            if let Some(st) = runs.get(session_id) {
                st.cancel.store(true, Ordering::SeqCst);
                return true;
            }
        }
        false
    }

    /// Number of currently active (running) sessions.
    pub fn session_active_count(&self) -> usize {
        self.session_runs
            .lock()
            .map(|r| r.values().filter(|s| s.running).count())
            .unwrap_or(0)
    }

    /// Whether `session_id` already has an in-flight run (parallel demux guard,
    /// used to queue a follow-up task instead of spawning a second runner).
    pub fn session_is_running(&self, session_id: &str) -> bool {
        self.session_runs
            .lock()
            .map(|r| r.get(session_id).map(|s| s.running).unwrap_or(false))
            .unwrap_or(false)
    }

    /// Install a fresh slot bound to `sink` for a run that is about to start
    /// (`session_slot_acquire` has already registered it). Idempotent: reuses the
    /// existing slot so a re-attached client and the spawned drain share it.
    pub fn session_install_sink(&self, session_id: &str, sink: &WsSink) -> SinkSlot {
        let runs = self.session_runs.lock().unwrap();
        if let Some(st) = runs.get(session_id) {
            *st.sink.lock().unwrap() = Some(sink.clone());
            return st.sink.clone();
        }
        // No registry entry (concurrency cap reached, connection-level fallback):
        // the run still gets a private slot, it just cannot be re-attached.
        Arc::new(std::sync::Mutex::new(Some(sink.clone())))
    }

    /// List in-flight sessions whose `wanted` set contains their id, and attach
    /// `sink` to each. Used on (re)connect: the client announces the session ids
    /// it can render, and every run still executing for one of them resumes
    /// streaming into this connection instead of finishing unheard.
    pub fn session_attach_sinks(&self, wanted: &[String], sink: &WsSink) -> Vec<String> {
        let mut attached = Vec::new();
        let runs = self.session_runs.lock().unwrap();
        for id in wanted {
            if let Some(st) = runs.get(id) {
                if st.running {
                    *st.sink.lock().unwrap() = Some(sink.clone());
                    attached.push(id.clone());
                }
            }
        }
        attached
    }
}

pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/static/{*path}", get(static_handler))
        .route("/ws", get(ws_handler))
        .route("/api/models", get(models_handler))
        .route("/api/providers", get(providers_handler))
        .route("/api/providers", post(providers_create_handler))
        .route("/api/providers/{title}", put(providers_update_handler))
        .route("/api/providers/{title}", delete(providers_delete_handler))
        .route("/api/providers/{title}/test", post(providers_test_handler))
        .route("/api/health", get(health_handler))
        .route("/api/skills", get(skills_handler))
        .route("/api/skills", post(skills_create_handler))
        .route("/api/skills/metrics", get(skills_metrics_handler))
        .route("/api/skills/reload", post(skills_reload_handler))
        .route("/api/skills/{name}", delete(skills_delete_handler))
        .route("/api/skills/{name}/toggle", post(skills_toggle_handler))
        .route("/api/mcp", get(mcp_handler))
        .route("/api/mcp", post(mcp_create_handler))
        .route("/api/mcp/{name}", delete(mcp_delete_handler))
        .route("/api/mcp/{name}/toggle", post(mcp_toggle_handler))
        .route("/api/mcp/{name}/restart", post(mcp_restart_handler))
        .route("/api/logs", get(logs_handler))
        .route("/api/logs/dates", get(log_dates_handler))
        .route("/api/managed/reset", post(managed_reset_handler))
        .route("/api/cron", get(cron_list_handler))
        .route("/api/cron", post(cron_create_handler))
        .route("/api/cron/{id}", put(cron_update_handler))
        .route("/api/cron/{id}", delete(cron_delete_handler))
        .route("/api/cron/{id}/toggle", post(cron_toggle_handler))
        .route("/api/cron/settings", get(cron_settings_handler))
        .route("/api/cron/settings", post(cron_settings_update_handler))
        .route("/api/notify", post(notify_handler))
        .route("/api/knowledge", get(knowledge_list_handler))
        .route("/api/knowledge", post(knowledge_create_handler))
        .route("/api/knowledge/preferred", post(knowledge_preferred_handler))
        .route("/api/knowledge/upload", post(knowledge_upload_handler))
        .route("/api/knowledge/rebuild", post(knowledge_rebuild_handler))
        .route("/api/knowledge/refresh", post(knowledge_refresh_handler))
        .route("/api/knowledge/{name}", delete(knowledge_delete_handler))
        .route("/api/memory/dates", get(memory_dates_handler))
        .route("/api/memory/summaries", get(memory_summaries_handler))
        .route("/api/memory", get(memory_entries_handler))
        .route("/api/memory/summarize", post(memory_summarize_handler))
.route("/api/memory/deep", get(deep_memory_list_handler))
        .route("/api/memory/deep", post(deep_memory_create_handler))
        .route("/api/memory/deep/{id}", put(deep_memory_update_handler))
        .route("/api/memory/deep/{id}", delete(deep_memory_delete_handler))
        .route("/api/history", get(history_handler))
        .route("/api/sessions", get(sessions_list_handler))
        .route("/api/sessions", post(sessions_create_handler))
        .route("/api/sessions/{id}", patch(sessions_rename_handler))
        .route("/api/sessions/{id}", delete(sessions_delete_handler))
        .route("/api/usage", get(usage_handler))
        .route("/api/usage/today", get(usage_today_handler))
        .route("/api/budget", get(budget_handler))
        .route("/api/sop/stats", get(sop_stats_handler))
        .route("/api/tools", get(tools_handler))
        .route("/api/tools/{name}/toggle", post(tools_toggle_handler))
        .route("/api/tools/builtin/{key}/toggle", post(builtin_tool_toggle_handler))
        .route("/api/tools/{name}/description", post(tools_desc_handler))
        .route("/api/config/files", get(config_files_handler))
        .route("/api/config/files/{name}", put(config_file_save_handler))
        .route("/api/checkpoints", get(checkpoints_list_handler))
        .route("/api/checkpoints/{id}", delete(checkpoints_delete_handler))
        .route("/api/settings/computer_use", post(computer_use_toggle_handler))
        .route("/api/settings/human_intervention", get(human_intervention_get_handler).post(human_intervention_toggle_handler))
        .route("/api/settings/heartbeat", get(heartbeat_get_handler).post(heartbeat_toggle_handler))
        .route("/api/settings/agent", post(agent_settings_save_handler))
        .route("/api/settings/agent/extended", post(agent_settings_extended_save_handler))
        .route("/api/settings/agent/expert", post(agent_settings_expert_save_handler))
        .route("/api/output/list", get(output_list_handler))
        .route("/api/output/download/{filename}", get(output_download_handler))
        .route("/api/output/open", post(output_open_handler))
        .route("/api/managed/runs", get(managed_runs_handler))
        .route("/api/managed/status", get(managed_status_handler))
        .route("/api/todos", get(todos_handler))
        .route("/workspace/{*path}", get(workspace_file_handler))
        .with_state(state)
}



async fn knowledge_list_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let ws = state.workspace_dir.clone();
    let files = crate::knowledge::list_files(&ws);
    let preferred = crate::knowledge::load_preferred(&ws);
    let items: Vec<Value> = files
        .into_iter()
        .map(|rel| {
            let pinned = preferred.iter().any(|p| p == &rel);
            let title = std::path::Path::new(&rel)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| rel.clone());
            json!({ "rel": rel, "title": title, "preferred": pinned })
        })
        .collect();
    Json(json!({ "files": items, "preferred": preferred, "count": items.len() }))
}

async fn knowledge_create_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let ws = state.workspace_dir.clone();
    let rel = body["rel"].as_str().unwrap_or("").to_string();
    let title = body["title"].as_str().unwrap_or("").to_string();
    let content = body["content"].as_str().unwrap_or("").to_string();
    if rel.is_empty() || content.is_empty() {
        return Json(json!({ "success": false, "error": "rel and content are required" }));
    }
    match crate::knowledge::create_file(&ws, &rel, &title, &content) {
        Ok(path) => Json(json!({ "success": true, "path": path })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

async fn knowledge_preferred_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let ws = state.workspace_dir.clone();
    let files: Vec<String> = body["files"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    match crate::knowledge::save_preferred(&ws, &files) {
        Ok(()) => Json(json!({ "success": true, "preferred": files })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

async fn knowledge_delete_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Json<Value> {
    let ws = state.workspace_dir.clone();
    match crate::knowledge::delete_file(&ws, &name) {
        Ok(()) => Json(json!({ "success": true })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

/// POST /api/knowledge/upload — write an uploaded Markdown document into the
/// knowledge corpus (knowledge/<rel>.md) and re-index it for search.
async fn knowledge_upload_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let ws = state.workspace_dir.clone();
    let rel = body["rel"].as_str().unwrap_or("").to_string();
    let content = body["content"].as_str().unwrap_or("").to_string();
    if rel.is_empty() || content.is_empty() {
        return Json(json!({ "success": false, "error": "rel and content are required" }));
    }
    match crate::knowledge::upload_file(&ws, &rel, &content) {
        Ok(path) => Json(json!({ "success": true, "path": path })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

/// POST /api/knowledge/rebuild — regenerate the global index and per-file
/// sidecars from the current files in the knowledge directory.
async fn knowledge_rebuild_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let ws = state.workspace_dir.clone();
    match crate::knowledge::build_index(&ws) {
        Ok(summary) => Json(json!({ "success": true, "summary": summary })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

/// POST /api/knowledge/refresh — re-scan the knowledge directory and return a
/// fresh (files, preferred) snapshot, pruning stale mounts, so the Knowledge
/// page mount list reflects files added/removed externally.
async fn knowledge_refresh_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let ws = state.workspace_dir.clone();
    let (files, preferred) = crate::knowledge::refresh(&ws);
    let items: Vec<Value> = files
        .iter()
        .map(|rel| {
            let pinned = preferred.iter().any(|p| p == rel);
            let title = std::path::Path::new(rel)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| rel.clone());
            json!({ "rel": rel, "title": title, "preferred": pinned })
        })
        .collect();
    Json(json!({ "files": items, "preferred": preferred, "count": items.len() }))
}

async fn index_handler(State(state): State<Arc<AppState>>) -> Response {
    StaticServer::serve_index(&state.workspace_dir)
}

async fn static_handler(State(state): State<Arc<AppState>>, Path(path): Path<String>) -> Response {
    StaticServer::serve_file(&path, &state.workspace_dir)
}

/// Serve files from workspace directory (e.g., output files, screenshots).
/// Includes path traversal protection — only serves files within workspace_dir.
async fn workspace_file_handler(State(state): State<Arc<AppState>>, Path(path): Path<String>) -> Response {
    use axum::http::{header, StatusCode};

    let workspace = std::path::Path::new(&state.workspace_dir);
    let file_path = workspace.join(&path);

    // Path traversal protection: ensure resolved path is within workspace
    let canonical = match file_path.canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let ws_canonical = match workspace.canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if !canonical.starts_with(&ws_canonical) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !canonical.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Determine content type from extension
    let mime = mime_guess::from_path(&canonical)
        .first_or_octet_stream();

    match tokio::fs::read(&canonical).await {
        Ok(data) => {
            let mut response = axum::body::Body::from(data).into_response();
            response.headers_mut().insert(header::CONTENT_TYPE, mime.to_string().parse().unwrap());
            response
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// List the most recent output artifacts in workspace/output/ (max 5).
/// GET /api/managed/runs — list Expert-mode run archives (F9 Dashboard).
/// Scans managed/<contract_id>/round_NN/ for plan / audit / state files
/// and returns a structured per-round view for the Runs dashboard.
async fn managed_runs_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let managed_dir = std::path::Path::new(&state.workspace_dir)
        .join("managed");

    let mut runs: Vec<(i64, Value)> = Vec::new();
    if let Ok(mut contracts) = tokio::fs::read_dir(&managed_dir).await {
        while let Ok(Some(entry)) = contracts.next_entry().await {
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            let contract_id = entry.file_name().to_string_lossy().to_string();
            let contract_dir = entry.path();
            // Get modification time for sorting (newest first)
            let mtime = contract_dir.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            let mut rounds: Vec<Value> = Vec::new();
            let mut total_rounds = 0usize;
            if let Ok(mut round_entries) = tokio::fs::read_dir(&contract_dir).await {
                let mut round_dirs: Vec<(u32, std::path::PathBuf)> = Vec::new();
                while let Ok(Some(re)) = round_entries.next_entry().await {
                    let name = re.file_name().to_string_lossy().to_string();
                    if let Some(round_str) = name.strip_prefix("round_") {
                        if let Ok(n) = round_str.parse::<u32>() {
                            round_dirs.push((n, re.path()));
                        }
                    }
                }
                round_dirs.sort_by_key(|(n, _)| *n);
                total_rounds = round_dirs.len();
                // Only process the last 5 rounds for display (others available via pagination)
                let display_rounds: Vec<_> = round_dirs.into_iter().rev().take(5).rev().collect();
                for (n, dir) in display_rounds {
                    let plan = tokio::fs::read_to_string(dir.join("plan.md")).await
                        .unwrap_or_default()
                        .chars().take(600).collect::<String>();
                    let audit = tokio::fs::read_to_string(dir.join("audit.json")).await
                        .unwrap_or_else(|_| "[]".to_string());
                    // Tool-call trace: count calls per tool + total duration.
                    let mut tool_calls: Vec<Value> = Vec::new();
                    let mut total_ms: u128 = 0;
                    let mut call_count = 0usize;
                    if let Ok(trace_str) = tokio::fs::read_to_string(dir.join("tool_calls.jsonl")).await {
                        for line in trace_str.lines() {
                            if let Ok(entry) = serde_json::from_str::<Value>(line) {
                                if let Some(tool) = entry["tool"].as_str() {
                                    if let Some(d) = entry["duration_ms"].as_u64() {
                                        total_ms += d as u128;
                                    }
                                    call_count += 1;
                                    tool_calls.push(json!({
                                        "tool": tool,
                                        "duration_ms": entry["duration_ms"].as_u64().unwrap_or(0),
                                        "ok": entry["ok"].as_bool().unwrap_or(true),
                                    }));
                                }
                            }
                        }
                    }
                    // Parse state.json for phase + findings count (best-effort).
                    let mut phase = String::new();
                    let mut findings = 0usize;
                    if let Ok(state_str) = tokio::fs::read_to_string(dir.join("state.json")).await {
                        if let Ok(state) = serde_json::from_str::<Value>(&state_str) {
                            phase = state["phase"].as_str().unwrap_or("").to_string();
                            findings = state["verified_findings"]
                                .as_array().map(|a| a.len()).unwrap_or(0);
                        }
                    }
                    rounds.push(json!({
                        "round": n,
                        "plan": plan,
                        "audit": audit,
                        "phase": phase,
                        "findings_count": findings,
                        "tool_calls": tool_calls,
                        "tool_call_count": call_count,
                        "tool_total_ms": total_ms,
                    }));
                }
            }

            runs.push((mtime, json!({
                "contract_id": contract_id,
                "round_count": rounds.len(),
                "total_rounds": total_rounds,
                "rounds": rounds,
            })));
        }
    }
    // Sort by modification time, newest first
    runs.sort_by(|a, b| b.0.cmp(&a.0));
    let runs: Vec<Value> = runs.into_iter().map(|(_, v)| v).collect();
    Json(json!({ "runs": runs }))
}

/// GET /api/managed/status — return current active Expert-mode task contract.
/// Returns round progress, findings, actions, and last 3 verified findings.
async fn managed_status_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.memory_store.get_latest_active_contract_global() {
        Ok(Some(json_str)) => {
            match serde_json::from_str::<Value>(&json_str) {
                Ok(c) => {
                    let findings = c["verified_findings"].as_array().cloned().unwrap_or_default();
                    let actions = c["verified_actions"].as_array().cloned().unwrap_or_default();
                    let open_leads = c["open_leads"].as_array().cloned().unwrap_or_default();
                    let remaining_work = c["remaining_work"].as_array().cloned().unwrap_or_default();
                    let last_3: Vec<Value> = findings.iter().rev().take(3).rev().cloned().collect();
                    Json(json!({
                        "active": true,
                        "task": c["original_task"].as_str().unwrap_or("").chars().take(80).collect::<String>(),
                        "phase": c["phase"].as_str().unwrap_or("unknown"),
                        "current_round": c["current_round"].as_u64().unwrap_or(0),
                        "max_rounds": c["max_rounds"].as_u64().unwrap_or(0),
                        "findings_count": findings.len(),
                        "actions_count": actions.len(),
                        "open_leads_count": open_leads.len(),
                        "remaining_work": remaining_work,
                        "last_findings": last_3,
                    }))
                }
                Err(_) => Json(json!({ "active": false })),
            }
        }
        _ => Json(json!({ "active": false })),
    }
}

async fn output_list_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let output_dir = std::path::Path::new(&state.workspace_dir).join("output");

    let mut files: Vec<Value> = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&output_dir).await {
        let mut items: Vec<(String, u64, i64)> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(meta) = entry.metadata().await {
                if meta.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let size = meta.len();
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    items.push((name, size, mtime));
                }
            }
        }
        // Sort by modification time, newest first
        items.sort_by(|a, b| b.2.cmp(&a.2));
        for (name, size, mtime) in items.into_iter().take(5) {
            files.push(json!({
                "name": name,
                "size": size,
                "modified": mtime,
                "url": format!("/workspace/output/{}", name),
                "download": format!("/api/output/download/{}", name),
            }));
        }
    }

    Json(json!({ "files": files, "dir": output_dir.to_string_lossy() }))
}

/// Download an output artifact with Content-Disposition: attachment.
async fn output_download_handler(
    State(state): State<Arc<AppState>>,
    Path(filename): Path<String>,
) -> Response {
    use axum::http::{header, StatusCode};

    // Prevent path traversal — only allow a plain file name
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let file_path = std::path::Path::new(&state.workspace_dir)
        .join("output")
        .join(&filename);

    match tokio::fs::read(&file_path).await {
        Ok(data) => {
            let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
            let mut response = axum::body::Body::from(data).into_response();
            response.headers_mut().insert(header::CONTENT_TYPE, mime.to_string().parse().unwrap());
            let disposition = format!("attachment; filename=\"{}\"", filename);
            response.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                disposition.parse().unwrap(),
            );
            response
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Open the workspace/output folder in the system file explorer.
async fn output_open_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let output_dir = std::path::Path::new(&state.workspace_dir).join("output");
    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        return Json(json!({ "success": false, "error": format!("Failed to create output dir: {}", e) }));
    }

    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer.exe").arg(&output_dir).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&output_dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&output_dir).spawn();

    match result {
        Ok(_) => Json(json!({ "success": true, "dir": output_dir.to_string_lossy() })),
        Err(e) => Json(json!({ "success": false, "error": format!("Failed to open folder: {}", e) })),
    }
}

/// GET /api/todos — return current TODO list from workspace/todos.json
#[derive(Deserialize)]
struct TodosQuery {
    session: Option<String>,
}

async fn todos_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TodosQuery>,
) -> Json<Value> {
    let session_id = query.session.unwrap_or_default();
    let todos_path = crate::tool::todo_update::todos_file_path(&state.workspace_dir, &session_id);
    if !todos_path.exists() {
        return Json(json!({ "items": [], "count": 0 }));
    }
    match tokio::fs::read_to_string(&todos_path).await {
        Ok(content) => {
            match serde_json::from_str::<Value>(&content) {
                Ok(data) => {
                    // Return the items array and count
                    let items = data.get("items").cloned().unwrap_or(json!([]));
                    let count = items.as_array().map(|a| a.len()).unwrap_or(0);
                    Json(json!({ "items": items, "count": count }))
                }
                Err(e) => {
                    warn!("Failed to parse todos.json: {}", e);
                    Json(json!({ "items": [], "count": 0, "error": format!("Parse error: {}", e) }))
                }
            }
        }
        Err(e) => {
            warn!("Failed to read todos.json: {}", e);
            Json(json!({ "items": [], "count": 0, "error": format!("Read error: {}", e) }))
        }
    }
}

async fn health_handler() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

/// 只读探测：当前这台机器上实际会选哪个浏览器（不会启动任何东西）。
/// Settings 与 Tools 页都要显示这一行，同一口径不能各算各的。
fn browser_probe_value(state: &AppState) -> Value {
    let configured = state
        .browser_executable
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();
    let discovery = crate::tool::browser_launch::discover(&configured);
    match discovery.chosen {
        Some(c) => json!({
            "path": c.path.to_string_lossy().to_string(),
            "source": c.source.label(),
            "version": crate::tool::browser_launch::version_from_layout(&c.path),
        }),
        None => json!({ "path": Value::Null, "source": "not found", "version": Value::Null }),
    }
}

async fn models_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let models = state.model_configs.read().await;
    let list: Vec<Value> = models.iter().map(|m| {
        json!({ "name": &m.name, "context_window": m.context_window, "supports_vision": m.supports_vision })
    }).collect();

    // Load config to get persisted settings
    let config = crate::config::Config::load(&state.workspace_dir).ok();
    let tool_permissions = config.as_ref()
        .map(|c| serde_json::to_value(&c.agent.tool_permissions).unwrap_or(json!({})))
        .unwrap_or(json!({}));

    // 当前机器上实际会选用的浏览器：自动探测选错时这一行就能看出来，
    // 不用先跑一次任务拿不到结果才知道。
    let browser_probe = browser_probe_value(&state);

    Json(json!({
        "models": list,
        "context_window_threshold": state.context_window_threshold.load(Ordering::SeqCst),
        "max_iterations": state.max_iterations.load(Ordering::SeqCst),
        "rabbit_hole_threshold": state.rabbit_hole_threshold.load(Ordering::SeqCst),
        "trim_redundant_tool_calls": state.trim_redundant_tool_calls.load(Ordering::SeqCst),
        "knowledge_pre_retrieval": state.knowledge_pre_retrieval.load(Ordering::SeqCst),
        "sop_replay": state.sop_replay.load(Ordering::SeqCst),
        "budget_dashboard": state.budget_dashboard.load(Ordering::SeqCst),
        "linux_ir_tools": state.linux_ir_tools.load(Ordering::SeqCst),
        "browser_headless": state.browser_headless.load(Ordering::SeqCst),
        "browser_executable": state.browser_executable.read().map(|g| g.clone()).unwrap_or_default(),
        "browser_enabled": state.browser_enabled.load(Ordering::SeqCst),
        "browser_probe": browser_probe,
        "two_tier_memory": state.two_tier_memory.load(Ordering::SeqCst),
        "enable_context_scaling": state.enable_context_scaling.load(Ordering::SeqCst),
        "max_inline_chars": state.max_inline_chars.load(Ordering::SeqCst),
        "skill_listing_strategy": crate::skill::SkillListingStrategy::from_index(state.skill_listing_strategy.load(Ordering::SeqCst)).as_str().to_string(),
        "skill_max_inline_chars": state.skill_max_inline_chars.load(Ordering::SeqCst),
        "skill_catalog_max": state.skill_catalog_max.load(Ordering::SeqCst),
        "skill_hot_top_k": state.skill_hot_top_k.load(Ordering::SeqCst),
        "tool_timeout_secs": state.tool_timeout_secs.load(Ordering::SeqCst),
        "max_tool_retries": state.max_tool_retries.load(Ordering::SeqCst),
        "primary_model": state.primary_model.read().unwrap().clone(),
        "fallback_model": state.fallback_model.read().unwrap().clone(),
        "expert_role_models": serde_json::to_value(state.expert_role_models.read().unwrap().clone()).unwrap_or(json!({})),
        "timezone_offset": *state.timezone_offset.read().unwrap(),
        "tool_permissions": tool_permissions,
    }))
}

async fn providers_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let models = state.model_configs.read().await;
    let list: Vec<Value> = models.iter().map(|m| {
        let masked_key = m.api_key.as_ref().map(|k| {
            if k.len() > 8 { format!("{}...{}", &k[..4], &k[k.len()-4..]) }
            else if !k.is_empty() { "****".to_string() }
            else { String::new() }
        });
        json!({
            "title": m.title,
            "name": m.name,
            "api_base": m.api_base,
            "api_key": masked_key,
            "api_key_env": m.api_key_env,
            "context_window": m.context_window,
            "max_tokens": m.max_tokens,
            "temperature": m.temperature,
        })
    }).collect();
    Json(json!({ "providers": list, "count": list.len() }))
}

async fn providers_create_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let name = body["name"].as_str().unwrap_or("").to_string();
    let api_base = body["api_base"].as_str().unwrap_or("").to_string();
    if name.is_empty() || api_base.is_empty() {
        return Json(json!({"error": "name and api_base are required"}));
    }
    let title_raw = body["title"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        .unwrap_or_else(|| name.clone());
    let mut models = state.model_configs.write().await;
    if models.iter().any(|m| m.title == title_raw) {
        return Json(json!({"error": format!("Provider title '{}' already exists", title_raw)}));
    }
    let new_config = crate::config::ModelConfig {
        title: title_raw.clone(),
        name: name.clone(),
        api_base,
        api_key: body["api_key"].as_str().map(|s| s.to_string()).filter(|s| !s.is_empty()),
        api_key_env: body["api_key_env"].as_str().map(|s| s.to_string()).filter(|s| !s.is_empty()),
        context_window: body["context_window"].as_u64().map(|v| v as usize).unwrap_or(128000),
        max_tokens: body["max_tokens"].as_u64().map(|v| v as u32).unwrap_or(16384),
        temperature: body["temperature"].as_f64().unwrap_or(0.7),
        supports_vision: body["supports_vision"].as_bool().unwrap_or(false),
    };
    models.push(new_config);
    crate::model_store::save_configs(&models, std::path::Path::new(&state.model_store_path));
    info!("Provider '{}' (title {}) added via API", name, title_raw);
    Json(json!({"ok": true, "name": name, "title": title_raw}))
}

async fn providers_update_handler(
    State(state): State<Arc<AppState>>,
    Path(title): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut models = state.model_configs.write().await;
    let idx = match models.iter().position(|m| m.title == title) {
        Some(i) => i,
        None => return Json(json!({"error": format!("Provider title '{}' not found", title)})),
    };
    let existing = &models[idx];
    // Preserve existing api_key if the incoming one is empty or looks like a masked value
    let incoming_key = body["api_key"].as_str().unwrap_or("").to_string();
    let api_key = if incoming_key.is_empty() || incoming_key.contains("...") || incoming_key == "****" {
        existing.api_key.clone()
    } else {
        Some(incoming_key)
    };
    let fallback_title = models[idx].title.clone();
    let new_title = body["title"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or(fallback_title);
    if new_title != title && models.iter().position(|m| m.title == new_title).is_some() {
        return Json(json!({"error": format!("Provider title '{}' already exists", new_title)}));
    }
    let fallback_name = models[idx].name.clone();
    models[idx] = crate::config::ModelConfig {
        title: new_title.clone(),
        name: body["name"].as_str().map(|s| s.to_string()).unwrap_or(fallback_name),
        api_base: body["api_base"].as_str().map(|s| s.to_string()).unwrap_or_else(|| existing.api_base.clone()),
        api_key,
        api_key_env: body["api_key_env"].as_str().map(|s| s.to_string()).filter(|s| !s.is_empty())
            .or_else(|| existing.api_key_env.clone()),
        context_window: body["context_window"].as_u64().map(|v| v as usize).unwrap_or(existing.context_window),
        max_tokens: body["max_tokens"].as_u64().map(|v| v as u32).unwrap_or(existing.max_tokens),
        temperature: body["temperature"].as_f64().unwrap_or(existing.temperature),
        supports_vision: body["supports_vision"].as_bool().unwrap_or(existing.supports_vision),
    };
    crate::model_store::save_configs(&models, std::path::Path::new(&state.model_store_path));
    info!("Provider title '{}' updated via API", title);
    Json(json!({"ok": true, "name": models[idx].name.clone(), "title": new_title}))
}

async fn providers_delete_handler(
    State(state): State<Arc<AppState>>,
    Path(title): Path<String>,
) -> Json<Value> {
    let mut models = state.model_configs.write().await;
    let len_before = models.len();
    models.retain(|m| m.title != title);
    if models.len() == len_before {
        return Json(json!({"error": format!("Provider title '{}' not found", title)}));
    }
    crate::model_store::save_configs(&models, std::path::Path::new(&state.model_store_path));
    info!("Provider title '{}' deleted via API", title);
    Json(json!({"ok": true, "title": title}))
}

/// POST /api/providers/{title}/test - verify a provider/model is reachable and
/// correctly configured by sending a tiny chat request and reporting latency.
async fn providers_test_handler(
    State(state): State<Arc<AppState>>,
    Path(title): Path<String>,
) -> Json<Value> {
    let model = { let models = state.model_configs.read().await; models.iter().find(|m| m.title == title).cloned() };
    let Some(model) = model else { return Json(json!({"ok": false, "error": format!("Provider title '{}' not found", title)})); };
    match state.provider.test_connection_for(&model).await {
        Ok((latency_ms, reply)) => {
            info!("Provider '{}' (title {}) tested OK ({} ms)", model.name, title, latency_ms);
            Json(json!({"ok": true, "name": model.name, "title": title, "latency_ms": latency_ms, "reply": reply}))
        }
        Err(e) => {
            warn!("Provider '{}' (title {}) test failed: {}", model.name, title, e);
            Json(json!({"ok": false, "name": model.name, "title": title, "error": e}))
        }
    }
}

async fn skills_handler(State(state): State<Arc<AppState>>) -> Json<Value> {

    let skills = state.skill_manager.list();
    Json(json!({ "skills": skills, "count": skills.len() }))
}

async fn skills_metrics_handler() -> Json<Value> {
    let metrics = crate::skill::metrics::snapshot();
    crate::skill::metrics::save();
    Json(metrics)
}

async fn skills_create_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let name = body["name"].as_str().unwrap_or("").to_string();
    let description = body["description"].as_str().unwrap_or("").to_string();
    let content = body["content"].as_str().unwrap_or("").to_string();
    if name.is_empty() || content.is_empty() {
        return Json(json!({ "success": false, "error": "Name and content are required" }));
    }
    match state.skill_manager.create_skill(&name, &description, &content) {
        Ok(filename) => Json(json!({ "success": true, "filename": filename })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

async fn skills_reload_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    state.skill_manager.reload();
    let skills = state.skill_manager.list();
    Json(json!({ "status": "reloaded", "count": skills.len() }))
}

/// POST /api/managed/reset — Clear all active (non-completed) Expert mode task contracts.
/// This resets the Expert mode state so the next Expert task starts fresh.
async fn managed_reset_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.memory_store.clear_active_contracts() {
        Ok(deleted) => {
            info!("Managed plans reset: {} active contract(s) cleared", deleted);
            Json(json!({ "success": true, "deleted": deleted }))
        }
        Err(e) => {
            error!("Failed to reset managed plans: {}", e);
            Json(json!({ "success": false, "error": e }))
        }
    }
}

async fn skills_delete_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Json<Value> {
    match state.skill_manager.delete_skill(&name) {
        Ok(_) => Json(json!({ "success": true })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

async fn skills_toggle_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Json<Value> {
    match state.skill_manager.toggle_skill(&name) {
        Some(enabled) => Json(json!({ "success": true, "enabled": enabled })),
        None => Json(json!({ "success": false, "error": "Not found" })),
    }
}

async fn mcp_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mgr = state.mcp_manager.lock().await;
    Json(json!({ "servers": mgr.server_info() }))
}

async fn mcp_create_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let name = body["name"].as_str().unwrap_or("").to_string();
    if name.is_empty() {
        return Json(json!({ "success": false, "error": "Missing name" }));
    }
    let transport = body["transport"].as_str().unwrap_or("stdio").to_string();
    let config = McpServerConfig {
        name: name.clone(),
        transport,
        command: body["command"].as_str().map(|s| s.to_string()),
        args: body["args"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        url: body["url"].as_str().map(|s| s.to_string()),
        auth_token: body["auth_token"].as_str().map(|s| s.to_string()),
        enabled: body["enabled"].as_bool().unwrap_or(true),
    };
    let mut mgr = state.mcp_manager.lock().await;
    // Snapshot old MCP tool names before connecting
    let old_names = mgr.tool_names();
    mgr.connect_server(&config).await;
    mgr.save_configs();
    // Sync registry: remove old, add new
    let new_names = mgr.tool_names();
    let mcp_tools = mgr.get_tools();
    drop(mgr);
    let mut registry = state.tools.write().await;
    registry.unregister_many(&old_names);
    for tool in &mcp_tools {
        registry.register(tool.clone());
    }
    info!("MCP registry synced: {} tools after create '{}'", new_names.len(), name);
    Json(json!({ "success": true, "name": name }))
}

async fn mcp_delete_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Json<Value> {
    let mut mgr = state.mcp_manager.lock().await;
    let old_names = mgr.tool_names();
    let ok = mgr.remove_server(&name).await;
    if ok {
        mgr.save_configs();
        let new_names = mgr.tool_names();
        let mcp_tools = mgr.get_tools();
        drop(mgr);
        let mut registry = state.tools.write().await;
        registry.unregister_many(&old_names);
        for tool in &mcp_tools {
            registry.register(tool.clone());
        }
        info!("MCP registry synced: {} tools after delete '{}'", new_names.len(), name);
    }
    Json(json!({ "success": ok }))
}

async fn mcp_toggle_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Json<Value> {
    let mut mgr = state.mcp_manager.lock().await;
    let old_names = mgr.tool_names();
    match mgr.toggle_server(&name).await {
        Some(enabled) => {
            mgr.save_configs();
            let mcp_tools = mgr.get_tools();
            drop(mgr);
            let mut registry = state.tools.write().await;
            registry.unregister_many(&old_names);
            for tool in &mcp_tools {
                registry.register(tool.clone());
            }
            Json(json!({ "success": true, "enabled": enabled }))
        }
        None => Json(json!({ "success": false, "error": "Not found" })),
    }
}

async fn mcp_restart_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Json<Value> {
    let mut mgr = state.mcp_manager.lock().await;
    let old_names = mgr.tool_names();
    let ok = mgr.reconnect_server(&name).await;
    if ok {
        mgr.save_configs();
        let mcp_tools = mgr.get_tools();
        drop(mgr);
        let mut registry = state.tools.write().await;
        registry.unregister_many(&old_names);
        for tool in &mcp_tools {
            registry.register(tool.clone());
        }
    }
    Json(json!({ "success": ok }))
}

#[derive(Deserialize)]
struct LogsQuery {
    date: Option<String>,
}

async fn logs_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LogsQuery>,
) -> Json<Value> {
    let date = query
        .date
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    match state.logger.read_logs(&date) {
        Ok(entries) => Json(json!({ "date": date, "entries": entries, "count": entries.len() })),
        Err(e) => Json(json!({ "error": e })),
    }
}

async fn log_dates_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let dates = state.logger.available_dates();
    Json(json!({ "dates": dates }))
}

// ============================================================
// CRON Task Handlers
// ============================================================

async fn cron_list_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let scheduler = state.scheduler.lock().await;
    let tasks = scheduler.list();
    Json(json!({ "tasks": tasks, "count": tasks.len() }))
}

async fn cron_create_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let task = CronTask {
        id: String::new(),
        name: body["name"].as_str().unwrap_or("Unnamed").to_string(),
        schedule: body["schedule"].as_str().unwrap_or("every 1h").to_string(),
        message: body["message"].as_str().unwrap_or("").to_string(),
        model: body["model"].as_str().unwrap_or("").to_string(),
        enabled: body["enabled"].as_bool().unwrap_or(true),
        last_run: None,
        next_run: None,
        interval_secs: 0,
        start_date: body["start_date"].as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        end_date: body["end_date"].as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        start_time: body["start_time"].as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        end_time: body["end_time"].as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    let mut scheduler = state.scheduler.lock().await;
    let created = scheduler.create(task);
    Json(json!({ "task": created }))
}

async fn cron_update_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut scheduler = state.scheduler.lock().await;
    let ok = scheduler.update(
        &id,
        body["name"].as_str().map(|s| s.to_string()),
        body["schedule"].as_str().map(|s| s.to_string()),
        body["message"].as_str().map(|s| s.to_string()),
        body["model"].as_str().map(|s| s.to_string()),
        body["start_date"].as_str().map(|s| s.to_string()),
        body["end_date"].as_str().map(|s| s.to_string()),
        body["start_time"].as_str().map(|s| s.to_string()),
        body["end_time"].as_str().map(|s| s.to_string()),
    );
    Json(json!({ "success": ok }))
}

async fn cron_delete_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let mut scheduler = state.scheduler.lock().await;
    let ok = scheduler.delete(&id);
    Json(json!({ "success": ok }))
}

async fn cron_toggle_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let mut scheduler = state.scheduler.lock().await;
    let ok = scheduler.toggle(&id);
    let enabled = if ok {
        scheduler.list().iter().find(|t| t.id == id).map(|t| t.enabled)
    } else {
        None
    };
    Json(json!({ "success": ok, "enabled": enabled }))
}

async fn cron_settings_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let scheduler = state.scheduler.lock().await;
    Json(json!({ "auto_approve": scheduler.auto_approve() }))
}

async fn cron_settings_update_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let on = body["auto_approve"].as_bool().unwrap_or(false);
    let mut scheduler = state.scheduler.lock().await;
    scheduler.set_auto_approve(on);
    Json(json!({ "success": true, "auto_approve": scheduler.auto_approve() }))
}

/// POST /api/notify — push a notification message to all connected WebSocket clients.
async fn notify_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let message = body["message"].as_str().unwrap_or("");
    if message.is_empty() {
        return Json(json!({ "success": false, "error": "Missing message" }));
    }
    // Build a WS-formatted notification JSON
    let ws_msg = json!({
        "type": "notification",
        "message": message,
        "timestamp": chrono::Utc::now().to_rfc3339()
    }).to_string();
    match state.notify_tx.send(ws_msg) {
        Ok(n) => Json(json!({ "success": true, "delivered_to": n })),
        Err(_) => Json(json!({ "success": false, "delivered_to": 0 })),
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}


/// Outcome of a best-effort, time-bounded WebSocket send.
#[derive(Clone, Copy, PartialEq)]
enum WsSendOutcome {
    /// Sent and accepted by the client's read path.
    Sent,
    /// The send stalled (client not reading, or the shared sink mutex was
    /// contended by another task) and the message was dropped so the agent
    /// pipeline can keep advancing. The connection is still considered usable.
    Dropped,
    /// The send failed because the connection is closed / gone.
    Closed,
}

/// Best-effort WebSocket send that is bounded in time. Without this, a slow or
/// stalled browser tab (TCP/WS backpressure, or another task holding the shared
/// sink mutex across a long `send` await) freezes the single server event loop,
/// which in turn back-pressures the bounded Manager->Executor channels and can
/// deadlock an entire multi-agent run with no recovery. On timeout we drop the
/// message and continue instead of blocking forever.
async fn ws_send_bounded(
    ws_sink: &Arc<Mutex<futures::stream::SplitSink<WebSocket, Message>>>,
    msg: String,
) -> WsSendOutcome {
    use futures::SinkExt;
    let fut = async {
        let mut sink = ws_sink.lock().await;
        sink.send(Message::Text(msg.into())).await
    };
    match tokio::time::timeout(std::time::Duration::from_secs(2), fut).await {
        Ok(Ok(())) => WsSendOutcome::Sent,
        Ok(Err(_)) => WsSendOutcome::Closed,
        Err(_) => {
            warn!("ws send timed out; dropping 1 message to avoid pipeline deadlock");
            WsSendOutcome::Dropped
        }
    }
}

/// Inject the owning session_id into a forwarded WS message (parallel
/// multi-session). All sessions share one sink; without this marker the
/// frontend cannot attribute an event to the correct session.
fn ws_msg_for_session(msg: String, session_id: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(&msg) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "session".to_string(),
                    serde_json::Value::String(session_id.to_string()),
                );
            }
            v.to_string()
        }
        Err(_) => msg,
    }
}

/// Sessions whose end-of-session distillation already ran for the current task
/// generation. A new run clears the mark (see `debrief_forget`), so a long-lived
/// session stays debriefable again after fresh work.
static DEBRIEF_DONE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>>
    = std::sync::OnceLock::new();

fn debrief_registry() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    DEBRIEF_DONE.get_or_init(Default::default)
}

fn debrief_forget(session_id: &str) {
    if let Ok(mut s) = debrief_registry().lock() {
        s.remove(session_id);
    }
}

/// End-of-session conclusion solidification (case writeup + optional SOP).
///
/// `history.len() >= 4` gate keeps a greeting-only session from authoring noise.
/// Fires from whoever observes the session's LAST run finish:
/// * the drain task, when the client was already gone (a refresh only detaches a
///   run now, so `handle_ws` has long since returned for that session);
/// * the WebSocket teardown path, when nothing was in flight.
///
/// [`DEBRIEF_DONE`] dedups the two, so a mid-run refresh can neither distill a
/// half-finished conversation nor let a detached run finish unheard-and-undistilled.
async fn maybe_debrief(state: &Arc<AppState>, session_id: &str, trigger: &str) {
    let short = &session_id[..8.min(session_id.len())];
    if !state.debrief_enabled.load(Ordering::SeqCst) {
        return;
    }
    if state.session_is_running(session_id) {
        info!("[session:{}] debrief skipped on {}: run still in flight", short, trigger);
        return;
    }
    if state.skill_used_sessions.lock().unwrap().contains(&session_id.to_string()) {
        info!("Session {} was skill-driven; skipping debrief", short);
        return;
    }
    if debrief_registry().lock().map(|s| s.contains(session_id)).unwrap_or(false) {
        return;
    }
    let history = state.sessions.lock().await.get(session_id).cloned().unwrap_or_default();
    if history.len() < 4 {
        return;
    }
    let tool_log = state
        .session_tool_log
        .lock()
        .await
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    if let Ok(mut s) = debrief_registry().lock() {
        s.insert(session_id.to_string());
    }
    let provider = state.provider.clone();
    let model_name = state
        .model_configs
        .read()
        .await
        .first()
        .map(|m| m.name.clone())
        .unwrap_or_default();
    let workspace_dir = state.workspace_dir.clone();
    let sid = session_id.to_string();
    info!("[session:{}] Solidifying session conclusions ({})", short, trigger);
    tokio::spawn(async move {
        match crate::debrief::run(provider, &model_name, &workspace_dir, &history, &tool_log).await {
            Ok(Some(out)) => {
                info!(
                    "Session {} debrief: case_note={:?} sop={:?}",
                    &sid[..8.min(sid.len())],
                    out.case_note_path,
                    out.sop_id
                );
            }
            Ok(None) => info!("Session {} debrief: nothing worth solidifying", &sid[..8.min(sid.len())]),
            Err(e) => warn!("Session {} debrief failed: {}", &sid[..8.min(sid.len())], e),
        }
    });
}

/// Drain a single session agent event stream and persist its outcome.
///
/// Slice-2 (parallel multi-session): each run (Instant AND Expert) is drained by
/// its own spawned task that ONLY consumes the agent event stream. It no longer
/// reads the shared client `ws_rx` — that is owned exclusively by the demux in
/// `handle_ws`, which routes stop / interject / permission responses into
/// per-session flags and the static interject queues. Cancellation is detected
/// by polling the per-session cancel flag (or the connection-level one).
///
/// Transport is NOT cancellation: events go through a [`SinkSlot`], so a dead
/// connection detaches the drain (it keeps consuming the stream, keeps the agent
/// loop alive, and still persists the final answer) instead of dropping the
/// stream — which used to close the agent's event channel and abort a running
/// task mid-flight. Only an explicit STOP breaks here, and it drops the stream
/// on purpose so the agent loop observes a closed channel and unwinds.
async fn drain_session_stream(
    state: Arc<AppState>,
    slot: SinkSlot,
    model: String,
    session_id: String,
    content: String,
    managed: bool,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    session_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    deep_injected: Vec<(String, String)>,
    mut event_stream: crate::agent::EventStream,
) {
    let mut assistant_text = String::new();
    let mut srv_events: u64 = 0;
    let mut srv_last = std::time::Instant::now();
    let mut detached = false;
    loop {
        tokio::select! {
            result = event_stream.next() => {
                match result {
                    Some(Ok(event)) => {
                        srv_events += 1;
                        if srv_last.elapsed().as_secs() >= 5 {
                            info!("[session:{}] server event loop alive: {} events", session_id, srv_events);
                            srv_last = std::time::Instant::now();
                        }
                        if let AgentEvent::TextDelta { content: c, .. } = &event {
                            assistant_text.push_str(c);
                        }
                        // 权威工具计数：记录每个 ToolResult 的成败，供会话结束时
                        // Debrief 门控使用（不再从 assistant 文本猜测）。
                        if let AgentEvent::ToolResult { name, result, .. } = &event {
                            let success = result.get("error").is_none();
                            let mut log = state.session_tool_log.lock().await;
                            let entries = log.entry(session_id.clone()).or_default();
                            if entries.len() < 200 {
                                entries.push((name.clone(), success));
                            }
                        }
                        if let AgentEvent::Usage { model: _, prompt_tokens, completion_tokens, total_tokens, cached_tokens, .. } = &event {
                            let ms = state.memory_store.clone();
                            let mdl = model.clone();
                            let pt = *prompt_tokens;
                            let ct = *completion_tokens;
                            let tt = *total_tokens;
                            let cst = *cached_tokens;
                            let sid = session_id.clone();
                            tokio::task::spawn_blocking(move || {
                                let _ = ms.record_usage(&mdl, pt, ct, tt, cst, &sid);
                            });
                        }
                        let msg_str = ws_msg_for_session(event.to_ws_message(), &session_id);
                        match slot_forward(&slot, msg_str).await {
                            Forward::Detached | Forward::NoClient if !detached => {
                                detached = true;
                                info!("[session:{}] Client gone; run continues in background (will persist the answer, re-attach on reconnect)", session_id);
                            }
                            Forward::Detached | Forward::NoClient => {}
                            _ => {
                                if detached {
                                    detached = false;
                                    info!("[session:{}] Client re-attached; resuming live streaming", session_id);
                                }
                            }
                        }
                        if event.is_done() {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        let err_event = AgentEvent::error(&e.to_string(), &session_id, "system");
                        let msg_str = ws_msg_for_session(err_event.to_ws_message(), &session_id);
                        let _ = slot_forward(&slot, msg_str).await;
                        break;
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                // Periodic wake so a long tool call that emits no events can still
                // observe a per-session stop in real time (the old inline drain did
                // this via the shared ws_rx channel).
            }
        }
        // Stop if EITHER the per-session flag is set (session STOP — Instant OR
        // Expert) or the connection-level flag is set (disconnect / disconnect
        // propagation). OR-ing keeps disconnect-terminates-Expert intact.
        let stop_requested =
            session_cancel.as_ref().map(|c| c.load(std::sync::atomic::Ordering::SeqCst)).unwrap_or(false)
            || cancelled.load(std::sync::atomic::Ordering::SeqCst);
        if stop_requested {
            info!("Agent execution stopped by user");
            if managed {
                state.memory_store.set_contract_stopped(&session_id);
                info!("[managed:{}] Set USER_STOPPED marker on TaskContract", session_id);
            }
            let stop_event = AgentEvent::text("\n\n*[Stopped by user]*", &session_id, "system");
            let msg_str = ws_msg_for_session(stop_event.to_ws_message(), &session_id);
            let _ = slot_forward(&slot, msg_str).await;
            let done_event = AgentEvent::done(&session_id, "system");
            let msg_str = ws_msg_for_session(done_event.to_ws_message(), &session_id);
            let _ = slot_forward(&slot, msg_str).await;
            spawn_deep_curator(state.clone(), &model, &session_id, &content, &assistant_text);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            break;
        }
    }

    // 记忆 KPI：loaded = 本轮注入的深层事实；referenced = 回复文本真正命中
    // 其关键词的事实。只降权、不删除（自然选择，不是清除）。
    //
    // Gate on visible text: a tool-only round, a stopped round and a round that
    // died before producing a body can never score a `referenced` hit, so
    // recording `loaded` for those would silently down-weight facts the model
    // never actually had a chance to use.
    if !deep_injected.is_empty() && !assistant_text.trim().is_empty() {
        let loaded: Vec<String> = deep_injected.iter().map(|(id, _)| id.clone()).collect();
        let referenced: Vec<String> = deep_injected
            .iter()
            .filter(|(_, body)| crate::deep_memory::body_referenced(body, &assistant_text))
            .map(|(id, _)| id.clone())
            .collect();
        let ms = state.memory_store.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = ms.deep_record_usage(&loaded, &referenced);
        });
    }
    // Persist final assistant text: deep memory, session history, SQLite.
    two_tier_write(&state, &mut assistant_text, &session_id, content.as_str());
    spawn_deep_curator(state.clone(), &model, &session_id, &content, &assistant_text);
    if !assistant_text.is_empty() {
        let mut sessions = state.sessions.lock().await;
        let hist = sessions.entry(session_id.clone()).or_insert_with(Vec::new);
        hist.push(ChatMessage::assistant(&assistant_text));
        if hist.len() > 50 {
            let drop_n = hist.len() - 50;
            hist.drain(..drop_n);
        }
        let _ = state.memory_store.store_entry(&session_id, "assistant", &assistant_text, None);
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let _ = state.memory_store.auto_summarize_date(&today);
    }

    // Release the parallel-execution slot (Instant runs only; managed had none).
    if session_cancel.is_some() {
        state.session_slot_release(&session_id);
    }
    // Nobody is listening: this drain is the last observer of the session, so it
    // owns end-of-session solidification (the WebSocket path already returned).
    if detached {
        maybe_debrief(&state, &session_id, "run-end, no client").await;
        // Queued follow-ups are dispatched by the per-connection loop, which is
        // gone. They survive in the global queue and flush on the next connect;
        // log it so an orphaned queue is never silent.
        if crate::interject::has_pending(&session_id) {
            warn!(
                "[session:{}] Run finished with no client attached and queued follow-ups pending; they will dispatch on the next connect",
                &session_id[..8.min(session_id.len())]
            );
        }
    }
}
async fn handle_ws(socket: WebSocket, state: Arc<AppState>) {
    use futures::SinkExt;

    let (mut ws_sink, mut ws_stream) = socket.split();
    info!("WebSocket client connected");

    // Phase 1: Authentication
    let authenticated = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        ws_stream.next(),
    )
    .await
    {
        Ok(Some(Ok(Message::Text(msg)))) => {
            let msg_str: String = msg.to_string();
            match serde_json::from_str::<Value>(&msg_str) {
                Ok(parsed) if parsed["type"] == "auth" => {
                    let pwd = parsed["password"].as_str().unwrap_or("");
                    if pwd == state.password {
                        let _ = ws_sink
                            .send(Message::Text(json!({"type":"auth_ok"}).to_string().into()))
                            .await;
                        true
                    } else {
                        let _ = ws_sink
                            .send(Message::Text(
                                json!({"type":"auth_fail","message":"Invalid password"})
                                    .to_string()
                                    .into(),
                            ))
                            .await;
                        false
                    }
                }
                _ => {
                    let _ = ws_sink
                        .send(Message::Text(
                            json!({"type":"auth_fail","message":"Send {type:'auth', password:'...'} first"})
                                .to_string().into(),
                        ))
                        .await;
                    false
                }
            }
        }
        _ => false,
    };

    if !authenticated {
        info!("Auth failed, closing connection");
        return;
    }
    info!("Client authenticated");

    // Phase 2: Chat loop with dedicated reader task
    let ws_sink = Arc::new(Mutex::new(ws_sink));
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut session_id = session_id; // mutable: may be replaced by the client's persistent session id

    // Single dedicated reader task: owns ws_stream, forwards ALL messages via channel.
    // This eliminates the race condition where two tasks compete for the same stream.
    let (ws_tx, mut ws_rx) = tokio::sync::mpsc::channel::<Message>(50);
    tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_stream.next().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
        // Signal stream ended
        let _ = ws_tx.send(Message::Close(None)).await;
    });

    // Subscribe to broadcast notifications and forward to this client's sink
    let mut notify_rx = state.notify_tx.subscribe();
    let notify_sink = ws_sink.clone();
    tokio::spawn(async move {
        while let Ok(msg) = notify_rx.recv().await {
            if matches!(ws_send_bounded(&notify_sink, msg).await, WsSendOutcome::Closed) {
                break;
            }
        }
    });

    let cancelled = Arc::new(AtomicBool::new(false));

    loop {
        // If a follow-up task is queued (sent while the previous task ran, no
        // "insert" click), dispatch it as the next sequential task BEFORE
        // waiting for new user input - but ONLY when no run is in flight.
        // Popping while a run is active just hits the session_is_running
        // re-queue below, turning the loop into a busy-spin (pop -> push ->
        // pop every few ms, re-running all memory injections each pass) until
        // the in-flight run finishes.
        // Queued user interjections are always dispatched into the execution
        // queue (FIFO), even after a Stop. Stop only cancels the in-flight
        // task; already-queued user messages must still execute.
        let user_msg = if state.session_is_running(&session_id) {
            // Run in flight: leave the follow-up queue untouched. Wait briefly
            // for new client input (stop/interject/new chat) or a timeout,
            // then re-check whether the run has finished.
            match tokio::time::timeout(std::time::Duration::from_millis(100), ws_rx.recv()).await {
                Ok(m) => m,
                Err(_) => continue,
            }
        } else {
            match crate::interject::pop_pending(&session_id) {
                Some(next_content) => {
                    info!("[session:{}] Dispatching queued follow-up task", session_id);
                    // Tell the client this queued interjection is now entering the
                    // execution queue so it can render a user-side bubble. It must
                    // NOT be shown earlier (only once it actually starts running).
                    let run_content = next_content.clone();
                    let _ = ws_send_bounded(&ws_sink, json!(
                        {"type":"queued_run","content":run_content,"session":session_id}).to_string()).await;
                    let msg_json = json!({ "type": "chat", "content": next_content });
                    Some(Message::Text(msg_json.to_string().into()))
                }
                _ => ws_rx.recv().await,
            }
        };
        let user_msg = match user_msg {
            Some(msg) => msg,
            None => break,
        };

        match user_msg {
            Message::Text(text) => {
                let text_str: String = text.to_string();
                if let Ok(parsed) = serde_json::from_str::<Value>(&text_str) {
                    let msg_type = parsed["type"].as_str().unwrap_or("");

                    match msg_type {
                        "chat" => {
                            let mut content = parsed["content"].as_str().unwrap_or("").to_string();
                            let default_model = {
                                let mc = state.model_configs.read().await;
                                mc.first().map(|m| m.name.clone()).unwrap_or_else(|| "gpt-4o".to_string())
                            };
                            let model = parsed["model"]
                                .as_str()
                                .unwrap_or(&default_model)
                                .to_string();
                            let max_iter = parsed["max_iterations"]
                                .as_u64()
                                .map(|v| v as usize)
                                .unwrap_or(state.max_iterations.load(Ordering::SeqCst));
                            let fallback_model = parsed["fallback_model"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(|s| s.to_string());
                            let rabbit_hole = parsed["rabbit_hole_threshold"]
                                .as_u64()
                                .map(|v| v as usize)
                                .unwrap_or(state.rabbit_hole_threshold.load(Ordering::SeqCst));
                            let ctx_window_threshold = parsed["context_window_threshold"]
                                .as_u64()
                                .map(|v| v as usize)
                                .unwrap_or(state.context_window_threshold.load(Ordering::SeqCst));
                            let tool_timeout = parsed["tool_timeout_secs"]
                                .as_u64()
                                .map(|v| v as usize)
                                .unwrap_or(state.tool_timeout_secs.load(Ordering::SeqCst));
                            let max_retries = parsed["max_tool_retries"]
                                .as_u64()
                                .map(|v| v as usize)
                                .unwrap_or(state.max_tool_retries.load(Ordering::SeqCst));
                            let ctx_window = {
                                let mc = state.model_configs.read().await;
                                mc.iter().find(|m| m.name == model).map(|m| m.context_window).unwrap_or(128000)
                            };

                            // Parse optional images (base64 data URIs or URLs)
                            let images: Vec<String> = parsed["images"]
                                .as_array()
                                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                .unwrap_or_default();

                            // Parse optional attachments (document files as base64 data URLs).
                            // Save each to workspace/output/attachments/ and prepend the path
                            // to the message so the Agent can file_read it.
                            if let Some(attachments) = parsed["attachments"].as_array() {
                                let att_dir = std::path::Path::new(&state.workspace_dir)
                                    .join("output").join("attachments");
                                let _ = std::fs::create_dir_all(&att_dir);
                                let mut entry = String::new();
                                for v in attachments {
                                    let name = match v["name"].as_str() {
                                        Some(n) if !n.contains("..") && !n.contains('/') && !n.contains('\\') => n,
                                        _ => continue,
                                    };
                                    let data_url = match v["data"].as_str() {
                                        Some(d) => d,
                                        None => continue,
                                    };
                                    // data URL: "data:[<mediatype>][;base64],<base64>"
                                    if let Some(base64_str) = data_url.split(',').nth(1) {
                                        use ::base64::Engine as _;
                                        let engine = ::base64::engine::general_purpose::STANDARD;
                                        if let Ok(bytes) = engine.decode(base64_str) {
                                            let file_path = att_dir.join(name);
                                            if let Err(e) = tokio::fs::write(&file_path, &bytes).await {
                                                tracing::warn!("Failed to save attachment {}: {}", name, e);
                                            } else {
                                                entry.push_str(&format!(
                                                    "\n*附件已保存到: {}*\n",
                                                    file_path.to_string_lossy()
                                                ));
                                            }
                                        }
                                    }
                                }
                                if !entry.is_empty() {
                                    content = format!("{}\n\n---\n{}", entry, content);
                                }
                            }

                            // If images are present, check that the model supports vision
                            if !images.is_empty() {
                                let supports_vision = {
                                    let mc = state.model_configs.read().await;
                                    mc.iter().find(|m| m.name == model).map(|m| m.supports_vision).unwrap_or(false)
                                };
                                if !supports_vision {
                                    let err_msg = format!("Model '{}' does not support image input. Please select a vision-capable model (e.g., gpt-4o).", model);
                                    let err_event = serde_json::json!({
                                        "type": "error",
                                        "message": err_msg
                                    });
                                    let mut sink = ws_sink.lock().await;
                                    let _ = sink.send(Message::Text(err_event.to_string().into())).await;
                                    continue;
                                }
                            }

                            if content.is_empty() && images.is_empty() {
                                continue;
                            }

                            // Adopt the client-provided persistent session id (if any).
                            // The frontend keeps a stable id across WebSocket reconnects,
                            // so Expert-mode TaskContract resume (keyed by session_id)
                            // keeps working instead of starting over at Round 1.
                            if let Some(client_sess) = parsed["session"].as_str() {
                                if !client_sess.is_empty() && client_sess != session_id {
                                    info!("Adopting client session_id {} (connection default was {})",
                                          client_sess, &session_id[..8.min(session_id.len())]);
                                    session_id = client_sess.to_string();
                                }
                            }
                            // Surface the active session in the navigation index so it
                            // appears (and is labelled) in the sidebar. Idempotent.
                            let _ = state.session_index.touch(&session_id, None);

                            // Reset cancellation for new chat (new explicit task
                            // re-enables follow-up auto-dispatch).
                            cancelled.store(false, Ordering::SeqCst);

                            // Get session history for multi-turn context
                            let mut history = {
                                let sessions = state.sessions.lock().await;
                                sessions.get(&session_id).cloned().unwrap_or_default()
                            };
                            // Persist the user message to in-memory session history IMMEDIATELY so
                            // it survives an early Stop (previously it was only written together
                            // with the assistant reply at run end, so stopping with no output
                            // dropped the user's message). The assistant reply is appended by the
                            // drain; we only add the user side here to avoid duplication.
                            {
                                let mut sessions = state.sessions.lock().await;
                                sessions.entry(session_id.clone()).or_default().push(ChatMessage::user(&content));
                                let _ = state.memory_store.store_entry(&session_id, "user", &content, None);
                            }

                            // Inject / refresh the daily-summary memory context (Fix D).
                            // - New session (empty in-memory history): inject a fresh 7-day
                            //   overview so the LLM treats it as authoritative background.
                            // - Long-lived open session: refresh when the previous injection
                            //   is stale, so newly generated daily summaries are picked up
                            //   without requiring a restart. Duplicate blocks are dropped.
                            const MEMORY_CTX_TTL_SECS: u64 = 4 * 3600;
                            let ctx_refresh_needed = {
                                let ts = state.memory_ctx_at.lock().unwrap();
                                match ts.get(&session_id) {
                                    Some(t) => crate::deep_memory::now_secs().saturating_sub(*t) > MEMORY_CTX_TTL_SECS,
                                    None => true,
                                }
                            };
                            if history.is_empty() || ctx_refresh_needed {
                                if let Some(mem_ctx) = state.memory_store.build_context_string(7) {
                                    history.retain(|m| {
                                        !(m.role == "system"
                                            && m.content_as_text()
                                                .map(|s| s.contains("Past Conversation Summaries"))
                                                .unwrap_or(false))
                                    });
                                    info!("Injecting/refreshing memory context ({} chars)", mem_ctx.len());
                                    history.push(ChatMessage::system(&mem_ctx));
                                    state.memory_ctx_at.lock().unwrap().insert(session_id.to_string(), crate::deep_memory::now_secs());
                                }
                            }

                            // Mid-session recall: if the user is asking about earlier
                            // conversations during an ongoing session, query SQLite
                            // (keyword search + daily summaries) and inject the result
                            // as an ephemeral SYSTEM message at the start of history.
                            // This is NOT persisted — the server only stores the
                            // original user content + assistant reply below.
                            // Per-turn continuity + recall (budgeted, unconditional):
                            // 1) session tail block (Fix B) — replay the current session's
                            //    most recent rounds so a continuation picks up prior findings
                            //    without re-running tools. Self-short-circuits on empty session.
                            // 2) lightweight auto-recall (Fix A) — keyword-match the current
                            //    message against recent conversations (all sessions) so related
                            //    earlier exchanges are "remembered" even without a recall keyword.
                            // Both are pure reads; injected as ephemeral SYSTEM messages and not
                            // persisted. Budget-capped so per-turn overhead stays small.
                            if let Some(sr) = state.memory_store.build_session_recall_block_default(&session_id) {
                                info!("Injecting session recall block ({} chars)", sr.len());
                                history.insert(0, ChatMessage::system(&sr));
                            }
                            if let Some(ar) = state.memory_store.build_auto_recall_block(&content, 14, 1800) {
                                info!("Injecting auto-recall block ({} chars)", ar.len());
                                history.insert(0, ChatMessage::system(&ar));
                            }
// P2: no auto-inject of a deep recall blob (that large dump made flash
// re-research instead of recalling). On explicit recall/continuation queries we
// only hint that the on-demand recall_memory tool exists; the agent pulls a
// distilled block ONLY when it actually needs specific past detail.
if is_recall_query(&content) || is_continuation_task(&content) {
let hint = "[memory] If you need specific past-conversation detail not already obvious from
the session tail above (e.g. exact version numbers / earlier conclusions), call the read-only
recall_memory tool with a query and answer directly from its result. Do not re-read source
archives to restate what recall_memory already returns.";
info!("Injecting recall hint ({} chars)", hint.len());
history.insert(0, ChatMessage::system(hint));
}

                            // Run via Runner (managed mode dispatches to ManagedRunner)

// O2: 收敛记忆注入 —— 只注入深层永久块（自动 MEMORY.md/Blackboard），
// 记忆注入：只注入深层永久块（对话事实由后台 curator 蒸馏进 deep_facts）。
// 记忆 KPI 追踪：(fact_id, 注入行) — 传入 drain，在回复落地后统计 loaded/referenced。
let mut eg_injected: Vec<(String, String)> = Vec::new();
if state.two_tier_memory.load(Ordering::SeqCst) {
    let (eg_block, _eg_tok, eg_ids, rel_ids) =
        state.memory_store.deep_permanent_block("global", &content, 1024, 60.0);
    if !eg_block.trim().is_empty() {
        info!("Injected deep permanent block ({} chars)", eg_block.len());
        // picked_ids 与块内事实行同序（header 之后），zip 得到 (id, 注入行) 供 KPI 判定。
        eg_injected = eg_ids
            .into_iter()
            .zip(eg_block.lines().skip(1).map(|l| l.to_string()))
            .collect();
        history.insert(0, ChatMessage::system(&eg_block));
        // 仅"与当前问题相关"的事实才 touch，冷门条目自然退火（解"注入即续命"）。
        if let Ok(touched) = state.memory_store.deep_touch_batch(&rel_ids) {
            info!("Deep memory recall touched {} entries", touched);
        }
    }
}

                            // Managed mode is activated PER-TASK via the 'managed' field —
                            // NOT a global setting. When true, the task runs through the
                            // Manager-Executor-Auditor loop for long-horizon IR tasks.
                        if state.session_is_running(&session_id) {
                            crate::interject::push_pending(&session_id, content.clone());
                            let _ = ws_send_bounded(&ws_sink, json!({"type":"queued_run","content":content.clone(),"session":session_id}).to_string()).await;
                        } else {
                            let managed = parsed["managed"].as_bool().unwrap_or(false);
                            let managed_scope = parsed["managed_scope"].as_str().unwrap_or("").to_string();

                            // Register EVERY run (Instant AND Expert) in the per-session
                            // cancel registry so STOP can reach an Expert task too, and so
                            // the drain can be spawned (Expert no longer blocks the demux
                            // loop). Falls back to connection-level cancel when the cap is full.
                            let session_cancel: Option<Arc<AtomicBool>> =
                                state.session_slot_acquire(&session_id);

                            let run_result = if managed {
                                info!("Expert mode requested for session {}", session_id);
                                // -- Instant -> Expert: inherit same-session Instant progress --
                                // Build a DISTILLED, evidence-indexed handoff of the session's
                                // prior Instant work: original/latest instruction, a bounded capture
                                // of the assistant's actual findings/analysis, and the evidence files
                                // referenced. Tagged UNVERIFIED so the Manager treats it as leads to
                                // continue AND re-audit -- but the substance is kept so the Expert does
                                // not blindly redo collection that already happened.
                                let handoff: Option<String> = {
                                    let mut parts: Vec<String> = Vec::new();
                                    let mut original_task: Option<String> = None;
                                    let mut latest_user: Option<String> = None;
                                    let mut findings: Vec<String> = Vec::new();
                                    let mut finding_chars = 0usize;
                                    let mut evidence: Vec<String> = Vec::new();
                                    const SUFFIXES: [&str; 11] = [".json", ".csv", ".txt", ".md", ".log", ".xml", ".html", ".png", ".evtx", ".zip", ".pdf"];
                                    for m in history.iter() {
                                        if m.role == "system" { continue; }
                                        let Some(text) = m.content_as_text() else { continue };
                                        let text = text.trim();
                                        if text.is_empty() { continue; }
                                        if m.role == "user" {
                                            let head: String = text.chars().take(300).collect();
                                            if original_task.is_none() { original_task = Some(head.clone()); }
                                            latest_user = Some(head);
                                        } else {
                                            // Keep substantive assistant findings (bounded), skip chatter,
                                            // and harvest evidence file references.
                                            if text.len() >= 40 && finding_chars < 2400 {
                                                let seg: String = text.chars().take(700).collect();
                                                finding_chars += seg.len();
                                                findings.push(seg);
                                            }
                                            for tok in text.split_whitespace() {
                                                let tok = tok.trim().trim_matches(|c: char| !(c.is_alphanumeric() || c == '.' || c == '\\' || c == '/' || c == '_' || c == '-'));
                                                let low = tok.to_lowercase();
                                                if tok.len() >= 6 && (tok.contains('\\') || tok.contains('/'))
                                                    && SUFFIXES.iter().any(|s| low.ends_with(s))
                                                    && !evidence.iter().any(|e| e == tok)
                                                {
                                                    evidence.push(tok.to_string());
                                                    if evidence.len() >= 12 { break; }
                                                }
                                            }
                                        }
                                    }
                                    // Bound to the most recent findings.
                                    while findings.len() > 4 && finding_chars > 2000 {
                                        findings.remove(0);
                                    }
                                    if let Some(t) = original_task.as_deref() { parts.push(format!("Original task: {}", t)); }
                                    if let Some(t) = latest_user.as_deref() {
                                        if t != original_task.as_deref().unwrap_or("") { parts.push(format!("Latest instruction: {}", t)); }
                                    }
                                    if !findings.is_empty() {
                                        parts.push("Prior findings/analysis (UNVERIFIED - use as leads; verify before trusting; DO NOT blindly redo this work):".to_string());
                                        parts.extend(findings.iter().cloned());
                                    }
                                    if !evidence.is_empty() {
                                        parts.push("Evidence files referenced earlier (RE-AUDIT before trusting):".to_string());
                                        for e in evidence.iter().take(12) { parts.push(format!("- {}", e)); }
                                    }
                                    if parts.is_empty() { None } else { Some(parts.join("\n")) }
                                };

                                let active_contract =
                                    state.memory_store.get_latest_active_contract(&session_id)
                                        .ok().flatten();
                                let has_expert_residue = active_contract.is_some();
                                let rounds_summary = active_contract.as_ref()
                                    .map(|(_id, json)| rounds_summary_from_contract(json))
                                    .unwrap_or_default();
                                let has_instant = handoff
                                    .as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false);

                                // Ask once whether to CONTINUE prior work (resume the Expert
                                // contract if present, else take over the Instant progress) or
                                // start a NEW round. Wait up to 30s; on no reply, default CONTINUE.
                                let mut start_fresh = false;
                                let mut aborted = false;
                                let mut ask_for_task = false;
                                if has_expert_residue || has_instant {
                                    // Summarize prior rounds into a short (<=100 word) digest for the
                                    // continue prompt instead of showing char-truncated raw notes. Falls
                                    // back to the concise summaries if the LLM call fails.
                                    let prior_source = if !rounds_summary.is_empty() {
                                        rounds_summary.clone()
                                    } else {
                                        handoff.clone().unwrap_or_default()
                                    };
                                    let prior_preview = if prior_source.trim().is_empty() {
                                        String::new()
                                    } else {
                                        crate::managed::manager::summarize_prior(&state.provider, &model, &prior_source)
                                            .await
                                            .unwrap_or_else(|_| {
                                                if !rounds_summary.is_empty() {
                                                    rounds_summary.clone()
                                                } else {
                                                    compress_handoff_summary(&prior_source)
                                                }
                                            })
                                    };
                                let prompt_event = serde_json::json!({
                                        "type": "expert_prompt",
                                        "has_expert": has_expert_residue,
                                        "has_instant": has_instant,
                                        "prior": prior_preview,
                                        "session": session_id,
                                    });
                                    {
                                        let mut sink = ws_sink.lock().await;
                                        let _ = sink.send(Message::Text(prompt_event.to_string().into())).await;
                                    }
                                    let mut choice: Option<String> = None;
                                    let choice_timeout = std::time::Duration::from_secs(30);
                                    loop {
                                        let recv = ws_rx.recv();
                                        match tokio::time::timeout(choice_timeout, recv).await {
                                            Ok(Some(Message::Text(ref t))) => {
                                                if let Ok(p) = serde_json::from_str::<Value>(t) {
                                                    match p["type"].as_str() {
                                                        Some("expert_choice") => {
                                                            let c = p["choice"].as_str().unwrap_or("continue").to_string();
                                                            if c == "continue" || c == "new" { choice = Some(c); }
                                                        }
                                                        Some("stop") => {
                                                            cancelled.store(true, Ordering::SeqCst);
                                                            aborted = true;
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                            Ok(Some(Message::Close(_))) => {
                                                cancelled.store(true, Ordering::SeqCst);
                                                aborted = true;
                                            }
                                            Ok(None) => { aborted = true; }
                                            Err(_elapsed) => {
                                                info!("[managed:{}] Expert choice prompt timed out (30s) - defaulting to CONTINUE", session_id);
                                                choice = Some("continue".to_string());
                                            }
                                            _ => {}
                                        }
                                        if choice.is_some() || aborted { break; }
                                    }
                                    if !aborted && choice.as_deref() == Some("new") {
                                        if !is_concrete_task(&content) {
                                            info!("[managed:{}] User chose NEW but gave no concrete task - asking for task", session_id);
                                            ask_for_task = true;
                                        } else {
                                            info!("[managed:{}] User chose NEW Expert round - clearing residue", session_id);
                                            let _ = state.memory_store.clear_session_active_contracts(&session_id);
                                            start_fresh = true;
                                        }
                                    }
                                }

                                if aborted {
                                    info!("[managed:{}] Expert start cancelled (no choice)", session_id);
                                    let stopped_stream: crate::agent::EventStream = Box::pin(futures::stream::iter(vec![
                                        Ok(AgentEvent::text("\n\n*[Expert start cancelled - no choice received]*", &session_id, "system")),
                                        Ok(AgentEvent::done(&session_id, "system")),
                                    ]));
                                    Ok(stopped_stream)
                                } else if ask_for_task {
                                    info!("[managed:{}] Expert waiting for a concrete task (new round chosen)", session_id);
                                    let ask_stream: crate::agent::EventStream = Box::pin(futures::stream::iter(vec![
                                        Ok(AgentEvent::text("\n\n**你选择了「Start New Job（开新任务）」，但当前这条消息更像是模式切换（例如 continue / go），没有给出具体任务。** 请在 Expert 模式下描述你想解决的具体任务，例如：“try to solve this challenge: <URL>”。收到具体任务后，我会清空旧进度并开始新一轮。\n\n*[Expert 已暂停 —— 等待具体任务指令 / waiting for your task description]*", &session_id, "system")),
                                        Ok(AgentEvent::done(&session_id, "system")),
                                    ]));
                                    Ok(ask_stream)
                                } else {
                                    let task_cancel = Arc::new(AtomicBool::new(false));
                                    {
                                        let mut tasks = state.expert_tasks.lock().unwrap();
                                        if let Some(old) = tasks.get(&session_id) {
                                            old.store(true, Ordering::SeqCst);
                                        }
                                        tasks.insert(session_id.clone(), task_cancel.clone());
                                    }
                                    let managed_runner = crate::managed::ManagedRunner::new(
                                        state.runner.clone(),
                                        state.provider.clone(),
                                        model.clone(),
                                        state.expert_max_managed_rounds.load(Ordering::SeqCst),
                                        state.memory_store.clone(),
                                        state.tools.clone(),
                                        ".".to_string(),
                                        state.workspace_dir.clone(),
                                        state.expert_max_iterations.load(Ordering::SeqCst),
                                        state.rabbit_hole_threshold.load(Ordering::SeqCst),
                                        ctx_window,
                                        state.expert_tool_timeout_secs.load(Ordering::SeqCst) as u64,
                                        state.expert_max_tool_retries.load(Ordering::SeqCst),
                                        state.skill_manager.clone(),
                                        state.computer_use_enabled.clone(),
                                        
                                        state.fallback_model.read().unwrap().clone(),
                                        state.expert_role_models.read().unwrap().clone(),
                                        state.human_intervention_enabled.clone(),
                                        *state.orchestration_limits,
                                    );
                                    let handoff = if start_fresh { None } else { handoff };
                                    // On CONTINUE: if there is an unfinished Expert contract for this
                                    // session, RESUME it (starts at its next round). Only force a fresh
                                    // run when the user chose NEW, or when taking over recent Instant
                                    // work with no Expert contract to resume. This matches the prompt
                                    // copy (resume the Expert contract if present, else take over Instant).
                                    let force_new_run = start_fresh || (has_instant && !has_expert_residue);
                                    managed_runner.run(
                                        &content, &session_id, &model, &managed_scope,
                                        state.permissions.clone(), state.permission_pending.clone(),
                                        task_cancel, handoff, force_new_run,
                                    ).await
                                }
                            } else {
                                state.runner.run(
                                    &content, &session_id, &model, max_iter, history.clone(),
                                    state.permissions.clone(), state.permission_pending.clone(),
                                    None, // no pre-authorization profile (normal chat)
                                    fallback_model, rabbit_hole,
                                    ctx_window, ctx_window_threshold,
                                    tool_timeout as u64,
                                    max_retries,
                                    images,
                                    None, None,  // normal chat — no checkpoint resume
                                    None,        // no per-round output override (Instant mode)
                                ).await
                            };
                            match run_result {
                                Ok(event_stream) => {
                                    if !managed {
                                        // 片②: spawn a dedicated drain task so the main loop keeps demuxing
                                        // other sessions (true parallelism across Instant sessions). Expert
                                        // (managed) runs stay inline and are awaited directly.
                                        let st = state.clone();
                                        // Route events through the session's swappable slot (not this
                                        // connection's sink) so a refresh detaches instead of killing.
                                        let w = state.session_install_sink(&session_id, &ws_sink);
                                        let s2 = session_id.clone();
                                        let m2 = model.clone();
                                        let c2 = content.clone();
                                        let can2 = cancelled.clone();
                                        let sc2 = session_cancel.clone();
                                        tokio::spawn(drain_session_stream(
                                            st, w, m2, s2, c2, false, can2, sc2, eg_injected, event_stream,
                                        ));
                                    } else {
                                        // Expert (managed): spawn a dedicated drain just like
                                        // Instant so the demux loop stays responsive (STOP
                                        // reaches the per-session cancel flag) and other
                                        // sessions keep working while this task audits.
                                        let st = state.clone();
                                        let w = state.session_install_sink(&session_id, &ws_sink);
                                        let s2 = session_id.clone();
                                        let m2 = model.clone();
                                        let c2 = content.clone();
                                        let can2 = cancelled.clone();
                                        let sc2 = session_cancel.clone();
                                        tokio::spawn(drain_session_stream(
                                            st, w, m2, s2, c2, true, can2, sc2, eg_injected, event_stream,
                                        ));
                                    }
                                }
                                Err(e) => {
                                    let err_event = AgentEvent::error(&e.to_string(), &session_id, "system");
                                    let msg_str = err_event.to_ws_message();
                                    let _ = ws_send_bounded(&ws_sink, msg_str).await;
                                    if session_cancel.is_some() {
                                        state.session_slot_release(&session_id);
                                    }
                                }
                            }
                        }
                        }
                        "clear" => {
                            // Route by the client-specified session so a Clear in
                            // one session never destroys another session's
                            // transcript. Fall back to the connection's adopted
                            // session for older clients.
                            let target = match parsed["session"].as_str() {
                                Some(s) if !s.is_empty() => s.to_string(),
                                _ => session_id.clone(),
                            };
                            state.sessions.lock().await.remove(&target);
                            let mut sink = ws_sink.lock().await;
                            let _ = sink
                                .send(Message::Text(json!({"type":"cleared","session":target}).to_string().into()))
                                .await;
                        }
                        "cancel_subagent" => {
                            // Frontend Agent Card Cancel: cancel by run_id across
                            // all live root orchestrators (Step 3 / Phase 0 精简版).
                            let run_id = parsed["run_id"].as_str().unwrap_or("").to_string();
                            let cancelled = if run_id.is_empty() {
                                false
                            } else {
                                crate::agent::orchestration::cancel_subagent_any(&run_id)
                            };
                            let mut sink = ws_sink.lock().await;
                            let _ = sink.send(Message::Text(
                                json!({"type":"subagent_cancelled","run_id":run_id,"cancelled":cancelled})
                                    .to_string().into())).await;
                        }
                        "resume" => {
                            let cp_id = parsed["checkpoint_id"].as_str().unwrap_or("").to_string();
                            if cp_id.is_empty() { continue; }

                            // Load checkpoint from SQLite
                            let cp = match state.memory_store.get_checkpoint(&cp_id) {
                                Ok(Some(cp)) => cp,
                                Ok(None) => {
                                    let err = json!({"type":"error","message":"Checkpoint not found"}).to_string();
                                    let mut sink = ws_sink.lock().await;
                                    let _ = sink.send(Message::Text(err.into())).await;
                                    continue;
                                }
                                Err(e) => {
                                    let err = json!({"type":"error","message":format!("Failed to load checkpoint: {}", e)}).to_string();
                                    let mut sink = ws_sink.lock().await;
                                    let _ = sink.send(Message::Text(err.into())).await;
                                    continue;
                                }
                            };

                            // Deserialize history
                            let history: Vec<ChatMessage> = match serde_json::from_str(&cp.history_json) {
                                Ok(h) => h,
                                Err(e) => {
                                    let err = json!({"type":"error","message":format!("Failed to deserialize checkpoint history: {}", e)}).to_string();
                                    let mut sink = ws_sink.lock().await;
                                    let _ = sink.send(Message::Text(err.into())).await;
                                    continue;
                                }
                            };

                            let model = cp.model_name.clone();
                            let resume_state = ResumeState {
                                history,
                                start_iteration: cp.iteration,
                            };
                            let new_cp_id = uuid::Uuid::new_v4().to_string();

                            info!("Resuming checkpoint {} (session: {}, model: {}, iter: {})",
                                  cp_id, session_id, model, cp.iteration);

                            // Send a status message to the UI
                            let resume_event = serde_json::json!({
                                "type": "text",
                                "content": format!("\n\n*[Resuming interrupted task from iteration {}...]*\n\n", cp.iteration + 1),
                                "invocation_id": session_id,
                                "author": "system"
                            });
                            {
                                let mut sink = ws_sink.lock().await;
                                let _ = sink.send(Message::Text(resume_event.to_string().into())).await;
                            }

                            let ctx_window = {
                                let mc = state.model_configs.read().await;
                                mc.iter().find(|m| m.name == model).map(|m| m.context_window).unwrap_or(128000)
                            };

                            cancelled.store(false, Ordering::SeqCst);

                            match state.runner.run(
                                &cp.user_message, &session_id, &model, state.max_iterations.load(Ordering::SeqCst),
                                vec![],  // empty base history — resume_state provides it
                                state.permissions.clone(), state.permission_pending.clone(),
                                None, // no pre-authorization profile (checkpoint resume)
                                None, state.rabbit_hole_threshold.load(Ordering::SeqCst),
                                ctx_window, state.context_window_threshold.load(Ordering::SeqCst),
                                state.tool_timeout_secs.load(Ordering::SeqCst) as u64,
                                state.max_tool_retries.load(Ordering::SeqCst),
                                vec![],  // no images
                                Some(new_cp_id),
                                Some(resume_state),
                                None, // no per-round output override (checkpoint resume)
                            ).await {
                                Ok(mut event_stream) => {
                                    let mut assistant_text = String::new();
                                    let mut srv_events: u64 = 0;
                                    let mut srv_last = std::time::Instant::now();
                                    // Same detach-not-cancel semantics as `drain_session_stream`:
                                    // events go through the session slot, and the socket dying only
                                    // stops the forwarding (and the ws_rx polling), never the run.
                                    let rslot = state.session_install_sink(&session_id, &ws_sink);
                                    let mut rdetached = false;
                                    loop {
                                        tokio::select! {
                                            result = event_stream.next() => {
                                                match result {
                                                    Some(Ok(event)) => {
                                                        srv_events += 1;
                                                        if srv_last.elapsed().as_secs() >= 5 {
                                                            info!("[managed:{}] server event loop alive: {} events", session_id, srv_events);
                                                            srv_last = std::time::Instant::now();
                                                        }
                                                        if let AgentEvent::TextDelta { content: c, .. } = &event {
                                                            assistant_text.push_str(c);
                                                        }
                                                        // Persist token usage to database
                                                        if let AgentEvent::Usage { model, prompt_tokens, completion_tokens, total_tokens, cached_tokens, .. } = &event {
                                                            {
                                                        // record_usage is a synchronous SQLite write guarded by a
                                                        // std::sync::Mutex inside the async event loop. Called inline it
                                                        // can block a runtime thread and, under DB contention, freeze the
                                                        // whole Manager->Executor pipeline (server stops draining).
                                                        // Move it off the hot path so the loop always keeps consuming.
                                                        let ms = state.memory_store.clone();
                                                        let mdl = model.clone();
                                                        let pt = *prompt_tokens;
                                                        let ct = *completion_tokens;
                                                        let tt = *total_tokens;
                                                        let cst = *cached_tokens;
                                                        let sid = session_id.clone();
                                                        tokio::task::spawn_blocking(move || {
                                                            let _ = ms.record_usage(&mdl, pt, ct, tt, cst, &sid);
                                                        });
                                                    }
                                                        }
                                                        let msg_str = event.to_ws_message();
                                                        match slot_forward(&rslot, msg_str).await {
                                                            Forward::Detached | Forward::NoClient => rdetached = true,
                                                            _ => rdetached = false,
                                                        }
                                                        if event.is_done() {
                                                            break;
                                                        }
                                                    }
                                                    Some(Err(e)) => {
                                                        let err_event = AgentEvent::error(&e.to_string(), &session_id, "system");
                                                        let msg_str = err_event.to_ws_message();
                                                        let _ = slot_forward(&rslot, msg_str).await;
                                                        break;
                                                    }
                                                    None => break,
                                                }
                                            }
                                            msg = ws_rx.recv(), if !rdetached => {
                                                match msg {
                                                    Some(Message::Text(ref t)) => {
                                                        if let Ok(p) = serde_json::from_str::<Value>(t) {
                                                            if p["type"].as_str() == Some("stop") {
                                                                cancelled.store(true, Ordering::SeqCst);
                                                            }
                                                            if p["type"].as_str() == Some("permission_response") {
                                                                let req_id = p["request_id"].as_str().unwrap_or("");
                                                                let allowed = p["allowed"].as_bool().unwrap_or(false);
                                                                state.permission_resolver.resolve(req_id, allowed).await;
                                                            }
                                                        }
                                                    }
                                                    // Connection loss is NOT a user stop: detach the sink
                                                    // (the `if !rdetached` guard stops polling ws_rx) and
                                                    // let the resumed run finish and persist its answer.
                                                    Some(Message::Close(_)) | None => {
                                                        rdetached = true;
                                                        *rslot.lock().unwrap() = None;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                        if cancelled.load(Ordering::SeqCst) {
                                            info!("Agent execution stopped by user (resume)");
                                            let stop_event = AgentEvent::text("\n\n*[Stopped by user]*", &session_id, "system");
                                            let msg_str = stop_event.to_ws_message();
                                            let _ = ws_send_bounded(&ws_sink, msg_str).await;
                                            let done_event = AgentEvent::done(&session_id, "system");
                                            let msg_str = done_event.to_ws_message();
                                            let _ = ws_send_bounded(&ws_sink, msg_str).await;
                                            break;
                                        }
                                    }

                                    two_tier_write(&state, &mut assistant_text, &session_id, &cp.user_message);
                                    // Store in memory (SQLite)
                                    if !assistant_text.is_empty() {
                                        let _ = state.memory_store.store_entry(&session_id, "user", &cp.user_message, None);
                                        let _ = state.memory_store.store_entry(&session_id, "assistant", &assistant_text, None);
                                        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = state.memory_store.auto_summarize_date(&today);
                                    }
                                }
                                Err(e) => {
                                    let err_event = AgentEvent::error(&e.to_string(), &session_id, "system");
                                    let msg_str = err_event.to_ws_message();
                                    let _ = ws_send_bounded(&ws_sink, msg_str).await;
                                }
                            }
                        }
                        "permissions" => {
                            // Update permission settings (when not in agent execution)
                            let mut perms = state.permissions.lock().await;
                            for cat in &["read", "write", "delete", "modify", "execute"] {
                                if let Some(v) = parsed[cat].as_bool() {
                                    perms.insert(cat.to_string(), v);
                                }
                            }
                            info!("Permissions updated: {:?}", *perms);
                        }
                        "permission_response" => {
                            // Handle permission response when not in agent execution (edge case)
                            let req_id = parsed["request_id"].as_str().unwrap_or("");
                            let allowed = parsed["allowed"].as_bool().unwrap_or(false);
                            state.permission_resolver.resolve(req_id, allowed).await;
                        }
                        "attach" => {
                            // (Re)connect handshake: the client announces every session id it can
                            // render. Any run still in flight for one of them is re-pointed at this
                            // connection (it kept executing while detached), and the client is told
                            // which sessions are live again so it can restore the running UI state.
                            let mut wanted: Vec<String> = parsed["sessions"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            if let Some(s) = parsed["session"].as_str() {
                                if !s.is_empty() && !wanted.iter().any(|w| w == s) {
                                    wanted.push(s.to_string());
                                }
                            }
                            if wanted.is_empty() {
                                wanted.push(session_id.clone());
                            }
                            let attached = state.session_attach_sinks(&wanted, &ws_sink);
                            for sid in attached {
                                info!("[session:{}] Client re-attached to running task", &sid[..8.min(sid.len())]);
                                let _ = ws_send_bounded(
                                    &ws_sink,
                                    json!({ "type": "run_attached", "session": sid }).to_string(),
                                )
                                .await;
                            }
                        }
                        "stop" => {
                        	// Session-scoped stop received while the main loop is idle
                        	// (no session currently draining). Route it to the named session
                        	// registry entry so a background run can still be cancelled;
                        	// no-op (returns false) when nothing is running.
                        	let stop_sid = parsed["session"].as_str().unwrap_or("").to_string();
                        	if stop_sid.is_empty() {
                        		info!("Outer stop ignored (no session supplied)");
                        	} else {
                        		let found = state.session_request_cancel(&stop_sid);
                        		info!(
                        			"[session:{}] Outer stop routed (running={})",
                        			&stop_sid[..8.min(stop_sid.len())],
                        			found
                        		);
                        	}
                        }
                        "interject" => {
                            // Demux: route mid-run interjections from the main loop. insert:true goes to
                            // the static insert-now queue that the running agent loop drains each
                            // iteration (drain_insert); insert:false is a queued follow-up.
                            let ij_sid = parsed["session"].as_str().unwrap_or("").to_string();
                            let ij_content = parsed["content"].as_str().unwrap_or("").to_string();
                            let ij_insert = parsed["insert"].as_bool().unwrap_or(false);
                            if !ij_sid.is_empty() && !ij_content.is_empty() {
                                if ij_insert {
                                    crate::interject::push_insert(&ij_sid, ij_content);
                                } else {
                                    crate::interject::push_pending(&ij_sid, ij_content);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::Close(_) => {
                info!("Client disconnected");
                break;
            }
            _ => {}
        }
    }

    // ── End-of-session SOP authoring ──
    // experience 由“蒸馏经验条目”改为“动态 SOP”：会话若含可复用的多步骤过程，
    // 经 LLM 作者判定后固化为可回放、可评分、可自动迭代的 SOP（sops.json）。
    // 用户手动挂接的 knowledge 不变。
    // A refresh mid-run only DETACHES the run, so this teardown must not distill
    // a conversation that is still being written; `maybe_debrief` re-checks that
    // (and dedups against the drain-side trigger) internally.
    maybe_debrief(&state, &session_id, "connection-close").await;
}

// ============================================================
// Memory Handlers
// ============================================================

async fn memory_dates_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.memory_store.available_dates() {
        Ok(dates) => Json(json!({ "dates": dates, "count": dates.len() })),
        Err(e) => Json(json!({ "error": e })),
    }
}

async fn memory_summaries_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.memory_store.get_all_summaries() {
        Ok(summaries) => Json(json!({ "summaries": summaries, "count": summaries.len() })),
        Err(e) => Json(json!({ "error": e })),
    }
}

#[derive(Deserialize)]
struct MemoryQuery {
    date: Option<String>,
}

async fn memory_entries_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MemoryQuery>,
) -> Json<Value> {
    let date = query.date.unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    match state.memory_store.get_entries_by_date(&date) {
        Ok(entries) => Json(json!({ "date": date, "entries": entries, "count": entries.len() })),
        Err(e) => Json(json!({ "error": e })),
    }
}

#[derive(Deserialize)]
struct SummarizeRequest {
    date: Option<String>,
}

async fn memory_summarize_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SummarizeRequest>,
) -> Json<Value> {
    let date = body.date.unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    match state.memory_store.build_raw_context_for_date(&date) {
        Ok(raw) => {
            // Store a simple extractive summary (LLM-based summary would need provider access).
            let lines: Vec<&str> = raw.lines().collect();
            let user_msgs: Vec<&str> = lines.iter()
                .filter(|l| l.starts_with("User:"))
                .copied()
                .collect();
            let summary = if user_msgs.is_empty() {
                format!("{} conversation entries recorded ({} chars)", lines.len(), raw.len())
            } else {
                let topics: Vec<String> = user_msgs.iter().take(5)
                    .map(|m| {
                        let text = m.trim_start_matches("User:").trim();
                        let preview: String = text.chars().take(80).collect();
                        preview
                    })
                    .collect();
                format!("Topics: {}", topics.join("; "))
            };
            match state.memory_store.store_summary(&date, &summary) {
                Ok(_) => {
                    info!("Summary stored for {}: {} chars", date, summary.len());
                    Json(json!({ "success": true, "date": date, "summary": summary }))
                }
                Err(e) => Json(json!({ "success": false, "error": e })),
            }
        }
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

// ── Deep Memory API (记忆查看器) ────────────────────────────

#[derive(Deserialize)]
struct DeepListQuery {
    q: Option<String>,
    limit: Option<usize>,
    scope: Option<String>,
}

fn deep_parse_type(t: &str) -> crate::deep_memory::FactType {
    match t.to_ascii_lowercase().as_str() {
        "identity" => crate::deep_memory::FactType::Identity,
        "preference" | "pref" => crate::deep_memory::FactType::Preference,
        "project" => crate::deep_memory::FactType::Project,
        "constraint" => crate::deep_memory::FactType::Constraint,
        _ => crate::deep_memory::FactType::Reference,
    }
}

fn deep_parse_pinned(t: &str) -> crate::deep_memory::PinnedBy {
    match t.to_ascii_lowercase().as_str() {
        "agent" => crate::deep_memory::PinnedBy::Agent,
        "none" => crate::deep_memory::PinnedBy::None,
        _ => crate::deep_memory::PinnedBy::User,
    }
}

fn deep_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn deep_make_id(scope: &str, subject: Option<&str>, content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let basis = match subject {
        Some(s) => format!("{}|subj|{}", scope, s),
        None => format!("{}|body|{}", scope, content),
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    basis.hash(&mut h);
    format!("eg{:016x}", h.finish())
}

/// 简单档位标注：仅供查看器展示（深层存储派生，不落盘）。
fn deep_display_quality(f: &crate::deep_memory::DeepFact, now: u64) -> (f32, String) {
    use crate::deep_memory::PinnedBy;
    let pinned_user = matches!(f.pinned_by, PinnedBy::User);
    if pinned_user {
        return (5.0, "Permanent".to_string());
    }
    let params = crate::deep_memory::DeepParams::default();
    let i_eff = crate::deep_memory::effective_importance(
        f.importance, f.last_accessed, now, false, params.tau_days,
    );
    let label = if i_eff >= params.theta_up {
        "Permanent".to_string()
    } else if i_eff >= params.theta_down {
        "Active".to_string()
    } else {
        "Archived".to_string()
    };
    (i_eff, label)
}

async fn deep_memory_list_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DeepListQuery>,
) -> Json<Value> {
    let scope = query.scope.clone().unwrap_or_else(|| "global".to_string());
    match state.memory_store.deep_list(&scope) {
        Ok(facts) => {
            let q = query
                .q
                .as_deref()
                .map(|s| s.trim().to_lowercase())
                .unwrap_or_default();
            let now = deep_now();
            let mut items: Vec<Value> = facts
                .iter()
                .filter_map(|f| {
                    if !q.is_empty() {
                        let hay = format!(
                            "{} {} {} {}",
                            f.content,
                            f.summary,
                            f.essence,
                            f.tags.join(" ")
                        )
                        .to_lowercase();
                        if !hay.contains(&q) {
                            return None;
                        }
                    }
                    let (i_eff, label) = deep_display_quality(f, now);
                    let mut v = serde_json::to_value(f).unwrap_or(Value::Null);
                    if let Value::Object(ref mut m) = v {
                        m.insert("i_eff".to_string(), json!(i_eff));
                        m.insert("quality".to_string(), json!(label));
                        m.insert("value".to_string(), json!(f.value(now)));
                    }
                    Some(v)
                })
                .collect();
            items.sort_by(|a, b| {
                let va = a.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let vb = b.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
                vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
            });
            let limit = query.limit.unwrap_or(100).min(500);
            items.truncate(limit);
            Json(json!({ "facts": items, "count": items.len() }))
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

#[derive(Deserialize)]
struct DeepCreateReq {
    content: String,
    #[serde(default)]
    fact_type: String,
    #[serde(default)]
    importance: Option<f32>,
    #[serde(default)]
    pinned_by: String,
    #[serde(default)]
    subject_key: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    essence: String,
}

async fn deep_memory_create_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DeepCreateReq>,
) -> Json<Value> {
    use crate::deep_memory::PinnedBy;
    let content = body.content.trim().to_string();
    if content.is_empty() {
        return Json(json!({ "success": false, "error": "content is required" }));
    }
    let scope = crate::deep_memory::MemoryScope::Global;
    let pinned = deep_parse_pinned(&body.pinned_by);
    let importance = body
        .importance
        .unwrap_or(if matches!(pinned, PinnedBy::User) { 5.0 } else { 4.0 })
        .clamp(crate::deep_memory::IMPORTANCE_MIN, crate::deep_memory::IMPORTANCE_MAX);
    let id = deep_make_id("global", body.subject_key.as_deref(), &content);
    let fact = crate::deep_memory::DeepFact {
        id,
        content,
        summary: body.summary.clone(),
        essence: body.essence.clone(),
        fact_type: deep_parse_type(&body.fact_type),
        scope,
        pinned_by: pinned,
        subject_key: body.subject_key.clone(),
        importance,
        created_at: deep_now(),
        last_accessed: deep_now(),
        tags: body.tags,
        links: Vec::new(),
        archived: false, loaded: 0, referenced: 0,
    };
    match state.memory_store.deep_store(&fact) {
        Ok(()) => Json(json!({ "success": true, "fact": serde_json::to_value(&fact).unwrap_or(Value::Null) })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

#[derive(Deserialize)]
struct DeepUpdateReq {
    content: Option<String>,
    summary: Option<String>,
    essence: Option<String>,
    fact_type: Option<String>,
    importance: Option<f32>,
    pinned_by: Option<String>,
    subject_key: Option<String>,
    tags: Option<Vec<String>>,
}

async fn deep_memory_update_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<DeepUpdateReq>,
) -> Json<Value> {
    let Some(mut fact) = state
        .memory_store
        .deep_get(&id)
        .map_err(|e| e)
        .unwrap_or(None)
    else {
        return Json(json!({ "success": false, "error": "not found" }));
    };
    if let Some(v) = body.content {
        if !v.trim().is_empty() {
            fact.content = v.trim().to_string();
        }
    }
    if let Some(v) = body.summary {
        fact.summary = v;
    }
    if let Some(v) = body.essence {
        fact.essence = v;
    }
    if let Some(v) = body.fact_type {
        fact.fact_type = deep_parse_type(&v);
    }
    if let Some(v) = body.importance {
        fact.importance = crate::deep_memory::ema_update(fact.importance, v.clamp(0.0, 5.0), 0.4);
    }
    if let Some(v) = body.pinned_by {
        fact.pinned_by = deep_parse_pinned(&v);
    }
    if let Some(v) = body.subject_key {
        fact.subject_key = if v.trim().is_empty() { None } else { Some(v.trim().to_string()) };
    }
    if let Some(v) = body.tags {
        fact.tags = v;
    }
    match state.memory_store.deep_store(&fact) {
        Ok(()) => Json(json!({ "success": true, "fact": serde_json::to_value(&fact).unwrap_or(Value::Null) })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

async fn deep_memory_delete_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    match state.memory_store.deep_forget(&id) {
        Ok(true) => Json(json!({ "success": true })),
        Ok(false) => Json(json!({ "success": false, "error": "not found" })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

// ── History API ──────────────────────────────────────────────

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default = "default_history_days")]
    days: usize,
    #[serde(default = "default_history_limit")]
    limit: usize,
    #[serde(default = "default_tz_offset")]
    tz_offset: i32,
    /// Optional session_id filter. When present, only entries for that session
    /// are returned; when absent, behaviour is unchanged (all recent entries).
    session: Option<String>,
}

// ── Engram Curator（后台自动蒸馏，temm1e 形态）───────────────

#[derive(serde::Deserialize)]
struct CuratedFact {
    content: String,
    #[serde(default)]
    fact_type: String,
    #[serde(default)]
    subject_key: Option<String>,
}

#[derive(serde::Deserialize)]
struct CuratedFacts {
    #[serde(default)]
    facts: Vec<CuratedFact>,
}

/// 从可能带代码围栏的 LLM 输出里截取第一个 `{...}` JSON 对象。
fn parse_curated_facts(content: &str) -> Option<Vec<CuratedFact>> {
    let a = content.find('{')?;
    let b = content.rfind('}')?;
    if b < a {
        return None;
    }
    serde_json::from_str::<CuratedFacts>(&content[a..=b])
        .ok()
        .map(|c| c.facts)
}

/// 词集 Jaccard 近似重复检测（阈值 0.6），防止把已存在的同义事实重复入深层。
fn deep_near_duplicate(a: &str, b: &str) -> bool {
    fn words(x: &str) -> std::collections::HashSet<String> {
        x.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
            .map(String::from)
            .collect()
    }
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return false;
    }
    let inter = wa.intersection(&wb).count() as f32;
    let union = wa.union(&wb).count() as f32;
    inter / union >= 0.6
}

/// 后台 Engram Curator：实质回复后异步蒸馏 durable facts 到深层记忆。
fn spawn_deep_curator(
    state: Arc<AppState>,
    model: &str,
    session_id: &str,
    user_text: &str,
    assistant_text: &str,
) {
    if !state.two_tier_memory.load(Ordering::SeqCst) {
        return;
    }
    // Distill durable facts from either side. Guard only against empty/trivial
    // interactions (e.g. pure greetings) so short but durable inputs (e.g. a
    // provided credential/endpoint) still get captured into deep memory.
    let user_len = user_text.trim().chars().count();
    let assistant_len = assistant_text.trim().chars().count();
    if user_len == 0 && assistant_len == 0 {
        return;
    }
    if user_len <= 4 && assistant_len <= 4 {
        return;
    }
    let model = model.to_string();
    let user_text = user_text.to_string();
    let assistant_text = assistant_text.to_string();
    tokio::spawn(async move {
        let trunc = |s: &str| s.chars().take(500).collect::<String>();
        let user_part = trunc(&user_text);
        let assistant_part = trunc(&assistant_text);
        let digest = if assistant_part.trim().is_empty() {
            format!("User: {}", user_part)
        } else {
            format!("User: {}\nTem: {}", user_part, assistant_part)
        };
        let sys = "You are a precise long-term-memory curator. From the conversation below extract DURABLE facts worth remembering across sessions:
1) user or project facts — standing preferences, identity details, hard constraints, stable project facts;
2) stable connection/endpoint details — target hosts, IPs, credentials, accounts, infrastructure references;
3) IR investigation findings/leads established or confirmed in the assistant reply — affected components/versions, C2/IP/domain/hash indicators, evidence or report file paths, and conclusions.
Ignore one-off or transient details, and do not re-state the same point more than once (dedupe by subject_key). Respond ONLY with JSON of the form {\"facts\":[{\"content\":\"<fact, third person>\",\"fact_type\":\"identity|preference|project|constraint|reference\",\"subject_key\":\"<short stable key>\"}]}. If nothing durable, respond {\"facts\":[]}.";
        let messages = vec![ChatMessage::system(sys), ChatMessage::user(&digest)];
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let content = match state
            .provider
            .chat_stream(&model, &messages, &[], tx, "deep-curator", "memory")
            .await
        {
            Ok((c, _, _, _, _, stream_timed_out)) => {
                if stream_timed_out {
                    // A curated JSON payload cut by a transport error will not parse, and memory
                    // writes must not be derived from a truncated payload. Skipping this round
                    // loses nothing: curation runs again on subsequent turns, whereas persisting
                    // facts parsed from a cut response could write wrong entries.
                    tracing::warn!(
                        "[deep-curator] LLM stream cut by transport error ({} chars received); skipping curation round",
                        c.chars().count()
                    );
                    return;
                }
                c
            }
            Err(e) => {
                tracing::warn!("[deep-curator] LLM call failed: {e}");
                return;
            }
        };
        let Some(facts) = parse_curated_facts(&content) else {
            return;
        };
        let now = deep_now();
        for cf in facts.iter().take(3) {
            let fcontent = cf.content.trim();
            if fcontent.is_empty() {
                continue;
            }
            let existing = cf.subject_key.as_deref().and_then(|sk| {
                state
                    .memory_store
                    .deep_find_subject_any("global", sk)
                    .ok()
                    .flatten()
            });
            if existing.is_none() {
                let dup = state
                    .memory_store
                    .deep_list("global")
                    .ok()
                    .map(|l| l.iter().any(|e| deep_near_duplicate(&e.content, fcontent)))
                    .unwrap_or(false);
                if dup {
                    continue;
                }
            }
            if let Some(mut fact) = existing {
                if matches!(fact.pinned_by, crate::deep_memory::PinnedBy::User) {
                    continue;
                }
                fact.content = fcontent.to_string();
                fact.fact_type = deep_parse_type(&cf.fact_type);
                fact.importance = fact.importance.max(4.0);
                fact.last_accessed = now;
                fact.archived = false;
                if state.memory_store.deep_store(&fact).is_ok() {
                    tracing::info!("[deep-curator] updated durable fact: {}", fcontent);
                }
                continue;
            }
            let fact = crate::deep_memory::DeepFact {
                id: deep_make_id("global", cf.subject_key.as_deref(), fcontent),
                content: fcontent.to_string(),
                summary: fcontent.chars().take(160).collect(),
                essence: fcontent.split_whitespace().take(6).collect::<Vec<_>>().join(" "),
                fact_type: deep_parse_type(&cf.fact_type),
                scope: crate::deep_memory::MemoryScope::Global,
                pinned_by: crate::deep_memory::PinnedBy::Agent,
                subject_key: cf.subject_key.clone(),
                importance: 4.0,
                created_at: now,
                last_accessed: now,
                tags: Vec::new(),
                links: Vec::new(),
                archived: false, loaded: 0, referenced: 0,
            };
            if let Err(e) = state.memory_store.deep_store(&fact) {
                tracing::warn!("[deep-curator] store failed: {e}");
                continue;
            }
            tracing::info!("[deep-curator] captured durable fact: {}", fcontent);
        }
        // O2: refresh the read-only projection after a curator run so the on-disk
        // overview stays in sync with deep memory (small table, cheap rewrite).
        if let Err(e) = state
            .memory_store
            .write_memory_projection(&state.workspace_dir)
        {
            tracing::warn!("[deep-curator] memory projection failed: {e}");
        }
        // 软归档 GC：非 User、低重要度、久未访问的事实归档（可恢复，不物理删）。
        if let Ok(n) = state.memory_store.deep_archive_stale(60.0, 3.0) {
            if n > 0 {
                tracing::info!("[deep-curator] soft-archived {n} stale low-importance facts");
            }
        }
    });
}

fn default_history_days() -> usize { 3 }
fn default_history_limit() -> usize { 50 }
fn default_tz_offset() -> i32 { 8 }

async fn history_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HistoryQuery>,
) -> Json<Value> {
    let days = query.days.max(1).min(30);
    let limit = query.limit.max(1).min(200);
    let tz_secs = query.tz_offset.clamp(-12, 14) * 3600;
    let tz = chrono::FixedOffset::east_opt(tz_secs).unwrap_or_else(|| chrono::FixedOffset::east_opt(8 * 3600).unwrap());
    match state.memory_store.get_recent_entries(days) {
        Ok(entries) => {
            // Filter to user/assistant roles and take the last N entries
            let filtered: Vec<_> = entries.into_iter()
                .filter(|e| e.role == "user" || e.role == "assistant")
                .filter(|e| match &query.session {
                    Some(sid) if !sid.is_empty() => e.session_id == *sid,
                    _ => true,
                })
                .collect();
            let chat: Vec<Value> = filtered.into_iter()
                .rev()
                .take(limit)
                .rev()
                .map(|e| json!({
                    "role": e.role,
                    "text": e.content,
                    "time": chrono::DateTime::parse_from_rfc3339(&e.timestamp)
                        .map(|dt| dt.with_timezone(&tz).format("%H:%M:%S").to_string())
                        .unwrap_or_default(),
                    "session_id": e.session_id,
                }))
                .collect();
            Json(json!({ "messages": chat, "count": chat.len() }))
        }
        Err(e) => Json(json!({ "messages": [], "count": 0, "error": e })),
    }
}

// ── Multi-session (navigation UI) API ────────────────────────
// These endpoints drive the sidebar session list + new/rename/delete lifecycle.
// Conversation isolation is already provided by AppState.sessions + memory.db
// (keyed by session_id); this layer only exposes which sessions exist and their
// labels. Adding these is strictly additive — no existing behaviour changes.

async fn sessions_list_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let list = state.session_index.list();
    let sessions: Vec<Value> = list
        .into_iter()
        .map(|(id, meta)| json!({
            "id": id,
            "title": meta.title.unwrap_or_else(|| "New session".to_string()),
            "created_at": meta.created_at.to_rfc3339(),
            "updated_at": meta.updated_at.to_rfc3339(),
            "main": meta.main,
        }))
        .collect();
    Json(json!({ "sessions": sessions, "count": sessions.len() }))
}

#[derive(Deserialize)]
struct SessionCreateBody {
    title: Option<String>,
}

async fn sessions_create_handler(
    State(state): State<Arc<AppState>>,
    body: Option<Json<SessionCreateBody>>,
) -> Json<Value> {
    let title = body.as_ref().and_then(|b| b.title.clone());
    let id = uuid::Uuid::new_v4().to_string();
    let _ = state.session_index.touch(&id, title);
    let meta = state.session_index.get(&id);
    Json(json!({
        "id": id,
        "title": meta.as_ref().and_then(|m| m.title.clone()).unwrap_or_else(|| "New session".to_string()),
        "created_at": meta.as_ref().map(|m| m.created_at.to_rfc3339()),
        "updated_at": meta.as_ref().map(|m| m.updated_at.to_rfc3339()),
    }))
}

#[derive(Deserialize)]
struct SessionRenameBody {
    title: String,
}

async fn sessions_rename_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Json<SessionRenameBody>,
) -> Json<Value> {
    match state.session_index.rename(&id, &body.title) {
        Ok(()) => Json(json!({ "success": true, "id": id, "title": body.title })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

async fn sessions_delete_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    match state.session_index.soft_delete(&id) {
        Ok(()) => Json(json!({ "success": true, "id": id })),
        Err(e) => Json(json!({ "success": false, "error": e })),
    }
}

// ============================================================
// External Tools Handlers
// ============================================================

async fn tools_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut mgr = state.external_tools.lock().await;
    mgr.scan();
    let handles = mgr.get_tool_handles();
    let tools = mgr.list_tools();
    let tools_dir = mgr.tools_dir().to_string_lossy().to_string();
    drop(mgr);

    // Sync external tools into ToolRegistry (LLM-visible)
    let mut registry = state.tools.write().await;
    registry.sync_external_tools(&handles);
    let registered_count = handles.len();
    drop(registry);

    info!("External tools synced to registry: {} tool(s)", registered_count);

    // 内置能力。这一列故意不只装 External Tools：用户要的是“浏览器、Linux 取证工具族
    // 这些能力能逐个开关”。但它们的“关”不是同一个语义，所以每行自己带标签，
    // 不要把“降为按需载入”写成“已停用”。
    let browser_probe = browser_probe_value(&state);
    // Value 的 Display 会连 JSON 引号一起打出来，取字符串再拼。
    let probe_path = browser_probe["path"].as_str().unwrap_or("");
    let probe_source = browser_probe["source"].as_str().unwrap_or("?");
    let builtins = vec![
        json!({
            "key": "browser_cdp",
            "name": "Web Browser",
            "description": "browser_cdp: navigate, screenshot, extract page content, run JS, one page per agent in one shared browser (`list_tabs` also counts pages the site opened itself). Uses its own persistent profile in the local application-data directory, one per workspace.",
            "enabled": state.browser_enabled.load(Ordering::SeqCst),
            "on_label": "Enabled",
            "off_label": "Disabled (unregistered)",
            "detail": if probe_path.is_empty() {
                format!("no browser detected ({})", probe_source)
            } else {
                format!("{} [{}]", probe_path, probe_source)
            },
        }),
        json!({
            "key": "computer_use",
            "name": "Computer Use",
            "description": "cu_* tools: screen capture, mouse and keyboard control of this desktop.",
            "enabled": state.computer_use_enabled.load(Ordering::SeqCst),
            "on_label": "Enabled",
            "off_label": "Disabled (unregistered)",
            "detail": "",
        }),
        json!({
            "key": "linux_ir_tools",
            "name": "Linux Forensics Tools",
            "description": "Linux IR family (category scanners + aggregator). Off here does NOT mean unavailable: it moves to the on-demand schema catalog and stays callable via load_tool_schema, which saves fixed context.",
            "enabled": state.linux_ir_tools.load(Ordering::SeqCst),
            "on_label": "Full load (every request)",
            "off_label": "On-demand (still callable)",
            "detail": "linux_ssh always stays loaded",
        }),
        json!({
            "key": "human_intervention",
            "name": "Simulated Human Intervention",
            "description": "Expert mode: when blocked, let the LLM play the human responder instead of stalling.",
            "enabled": state.human_intervention_enabled.load(Ordering::SeqCst),
            "on_label": "Enabled",
            "off_label": "Disabled",
            "detail": "",
        }),
    ];

    Json(json!({
        "tools": tools,
        "tools_dir": tools_dir,
        "count": tools.len(),
        "registered": registered_count,
        "builtins": builtins,
    }))
}

/// Tools 页那一行的开关。四个 key 走同一条路：改运行期状态 → 落盘 config.toml。
/// 落盘失败不会回滚运行期效果（用户已经看到切换了），但会把 persisted=false 带回去，
/// 不假装存住了。
async fn builtin_tool_toggle_handler(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Json<Value> {
    let next = match key.as_str() {
        "browser_cdp" => !state.browser_enabled.load(Ordering::SeqCst),
        "computer_use" => !state.computer_use_enabled.load(Ordering::SeqCst),
        "linux_ir_tools" => !state.linux_ir_tools.load(Ordering::SeqCst),
        "human_intervention" => !state.human_intervention_enabled.load(Ordering::SeqCst),
        other => {
            return Json(json!({ "success": false, "error": format!("unknown tool switch: {}", other) }))
        }
    };

    match key.as_str() {
        "browser_cdp" => {
            if next {
                // 拿回同一个会话：另建一个会多一份持久 profile，登录态就对不上了。
                let mut reg = state.tools.write().await;
                reg.register(Arc::new(crate::tool::browser_cdp::BrowserCdpTool::new(
                    state.browser_session.clone(),
                )));
                state.browser_enabled.store(true, Ordering::SeqCst);
                info!("Web Browser ENABLED: browser_cdp registered");
            } else {
                // 先注销再关浏览器：反过来会有一个在跑的进程持着 user-data-dir，
                // 下次启动撞上单实例互斥，表现成“启动失败”。
                {
                    let mut reg = state.tools.write().await;
                    reg.unregister("browser_cdp");
                }
                state.browser_enabled.store(false, Ordering::SeqCst);
                if let Err(e) = state.browser_session.close().await {
                    warn!("Web Browser disabled but closing the browser failed: {}", e);
                }
                info!("Web Browser DISABLED: browser_cdp unregistered, session closed");
            }
        }
        "computer_use" => {
            let mut reg = state.tools.write().await;
            if next {
                crate::tool::computer_use::register_computer_use_tools(&mut reg);
            } else {
                crate::tool::computer_use::unregister_computer_use_tools(&mut reg);
            }
            drop(reg);
            state.computer_use_enabled.store(next, Ordering::SeqCst);
            info!("Computer Use tools {}", if next { "ENABLED" } else { "DISABLED" });
        }
        "linux_ir_tools" => {
            state.linux_ir_tools.store(next, Ordering::SeqCst);
            info!("Linux IR tools {}", if next { "full load" } else { "on-demand" });
        }
        _ => {
            // human_intervention：没有工具表可改，只是一个运行期开关。
            state.human_intervention_enabled.store(next, Ordering::SeqCst);
            info!("Human Intervention Simulation {}", if next { "ENABLED" } else { "DISABLED" });
        }
    }

    let mut response = json!({ "success": true, "key": key, "enabled": next });
    if let Err(e) = crate::config::Config::set_builtin_tool_switch(&state.workspace_dir, &key, next) {
        error!("Failed to persist tool switch {} = {}: {}", key, next, e);
        response["persisted"] = json!(false);
        response["persist_error"] = json!(format!("changed for this run, but not saved: {}", e));
    } else {
        response["persisted"] = json!(true);
    }
    Json(response)
}

async fn tools_toggle_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Json<Value> {
    let mut mgr = state.external_tools.lock().await;
    match mgr.toggle_tool(&name) {
        Some(enabled) => {
            mgr.save_state();
            let handles = mgr.get_tool_handles();
            drop(mgr);

            // Re-sync registry after toggle
            let mut registry = state.tools.write().await;
            registry.sync_external_tools(&handles);

            Json(json!({ "success": true, "enabled": enabled }))
        }
        None => Json(json!({ "success": false, "error": "Not found" })),
    }
}

async fn tools_desc_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let description = body["description"].as_str().unwrap_or("").to_string();
    let mut mgr = state.external_tools.lock().await;
    if mgr.update_description(&name, &description) {
        mgr.save_state();
        Json(json!({ "success": true }))
    } else {
        Json(json!({ "success": false, "error": "Not found" }))
    }
}

// ============================================================
// Computer Use toggle
// ============================================================

async fn computer_use_toggle_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let enabled = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let prev = state.computer_use_enabled.swap(enabled, Ordering::SeqCst);

    if prev != enabled {
        let mut registry = state.tools.write().await;
        if enabled {
            crate::tool::computer_use::register_computer_use_tools(&mut registry);
            info!("Computer Use tools ENABLED ({} tools registered)", crate::tool::computer_use::CU_TOOL_NAMES.len());
        } else {
            crate::tool::computer_use::unregister_computer_use_tools(&mut registry);
            info!("Computer Use tools DISABLED");
        }
    }

    // 以前这里只改内存就返回，重启后开关又回到 config 里的旧值。跟 Tools 页一致落盘。
    let mut response = json!({ "success": true, "enabled": enabled });
    if let Err(e) = crate::config::Config::set_builtin_tool_switch(&state.workspace_dir, "computer_use", enabled) {
        error!("Failed to persist computer_use={}: {}", enabled, e);
        response["persisted"] = json!(false);
        response["persist_error"] = json!(format!("changed for this run, but not saved: {}", e));
    } else {
        response["persisted"] = json!(true);
    }
    Json(response)
}

// ============================================================

// Human Intervention Simulation toggle
// ============================================================

async fn human_intervention_get_handler(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    let enabled = state.human_intervention_enabled.load(Ordering::SeqCst);
    Json(json!({ "success": true, "enabled": enabled }))
}

async fn human_intervention_toggle_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let enabled = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let prev = state.human_intervention_enabled.swap(enabled, Ordering::SeqCst);
    
    if prev != enabled {
        info!("Human Intervention Simulation {}", if enabled { "ENABLED" } else { "DISABLED" });
    }

    // 同上：不落盘的话这一勾只活到下一次重启。
    let mut response = json!({ "success": true, "enabled": enabled });
    if let Err(e) = crate::config::Config::set_builtin_tool_switch(&state.workspace_dir, "human_intervention", enabled) {
        error!("Failed to persist human_intervention={}: {}", enabled, e);
        response["persisted"] = json!(false);
        response["persist_error"] = json!(format!("changed for this run, but not saved: {}", e));
    } else {
        response["persisted"] = json!(true);
    }
    Json(response)
}

// Heartbeat toggle
// ============================================================

/// Re-spawn the heartbeat background loop. Used by the runtime toggle when the
/// user enables heartbeat after it was started disabled (or after a disable), so
/// off->on re-activation works without a restart.
fn spawn_heartbeat(state: &std::sync::Arc<AppState>) {
    if !state.heartbeat_enabled.load(Ordering::SeqCst) {
        // Only spawn when actually enabled; if enabled and already running, the
        // re-spawn is harmless (the previous loop exits on disable).
        return;
    }
    let heartbeat = Heartbeat::new(
        state.runner.clone(),
        state.model_configs.clone(),
        state.permissions.clone(),
        state.permission_pending.clone(),
        state.max_iterations.load(Ordering::SeqCst),
        state.rabbit_hole_threshold.load(Ordering::SeqCst),
        128000,
        state.context_window_threshold.load(Ordering::SeqCst),
        state.tool_timeout_secs.load(Ordering::SeqCst) as u64,
        state.notify_tx.clone(),
        state.workspace_dir.clone(),
        state.heartbeat_enabled.clone(),
    );
    tokio::spawn(async move {
        heartbeat.run_loop().await;
    });
    info!("Heartbeat background loop spawned (runtime enable)");
}

async fn heartbeat_get_handler(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    let enabled = state.heartbeat_enabled.load(Ordering::SeqCst);
    Json(json!({ "success": true, "enabled": enabled }))
}

async fn heartbeat_toggle_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let enabled = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let prev = state.heartbeat_enabled.swap(enabled, Ordering::SeqCst);
    
    if prev != enabled {
        info!("Heartbeat {}", if enabled { "ENABLED" } else { "DISABLED" });
        let _ = crate::config::Config::save_heartbeat_setting(&state.workspace_dir, enabled);
        if enabled {
            spawn_heartbeat(&state);
        }
    }
    
    Json(json!({ "success": true, "enabled": enabled }))
}

/// Save agent settings (max_iterations, rabbit_hole_threshold, etc.) to config.toml
/// and update the in-memory AppState.
async fn agent_settings_save_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let max_iterations = body.get("max_iterations")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.max_iterations.load(Ordering::SeqCst));
    let rabbit_hole_threshold = body.get("rabbit_hole_threshold")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.rabbit_hole_threshold.load(Ordering::SeqCst));
    let context_window_threshold = body.get("context_window_threshold")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.context_window_threshold.load(Ordering::SeqCst));
    let tool_timeout_secs = body.get("tool_timeout_secs")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.tool_timeout_secs.load(Ordering::SeqCst));
    let max_tool_retries = body.get("max_tool_retries")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.max_tool_retries.load(Ordering::SeqCst));
    let trim_redundant_tool_calls = body.get("trim_redundant_tool_calls")
        .and_then(|v| v.as_bool())
        .unwrap_or(state.trim_redundant_tool_calls.load(Ordering::SeqCst));
    let knowledge_pre_retrieval = body.get("knowledge_pre_retrieval")
        .and_then(|v| v.as_bool())
        .unwrap_or(state.knowledge_pre_retrieval.load(Ordering::SeqCst));
    let sop_replay = body.get("sop_replay")
        .and_then(|v| v.as_bool())
        .unwrap_or(state.sop_replay.load(Ordering::SeqCst));
    let two_tier_memory = body.get("two_tier_memory")
        .and_then(|v| v.as_bool())
        .unwrap_or(state.two_tier_memory.load(Ordering::SeqCst));
    let budget_dashboard = body.get("budget_dashboard")
        .and_then(|v| v.as_bool())
        .unwrap_or(state.budget_dashboard.load(Ordering::SeqCst));
    let linux_ir_tools = body.get("linux_ir_tools")
        .and_then(|v| v.as_bool())
        .unwrap_or(state.linux_ir_tools.load(Ordering::SeqCst));
    let browser_headless = body.get("browser_headless")
        .and_then(|v| v.as_bool())
        .unwrap_or(state.browser_headless.load(Ordering::SeqCst));
    let browser_executable = body.get("browser_executable")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| state.browser_executable.read().map(|g| g.clone()).unwrap_or_default());
    let enable_context_scaling = body.get("enable_context_scaling")
        .and_then(|v| v.as_bool())
        .unwrap_or(state.enable_context_scaling.load(Ordering::SeqCst));
    let max_inline_chars = body.get("max_inline_chars")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.max_inline_chars.load(Ordering::SeqCst));
    let skill_listing_strategy = body.get("skill_listing_strategy")
        .and_then(|v| v.as_str())
        .map(crate::skill::SkillListingStrategy::from_str)
        .unwrap_or_else(|| crate::skill::SkillListingStrategy::from_index(state.skill_listing_strategy.load(Ordering::SeqCst)));
    let skill_max_inline_chars = body.get("skill_max_inline_chars")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.skill_max_inline_chars.load(Ordering::SeqCst));
    let skill_catalog_max = body.get("skill_catalog_max")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.skill_catalog_max.load(Ordering::SeqCst));
    let skill_hot_top_k = body.get("skill_hot_top_k")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.skill_hot_top_k.load(Ordering::SeqCst));
    let session_max = body.get("session_max")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(state.session_max.load(Ordering::SeqCst));

    // Save to config.toml
    let workspace_dir = &state.workspace_dir;
    match crate::config::Config::save_agent_settings(
        workspace_dir,
        max_iterations,
        rabbit_hole_threshold,
        context_window_threshold,
        tool_timeout_secs,
        max_tool_retries,
        trim_redundant_tool_calls,
        knowledge_pre_retrieval,
        two_tier_memory,
        budget_dashboard,
        linux_ir_tools,
        enable_context_scaling,
        max_inline_chars,
        skill_listing_strategy.as_str().to_string(),
        skill_max_inline_chars,
        skill_catalog_max,
        skill_hot_top_k,
        browser_headless,
        browser_executable.clone(),
    ) {
        Ok(()) => {
            // Hot-reload in-memory values so the next run picks them up immediately
            state.max_iterations.store(max_iterations, Ordering::SeqCst);
            state.rabbit_hole_threshold.store(rabbit_hole_threshold, Ordering::SeqCst);
            state.context_window_threshold.store(context_window_threshold, Ordering::SeqCst);
            state.tool_timeout_secs.store(tool_timeout_secs, Ordering::SeqCst);
            state.max_tool_retries.store(max_tool_retries, Ordering::SeqCst);
            state.trim_redundant_tool_calls.store(trim_redundant_tool_calls, Ordering::SeqCst);
            state.knowledge_pre_retrieval.store(knowledge_pre_retrieval, Ordering::SeqCst);
            state.sop_replay.store(sop_replay, Ordering::SeqCst);
        state.two_tier_memory.store(two_tier_memory, Ordering::SeqCst);
        state.budget_dashboard.store(budget_dashboard, Ordering::SeqCst);
        state.linux_ir_tools.store(linux_ir_tools, Ordering::SeqCst);
            state.browser_headless.store(browser_headless, Ordering::SeqCst);
            if let Ok(mut g) = state.browser_executable.write() {
                *g = browser_executable.clone();
            }
            state.enable_context_scaling.store(enable_context_scaling, Ordering::SeqCst);
            state.max_inline_chars.store(max_inline_chars, Ordering::SeqCst);
            state.skill_listing_strategy.store(skill_listing_strategy.index(), Ordering::SeqCst);
            state.skill_max_inline_chars.store(skill_max_inline_chars, Ordering::SeqCst);
            state.skill_catalog_max.store(skill_catalog_max, Ordering::SeqCst);
            state.skill_hot_top_k.store(skill_hot_top_k, Ordering::SeqCst);
            state.session_max.store(session_max, Ordering::SeqCst);

            info!("Agent settings saved and hot-reloaded: max_iterations={}, rabbit_hole={}, ctx_threshold={}, tool_timeout={}, max_retries={}",
                max_iterations, rabbit_hole_threshold, context_window_threshold, tool_timeout_secs, max_tool_retries);
            Json(json!({
                "success": true,
                "max_iterations": max_iterations,
                "rabbit_hole_threshold": rabbit_hole_threshold,
                "context_window_threshold": context_window_threshold,
                "tool_timeout_secs": tool_timeout_secs,
                "max_tool_retries": max_tool_retries,
                "trim_redundant_tool_calls": trim_redundant_tool_calls,
                "knowledge_pre_retrieval": knowledge_pre_retrieval,
                "sop_replay": sop_replay,
                "two_tier_memory": two_tier_memory,
                "budget_dashboard": budget_dashboard,
                "linux_ir_tools": linux_ir_tools,
                "enable_context_scaling": enable_context_scaling,
                "max_inline_chars": max_inline_chars,
                "skill_listing_strategy": skill_listing_strategy.as_str().to_string(),
                "skill_max_inline_chars": skill_max_inline_chars,
                "skill_catalog_max": skill_catalog_max,
                "skill_hot_top_k": skill_hot_top_k,
            }))
        }
        Err(e) => {
            error!("Failed to save agent settings: {}", e);
            Json(json!({ "success": false, "error": format!("Failed to save: {}", e) }))
        }
    }
}

/// Save extended agent settings (model selection, timezone, permissions) to config.toml.
async fn agent_settings_extended_save_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let primary_model = body.get("primary_model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let fallback_model = body.get("fallback_model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let timezone_offset = body.get("timezone_offset")
        .and_then(|v| v.as_i64())
        .map(|v| v as i8)
        .unwrap_or(8);

    // Parse tool_permissions from the request
    let tool_permissions: std::collections::HashMap<String, bool> = body.get("tool_permissions")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter().filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b))).collect())
        .unwrap_or_default();

    let workspace_dir = &state.workspace_dir;
    match crate::config::Config::save_extended_settings(
        workspace_dir,
        primary_model.clone(),
        fallback_model.clone(),
        timezone_offset,
        tool_permissions.clone(),
    ) {
        Ok(()) => {
            // Hot-reload in-memory values
            if let Some(ref m) = primary_model {
                *state.primary_model.write().unwrap() = Some(m.clone());
            }
            if let Some(ref m) = fallback_model {
                *state.fallback_model.write().unwrap() = Some(m.clone());
            }
            *state.timezone_offset.write().unwrap() = timezone_offset;

            info!("Extended settings saved and hot-reloaded: primary_model={:?}, fallback_model={:?}, timezone={}, permissions={:?}",
                primary_model, fallback_model, timezone_offset, tool_permissions);
            Json(json!({
                "success": true,
                "primary_model": primary_model,
                "fallback_model": fallback_model,
                "timezone_offset": timezone_offset,
                "tool_permissions": tool_permissions,
            }))
        }
        Err(e) => {
            error!("Failed to save extended settings: {}", e);
            Json(json!({ "success": false, "error": format!("Failed to save: {}", e) }))
        }
    }
}

/// Save Expert mode settings to config.toml (iterations/timeout/retries/rounds)
/// plus per-role model overrides for Manager/Auditor/Executor and their fallbacks.
async fn agent_settings_expert_save_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let expert_max_iterations = body.get("expert_max_iterations")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(200);
    let expert_tool_timeout_secs = body.get("expert_tool_timeout_secs")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(600);
    let expert_max_tool_retries = body.get("expert_max_tool_retries")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(3);
    let expert_max_managed_rounds = body.get("expert_max_managed_rounds")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(50);

    // Per-role model overrides (optional). Empty/missing => role uses the primary model.
    let str_opt = |k: &str| body.get(k)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let role_models = crate::config::RoleModelsConfig {
        manager: str_opt("role_manager"),
        manager_fallback: str_opt("role_manager_fallback"),
        auditor: str_opt("role_auditor"),
        auditor_fallback: str_opt("role_auditor_fallback"),
        executor: str_opt("role_executor"),
        executor_fallback: str_opt("role_executor_fallback"),
    };

    let workspace_dir = &state.workspace_dir;
    let save_main = crate::config::Config::save_expert_settings(
        workspace_dir,
        expert_max_iterations,
        expert_tool_timeout_secs,
        expert_max_tool_retries,
        expert_max_managed_rounds,
    );
    let save_roles = crate::config::Config::save_role_models(workspace_dir, &role_models);
    if let Err(e) = &save_main {
        error!("Failed to save expert settings: {}", e);
        return Json(json!({ "success": false, "error": format!("Failed to save: {}", e) }));
    }
    if let Err(e) = &save_roles {
        error!("Failed to save role models: {}", e);
        return Json(json!({ "success": false, "error": format!("Failed to save role models: {}", e) }));
    }

    // Hot-reload in-memory values so the next Expert run picks them up
    state.expert_max_iterations.store(expert_max_iterations, Ordering::SeqCst);
    state.expert_tool_timeout_secs.store(expert_tool_timeout_secs, Ordering::SeqCst);
    state.expert_max_tool_retries.store(expert_max_tool_retries, Ordering::SeqCst);
    state.expert_max_managed_rounds.store(expert_max_managed_rounds, Ordering::SeqCst);
    *state.expert_role_models.write().unwrap() = role_models.clone();

    info!("Expert settings saved: max_iter={}, timeout={}, retries={}, rounds={}, role_models={:?}",
        expert_max_iterations, expert_tool_timeout_secs, expert_max_tool_retries, expert_max_managed_rounds, role_models);
    Json(json!({
        "success": true,
        "expert_max_iterations": expert_max_iterations,
        "expert_tool_timeout_secs": expert_tool_timeout_secs,
        "expert_max_tool_retries": expert_max_tool_retries,
        "expert_max_managed_rounds": expert_max_managed_rounds,
        "role_models": role_models,
    }))
}

// ============================================================
// Config Files (AGENTS.md, SOUL.md, TOOLS.md)
// ============================================================

async fn config_files_handler(State(state): State<Arc<AppState>>) -> Result<Json<Value>, (axum::http::StatusCode, Json<Value>)> {
    let workspace = &state.workspace_dir;
    let files = ["AGENTS.md", "SOUL.md", "TOOLS.md", "MEMORY.md", "USER.md"];
    let mut result = serde_json::Map::new();

    for file_name in &files {
        let path = std::path::Path::new(workspace).join(file_name);
        // A2：读失败必须显式报错，区分"不存在"（返回空）与"读取失败"（返回错误）——
        // 杜绝"静默读空 → 保存清空"的身份文件丢失回路。B2：改用异步 tokio::fs。
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                result.insert(file_name.to_string(), json!(content));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                result.insert(file_name.to_string(), json!(""));
            }
            Err(e) => {
                return Err((
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to read config file {}: {}", file_name, e) })),
                ));
            }
        }
    }

    Ok(Json(json!({ "files": result, "workspace_dir": workspace })))
}

async fn config_file_save_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (axum::http::StatusCode, Json<Value>)> {
    let allowed = ["AGENTS.md", "SOUL.md", "TOOLS.md", "MEMORY.md", "USER.md"];
    if !allowed.contains(&name.as_str()) {
        return Ok(Json(json!({ "success": false, "error": "Invalid file name. Allowed: AGENTS.md, SOUL.md, TOOLS.md, MEMORY.md, USER.md" })));
    }

    let content = body["content"].as_str().unwrap_or("");
    let path = std::path::Path::new(&state.workspace_dir).join(&name);

    // A2：空内容覆盖非空文件 → 拒绝（防"读空→写空"清空人格/规则/身份文件）。
    // 与"用户刻意清空"区分：只有现有内容非空且新内容为空才拦截。
    if content.trim().is_empty() {
        match tokio::fs::read_to_string(&path).await {
            Ok(existing) if !existing.trim().is_empty() => {
                return Ok(Json(json!({
                    "success": false,
                    "error": format!("Refusing to overwrite non-empty {} with empty content (A2 clobber guard)", name)
                })));
            }
            _ => {}
        }
    }

    if let Err(e) = tokio::fs::create_dir_all(&state.workspace_dir).await {
        return Ok(Json(json!({ "success": false, "error": format!("Failed to create workspace: {}", e) })));
    }

    // A2：覆盖前保留 .bak 快照（仅当现有内容非空且与本次不同）。
    if !content.trim().is_empty() {
        if let Ok(existing) = tokio::fs::read_to_string(&path).await {
            if !existing.trim().is_empty() && existing != content {
                let bak = path.with_extension("md.bak");
                let _ = tokio::fs::write(&bak, &existing).await;
            }
        }
    }

    match tokio::fs::write(&path, content).await {
        Ok(_) => {
            info!("Config file saved: {}", path.display());
            Ok(Json(json!({ "success": true, "file": name })))
        }
        Err(e) => Ok(Json(json!({ "success": false, "error": format!("Failed to save: {}", e) }))),
    }
}

// ============================================================
// Checkpoint Handlers
// ============================================================

async fn checkpoints_list_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.memory_store.list_checkpoints() {
        Ok(cps) => {
            // Return metadata only — do NOT send the full history_json to the client.
            let items: Vec<Value> = cps.iter().map(|cp| {
                json!({
                    "id": cp.id,
                    "session_id": cp.session_id,
                    "model_name": cp.model_name,
                    "user_message": cp.user_message.chars().take(200).collect::<String>(),
                    "iteration": cp.iteration,
                    "tool_summary": cp.tool_summary,
                    "created_at": cp.created_at,
                    "updated_at": cp.updated_at,
                })
            }).collect();
            Json(json!({ "checkpoints": items, "count": items.len() }))
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

async fn checkpoints_delete_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    match state.memory_store.delete_checkpoint(&id) {
        Ok(_) => {
            info!("Checkpoint {} deleted via API", id);
            Json(json!({ "ok": true }))
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

/// Detect whether the user's message is asking about earlier conversations.
/// Used to trigger mid-session injection of the memory context so the agent
/// can recall past topics instead of claiming it has no history.
/// Detect a continuation-style first message ("continue previous work") that
/// implies the user expects us to remember and resume prior context, even
/// without explicit past-tense recall keywords.
fn is_continuation_task(text: &str) -> bool {
    let lower = text.to_lowercase();
    const KEYWORDS: &[&str] = &[
        "继续", "接着", "还有", "接下来", "回到", "还没",
        "继续上次", "接着上次", "继续之前", "接着之前", "继续干", "接着干",
        "未完", "上一个", "上次那个",
        "continue", "go on", "go ahead", "resume", "keep going",
        "next", "and then", "onto", "still",
    ];
    KEYWORDS.iter().any(|k| lower.contains(k))
}

fn is_recall_query(text: &str) -> bool {
    let lower = text.to_lowercase();
    const KEYWORDS: &[&str] = &[
        // Chinese
        "之前", "昨天", "前天", "上次", "历史", "过往", "以前",
        "记得", "回忆", "我们讨论", "我们聊", "我们说过", "你之前",
        "你说过", "之前的对话", "前几次",
        "什么时候",
        "何时",
        "哪一天",
        "哪天",
        "几月",
        "几号",
        "什么时间",
        "的时候",
        "时间点",
        "是具体",
        "具体是",
        "查一下",
        "查查",
        "查一查",
        "是什么",
        "是哪次",
        "哪次",
        // English
        "previous", "yesterday", "last time", "earlier", "we discussed",
        "we talked", "do you remember", "chat history", "previous chat",
        "earlier conversation", "before we",
        "when did", "when was", "what time", "which day", "what date",
        "what happened", "tell me about", "remind me", "look up",
        "what was", "details on",
    ];
    KEYWORDS.iter().any(|k| lower.contains(k))
}

// ============================================================
// Token usage tracking API
// ============================================================

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default = "default_usage_days")]
    days: usize,
    #[serde(default)]
    tz: f64,
}

fn default_usage_days() -> usize { 7 }

async fn usage_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UsageQuery>,
) -> Json<Value> {
    // days == 0 -> all-time cumulative; otherwise clamp to [1, 90]
    let days = if query.days == 0 { 0 } else { query.days.max(1).min(90) };
    // Clamp timezone offset to a sane range (UTC-12 .. UTC+14)
    let tz = query.tz.max(-12.0).min(14.0);
    match state.memory_store.get_usage_stats(days, tz) {
        Ok(data) => {
            // Compute summary totals from the data array
            let mut total_calls: i64 = 0;
            let mut total_prompt: i64 = 0;
            let mut total_completion: i64 = 0;
            let mut total_tokens: i64 = 0;
            // Prompt-cache rollup. The denominator is input tokens of calls that
            // actually reported cache accounting (`cache_input_tokens`), never all
            // input tokens: a cache-blind endpoint must produce a null hit rate,
            // not a misleading 0%.
            let mut total_cached: i64 = 0;
            let mut total_cache_input: i64 = 0;
            let mut cache_reported_calls: i64 = 0;
            if let Some(arr) = data.as_array() {
                for item in arr {
                    total_calls += item["calls"].as_i64().unwrap_or(0);
                    total_prompt += item["prompt_tokens"].as_i64().unwrap_or(0);
                    total_completion += item["completion_tokens"].as_i64().unwrap_or(0);
                    total_tokens += item["total_tokens"].as_i64().unwrap_or(0);
                    total_cached += item["cached_tokens"].as_i64().unwrap_or(0);
                    total_cache_input += item["cache_input_tokens"].as_i64().unwrap_or(0);
                    cache_reported_calls += item["cache_reported_calls"].as_i64().unwrap_or(0);
                }
            }
            let cache_hit_rate = if total_cache_input > 0 {
                json!(total_cached as f64 / total_cache_input as f64)
            } else {
                Value::Null
            };
            Json(json!({
                "days": days,
                "data": data,
                "summary": {
                    "total_calls": total_calls,
                    "total_prompt_tokens": total_prompt,
                    "total_completion_tokens": total_completion,
                    "total_tokens": total_tokens,
                    "total_cached_tokens": total_cached,
                    "total_cache_input_tokens": total_cache_input,
                    "cache_reported_calls": cache_reported_calls,
                    "cache_hit_rate": cache_hit_rate,
                }
            }))
        },
        Err(e) => Json(json!({ "error": e })),
    }
}

async fn usage_today_handler(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    match state.memory_store.get_today_usage() {
        Ok(data) => Json(data),
        Err(e) => Json(json!({ "error": e })),
    }
}

/// 统一上下文预算（Finite Brain）仪表盘数据：返回窗口 / 预留 / 占用 / 剩余
/// 及各用途的估算分配，供 Dashboard 页的「有限脑预算」视图渲染。
async fn sop_stats_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let ws = state.workspace_dir.clone();
    let metrics = crate::sop::sop_metrics(&ws);
    Json(json!({
        "metrics": metrics,
        "replay_enabled": state.sop_replay.load(Ordering::SeqCst),
    }))
}
async fn budget_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let enabled = state.budget_dashboard.load(Ordering::SeqCst);
    let threshold = state.context_window_threshold.load(Ordering::SeqCst).max(1).min(100);

    // 优先显示 agent 每次真正组装上下文时写入的实测快照（与系统提示同源）。
    let snap = state.context_budget.lock().unwrap().clone();
    let latest = snap.is_some();

    let report = match snap {
        Some(r) => r,
        None => {
            // 尚无会话：显示配置包络（window / reserve，used=0）。
            let window = {
                let model_configs = state.model_configs.read().await;
                model_configs.iter().map(|m| m.context_window).max().unwrap_or(128_000)
            };
            let reserve = window / 10 + window / 50;
            crate::context_arbiter::budget_report(window, reserve, &[])
        }
    };

    let max_history = report.window * threshold / 100;
    let lines: Vec<Value> = report.lines.iter()
        .map(|l| json!({ "category": l.category, "tokens": l.tokens }))
        .collect();

    Json(json!({
        "enabled": enabled,
        "window_threshold_pct": threshold,
        "max_history_budget": max_history,
        "window": report.window,
        "reserve": report.reserve,
        "used": report.used,
        "free": report.free,
        "latest": latest,
        "lines": lines,
    }))
}






/// 写入路径：从 assistant 文本剥离模型可能吐出的 <memory> 块（记忆持久化由后台
/// curator 负责）。仅当记忆开关开启时生效（默认开）。
fn two_tier_write(state: &AppState, assistant_text: &mut String, session_id: &str, user_text: &str) {
    if !state.two_tier_memory.load(Ordering::SeqCst) {
        return;
    }
    // O2: 记忆持久化统一交给后台 deep curator（findings 蒸馏）。本函数只负责从
    // 展示文本剥离模型可能吐出的 <memory> 块。
    let _ = session_id;
    let _ = user_text;
    *assistant_text = crate::deep_memory::strip_memory_blocks(assistant_text);
}






