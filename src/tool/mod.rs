pub mod file_ops;
pub mod shell_exec;
pub mod recycle;
pub mod sys_info;
pub mod sys_eventlog;
pub mod sys_process;
pub mod sys_service;
pub mod sys_remind;
pub mod app_launch;
pub mod browser_open;
pub mod mcp_client;
pub mod web_fetch;
pub mod cron_manage;
pub mod memory_md;
pub mod deep_memory;
pub mod knowledge_search;
pub mod recall_memory;
pub mod knowledge_ingest;
pub mod todo_update;
pub mod browser_cdp;
pub mod ir_scan;
pub mod ir_process;
pub mod ir_account;
pub mod ir_persistence;
pub mod ir_network;
pub mod ir_eventlog;
pub mod ir_file;
pub mod ir_artifacts;
pub mod ir_driver;
pub mod ir_analyzer;
pub mod ir_report;
pub mod malware_analysis;
pub mod malware_scan;
pub mod malware_deep;
pub mod ir_weblog_scan;
pub mod ir_evtx_parse;
pub mod ir_log_parse;
pub mod ir_pcap_analyze;
pub mod ir_timeline;
pub mod external_exec;
pub mod computer_use;
pub mod orchestration;
pub mod linux_ssh;
pub mod winrm;
pub mod evidence;
pub mod linux_ir_common;
pub mod linux_ir_process;
pub mod linux_ir_network;
pub mod linux_ir_persistence;
pub mod linux_ir_rootkit;
pub mod linux_ir_file;
pub mod linux_ir_web;
pub mod linux_ir_mining;
pub mod linux_ir_lateral;
pub mod linux_ir_auth;
pub mod linux_ir_backdoor;
pub mod linux_ir_bruteforce;
pub mod linux_ir_integrity;
pub mod linux_ir_config;
pub mod ir_linux;
pub mod ir_vss;
pub mod ir_usn;
pub mod ir_memdump;
pub mod ir_case;
pub mod ir_attackpath;
pub mod ir_eml;
pub mod partial_result;

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::context::ToolContext;
use crate::error::AgentResult;
use crate::model::ToolDefinition;

// ============================================================
// Tool timeout stage classification — for long-horizon tasks
// ============================================================

/// Timeout stage for tools, controlling how long the executor waits
/// before aborting. Modeled after LongHorizon-Harness graded timeout policy.
///
/// Tools that perform disk scans, remote SSH operations, or large data
/// processing should override [`Tool::timeout_secs`] to return [`TimeoutStage::Long`]
/// or [`TimeoutStage::Watchdog`] instead of the default [`TimeoutStage::Normal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutStage {
    /// Fast operations: file reads, simple shell commands, sys_info (~30s).
    Fast,
    /// Normal operations: most tools, IR collection, web fetches (~300s).
    Normal,
    /// Long operations: full YARA scans, remote SSH scans, large eventlog
    /// exports, PCAP analysis (~30min).
    Long,
    /// Watchdog operations: memory dumps, deep disassembly — no hard wall-clock
    /// limit, only a liveness watchdog (abort if silent for 10min).
    Watchdog,
}

impl TimeoutStage {
    /// Convert stage to a wall-clock timeout in seconds.
    /// `Watchdog` returns `None` (no hard limit; caller uses liveness watchdog).
    pub fn as_secs(self) -> Option<u64> {
        match self {
            TimeoutStage::Fast => Some(30),
            TimeoutStage::Normal => Some(300),
            TimeoutStage::Long => Some(1800),
            TimeoutStage::Watchdog => None,
        }
    }

    /// Liveness watchdog threshold for this stage (seconds of silence before abort).
    /// Used when the hard wall-clock is `None` (Watchdog) or as a secondary check.
    pub fn watchdog_silence_secs(self) -> u64 {
        match self {
            TimeoutStage::Fast => 15,
            TimeoutStage::Normal => 60,
            TimeoutStage::Long => 300,
            TimeoutStage::Watchdog => 600,
        }
    }
}

impl Default for TimeoutStage {
    fn default() -> Self {
        TimeoutStage::Normal
    }
}

// ============================================================
// Tool trait — enriched interface modeled after ADK-RUST
// ============================================================

/// The Tool trait — core abstraction for all callable tools.
/// Modeled after ADK-RUST's Tool trait with rich metadata methods.
///
/// Default implementations return sensible values so most tools
/// only need to implement name(), description(), parameters_schema(), execute().
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (unique identifier, used by the LLM).
    fn name(&self) -> &str;

    /// Human-readable description for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters_schema(&self) -> Value;

    /// Execute the tool with given arguments and context.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value>;

    // --- ADK-RUST inspired metadata methods (defaults provided) ---

    /// Whether this tool is a built-in tool (vs user-provided or MCP).
    fn is_builtin(&self) -> bool { false }

    /// Whether this tool requires exclusive access to a shared resource
    /// (e.g. a long-lived browser/WinRM session). Default: shared.
    fn exclusivity(&self) -> crate::agent::exclusivity::Exclusivity {
        crate::agent::exclusivity::Exclusivity::Shared
    }

    /// Whether this tool is "peripheral" (e.g. MCP or external). Peripheral tools
    /// are NOT sent in full to the LLM every request; they are exposed on demand
    /// via the `load_tool_schema` helper to keep the per-request payload small.
    fn is_peripheral(&self) -> bool { false }

    /// Whether this tool only reads data without modifying anything.
    /// Read-only tools can be executed concurrently in Parallel strategy.
    fn is_read_only(&self) -> bool { false }

    /// Permission category for this tool: read, write, delete, modify, or execute.
    fn category(&self) -> &str {
        crate::permission::tool_category(self.name())
    }

    /// Whether this tool is safe for concurrent execution.
    fn is_concurrency_safe(&self) -> bool { true }

    /// Whether this tool is long-running (e.g., file downloads, installs).
    fn is_long_running(&self) -> bool { false }

    /// Timeout stage for this tool. Determines the wall-clock limit and
    /// liveness watchdog threshold used by the executor.
    ///
    /// Override this for tools that perform disk scans, remote operations,
    /// or large data processing to return [`TimeoutStage::Long`] or
    /// [`TimeoutStage::Watchdog`].
    ///
    /// Default: [`TimeoutStage::Normal`] (300s).
    fn timeout_stage(&self) -> TimeoutStage {
        TimeoutStage::Normal
    }

    /// Effective timeout in seconds for this tool, or `None` for watchdog-only.
    /// Convenience wrapper around [`Tool::timeout_stage`].
    fn timeout_secs(&self) -> Option<u64> {
        self.timeout_stage().as_secs()
    }

    /// JSON Schema for the tool's response (optional).
    fn response_schema(&self) -> Option<Value> { None }

    /// Enhanced description with additional context (e.g., platform info).
    fn enhanced_description(&self) -> String { self.description().to_string() }

    /// Required scopes for authorization (empty = no auth required).
    fn required_scopes(&self) -> Vec<String> { vec![] }

    /// Convert to a ToolDefinition for LLM function-calling protocol.
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: crate::model::FunctionDefinition {
                name: self.name().to_string(),
                description: self.enhanced_description(),
                parameters: self.parameters_schema(),
            },
        }
    }
}

// ============================================================
// Tool execution strategy — modeled after ADK-RUST
// ============================================================

/// How tools should be executed within a single agent iteration.
/// Modeled after ADK-RUST's ToolExecutionStrategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionStrategy {
    /// Execute tools one at a time, in order.
    Sequential,
    /// Execute all tools concurrently (only safe for read-only/concurrency-safe tools).
    Parallel,
    /// Automatically choose: concurrent for read-only tools, sequential for mutable ones.
    Auto,
}

impl Default for ToolExecutionStrategy {
    fn default() -> Self {
        Self::Sequential
    }
}

// ============================================================
// Toolset abstraction — modeled after ADK-RUST
// ============================================================

/// A collection of tools that can be resolved dynamically.
/// Modeled after ADK-RUST's Toolset trait.
pub trait Toolset: Send + Sync {
    /// Get all tools in this toolset.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

/// Basic toolset — a fixed list of tools.
pub struct BasicToolset {
    tools: Vec<Arc<dyn Tool>>,
}

impl BasicToolset {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }
}

impl Toolset for BasicToolset {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

/// Merged toolset — combines multiple toolsets.
pub struct MergedToolset {
    toolsets: Vec<Box<dyn Toolset>>,
}

impl MergedToolset {
    pub fn new(toolsets: Vec<Box<dyn Toolset>>) -> Self {
        Self { toolsets }
    }
}

impl Toolset for MergedToolset {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.toolsets.iter().flat_map(|ts| ts.tools()).collect()
    }
}

// ============================================================
// Tool registry — name-to-instance lookup
// ============================================================

/// Registry of tools — provides name-based lookup and definition generation.
/// Modeled after ADK-RUST's ToolRegistry.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.register_arc(tool);
    }

    /// Register a tool from a shared `Arc` (SDD v1.5 §13 Step 1.9 `register_arc`).
    /// Canonical registration entry point; `register` delegates here.
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Remove a tool by name. Returns true if a tool was removed.
    pub fn unregister(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    /// Remove multiple tools by name.
    pub fn unregister_many(&mut self, names: &[String]) {
        for name in names {
            self.tools.remove(name.as_str());
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.to_definition()).collect()
    }

    /// Full definitions of core (always-on) tools. Peripheral tools
    /// (e.g. MCP / external) are exposed on-demand via `load_tool_schema`.
    pub fn core_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .filter(|t| !t.is_peripheral())
            .map(|t| t.to_definition())
            .collect()
    }

    /// Compact (name, one-line) summary of peripheral tools for the catalog.
    pub fn peripheral_tools(&self) -> Vec<(String, String)> {
        self.tools
            .values()
            .filter(|t| t.is_peripheral())
            .map(|t| (t.name().to_string(), t.description().lines().next().unwrap_or("").to_string()))
            .collect()
    }

    /// Resolve the full definition of a tool by name (any tool).
    pub fn get_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.tools.get(name).map(|t| t.to_definition())
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Names of all read-only tools currently registered. Used to build the
    /// default read-only allowlist for Phase 0 sub-agent workers.
    pub fn read_only_names(&self) -> Vec<String> {
        self.tools
            .values()
            .filter(|t| t.is_read_only())
            .map(|t| t.name().to_string())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Names of tools that require write/modify (allow_write) or execute
    /// (allow_exec) capability, per the three-tier access model (SDD v1.5
    /// §7.10). Used to expand a worker allowlist for Step 2b write/exec workers.
    pub fn write_exec_names(&self, allow_write: bool, allow_exec: bool) -> Vec<String> {
        self.tools
            .values()
            .filter(|t| {
                let cat = t.category();
                (allow_write && matches!(cat, "write" | "modify" | "delete"))
                    || (allow_exec && cat == "execute")
            })
            .map(|t| t.name().to_string())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Remove all external tools (names starting with "ext_").
    pub fn unregister_external(&mut self) {
        self.tools.retain(|name, _| !name.starts_with("ext_"));
    }

    /// Register external tools from workspace/tools/ directory.
    /// Removes existing ext_* tools first, then registers current enabled tools.
    pub fn sync_external_tools(&mut self, handles: &[(String, std::path::PathBuf, String, String)]) {
        self.unregister_external();
        for (name, path, description, extension) in handles {
            let tool = Arc::new(external_exec::ExternalToolExecutor::new(
                name,
                path.clone(),
                description,
                extension,
            ));
            self.register(tool);
        }
    }

    /// Build the default registry with all built-in Windows tools.
    /// `notify_tx` is the broadcast channel used by `sys_remind` to push
    /// reminders to WebSocket clients (pass `None` when unavailable).
    pub fn build_default(working_dir: &str, notify_tx: Option<crate::tool::sys_remind::NotifyTx>) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(file_ops::FileReadTool));
        registry.register(Arc::new(file_ops::FileWriteTool));
        registry.register(Arc::new(file_ops::FileDeleteTool));
        registry.register(Arc::new(file_ops::FileModifyTool));
        registry.register(Arc::new(file_ops::FileListTool));
        registry.register(Arc::new(shell_exec::ShellExecTool));
        registry.register(Arc::new(sys_info::SysInfoTool));
        registry.register(Arc::new(sys_eventlog::SysEventLogTool));
        registry.register(Arc::new(sys_process::SysProcessTool));
        registry.register(Arc::new(sys_service::SysServiceTool));
        registry.register(Arc::new(sys_remind::SysRemindTool::with_notify_tx_optional(notify_tx)));
        registry.register(Arc::new(app_launch::AppLaunchTool));
        registry.register(Arc::new(browser_open::BrowserOpenTool));
        registry.register(Arc::new(web_fetch::WebFetchTool));
        // IR (Incident Response) tools — ported from yinghuo
        registry.register(Arc::new(ir_scan::IrScanTool));
        registry.register(Arc::new(ir_process::IrProcessTool));
        registry.register(Arc::new(ir_account::IrAccountTool));
        registry.register(Arc::new(ir_persistence::IrPersistenceTool));
        registry.register(Arc::new(ir_network::IrNetworkTool));
        registry.register(Arc::new(ir_eventlog::IrEventLogTool));
        registry.register(Arc::new(ir_file::IrFileTool));
        registry.register(Arc::new(ir_artifacts::IrArtifactsTool));
        registry.register(Arc::new(ir_driver::IrDriverTool));
        registry.register(Arc::new(ir_analyzer::IrAnalyzerTool));
        registry.register(Arc::new(ir_report::IrReportTool));
        // Investigation case tracker and attack path modeling
        registry.register(Arc::new(ir_case::IrCaseTool));
        registry.register(Arc::new(ir_attackpath::IrAttackPathTool));
        // EML email parser for phishing analysis
        registry.register(Arc::new(ir_eml::IrEmlTool));
        // Malware analysis tools — ported from hacksguard
        registry.register(Arc::new(malware_scan::MalwareScanTool));
        registry.register(Arc::new(malware_deep::MalwareDeepTool));
        // Log analysis tools — ported from RavenEye
        registry.register(Arc::new(ir_weblog_scan::IrWeblogScanTool));
        registry.register(Arc::new(ir_evtx_parse::IrEvtxParseTool));
        registry.register(Arc::new(ir_log_parse::IrLogParseTool));
        // PCAP analysis
        registry.register(Arc::new(ir_pcap_analyze::IrPcapAnalyzeTool));
        // Timeline reconstruction
        registry.register(Arc::new(ir_timeline::IrTimelineTool));
        // Linux IR — remote SSH-based incident response
        registry.register(Arc::new(ir_linux::IrLinuxTool));
        // Linux IR — individual category tools (like Windows IR tools)
        for tool in ir_linux::linux_ir_category_tools() {
            registry.register(tool);
        }
        // General-purpose SSH command execution (like shell_exec for remote Linux)
        registry.register(Arc::new(linux_ssh::SshExecTool));
        // WinRM command execution (like shell_exec for remote Windows)
        registry.register(Arc::new(winrm::WinrmExecTool));
        // Forensic disk/memory tools
        registry.register(Arc::new(ir_vss::IrVssTool));
        registry.register(Arc::new(ir_usn::IrUsnTool));
        registry.register(Arc::new(ir_memdump::IrMemdumpTool));
        let _ = working_dir;
        registry
    }

    /// Add all tools from a toolset.
    pub fn add_toolset(&mut self, toolset: &dyn Toolset) {
        for tool in toolset.tools() {
            self.register(tool);
        }
    }

    /// Return a new registry containing only the named tools (authorization subset).
    /// Used in Step 2a to build a worker-scoped tool set from `authorized_tools()`.
    pub fn subset(&self, names: &[String]) -> ToolRegistry {
        let mut out = ToolRegistry::new();
        for name in names {
            if let Some(tool) = self.tools.get(name) {
                out.register(tool.clone());
            }
        }
        out
    }

    /// Iterate over all registered (name, tool) pairs (SDD v1.5 §13 Step 1.9 `iter`).
    /// Used for registry introspection and for copying tools across registries.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Tool>)> {
        self.tools.iter().map(|(name, tool)| (name.as_str(), tool))
    }

    /// Remove the named tools in place. Returns the number of tools removed.
    pub fn minus(&mut self, names: &[String]) -> usize {
        let mut removed = 0;
        for name in names {
            if self.tools.remove(name).is_some() {
                removed += 1;
            }
        }
        removed
    }
}

// ============================================================
// Binary resolution — Windows-style search path
// ============================================================

/// Resolve an external binary path using a 3-tier search order:
///
/// 1. Application directory (where RustAgent.exe lives) — for bundled tools
/// 2. `{workspace}/tools/` — for user-installed tools
/// 3. System PATH — fallback to OS resolution
///
/// This follows the Windows convention where application-local binaries
/// take priority over user tools, which take priority over system-wide tools.
///
/// Returns the full path if found in tiers 1-2, or the bare name for PATH fallback.
pub fn resolve_binary(name: &str, workspace_dir: &str) -> String {
    // Tier 1: Application directory (where the RustAgent executable lives)
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let candidate = exe_dir.join(name);
            if candidate.exists() {
                tracing::debug!("Binary '{}' resolved from app dir: {}", name, candidate.display());
                return candidate.to_string_lossy().to_string();
            }
        }
    }

    // Tier 2: workspace/tools/ directory
    let tools_candidate = std::path::Path::new(workspace_dir)
        .join("tools")
        .join(name);
    if tools_candidate.exists() {
        tracing::debug!("Binary '{}' resolved from workspace/tools: {}", name, tools_candidate.display());
        return tools_candidate.to_string_lossy().to_string();
    }

    // Tier 3: System PATH — return bare name, let OS resolve it
    tracing::debug!("Binary '{}' not found locally, falling back to system PATH", name);
    name.to_string()
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The default registry has read-only tools; `read_only_names` returns only
    /// those, and `subset` keeps exactly the requested names.
    #[test]
    fn read_only_names_and_subset_filter() {
        let reg = ToolRegistry::build_default("", None);
        let ro = reg.read_only_names();
        assert!(!ro.is_empty(), "expected some read-only tools");
        assert!(ro.contains(&"file_read".to_string()), "file_read should be read-only");
        // subset keeps only the named tools; skill/orchestration not included here.
        let subset = reg.subset(&["file_read".to_string(), "no_such_tool".to_string()]);
        assert_eq!(subset.tool_names(), vec!["file_read".to_string()]);
    }

    /// `iter` yields every registered (name, tool) pair; `register_arc` is the
    /// canonical Arc-based registration used by `register` (SDD v1.5 §13 Step 1.9).
    #[test]
    fn iter_covers_registry_and_register_arc_inserts() {
        let mut reg = ToolRegistry::new();
        reg.register_arc(Arc::new(file_ops::FileReadTool));
        reg.register(Arc::new(file_ops::FileListTool));
        assert_eq!(reg.len(), 2);
        let mut names: Vec<&str> = reg.iter().map(|(name, _)| name).collect();
        names.sort();
        assert_eq!(names, vec!["file_list", "file_read"]);
        // Every iterated tool is retrievable by name and shares the same Arc identity.
        for (name, tool) in reg.iter() {
            let fetched = reg.get(name).expect("iterated tool must be retrievable");
            assert!(Arc::ptr_eq(tool, &fetched));
        }
    }

    /// `minus` removes the named tools in place.
    #[test]
    fn minus_removes_named_tools() {
        let mut reg = ToolRegistry::build_default("", None);
        let before = reg.len();
        let removed = reg.minus(&["file_read".to_string()]);
        assert_eq!(removed, 1);
        assert_eq!(reg.len(), before - 1);
        assert!(reg.get("file_read").is_none());
    }

    /// `write_exec_names` admits only the opt-in capability tiers (SDD v1.5
    /// §7.10 layer 2): allow_write -> write/modify/delete, allow_exec -> execute.
    #[test]
    fn write_exec_names_admits_optin_tiers() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(file_ops::FileReadTool));   // read
        reg.register(Arc::new(file_ops::FileWriteTool));  // write
        reg.register(Arc::new(shell_exec::ShellExecTool)); // execute
        let none = reg.write_exec_names(false, false);
        assert!(none.is_empty(), "no opt-in => no write/exec tools, got {none:?}");
        let w = reg.write_exec_names(true, false);
        assert!(w.contains(&"file_write".to_string()), "allow_write admits file_write, got {w:?}");
        assert!(!w.contains(&"shell_exec".to_string()), "allow_write must not admit shell_exec");
        let e = reg.write_exec_names(false, true);
        assert!(e.contains(&"shell_exec".to_string()), "allow_exec admits shell_exec, got {e:?}");
        assert!(!e.contains(&"file_write".to_string()), "allow_exec must not admit file_write");
        let both = reg.write_exec_names(true, true);
        assert!(both.contains(&"file_write".to_string()) && both.contains(&"shell_exec".to_string()));
    }
}
