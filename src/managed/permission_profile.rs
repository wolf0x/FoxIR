//! Permission pre-authorization profiles for managed (long-horizon) tasks.
//!
//! In managed mode, tasks may run for hours without human intervention.
//! The permission system would normally pause for approval on containment
//! actions (kill process, disable service, remove persistence). This module
//! provides pre-authorization profiles that allow specific action classes
//! to proceed without waiting for human approval.
//!
//! # Safety Model
//!
//! Pre-authorization is scoped by action class:
//! - **Read-only**: Always pre-authorized (no risk)
//! - **Containment**: Can be pre-authorized (kill process, isolate host)
//! - **Eradication**: Can be pre-authorized with restrictions (delete known malware)
//! - **Destructive**: NEVER pre-authorized (format disk, delete system files)
//!
//! Pre-authorization profiles are stored per-task and expire when the task completes.
//! The Auditor verifies all pre-authorized actions post-hoc.
//!
//! # Integration
//!
//! The ManagedRunner checks the pre-authorization profile before each Executor round.
//! If an action is pre-authorized, it bypasses the normal permission gate.
//! If not pre-authorized, the task blocks until human approval (Phase 6 human gate).

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Pre-authorization profile for a managed task.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionProfile {
    /// Task ID this profile applies to.
    pub task_id: String,
    /// Action classes that are pre-authorized.
    pub preauthorized: HashSet<PreauthorizedAction>,
    /// When true, bypasses the permission gate entirely (global CRON auto-approve).
    #[serde(default)]
    pub allow_all: bool,
    /// When this profile was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When this profile expires (optional — defaults to task completion).
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Action classes that can be pre-authorized.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreauthorizedAction {
    /// Kill any process (by name or PID).
    KillProcess,
    /// Kill a specific process (by name).
    KillProcessNamed(String),
    /// Stop/disable a service.
    StopService,
    /// Remove persistence (registry, scheduled task, etc.).
    RemovePersistence,
    /// Isolate host (block network).
    IsolateHost,
    /// Delete files in specific directories (e.g., temp, malware drops).
    DeleteFilesInPath(String),
    /// Disable user account.
    DisableAccount,
}

impl PermissionProfile {
    /// Create a new permission profile for a task.
    pub fn new(task_id: String) -> Self {
        Self {
            task_id,
            preauthorized: HashSet::new(),
            allow_all: false,
            created_at: chrono::Utc::now(),
            expires_at: None,
        }
    }

    /// Create a profile with standard IR containment pre-authorizations.
    pub fn ir_containment(task_id: String) -> Self {
        let mut profile = Self::new(task_id);
        profile.preauthorized.insert(PreauthorizedAction::KillProcess);
        profile.preauthorized.insert(PreauthorizedAction::StopService);
        profile.preauthorized.insert(PreauthorizedAction::RemovePersistence);
        profile
    }

    /// Create a profile with full IR pre-authorizations (including eradication).
    pub fn ir_full(task_id: String) -> Self {
        let mut profile = Self::ir_containment(task_id);
        profile.preauthorized.insert(PreauthorizedAction::IsolateHost);
        profile.preauthorized.insert(PreauthorizedAction::DisableAccount);
        profile
    }

    /// Check if an action is pre-authorized.
    pub fn is_preauthorized(&self, action: &PreauthorizedAction) -> bool {
        // Check exact match
        if self.preauthorized.contains(action) {
            return true;
        }

        // Check wildcard match (e.g., KillProcess matches KillProcessNamed)
        match action {
            PreauthorizedAction::KillProcessNamed(_) => {
                self.preauthorized.contains(&PreauthorizedAction::KillProcess)
            }
            PreauthorizedAction::DeleteFilesInPath(path) => {
                self.preauthorized.iter().any(|a| {
                    matches!(a, PreauthorizedAction::DeleteFilesInPath(p) if path.starts_with(p))
                })
            }
            _ => false,
        }
    }

    /// Add a pre-authorized action.
    pub fn authorize(&mut self, action: PreauthorizedAction) {
        self.preauthorized.insert(action);
    }

    /// Remove a pre-authorized action.
    pub fn revoke(&mut self, action: &PreauthorizedAction) {
        self.preauthorized.remove(action);
    }

    /// Check if the profile has expired.
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => chrono::Utc::now() > exp,
            None => false, // No expiration — valid until task completes
        }
    }
}

/// Shell launchers whose own presence only means "the real command follows".
/// Skipped (with their switches) so a grant can never be earned by a word that
/// merely appears inside a quoted argument.
const LAUNCHERS: [&str; 6] = [
    "powershell",
    "powershell.exe",
    "pwsh",
    "cmd",
    "cmd.exe",
    "call",
];

/// Command heads covered by a `KillProcess` grant. Anything not listed here
/// falls through to the normal permission prompt — deliberately conservative.
const KILL_HEADS: [&str; 4] = ["taskkill", "stop-process", "kill", "spps"];

fn strip_quotes(token: &str) -> &str {
    token.trim_matches(|c: char| c == '"' || c == '\'' || c == '`')
}

/// The command's own tokens, starting at the first one that would execute.
///
/// Leading launchers and switches are skipped (repeatedly, so `cmd /c
/// powershell -c "..."` resolves to its real head); once the head is found the
/// remaining non-switch tokens follow it. A word that only appears as an
/// argument is therefore never the head.
fn executable_tokens(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at_head = false;
    for raw in command
        .to_lowercase()
        .split_whitespace()
        .flat_map(|token| token.split([';', '|', '&', '(', ')', '>']))
    {
        let token = strip_quotes(raw);
        if token.is_empty() {
            continue;
        }
        if !at_head {
            if LAUNCHERS.contains(&token) || token.starts_with('-') || token.starts_with('/') {
                continue;
            }
            at_head = true;
        } else if token.starts_with('-') || token.starts_with('/') {
            continue;
        }
        out.push(token.to_string());
    }
    out
}

/// True when the executed command is a process kill.
fn head_is_kill(command: &str) -> bool {
    executable_tokens(command)
        .first()
        .map(|head| KILL_HEADS.contains(&head.as_str()))
        .unwrap_or(false)
}

/// True when the executed command stops a service (`Stop-Service`, or `sc stop`).
fn head_is_service_stop(command: &str) -> bool {
    let tokens = executable_tokens(command);
    match tokens.first().map(String::as_str) {
        Some("stop-service") => true,
        Some("sc") => tokens.get(1).map(String::as_str) == Some("stop"),
        _ => false,
    }
}

/// Check if a tool call is pre-authorized by the permission profile.
///
/// This is called by the ManagedRunner before each Executor round.
/// Returns true if the action can proceed without human approval.
pub fn check_preauthorization(
    profile: &PermissionProfile,
    tool_name: &str,
    args: &serde_json::Value,
) -> bool {
    if profile.is_expired() {
        return false;
    }
    // Global CRON auto-approve: run arbitrary commands without pausing for approval.
    // Only set when the CRON "auto-approve commands" toggle is on (default off).
    if profile.allow_all {
        return true;
    }

    // Map tool calls to pre-authorized actions
    match tool_name {
        "shell_exec" => {
            let cmd = args["command"].as_str().unwrap_or("");

            // Process kill — judged on the command head, not on any substring.
            if head_is_kill(cmd) {
                return profile.is_preauthorized(&PreauthorizedAction::KillProcess);
            }

            // Service stop
            if head_is_service_stop(cmd) {
                return profile.is_preauthorized(&PreauthorizedAction::StopService);
            }

            false
        }

        "ir_persistence" => {
            let action = args["action"].as_str().unwrap_or("");
            if action == "remove" || action == "delete" {
                return profile.is_preauthorized(&PreauthorizedAction::RemovePersistence);
            }
            false
        }

        "sys_process" => {
            let action = args["action"].as_str().unwrap_or("");
            if action == "kill" {
                return profile.is_preauthorized(&PreauthorizedAction::KillProcess);
            }
            false
        }

        _ => false,
    }
}

/// Reverse mapping of [`check_preauthorization`]: return the concrete tool names
/// a worker is authorized to use (beyond its read-only base) given a profile's
/// pre-authorized action classes. Grounded strictly on the `tool_name` arm set
/// already recognized by [`check_preauthorization`] — actions with no recognized
/// tool arm (IsolateHost / DeleteFilesInPath / DisableAccount) contribute none.
/// `allow_all` is a permission-gate bypass, not a tool-set grant, so it also
/// contributes none here.
pub fn authorized_tool_names(profile: &PermissionProfile) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |n: &str| {
        if !out.iter().any(|x| x == n) {
            out.push(n.to_string());
        }
    };
    for action in &profile.preauthorized {
        match action {
            PreauthorizedAction::KillProcess
            | PreauthorizedAction::KillProcessNamed(_) => {
                push("shell_exec"); // taskkill / stop-process
                push("sys_process"); // action == "kill"
            }
            PreauthorizedAction::StopService => {
                push("shell_exec"); // sc stop / stop-service
            }
            PreauthorizedAction::RemovePersistence => {
                push("ir_persistence"); // action == remove/delete
            }
            PreauthorizedAction::IsolateHost
            | PreauthorizedAction::DeleteFilesInPath(_)
            | PreauthorizedAction::DisableAccount => {}
        }
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with(actions: &[PreauthorizedAction]) -> PermissionProfile {
        let mut p = PermissionProfile::new("p-test".to_string());
        for a in actions {
            p.authorize(a.clone());
        }
        p
    }

    /// 规格 §3.0 的入口条件：授权跟着"命令头"走，不是跟着任意位置的子串走。
    /// 今天是 `lower.contains("taskkill")`，于是 `echo taskkill` 这种"只是提到"
    /// 的命令也能免审批 —— 在把更多 profile 交给它之前必须先修。
    #[test]
    fn kill_grant_follows_the_command_head_not_a_substring() {
        let p = profile_with(&[
            PreauthorizedAction::KillProcess,
            PreauthorizedAction::StopService,
        ]);
        let cmd = |c: &str| serde_json::json!({ "command": c });

        // 真动作：动词就是被执行的命令头（含合法的启动器前缀）
        assert!(
            check_preauthorization(&p, "shell_exec", &cmd("taskkill /f /im evil.exe")),
            "a real taskkill is pre-authorized"
        );
        assert!(
            check_preauthorization(
                &p,
                "shell_exec",
                &cmd("powershell -NoProfile -Command \"taskkill /f /im evil.exe\"")
            ),
            "a launcher prefix must not hide a real kill"
        );
        assert!(
            check_preauthorization(&p, "shell_exec", &cmd("Stop-Process -Name evil")),
            "Stop-Process is a kill"
        );
        assert!(
            check_preauthorization(&p, "shell_exec", &cmd("sc stop evilsvc")),
            "sc stop is a service action"
        );

        // 受害样本：动词只是参数里的字符串，没有被执行
        assert!(
            !check_preauthorization(&p, "shell_exec", &cmd("powershell -c \"echo taskkill\"")),
            "a mention inside an argument is not a kill"
        );
        assert!(
            !check_preauthorization(
                &p,
                "shell_exec",
                &cmd("findstr /i taskkill C:\\Users\\me\\notes.txt")
            ),
            "searching for a word is not running it"
        );
        assert!(
            !check_preauthorization(&p, "shell_exec", &cmd("del C:\\temp\\x")),
            "a kill grant says nothing about deletion"
        );
    }

    /// 为什么授权判据不直接用 `parse_intent().verb == Stop`：这条把上游分类器的
    /// 实测行为钉住（2026-10-07 跑出来的，不是推断）——它按子串认动词，所以
    /// "只是搜这个词"也会被认成 Stop。头部匹配器（`executable_tokens`）不受影响。
    #[test]
    fn policy_verb_classifies_an_argument_mention_as_stop_so_head_matching_is_required() {
        use crate::policy::parse::{parse_intent, Verb};
        for cmd in [
            "cmd /c \"findstr taskkill notes.txt\"",
            "findstr /i taskkill C:\\Users\\me\\notes.txt",
        ] {
            assert_eq!(
                parse_intent(cmd, "cmd").verb,
                Verb::Stop,
                "characterization: the policy verb classifier treats a mentioned word as Stop"
            );
            assert!(
                !head_is_kill(cmd),
                "our grant matcher must not: {cmd}"
            );
        }
    }

    #[test]
    fn authorized_tool_names_maps_grounded_actions() {
        let p = profile_with(&[
            PreauthorizedAction::KillProcess,
            PreauthorizedAction::StopService,
            PreauthorizedAction::RemovePersistence,
        ]);
        let mut names = authorized_tool_names(&p);
        names.sort();
        // Only names recognized by check_preauthorization's tool arms.
        assert_eq!(names, vec!["ir_persistence", "shell_exec", "sys_process"]);
    }

    #[test]
    fn authorized_tool_names_named_kill_wildcards_to_kill() {
        let p = profile_with(&[PreauthorizedAction::KillProcessNamed("evil.exe".into())]);
        let names = authorized_tool_names(&p);
        assert!(names.contains(&"shell_exec".to_string()));
        assert!(names.contains(&"sys_process".to_string()));
    }

    #[test]
    fn authorized_tool_names_unmapped_actions_contribute_none() {
        let p = profile_with(&[
            PreauthorizedAction::IsolateHost,
            PreauthorizedAction::DeleteFilesInPath("C:\\temp".into()),
            PreauthorizedAction::DisableAccount,
        ]);
        assert!(authorized_tool_names(&p).is_empty(),
            "no recognized tool arm -> no authorized tools");
    }

    #[test]
    fn authorized_tool_names_empty_and_allow_all_are_empty() {
        assert!(authorized_tool_names(&PermissionProfile::new("x".into())).is_empty());
        let mut all = PermissionProfile::new("x".into());
        all.allow_all = true;
        assert!(authorized_tool_names(&all).is_empty(),
            "allow_all is a permission-gate bypass, not a tool-set grant");
    }

    #[test]
    fn authorized_tool_names_dedupes() {
        let p = profile_with(&[PreauthorizedAction::KillProcess, PreauthorizedAction::KillProcessNamed("a".into())]);
        let names = authorized_tool_names(&p);
        assert_eq!(names.iter().filter(|n| *n == "shell_exec").count(), 1);
        assert_eq!(names.iter().filter(|n| *n == "sys_process").count(), 1);
    }
}
