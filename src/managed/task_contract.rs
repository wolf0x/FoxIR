//! TaskContract — persistent state for managed (long-horizon) task execution.
//!
//! The TaskContract is the single source of truth for a managed task's progress.
//! It survives across Executor rounds and is the only input to the Manager's
//! planning decisions. This prevents context drift by keeping verified state
//! separate from the Executor's growing (and eventually trimmed) history.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Incident Response phase lifecycle (NIST SP 800-61 aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IrPhase {
    /// Initial data collection (ir_* tools, YARA scans, etc.)
    Collection,
    /// Analysis and correlation of collected evidence
    Analysis,
    /// Attribution and attack path reconstruction
    Attribution,
    /// Containment actions (kill processes, isolate hosts)
    Containment,
    /// Eradication (remove persistence, clean artifacts)
    Eradication,
    /// Report generation and delivery
    Reporting,
    /// Task completed successfully
    Completed,
    /// Task blocked (waiting for human input or external dependency)
    Blocked,
}

impl Default for IrPhase {
    fn default() -> Self {
        IrPhase::Collection
    }
}


/// Executable knowledge domain for a managed task. The orchestration loop is
/// domain-agnostic; each domain supplies a phase vocabulary, default phase, and
/// terminal-phase semantics. IR is the first (and today only) built-in domain
/// pack; a new domain can be added by introducing a variant here without
/// touching the Manager/Executor/Auditor loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskDomain {
    /// Incident Response (NIST SP 800-61 lifecycle).
    Ir,
    /// Fallback domain for non-IR long-horizon tasks.
    Generic,
}

impl Default for TaskDomain {
    fn default() -> Self {
        TaskDomain::Ir
    }
}

impl TaskDomain {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskDomain::Ir => "ir",
            TaskDomain::Generic => "generic",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "ir" => TaskDomain::Ir,
            _ => TaskDomain::Generic,
        }
    }
    /// Heuristically choose a knowledge domain from the task text.
    /// IR keywords -> Incident Response pack; otherwise use the generic
    /// long-horizon pack (web/CTF/creative/research tasks should NOT be
    /// framed as host forensics).
    pub fn detect(text: &str) -> Self {
        let t = text.to_lowercase();
        let ir_keywords: &[&str] = &[
            "incident", "应急", "malware", "恶意", "勒索", "ransom", "yara", "钓鱼", "phishing",
            "威胁", "threat", "attacker", "攻击", "入侵", "intrusion", "取证", "forensic", "apt",
            "告警", "alert", "病毒", "感染", "compromise", "data breach", "数据泄露", "应急响应",
            "containment", "threat hunting", "ioc", "mitre", "triage", "snort", "suricata", "sigma",
        ];
        if ir_keywords.iter().any(|k| t.contains(k)) {
            TaskDomain::Ir
        } else {
            TaskDomain::Generic
        }
    }
    pub fn phases(&self) -> Vec<&'static str> {
        match self {
            TaskDomain::Ir => vec!["collection", "analysis", "attribution", "containment", "eradication", "reporting", "completed", "blocked"],
            TaskDomain::Generic => vec!["plan", "execute", "verify", "completed", "blocked"],
        }
    }
    pub fn default_phase(&self) -> &'static str {
        match self {
            TaskDomain::Ir => "collection",
            TaskDomain::Generic => "plan",
        }
    }
    pub fn is_terminal(&self, phase: &str) -> bool {
        matches!(phase, "completed" | "blocked")
    }
}

fn default_domain() -> String {
    "ir".to_string()
}

/// A domain-agnostic task-state record (requirement / artifact / fact).
/// This is the generic counterpart to IR-specific findings: a record is promoted
/// into persistent state only when backed by audited evidence (completed + clean).
/// Executor claims or handoffs are seeded as `untrusted` until independently audited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Stable identifier.
    pub id: String,
    /// Record kind: requirement | artifact | fact.
    pub kind: String,
    /// Short human description.
    pub title: String,
    /// Status: completed | pending | blocked | untrusted.
    #[serde(default = "default_pending")]
    pub status: String,
    /// Evidence integrity: clean | suspect | violation.
    #[serde(default = "default_clean")]
    pub integrity: String,
    /// Summary of the supporting evidence.
    #[serde(default)]
    pub evidence_summary: String,
    /// Path to the evidence file (if applicable).
    #[serde(default)]
    pub evidence_path: Option<String>,
    /// Domain phase this record belongs to.
    #[serde(default)]
    pub phase: Option<String>,
    /// Manager round that last touched this record.
    #[serde(default)]
    pub round_index: usize,
    /// When this record was last verified.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Task-tree parent (the round/subtask this record belongs to). Spec §7.8.1.
    /// Absent for round-level records.
    #[serde(default)]
    pub parent_task_id: Option<String>,
    /// Task-tree dependencies (ids this record depends on). Spec §7.8.1.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

fn default_pending() -> String {
    "pending".to_string()
}
/// A verified finding — evidence that has passed Auditor verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedFinding {
    /// Unique identifier for this finding.
    pub id: String,
    /// Short title (e.g., "XMRig mining process detected").
    pub title: String,
    /// Severity: critical, high, medium, low, info.
    pub severity: String,
    /// Audit status: complete / incomplete / blocked (default: complete).
    #[serde(default = "default_complete")]
    pub status: String,
    /// Evidence integrity: clean / suspect / violation (default: clean).
    #[serde(default = "default_clean")]
    pub integrity_status: String,
    /// Evidence summary (not full content — reference files for details).
    pub evidence_summary: String,
    /// Path to evidence file in workspace (if applicable).
    pub evidence_path: Option<String>,
    /// MITRE ATT&CK technique ID (if applicable).
    pub mitre_technique: Option<String>,
    /// When this finding was verified.
    pub verified_at: DateTime<Utc>,
    /// Which Manager round verified this.
    pub round_index: usize,
}

fn default_complete() -> String { "complete".to_string() }
fn default_clean() -> String { "clean".to_string() }

/// A verified action — containment/eradication step confirmed by Auditor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedAction {
    /// Unique identifier.
    pub id: String,
    /// What action was taken (e.g., "Killed process xmrig.exe PID 1234").
    pub description: String,
    /// Verification result (e.g., "Process no longer in process list").
    pub verification: String,
    /// When this action was verified.
    pub verified_at: DateTime<Utc>,
    /// Which Manager round verified this.
    pub round_index: usize,
}

/// An open lead being investigated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenLead {
    /// Short description of the lead.
    pub description: String,
    /// Current status: pending / investigating / resolved / abandoned.
    pub status: String,
    /// Evidence or context so far.
    pub context: String,
    /// Round index of the lead's last activity.
    #[serde(default)]
    pub round_index: usize,
    /// Why the lead was resolved or abandoned.
    #[serde(default)]
    pub reason: Option<String>,
}

/// A single round's independent Auditor verdict, retained as the trusted
/// cross-round memory the Manager plans forward from (LongHorizon-Harness
/// style: auditor reports are the authority for trusted intermediate state).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundAudit {
    /// Round index this audit belongs to (1-based, matches the displayed round).
    #[serde(default)]
    pub round_index: usize,
    /// completion: complete | incomplete | blocked.
    #[serde(default)]
    pub completion: String,
    /// integrity: clean | suspect | violation.
    #[serde(default)]
    pub integrity: String,
    /// One-sentence reviewer note.
    #[serde(default)]
    pub note: String,
    /// Facts the auditor independently corroborated.
    #[serde(default)]
    pub supported_facts: Vec<String>,
    /// Unmet requirements / open gaps.
    #[serde(default)]
    pub gaps: Vec<String>,
}

/// The TaskContract — persistent state for a managed task.
///
/// This struct is serialized to JSON and stored in SQLite. It is the sole
/// input to the Manager's planning decisions, ensuring that verified progress
/// is never lost to context window trimming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskContract {
    /// Unique identifier for this managed task.
    pub id: String,
    /// Original task description from the user.
    pub original_task: String,
    /// Current IR phase.
    pub phase: IrPhase,
    /// Target host(s) or scope (e.g., "10.0.0.5", "this machine").
    pub scope: String,
    /// Event hypothesis (what we think happened, updated as evidence accumulates).
    pub hypothesis: String,
    /// Verified findings — only results confirmed by Auditor.
    pub verified_findings: Vec<VerifiedFinding>,
    /// Verified actions — containment/eradication confirmed by re-check.
    pub verified_actions: Vec<VerifiedAction>,
    /// Open leads being investigated.
    pub open_leads: Vec<OpenLead>,
    /// Remaining work / unresolved requirements after the latest round. This
    /// is the authoritative to-do list the Manager plans forward from (audit-report
    /// style cross-round memory, per LongHorizon-Harness).
    #[serde(default)]
    pub remaining_work: Vec<String>,
    /// Index into open_leads of the lead currently being pursued (DFS focus).
    #[serde(default)]
    pub current_focus: Option<usize>,
    /// Total number of times the runner backtracked off a dead-end lead.
    #[serde(default)]
    pub backtracks: usize,
    /// Current Manager round index (0-based).
    pub current_round: usize,
    /// Maximum rounds allowed.
    pub max_rounds: usize,
    /// When this task was created.
    pub created_at: DateTime<Utc>,
    /// When this task was last updated.
    pub updated_at: DateTime<Utc>,
    /// Free-form notes from the Manager (e.g., "Need to check lateral movement from host X").
    pub manager_notes: Vec<String>,
    /// Why the task is blocked (if phase == Blocked).
    pub blocked_reason: Option<String>,
    /// Original phase before blocking (used to restore on unblock).
    #[serde(default)]
    pub phase_before_block: Option<IrPhase>,

    /// Knowledge domain for this task ("ir" is the built-in domain pack).
    #[serde(default = "default_domain")]
    pub domain: String,
    /// Domain-agnostic task-state records (requirement / artifact / fact).
    #[serde(default)]
    pub records: Vec<TaskRecord>,

    /// Distilled, UNVERIFIED prior-session (Instant) context handed over to Expert.
    #[serde(default)]
    pub prior_handoff: Option<String>,
    /// Serialized in-flight Manager plan (job). When a round is STOP'd mid-
    /// execution it is persisted here so a bare "continue" resumes the EXACT
    /// same subtask instead of re-planning a divergent one.
    #[serde(default)]
    pub pending_plan: Option<String>,
    /// Serialized shared Orchestrator plan (update_plan tool / manager plan),
    /// persisted as the durable plan truth-source (SDD v1.5 7.8.1). Kept
    /// distinct from `pending_plan` so a published plan survives a respawn and
    /// can be re-seeded onto a fresh Orchestrator on resume.
    #[serde(default)]
    pub orchestrator_plan: Option<String>,

    /// Per-round independent Auditor reports (ordered), surfaced to the
    /// Manager as the trusted cross-round memory / failure evidence.
    #[serde(default)]
    pub round_audits: Vec<RoundAudit>,
}

impl TaskContract {
    /// Create a new TaskContract for a managed task.
    pub fn new(id: String, original_task: String, scope: String, max_rounds: usize) -> Self {
        let domain = TaskDomain::detect(&original_task).as_str().to_string();
        let now = Utc::now();
        Self {
            id,
            original_task,
            phase: IrPhase::default(),
            scope,
            hypothesis: String::new(),
            verified_findings: Vec::new(),
            verified_actions: Vec::new(),
            open_leads: Vec::new(),
            current_focus: None,
            backtracks: 0,
            current_round: 0,
            max_rounds,
            created_at: now,
            updated_at: now,
            manager_notes: Vec::new(),
            blocked_reason: None,
            phase_before_block: None,
            remaining_work: Vec::new(),

            domain,
            records: Vec::new(),

            prior_handoff: None,
            pending_plan: None,
            orchestrator_plan: None,

            round_audits: Vec::new(),
        }
    }

    /// Generate a condensed brief for the Executor.
    ///
    /// This brief replaces the full conversation history in managed mode.
    /// It contains only verified state and the current subtask, keeping
    /// the Executor's context small and focused.
    pub fn executor_brief(&self, subtask: &str, success_criteria: &str) -> String {
        let mut brief = String::new();

        brief.push_str("# Task Brief\n\n");
        brief.push_str(&format!("**Original Task**: {}\n", self.original_task));
        brief.push_str(&format!("**Scope**: {}\n", self.scope));
        brief.push_str(&format!("**Current Phase**: {:?}\n", self.phase));
        brief.push_str(&format!("**Round**: {} / {}\n\n", self.current_round + 1, self.max_rounds));

        if let Some(ph) = &self.prior_handoff {
            let ph = ph.trim();
            if !ph.is_empty() {
                brief.push_str("## Prior Session Context (UNVERIFIED)\n");
                brief.push_str("Work done earlier in this session. Treat as LEADS/hypotheses, NOT verified facts. DO NOT blindly redo completed prior work; continue from where it left off and verify before trusting.\n");
                brief.push_str(ph);
                brief.push_str("\n\n");
            }
        }
        if !self.hypothesis.is_empty() {
            brief.push_str("## Hypothesis\n");
            brief.push_str(&self.hypothesis);
            brief.push_str("\n\n");
        }

        if !self.verified_findings.is_empty() {
            brief.push_str("## Verified Findings\n");
            // Show only the most recent 20 findings to prevent brief from growing too large
            let display_findings: Vec<_> = self.verified_findings.iter().rev().take(20).rev().collect();
            if display_findings.len() < self.verified_findings.len() {
                brief.push_str(&format!("(Showing most recent {} of {} findings)\n", display_findings.len(), self.verified_findings.len()));
            }
            for f in display_findings {
                brief.push_str(&format!("- [{}] {} ({})\n", f.severity.to_uppercase(), f.title, f.evidence_summary));
                if let Some(ref path) = f.evidence_path {
                    brief.push_str(&format!("  Evidence file: `{}`\n", path));
                }
            }
            brief.push('\n');
        }

        if !self.verified_actions.is_empty() {
            brief.push_str("## Verified Actions Taken\n");
            // Show only the most recent 10 actions to prevent brief from growing too large
            let display_actions: Vec<_> = self.verified_actions.iter().rev().take(10).rev().collect();
            if display_actions.len() < self.verified_actions.len() {
                brief.push_str(&format!("(Showing most recent {} of {} actions)\n", display_actions.len(), self.verified_actions.len()));
            }
            for a in display_actions {
                brief.push_str(&format!("- {} → {}\n", a.description, a.verification));
            }
            brief.push('\n');
        }

        if !self.open_leads.is_empty() {
            brief.push_str("## Open Leads\n");
            // Show only the most recent 10 leads to prevent brief from growing too large
            let display_leads: Vec<_> = self.open_leads.iter().rev().take(10).rev().collect();
            if display_leads.len() < self.open_leads.len() {
                brief.push_str(&format!("(Showing most recent {} of {} leads)\n", display_leads.len(), self.open_leads.len()));
            }
            for l in display_leads {
                brief.push_str(&format!("- [{}] {}: {}\n", l.status, l.description, l.context));
            }
            brief.push('\n');
        }

        // D2: only surface notes that carry durable signal (audit guard / user
        // resume / clarification / parallel guidance), plus the most recent note.
        // Filter out verbose per-round "Round N: <executor output>" summaries so
        // the Executor is not distracted by stale noise. Capped to the last 6.
        let relevant_notes: Vec<&String> = self.manager_notes.iter()
            .rev()
            .take(6)
            .filter(|n| {
                n.starts_with("[Audit Guard]") || n.starts_with("[User Resume]")
                    || n.starts_with("[Clarify]") || n.starts_with("[Parallel]")
                    || n.starts_with("[Simulated Human Guidance]")
                    || !n.starts_with("Round ")
            })
            .collect();
        if !relevant_notes.is_empty() {
            brief.push_str("## Manager Notes\n");
            for note in relevant_notes.iter().rev() {
                brief.push_str(&format!("- {}\n", note));
            }
            brief.push('\n');
        }

        // D2: surface the most recent independent Audit verdict so the Executor
        // knows what was already verified / still open without re-discovering it.
        if let Some(rp) = self.round_audits.last() {
            brief.push_str("## Most Recent Audit Verdict (what this subtask builds on)\n");
            brief.push_str(&format!("completion={}, integrity={}\n", rp.completion, rp.integrity));
            if !rp.supported_facts.is_empty() {
                brief.push_str(&format!("Verified: {}\n", rp.supported_facts.join("; ")));
            }
            if !rp.gaps.is_empty() {
                let gaps: Vec<&str> = rp.gaps.iter().take(4).map(|s| s.as_str()).collect();
                brief.push_str(&format!("Open: {}\n", gaps.join("; ")));
            }
            brief.push('\n');
        }

        brief.push_str("## Current Subtask\n");
        brief.push_str(subtask);
        brief.push_str("\n\n");

        brief.push_str("## Success Criteria\n");
        brief.push_str(success_criteria);
        brief.push_str("\n\n");

        brief.push_str("## Instructions\n");
        brief.push_str("- Execute this subtask using available tools.\n");
        brief.push_str("- **MANDATORY**: Save ALL artifacts, evidence files, reports, and outputs under the SHARED project artifact directory — the exact absolute path is stated in the brief (managed/<contract>/). It is the same directory every round, so later rounds can reuse files saved here. NEVER save files to C:\\, D:\\, or outside that directory.\n");
        brief.push_str("- Reference the saved file paths in your response as the absolute path, and list them in the FINAL STATE ARTIFACTS block.\n");
        brief.push_str("- Focus ONLY on this subtask. Do not repeat work from verified findings.\n");
        brief.push_str("- If you encounter a timeout, use partial results and narrow scope.\n\n");

        brief.push_str("## Final State Declaration (MANDATORY)\n");
        brief.push_str("At the END of your work for this round, you MUST output a structured block in this exact format:\n\n");
        brief.push_str("```\n");
        brief.push_str("FINAL STATE:\n");
        brief.push_str("DONE: <comma-separated list of what you completed this round>\n");
        brief.push_str("PENDING: <comma-separated list of what remains undone>\n");
        brief.push_str("ARTIFACTS: <comma-separated list of file paths you created/modified>\n");
        brief.push_str("```\n\n");
        brief.push_str("This block is used by the Auditor to verify your work. Be accurate — overclaiming will be caught.\n");

        brief
    }

    /// Add a verified finding.
    /// **Limit**: Max 50 findings (FIFO eviction) to prevent executor_brief from growing too large.
    pub fn add_finding(&mut self, finding: VerifiedFinding) {
        self.verified_findings.push(finding);
        // Enforce limit: remove oldest if over 50
        if self.verified_findings.len() > 50 {
            self.verified_findings.remove(0);
        }
        self.updated_at = Utc::now();
    }

    /// Add an open lead (unresolved item awaiting investigation).
    /// If a lead with the same description exists, update its context (failure reason).
    /// **Limit**: Max 20 leads (FIFO eviction) to prevent executor_brief from growing too large.
    pub fn add_lead(&mut self, description: &str, context: &str) {
        if let Some(existing) = self.open_leads.iter_mut().find(|l| l.description == description) {
            // Update context with latest failure reason
            existing.context = context.to_string();
            existing.status = "pending".to_string(); // Reset to pending for re-investigation
            existing.round_index = self.current_round;
            self.updated_at = Utc::now();
        } else {
            self.open_leads.push(OpenLead {
                description: description.to_string(),
                status: "pending".to_string(),
                context: context.to_string(),
                round_index: self.current_round,
                reason: None,
            });
            // Enforce limit: remove oldest if over 20
            if self.open_leads.len() > 20 {
                self.open_leads.remove(0);
            }
            self.updated_at = Utc::now();
        }
    }

    /// Abandon the current focus lead and switch to the next still-active lead
    /// (backtracking). Returns true if a lead remains to pursue, false if the
    /// frontier is exhausted (all leads resolved/abandoned). Bounded: each call
    /// abandons at most one lead; callers cap the total via MAX_GLOBAL_BACKTRACKS.
    pub fn try_backtrack(&mut self, round: usize) -> bool {
        // Abandon the current focus (it made no progress).
        if let Some(idx) = self.current_focus {
            if let Some(l) = self.open_leads.get_mut(idx) {
                if l.status == "pending" || l.status == "investigating" {
                    l.status = "abandoned".to_string();
                    l.reason = Some("no progress after consecutive rounds".to_string());
                    l.round_index = round;
                    self.backtracks += 1;
                }
            }
            self.current_focus = None;
        }
        // Pick the next still-active lead, or none if the frontier is exhausted.
        if let Some(idx) = self.open_leads.iter().position(|l| l.status == "pending" || l.status == "investigating") {
            self.current_focus = Some(idx);
            self.open_leads[idx].status = "investigating".to_string();
            self.open_leads[idx].round_index = round;
            true
        } else {
            false
        }
    }

    /// Add a verified action.
    pub fn add_action(&mut self, action: VerifiedAction) {
        self.verified_actions.push(action);
        self.updated_at = Utc::now();
    }

    /// Advance to the next phase.
    pub fn advance_phase(&mut self, phase: IrPhase) {
        self.phase = phase;
        self.updated_at = Utc::now();
    }

    /// Mark task as completed.
    pub fn complete(&mut self) {
        self.phase = IrPhase::Completed;
        // Clear stale resume metadata so a completed contract is not resumable.
        self.phase_before_block = None;
        self.updated_at = Utc::now();
    }

    /// Mark task as blocked.
    pub fn block(&mut self, reason: String) {
        // Save the current phase before blocking so it can be restored on unblock
        // Guard against double-blocking overwriting a previously saved phase.
        if self.phase != IrPhase::Blocked {
            self.phase_before_block = Some(self.phase);
        }
        self.phase = IrPhase::Blocked;
        self.blocked_reason = Some(reason);
        self.updated_at = Utc::now();
    }

    /// Unblock a previously blocked task, restoring the original phase.
    /// This is called when resuming a blocked contract with new user instructions.
    pub fn unblock(&mut self) {
        // Restore the original phase (or default to Collection if not saved)
        self.phase = self.phase_before_block.unwrap_or(IrPhase::Collection);
        self.phase_before_block = None;
        self.blocked_reason = None;
        self.updated_at = Utc::now();
    }

    /// Add a domain-agnostic record (requirement / artifact / fact). Cap at 100.
    pub fn add_record(&mut self, mut record: TaskRecord) {
        record.round_index = self.current_round;
        if record.updated_at.is_none() {
            record.updated_at = Some(Utc::now());
        }
        self.records.push(record);
        if self.records.len() > 100 {
            self.records.remove(0);
        }
        self.updated_at = Utc::now();
    }

    /// Seed the contract with a distilled prior-work handoff as an UNTRUSTED
    /// record plus a labelled manager note, so a resumed Expert task treats
    /// earlier Instant findings as leads/hypotheses, not verified facts.
    pub fn seed_untrusted_handoff(&mut self, handoff: &str) -> usize {
        let h = handoff.trim();
        if h.is_empty() {
            return 0;
        }
        self.prior_handoff = Some(h.to_string());
        self.manager_notes.push(
            "[UNVERIFIED prior-session handoff seeded; see 'Prior Session Context'. Treat as leads, re-audit before promotion.]".to_string()
        );        self.records.push(TaskRecord {
            id: uuid::Uuid::new_v4().to_string(),
            kind: "fact".to_string(),
            title: "Prior-session handoff (untrusted)".to_string(),
            status: "untrusted".to_string(),
            integrity: "suspect".to_string(),
            evidence_summary: h.chars().take(800).collect(),
            evidence_path: None,
            phase: None,
            round_index: self.current_round,
            updated_at: Some(Utc::now()),
            parent_task_id: None,
            depends_on: Vec::new(),
        });
        if self.records.len() > 100 {
            self.records.remove(0);
        }
        self.updated_at = Utc::now();
        1
    }

    /// Store the shared Orchestrator plan as the durable plan truth-source.
    pub fn set_orchestrator_plan(&mut self, plan: &serde_json::Value) {
        self.orchestrator_plan = Some(serde_json::to_string(plan).unwrap_or_else(|_| plan.to_string()));
        self.updated_at = Utc::now();
    }

    /// Deserialize the persisted Orchestrator plan back into a JSON value.
    pub fn orchestrator_plan_value(&self) -> Option<serde_json::Value> {
        self.orchestrator_plan
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
    }

    /// Serialize to JSON for storage.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize TaskContract: {}", e))
    }

    /// Deserialize from JSON.
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("Failed to deserialize TaskContract: {}", e))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_audits_default_empty_and_roundtrip() {
        // Old contracts without the new field must load with an empty list.
        let old = r#"{"id":"c1","original_task":"t","phase":"collection","scope":"s","hypothesis":"","verified_findings":[],"verified_actions":[],"open_leads":[],"remaining_work":[],"current_focus":null,"backtracks":0,"current_round":0,"max_rounds":10,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","manager_notes":[],"records":[]}"#;
        let c: TaskContract = serde_json::from_str(old).unwrap();
        assert!(c.round_audits.is_empty());

        let mut c2 = TaskContract::new("c2".into(), "task".into(), "s".into(), 5);
        c2.round_audits.push(RoundAudit {
            round_index: 1,
            completion: "incomplete".into(),
            integrity: "suspect".into(),
            note: "port scan cites no evidence".into(),
            supported_facts: vec!["443 open".into()],
            gaps: vec!["need process mapping".into()],
        });
        let json = c2.to_json().unwrap();
        let back: TaskContract = serde_json::from_str(&json).unwrap();
        assert_eq!(back.round_audits.len(), 1);
        assert_eq!(back.round_audits[0].supported_facts, vec!["443 open"]);
        assert_eq!(back.round_audits[0].note, "port scan cites no evidence");
    }

    #[test]
    fn task_record_old_json_backward_compatible() {
        // A legacy blob without the new tree fields must still deserialize,
        // defaulting to "no parent / no deps" (spec §7.8.1 additive).
        let old = r#"{"id":"x","kind":"artifact","title":"t","status":"completed","integrity":"clean","evidence_summary":"s","evidence_path":null,"phase":null,"round_index":0,"updated_at":null}"#;
        let r: TaskRecord = serde_json::from_str(old).unwrap();
        assert_eq!(r.parent_task_id, None);
        assert!(r.depends_on.is_empty());
    }

    #[test]
    fn orchestrator_plan_roundtrips_and_defaults_none() {
        // A legacy contract blob without the new field deserializes with None.
        // Pre-change schema: all currently-required fields present, but the new
        // `orchestrator_plan` (and `pending_plan`) omitted -> must default to None.
        let old = r#"{"id":"c1","original_task":"t","phase":"collection","scope":"s","hypothesis":"","verified_findings":[],"verified_actions":[],"open_leads":[],"current_round":0,"max_rounds":5,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","manager_notes":[]}"#;
        let c: TaskContract = TaskContract::from_json(old).unwrap();
        assert_eq!(c.orchestrator_plan, None, "absent field must default to None");

        // set_orchestrator_plan stores a serialized JSON value; value() restores it.
        let mut c2 = TaskContract::new("c2".into(), "task".into(), "scope".into(), 5);
        assert_eq!(c2.orchestrator_plan, None);
        let plan = serde_json::json!({ "subtask": "recon" });
        c2.set_orchestrator_plan(&plan);
        assert!(c2.orchestrator_plan.is_some());
        assert_eq!(c2.orchestrator_plan_value(), Some(plan.clone()));

        // Full contract round-trip preserves the field.
        let back = TaskContract::from_json(&c2.to_json().unwrap()).unwrap();
        assert_eq!(back.orchestrator_plan_value(), Some(plan));
    }

    #[test]
    fn task_record_tree_fields_roundtrip() {
        let json = r#"{"id":"sub-1","kind":"subtask","title":"Parallel worker: scan","status":"completed","integrity":"clean","evidence_summary":"found","evidence_path":null,"phase":"collection","round_index":0,"updated_at":null,"parent_task_id":"round-0","depends_on":["d1"]}"#;
        let r: TaskRecord = serde_json::from_str(json).unwrap();
        assert_eq!(r.parent_task_id.as_deref(), Some("round-0"));
        assert_eq!(r.depends_on, vec!["d1"]);
        // And it round-trips.
        let back: TaskRecord = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.parent_task_id.as_deref(), Some("round-0"));
    }
}

