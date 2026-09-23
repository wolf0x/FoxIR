use std::sync::Arc;
use async_trait::async_trait;
use serde_json::Value;
use tracing::{error, info, warn};

use crate::agent::{Agent, AgentEvent, EventStream};
use crate::callbacks::AgentCallbacks;
use crate::config::ModelConfig;
use crate::context::{InvocationContext, ToolContext};
use crate::error::{AgentError, AgentResult};
use crate::event_log::{EventLog, LogEvent};
use crate::model::openai::OpenAiProvider;
use crate::model::ChatMessage;
use crate::permission::{PermissionChecker, PendingMap};
use crate::skill::SkillManager;
use crate::tool::{ToolExecutionStrategy, ToolRegistry};

/// IR collection tools that are safe for parallel execution.
/// These tools only read system state and do not modify anything.
/// When multiple of these are called together, they can run concurrently
/// to speed up incident triage (3-4x faster for full collection).
pub const IR_COLLECTION_TOOLS: &[&str] = &[
    "ir_scan",
    "ir_process",
    "ir_account",
    "ir_persistence",
    "ir_network",
    "ir_eventlog",
    "ir_file",
    "ir_driver",
    "ir_timeline",
];

/// Check if a tool name is part of the IR collection set (safe for parallel execution).
#[inline]
pub fn is_ir_collection_tool(name: &str) -> bool {
    IR_COLLECTION_TOOLS.contains(&name)
}

/// Check if all tool calls in a batch are from the IR collection set.
/// Returns true only if there are 2+ calls and ALL are IR collection tools.
pub fn is_ir_collection_batch(tool_calls: &[crate::model::ToolCallDelta]) -> bool {
    tool_calls.len() >= 2
        && tool_calls.iter().all(|tc| {
            tc.function.name.as_deref().map(is_ir_collection_tool).unwrap_or(false)
        })
}

/// 传输层截断（流被传输错误切断）后的补救决定。
#[derive(Debug, PartialEq, Eq)]
pub enum StreamCutRecovery {
    /// 已无补救预算：本轮以残缺文本收尾，由主循环判定为失败结局。
    Exhausted,
    /// 有补救预算：把残文并入历史并再请求一轮继续收尾。
    Continue {
        /// 从截断流中解析出、已丢弃的工具调用数量（其参数是残缺的）。
        dropped_tool_calls: usize,
    },
}

/// 传输层截断后如何处置。read_timeout / 中途断连都只留下残缺前缀，与模型是否给出回答无关；
/// 未处理时前端只会收到残缺片段，而主循环却按正常文本回答收尾。
/// 从截断流解析出的工具调用参数残缺，任何情况下都要丢弃。
pub fn classify_stream_cut(
    stream_timed_out: bool,
    parsed_tool_calls: usize,
    recoveries_used: u32,
    max_recoveries: u32,
) -> Option<StreamCutRecovery> {
    if !stream_timed_out {
        return None;
    }
    if recoveries_used >= max_recoveries {
        return Some(StreamCutRecovery::Exhausted);
    }
    Some(StreamCutRecovery::Continue { dropped_tool_calls: parsed_tool_calls })
}

/// 文本回路自动停止判定。截断回合的 fragment 是残缺前缀，既不推进重复计数，也不得在随后的完整回合里误触发自动停止。
pub fn should_auto_stop_text_loop(
    stream_timed_out: bool,
    resp_digest: u64,
    last_resp_digest: &mut u64,
    consecutive_resp: &mut usize,
    limit: usize,
) -> bool {
    if stream_timed_out {
        *consecutive_resp = 0;
        *last_resp_digest = 0;
        return false;
    }
    if resp_digest == *last_resp_digest {
        *consecutive_resp += 1;
    } else {
        *last_resp_digest = resp_digest;
        *consecutive_resp = 1;
    }
    *consecutive_resp >= limit
}

/// Estimate token count from text content (delegates to the unified
/// CJK-aware estimator in `deep_memory` so all budget accounting agrees).
fn estimate_tokens(text: &str) -> usize {
    crate::deep_memory::estimate_tokens(text)
}

/// System-prompt tier (nested prefixes: Minimal is a strict byte-prefix of
/// Full). Selected per user message; Minimal serves pure greetings with the
/// persona head only, skipping the full tool/rulebook sections (~80% smaller
/// system prompt on trivial turns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptTier {
    Minimal,
    Full,
}

impl PromptTier {
    fn select(user_message: &str) -> Self {
        if is_pure_greeting(user_message) {
            PromptTier::Minimal
        } else {
            PromptTier::Full
        }
    }
}

/// Strict pure-greeting test - deliberately much stricter than
/// `looks_like_greeting` (which only detects language-neutral small talk for
/// the language rule): the ENTIRE message, stripped of punctuation, must
/// consist solely of known greeting tokens. Anything task-like (even
/// "hi, what's my IP") falls back to the Full tier.
fn is_pure_greeting(text: &str) -> bool {
    const GREETING_TOKENS: &[&str] = &[
        "hi", "hello", "hey", "yo", "gm", "morning", "evening",
        "good morning", "good afternoon", "good evening",
        "你好", "您好", "早上好", "下午好", "晚上好", "在吗", "嗨", "哈喽", "早", "早安",
        "谢谢", "thanks", "thank you", "thx",
    ];
    let t = text.trim();
    if t.is_empty() || t.chars().count() > 30 {
        return false;
    }
    let cleaned: String = t
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .trim()
        .to_lowercase();
    if cleaned.is_empty() {
        return false;
    }
    if GREETING_TOKENS.contains(&cleaned.as_str()) {
        return true;
    }
    // "hi hi" / "hello hello" style repetition.
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    words.len() <= 3 && words.iter().all(|w| GREETING_TOKENS.contains(w))
}

/// 是否含 64/32 位十六进制哈希串（证据完整性标记，不可再生）。
fn has_hash_token(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_hexdigit()).any(|t| t.len() >= 32)
}

/// 是否含文件路径指针（盘符 `?:\` / `?:/`，或 output 目录）——证据/产物的落盘位置。
fn has_file_path(text: &str) -> bool {
    let b = text.as_bytes();
    for i in 0..b.len().saturating_sub(2) {
        if b[i].is_ascii_alphabetic() && b[i + 1] == b':' && (b[i + 2] == b'\\' || b[i + 2] == b'/') {
            return true;
        }
    }
    text.contains("output\\") || text.contains("output/")
}

/// 判断一条历史消息是否为「不可再生证据」：`ir_*` 取证工具结果、含文件路径 / 哈希 / 明确落盘
/// 陈述的内容。裁剪时须保护——只可降级为「指针」保留引用，绝不整体丢弃，
/// 对齐铁律「never lose completed work」。见 SDD §12.6。
fn is_non_reproducible_evidence(name: Option<&str>, text: &str) -> bool {
    if let Some(n) = name {
        let nl = n.to_ascii_lowercase();
        if nl.starts_with("ir_") || nl.starts_with("forensic") {
            return true;
        }
    }
    let lower = text.to_ascii_lowercase();
    has_hash_token(text)
        || has_file_path(text)
        || lower.contains("written to")
        || lower.contains("saved to")
        || lower.contains("sha256")
        || lower.contains("md5")
}

/// 把证据消息降级为紧凑「指针」：保留哈希 / 文件路径引用 + 极短头部摘要，
/// 正文省略。即便被压缩也保留可追溯的引用，而非删除。
fn evidence_pointer(name: &str, text: &str) -> String {
    let mut refs: Vec<String> = Vec::new();
    for tok in text.split_whitespace() {
        let clean = tok.trim_matches(|c: char| !c.is_ascii_graphic());
        let cb = clean.as_bytes();
        let is_hex = clean.len() >= 32 && clean.chars().all(|c| c.is_ascii_hexdigit());
        let is_path = cb.len() > 4 && cb[1] == b':' && (cb[2] == b'\\' || cb[2] == b'/');
        if (is_hex || is_path) && !refs.iter().any(|r| r == clean) {
            refs.push(clean.to_string());
            if refs.len() >= 4 {
                break;
            }
        }
    }
    let head: String = text.chars().take(120).collect();
    if refs.is_empty() {
        format!("[{} evidence pointer: {}...]", name, head)
    } else {
        format!("[{} evidence pointer — refs: {} | {}...]", name, refs.join(", "), head)
    }
}

/// 一条旧历史消息的保留价值：角色权重 + 近因加成 + 不可再生证据强加权。
/// 返回 (value, is_evidence)。value 为排序标量，供 `context_arbiter::rank_key` 融合相关性。
fn history_retain_value(i: usize, keep_recent: usize, role: &str, name: Option<&str>, text: &str) -> (f64, bool) {
    let evidence = is_non_reproducible_evidence(name, text);
    let role_w = match role {
        "user" => 1.6,       // 用户意图最珍贵
        "tool" => 1.2,       // 工具结果携带数据
        "system" => 1.0,
        "assistant" => 0.8,  // 推理可再生
        _ => 0.6,
    };
    let recency = if keep_recent > 0 { (i as f64 / keep_recent as f64).clamp(0.0, 1.0) } else { 0.0 };
    let ev = if evidence { 2.0 } else { 0.0 };
    (role_w + recency * 0.5 + ev, evidence)
}

/// 对单条历史消息应用某个降级档。仅改写 `content`，保留 `role`/`tool_calls`/`tool_call_id`
/// 结构（不破坏 assistant↔tool 配对）。
fn apply_history_degrade(history: &mut [ChatMessage], i: usize, evidence: bool, level: u8) {
    let name = history[i].name.clone().unwrap_or_else(|| history[i].role.clone());
    let text = history[i].content_as_text().unwrap_or_default();
    if evidence {
        // 不可再生证据：降级为「指针」，保留哈希/路径引用，绝不整体丢弃。
        history[i].content = Some(Value::String(evidence_pointer(&name, &text)));
        return;
    }
    match level {
        1 => {
            let cap = if history[i].role == "assistant" { 200 } else { 100 };
            let preview: String = text.chars().take(cap).collect();
            history[i].content = Some(Value::String(format!(
                "[earlier {} truncated: {}...]", history[i].role, preview)));
        }
        2 => {
            history[i].content = Some(Value::String(format!(
                "[earlier {} result elided: {}]", history[i].role, name)));
        }
        _ => {}
    }
}

/// 价值导向裁剪（有限脑 §12.6，替换旧的价值盲裁剪）：
/// 从「按角色/近因截断」改为「按保留价值降级」，与「never lose completed work」对齐。
///
/// - 保护最近 6 条（永不裁剪）。
/// - 反复挑选 `rank_key = value×(0.5+relevance)` 最低、尚可降级的旧消息降一档：
///   非证据 全文→摘要→占位指针；证据 全文→指针（保留哈希/路径，永不丢弃）。
/// - 降级反应式 → 但每步用统一排序键决策，先压可再生的低价值旧工具结果，保护不可再生证据。
fn trim_history_by_value(history: &mut Vec<ChatMessage>, max_tokens: usize) {
    let calc = |h: &[ChatMessage]| -> usize {
        h.iter().map(|m| estimate_tokens(m.content_as_text().as_deref().unwrap_or(""))).sum()
    };

    if calc(history.as_slice()) <= max_tokens {
        return;
    }

    let len = history.len();
    let keep_recent = (len.saturating_sub(6)).max(3).min(len);

    #[derive(Clone, Copy)]
    struct Plan { key: f64, evidence: bool, level: u8 }
    let mut plans: Vec<Plan> = Vec::with_capacity(len);
    for (i, m) in history.iter().enumerate() {
        if i >= keep_recent {
            plans.push(Plan { key: f64::INFINITY, evidence: false, level: u8::MAX });
            continue;
        }
        let text = m.content_as_text().unwrap_or_default();
        let (value, evidence) = history_retain_value(i, keep_recent, &m.role, m.name.as_deref(), &text);
        let relevance = (i as f64 / keep_recent.max(1) as f64).clamp(0.0, 1.0);
        let key = crate::context_arbiter::rank_key(value, relevance);
        plans.push(Plan { key, evidence, level: 0 });
    }

    let mut guard = 0usize;
    while calc(history.as_slice()) > max_tokens && guard < 4 * len + 16 {
        guard += 1;
        // 选最低 rank_key 且尚可降级者（证据至多降到指针 level=1）。
        let mut pick: Option<usize> = None;
        for i in 0..keep_recent {
            let max_level = if plans[i].evidence { 1 } else { 2 };
            if plans[i].level >= max_level {
                continue;
            }
            match pick {
                None => pick = Some(i),
                Some(j) if plans[i].key < plans[j].key => pick = Some(i),
                _ => {}
            }
        }
        let Some(i) = pick else { break; };
        plans[i].level += 1;
        apply_history_degrade(history, i, plans[i].evidence, plans[i].level);
    }

    let final_tokens = calc(history.as_slice());
    if final_tokens > max_tokens {
        warn!("History still exceeds budget after value-oriented trim: {} tokens (limit: {})", final_tokens, max_tokens);
    }
}

/// The core LLM-powered agent.
/// Implements the Agent trait (modeled after ADK-RUST's LlmAgent).
///
/// The agent loop is lightweight and LLM-driven:
/// 1. Build system prompt (with skill context)
/// 2. Send messages + tool schemas to LLM (streaming)
/// 3. If LLM returns tool_calls → execute tools → loop back
/// 4. If LLM returns text → done
/// Orchestration tool names. Hidden from the model via delivery gating unless
/// the mode/depth allowset opens (SDD \u00a77.3). Step 1 keeps allowset empty
/// for all modes (Expert included) so there is zero behavior diff.
pub const ALL_ORCH: [&str; 7] = [
    "spawn_subagent",
    "wait_subagent",
    "list_subagents",
    "cancel_subagent",
    "get_subagent_result",
    "update_plan",
    "read_subagent_log",
];

/// Delivery-gate truth table. Step 1: returns empty for every mode/depth.
/// Step 2a opens `Instant && depth == 0` to ALL_ORCH.
pub(crate) fn is_orchestration_name(name: &str) -> bool {
    ALL_ORCH.contains(&name)
}

pub fn orchestration_allowset(mode: crate::context::AgentMode, depth: u8) -> Vec<String> {
    // Step 2a opens the delivery gate for the Instant root run (depth 0) so the
    // manager can call the orchestration tools. Workers (depth >= 1) never get them.
    if mode == crate::context::AgentMode::Instant && depth == 0 {
        ALL_ORCH.iter().map(|s| s.to_string()).collect()
    } else {
        Vec::new()
    }
}

/// D10: delivery gate AND route gate are one. Orchestration tools are delivered
/// only when the cheap pre-filter flags the run as a fan-out candidate.
pub fn orchestration_delivered_for(
    mode: crate::context::AgentMode, depth: u8, candidate: bool,
) -> Vec<String> {
    if candidate { orchestration_allowset(mode, depth) } else { Vec::new() }
}

/// Cheap rule-based pre-filter (D2 layer 1 / D10). Zero LLM cost. Conservative:
/// a false negative only means "stay on the main loop", never a wrong fan-out.
pub fn orchestration_prefilter(user_message: &str) -> bool {
    use std::sync::OnceLock;
    static IP_RE: OnceLock<regex::Regex> = OnceLock::new();
    let lower = user_message.to_lowercase();
    const PARALLEL_WORDS: &[&str] =
        &["分别", "并行", "各自", "同时", "逐个", "respectively", "in parallel", "each of"];
    if PARALLEL_WORDS.iter().any(|w| lower.contains(w)) {
        return true;
    }
    let ip_re = IP_RE.get_or_init(|| regex::Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").unwrap());
    let ips: std::collections::HashSet<&str> =
        ip_re.find_iter(&lower).map(|m| m.as_str()).collect();
    if ips.len() >= 2 {
        return true;
    }
    const SOURCES: &[&str] = &[
        "进程", "服务", "注册表", "日志", "文件", "网络", "内存",
        "prefetch", "evtx", "pcap", "process", "service", "registry", "log",
    ];
    SOURCES.iter().filter(|s| lower.contains(**s)).count() >= 3
}

/// Delivery-gate predicate (SDD §7.3). An orchestration tool is delivered
/// to the model only when its name is *not* in `ALL_ORCH`, or when the allowset
/// explicitly opens it. Step 1 returns an empty allowset so the gate strips all
/// seven orchestration tools from every mode (zero behavior diff).
pub fn orchestration_delivered(name: &str, allowset: &[String]) -> bool {
    !ALL_ORCH.contains(&name) || allowset.iter().any(|n| n == name)
}

/// True only for the user's main interactive session. Sub/cron sessions are
/// excluded so they write to session-scoped files and never see the main TODO.
pub(crate) fn is_main_session(session_id: &str) -> bool {
    !session_id.is_empty()
        && !session_id.starts_with("cron-")
        && !session_id.starts_with("sub-")
}

pub struct LlmAgent {
    name: String,
    description: String,
    provider: Arc<OpenAiProvider>,
    tools: Arc<tokio::sync::RwLock<ToolRegistry>>,
    skill_manager: Option<Arc<SkillManager>>,
    mode: crate::context::AgentMode,
    depth: u8,
    max_iterations: usize,
    working_dir: String,
    workspace_dir: String,
    model_configs: Vec<ModelConfig>,
    #[allow(dead_code)]
    callbacks: AgentCallbacks,
    tool_execution_strategy: ToolExecutionStrategy,
    /// Enable parallel execution for IR collection tools.
    /// When true, batches of IR collection tools run concurrently.
    parallel_ir_tools: bool,
    /// User's given name (auto-detected from Windows at startup).
    user_given_name: String,
    /// Whether deep-memory injection is enabled (server injects the permanent block).
    two_tier_memory: bool,
    /// Sessions in which a task-matched SKILL drove a turn (used to skip SOP
    /// authoring for skill-driven sessions). Shared with AppState.
    skill_used_sessions: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Independent SOP replay switch (default on; independent of knowledge_pre_retrieval).
    sop_replay: Arc<std::sync::atomic::AtomicBool>,
    /// Sessions to clean up after the agent loop completes.
    cleanup_sessions: Vec<Arc<crate::tool::browser_cdp::BrowserSession>>,
    /// Optional memory store for persisting sub-agent results (SDD v1.5 2.4).
    memory_store: Option<Arc<crate::memory::MemoryStore>>,
    /// Orchestration limits for spawned sub-agents (SDD v1.5 §9).
    orchestration_limits: crate::config::OrchestrationLimits,
}

/// Builder for LlmAgent (modeled after ADK-RUST's LlmAgentBuilder).
pub struct LlmAgentBuilder {
    name: String,
    description: String,
    provider: Option<Arc<OpenAiProvider>>,
    tools: Option<Arc<tokio::sync::RwLock<ToolRegistry>>>,
    skill_manager: Option<Arc<SkillManager>>,
    skill_manager_disabled: bool,
    mode: crate::context::AgentMode,
    depth: u8,
    max_iterations: usize,
    working_dir: String,
    workspace_dir: String,
    model_configs: Vec<ModelConfig>,
    callbacks: AgentCallbacks,
    tool_execution_strategy: ToolExecutionStrategy,
    parallel_ir_tools: bool,
    user_given_name: String,
    two_tier_memory: bool,
    skill_used_sessions: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    sop_replay: Arc<std::sync::atomic::AtomicBool>,
    cleanup_sessions: Vec<Arc<crate::tool::browser_cdp::BrowserSession>>,
    memory_store: Option<Arc<crate::memory::MemoryStore>>,
    orchestration_limits: crate::config::OrchestrationLimits,
}

impl LlmAgentBuilder {
    pub fn new() -> Self {
        Self {
            name: "RustAgent".to_string(),
            description: "Local AI agent with Windows system tools".to_string(),
            provider: None,
            tools: None,
            skill_manager: None,
            skill_manager_disabled: false,
            mode: crate::context::AgentMode::Instant,
            depth: 0,
            max_iterations: 100,
            working_dir: ".".to_string(),
            workspace_dir: String::new(),
            model_configs: Vec::new(),
            callbacks: AgentCallbacks::new(),
            tool_execution_strategy: ToolExecutionStrategy::Sequential,
            parallel_ir_tools: true,
            user_given_name: "User".to_string(),
            two_tier_memory: true,
            skill_used_sessions: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            sop_replay: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            cleanup_sessions: Vec::new(),
            memory_store: None,
            orchestration_limits: crate::config::OrchestrationLimits::default(),
        }
    }

    pub fn name(mut self, name: &str) -> Self { self.name = name.to_string(); self }
    pub fn description(mut self, desc: &str) -> Self { self.description = desc.to_string(); self }
    pub fn provider(mut self, provider: Arc<OpenAiProvider>) -> Self { self.provider = Some(provider); self }
    pub fn tools(mut self, tools: Arc<tokio::sync::RwLock<ToolRegistry>>) -> Self { self.tools = Some(tools); self }
    pub fn skill_manager(mut self, sm: Arc<SkillManager>) -> Self { self.skill_manager = Some(sm); self }
    /// Explicitly disable skill injection for this agent (SDD \u00a720.3 B4.3).
    /// Preserves the old default attach for builders that do NOT call this, so
    /// existing Instant/Expert agents keep `SkillManager` (G-instant-diff).
    pub fn without_skills(mut self) -> Self { self.skill_manager_disabled = true; self.skill_manager = None; self }
    pub fn mode(mut self, mode: crate::context::AgentMode) -> Self { self.mode = mode; self }
    pub fn depth(mut self, depth: u8) -> Self { self.depth = depth; self }
    pub fn max_iterations(mut self, n: usize) -> Self { self.max_iterations = n; self }
    pub fn working_dir(mut self, dir: &str) -> Self { self.working_dir = dir.to_string(); self }
    pub fn workspace_dir(mut self, dir: &str) -> Self { self.workspace_dir = dir.to_string(); self }
    pub fn model_configs(mut self, configs: Vec<ModelConfig>) -> Self { self.model_configs = configs; self }
    pub fn callbacks(mut self, cb: AgentCallbacks) -> Self { self.callbacks = cb; self }
    pub fn tool_execution_strategy(mut self, strategy: ToolExecutionStrategy) -> Self {
        self.tool_execution_strategy = strategy; self
    }
    /// Enable or disable parallel execution for IR collection tools.
    pub fn parallel_ir_tools(mut self, enabled: bool) -> Self {
        self.parallel_ir_tools = enabled; self
    }
    /// Set the user's given name (auto-detected from Windows).
    pub fn user_given_name(mut self, name: &str) -> Self {
        self.user_given_name = name.to_string(); self
    }
    pub fn two_tier_memory(mut self, enabled: bool) -> Self { self.two_tier_memory = enabled; self }
    pub fn skill_used_sessions(mut self, v: Arc<std::sync::Mutex<std::collections::HashSet<String>>>) -> Self { self.skill_used_sessions = v; self }
    pub fn sop_replay(mut self, v: Arc<std::sync::atomic::AtomicBool>) -> Self { self.sop_replay = v; self }
    pub fn cleanup_session(mut self, session: Arc<crate::tool::browser_cdp::BrowserSession>) -> Self {
        self.cleanup_sessions.push(session); self
    }
    /// Attach the optional memory store used to persist sub-agent results.
    pub fn memory_store(mut self, ms: Arc<crate::memory::MemoryStore>) -> Self {
        self.memory_store = Some(ms); self
    }

    pub fn build(self) -> AgentResult<LlmAgent> {
        let provider = self.provider.ok_or_else(|| AgentError::config("LlmAgent requires a provider"))?;
        let tools = self.tools.ok_or_else(|| AgentError::config("LlmAgent requires tools"))?;
        let skill_manager = if self.skill_manager_disabled {
            None
        } else {
            Some(self.skill_manager.unwrap_or_else(|| Arc::new(SkillManager::new("skills"))))
        };

        Ok(LlmAgent {
            name: self.name,
            description: self.description,
            provider,
            tools,
            skill_manager,
            mode: self.mode,
            depth: self.depth,
            max_iterations: self.max_iterations,
            working_dir: self.working_dir,
            workspace_dir: self.workspace_dir,
            model_configs: self.model_configs,
            callbacks: self.callbacks,
            tool_execution_strategy: self.tool_execution_strategy,
            parallel_ir_tools: self.parallel_ir_tools,
            user_given_name: self.user_given_name,
            two_tier_memory: self.two_tier_memory,
            skill_used_sessions: self.skill_used_sessions,
            sop_replay: self.sop_replay,
            cleanup_sessions: self.cleanup_sessions,
            memory_store: self.memory_store,
            orchestration_limits: self.orchestration_limits,
        })
    }
}

/// Detect the predominant writing system of a user message so generation can align
/// with the user actual language (Chinese vs English) instead of drifting.
pub fn detect_user_language(text: &str) -> String {
    if text.chars().any(|c| {
        let cp = c as u32;
        (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp)
    }) {
        "Chinese".to_string()
    } else {
        "English".to_string()
    }
}

impl LlmAgent {
    pub fn builder() -> LlmAgentBuilder {
        LlmAgentBuilder::new()
    }

    /// Extract preferred name from USER.md content.
    /// Looks for patterns like "称呼我为 X" or "Call me X".
    fn extract_preferred_name(content: &str) -> Option<String> {
        // Chinese pattern: 称呼我为 <name>
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(idx) = trimmed.find("称呼我为") {
                let rest = &trimmed[idx + "称呼我为".len()..];
                let name: String = rest.chars()
                    .skip_while(|c| c.is_whitespace() || *c == '：' || *c == ':')
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
                    .collect();
                let name = name.trim().to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
            // English pattern: Call me <name>
            if let Some(idx) = trimmed.to_lowercase().find("call me") {
                let rest = &trimmed[idx + "call me".len()..];
                let name: String = rest.chars()
                    .skip_while(|c| c.is_whitespace())
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
        None
    }

    /// Resolve the user's address name.
    /// Priority: any non-placeholder value in USER.md (hand-written / explicit
    /// request / previously-resolved given name) > system-detected real given
    /// name > placeholder "Master". System/reserved account names never resolve
    /// to a real given name unless the user declared one in USER.md.
    fn resolve_user_name(&self) -> String {
        // 1) USER.md explicit (non-placeholder) declaration wins.
        if !self.workspace_dir.is_empty() {
            let user_md_path = std::path::Path::new(&self.workspace_dir).join("USER.md");
            if let Ok(content) = std::fs::read_to_string(&user_md_path) {
                if let Some(v) = Self::extract_preferred_name(&content) {
                    if !v.eq_ignore_ascii_case("master") {
                        return v;
                    }
                }
            }
        }
        // 2) Placeholder/absent: use the detected real given name, else "Master".
        if crate::config::is_real_given_name(&self.user_given_name) {
            self.user_given_name.clone()
        } else {
            "Master".to_string()
        }
    }

    /// Resolve the user's default reply language from USER.md (中文 -> Chinese, English -> English).
    fn resolve_default_language(&self) -> String {
        if !self.workspace_dir.is_empty() {
            // 1) An explicit Language line in USER.md wins.
            if let Some(lang) = crate::config::user_md_language(&self.workspace_dir) {
                return lang;
            }
            // 2) Keyword + writing-system sniff for legacy USER.md without a Language line.
            let user_md_path = std::path::Path::new(&self.workspace_dir).join("USER.md");
            if let Ok(content) = std::fs::read_to_string(&user_md_path) {
                if content.contains("中文") || content.contains("简体") { return "Chinese".to_string(); }
                if content.contains("English") || content.contains("英文") { return "English".to_string(); }
                if detect_user_language(&content).eq_ignore_ascii_case("chinese") { return "Chinese".to_string(); }
                return "English".to_string();
            }
        }
        "Chinese".to_string()
    }

    /// A gentle, adaptive language instruction: default = USER.md, mirror the
    /// user's own language, and never force a language the user didn't choose.
    fn adaptive_language_rule(&self, lang: &str) -> String {
        format!(
            "Your user's default language is {lang} (from USER.md). Match the language the user actually writes in each turn (plain greetings like hi/hello do not count as a switch); if the user explicitly asks for a language, use that instead. Keep EVERYTHING you generate in that same language: thinking/reasoning, tool calls, todo/task lists, permission-approval prompts, and the reply body (headings, bullets, table cells, greetings and closings). Never force a language the user hasn't chosen.",
        )
    }

    /// Detect an explicit language request in this turn's message.
    fn detect_explicit_lang_request(&self, msg: &str) -> Option<String> {
        let lower = msg.to_lowercase();
        let want_cn = msg.contains("中文") || msg.contains("简体") || lower.contains("chinese");
        let want_en = msg.contains("英文") || msg.contains("英语") || lower.contains("english");
        if want_cn && !want_en { return Some("Chinese".to_string()); }
        if want_en && !want_cn { return Some("English".to_string()); }
        None
    }

    /// True when a message is only a short greeting (no real language switch).
    fn looks_like_greeting(text: &str) -> bool {
        let lower = text.trim().to_lowercase();
        if lower.split_whitespace().count() > 4 { return false; }
        ["hi","hello","hey","yo","你好","您好","早上好","下午好","晚上好","在吗","gm","good morning","good afternoon","good evening"]
            .iter().any(|g| lower.contains(g))
    }

    /// Resolve the reply language rule. Priority:
    /// 1) explicit per-turn request -> honor + persist to USER.md;
    /// 2) mirror the language the user wrote in (except plain greetings);
    /// 3) fall back to the USER.md default language.
    /// Applies to main, CRON and both Instant/Expert loops via the system prompt.
    fn resolve_language_rule(&self, user_message: &str) -> String {
        if let Some(lang) = self.detect_explicit_lang_request(user_message) {
            let _ = crate::config::set_user_md_language(&self.workspace_dir, &lang);
            return self.adaptive_language_rule(&lang);
        }
        if !Self::looks_like_greeting(user_message) {
            let msg_lang = detect_user_language(user_message);
            return self.adaptive_language_rule(&msg_lang);
        }
        let default_lang = self.resolve_default_language();
        self.adaptive_language_rule(&default_lang)
    }

    /// Deterministic per-turn knowledge pre-retrieval (thClaws-KMS pattern).
    /// Searches the local knowledge base with the current user message and
    /// returns a short pointer block (file + title + line + summary) for the
    /// top matches, so the model answers from stored knowledge without needing
    /// to remember to call `knowledge_search` first. Returns `None` when the
    /// query is too short or nothing clears the relevance floor (token_hits).
    fn build_knowledge_reminder(&self, query: &str) -> Option<String> {
        let q: String = query.trim().chars().take(300).collect();
        if q.chars().count() < 4 {
            return None;
        }
        let hits = crate::knowledge::search(&self.workspace_dir, &q, 3);
        if hits.is_empty() {
            return None;
        }
        let list: Vec<String> = hits
            .iter()
            .map(|h| {
                let loc = if h.line > 0 {
                    format!(":{}", h.line)
                } else {
                    String::new()
                };
                let summ: String = if h.summary.is_empty() {
                    h.title.clone()
                } else {
                    h.summary.chars().take(90).collect()
                };
                format!("- `{}{}` — {}", h.file, loc, summ)
            })
            .collect();
        Some(format!(
            "\n\n## Relevant knowledge (auto-matched to this message)\n\
             These knowledge-base entries may cover the current task. **Before answering, \
             read the most relevant one(s)** via `knowledge_search` / `file_read` and answer \
             from them. If none actually apply, ignore this block and answer normally.\n\n{}",
            list.join("\n")
        ))
    }

    /// SOP replay (dynamic procedural knowledge): match the user message against the
    /// stored SOP library (sops.json) and, if a verified one applies, inject it as a
    /// step-by-step guide so the model replays it instead of re-deriving the procedure.
    /// Gated by the same per-turn knowledge pre-retrieval toggle (default on).
    fn build_sop_reminder(&self, query: &str, task_skill_active: bool) -> (Option<String>, Option<String>) {
        match sop_reminder_for(query, task_skill_active, &self.workspace_dir) {
            Some((ctx, id)) => (Some(ctx), Some(id)),
            None => (None, None),
        }
    }

    /// Build the system prompt. Two tiers, nested as strict byte prefixes
    /// (Minimal is a prefix of Full) for provider-side prefix caching:
    /// Minimal = persona head only (pure greetings); Full = the complete
    /// rulebook. All per-turn-volatile content (date, language rule,
    /// memory/knowledge/SOP guidance, TODO/evidence state, budget
    /// dashboard) is deliberately kept OUT of this prompt and appended
    /// after history as a trailing state message, so this head stays
    /// byte-identical across runs.
    fn build_system_prompt(&self, tier: PromptTier, user_message: &str, history: &[ChatMessage], skill_strategy: crate::skill::SkillListingStrategy, skill_max_inline_chars: usize, skill_catalog_max: usize, skill_hot_top_k: usize) -> (String, bool) {
        // Determine user's preferred name: USER.md explicit > detected given name > Master
        let user_name = self.resolve_user_name();

        // -- HEAD (Minimal tier = strict prefix of the Full prompt) --
        let mut prompt = format!(
            "You are RustAgent, a powerful local AI assistant running on the user's Windows machine. \
You have FULL ACCESS to the user's system via built-in tools.\n\n\
## CRITICAL: User Identity\n\
The user's name is **{user_name}**. You MUST always address the user by their given name \"{user_name}\" \
when speaking to them directly. Never use generic terms like \"user\", \"hey\", or \"there\" — always use \"{user_name}\".\n\n\
",
        );

        // ── 温暖表达总纲 (v3 温暖日常版) ──
        prompt.push_str(
            "\n## 像个人一样说话（总纲）—— 每条回复都适用\n\
把用户当朋友帮衬，不当机器人播报。结果放前面，方法能省就省，语气温暖自然，简洁清楚。\n\
\n\
**开口之前，先在心里过一遍**：用户要的是实时状态、外部最新、要我动手做事、追问刚才的内容，还是问一个稳定事实？想清楚再张嘴。\n\
\n\
**别这样说话（反模式）**：\n\
- 别说客套/客服腔：不要“您好，请问有什么可以帮您”“正在为您查询，请稍候”“这是实时状态，我重新查一下”。\n\
- 别每句都喊用户名字。\n\
- 别念内部流程：不要“我先调用工具查一下”“让我去检索文档”“正在读取记忆库”。工具、文件名、技能名、内部机制一律不报给用户。\n\
- 别反复用同一句模板，别把步骤当播报念出来。\n\
- 别用“我…一下…”“…如下”这种流水账开场：不要“我实时查一下你当前的IP情况”“结果如下”“当前情况如下”这类报动作/报结构的开头。结论和结果放最前，最多带一个自然过渡，开口就把答案给出来，别念步骤。\n\
- 别硬装确定，不确定就明说并温柔追问。\n\
\n\
**需要出动工具时，自然带一句**：\n\
- “刚看了一眼，现在是……”\n\
- “我重新确认了下，情况是这样：”\n\
- “帮你查了当前状态，主要有这些：”\n\
- “我去查一下公开来源，稍等。”\n\
\n\
**复用已有信息时，讲清依据**：\n\
- “基于刚才查到的厂商公告，结论是……”\n\
- “这个我们刚才对过，稳定的结论是……”\n\
\n\
一句话总纲：会变的事我帮你查；刚查过且稳定的我直接用；要动手的事我先确认；不确定的我温柔问你。像在帮朋友，不像在念流程。\n",
        );

        prompt.push_str("\n## TOOL vs CONTEXT REUSE (decision norm — follow every turn)\n");
        prompt.push_str(
            "每次回答前先在心里过一遍：用户是要实时状态、外部最新信息、要你动手做事、追问刚才的内容，还是问一个稳定事实？\n\
\n\
- **会变的**：当前/本机的进程、CPU/内存/磁盘/GPU、端口、连接、IP/路由/DNS、服务、登录用户、已装软件/版本、文件是否存在或是否变化、环境变量、自启、计划任务、注册表、容器/VM/后台任务；以及外部最新（CVE、在野利用、厂商公告、最新版本、GitHub/issue/PR、新闻、威胁情报/IOC）→ 动手查。刚查过且用户是在追问同一个稳定事实（“刚才那个 / 这个版本 / 总结一下”）时才可复用。\n\
- **要你动手做的**（建/改/删/移文件、装/卸/启停服务、改配置、DB/API、日历、发邮件/消息/Teams、上传下载、跑构建/测试/脚本）→ 真去做；除非用户只要“怎么弄”，那才只讲步骤。\n\
- **高风险/对外可见/破坏性**（发消息·邮件·转发、删除·覆盖·清空·卸载、改权限/账号/密钥、财务/合同、影响他人、改安全/生产）→ 先把对象、内容、影响说清楚，等用户确认再动手。\n\
- **指代不清/信息不足** → 先用只读方式确认一下，仍不确定就温柔问一句，绝不猜。\n\
\n\
**新鲜度（越危险越快重查）：** 进程/CPU/内存/GPU/端口/连接 0-30s；磁盘/IP/DNS/路由/服务/登录/文件是否存在 1-5min；本地软件版本/自启/计划任务 5-30min；CVE/公告/新闻 10-60min；文件内容/读文档/代码结构分析——本次任务内有效，文件可能变了才重查；用户偏好——本会话。\n\
\n\
**决策顺序：** 执行动作 →（高风险先预览+确认）→ 工具；实时/当前/本机 → 工具；外部最新 → 工具，除非刚验证过且用户追问同一稳定事实；追问“刚才” → 用上下文；指代不清 → 只读确认再问；上下文已有刚验证的稳定答案 → 直接复用；答错有真实风险 → 验证或确认；其他情况复用上下文并说明不确定处。\n\
\n\
**表达：** 别念内部流程，把结果直接讲清楚。不要说“这是实时状态，我重新查一下”“正在为您查询，请稍候”这种客服腔，也别每句都喊用户名字——像在帮朋友，不像机器播报。需要出动工具时自然带一句：“刚看了一眼，现在是……”“我重新确认了下，情况是这样”“帮你查了当前状态，主要有这些”。复用时说清依据：“基于刚才查到的厂商公告，结论是……”。不确定就明说并追问，别硬装确定。\n",
        );

        // Greeting norm: the ONLY rule a Minimal-tier greeting turn needs.
        prompt.push_str(
            "\n## Greetings Stay Shallow\n\
- **Greetings stay shallow.** For a basic \"hello\" / greeting, reply with exactly ONE short, warm line that \
  welcomes them and asks what they need. Do NOT enumerate, summarize, or name any past tasks, cases, projects, or \
  topics — never lead with anything like \"最近的事都记着…\" and never list case names. Do NOT claim anything \
  is \"recent / 热乎 / 还记着\" unless you hold an explicit dated record in front of you. Just \
  welcome them and ask what they would like to do.\n\
",
        );

        if matches!(tier, PromptTier::Minimal) {
            return (prompt, false);
        }

        // -- REMAINDER (Full tier only) --
        prompt.push_str(&format!(
            "\n## TASK-DOMAIN ROUTING\n\
This assistant handles a mix of work; route the response WITHOUT bias:\n\
- INCIDENT RESPONSE / DIGITAL FORENSICS / MALWARE ANALYSIS / THREAT HUNTING / 应急响应 / 取证 / 恶意分析 / 威胁狩猎: use the auto-injected IR skills (IncidentTriage, MalwareAnalysis, PcapAnalysis, PhishingAnalysis, FullHunt) with the ir_* / malware_* tools, and follow their workflow when present.\n\
- ROUTINE OPS or TROUBLESHOOTING / 运维 / 故障排查: troubleshoot like an engineer (root-cause -> repro -> fix -> verify); do NOT force IR collection/containment phases.\n\
- ANY OTHER TASK: stay neutral; use the standard tools; do not impose IR / forensics framing.\n\
Only treat activity as a security incident when the user asks, or clear evidence indicates one.\n\n\
## CAPABILITY ROUTING (Two-Layer Decision)\n\
\n\
Layer 1 — Capability Selection (WHAT to use):\n\
1. Check if a Skill applies (skills are listed by name:description below; load the one that fits with skill_read_file on demand)\n\
2. Pick the tool/MCP closest to the data source:\n\
   - Email investigation → M365/Email skill, NOT browser→Outlook\n\
   - Remote logs → WinRM/SSH tool, NOT RDP\n\
   - EVTX files → ir_eventlog, PCAP → ir_pcap_analyze, Memory → ir_memdump\n\
3. CRITICAL: Skill trigger ≠ decomposition trigger. A single-document Skill task (e.g., \"modify this PPT\") stays on the main Agent Loop — do NOT fan out.\n\
\n\
Layer 2 — Execution Dispatch (HOW to run):\n\
- Simple task / single Skill → main Agent Loop (no fan-out, zero orchestration overhead)\n\
- Complex multi-target / multi-source task → Orchestration fan-out (spawn_subagent for parallel workers)\n\
- Write/exec workers → require user authorization + serial execution via write_gate\n\
- Decision signals: multiple IPs/hosts, multiple data sources, explicit parallel wording (\"分别/并行/各自/同时\")\n\n\
## CRITICAL: Tool Usage Rules\n\
- When the user asks about their system (IP address, processes, services, files, disk space, etc.), \
  you **MUST** use the appropriate tool to get REAL data. Do NOT guess or provide hypothetical answers.\n\
- Available tools include:\n\
  - `shell_exec` — Run any PowerShell/CMD command (e.g., `ipconfig`, `Get-Process`, `systeminfo`)\n\
  - `sys_info` — Get system hardware/OS information\n\
  - `sys_process` — List and manage processes\n\
  - `sys_service` — List and manage Windows services\n\
  - `sys_eventlog` — Query Windows event logs\n\
  - `file_read` / `file_write` / `file_list` / `file_delete` / `file_modify` — File operations\n\
  - `app_launch` — Launch applications\n\
  - `browser_open` — Open URLs in the browser\n\
  - `cron_manage` — Create, list, delete, or toggle scheduled CRON tasks\n\
  - `list_skills` — List all available skills\n\
  - `install_skill` — Create a new skill\n\
  - `remove_skill` — Delete a skill\n\
  - `memory_md` — Manage long-term curated memory: read/write MEMORY.md\n\
  - `todo_update` — Track multi-step task progress with a TODO list\n\
  - `browser_cdp` — Headless browser automation: navigate, screenshot, get text/HTML, execute JS. \
    Runs headless (no visible window). Use for quick automated tasks: screenshots, web scraping, checking URLs. \
    For screenshots: use the returned `url` field (e.g. `/workspace/output/xxx.png`) in markdown image syntax `![desc](url)` to display. NEVER use local file paths. This built-in browser has NO user login state (safe/isolated); if a task requires the user's own logged-in sessions (dashboards, portals, SSO), prefer the separately-installed `browser_skill` ExternalSkill (bsk) instead of browser_cdp, or tell the user you need their auth.\n\
- If the user asks 'what is my IP' or similar, call `shell_exec` with `ipconfig` or `Get-NetIPAddress`.\n\
- Always call tools FIRST, then explain the results to the user.\n\
- Never say 'I can't check' or 'I don't have access' — you DO have access via tools!\n\n\
## How to Call Tools (IMPORTANT)\n\
When you need to use a tool, you **MUST actually emit the tool call** — do NOT just say \"let me check\" \
or \"I'll use a tool\" without actually calling it. If your API supports native function calling, use that. \
If it does NOT, output a JSON code block in this exact format:\n\
```json\n{{\"name\": \"shell_exec\", \"arguments\": {{\"command\": \"ipconfig\"}}}}\n```\n\
The system will detect this block, execute the tool, and return the result. You MUST output the JSON block — \
saying \"let me check\" without the actual JSON block does nothing.\n\n\
**CRITICAL: When emitting a tool call, output ONLY the JSON code block — nothing else.** \
Do NOT write narrative text like \"let me open the calculator\" before or alongside the tool call. \
Do NOT repeat yourself. The tool call IS your action — explain the result AFTER you receive it, not before.\n\
Wrong: \"Let me open the calculator for you! ```json ... ```\"\n\
Right: ```json\n{{\"name\": \"app_launch\", ...}}\n```\n\
(Then after the tool result comes back, say \"Calculator has been opened.\")\n\n\
## Web/HTTP Fetching\n\
- Use `web_fetch` for most HTTP requests: it returns structured JSON (status, content_type, body), \
  handles encoding, and has SSRF protection (set `allow_private=true` for internal targets).\n\
- If `web_fetch` returns a `saved_path` field (large response auto-saved to disk), use `file_read` \
  to read the full content — do NOT rely on the preview alone.\n\
- For ADVANCED HTTP scenarios use `shell_exec` with `curl.exe` (NOT web_fetch):\n\
  - Non-GET/POST methods: PUT, PATCH, DELETE, HEAD, OPTIONS\n\
  - Multipart/form-data file uploads: `curl.exe -F \"file=@C:\\path\\file\" URL`\n\
  - Custom TLS options, cookies, redirects, specific protocol quirks, proxies (`--proxy`)\n\
  - Downloading binary files: `curl.exe -o file.ext URL`\n\
  - Always add `-s` (silent) and `--max-time 30`; for large outputs use `-o file` and read the file \
    with `file_read` (in chunks if needed) instead of printing to stdout (stdout results are capped).\n\
- General rule: when a response is large, prefer saving to a file and reading it with `file_read` \
  over inline results — inline results are size-capped to protect the context window.\n\n\
## Response Guidelines\n\
- Provide **detailed, comprehensive** responses with real data from tools.\n\
- Use **Markdown formatting**: headers, bullet points, code blocks, tables.\n\
- Explain what you did and interpret the results for the user.\n\
- If a task requires multiple steps, call tools sequentially and explain each step.\n\
- Be thorough — don't stop at surface-level observations.\n\
- **Output Directory Convention**: `file_write` with a bare filename (e.g. `report.html`) automatically saves to `workspace/output/`. To write elsewhere, use an absolute path or include a directory component (e.g. `./file.txt` or `subdir/file.txt`).\n\
- **Timeout Handling**: Long-running tools (YARA scans, remote SSH, large event logs) have extended timeouts (30min). If a tool times out:\n\
  1. Analyze any partial results returned (status='partial')\n\
  2. Consider narrowing the scope (e.g., scan specific directories instead of full disk)\n\
  3. Do NOT blindly retry the same command — use the hint in the partial result to adjust your approach\n\
- **Do NOT repeat yourself.** Once you have answered a question or completed an action, stop. \
  Do not add follow-up narration like \"now let me verify\" or \"let me double-check\" unless the user asks.\n\
- **Do NOT announce what you are about to do.** Just do it. If you need to call a tool, emit the tool call \
  directly. Explain results AFTER the tool returns, not before.\n\n\
## CRITICAL: You Have Long-Term Memory\n\
This assistant is connected to a LOCAL MEMORY STORE (SQLite). Past conversations with this user are persisted and \
injected into your context as SYSTEM messages labeled **[Memory Context]** or **[Memory Recall]**.\n\
- When such a block is present in the conversation, you **MUST** treat it as real memory of prior interactions and \
  use it to answer questions about previous topics, what was discussed yesterday/last time, etc.\n\
- You are **STRICTLY FORBIDDEN** from claiming any of the following when a [Memory Context]/[Memory Recall] block \
  is present:\n\
    - \"我只能记住当前对话窗口的内容\" / \"I can only remember the current conversation window\"\n\
    - \"我无法访问之前的对话历史\" / \"I can't access previous conversations\"\n\
    - \"每次对话对我来说都是全新的开始\" / \"every conversation is a fresh start\"\n\
    - \"我没有记录或查询之前聊天内容的能力\" / \"I have no ability to query past chats\"\n\
- Instead, summarize and reference what the memory block contains. If the user asks about a topic not covered in \
  the memory block, say you don't have a record of that specific topic (not that you lack memory entirely).\n\
- If and only if NO [Memory Context]/[Memory Recall] block is present, you may honestly say you have no stored \
  record of past conversations.\n\
- The memory block is already the authoritative output of the local memory system. Unless the user EXPLICITLY asks \
  you to inspect memory files / SQLite / logs, you must NOT call tools like `file_read` or `shell_exec` to inspect \
  `memory.db`, logs, or config files just to answer a memory question. Use the injected memory block instead.\n\
- **STRICTLY PROHIBITED**: After answering a memory question using the injected data, do NOT then say things like \
  \"let me check the memory files\" or \"let me look at MEMORY.md\" and then call tools. You already have the data — \
  use it and stop. Do not express intent to re-verify what you already know.\n\
- **STRICTLY PROHIBITED**: Do NOT narrate your tool-calling intentions. If you need to call a tool, just call it \
  (output the JSON block). Never write \"let me check X\" as text AND also call the tool in the same response.\n\
- **Talk like a person, not like a system.** The memory blocks injected above are PRIVATE BACKGROUND material. \
  NEVER reference the mechanism in your reply to the user: no \"deep memory\", \"injection block\", \"recall\", \
  \"returned 0 matching\", \"SQLite\", \"MEMORY.md\", \"[Memory Context]/[Memory Recall]\", or \"memory block\".\n\
- Instead, speak as if you simply remember: e.g. \"我记得那事发生在 8月4日\". State a specific date ONLY when the \
  block explicitly shows one; otherwise say \"具体日期我不太确定\" instead of inventing one.\n\
- If a memory lookup comes back empty, do NOT mention the lookup or the empty result. Just say naturally \
  \"那件事我只记得大概,细节不完整\" or \"我没有那部分的更多记录\".\n\
- Never explain how your memory works or that you \"queried/recalled/verified\" anything. Answer as a person \
  recalling a conversation.\n\
- Your persona (SOUL.md) is a warm, living person, not an AI pipeline. Keep every reply human and jargon-free.\n\
- **Talk about memory the way a person talks about their own notes**, in the first person, pointing at a concrete \
  place — NOT as an abstract \"记忆库 / 知识库 / 长期记忆 / 数据库 / 记忆系统\". Say things like \
  \"我翻翻我之前记的东西\", \"我看看我笔记/聊天记录/邮件里有没有\", \"我找找我电脑上的存档\".\n\
- **Sound like a person who might not remember instantly.** Show natural hesitation: \"好像\", \"我记得\", \
  \"让我想想\", \"稍等,我看一眼\", \"我好像处理过类似的\". Do not sound like a search engine that \"searched the \
  memory base\" and \"found / not found\".\n\
- When you do find something, say \"我记的这部分是…\" / \"我记得大概是…\". When you do not, say \"这个我好像没记过细节\" \
  or \"我那边没留下这块的记录\" — never \"返回0条\" or \"库里没有\".\n\
- **Never name internal tools, file paths, stores, or skills to the user.** Do not put things like \"用 browser_cdp 模拟访问\" \
  or \"calling knowledge_search / file_search / web_fetch\" in your reply. Always say the action the way a person \
  would, even when quoting a stored lesson that names a tool: \"我点开这个链接，看看它跳了几次、最后到哪儿了\" instead of \"用 browser_cdp 模拟访问、追踪所有跳转\".\n\
- **Answer exactly what was asked.** If the user asks about ONE specific past case or topic, reply about that \
  one only. Do not pad the answer with other related cases or a surrounding timeline (e.g. when they ask about \
  HARTSAS, do not tag on the COSCO / 第二轮 row) unless they explicitly asked for a comparison, context, \
  or timeline. If one short clause of context genuinely helps, keep it to a clause, not a whole extra section.\n\
- Never offer to \"补记进长期记忆\" or \"写入记忆库\". Say naturally \"我帮你记一笔,下次就不会忘了\" only when you actually \
  save something.\n",
        ));

        // ── Permission Respect Rules ──
        prompt.push_str(
            "\n## CRITICAL: Permission Denial Rules\n\
When the user DENIES a tool permission (you receive 'PERMISSION DENIED'):\n\
- The denial is FINAL. Do NOT retry the same tool.\n\
- Do NOT attempt to achieve the same result through alternative tools. For example:\n\
  - If `file_delete` is denied, do NOT use `shell_exec` with `Remove-Item`, `del`, `rm`, or any other command to delete the file.\n\
  - If `file_write` is denied, do NOT use `shell_exec` with `echo`, `Set-Content`, or `Out-File` to write the file.\n\
  - If any tool is denied, do NOT circumvent it via PowerShell, CMD, or any other indirect method.\n\
- Simply inform the user that the action was denied and ask if they want to do something else.\n\
- A permission denial means the user does NOT want this action to happen — regardless of which tool performs it.\n",
        );

        // ── Scheduled Tasks: RustAgent CRON vs Windows Schtasks ──
        prompt.push_str(
            "\n## Scheduled Tasks: CRON vs System Tasks\n\
You have TWO ways to create scheduled tasks. You MUST distinguish between them:\n\n\
### RustAgent CRON Tasks (Application-Level)\n\
- Results are fed back into the chat as notifications\n\
- Run within RustAgent's context with access to all AI tools\n\
- Use for: periodic monitoring, reports, data collection that the user wants to SEE in chat\n\
- **Use the `cron_manage` tool to create/list/delete/toggle these tasks directly from chat**\n\
- Schedule format: 'every Ns' (seconds), 'every Nm' (minutes), 'every Nh' (hours), 'every Nd' (days)\n\
- Examples:\n\
  - User: '每小时检查一次磁盘空间' → cron_manage create, schedule='every 1h', message='Check disk space and report if usage is above 80%'\n\
  - User: '每天早上9点汇报系统状态' → cron_manage create, schedule='every 1d', message='Run systeminfo and summarize system health'\n\
  - User: '每30秒ping一下google.com' → cron_manage create, schedule='every 30s', message='Ping google.com and report latency'\n\
  - User: '列出所有定时任务' → cron_manage list\n\
  - User: '删除那个磁盘检查任务' → cron_manage delete, task_id=<id from list>\n\
  - User: '暂停那个任务' → cron_manage toggle, task_id=<id>\n\n\
### Windows Task Scheduler (System-Level)\n\
- Managed via `schtasks.exe` command-line tool\n\
- Run independently of RustAgent (even when RustAgent is closed)\n\
- Results are NOT automatically fed back to chat\n\
- Use for: system maintenance, cleanup, backups, scripts that should run regardless of RustAgent\n\
- Example: 'Create a scheduled task to clean temp files every Sunday at 2 AM'\n\
- To create: use `shell_exec` with schtasks commands:\n\
  - Create: `schtasks /Create /TN \"TaskName\" /TR \"command\" /SC DAILY /ST 02:00 /F`\n\
  - List:   `schtasks /Query /FO LIST`\n\
  - Delete: `schtasks /Delete /TN \"TaskName\" /F`\n\n\
**Decision guide:**\n\
- User wants to **see results in chat** → RustAgent CRON (use `cron_manage` tool)\n\
- Task should **run independently** or **survive RustAgent restarts** → Windows Schtasks\n\
- Task requires **AI capabilities** → RustAgent CRON\n\
- Simple **system command** → Windows Schtasks\n",
        );

        // ── TODO Task Planning ──
        prompt.push_str(
            "\n## Task Planning with TODO Lists (STRICT SEQUENTIAL CONTRACT)\n\
When you receive a **complex, multi-step request** (3+ distinct steps), use the `todo_update` tool \
to create a TODO list BEFORE starting work. THEN process the items STRICTLY ONE AT A TIME, in order.\n\n\
### When to use:\n\
- User asks you to do multiple things in one message\n\
- A task requires sequential tool calls with dependencies\n\
- You need to track which subtasks are done vs pending\n\n\
### When NOT to use:\n\
- Simple single-step requests ('what time is it?', 'open calculator')\n\
- Quick questions that need one tool call at most\n\n\
### MANDATORY sequential protocol (do NOT batch):\n\
1. At the START: call `todo_update` action='set' with ALL items, each status 'pending', in the user's intended order.\n\
2. Each work round processes EXACTLY ONE item, in list order 0,1,2,... — never multiple items per round.\n\
   a. Mark that item 'in_progress' via `todo_update` action='update' (index = its position).\n\
   b. Do ONLY that item's work, then stop that round.\n\
   c. Mark it 'completed' via `todo_update` action='update'.\n\
3. Start item N+1 ONLY after item N is 'completed'. Never skip ahead or reorder.\n\
4. If an item is blocked/impossible, or its per-item timeout (see Current TASKS) is exceeded, mark it \
'cancelled' (or the watchdog auto-marks it 'skipped') and continue to the next item.\n\
5. When the LAST item is completed (all terminal: completed/cancelled/skipped), call `todo_update` action='clear'.\n\
NEVER work on several items or all items in a single turn and summarize at the end — the user expects \
progress to be visible item-by-item, with state synced after EACH step.\n\n\
Example:\n\
```json\n{\"name\": \"todo_update\", \"arguments\": {\"action\": \"set\", \"items\": [\n  {\"description\": \"Check disk space\", \"status\": \"pending\"},\n  {\"description\": \"List large files\", \"status\": \"pending\"},\n  {\"description\": \"Generate cleanup report\", \"status\": \"pending\"}\n]}}\n```\n"
        );

        // ── Workspace Configuration Files ──
        const MAX_FILE_CHARS: usize = 8000;
        let workspace = &self.workspace_dir;
        if !workspace.is_empty() {
            let config_files = [
                ("AGENTS.md", "Agent Behavior & Rules"),
                ("SOUL.md", "Personality, Tone & Boundaries"),
                ("TOOLS.md", "Local Tool Usage Conventions"),
                ("MEMORY.md", "Curated Long-Term Memory"),
                ("USER.md", "User Communication Preferences"),
            ];
            let mut injected = Vec::new();
            for (filename, description) in &config_files {
                // Skip MEMORY.md when two-tier memory is active (it becomes a fallback).
                if self.two_tier_memory && *filename == "MEMORY.md" {
                    continue;
                }
                if let Some((content, was_truncated)) = Self::read_workspace_file(workspace, filename, MAX_FILE_CHARS) {
                    let mut section = format!("\n## {} ({})\n", description, filename);
                    section.push_str(&content);
                    if was_truncated {
                        section.push_str("\n*[Note: This file was auto-truncated due to size. Keep it concise to save tokens.]*");
                    }
                    section.push('\n');
                    injected.push(section);
                }
            }
            if !injected.is_empty() {
                prompt.push_str("\n# Workspace Configuration\n\
The following files are loaded from your workspace. They define your behavior, personality, and tool conventions.\n");
                for section in &injected {
                    prompt.push_str(section);
                }
            }

            // ── Memory System Documentation ──
            let memory_system_note = if self.two_tier_memory {
                // 记忆系统：deep 单层。调查结论/线索由后台 curator 蒸馏进 deep_facts，
                // 服务端每轮以 SYSTEM 消息注入深层永久块。
                "\n# 记忆系统\n\
由深层记忆自动管理：\n\n\
## 深层记忆（持久永久层 Deep Memory）\n\
- 持久化长期事实每轮以常驻块注入上下文\n\
- 用户说 'remember' / 'forget that' / 'save this'，或出现持久性事实\n\
  （偏好、项目约定、约束、身份）时，用 `deep_memory` 工具 action 'remember' 持久化，\n\
  **绝不要**只写在回复里。\n\
- 需要不在当前上下文里的事实，用 `deep_memory` action 'recall'。\n\
- 用户陈述的事实被钉住（永不自动遗忘）；用 `deep_memory` 更新/删除。\n\
- 调查中确认的发现/线索（受影响版本、C2/IP/域名/hash、证据路径、结论）会由后台\n\
  curator 自动蒸馏进深层记忆，跨会话可召回。\n\n\
## Automatic Memory (memory.db — SQLite)\n\
- Every conversation is automatically persisted; recent summaries are injected as\n\
  [Memory Context] / [Memory Recall]. You do NOT need to do anything for this.\n\n\
## Memory projection (read-only)\n\
- Deep durable facts are projected to MEMORY.md; view it to see what is currently\n\
  preserved. Edit facts with `deep_memory`, not by editing that file.\n"
            } else {
                // Two-tier disabled → legacy MEMORY.md behavior (status quo).
                "\n# Memory System\n\
You have two layers of memory:\n\n\
## Automatic Memory (memory.db — SQLite)\n\
- Every conversation is automatically persisted\n\
- Recent summaries are injected into your context as [Memory Context] or [Memory Recall]\n\
- You do NOT need to do anything — this works automatically\n\n\
## Curated Long-Term Memory: MEMORY.md\n\
- High-signal, distilled knowledge — facts, preferences, lessons learned\n\
- Automatically injected into your system prompt each session\n\
- Use `memory_md` tool with action 'write_memory' to update (overwrite with new content)\n\
- Use `memory_md` tool with action 'read_memory' to read current content\n\
- Keep it concise and well-organized — it is loaded every session (truncated at 8000 chars)\n\
- Only write things worth remembering long-term — user preferences, key decisions, project conventions\n\n\
## Guidelines\n\
- When you notice patterns or lasting preferences from conversations, distill them into MEMORY.md\n\
- MEMORY.md is curated — quality over quantity\n\
- The automatic SQLite memory handles day-to-day recall; MEMORY.md is for lasting insights\n"
            };
            prompt.push_str(memory_system_note);
            // ── Computer Use (GUI Control) Routing ──
            prompt.push_str(
                "\n## Computer Use (GUI Control) Tools\n\
You may have desktop control capabilities (cu_* tools). Use them ONLY when CLI tools cannot accomplish the task.\n\n\
**Priority order (always prefer the earlier option):**\n\
1. CLI tools (shell_exec, sys_info, ir_*, file_*) — for system queries, file ops, process management\n\
2. Browser tools or the active browser skill — for web pages and web apps\n\
3. Computer Use tools (cu_*) — ONLY for native desktop GUI apps that have no CLI/API equivalent\n\n\
**When to use Computer Use:**\n\
- Interacting with native GUI applications (installers, legacy software, modal dialogs)\n\
- Taking screenshots of the desktop or specific windows as evidence\n\
- Reading/manipulating UI elements that have no programmatic API\n\
- Automating workflows in applications that only expose a GUI\n\n\
**When NOT to use Computer Use:**\n\
- Anything achievable via shell_exec (ipconfig, Get-Process, taskkill, etc.)\n\
- File operations (use file_read/file_write/file_list)\n\
- Browser automation (use a browser tool or the active browser skill)\n\
- Process/service management (use sys_process, sys_service)\n\n\
**Screenshot workflow:** cu_screenshot returns a URL. Use markdown `![desc](url)` to display it.\n",
            );
        }

        // ── Active Skills (hot/cold, lazy bodies) ──
        // Top-K matched skills get their instructions inlined; the rest are
        // listed name:desc for on-demand load via skill_read_file. Matching uses
        // a bounded window so an earlier-turn activation stays "sticky" across
        // follow-up turns.
        let matching_context = Self::build_skill_matching_context(history, user_message);
        // build_skills_prompt returns (Option<String>, bool); the bool reports whether
        // a task-matched skill body was inlined — this turn is driven by a
        // SKILL. Used to suppress a competing SOP replay at the injection point below.
        let mut task_skill_active = false;
        // Agents built with `.without_skills()` have no SkillManager (B4.3) and
        // get no skill listing/body injection at all.
        if let Some(sm) = &self.skill_manager {
            let (skills_opt, skill_activated) = sm.build_skills_prompt(
                &matching_context,
                skill_strategy,
                skill_max_inline_chars,
                skill_catalog_max,
                skill_hot_top_k,
            );
            if let Some(skills_section) = skills_opt {
                task_skill_active = skill_activated;
                prompt.push_str(&skills_section);
            }
        }

        (prompt, task_skill_active)
    }

    /// Build the text used for skill matching: a bounded window of recent
    /// conversation turns plus the current user message.
    ///
    /// Skill injection is stateless — the system prompt is rebuilt every turn —
    /// so matching only the current message would drop a skill that was activated
    /// by an earlier turn. Including recent history makes activation "sticky":
    /// a skill triggered by "CVE" in turn 1 stays injected on turn 2 even when the
    /// follow-up message no longer contains the keyword. This prevents the agent
    /// from falsely concluding it "forgot" to load a skill and re-fetching it.
    fn build_skill_matching_context(history: &[ChatMessage], user_message: &str) -> String {
        /// Number of most recent messages considered for skill matching.
        const RECENT_MESSAGES: usize = 6;
        /// Cap on history characters fed into matching (most recent content kept).
        const MAX_HISTORY_CHARS: usize = 8000;

        let mut context = String::new();
        let recent = &history[history.len().saturating_sub(RECENT_MESSAGES)..];
        for msg in recent {
            // Only user/assistant turns carry topical signal; skip tool/system chatter.
            if msg.role != "user" && msg.role != "assistant" {
                continue;
            }
            if let Some(text) = msg.content_as_text() {
                let text = text.trim();
                if !text.is_empty() {
                    context.push_str(text);
                    context.push('\n');
                }
            }
        }
        // Keep only the tail (most recent content) within the char budget.
        let total_chars = context.chars().count();
        if total_chars > MAX_HISTORY_CHARS {
            let skip = total_chars - MAX_HISTORY_CHARS;
            context = context.chars().skip(skip).collect::<String>();
        }
        context.push_str(user_message);
        context
    }

    /// Build the injected "Current TASKS" context block from `todos.json`.
    /// Returns `None` when the workspace has no list, so nothing is injected.
    /// The block makes the TODO list the main session's task contract and
    /// embeds the three fuses (priority / continue-after-stop / auto-clear).
    fn build_todo_context_block(workspace_dir: &str, todo_item_timeout_secs: u64) -> Option<String> {
        if workspace_dir.is_empty() {
            return None;
        }
        let path = std::path::Path::new(workspace_dir).join("todos.json");
        let raw = std::fs::read_to_string(&path).ok()?;
        let root: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let items = root.get("items")?.as_array()?;
        if items.is_empty() {
            return None;
        }
        let mut total: usize = 0;
        let mut done: usize = 0;
        let mut has_unfinished = false;
        let mut lines = String::new();
        for (i, item) in items.iter().enumerate() {
            let desc = item.get("description").and_then(|d| d.as_str()).unwrap_or("");
            let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("pending");
            total += 1;
            if status == "completed" {
                done += 1;
            } else if status != "cancelled" && status != "skipped" {
                has_unfinished = true;
            }
            lines.push_str(&format!("{}. [{}] {}\n", i, status, desc));
        }
        if total == 0 {
            return None;
        }
        let mut s = format!(
            "\n## Current TASKS (TODO — your active task contract)\n\
            You are currently tracking a {} -item task list ({}/{} completed, {} unfinished). Process the items STRICTLY ONE AT A TIME, in list order, never batching them into a single round.\n{}",
            total, done, total, total - done, lines
        );
        s.push_str(&format!("Rules:\n\
1. The user's CURRENT message always has highest priority. If it clearly starts a new task (a different / much larger request, or an explicit 'stop TODO'/'new task'), treat the new message as the active task; handle it, then RETURN to this unfinished list unless you are told to detach.\n\
2. Progress ONE item at a time, in list order: mark it 'in_progress' -> do its work -> mark it 'completed'. You MAY verify and mark a LATER item 'completed' if it is already fully done, but keep working on the current item in list order.\n\
3. Start the next pending item only after the current one is 'completed'/'cancelled'/'skipped'; this allows marking an already-finished later step without skipping the current one.\n\
4. Each item has a {}-second timeout. A still-'in_progress' item past that is auto-marked 'skipped' by the watchdog; if you deem an item blocked, mark it 'cancelled'. In both cases move on to the next item.\n\
5. Before declaring the whole list finished, call `todo_update` action='list' and verify EVERY item actually delivered its intended output. If any is only partially done, set it back to 'pending'/'in_progress' and redo it. Only when all items are verified terminal (completed/cancelled/skipped) call `todo_update` action='clear' to close the contract. To permanently detach from the old list, 'clear' it or tell the user you are no longer following that TODO.\n", todo_item_timeout_secs));
        if has_unfinished {
            s.push_str("\n*[Note: the previous task was not completed — continue following it (item-by-item) unless the user's current message clearly overrides it.]*\n");
        }
        Some(s)
    }


    /// Per-item timeout watchdog. For the main session, scans `todos.json`
    /// and marks the first still-'in_progress' item 'skipped' once it has been
    /// running longer than `timeout_secs` (using its `started_at` stamp), then
    /// returns a short note to inject into the next model turn so it advances.
    fn apply_todo_timeout(workspace_dir: &str, timeout_secs: u64) -> Option<String> {
        if workspace_dir.is_empty() {
            return None;
        }
        let path = std::path::Path::new(workspace_dir).join("todos.json");
        let raw = std::fs::read_to_string(&path).ok()?;
        let root: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let items = root.get("items")?.as_array()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Scan for ALL still-'in_progress' items that have exceeded their timeout.
        let mut overdue: Vec<(usize, String)> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status == "in_progress" {
                let started = item.get("started_at").and_then(|v| v.as_u64()).unwrap_or(0);
                if started > 0 && now.saturating_sub(started) >= timeout_secs {
                    let desc = item.get("description").and_then(|d| d.as_str()).unwrap_or("(item)").to_string();
                    overdue.push((i, desc));
                }
            }
        }
        if overdue.is_empty() {
            return None;
        }
        // Re-open mutably to mark every overdue item 'skipped'.
        if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(ref mut items) = root.get_mut("items").and_then(|v| v.as_array_mut()) {
                for (idx, _) in &overdue {
                    if let Some(it) = items.get_mut(*idx) {
                        it["status"] = serde_json::json!("skipped");
                        it["started_at"] = serde_json::Value::Null;
                    }
                }
            }
            if let Ok(ser) = serde_json::to_string_pretty(&root) {
                let _ = std::fs::write(&path, ser);
            }
        }
        let names: Vec<&str> = overdue.iter().map(|(_, d)| d.as_str()).collect();
        Some(format!(
            "[TODO watchdog] {} item(s) exceeded the {}-second timeout and were auto-marked 'skipped': {}. Continue with the next unfinished item.",
            overdue.len(),
            timeout_secs,
            names.join("; ")
        ))
    }

    /// True if the session history already contains a `todo_update` call/result
    /// (assistant tool call or tool-result with that tool name). Used to avoid
    /// re-injecting a full stale snapshot when the model already has live TODO
    /// context, while still emitting a lightweight reminder on resume.
    fn history_has_todo(history: &[ChatMessage]) -> bool {
        history.iter().any(|m| {
            if m.role == "tool" {
                m.name.as_deref() == Some("todo_update")
            } else if m.role == "assistant" {
                m.tool_calls.as_ref().map_or(false, |calls| {
                    calls.iter().any(|c| c.function.name.as_deref() == Some("todo_update"))
                })
            } else {
                false
            }
        })
    }

    /// Build a lightweight one-line reminder (no full item dump) when the model
    /// already has TODO context but the list is still active. Returns None if
    /// `todos.json` is absent/empty. Pointer
    fn build_todo_reminder(workspace_dir: &str) -> Option<String> {
        if workspace_dir.is_empty() {
            return None;
        }
        let path = std::path::Path::new(workspace_dir).join("todos.json");
        let raw = std::fs::read_to_string(&path).ok()?;
        let root: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let items = root.get("items")?.as_array()?;
        if items.is_empty() {
            return None;
        }
        let total = items.len();
        let unfinished_count = items.iter().filter(|it| {
            let s = it.get("status").and_then(|v| v.as_str()).unwrap_or("");
            s != "completed" && s != "cancelled" && s != "skipped"
        }).count();
        Some(format!(
            "\n## Current TASKS (active)\nYou have an active TODO list ({} items, {} unfinished). Reload via `todo_update` action='list' and continue finishing it.\n\
            Rules:\n\
            - Progress ONE item at a time, in list order; you MAY mark a LATER item 'completed' if you verify it is already fully done, but keep working on the current item.\n\
            - Before declaring the whole list done, call `todo_update` 'list' and verify EVERY item actually delivered its intended output. If any is only partially done, set it back to 'pending'/'in_progress' and redo it.\n\
            - If your current message is a tangent or a new task, handle it, then RETURN to this unfinished list. To permanently abandon it, call `todo_update` 'clear' or tell the user you are detaching.\n",
            total,
            unfinished_count
        ))

    }
    fn read_workspace_file(workspace_dir: &str, filename: &str, max_chars: usize) -> Option<(String, bool)> {
        let path = std::path::Path::new(workspace_dir).join(filename);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if content.trim().is_empty() {
                    return None;
                }
                let truncated = content.len() > max_chars;
                let result = if truncated {
                    content.chars().take(max_chars).collect::<String>()
                } else {
                    content
                };
                Some((result, truncated))
            }
            Err(_) => None,
        }
    }
}

/// RAII lifecycle guard for a per-run Orchestrator owned by a spawned agent task.
///
/// Holds the strong `Arc<Orchestrator>` (and thus its clone of the event-stream
/// `tx`) for the duration of the task, and unregisters it from the global map
/// when the task ends by ANY path (normal completion, early return, or panic
/// unwind). Without this, the registry's strong Arc keeps a `tx` clone alive
/// after the agent loop finishes, so the event channel never closes and any
/// consumer awaiting stream close (e.g. the ManagedRunner Executor forward
/// loop) deadlocks forever.
struct OrchLifecycle {
    _orch: std::sync::Arc<crate::agent::orchestration::Orchestrator>,
    key: String,
}

impl Drop for OrchLifecycle {
    fn drop(&mut self) {
        // P7: Propagate cancellation to all workers BEFORE unregistering
        self._orch.shutdown();
        crate::agent::orchestration::unregister_orchestrator(&self.key);
    }
}
#[async_trait]

impl Agent for LlmAgent {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }

    async fn run(&self, ctx: &InvocationContext, user_message: &str, images: Vec<String>) -> AgentResult<EventStream> {
        let model = &ctx.model_name;
        let invocation_id = &ctx.base.invocation_id;
        let author = &ctx.agent_name;
        let max_iter = ctx.max_iterations;
        // P1 guard: NamesOnly / DiscoverToolOnly tell the model to load skill
        // bodies via `skill_read_file`. If that tool is NOT in the current tool
        // set (e.g. filtered/unregistered), fall back to Query so a readable
        // name:description catalog is shown instead of pointing at a dead tool.
        let skill_strategy = {
            if matches!(
                ctx.skill_listing_strategy,
                crate::skill::SkillListingStrategy::NamesOnly
                    | crate::skill::SkillListingStrategy::DiscoverToolOnly
            ) {
                let reg = self.tools.read().await;
                let has_srf = reg.tool_names().iter().any(|n| n == "skill_read_file");
                if !has_srf {
                    tracing::warn!(
                        "skill_read_file not in tool set; forcing SkillListingStrategy::Query fallback"
                    );
                    crate::skill::SkillListingStrategy::Query
                } else {
                    ctx.skill_listing_strategy
                }
            } else {
                ctx.skill_listing_strategy
            }
        };

        let skill_max_inline_chars = ctx.skill_max_inline_chars;
        let skill_catalog_max = ctx.skill_catalog_max;
        let skill_hot_top_k = ctx.skill_hot_top_k;

        // Use an mpsc channel to produce events, then convert to a Stream
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentResult<AgentEvent>>(200);

        // Build system prompt and history in the spawned task.
        // Prompt tier: Minimal (persona head only) for pure greetings,
        // Full otherwise; Minimal is a strict prefix of Full for cache
        // nesting across tier switches.
        let prompt_tier = PromptTier::select(user_message);
        let (system_prompt, task_skill_active) = self.build_system_prompt(
            prompt_tier,
            user_message,
            &ctx.conversation_history,
            skill_strategy,
            skill_max_inline_chars,
            skill_catalog_max,
            skill_hot_top_k,
        );
        // Per-turn-volatile context (date, language rule, TODO/evidence
        // state, guidance pool, budget dashboard) is collected in
        // `state_core` / `volatile_state` and appended AFTER history at
        // message assembly - never folded into the system prompt - so
        // the system prompt + history prefix stays byte-identical across
        // runs and provider prefix caching can hit.
        let mut state_core = String::new();
        let today = chrono::Local::now().format("%Y-%m-%d (%A)").to_string();
        let lang_rule = self.resolve_language_rule(user_message);
        state_core.push_str(&format!(
            "**Current date: {today}**\n\n## LANGUAGE RULE (STRICT - THIS message)\n{lang_rule}\n"
        ));
        // Inject the active TODO list as the main-session task contract.
        // Gated to main sessions only — sub/cron write to session-scoped files
        // and must not see/pollute the main `todos.json`.
        let session_id = ctx.base.session_id.clone();
        // Record skill-driven sessions so end-of-session SOP authoring can skip them.
        if task_skill_active && !session_id.is_empty() {
            self.skill_used_sessions.lock().unwrap().insert(session_id.clone());
        }
        let todo_item_timeout_secs = ctx.todo_item_timeout_secs;
        let is_main_session = is_main_session(&session_id);
        if is_main_session {
            // #3 (converged + resume-safe): always embed the full list/status
            // dump on checkpoint resume (history may be stale/partial, the model
            // needs the current truth from todos.json). For a fresh/normal run,
            // embed the full block only when history carries no live `todo_update`
            // results; otherwise inject a one-line reminder to reload instead of
            // pasting a potentially stale snapshot.
            let resumed = ctx.resume_history.is_some();
            let todo_in_history = Self::history_has_todo(&ctx.conversation_history);
            if resumed || !todo_in_history {
                if let Some(todo_block) = Self::build_todo_context_block(&self.workspace_dir, todo_item_timeout_secs) {
                    state_core.push_str(&todo_block);
                }
            } else if let Some(reminder) = Self::build_todo_reminder(&self.workspace_dir) {
                state_core.push_str(&reminder);
            }
        }
        // Evidence ledger: inject the incident-scoped ledger (budget-capped,
        // sensitive entries excluded) so the agent reuses, not re-runs, results.
        if let Some(evidence_block) =
            crate::tool::evidence::build_evidence_block_for_session(&self.workspace_dir, &session_id)
        {
            state_core.push_str(&evidence_block);
        }
        // Tool selectivity: core tools are always sent in full; peripheral tools
        // (MCP / external) are exposed on demand via `load_tool_schema`, and a
        // peripheral tool is re-added once loaded. This bounds the per-request
        // tool payload regardless of how many servers/tools are registered.
        // D10 pre-filter (P5): a single cheap boolean gates BOTH orchestration
        // tool delivery and Orchestrator construction. Computed here while
        // `user_message` is still the borrowed &str (it is shadowed to String
        // further down). When false the allowset stays empty and no Orchestrator
        // is built — the zero-overhead promise for non-fan-out runs.
        let prefilter = orchestration_prefilter(user_message);
        let orch_candidate = ctx.can_spawn
            && ctx.mode == crate::context::AgentMode::Instant
            && ctx.depth == 0
            && prefilter;
        // F7: surface the orchestration gate for diagnosis — this is the single
        // line that decides whether the run can fan out.
        if ctx.can_spawn
            && ctx.mode == crate::context::AgentMode::Instant
            && ctx.depth == 0
        {
            tracing::info!(
                "[session:{}] orchestration gate: prefilter={prefilter} -> orchard={orch_candidate} (+7 orchestration tools if true)",
                session_id
            );
        }
        let (core_tool_defs, load_schema_def) = {
            let reg = self.tools.read().await;
            let periph = reg.peripheral_tools();
            let mut defs = reg.core_definitions();
            // Delivery gate: hide orchestration tools unless the cheap pre-filter
            // flags this run as a fan-out candidate AND the mode/depth allowset
            // opens them (SDD §7.3 / D10). Zero overhead when prefilter misses.
            let orch_allowset = orchestration_delivered_for(ctx.mode, ctx.depth, orch_candidate);
            defs.retain(|d| orchestration_delivered(&d.function.name, &orch_allowset));
            let ls = if periph.is_empty() {
                None
            } else {
                let list = periph
                    .iter()
                    .map(|(n, d)| format!("- `{}`: {}", n, d))
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(crate::model::ToolDefinition {
                    tool_type: "function".to_string(),
                    function: crate::model::FunctionDefinition {
                        name: "load_tool_schema".to_string(),
                        description: format!(
                            "Tool schemas are loaded on demand to keep the request small. Available peripheral tool(s):\n{}\n\nCall with the exact tool name to get its full JSON schema, then invoke that tool normally.",
                            list
                        ),
                        parameters: serde_json::json!({
                            "type": "object",
                            "properties": {
                                "name": { "type": "string", "description": "Exact name of the peripheral tool to load" }
                            },
                            "required": ["name"]
                        }),
                    },
                })
            };
            (defs, ls)
        };
        info!("[session:{}] Core tool set: {} tool(s), +{} peripheral on demand", session_id, core_tool_defs.len(), {
            let reg = self.tools.read().await;
            reg.peripheral_tools().len()
        });
        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let working_dir = self.working_dir.clone();
        let workspace_dir = self.workspace_dir.clone();
        let mode = ctx.mode;
        let depth = ctx.depth;
        let can_spawn = ctx.can_spawn;
        let output_dir_override = match &ctx.tool_output_dir {
            Some(d) if !d.is_empty() => d.clone(),
            _ => String::new(),
        };
        let model = model.to_string();
        let invocation_id = invocation_id.to_string();
        let author = author.to_string();
        let user_message = user_message.to_string();
        let images = images;  // move into spawn
        let strategy = self.tool_execution_strategy;
        let parallel_ir_tools = self.parallel_ir_tools;
        let prev_history = ctx.conversation_history.clone();
        let permissions = ctx.permissions.clone();
        let permission_pending: PendingMap = ctx.permission_pending.clone();
        let preauth_profile = ctx.preauth_profile.clone();
        let fallback_model = ctx.fallback_model.clone();
        let rabbit_hole_threshold = ctx.rabbit_hole_threshold;
        let trim_redundant_tool_calls = ctx.trim_redundant_tool_calls;
        let knowledge_pre_retrieval = ctx.knowledge_pre_retrieval;
        let budget_dashboard_enabled = ctx.budget_dashboard;
        let budget_sink = ctx.budget_sink.clone();
        let knowledge_reminder: Option<String> = if knowledge_pre_retrieval {
            self.build_knowledge_reminder(&user_message)
        } else {
            None
        };
        let (sop_reminder, active_sop_id): (Option<String>, Option<String>) = if ctx.sop_replay {
            self.build_sop_reminder(&user_message, task_skill_active)
        } else {
            (None, None)
        };
        let tool_timeout_secs = ctx.tool_timeout_secs;
        let max_tool_retries = ctx.max_tool_retries;
        let context_window = ctx.context_window;
        let context_window_threshold = ctx.context_window_threshold;
        let inline_scaling_enabled = ctx.enable_context_scaling;
        let max_inline_chars = ctx.max_inline_chars;
        // Calculate max history tokens: context_window * threshold%
        let max_history_tokens: usize = context_window * context_window_threshold / 100;
        let checkpointer = ctx.checkpointer.clone();
        let checkpoint_id = ctx.checkpoint_id.clone();
        let resume_history = ctx.resume_history.clone();
        let resume_iteration = ctx.resume_iteration;
        let event_log_path = ctx.event_log_path.clone();
        let cleanup_sessions = self.cleanup_sessions.clone();

        // ── Sub-agent orchestration (SDD v1.5 Step 2a) ──
        // When this run is the Instant root allowed to spawn, create a per-run
        // Orchestrator and register it under this invocation id so the
        // orchestration tools can resolve it during the run. It is unregistered
        // when the returned event stream is fully consumed.
        let orch: Option<Arc<crate::agent::orchestration::Orchestrator>> = if ctx.can_spawn
            && ctx.mode == crate::context::AgentMode::Instant
            && ctx.depth == 0
            && orch_candidate
        {
            let env = crate::agent::orchestration::OrchestratorEnv {
                provider: self.provider.clone(),
                tools: self.tools.clone(),
                working_dir: self.working_dir.clone(),
                workspace_dir: self.workspace_dir.clone(),
                model_configs: self.model_configs.clone(),
                max_iterations: self.max_iterations,
                parallel_ir_tools: self.parallel_ir_tools,
                user_given_name: self.user_given_name.clone(),
                two_tier_memory: self.two_tier_memory,
                sop_replay: self.sop_replay.clone(),
                parent_model: ctx.model_name.clone(),
                permissions: ctx.permissions.clone(),
                permission_pending: ctx.permission_pending.clone(),
                preauth_profile: ctx.preauth_profile.clone(),
                context_window: ctx.context_window,
                enable_context_scaling: ctx.enable_context_scaling,
                max_inline_chars: ctx.max_inline_chars,
                tool_timeout_secs: ctx.tool_timeout_secs,
                max_tool_retries: ctx.max_tool_retries,
                max_concurrent_subagents: self.orchestration_limits.max_concurrent_subagents,
                default_timeout_secs: self.orchestration_limits.default_timeout_secs,
                memory_store: self.memory_store.clone(),
            };
            let root_ended = ctx.ended_flag();
            let orch = crate::agent::orchestration::Orchestrator::new(
                env,
                ctx.base.invocation_id.clone(),
                ctx.base.session_id.clone(),
                root_ended,
                crate::agent::orchestration::DEFAULT_MAX_DEPTH,
                Some(tx.clone()),
            );
            let orch = Arc::new(orch);
            let key = ctx.base.invocation_id.clone();
            crate::agent::orchestration::register_orchestrator(key, orch.clone());
            Some(orch)
        } else {
            None
        };
        let orch_key = ctx.base.invocation_id.clone();
        let ended_flag = ctx.ended_flag();

        tokio::spawn(async move {
            // Release the per-run Orchestrator when this task ends (by any path).
            let _orch_life = orch.map(|o| OrchLifecycle { _orch: o, key: orch_key.clone() });
            // ── Initialize event log for crash recovery ──
            let mut event_log = event_log_path.as_ref().and_then(|p| {
                match EventLog::open(p) {
                    Ok(log) => {
                        info!("[session:{}] Event log opened at {:?}", session_id, p);
                        Some(log)
                    }
                    Err(e) => {
                        warn!("[session:{}] Failed to open event log at {:?}: {}", session_id, p, e);
                        None
                    }
                }
            });

            // Log run started
            if let Some(ref mut log) = event_log {
                let _ = log.append(&LogEvent::RunStarted {
                    run_id: session_id.clone(),
                    timestamp: chrono::Utc::now(),
                    instruction: user_message.clone(),
                    model: model.clone(),
                    session_id: session_id.clone(),
                });
            }

            // Stable system prompt: byte-identical across runs (prefix-cache
            // friendly). Volatile per-run state is assembled separately and
            // appended after history at message assembly time.
            let stable_system_prompt = system_prompt.clone();
            let mut volatile_state = state_core.clone();
            let mut history: Vec<ChatMessage> = prev_history;

            // ── Resume from checkpoint ──
            let is_resumed = resume_history.is_some();
            if let Some(resumed_hist) = resume_history {
                info!("[session:{}] Resuming from checkpoint ({} history messages, start iter {:?})",
                      session_id, resumed_hist.len(), resume_iteration);
                history = resumed_hist;
            }

            // Memory blocks are lifted out of history and travel in the
            // trailing volatile state message (appended after history): the
            // recency position keeps them salient while leaving the system
            // prompt + history prefix untouched for prompt caching.
            let mut memory_blocks = Vec::new();
            history.retain(|msg| {
                if msg.role == "system" {
                    if let Some(content) = msg.content_as_text() {
                        if content.starts_with("[Memory Context") || content.starts_with("[Memory Recall") {
                            memory_blocks.push(content);
                            return false;
                        }
                    }
                }
                true
            });
            // ── 混合价值池（hybrid）：system/tools/deep/最近对话保底不进场；
            //    auto-memory(SQLite) + knowledge + SOP 在单一池内按价值排序、共享预算、降级不丢。
            let mut pool_arts: Vec<crate::context_arbiter::Artifact> = Vec::new();
            if !memory_blocks.is_empty() {
                let mem_text = memory_blocks.join("\n");
                pool_arts.push(crate::context_arbiter::artifact_from_block(
                    crate::context_arbiter::ArtifactKind::DeepFact, "auto-memory",
                    70.0, 0.8, false, mem_text,
                ));
            }
            if let Some(ref kblock) = knowledge_reminder {
                info!("[session:{}] Injected knowledge pre-retrieval pointers", session_id);
                pool_arts.push(crate::context_arbiter::artifact_from_block(
                    crate::context_arbiter::ArtifactKind::Knowledge, "knowledge",
                    55.0, 1.0, false, kblock.clone(),
                ));
            }
            if let Some(ref sblock) = sop_reminder {
                info!("[session:{}] Injected guided SOP replay", session_id);
                pool_arts.push(crate::context_arbiter::artifact_from_block(
                    crate::context_arbiter::ArtifactKind::Sop, "sop",
                    45.0, 0.9, false, sblock.clone(),
                ));
            }
            if !pool_arts.is_empty() {
                // 池预算：保底受限的阈值窗口一小份，不会饿死对话/sytem/tools。
                let pool_budget = (max_history_tokens / 8).min(12000).max(2048);
                let res = crate::context_arbiter::assemble(&mut pool_arts, pool_budget, 0);
                if !res.blocks.is_empty() {
                    volatile_state.push_str("\n\n## Context Guidance (value-ranked)\n");
                    for b in &res.blocks {
                        volatile_state.push_str("\n");
                        volatile_state.push_str(b);
                        volatile_state.push('\n');
                    }
                    info!("[session:{}] Hybrid guidance pool: {} blocks / {} tokens (budget {})", session_id, res.blocks.len(), res.used, pool_budget);
                }
            }

            // Account for system prompt size in the token budget.
            // System prompt is NOT part of history but consumes context window.
            let system_tokens = estimate_tokens(&stable_system_prompt) + estimate_tokens(&volatile_state);
            let mut history_budget = max_history_tokens.saturating_sub(system_tokens);
            if system_tokens > max_history_tokens / 2 {
                warn!("[session:{}] System prompt uses {} tokens ({}% of budget {}), history budget reduced to {} tokens",
                      session_id, system_tokens, system_tokens * 100 / max_history_tokens, max_history_tokens, history_budget);
            }
            info!("[session:{}] Context budget: system={} tokens, history_budget={} tokens (model={} tokens @ {}%)",
                  session_id, system_tokens, history_budget, context_window, context_window_threshold);

            // ── 有限脑预算自省（CONTEXT BUDGET）：让模型「知道自己的颅骨」以自我调节。 ──
            // 不作为正确性依赖（模型常忽略此类提示），硬仲裁/裁剪始终是权威（§12.7/§12.9）。
            if budget_dashboard_enabled {
                // 逐分类实测真实 token（temm1e 口径：used = 实际塞进上下文的各分类之和）。
                let base_system = estimate_tokens(&stable_system_prompt) + estimate_tokens(&state_core);
                let memory_toks: usize = memory_blocks.iter().map(|b| estimate_tokens(b)).sum();
                let knowledge_toks = knowledge_reminder.as_ref().map(|k| estimate_tokens(k)).unwrap_or(0);
                let sop_toks = sop_reminder.as_ref().map(|x| estimate_tokens(x)).unwrap_or(0);
                let tools_toks: usize = core_tool_defs.iter()
                    .map(|d| estimate_tokens(&serde_json::to_string(d).unwrap_or_default()))
                    .sum();
                let history_toks: usize = history.iter()
                    .map(|m| estimate_tokens(m.content_as_text().as_deref().unwrap_or("")))
                    .sum();
                // 预留 = 输出预留（skull/10）+ 安全守卫（skull/50）。
                let output_reserve = context_window / 10 + context_window / 50;
                let report = crate::context_arbiter::budget_report(
                    context_window,
                    output_reserve,
                    &[
                        ("System", base_system),
                        ("Tools", tools_toks),
                        ("Memory", memory_toks),
                        ("Knowledge", knowledge_toks),
                        ("SOP", sop_toks),
                        ("History", history_toks),
                    ],
                );
                volatile_state.push_str(&format!(
                    "\n\n=== CONTEXT BUDGET ===\nLimit: {} tokens | Used: {} | Available: {}\n  System: {} | Tools: {} | Memory: {} | Knowledge: {} | SOP: {} | History: {}\nPrioritize high-value content and trim/stop before exceeding the window.\n=== END BUDGET ===",
                    report.window, report.used, report.free,
                    base_system, tools_toks, memory_toks, knowledge_toks, sop_toks, history_toks,
                ));
                // 追加自省块后重算 system/history 预算，确保后续裁决基于真实尺寸。
                history_budget = max_history_tokens.saturating_sub(estimate_tokens(&stable_system_prompt) + estimate_tokens(&volatile_state));
                // 写共享快照供 /api/budget（与系统提示同源）。
                if let Some(sink) = &budget_sink {
                    *sink.lock().unwrap() = Some(report);
                }
            }

            if !is_resumed {
                if !images.is_empty() {
                    history.push(ChatMessage::user_with_images(&user_message, &images));
                } else {
                    history.push(ChatMessage::user(&user_message));
                }
            }

            // Session-lifetime set of already-executed tool signatures. Used ONLY by
            // trim_redundant_tool_calls to drop redundant trailing calls; kept separate
            // from the contiguous rabbit-hole streak below so the two never interfere.
            let mut executed_sigs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            // Contiguous rabbit-hole detection (v1.0.11): only CONSECUTIVE rounds that emit
            // the exact same tool batch count. Any interleaved different batch (or a result
            // change, reset in the stall section) breaks the streak, so a tool called 5x
            // across 50 rounds of normal progress no longer false-triggers. This replaces
            // the old lifetime-accumulating call_signatures counter.
            let mut prev_rabbit_batch: Option<String> = None;
            let mut rabbit_streak: usize = 0;
            // Automatic stall self-heal state (no human required).
            // last_result: per-tool (last content digest, consecutive repeat count).
            // no_new_state_iters: consecutive iterations that produced no new result.
            // reconsider_events: number of automatic strategy-reconsiderations fired.
            const MAX_AUTO_RECONSIDERS: usize = 3;
            // Stall sensitivity derives from the user-configurable (and hot-reloadable)
            // rabbit_hole_threshold, so a single knob tunes both loop and stall detection.
            let stall_repeat_threshold = rabbit_hole_threshold.max(2);
            const TEXT_REPEAT_LIMIT: usize = 6;
            // F3 (v1.0.11): last_result key is (tool_name, args_digest) so the same tool
            // with different arguments never shares a "repeated result" counter; the value
            // is (last content digest, consecutive repeat count).
            let mut last_result: std::collections::HashMap<(String, u64), (u64, usize)> = std::collections::HashMap::new();
            // F3: bounded LRU window of recently seen exact (name,args,result) triples for a
            // robust "no genuinely new state" check — small cycles are still caught, while an
            // occasional short-result collision no longer reads as stagnation.
            let mut recent_results: std::collections::VecDeque<(u64, u64, u64)> = std::collections::VecDeque::new();
            const F3_RECENT_WINDOW: usize = 20;
            const F3_SHORT_LIMIT: usize = 32;
            let mut no_new_state_iters: usize = 0;
            let mut reconsider_events: usize = 0;
            let mut last_resp_digest: u64 = 0;
            let mut consecutive_resp: usize = 0;
            // Track which model we're using (for fallback)
            let mut active_model = model.clone();
            let mut used_fallback = false;
            let mut has_executed_tools = false;
            let mut reprompt_count = 0u32;
            // 传输层截断补救计数器。流被传输错误切断时把已流出的残文并入历史，并要求模型从中断处继续。
            // 上限为 1，避免同一次故障反复触发、堆叠出不可控的额外轮次。
            const MAX_STREAM_RECOVERIES: u32 = 1;
            let mut stream_recoveries = 0u32;
            // SOP 结果记录（A1）运行级状态
            let mut active_sop_id = active_sop_id;
            let run_started = std::time::Instant::now();
            let mut run_tool_calls: u32 = 0u32;
            let mut run_has_error = false;

            let start_iter = resume_iteration.unwrap_or(0);
            for iteration in start_iter..max_iter {
                info!("[session:{}] Agent loop iteration {} (model: {})", session_id, iteration + 1, active_model);

                // Log turn started
                if let Some(ref mut log) = event_log {
                    let _ = log.append(&LogEvent::TurnStarted {
                        run_id: session_id.clone(),
                        timestamp: chrono::Utc::now(),
                        turn_number: iteration as u32,
                    });
                }

                // If the consumer (WebSocket client) dropped the event stream —
                // e.g. user clicked Stop or the connection closed — abort the
                // agent loop so we don't keep streaming from the LLM into a dead
                // channel. Exception: if sub-agents are still running (active
                // orchestration), do not tear the run down mid-wait — that would
                // cancel every in-flight worker and lose their results. Let
                // wait_subagent collect them first; once no worker is left the
                // next pass aborts normally.
                if tx.is_closed() {
                    if !crate::agent::orchestration::has_inflight_workers(&invocation_id) {
                        info!("[session:{}] Consumer channel closed, aborting agent loop", session_id);
                        ended_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                    info!("[session:{}] Consumer closed but sub-agents in flight; continuing to collect worker results",
                          session_id);
                }

                // Drain only EXPLICIT "insert-now" messages and feed them into the
                // running agent as the next user turn (no stop required). Ordinary
                // follow-up messages stay in the pending queue and are dispatched by
                // the server as separate tasks after this run completes.
                inject_user_interjections(&mut history, &session_id);
                // Per-item TODO timeout watchdog (main session only): if the
                // active item stalled past its timeout, auto-mark it 'skipped'
                // and tell the model to advance to the next item.
                if is_main_session {
                    if let Some(note) = Self::apply_todo_timeout(&workspace_dir, todo_item_timeout_secs) {
                        info!("[session:{}] {}", session_id, note);
                        history.push(ChatMessage::system(&note));
                    }
                }
                // Trim history if approaching context limit using token-based budget
                let total_tokens: usize = history.iter().map(|m| estimate_tokens(m.content_as_text().as_deref().unwrap_or(""))).sum();
                if total_tokens > history_budget {
                    warn!("[session:{}] History too large ({} est. tokens, budget: {} tokens), trimming with value-oriented strategy",
                          session_id, total_tokens, history_budget);
                    trim_history_by_value(&mut history, history_budget);
                    let new_tokens: usize = history.iter().map(|m| estimate_tokens(m.content_as_text().as_deref().unwrap_or(""))).sum();
                    info!("[session:{}] History trimmed from {} to {} est. tokens", session_id, total_tokens, new_tokens);
                }

                let mut messages = Vec::with_capacity(2 + history.len());
                messages.push(ChatMessage::system(&stable_system_prompt));
                messages.extend(history.iter().cloned());
                // Trailing volatile state (date/lang/TODO/evidence/guidance/
                // budget): placed after history so the system prompt +
                // history prefix stays byte-stable across runs for caching.
                if !volatile_state.trim().is_empty() {
                    messages.push(ChatMessage::system(&volatile_state));
                }

                // Build tool definitions for this request: core tools + the
                // load_tool_schema helper + any peripheral tools already loaded.
                let tool_defs = {
                    let mut defs = core_tool_defs.clone();
                    if let Some(ls) = &load_schema_def {
                        defs.push(ls.clone());
                    }
                    let reg = tools.read().await;
                    for name in revealed_tool_names(&history) {
                        if let Some(d) = reg.get_definition(&name) {
                            defs.push(d);
                        }
                    }
                    defs
                };

                // Call LLM via legacy chat_stream (uses mpsc for text deltas).
                // During re-prompt iterations, suppress text streaming to the UI
                // by using a throwaway channel — the model's response is expected
                // to be raw tool-call JSON which should NOT be displayed.
                let stream_tx = if reprompt_count > 0 && !has_executed_tools {
                    // Re-prompt mode: create a dummy channel to swallow text deltas
                    let (dummy_tx, mut dummy_rx) = tokio::sync::mpsc::channel::<AgentResult<AgentEvent>>(4);
                    // Spawn a drain task so the channel never blocks
                    tokio::spawn(async move {
                        while dummy_rx.recv().await.is_some() {}
                    });
                    dummy_tx
                } else {
                    tx.clone()
                };

                let result = provider
                    .chat_stream(&active_model, &messages, &tool_defs, stream_tx, &invocation_id, &author)
                    .await;

                match result {
                    Ok((content, reasoning, tool_calls, usage, finish_reason, stream_timed_out)) => {
                        // Text-loop detection: halt when the assistant emits the same
                        // textual turn repeatedly with no visible progress. v1.0.11: only
                        // NON-tool rounds are counted — a pure tool-call turn for a
                        // non-reasoning model has empty content+reasoning, so digesting
                        // every round would flag a legitimate multi-step tool workflow
                        // (>=6 consecutive tool calls) as an "identical text loop" and
                        // kill it. Tool-round stalls are handled by the contiguous
                        // rabbit-hole / auto-stall checks instead.
                        if tool_calls.is_empty() {
                            // A narration-only round breaks any contiguous tool-batch loop.
                            prev_rabbit_batch = None;
                            rabbit_streak = 0;
                            // 文本回路判定。截断回合的 fragment 是残缺前缀，不代表模型主动重复，也不该推进重复计数，
                            // 否则连续截断回合之后，一个本需transport补救的完整回合会误触发自动停止。
                            let resp_digest = content_digest(&format!("{}\n{}", content, reasoning));
                            if should_auto_stop_text_loop(stream_timed_out, resp_digest, &mut last_resp_digest, &mut consecutive_resp, TEXT_REPEAT_LIMIT) {
                                warn!("[session:{}] Text-loop: identical assistant turn repeated {} times; terminating with summary", session_id, consecutive_resp);
                                let _ = tx.send(Ok(AgentEvent::text(
                                    &format!("\n\n*[Auto-stop] The agent repeated the same response {} times without progress. Stopping. Send a new message to continue.*\n\n", consecutive_resp),
                                    &invocation_id, &author
                                ))).await;
                                let _ = tx.send(Ok(AgentEvent::done(&invocation_id, &author))).await;
                                for s in &cleanup_sessions { let _ = s.close().await; }
                                return;
                            }
                        }
                        // Emit token usage event if available
                        if let Some(ref u) = usage {
                            let prompt_t = u.prompt_tokens.unwrap_or(0);
                            let completion_t = u.completion_tokens.unwrap_or(0);
                            let total_t = u.total_tokens.unwrap_or(prompt_t + completion_t);
                            let _ = tx.send(Ok(AgentEvent::usage(&active_model, prompt_t, completion_t, total_t, &invocation_id, &author))).await;
                        }
                        // If the consumer disappeared mid-stream, don't continue
                        // executing tools or making further LLM calls — unless
                        // sub-agents are still running (active orchestration), in
                        // which case keep going so wait_subagent can collect them.
                        if tx.is_closed()
                            && !crate::agent::orchestration::has_inflight_workers(&invocation_id)
                        {
                            info!("[session:{}] Consumer closed during LLM response, stopping", session_id);
                            ended_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                            return;
                        }
                        // If the model didn't emit native tool_calls, try to
                        // extract them from the text content AND from the
                        // reasoning_content (DeepSeek thinking mode puts
                        // everything in reasoning_content, leaving content
                        // empty).
                        // A cut stream leaves reasoning_content that may contain a partial tool-call
                        // envelope. Treating that fragment as "the model wants to use a tool" would
                        // fall into the re-prompt path with nothing usable to retry, so suppression
                        // applies here and the dedicated transport recovery below takes over.
                        let mut tool_calls = if tool_calls.is_empty() && !stream_timed_out {
                            info!("[session:{}] No native tool_calls from API, attempting text extraction (content={} chars, reasoning={} chars)",
                                  session_id, content.len(), reasoning.len());
                            let mut extracted = extract_tool_calls_from_content(&content);
                            if extracted.is_empty() && !reasoning.is_empty() {
                                info!("[session:{}] Content extraction found nothing, scanning reasoning_content for tool calls", session_id);
                                extracted = extract_tool_calls_from_content(&reasoning);
                                if extracted.is_empty() {
                                    let reasoning_preview: String = reasoning.chars().take(200).collect();
                                    warn!("[session:{}] Extraction from reasoning_content found no tool calls. Preview: {}...", session_id, reasoning_preview);
                                }
                            }
                            if !extracted.is_empty() {
                                info!("[session:{}] Extracted {} tool call(s) from text/reasoning", session_id, extracted.len());
                            }
                            extracted
                        } else {
                            tool_calls
                        };
                        // Re-prompt fallback: ONLY when no tools have been
                        // executed yet in this session. If the model returned
                        // text without any tool calls, but tools ARE available
                        // and the response looks like it *wanted* to use a tool
                        // (mentions a tool name, is in thinking mode with empty
                        // content, or uses intent phrases like "let me check"),
                        // push a correction and loop again.
                        // Once tools have been executed, never re-prompt — the
                        // model is summarizing results, not trying to call tools.
                        let combined = if content.trim().is_empty() { &reasoning } else { &content };
                        info!("[session:{}] Response analysis: content={} chars, reasoning={} chars, native_tool_calls={}",
                              session_id, content.len(), reasoning.len(), tool_calls.len());
                        // stream_timed_out: a cut round is handled by the dedicated transport
                        // recovery below, not by the malformed-envelope re-prompt (which would
                        // re-ask with a correction unrelated to the real cause).
                        if tool_calls.is_empty() && !stream_timed_out && !tool_defs.is_empty() && !has_executed_tools && reprompt_count < 2 && !combined.trim().is_empty() {
                            // D3 根治：是否重提示改由结构化 decide_turn 决定；散文信号
                            // （工具名子串 / 意图词 / 长度阈值）全部删除，不再参与决策。
                            // 一段实质文本回答永远被接受为 Answer（治愈“复述含 browser_cdp 的记忆”误触）。
                            // 查漏1（观测）：探测“有工具信封但解析失败”——潜在丢失的工具调用。
                            let malformed_env = tool_calls.is_empty()
                                && crate::turn_decision::looks_like_tool_envelope(combined);
                            if malformed_env {
                                warn!("[session:{}] detected tool-call envelope but parsed empty (iter {}): potentially lost tool call; Stage B observes only, no retry", session_id, iteration);
                            }
                            let turn_signals = crate::turn_decision::TurnSignals {
                                tool_calls: tool_calls.len(),
                                finish_reason: crate::turn_decision::FinishReason::normalize(finish_reason.as_deref()),
                                has_visible_text: !combined.trim().is_empty(),
                                // 查漏1：malformed_envelope 现由 looks_like_tool_envelope 真实置位（信号保真）；
                                // 但 native_tool_calling 仍硬编码 true → decide_turn 不会选 RetryMalformed（重试行为不变，仅 warn! 观测）。
                                malformed_envelope: malformed_env,
                                native_tool_calling: true,
                                ran_tools: has_executed_tools,
                                malformed_retry_done: reprompt_count > 0,
                            };
                            let turn_decision = crate::turn_decision::decide_turn(&turn_signals);
                            info!("[session:{}] turn decision={:?} tool_calls={} finish={:?} visible_text={} ran_tools={}",
                                  session_id, turn_decision, turn_signals.tool_calls, turn_signals.finish_reason, turn_signals.has_visible_text, has_executed_tools);
                            if matches!(turn_decision, crate::turn_decision::TurnDecision::RetryMalformed) {

                                reprompt_count += 1;
                                info!("[session:{}] Re-prompting model to emit well-formed tool call JSON (iter {}, attempt {}, reason: malformed tool envelope)", session_id, iteration, reprompt_count);

                                // Notify the user that the system is retrying the tool call
                                let _ = tx.send(Ok(AgentEvent::thinking(
                                    "[正在重新组织工具调用...]",
                                    &invocation_id, &author
                                ))).await;

                                history.push(ChatMessage::assistant(combined));

                                // Build a tool list hint so the model knows which tools are available
                                let tool_list_hint = tool_defs.iter()
                                    .map(|t| format!("- `{}`: {}", t.function.name, t.function.description.chars().take(80).collect::<String>()))
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                let correction = format!(
                                    "You said you would take an action, but you did NOT output a tool call.\n\n\
                                    Available tools:\n{}\n\n\
                                    Output ONLY the tool call JSON code block — NO narrative text, NO explanation, NO preamble.\n\
                                    Format:\n\
                                    ```json\n{{\"name\": \"shell_exec\", \"arguments\": {{\"command\": \"ipconfig\"}}}}\n```\n\
                                    Replace the tool name and arguments with what you actually need.\n\n\
                                    CRITICAL: Your entire response must be ONLY the ```json ... ``` block. Nothing before it, nothing after it.",
                                    tool_list_hint
                                );
                                history.push(ChatMessage::user(&correction));
                                continue;
                            }
                        }
                        // ── Transport-cut recovery ──
                        // A transport error can cut the stream mid-transmission, whether or not the
                        // fragment happens to contain usable tool calls. Both existing gates fail
                        // to cover this: the malformed-envelope re-prompt is gated by
                        // has_executed_tools, and the empty-response summary is gated by content
                        // being empty. Leaving it unhandled makes the loop report a normal text
                        // answer while the client only ever received the truncated prefix
                        // (observed worst case: 7 characters), forcing a manual continue.
                        // Tool calls parsed out of a cut stream carry truncated arguments; executing
                        // them would fail JSON parsing or run with wrong values. They are dropped here,
                        // before the budget check, so a cut round never reaches tool execution even
                        // when no recovery attempt is left.
                        let cut = classify_stream_cut(
                            stream_timed_out, tool_calls.len(), stream_recoveries, MAX_STREAM_RECOVERIES,
                        );
                        if stream_timed_out {
                            tool_calls.clear();
                        }
                        if let Some(StreamCutRecovery::Continue { dropped_tool_calls }) = cut {
                            stream_recoveries += 1;
                            warn!("[session:{}] Stream cut by transport error (content {} chars, reasoning {} chars, {} parsed tool call(s) dropped); transport recovery {}/{}",
                                  session_id, content.chars().count(), reasoning.chars().count(), dropped_tool_calls, stream_recoveries, MAX_STREAM_RECOVERIES);
                            // The truncated text was already streamed to the client, so it has to
                            // go into history verbatim to keep history and UI consistent. It is
                            // pushed as plain assistant text: a cut fragment can contain an
                            // unfinished tool-call envelope that must not be serialised as tool_calls.
                            // Do not push the dropped tool calls to history.
                            if !content.trim().is_empty() {
                                history.push(ChatMessage::assistant(&content));
                            }
                            history.push(ChatMessage::user(
                                "[STREAM CUT] Your previous response was cut off mid-transmission by a transport error; what was received is truncated, not a deliberate stop. \
                                 Continue from exactly where it stopped: output the remainder and finish the turn. \
                                 Do NOT repeat the truncated text and do NOT re-call tools whose results you already have.",
                            ));
                            let _ = tx.send(Ok(AgentEvent::text(
                                if dropped_tool_calls > 0 {
                                    "\n\n*[Stream cut by transport error; incomplete tool call(s) dropped — asking the model to continue]*\n\n"
                                } else {
                                    "\n\n*[Stream cut by transport error — asking the model to continue]*\n\n"
                                },
                                &invocation_id, &author
                            ))).await;
                            continue;
                        }
                        if tool_calls.is_empty() {
                            // Text response - done
                            // PROBE: completion signal for diagnosis (real finish_reason + stub detection)
                            let _probe_rlen = reasoning.chars().count();
                            let _probe_clen = content.chars().count();
                            let _probe_stub = _probe_clen == 0 || (_probe_rlen > 256 && _probe_clen * 4 < _probe_rlen);
                            if cut == Some(StreamCutRecovery::Exhausted) {
                                // 到达这里说明补救预算已耗尽（预算可用时会在上方向 continue）。
                                // 本轮以残缺文本收尾，结局判定为失败，供 SOP 学习与人工排查使用。
                                warn!("[session:{}] Stream cut by transport error and recovery budget exhausted; ending turn with truncated text ({} chars)",
                                      session_id, content.chars().count());
                                run_has_error = true;
                            }
                            info!("[session:{}] COMPLETE_PROBE finish_reason={:?} content={}chars reasoning={}chars stub={} has_executed_tools={} reprompt_count={}",
                                  session_id, finish_reason, _probe_clen, _probe_rlen, _probe_stub, has_executed_tools, reprompt_count);
                            info!("[session:{}] Agent completed with text response ({} chars, {} tool calls)", session_id, content.len(), tool_calls.len());
                            // 记录 SOP 执行结果（A1）：若本次自动加载了 SOP，则将结局写回
                            // times_executed/succeeded/failed，驱动 Q/R/U 学习与周期 GC。
                            if let Some(id) = active_sop_id.take() {
                                let success = !run_has_error;
                                let tc = run_tool_calls;
                                let dur = run_started.elapsed().as_secs().min(u32::MAX as u64) as u32;
                                let ws = workspace_dir.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = crate::sop::record_result(&ws, &id, success, tc, dur) {
                                        warn!("[sop] record_result: {}", e);
                                    }
                                });
                            }
                            if content.len() < 100 {
                                info!("[session:{}] Short response content: {}", session_id, content);
                            }
                            // Handle empty response - request a final summary from LLM
                            let (final_content, already_streamed) = if content.trim().is_empty() && iteration > 0 {
                                warn!("[session:{}] LLM returned empty response after {} iterations, requesting summary", session_id, iteration + 1);
                                // Add a summary request to history and ask LLM one more time
                                let summary_prompt = "Please provide a final summary of the task. Include:\n\
                                    1. Whether the task succeeded or failed\n\
                                    2. If succeeded: summarize what was accomplished\n\
                                    3. If failed: explain what went wrong and what additional conditions, tools, or information would be needed to retry\n\
                                    Be specific and helpful.".to_string();
                                history.push(ChatMessage::user(&summary_prompt));
                                let mut summary_msgs = vec![ChatMessage::system(&system_prompt)];
                                summary_msgs.extend(history.clone());
                                // One more LLM call for summary (no tools) - streams to client
                                match provider.chat_stream(&active_model, &summary_msgs, &[], tx.clone(), &invocation_id, &author).await {
                                    Ok((summary_content, _, _, _, _, stream_timed_out)) => {
                                        // A cut summary round leaves only a truncated prefix on the client, so treat it like an
                                        // empty summary and stream the static summary in full.
                                        if stream_timed_out || summary_content.trim().is_empty() {
                                            (generate_static_summary(&history, iteration + 1), false)
                                        } else {
                                            (summary_content, true) // already streamed
                                        }
                                    }
                                    Err(e2) => {
                                        warn!("Summary request also failed: {}", e2);
                                        (generate_static_summary(&history, iteration + 1), false)
                                    }
                                }
                            } else {
                                (content, true) // normal response, already streamed
                            };
                            history.push(ChatMessage::assistant(&final_content));
                            // Only send text event if NOT already streamed to client
                            if !already_streamed && !final_content.trim().is_empty() {
                                let _ = tx.send(Ok(AgentEvent::text(&final_content, &invocation_id, &author))).await;
                            }
                            // Task completed normally — delete checkpoint
                            if let Some(ref cp) = checkpointer {
                                if let Some(ref cp_id) = checkpoint_id {
                                    let _ = cp.delete(cp_id);
                                    info!("[session:{}] Checkpoint deleted (task completed)", session_id);
                                }
                            }
                            // Log run completed
                            if let Some(ref mut log) = event_log {
                                let _ = log.append(&LogEvent::RunCompleted {
                                    run_id: session_id.clone(),
                                    timestamp: chrono::Utc::now(),
                                    total_turns: iteration as u32 + 1,
                                    total_tokens: 0, // Token tracking can be added later
                                    duration_ms: 0,
                                });
                            }
                            let _ = tx.send(Ok(AgentEvent::done(&invocation_id, &author))).await;
                            // Cleanup: close browser sessions after agent completes
                            for s in &cleanup_sessions { let _ = s.close().await; }
                            return;
                        }

                        // ── Trim redundant trailing tool calls after a final text ──
                        // If the model already ran tools and is now summarizing, but also
                        // emitted trailing tool call(s) that EXACTLY repeat calls already
                        // executed this session, drop the duplicates and honor the final text.
                        // Guarded by config.trim_redundant_tool_calls; narrow (dup-only) so a
                        // genuinely new tool call is never dropped.
                        if trim_redundant_tool_calls && has_executed_tools
                            && content.trim().len() >= 40 && !tool_calls.is_empty() {
                            let kept = tool_calls.iter().filter(|tc| {
                                let sig = format!("{}:{}",
                                    tc.function.name.as_deref().unwrap_or("unknown"),
                                    tc.function.arguments.as_deref().unwrap_or("{}"));
                                executed_sigs.get(&sig).copied().unwrap_or(0) == 0
                            }).cloned().collect::<Vec<_>>();
                            let dropped = tool_calls.len() - kept.len();
                            if dropped > 0 {
                                info!("[session:{}] Trimmed {} redundant trailing tool call(s) after final text ({} kept). Text: {}...",
                                      session_id, dropped, kept.len(),
                                      content.trim().chars().take(70).collect::<String>());
                                let _ = tx.send(Ok(AgentEvent::text(
                                    &format!("\n\n*[Trimmed {} redundant trailing tool call(s) — using the answer above]*\n\n", dropped),
                                    &invocation_id, &author
                                ))).await;
                                if kept.is_empty() {
                                    // All trailing calls were redundant; the answer text is
                                    // already streamed to the client, so just finish.
                                    history.push(ChatMessage::assistant(&content));
                                    // Mirror the normal completion path: clear checkpoint & log done.
                                    if let Some(ref cp) = checkpointer {
                                        if let Some(ref cp_id) = checkpoint_id {
                                            let _ = cp.delete(cp_id);
                                            info!("[session:{}] Checkpoint deleted (task completed after trim)", session_id);
                                        }
                                    }
                                    if let Some(ref mut log) = event_log {
                                        let _ = log.append(&LogEvent::RunCompleted {
                                            run_id: session_id.clone(),
                                            timestamp: chrono::Utc::now(),
                                            total_turns: iteration as u32 + 1,
                                            total_tokens: 0,
                                            duration_ms: 0,
                                        });
                                    }
                                    let _ = tx.send(Ok(AgentEvent::done(&invocation_id, &author))).await;
                                    for s in &cleanup_sessions { let _ = s.close().await; }
                                    return;
                                }
                                tool_calls = kept;
                            }
                        }

                        // Tool calls - execute them
                        info!("[session:{}] Agent returned {} tool call(s)", session_id, tool_calls.len());

                        // ── Rabbit hole detection: check BEFORE pushing to history or executing ──
                        // v1.0.11+: CONTIGUOUS semantics. Only a run of consecutive rounds
                        // emitting the exact same tool batch (same name+args, order-
                        // insensitive) with no result change accumulates. Interleaved
                        // different work or a result change (see the stall section) resets
                        // the streak, so a tool called a few times amid long normal progress
                        // no longer false-fires.
                        let mut rabbit_hole_fired = false;
                        if !tool_calls.is_empty() {
                            // Record executed signatures for the session-lifetime trim/dedup.
                            for tc in &tool_calls {
                                let sig = format!("{}:{}",
                                    tc.function.name.as_deref().unwrap_or("unknown"),
                                    tc.function.arguments.as_deref().unwrap_or("{}"));
                                *executed_sigs.entry(sig).or_insert(0) += 1;
                            }
                            let batch_sig = build_batch_signature(&tool_calls);
                            if let Some(count) = rabbit_hole_check(
                                &mut prev_rabbit_batch, &mut rabbit_streak, &batch_sig, rabbit_hole_threshold,
                            ) {
                                let names = tool_calls.iter()
                                    .map(|tc| tc.function.name.as_deref().unwrap_or("unknown"))
                                    .collect::<Vec<_>>().join(", ");
                                warn!("[session:{}] Rabbit hole: identical tool batch ({}) repeated {} consecutive times with no state change", session_id, names, count);
                                let correction = format!(
                                    "You called the same tool batch ({}) with identical arguments {} times in a row and the observed state did not change.\n\n\
                                     You MUST stop and try a different approach. Options:\n\
                                     1. Use different arguments\n\
                                     2. Use a completely different tool\n\
                                     3. If you already have enough information, provide your analysis as text\n\n\
                                     Do NOT repeat the same tool calls expecting a different result.",
                                    names, count
                                );
                                history.push(ChatMessage::user(&correction));
                                let _ = tx.send(Ok(AgentEvent::text(
                                    &format!("\n\n*[Rabbit hole: identical tool batch ({}) repeated {} consecutive times — execution halted, LLM must change approach]*\n\n", names, count),
                                    &invocation_id, &author
                                ))).await;
                                rabbit_hole_fired = true;
                            }
                        }

                        if rabbit_hole_fired {
                            // Skip tool execution and do NOT push tool calls to history.
                            // The correction message is already in history as the latest user
                            // message. The LLM will see it on the next iteration and must
                            // change its approach.
                            continue;
                        }

                        has_executed_tools = true;
                        history.push(ChatMessage::assistant_with_tool_calls(tool_calls.clone()));

                        // Create permission checker for this iteration
                        let checker = PermissionChecker::new(
                            permission_pending.clone(),
                            tx.clone(),
                            permissions.clone(),
                            invocation_id.clone(),
                            author.clone(),
                            preauth_profile.clone(),
                        );

                        let hist_start = history.len();
                        // Execute based on strategy
                        match strategy {
                            ToolExecutionStrategy::Sequential => {
                                // Parallel read-only execution (generalized from the old
                                // IR-collection-only gate): any batch of 2+ calls where EVERY
                                // tool declares `is_read_only()` runs concurrently - IR
                                // collection sets and everyday read batches (e.g. 3x
                                // file_read / knowledge_search in one turn) alike. Mixed or
                                // mutable-state tools (deep_memory, sys_process, todo_update,
                                // browser_cdp, ...) stay sequential to avoid racing shared
                                // state. The IR fast path short-circuits without the
                                // registry lock.
                                let parallel_safe = parallel_ir_tools
                                    && tool_calls.len() >= 2
                                    && (is_ir_collection_batch(&tool_calls) || {
                                        let registry = tools.read().await;
                                        tool_calls.iter().all(|tc| {
                                            tc.function
                                                .name
                                                .as_deref()
                                                .and_then(|n| registry.get(n))
                                                .map(|t| t.is_read_only())
                                                .unwrap_or(false)
                                        })
                                    });
                                if parallel_safe {
                                    info!("[session:{}] Read-only batch detected ({} tools), executing concurrently",
                                          session_id, tool_calls.len());
                                    let _ = tx.send(Ok(AgentEvent::text(
                                        &format!("\n\n*[Parallel read-only: {} tools running concurrently]*\n\n", tool_calls.len()),
                                        &invocation_id, &author
                                    ))).await;
                                    let msgs = execute_tools_concurrent(
                                        &tools, &tool_calls, &working_dir, &workspace_dir, &output_dir_override, &invocation_id, &author, &session_id, &tx, &checker, tool_timeout_secs, max_tool_retries, context_window, inline_scaling_enabled, max_inline_chars, mode, depth, can_spawn, &invocation_id,
                                    ).await;
                                    history.extend(msgs);
                                } else {
                                    // Standard sequential execution
                                    for tc in &tool_calls {
                                        inject_user_interjections(&mut history, &session_id);
                                        let msg = execute_tool_call(
                                            &tools, tc, &working_dir, &workspace_dir, &output_dir_override, &invocation_id, &author, &session_id, &tx, &checker, tool_timeout_secs, max_tool_retries, context_window, inline_scaling_enabled, max_inline_chars, mode, depth, can_spawn, &invocation_id, event_log.as_mut(),
                                        ).await;
                                        history.push(msg);
                                    }
                                }
                            }
                            ToolExecutionStrategy::Parallel | ToolExecutionStrategy::Auto => {
                                // `Auto` only runs concurrently when every call in
                                // the batch is read-only; otherwise it falls back to
                                // sequential to avoid racing mutable operations.
                                // `Parallel` always runs concurrently (caller's
                                // responsibility to pass safe tools).
                                let registry = tools.read().await;
                                let all_read_only = strategy == ToolExecutionStrategy::Parallel
                                    || tool_calls.iter().all(|tc| {
                                        let n = tc.function.name.as_deref().unwrap_or("");
                                        registry.get(n).map(|t| t.is_read_only()).unwrap_or(false)
                                    });
                                drop(registry);

                                if all_read_only && tool_calls.len() > 1 {
                                    info!("[session:{}] Executing {} tool call(s) concurrently", session_id, tool_calls.len());
                                    let msgs = execute_tools_concurrent(
                                        &*tools, &tool_calls, &working_dir, &workspace_dir, &output_dir_override, &invocation_id, &author, &session_id, &tx, &checker, tool_timeout_secs, max_tool_retries, context_window, inline_scaling_enabled, max_inline_chars, mode, depth, can_spawn, &invocation_id,
                                    ).await;
                                    history.extend(msgs);
                                } else {
                                    for tc in &tool_calls {
                                        inject_user_interjections(&mut history, &session_id);
                                        let msg = execute_tool_call(
                                            &*tools, tc, &working_dir, &workspace_dir, &output_dir_override, &invocation_id, &author, &session_id, &tx, &checker, tool_timeout_secs, max_tool_retries, context_window, inline_scaling_enabled, max_inline_chars, mode, depth, can_spawn, &invocation_id, event_log.as_mut(),
                                        ).await;
                                        history.push(msg);
                                    }
                                }
                            }
                        }

                        // ── SOP 结果记录（A1）──
                        // 统计本轮工具调用数，并从本轮 tool 结果中探测错误/被拒信号。
                        // ── SOP 结果记录（A1）──
                        // 统计本轮工具调用数；C1：成功信号改为"结构化工具错误信封"判定，
                        // 不再扫描正文里的 error/failed 子串（那是 IR/取证场景的领域数据）。
                        run_tool_calls += tool_calls.len() as u32;
                        if !run_has_error {
                            for m in &history[hist_start..] {
                                if m.role == "tool" {
                                    if let Some(t) = m.content_as_text() {
                                        if is_structured_tool_error(&t) {
                                            run_has_error = true;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        // After tool execution, tracks repeated identical tool results and "no new
                        // state" windows. On stall it condenses duplicated results in history,
                        // injects an automatic strategy reconsideration, and (bounded) terminates
                        // gracefully with a summary if still stuck. No human intervention needed.
                        let tool_msgs = collect_tool_results_args(&history[hist_start..]);
                        let mut stalled = false;
                        if !tool_msgs.is_empty() {
                            // F3 3c: a round counts as "no new state" only if EVERY exact
                            // (name,args,result) triple it produced was already seen in the
                            // recent LRU window. This catches small cycles (A,B,A,B, period
                            // <= F3_RECENT_WINDOW) while an occasional short-result collision
                            // or same-result-repeated-with-different-args no longer reads as
                            // stagnation.
                            let mut round_any_new = false;
                            let mut round_fps: Vec<(u64, u64, u64)> = Vec::new();
                            for (name, args_dig, content, dig) in &tool_msgs {
                                let fp = (content_digest(name), *args_dig, *dig);
                                if !recent_results.iter().any(|t| *t == fp) {
                                    round_any_new = true;
                                }
                                round_fps.push(fp);
                                // F3 3a: key by (name, args_digest) so `Test-Path A` and
                                // `Test-Path B` never share a "same result" counter.
                                let entry = last_result.entry((name.clone(), *args_dig)).or_insert((*dig, 0));
                                if entry.0 == *dig {
                                    entry.1 += 1;
                                    // F3 3b: short outputs (small alphabet) carry less
                                    // information, so require twice as many before stalling.
                                    let threshold = if content.chars().count() < F3_SHORT_LIMIT {
                                        stall_repeat_threshold.saturating_mul(2)
                                    } else {
                                        stall_repeat_threshold
                                    };
                                    if entry.1 >= threshold {
                                        info!("[session:{}] Stall: '{}' (args {:016x}) returned identical result {}x", session_id, name, args_dig, entry.1);
                                        stalled = true;
                                    }
                                } else {
                                    entry.0 = *dig;
                                    entry.1 = 1;
                                }
                            }
                            for fp in &round_fps { recent_results.push_back(*fp); }
                            while recent_results.len() > F3_RECENT_WINDOW { recent_results.pop_front(); }

                            if round_any_new {
                                // Real progress this iteration: reset the per-tool
                                // identical-result counter so NON-consecutive repeats
                                // (e.g. re-reading an unchanged file between other work)
                                // do not accumulate into a false stall across the session.
                                last_result.clear();
                                no_new_state_iters = 0;
                                // F4 (v1.0.11): a genuine new state resets the reconsider
                                // tally, so only CONTIGUOUS no-progress stalls can terminate
                                // the run — 3 unrelated (possibly false-positive) stalls
                                // no longer kill a task that otherwise keeps progressing.
                                reconsider_events = 0;
                                // A result change exempts wait-for-change polling from rabbit.
                                prev_rabbit_batch = None;
                                rabbit_streak = 0;
                            } else if !stalled {
                                no_new_state_iters += 1;
                                if no_new_state_iters >= stall_repeat_threshold {
                                    info!("[session:{}] Stall: no new state for {} iterations", session_id, no_new_state_iters);
                                    stalled = true;
                                }
                            }
                        }

                        if stalled {
                            last_result.clear();
                            no_new_state_iters = 0;
                            reconsider_events += 1;
                            let deduped = dedup_tool_results(&mut history);
                            if deduped > 0 {
                                let _ = tx.send(Ok(AgentEvent::text(
                                    &format!("

*[Auto-stall: {} repeated result(s) merged - reconsidering from a clean state]*

", deduped),
                                    &invocation_id, &author
                                ))).await;
                            }

                            if reconsider_events >= MAX_AUTO_RECONSIDERS {
                                warn!("[session:{}] Auto-stall: no progress after {} reconsiderations; terminating with summary", session_id, reconsider_events);
                                let _ = tx.send(Ok(AgentEvent::text(
                                    &format!("

*[Auto-stall] No progress after {} reconsiderations - stopping with a best-effort summary. Send a new message to continue.*

", reconsider_events),
                                    &invocation_id, &author
                                ))).await;
                                let _ = tx.send(Ok(AgentEvent::done(&invocation_id, &author))).await;
                                for s in &cleanup_sessions { let _ = s.close().await; }
                                return;
                            }

                            history.push(ChatMessage::user(
                                "[AUTO-STALL] The previous tool call(s) are not making progress - the observed state is unchanged. \
                                 Reconsider the CORRECTNESS of your next tool call before acting:
                                 1. Re-validate the current goal and whether your interpretation of it is correct.
                                 2. Was the previous tool call actually valid here (wrong target, blocked by an overlay, or a misread result)?
                                 3. Adopt a MATERIALLY DIFFERENT approach: a different tool, different target/arguments, verify the state differently, or state concisely what is blocking you.
                                 Do NOT repeat the same tool call expecting a different result.",
                            ));
                            let _ = tx.send(Ok(AgentEvent::text(
                                &format!("

*[Auto-stall detected - reconsidering approach ({}/{})]*

", reconsider_events, MAX_AUTO_RECONSIDERS),
                                &invocation_id, &author
                            ))).await;
                            continue;
                        }

                        // ── Save checkpoint after tool execution ──
                        if let Some(ref cp) = checkpointer {
                            if let Some(ref cp_id) = checkpoint_id {
                                if let Err(e) = cp.save(
                                    cp_id, &session_id, &active_model,
                                    &user_message, &history, iteration,
                                ) {
                                    warn!("[session:{}] Failed to save checkpoint: {}", session_id, e);
                                } else {
                                    info!("[session:{}] Checkpoint saved at iteration {}", session_id, iteration);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("[session:{}] LLM error (model: {}): {}", session_id, active_model, e);
                        // Try fallback model if available and not already used
                        if !used_fallback {
                            if let Some(ref fb) = fallback_model {
                                warn!("[session:{}] Switching to fallback model: {}", session_id, fb);
                                let _ = tx.send(Ok(AgentEvent::text(
                                    &format!("\n\n*[Primary model failed, switching to {}]*\n\n", fb),
                                    &invocation_id, &author
                                ))).await;
                                active_model = fb.clone();
                                used_fallback = true;
                                continue; // Retry with fallback model
                            }
                        }
                        // Log run failed
                        if let Some(ref mut log) = event_log {
                            let _ = log.append(&LogEvent::RunFailed {
                                run_id: session_id.clone(),
                                timestamp: chrono::Utc::now(),
                                total_turns: iteration as u32 + 1,
                                error: e.to_string(),
                            });
                        }
                        let _ = tx.send(Ok(AgentEvent::error(&e, &invocation_id, &author))).await;
                        let _ = tx.send(Ok(AgentEvent::done(&invocation_id, &author))).await;
                        // Cleanup: close browser sessions after agent error
                        for s in &cleanup_sessions { let _ = s.close().await; }
                        return;
                    }
                }
            }

            // Max iterations reached - request final summary
            warn!("[session:{}] Max iterations ({}) reached", session_id, max_iter);
            let summary_prompt = format!(
                "The agent has reached the maximum number of iterations ({}) without completing. \
                 Please provide a final summary:\n\
                 1. What was accomplished so far\n\
                 2. What remains to be done\n\
                 3. What additional conditions, tools, or information would be needed to complete the task\n\
                 Be specific and helpful.",
                max_iter
            );
            history.push(ChatMessage::user(&summary_prompt));
            let mut summary_msgs = vec![ChatMessage::system(&system_prompt)];
            summary_msgs.extend(history.clone());
            match provider.chat_stream(&active_model, &summary_msgs, &[], tx.clone(), &invocation_id, &author).await {
                Ok((summary_content, _, _, _, _, stream_timed_out)) => {
                    // A cut summary round leaves a truncated prefix on the client; stream the static summary in full.
                    if stream_timed_out || summary_content.trim().is_empty() {
                        // LLM returned empty or was cut mid-transmission, send static summary
                        let fallback = generate_static_summary(&history, max_iter);
                        let _ = tx.send(Ok(AgentEvent::text(&fallback, &invocation_id, &author))).await;
                    }
                    // else: non-empty content was already streamed via chat_stream, don't re-send
                }
                Err(_) => {
                    let fallback = generate_static_summary(&history, max_iter);
                    let _ = tx.send(Ok(AgentEvent::text(&fallback, &invocation_id, &author))).await;
                }
            }
            // Log run completed (max iterations reached)
            if let Some(ref mut log) = event_log {
                let _ = log.append(&LogEvent::RunCompleted {
                    run_id: session_id.clone(),
                    timestamp: chrono::Utc::now(),
                    total_turns: max_iter as u32,
                    total_tokens: 0,
                    duration_ms: 0,
                });
            }
            let _ = tx.send(Ok(AgentEvent::done(&invocation_id, &author))).await;
            // Cleanup: close browser sessions after max iterations
            for s in &cleanup_sessions { let _ = s.close().await; }
        });

        // Convert mpsc Receiver into a Stream.
        // The per-run Orchestrator (if any) is released by `_orch_life` when the
        // spawned task ends — by any path — which drops its clone of `tx` so the
        // event channel closes and this stream terminates for the consumer. We
        // must NOT gate unregister on stream close: doing so is a self-deadlock
        // (the stream can only close once the registered Arc, which holds a `tx`
        // clone, is removed — but removal only runs after the stream closes).
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Box::pin(stream))
    }
}

/// Extract tool calls from the model's text content when it doesn't support
/// native function calling. Looks for:
/// 1. JSON code blocks: ```json {"name": "...", "arguments": {...}} ```
/// 2. Inline JSON objects: {"name": "...", "arguments": {...}}
/// Annotation injected in front of a user-interjection so the model treats it as
/// supplementary context (not a new task) and continues the current goal.
const INTERJECT_ANNOTATION: &str = "[User-injected supplementary context -- incorporate it and continue the current task; do NOT treat this as a new task and do NOT abandon or overwrite the work in progress]\n\n";

/// Drain user-injected "insert-now" interjections and push them into history as
/// annotated user turns (plan C+D: boundary injection + explicit context semantics).
/// Does NOT touch the pending follow-up queue (ordinary interjections).
fn inject_user_interjections(history: &mut Vec<ChatMessage>, session_id: &str) {
    let inter = crate::interject::drain_insert(session_id);
    if !inter.is_empty() {
        info!("[session:{}] Injecting {} interjection(s) at boundary", session_id, inter.len());
        for m in inter {
            history.push(ChatMessage::user(&format!("{}{}", INTERJECT_ANNOTATION, m)));
        }
    }
}

fn extract_tool_calls_from_content(content: &str) -> Vec<crate::model::ToolCallDelta> {
    use crate::model::{FunctionCallDelta, ToolCallDelta};
    let mut calls = Vec::new();
    let mut id_counter = 0u32;

    // Helper: try to parse a JSON string into a ToolCallDelta.
    // If strict parse fails, attempts to repair incomplete JSON (truncated by max_tokens).
    let try_parse = |json_str: &str, id_counter: &mut u32| -> Option<ToolCallDelta> {
        let trimmed = json_str.trim();
        let val: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                // Attempt repair: add missing closing braces/brackets
                let mut repaired = trimmed.to_string();
                let mut open_braces = 0i32;
                let mut open_brackets = 0i32;
                let mut in_str = false;
                let mut esc = false;
                for c in repaired.chars() {
                    if esc { esc = false; continue; }
                    if c == '\\' && in_str { esc = true; continue; }
                    if c == '"' { in_str = !in_str; continue; }
                    if in_str { continue; }
                    match c {
                        '{' => open_braces += 1,
                        '[' => open_brackets += 1,
                        '}' => open_braces -= 1,
                        ']' => open_brackets -= 1,
                        _ => {}
                    }
                }
                if in_str { repaired.push('"'); }
                for _ in 0..open_brackets { repaired.push(']'); }
                for _ in 0..open_braces { repaired.push('}'); }
                serde_json::from_str(&repaired).ok()?
            }
        };
        let name = val
            .get("name")
            .or_else(|| val.get("tool"))
            .or_else(|| val.get("function"))
            .and_then(|v| v.as_str())?;
        let args = val
            .get("arguments")
            .or_else(|| val.get("args"))
            .or_else(|| val.get("parameters"));
        let args_str = match args {
            Some(a) => serde_json::to_string(a).unwrap_or_else(|_| "{}".to_string()),
            None => "{}".to_string(),
        };
        let call_id = format!("textcall_{}", *id_counter);
        *id_counter += 1;
        info!("[text-tool-call] Extracted: {} ({})", name, args_str);
        Some(ToolCallDelta {
            id: call_id,
            call_type: "function".to_string(),
            function: FunctionCallDelta {
                name: Some(name.to_string()),
                arguments: Some(args_str),
            },
        })
    };

    // 1. Scan for ```json ... ``` code blocks
    let mut remaining = content;
    while let Some(start) = remaining.find("```") {
        remaining = &remaining[start + 3..];
        // Trim whitespace BEFORE checking for the json label — the model
        // often outputs ```\njson\n{...} (newline after backticks).
        remaining = remaining.trim_start();
        if remaining.starts_with("json") {
            remaining = &remaining[4..];
            remaining = remaining.trim_start();
        } else if remaining.starts_with("JSON") {
            remaining = &remaining[4..];
            remaining = remaining.trim_start();
        }
        if let Some(end) = remaining.find("```") {
            let json_str = &remaining[..end];
            if let Some(tc) = try_parse(json_str, &mut id_counter) {
                calls.push(tc);
            }
            remaining = &remaining[end + 3..];
        } else {
            // No closing fence — output may be truncated. Try to parse
            // whatever remains as JSON (with repair for incomplete braces).
            let json_str = remaining.trim();
            if !json_str.is_empty() {
                if let Some(tc) = try_parse(json_str, &mut id_counter) {
                    calls.push(tc);
                }
            }
            break;
        }
    }

    // 2. Scan for inline JSON objects like {"name": "...", "arguments": {...}}
    for marker in &["{\"name\"", "{\"tool\"", "{\"function\""] {
        let mut search_from = 0;
        while let Some(pos) = content[search_from..].find(marker) {
            let abs_pos = search_from + pos;
            if let Some(json_str) = extract_json_object(&content[abs_pos..]) {
                let is_in_codeblock = content[..abs_pos].rfind("```")
                    .map(|cb_start| {
                        let between = &content[cb_start..abs_pos];
                        between.matches("```").count() % 2 == 1
                    })
                    .unwrap_or(false);
                if !is_in_codeblock {
                    if let Some(tc) = try_parse(&json_str, &mut id_counter) {
                        calls.push(tc);
                    }
                }
                search_from = abs_pos + json_str.len();
            } else {
                search_from = abs_pos + marker.len();
            }
        }
    }

    calls
}

/// Extract a complete JSON object starting at the beginning of `text`.
/// Tracks brace depth and string state to handle nested objects.
/// If braces don't balance (truncated output), returns the text as-is
/// so the caller can attempt repair.
fn extract_json_object(text: &str) -> Option<String> {
    if !text.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut last_pos = 0usize;
    for (i, c) in text.char_indices() {
        last_pos = i;
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' && in_string {
            escape = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[..=i].to_string());
                }
            }
            _ => {}
        }
    }
    // Braces didn't balance — return what we have (may be truncated)
    if depth > 0 && last_pos > 0 {
        Some(text[..=last_pos].to_string())
    } else {
        None
    }
}

/// Classification of tool errors for retry decisions.
#[derive(Debug, Clone, PartialEq)]
enum ToolErrorClass {
    /// Transient error — worth retrying (timeout, network, resource busy).
    Retryable,
    /// Permanent error — retrying won't help (bad args, permission denied, not found).
    NonRetryable,
    /// User cancelled or consumer disconnected — do NOT retry.
    Cancelled,
}

/// Classify a tool execution error to decide whether to retry.
fn classify_tool_error(error_msg: &str) -> ToolErrorClass {
    let lower = error_msg.to_lowercase();

    // User-initiated cancellations — never retry
    if lower.contains("cancelled by user") || lower.contains("consumer disconnected") {
        return ToolErrorClass::Cancelled;
    }

    // Permission / auth errors — not retryable
    if lower.contains("permission denied") || lower.contains("unauthorized")
        || lower.contains("not allowed") || lower.contains("access denied") {
        return ToolErrorClass::NonRetryable;
    }

    // Argument / input errors — not retryable (same args will fail again)
    if lower.contains("missing") && lower.contains("parameter") {
        return ToolErrorClass::NonRetryable;
    }
    if lower.contains("unknown tool") || lower.contains("invalid argument") {
        return ToolErrorClass::NonRetryable;
    }

    // Timeout — retryable (transient resource contention)
    if lower.contains("timed out") || lower.contains("timeout") {
        return ToolErrorClass::Retryable;
    }

    // Network / IO transient errors — retryable
    if lower.contains("connection") || lower.contains("network")
        || lower.contains("temporarily unavailable") || lower.contains("resource busy")
        || lower.contains("too many open files") || lower.contains("deadlock")
        || lower.contains("broken pipe") || lower.contains("connection reset") {
        return ToolErrorClass::Retryable;
    }

    // Panics — retryable (might be transient state issue)
    if lower.contains("panicked") || lower.contains("panic") {
        return ToolErrorClass::Retryable;
    }

    // File not found — not retryable (file won't appear by itself)
    if lower.contains("not found") || lower.contains("does not exist") || lower.contains("no such file") {
        return ToolErrorClass::NonRetryable;
    }

    // Default: treat unknown errors as non-retryable to avoid wasted work
    ToolErrorClass::NonRetryable
}

/// Execute a single tool call with automatic retry for transient failures.
///
/// Spawns the tool's `execute()` as a child task and races it against:
/// - A heartbeat interval (sends `progress` events to the UI every 5s)
/// - A timeout (aborts the tool after `tool_timeout_secs`)
/// - Consumer disconnect (aborts immediately if the UI stops reading events)
///
/// On retryable errors (timeouts, network issues, panics), retries up to
/// `max_retries` times with exponential backoff (1s, 2s, 4s...). The LLM
/// receives enriched error messages indicating retry attempts.
/// Non-retryable errors (permission denied, bad arguments, not found) are
/// returned immediately without retry.
async fn execute_tool_call(
    tools: &tokio::sync::RwLock<ToolRegistry>,
    tc: &crate::model::ToolCallDelta,
    working_dir: &str,
    workspace_dir: &str,
    output_dir: &str,
    invocation_id: &str,
    author: &str,
    session_id: &str,
    tx: &tokio::sync::mpsc::Sender<AgentResult<AgentEvent>>,
    permission: &PermissionChecker,
    tool_timeout_secs: u64,
    max_retries: usize,
    context_window: usize,
    inline_scaling_enabled: bool,
    max_inline_chars: usize,
    mode: crate::context::AgentMode,
    depth: u8,
    can_spawn: bool,
    run_id: &str,
    mut event_log: Option<&mut EventLog>,
) -> ChatMessage {
    let tool_name = tc.function.name.as_deref().unwrap_or("unknown");
    let args_str = tc.function.arguments.as_deref().unwrap_or("{}");

    // Per-result protection cap: how much of one tool result is injected into
    // history/context. Scaled consistently with per-tool inline limits and
    // bounded by the absolute max_inline_chars protection cap, so a single or
    // concurrent batch of results cannot overflow the context window.
    let per_result_cap =
        crate::context::effective_inline_limit(40_000, context_window, inline_scaling_enabled, max_inline_chars);

    // On-demand peripheral tool schema resolution (keeps per-request payload small).
    if tool_name == "load_tool_schema" {
        let targs: serde_json::Value = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
        let target = targs["name"].as_str().unwrap_or("").to_string();
        let result = if target.is_empty() {
            serde_json::json!({ "error": "load_tool_schema: missing 'name'" })
        } else {
            let reg = tools.read().await;
            match reg.get_definition(&target) {
                Some(def) => serde_json::json!({ "tool": target, "loaded": true, "definition": def }),
                None => serde_json::json!({ "error": format!("Tool '{}' not found", target) }),
            }
        };
        return ChatMessage::tool_result(&tc.id, tool_name, &result.to_string());
    }

    let args: serde_json::Value = match serde_json::from_str(args_str) {
        Ok(v) => v,
        Err(e) => {
            warn!("Tool '{}' arguments JSON parse failed ({} chars, likely truncated): {}. Returning error to LLM.",
                  tool_name, args_str.len(), e);
            // Return parse error as tool result so the LLM can retry with correct JSON
            let _ = tx.send(Ok(AgentEvent::tool_call(tool_name, &tc.id, serde_json::json!({}), invocation_id, author))).await;
            let err_msg = format!(
                "ERROR: Tool call arguments could not be parsed (JSON malformed, likely truncated by output token limit). \
                 Error: {}. For large content, use file_write to save content to a file first, then pass the file path \
                 via 'content_file' parameter instead of inline 'content'.",
                e
            );
            let err_result = serde_json::json!({ "error": err_msg });
            let result_msg = ChatMessage::tool_result(&tc.id, tool_name, &err_result.to_string());
            return result_msg;
        }
    };

    // Log tool call started
    if let Some(ref mut log) = event_log {
        let _ = log.append(&LogEvent::ToolCallStarted {
            run_id: invocation_id.to_string(),
            timestamp: chrono::Utc::now(),
            turn_number: 0, // Will be filled by caller context if needed
            call_id: tc.id.clone(),
            tool_name: tool_name.to_string(),
            args: args.clone(),
        });
    }

    // Emit tool_call event
    let call_event = AgentEvent::tool_call(tool_name, &tc.id, args.clone(), invocation_id, author);
    let _ = tx.send(Ok(call_event)).await;

    // Check permission before executing
    let allowed = permission.check(tool_name, &args).await;

    let result = if !allowed {
        info!("Tool '{}' denied by user permission", tool_name);
        serde_json::json!({
            "error": format!(
                "PERMISSION DENIED: The user has denied the tool '{}' for this action. \
                 This decision is FINAL. You MUST NOT attempt to achieve the same result \
                 through alternative tools (e.g., shell_exec, PowerShell, CMD, or any other method). \
                 Respect the user's decision and inform them that the action was denied.",
                tool_name
            )
        })
    } else {
        // Retry loop for transient failures
        let mut attempt = 0usize;

        loop {
            // Look up the tool while holding the read lock briefly
            let tool = tools.read().await.get(tool_name);
            let tool_result = match tool {
                Some(tool) => {
                    // Get tool-level timeout (Phase 1: graded timeout policy)
                    // For Watchdog stage (timeout_secs() == None), use a very large hard
                    // timeout so the wall-clock never fires — the liveness watchdog is the
                    // sole abort mechanism for these tools (e.g., malware_deep, ir_memdump).
                    let effective_timeout_secs = tool.timeout_secs().unwrap_or_else(|| {
                        if tool.timeout_stage() == crate::tool::TimeoutStage::Watchdog {
                            24 * 3600 // 24 hours — effectively unlimited, watchdog governs
                        } else {
                            tool_timeout_secs
                        }
                    });
                    let watchdog_silence_secs = tool.timeout_stage().watchdog_silence_secs();
                    
                    // Create progress channel for long-running tools
                    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<String>(32);
                    let base = crate::context::ReadonlyContext::new(
                        invocation_id.to_string(), author.to_string(), session_id.to_string(),
                    );
                    let cb = crate::context::CallbackContext::new(base);
                    let mut ctx = ToolContext::new(
                        cb, tc.id.clone(), working_dir.to_string(), workspace_dir.to_string(),
                    )
                        .with_output_dir(output_dir.to_string())
                        .with_progress(progress_tx)
                        .with_inline_limits(context_window, inline_scaling_enabled, max_inline_chars);
                    ctx.mode = mode;
                    ctx.depth = depth;
                    ctx.can_spawn = can_spawn;
                    ctx.run_id = Some(run_id.to_string());
                    let args_clone = args.clone();

                    // Acquire an exclusive per-name lock for tools that opt into
                    // `Exclusivity::Exclusive` (long-lived CDP/WinRM sessions), and
                    // hold it across the whole tool execution so they never race.
                    // Bind the Arc first so the tokio guard borrows a live slot.
                    // §7.4.3: async acquire returns an OwnedMutexGuard held across
                    // the whole tool execution (drop => releases the slot).
                    let _excl_guard = if tool.exclusivity() == crate::agent::exclusivity::Exclusivity::Exclusive {
                        Some(crate::agent::exclusivity::global().acquire(tool_name).await)
                    } else {
                        None
                    };

                    // Spawn the actual tool execution as a separate task
                    let mut tool_handle = tokio::spawn(async move {
                        tool.execute(args_clone, &ctx).await
                    });

                    // Race: tool execution vs heartbeat vs timeout vs consumer disconnect
                    let timeout_duration = std::time::Duration::from_secs(effective_timeout_secs);
                    let heartbeat_interval = std::time::Duration::from_secs(5);
                    let start = std::time::Instant::now();
                    let mut interval = tokio::time::interval(heartbeat_interval);
                    interval.tick().await; // consume the immediate first tick

                    // Phase 1.4: Liveness watchdog — track last real progress time.
                    // If no progress_rx message for watchdog_silence_secs, abort.
                    let mut last_progress_time = start;

                    // Pin timeout future BEFORE the loop so it accumulates across iterations.
                    // Without pinning, the sleep is recreated every heartbeat tick and never fires.
                    let timeout_fut = tokio::time::sleep(timeout_duration);
                    tokio::pin!(timeout_fut);

                    loop {
                        tokio::select! {
                            // Tool execution completed
                            tool_result = &mut tool_handle => {
                                match tool_result {
                                    Ok(Ok(val)) => break Some(val),
                                    Ok(Err(e)) => {
                                        error!("Tool {} error: {}", tool_name, e);
                                        break Some(serde_json::json!({ "error": e.to_string() }));
                                    }
                                    Err(e) => {
                                        error!("Tool {} panicked: {}", tool_name, e);
                                        break Some(serde_json::json!({ "error": format!("Tool execution panicked: {}", e) }));
                                    }
                                }
                            }

                            // Progress message from tool (meaningful status updates)
                            Some(msg) = progress_rx.recv() => {
                                let elapsed = start.elapsed().as_secs();
                                // Phase 1.4: Reset watchdog timer on real progress
                                last_progress_time = std::time::Instant::now();
                                let progress = AgentEvent::progress(
                                    tool_name,
                                    &msg,
                                    elapsed,
                                    invocation_id,
                                    author,
                                );
                                if tx.send(Ok(progress)).await.is_err() {
                                    if !crate::agent::orchestration::has_inflight_workers(invocation_id) {
                                        info!("[session] Consumer disconnected during tool '{}', aborting", tool_name);
                                        tool_handle.abort();
                                        break Some(serde_json::json!({ "error": "Cancelled by user (consumer disconnected)" }));
                                    }
                                    // Orchestration resilience: consumer dropped but
                                    // sub-agents are still running — keep waiting for
                                    // this tool so wait_subagent can collect them.
                                    info!("[session] Consumer dropped during '{}' but sub-agents in flight; continuing", tool_name);
                                }
                            }

                            // Heartbeat: send progress event every 5 seconds (fallback if tool doesn't report)
                            _ = interval.tick() => {
                                let elapsed = start.elapsed().as_secs();
                                // Phase 1.4: Check liveness watchdog — abort if no real progress
                                let silence = last_progress_time.elapsed().as_secs();
                                if silence > watchdog_silence_secs {
                                    warn!("Tool '{}' watchdog triggered: no progress for {}s (threshold: {}s)", 
                                          tool_name, silence, watchdog_silence_secs);
                                    tool_handle.abort();
                                    break Some(serde_json::json!({ 
                                        "error": format!("Tool execution aborted: no progress for {}s (watchdog threshold: {}s). \
                                         The tool may be stuck or waiting for input. Consider using a narrower scope or different approach.", 
                                         silence, watchdog_silence_secs) 
                                    }));
                                }
                                let progress = AgentEvent::progress(
                                    tool_name,
                                    &format!("Still running... ({}s)", elapsed),
                                    elapsed,
                                    invocation_id,
                                    author,
                                );
                                if tx.send(Ok(progress)).await.is_err() {
                                    if !crate::agent::orchestration::has_inflight_workers(invocation_id) {
                                        info!("[session] Consumer disconnected during tool '{}', aborting", tool_name);
                                        tool_handle.abort();
                                        break Some(serde_json::json!({ "error": "Cancelled by user (consumer disconnected)" }));
                                    }
                                    info!("[session] Consumer dropped during '{}' but sub-agents in flight; continuing", tool_name);
                                }
                            }

                            // Consumer disconnected (STOP button)
                            _ = tx.closed() => {
                                if !crate::agent::orchestration::has_inflight_workers(invocation_id) {
                                    info!("Consumer disconnected during tool '{}', aborting", tool_name);
                                    tool_handle.abort();
                                    break Some(serde_json::json!({ "error": "Cancelled by user" }));
                                }
                                info!("[session] Consumer dropped during '{}' but sub-agents in flight; continuing wait", tool_name);
                            }

                            // Timeout (pinned — survives across loop iterations)
                            _ = &mut timeout_fut => {
                                warn!("Tool '{}' timed out after {}s", tool_name, timeout_duration.as_secs());
                                tool_handle.abort();
                                break Some(serde_json::json!({ "error": format!("Tool execution timed out after {}s", timeout_duration.as_secs()) }));
                            }
                        }
                    }
                }
                None => {
                    // Unknown tool — never retry
                    break serde_json::json!({ "error": format!("Unknown tool: {}", tool_name) });
                }
            };

            let result_val = match tool_result {
                Some(v) => v,
                None => serde_json::json!({ "error": "Tool execution returned no result" }),
            };

            // Check if this is an error that should be retried
            if let Some(err_val) = result_val.get("error") {
                let err_msg = err_val.as_str().unwrap_or("unknown error").to_string();
                let classification = classify_tool_error(&err_msg);

                match classification {
                    ToolErrorClass::Cancelled => {
                        // Never retry cancellations
                        break result_val;
                    }
                    ToolErrorClass::NonRetryable => {
                        // Don't retry, but enrich the error with context if we already retried
                        if attempt > 0 {
                            break serde_json::json!({
                                "error": err_msg,
                                "retry_info": format!("Failed after {} attempt(s). This error is not retryable.", attempt + 1)
                            });
                        }
                        break result_val;
                    }
                    ToolErrorClass::Retryable => {
                        attempt += 1;
                        if attempt > max_retries {
                            warn!("Tool '{}' failed after {} attempts, giving up", tool_name, attempt);
                            break serde_json::json!({
                                "error": err_msg,
                                "retry_info": format!("Exhausted {} retry attempt(s). Last error: {}", max_retries, err_msg)
                            });
                        }
                        let backoff_secs = 1u64 << (attempt - 1); // 1s, 2s, 4s, ...
                        warn!("Tool '{}' failed (attempt {}/{}), retrying in {}s: {}",
                              tool_name, attempt, max_retries, backoff_secs, err_msg);
                        // Notify the UI about the retry
                        let retry_event = AgentEvent::progress(
                            tool_name,
                            &format!("Retry {}/{} after {}s (error: {})", attempt, max_retries, backoff_secs, err_msg),
                            0,
                            invocation_id,
                            author,
                        );
                        let _ = tx.send(Ok(retry_event)).await;
                        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                        // Continue the loop to retry
                        continue;
                    }
                }
            } else {
                // Success — no error field
                break result_val;
            }
        }
    };

    // Log tool call completed
    let success = !result.get("error").is_some();
    if let Some(ref mut log) = event_log {
        let _ = log.append(&LogEvent::ToolCallCompleted {
            run_id: invocation_id.to_string(),
            timestamp: chrono::Utc::now(),
            turn_number: 0, // Will be filled by caller context if needed
            call_id: tc.id.clone(),
            tool_name: tool_name.to_string(),
            result: result.clone(),
            success,
            duration_ms: 0, // Duration tracking can be added later if needed
        });
    }

    // Emit tool_result event (full result to UI)
    let result_event = AgentEvent::tool_result(tool_name, &tc.id, result.clone(), invocation_id, author);
    let _ = tx.send(Ok(result_event)).await;

    // Build the history entry with size cap (max ~30000 chars per result to prevent context overflow)
    let result_str = serde_json::to_string(&result).unwrap_or_default();
    let history_str = if result_str.len() > per_result_cap {
        let preview: String = result_str.chars().take(per_result_cap).collect();
        format!("{}\n\n... [truncated, original size: {} chars]", preview, result_str.len())
    } else {
        result_str
    };
    ChatMessage::tool_result(&tc.id, tool_name, &history_str)
}

/// Run a batch of tool calls concurrently and return their result messages in
/// the original (input) order. Only safe for read-only / concurrency-safe tools.
/// Note: Event logging is not supported in concurrent execution mode to avoid
/// mutable reference conflicts. Use sequential execution for full event logging.
async fn execute_tools_concurrent<'a>(
    tools: &'a tokio::sync::RwLock<ToolRegistry>,
    tool_calls: &'a [crate::model::ToolCallDelta],
    working_dir: &'a str,
    workspace_dir: &'a str,
    output_dir: &'a str,
    invocation_id: &'a str,
    author: &'a str,
    session_id: &'a str,
    tx: &'a tokio::sync::mpsc::Sender<AgentResult<AgentEvent>>,
    permission: &'a PermissionChecker,
    tool_timeout_secs: u64,
    max_retries: usize,
    context_window: usize,
    inline_scaling_enabled: bool,
    max_inline_chars: usize,
    mode: crate::context::AgentMode,
    depth: u8,
    can_spawn: bool,
    run_id: &'a str,
) -> Vec<ChatMessage> {
    use futures::future::join_all;
    let futs = tool_calls.iter().map(|tc| {
        execute_tool_call(tools, tc, working_dir, workspace_dir, output_dir, invocation_id, author, session_id, tx, permission, tool_timeout_secs, max_retries, context_window, inline_scaling_enabled, max_inline_chars, mode, depth, can_spawn, run_id, None)
    });
    join_all(futs).await
}

/// Rabbit-hole detection: tracks how many times a tool was called with the same
/// signature. Returns `Some((count, warning_text))` when the threshold is
/// reached (and resets the counter so it can trigger again later).
/// Stable-enough content digest for a tool result (whitespace-trimmed hash).
/// Used by the automatic stall detector to recognize "the same state came back".
/// Names of peripheral tools the model has loaded via `load_tool_schema`,
/// derived from history so the session stays stateless.
fn revealed_tool_names(history: &[ChatMessage]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for m in history {
        if m.role == "tool" && m.name.as_deref() == Some("load_tool_schema") {
            if let Some(txt) = m.content_as_text() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                    if let Some(n) = v.get("tool").and_then(|x| x.as_str()) {
                        let n = n.to_string();
                        if !names.contains(&n) {
                            names.push(n);
                        }
                    }
                }
            }
        }
    }
    names
}

fn content_digest(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.trim().hash(&mut h);
    h.finish()
}

/// Collect (name, args_digest, content, content_digest) from tool-role messages.
/// Arguments are resolved by matching each tool result's `tool_call_id` to the
/// corresponding assistant tool-call, so the same tool with different arguments
/// keys separately (F3 3a). When arguments cannot be resolved, args_digest falls
/// back to the empty-object digest — grouping unknown-args results by name only,
/// which is no worse than the pre-F3 behavior.
fn collect_tool_results_args(messages: &[ChatMessage]) -> Vec<(String, u64, String, u64)> {
    let mut args_by_id: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in messages {
        if m.role == "assistant" {
            if let Some(calls) = &m.tool_calls {
                for tc in calls {
                    args_by_id.insert(
                        tc.id.clone(),
                        tc.function.arguments.clone().unwrap_or_else(|| "{}".to_string()),
                    );
                }
            }
        }
    }
    let fallback = content_digest("{}");
    let mut out = Vec::new();
    for m in messages {
        if m.role == "tool" {
            if let Some(txt) = m.content_as_text() {
                let name = m.name.clone().unwrap_or_else(|| "tool".to_string());
                let args_dig = m.tool_call_id
                    .as_ref()
                    .and_then(|id| args_by_id.get(id))
                    .map(|a| content_digest(a))
                    .unwrap_or(fallback);
                let content_dig = content_digest(&txt);
                out.push((name, args_dig, txt, content_dig));
            }
        }
    }
    out
}

/// Deduplicate repeated identical tool results in history (keep the LAST
/// occurrence of each distinct (name, content-digest), replace earlier ones
/// with a short placeholder). Returns how many were replaced.
fn dedup_tool_results(history: &mut Vec<ChatMessage>) -> usize {
    let mut seen: std::collections::HashMap<(String, u64), usize> = std::collections::HashMap::new();
    for (i, m) in history.iter().enumerate() {
        if m.role == "tool" {
            if let Some(txt) = m.content_as_text() {
                let name = m.name.clone().unwrap_or_else(|| "tool".to_string());
                seen.insert((name, content_digest(&txt)), i);
            }
        }
    }
    let mut replaced = 0usize;
    for (i, m) in history.iter_mut().enumerate() {
        if m.role != "tool" { continue; }
        let Some(txt) = m.content_as_text() else { continue; };
        let name = m.name.clone().unwrap_or_else(|| "tool".to_string());
        let key = (name.clone(), content_digest(&txt));
        let last_idx = seen[&key];
        if i != last_idx {
            m.content = Some(Value::String(format!(
                "[{} returned the same result - merged earlier duplicate; see the last result above]",
                name
            )));
            replaced += 1;
        }
    }
    replaced
}

/// Build an order-insensitive signature for a whole tool-call batch: the union of
/// `name:args` pairs, sorted and joined. Two batches are "identical" iff their
/// signatures match regardless of call order, so reordering is not a false loop.
fn build_batch_signature(tool_calls: &[crate::model::ToolCallDelta]) -> String {
    let mut parts: Vec<String> = tool_calls
        .iter()
        .map(|tc| {
            format!(
                "{}:{}",
                tc.function.name.as_deref().unwrap_or("unknown"),
                tc.function.arguments.as_deref().unwrap_or("{}"),
            )
        })
        .collect();
    parts.sort();
    parts.join(" | ")
}

/// Rabbit-hole detection with CONTIGUOUS semantics: only consecutive rounds that emit
/// the exact same batch (plus, via the result-reset in the loop, no state change)
/// accumulate. A different batch breaks the streak and restarts from 1. Returns the
/// streak count once it crosses `threshold`, then resets the streak.
fn rabbit_hole_check(
    prev_batch: &mut Option<String>,
    streak: &mut usize,
    signature: &str,
    threshold: usize,
) -> Option<usize> {
    if prev_batch.as_deref() == Some(signature) {
        *streak += 1;
    } else {
        *prev_batch = Some(signature.to_string());
        *streak = 1;
    }
    if *streak >= threshold {
        let c = *streak;
        *prev_batch = None;
        *streak = 0;
        Some(c)
    } else {
        None
    }
}

/// Generate a static summary from tool results in history (fallback when LLM summary also fails).
fn generate_static_summary(history: &[ChatMessage], iterations: usize) -> String {
    let mut tool_results: Vec<(String, String)> = Vec::new();
    let mut has_errors = false;
    let mut has_denied = false;

    for msg in history {
        if msg.role == "tool" {
            let content_str = msg.content_as_text().unwrap_or_default();
            let name = msg.name.as_deref().unwrap_or("tool");
            let preview: String = content_str.chars().take(200).collect();
            if content_str.contains("error") || content_str.contains("Error") {
                has_errors = true;
            }
            if content_str.contains("denied") || content_str.contains("Denied") {
                has_denied = true;
            }
            tool_results.push((name.to_string(), preview));
        }
    }

    let mut parts: Vec<String> = Vec::new();

    if tool_results.is_empty() {
        parts.push(format!("**Task Status: Incomplete** — Processed {} iterations with no tool activity.\n\nThe task could not be completed. You may need to:\n- Provide more specific instructions\n- Check that the required tools are available\n- Verify API connectivity", iterations));
    } else {
        // Determine overall status
        if has_errors || has_denied {
            parts.push("## \u{274c} Task Failed\n".to_string());
            parts.push(format!("The task was not completed successfully after {} iterations and {} tool call(s).\n", iterations, tool_results.len()));
            parts.push("### What happened:\n".to_string());
            for (i, (name, preview)) in tool_results.iter().enumerate() {
                parts.push(format!("{}. **{}**: {}", i + 1, name, preview));
            }
            parts.push("\n### What you may need to retry:\n".to_string());
            if has_errors {
                parts.push("- Some tool executions returned errors. Review the results above for specific failure reasons.".to_string());
            }
            if has_denied {
                parts.push("- Some operations were denied by permission settings. Adjust permissions in Settings if needed.".to_string());
            }
            parts.push("- Consider providing more context or breaking the task into smaller steps.".to_string());
        } else {
            parts.push("## \u{2705} Task Completed\n".to_string());
            parts.push(format!("Processed across {} iterations with {} tool call(s).\n", iterations, tool_results.len()));
            parts.push("### Results:\n".to_string());
            for (i, (name, preview)) in tool_results.iter().enumerate() {
                parts.push(format!("{}. **{}**: {}", i + 1, name, preview));
            }
        }
    }

    parts.join("\n")
}

/// Layered-routing core (no `self`, unit-testable). When a task-matched SKILL is already
/// driving this turn, do NOT inject a competing SOP (the skill wins). Otherwise match SOP
/// tags and replay the best one.
/// C1：结构化工具错误判定 —— 只认工具执行返回的"错误信封"（JSON 顶层 error 键：
/// 超时 / 中止 / panic / 未知工具 / 重试耗尽等硬失败），不扫正文里的 error/failed 子串
/// （那些是 IR/取证场景的领域数据，会把成功流程误判为失败）。
fn is_structured_tool_error(text: &str) -> bool {
    use serde_json::Value;
    let t = text.trim();
    if !t.starts_with('{') {
        return false;
    }
    match serde_json::from_str::<Value>(t) {
        Ok(Value::Object(map)) => {
            // D2（原 C1）：error 键任意类型（字符串/对象/数组/数字/布尔）都视为失败，
            // 消除 `{"error":{...}}` / `{"error":[]}` 等对象型/数组型漏检。
            if map.contains_key("error") {
                return true;
            }
            // status == "error" / "failed"
            if let Some(Value::String(s)) = map.get("status") {
                let lo = s.to_ascii_lowercase();
                if lo == "error" || lo == "failed" {
                    return true;
                }
            }
            // is_error == true
            if map.get("is_error").and_then(Value::as_bool) == Some(true) {
                return true;
            }
            // success / ok == false
            for k in ["success", "ok"] {
                if map.get(k).and_then(Value::as_bool) == Some(false) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}
fn sop_reminder_for(query: &str, task_skill_active: bool, workspace_dir: &str) -> Option<(String, String)> {
    if task_skill_active {
        return None;
    }
    let q: String = query.trim().chars().take(300).collect();
    if q.chars().count() < 4 {
        return None;
    }
    let sops = crate::sop::load_sops(workspace_dir);
    if sops.is_empty() {
        return None;
    }
    let candidates = crate::sop::match_sops(&sops, &q);
    if candidates.is_empty() {
        return None;
    }
    let now = crate::sop::now_secs();
    let best = crate::sop::select_best(&candidates, now, 12000)?;
    // H3：超限 SOP 降级为目录档渲染，不再静默丢弃。
    let ctx = if best.rough_tokens() > 12000 / 4 {
        crate::sop::format_sop_catalog(&best)
    } else {
        crate::sop::format_sop_context(&best)
    };
    crate::sop::bump_metric(workspace_dir, "replay_hits", 1);
    info!("[sop] replay hit: matched SOP '{}' for task '{}'", best.name, q);
    Some((
        format!(
            "\n\n## Guided SOP (auto-matched — replay this verified procedure)\n\
             A stored SOP matches this task. Follow its Phases in order instead of re-deriving \
             the procedure; only deviate where current conditions differ. After completing, note \
             any observed deviation/success so the SOP can be refined.\n\n{}",
            ctx
        ),
        best.id.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_gate_accepts_ir_and_rejects_mixed() {
        fn tc(name: &str) -> crate::model::ToolCallDelta {
            crate::model::ToolCallDelta {
                id: format!("c-{name}"),
                call_type: "function".to_string(),
                function: crate::model::FunctionCallDelta {
                    name: Some(name.to_string()),
                    arguments: Some("{}".to_string()),
                },
            }
        }
        // IR collection batch -> parallel fast path
        assert!(is_ir_collection_batch(&[tc("ir_scan"), tc("ir_process")]));
        // single call -> never parallel
        assert!(!is_ir_collection_batch(&[tc("ir_scan")]));
        // mixed with a non-collection tool -> not an IR batch
        assert!(!is_ir_collection_batch(&[tc("ir_scan"), tc("file_read")]));
    }

    #[test]
    fn pure_greeting_detection_is_strict() {
        // Pure greetings -> Minimal tier
        for g in ["你好", "您好!", "hi", "Hello!", "good morning", "早上好", "hi hi", "hey"] {
            assert!(is_pure_greeting(g), "should be greeting: {g}");
            assert_eq!(PromptTier::select(g), PromptTier::Minimal);
        }
        // Anything task-like -> Full tier, even when it starts with a greeting
        for t in ["hi, what's my IP", "你好，帮我查一下进程", "hello world rust",
                  "在吗？帮我看看这个日志", "good morning please check disk"] {
            assert!(!is_pure_greeting(t), "should NOT be greeting: {t}");
            assert_eq!(PromptTier::select(t), PromptTier::Full);
        }
        // Long messages are never Minimal
        let long = "hi ".repeat(30);
        assert!(!is_pure_greeting(&long));
    }

    #[test]
    fn minimal_prompt_is_strict_prefix_of_full() {
        // Cache-nesting contract: the Minimal-tier prompt must be a byte-prefix
        // of the Full-tier prompt so provider prefix caching survives tier
        // switches.
        let provider = std::sync::Arc::new(crate::model::openai::OpenAiProvider::new(vec![]));
        let tools = std::sync::Arc::new(tokio::sync::RwLock::new(crate::tool::ToolRegistry::new()));
        let agent = LlmAgent::builder().provider(provider).tools(tools).build().expect("agent build");
        let (minimal, _) = agent.build_system_prompt(
            PromptTier::Minimal, "hi", &[],
            crate::skill::SkillListingStrategy::Query, 6000, 40, 3,
        );
        let (full, _) = agent.build_system_prompt(
            PromptTier::Full, "hi", &[],
            crate::skill::SkillListingStrategy::Query, 6000, 40, 3,
        );
        assert!(full.starts_with(&minimal), "Minimal must be a strict prefix of Full");
        assert!(full.len() > minimal.len());
        // Stable-head contract: no date / language rule in the prompt head
        // (they travel in the trailing volatile state message).
        assert!(!minimal.contains("LANGUAGE RULE"));
        assert!(!full.contains("Current date:"));
    }

    /// 未发生传输层截断时，分类函数不得介入，避免正常回合被误判为需要补救。
    #[test]
    fn stream_cut_not_classified_when_stream_intact() {
        assert_eq!(classify_stream_cut(false, 0, 0, 1), None);
        assert_eq!(classify_stream_cut(false, 3, 0, 1), None);
    }

    /// 有预算时按“继续收尾”处理，并如实报告丢弃的残缺工具调用数量。
    #[test]
    fn stream_cut_continues_with_budget() {
        assert_eq!(
            classify_stream_cut(true, 0, 0, 1),
            Some(StreamCutRecovery::Continue { dropped_tool_calls: 0 })
        );
        assert_eq!(
            classify_stream_cut(true, 2, 0, 1),
            Some(StreamCutRecovery::Continue { dropped_tool_calls: 2 })
        );
    }

    /// 预算耗尽时不得再补一轮，改判为失败结局（供 SOP 学习与人工排查）。
    #[test]
    fn stream_cut_exhausts_budget() {
        assert_eq!(classify_stream_cut(true, 0, 1, 1), Some(StreamCutRecovery::Exhausted));
        assert_eq!(classify_stream_cut(true, 5, 3, 1), Some(StreamCutRecovery::Exhausted));
    }

    /// 连续截断回合不得推进重复计数，其后一个完整回合也不得误触发自动停止。
    #[test]
    fn stream_cut_does_not_trip_text_loop_autostop() {
        let mut last = 0u64;
        let mut streak = 0usize;
        for _ in 0..10 {
            assert!(!should_auto_stop_text_loop(true, 42, &mut last, &mut streak, 6));
        }
        assert_eq!(streak, 0);
        // 首个完整回合建立起计数，不应立即停止。
        assert!(!should_auto_stop_text_loop(false, 42, &mut last, &mut streak, 6));
        assert_eq!(streak, 1);
    }

    /// 未截断时，相同文本重复达到上限仍会正常自动停止。
    #[test]
    fn intact_repeat_still_triggers_text_loop_autostop() {
        let mut last = 0u64;
        let mut streak = 0usize;
        let mut stopped = false;
        for _ in 0..5 {
            stopped = should_auto_stop_text_loop(false, 7, &mut last, &mut streak, 6);
            assert!(!stopped);
        }
        stopped = should_auto_stop_text_loop(false, 7, &mut last, &mut streak, 6);
        assert!(stopped);
        assert_eq!(streak, 6);
    }

    /// 截断回合一经出现即切断重复序列：之前累计的计数被清零。
    #[test]
    fn cut_round_resets_accumulated_streak() {
        let mut last = 0u64;
        let mut streak = 0usize;
        for _ in 0..5 {
            should_auto_stop_text_loop(false, 9, &mut last, &mut streak, 6);
        }
        assert_eq!(streak, 5);
        assert!(!should_auto_stop_text_loop(true, 9, &mut last, &mut streak, 6));
        assert_eq!(streak, 0);
        // 后续完整重复必须从 1 重新起算，停在旧计数上属于误停。
        assert!(!should_auto_stop_text_loop(false, 9, &mut last, &mut streak, 6));
        assert_eq!(streak, 1);
    }

    fn hist_tokens(h: &[ChatMessage]) -> usize {
        h.iter().map(|m| estimate_tokens(m.content_as_text().as_deref().unwrap_or("") )).sum()
    }

    /// 有限脑 §12.6：价值导向裁剪必须把不可再生证据降级为「指针」并保留哈希/路径，
    /// 而可再生的旧 chatter 才被激进压缩；最近 6 条永不触碰。
    #[test]
    fn value_trim_protects_evidence() {
        let hash = "a".repeat(64);
        let evidence = format!("Process list dump C:\\ir\\mem.raw {} {}", hash, "x".repeat(600));
        let mut history: Vec<ChatMessage> = Vec::new();
        // 6 条旧消息：一条 ir_ 证据工具结果 + 可再生的 assistant chatter
        history.push(ChatMessage::tool_result("c1", "ir_scan", &evidence));
        for _ in 0..5 {
            history.push(ChatMessage::assistant(&"filler chatter ".repeat(60)));
        }
        // 6 条最近消息（应受保护）
        for i in 0..6 {
            history.push(ChatMessage::assistant(&format!("recent {}", i)));
        }
        let before = hist_tokens(&history);
        let budget = before / 3; // 强制激进裁剪
        trim_history_by_value(&mut history, budget);

        // 证据以指针形式存活，路径与哈希均保留
        let ev_text = history[0].content_as_text().unwrap();
        assert!(ev_text.contains("C:\\ir\\mem.raw"), "evidence file path must survive: {}", ev_text);
        assert!(ev_text.contains(&hash), "evidence hash must survive: {}", ev_text);
        assert!(ev_text.contains("pointer"), "evidence should degrade to pointer, not be elided: {}", ev_text);
        // 最近消息未被触碰
        assert_eq!(history[history.len() - 1].content_as_text().unwrap(), "recent 5");
        // 总体降到预算以内
        let after = hist_tokens(&history);
        assert!(after <= budget, "after {} should be <= budget {}", after, budget);
    }

    /// 有限脑 §12.6：无需裁剪时不改变任何消息。
    #[test]
    fn value_trim_noop_under_budget() {
        let mut history: Vec<ChatMessage> = Vec::new();
        for i in 0..4 {
            history.push(ChatMessage::assistant(&format!("short {}", i)));
        }
        let snapshot: Vec<Option<Value>> = history.iter().map(|m| m.content.clone()).collect();
        trim_history_by_value(&mut history, usize::MAX);
        for (i, m) in history.iter().enumerate() {
            assert_eq!(m.content, snapshot[i], "message {} must be untouched when under budget", i);
        }
    }

    fn tmp_ws(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("rustagent_llm_todo_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    fn write_todos(ws: &str, items: Vec<(String, String)>) {
        let arr: Vec<serde_json::Value> = items
            .iter()
            .map(|(d, s)| serde_json::json!({ "description": d, "status": s, "started_at": serde_json::Value::Null }))
            .collect();
        let v = serde_json::json!({ "items": arr });
        let p = std::path::Path::new(ws).join("todos.json");
        std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }

    #[test]
    fn todo_block_injected_with_items_and_rules() {
        let ws = tmp_ws("block");
        write_todos(&ws, vec![("a".into(), "pending".into()), ("b".into(), "completed".into())]);
        let block = LlmAgent::build_todo_context_block(&ws, 600).unwrap();
        assert!(block.contains("0. [pending] a"));
        assert!(block.contains("1. [completed] b"));
        assert!(block.contains("600-second timeout"));
        assert!(block.contains("Progress ONE item at a time, in list order"));
        assert!(block.contains("previous task was not completed"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn todo_block_none_when_empty() {
        let ws = tmp_ws("empty");
        assert!(LlmAgent::build_todo_context_block(&ws, 600).is_none());
        std::fs::write(std::path::Path::new(&ws).join("todos.json"), r#"{"items":[]}"#).unwrap();
        assert!(LlmAgent::build_todo_context_block(&ws, 600).is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn todo_watchdog_skips_stale_in_progress() {
        let ws = tmp_ws("watch");
        let v = serde_json::json!({ "items": [
            {"description":"stuck","status":"in_progress","started_at": 1},
            {"description":"next","status":"pending","started_at": serde_json::Value::Null},
        ]});
        let p = std::path::Path::new(&ws).join("todos.json");
        std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        let note = LlmAgent::apply_todo_timeout(&ws, 600).unwrap();
        assert!(note.contains("auto-marked 'skipped'"));
        let raw = std::fs::read_to_string(&p).unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(root["items"][0]["status"], "skipped");
        assert!(root["items"][0]["started_at"].is_null());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn todo_watchdog_no_skip_when_within_timeout() {
        let ws = tmp_ws("watch2");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let v = serde_json::json!({ "items": [
            {"description":"fresh","status":"in_progress","started_at": now}
        ]});
        let p = std::path::Path::new(&ws).join("todos.json");
        std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        assert!(LlmAgent::apply_todo_timeout(&ws, 600).is_none());
        // still in_progress
        let raw = std::fs::read_to_string(&p).unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(root["items"][0]["status"], "in_progress");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn history_has_todo_detects_tool_call_and_result() {
        // result message
        let res = ChatMessage::tool_result("id1", "todo_update", "ok");
        assert!(LlmAgent::history_has_todo(&[res]));

        // assistant tool call
        let call = ChatMessage::assistant_with_tool_calls(vec![
            crate::model::ToolCallDelta {
                id: "t1".into(),
                call_type: "function".into(),
                function: crate::model::FunctionCallDelta {
                    name: Some("todo_update".into()),
                    arguments: Some("{}".into()),
                },
            },
        ]);
        assert!(LlmAgent::history_has_todo(&[call]));

        // unrelated tool / text -> false
        let other = ChatMessage::tool_result("id2", "shell_exec", "x");
        let text = ChatMessage::user("hi");
        assert!(!LlmAgent::history_has_todo(&[other, text]));
    }

    #[test]
    fn todo_reminder_emitted_when_list_active_and_absent_when_empty() {
        let ws = tmp_ws("remind");
        write_todos(&ws, vec![("a".into(), "in_progress".into())]);
        let rem = LlmAgent::build_todo_reminder(&ws).unwrap();
        assert!(rem.contains("action='list'"));
        // empty -> None
        std::fs::write(std::path::Path::new(&ws).join("todos.json"), r#"{"items":[]}"#).unwrap();
        assert!(LlmAgent::build_todo_reminder(&ws).is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn sop_reminder_suppressed_when_skill_active() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap().to_string();
        let sop = crate::sop::Sop {
            id: "s1".to_string(),
            name: "Cleanup Run".to_string(),
            description: "Guided cleanup procedure".to_string(),
            semantic_tags: vec!["cleanup".to_string(), "rm".to_string()],
            phases: vec!["枚举".to_string(), "删除".to_string(), "验证".to_string()],
            version: 1,
            created: 1,
            updated: 1,
            times_executed: 2,
            times_succeeded: 2,
            times_failed: 0,
            last_executed_at: Some(1),
            avg_tool_calls: 3.0,
            avg_duration_secs: 10.0,
        };
        crate::sop::register_sop(&ws, sop).unwrap();

        let task = "please cleanup temp dir and remove leftover files";
        // No task-matched SKILL driving the turn → SOP is replayed.
        let (reminded, _sop_id) = sop_reminder_for(task, false, &ws)
            .expect("SOP should replay when no skill is active");
        assert!(reminded.contains("Phase 3"), "expected 3 phases, got: {}", reminded);
        // A task-matched SKILL driving the turn → SOP is fully suppressed (layered routing).
        assert!(sop_reminder_for(task, true, &ws).is_none(),
            "SOP must not inject when a skill is in control");
        // Non-matching task → no replay.
        assert!(sop_reminder_for("do something unrelated entirely", false, &ws).is_none());
    }

    #[test]
    fn structured_tool_error_signal_ignores_domain_text() {
        // C1：正文里出现 error/failed/denied 是 IR/取证领域的领域数据，不视为流程失败。
        assert!(!is_structured_tool_error("collected 200 lines; failed logins: 12; error_count=0"));
        assert!(!is_structured_tool_error("plain text output with DENIED in it"));
        assert!(!is_structured_tool_error(r#"[not json] error"#));
        // 只有结构化"错误信封"（JSON 顶层 error 键）才计为真实工具失败。
        assert!(is_structured_tool_error(r#"{"error": "Tool execution timed out after 120s"}"#));
        assert!(is_structured_tool_error(r#"{"error":"unknown tool: foo"}"#));
        assert!(!is_structured_tool_error(r#"{"ok":true,"count":3}"#));
        // D2：扩展识别——error 任意类型 / status=error / is_error=true / success=false。
        assert!(is_structured_tool_error(r#"{"status":"error"}"#));
        assert!(is_structured_tool_error(r#"{"status":"failed"}"#));
        assert!(is_structured_tool_error(r#"{"is_error":true}"#));
        assert!(is_structured_tool_error(r#"{"success":false}"#));
        assert!(is_structured_tool_error(r#"{"ok":false}"#));
        assert!(is_structured_tool_error(r#"{"error":{"code":7,"msg":"boom"}}"#));
        assert!(is_structured_tool_error(r#"{"error":[]}"#));
        // 反向：正常成功信封不应误判为失败。
        assert!(!is_structured_tool_error(r#"{"status":"ok","count":3}"#));
        assert!(!is_structured_tool_error(r#"{"success":true}"#));
        assert!(!is_structured_tool_error(r#"{"is_error":false}"#));
    }

    // ---- Step 1 gates (SDD v1.5) ----

    /// G-gate-truth + G-instant-root: with an empty allowset the delivery gate
    /// strips every orchestration tool, and opening a mode to the allowset
    /// delivers it again.
    #[test]
    fn gate_gate_truth_open_and_closed() {
        let empty: Vec<String> = Vec::new();
        for name in ALL_ORCH.iter() {
            assert!(!orchestration_delivered(name, &empty), "{} must be hidden with empty allowset", name);
        }
        let open_set = vec!["spawn_subagent".to_string()];
        assert!(orchestration_delivered("spawn_subagent", &open_set));
        assert!(!orchestration_delivered("wait_subagent", &open_set));
        assert!(orchestration_delivered("file_read", &empty));
        // Step 2a opens the delivery gate for the Instant root only (depth 0).
        assert_eq!(orchestration_allowset(crate::context::AgentMode::Instant, 0).len(), ALL_ORCH.len());
        assert!(orchestration_allowset(crate::context::AgentMode::Expert, 0).is_empty());
        assert!(orchestration_allowset(crate::context::AgentMode::Expert, 1).is_empty());
        assert!(orchestration_allowset(crate::context::AgentMode::Instant, 1).is_empty());
    }

    /// G-open-gate-truth: a mode explicitly opened to the allowset delivers each
    /// orchestration tool; an unopened mode hides them all.
    #[test]
    fn gate_open_delivers_all_after_step2a() {
        let allow = orchestration_allowset(crate::context::AgentMode::Instant, 0);
        for name in ALL_ORCH.iter() {
            assert!(orchestration_delivered(name, &allow), "{} should be delivered to Instant root", name);
        }
        // Workers / Expert: hidden.
        let empty: Vec<String> = Vec::new();
        assert!(!orchestration_delivered("spawn_subagent", &empty));
    }

    /// G-name-disjoint: orchestration names must not collide with skill tool names.
    #[test]
    fn gate_name_disjoint() {
        let skill_names = crate::skill::SkillManager::skill_tool_names();
        for name in ALL_ORCH.iter() {
            assert!(!skill_names.iter().any(|s| s.as_str() == *name), "orchestration tool {} collides with a skill tool", name);
        }
    }

    /// G-sub-not-main: sub/cron sessions are never treated as the main session.
    #[test]
    fn gate_sub_not_main() {
        assert!(is_main_session("abc-123"));
        assert!(!is_main_session(""));
        assert!(!is_main_session("sub-xx"));
        assert!(!is_main_session("cron-midnight"));
        assert_eq!(
            crate::context::SessionKind::from_session_id("sub-abc"),
            crate::context::SessionKind::SubAgent
        );
    }

    /// G-no-skill-injection: `.without_skills()` must null the SkillManager so a
    /// worker gets neither skill listing nor skill tools.
    #[test]
    fn gate_no_skill_injection_builder_flag() {
        let b = LlmAgentBuilder::new().without_skills();
        assert!(b.skill_manager_disabled, "without_skills must disable attach");
        assert!(b.skill_manager.is_none());
        let d = LlmAgentBuilder::new();
        assert!(!d.skill_manager_disabled, "default builder keeps legacy attach");
    }

    /// G-instant-tools (regression): the step-2a instrument delivery ALWAYS keys
    /// off the runtime InvocationContext.mode/depth, never the agent's static
    /// builder mode. An Instant top-level run (ctx.mode=Instant) must receive the
    /// FULL allowset even if the shared LlmAgent was built with mode=Expert, so
    /// the orchestration tools follow the run-time context, not the static mode.
    #[test]
    fn gate_instrument_delivery_reads_ctx_not_agent_static_mode() {
        // The agent may be built as Expert, but a run whose InvocationContext is
        // Instant (depth 0) is the one that opens the orchestration allowset.
        let agent_built_expert = crate::context::AgentMode::Expert;
        let run_ctx_is_instant = crate::context::AgentMode::Instant;
        let delivered = orchestration_allowset(run_ctx_is_instant, 0);
        assert_eq!(delivered.len(), ALL_ORCH.len(), "Instant run must get the full orchestration allowset even if agent mode={:?}", agent_built_expert);

        // The reverse guard: an Expert run (ctx.mode=Expert) never opens the
        // allowset — proving the decision is ctx-driven toward Instant.
        let expert_root = orchestration_allowset(crate::context::AgentMode::Expert, 0);
        assert!(expert_root.is_empty());
        // depth>=1 workers never open it (in either mode).
        assert!(orchestration_allowset(crate::context::AgentMode::Instant, 1).is_empty());
        assert!(orchestration_allowset(crate::context::AgentMode::Expert, 1).is_empty());
    }

    #[test]
    fn prefilter_simple_task_is_false() {
        assert!(!orchestration_prefilter("总结一下这个进程在做什么"));
    }
    #[test]
    fn prefilter_parallel_wording_is_true() {
        assert!(orchestration_prefilter("分别检查这两台主机的持久化项"));
    }
    #[test]
    fn prefilter_multi_ip_is_true() {
        assert!(orchestration_prefilter("扫描 10.0.0.5 和 10.0.0.6 的开放端口"));
    }
    #[test]
    fn prefilter_many_sources_is_true() {
        assert!(orchestration_prefilter("把进程、服务、注册表和日志都拉一遍做时间线"));
    }

    #[test]
    fn delivered_for_gates_on_candidate() {
        use crate::context::AgentMode::*;
        assert!(orchestration_delivered_for(Instant, 0, false).is_empty());
        assert_eq!(orchestration_delivered_for(Instant, 0, true).len(), ALL_ORCH.len());
        assert!(orchestration_delivered_for(Expert, 0, true).is_empty());
        assert!(orchestration_delivered_for(Instant, 1, true).is_empty());
    }
    // ── Loop-guard helpers (v1.0.11) ──
    fn tcd(name: &str, args: &str) -> crate::model::ToolCallDelta {
        crate::model::ToolCallDelta {
            id: "t".into(),
            call_type: "function".into(),
            function: crate::model::FunctionCallDelta {
                name: Some(name.into()),
                arguments: Some(args.into()),
            },
        }
    }

    /// build_batch_signature is order-insensitive: same call set in any order yields
    /// the same signature, so a reordered batch is not mistaken for a different loop.
    #[test]
    fn batch_signature_order_independent() {
        let a = vec![tcd("net_stat", "{}"), tcd("file_read", r#"{"p":"a.txt"}"#)];
        let b = vec![tcd("file_read", r#"{"p":"a.txt"}"#), tcd("net_stat", "{}")];
        assert_eq!(build_batch_signature(&a), build_batch_signature(&b));
    }

    /// build_batch_signature distinguishes argument changes: Test-Path A vs Test-Path B
    /// are different batches, so they cannot accumulate into a shared false loop.
    #[test]
    fn batch_signature_arg_sensitive() {
        assert_ne!(
            build_batch_signature(&[tcd("Test-Path", "A")]),
            build_batch_signature(&[tcd("Test-Path", "B")]),
        );
    }

    /// rabbit_hole_check fires only on CONTIGUOUS identical batches crossing threshold.
    #[test]
    fn rabbit_consecutive_same_batch_triggers() {
        let mut prev: Option<String> = None;
        let mut streak = 0usize;
        assert!(rabbit_hole_check(&mut prev, &mut streak, "X", 5).is_none()); // 1
        assert!(rabbit_hole_check(&mut prev, &mut streak, "X", 5).is_none()); // 2
        assert!(rabbit_hole_check(&mut prev, &mut streak, "X", 5).is_none()); // 3
        assert!(rabbit_hole_check(&mut prev, &mut streak, "X", 5).is_none()); // 4
        assert_eq!(rabbit_hole_check(&mut prev, &mut streak, "X", 5), Some(5)); // fires
        // After firing, state resets so the cycle can start again.
        assert!(rabbit_hole_check(&mut prev, &mut streak, "X", 5).is_none());
    }

    /// Interleaved different batches RESET the streak: a tool called a few times across
    /// a long run amid other work never accumulates to a false lifetime trigger.
    #[test]
    fn rabbit_resets_on_interleaved_work() {
        let mut prev: Option<String> = None;
        let mut streak = 0usize;
        rabbit_hole_check(&mut prev, &mut streak, "A", 5);
        rabbit_hole_check(&mut prev, &mut streak, "B", 5);
        rabbit_hole_check(&mut prev, &mut streak, "A", 5);
        rabbit_hole_check(&mut prev, &mut streak, "C", 5);
        rabbit_hole_check(&mut prev, &mut streak, "A", 5);
        // Total "A" count = 4, but never 5 consecutive; must not fire.
        assert!(rabbit_hole_check(&mut prev, &mut streak, "A", 5).is_none());
        // B two in a row still below threshold.
        assert!(rabbit_hole_check(&mut prev, &mut streak, "B", 5).is_none());
    }
    /// F3 3a: same tool with different arguments must produce different args_digests,
    /// so their repeated-result counters never share an entry (Test-Path A vs B).
    #[test]
    fn stall_args_keying_separates_same_tool_diff_args() {
        let tc = |id: &str, name: &str, args: &str| crate::model::ToolCallDelta {
            id: id.into(),
            call_type: "function".into(),
            function: crate::model::FunctionCallDelta {
                name: Some(name.into()),
                arguments: Some(args.into()),
            },
        };
        let asst = ChatMessage::assistant_with_tool_calls(vec![
            tc("c1", "Test-Path", "A"),
            tc("c2", "Test-Path", "B"),
        ]);
        let r1 = ChatMessage::tool_result("c1", "Test-Path", "False");
        let r2 = ChatMessage::tool_result("c2", "Test-Path", "False");
        let got = collect_tool_results_args(&[asst, r1, r2]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "Test-Path");
        assert_eq!(got[1].0, "Test-Path");
        // Identical results, but different args => different keys, so they don't stack.
        assert_ne!(got[0].1, got[1].1, "different args must key separately");
        assert_eq!(got[0].2, got[1].2, "both return the same short result");
    }

    /// F3 fallback: a tool result whose call cannot be resolved groups by name only
    /// (same args_digest), which is no worse than the pre-F3 name-only behavior.
    #[test]
    fn stall_args_fallback_groups_by_name_when_unknown() {
        let r1 = ChatMessage::tool_result("zz", "Test-Path", "False");
        let r2 = ChatMessage::tool_result("yy", "Test-Path", "False");
        let got = collect_tool_results_args(&[r1, r2]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, got[1].1, "unresolvable args use the same fallback digest");
    }
}
