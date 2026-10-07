//! P3 — `allowed-tools` → 意图窄化的 run 局部授权。
//!
//! 天花板不是"名单长短"的审美问题：现有匹配器只为三个工具准备了意图臂
//! （`sys_process` 的 `action=="kill"`、`ir_persistence` 的 `remove|delete`、
//! `shell_exec` 的 kill/service 命令头）。放开别的名字必须先新写一条臂，
//! 那是在扩大旁路面，属独立决策，不在本期。
//!
//! frontmatter 层的键名是 `allowed-tools`；Rust 侧字段是 `allowed_tools`。

use crate::managed::permission_profile::{PermissionProfile, PreauthorizedAction};
use super::schema::Finding;

/// 一份技能声明的 `allowed-tools` 经过天花板后的结果。
#[derive(Debug, Default)]
pub struct GrantPlan {
    /// 能安全落地的动作类（喂给 `PermissionProfile`）。
    pub actions: Vec<PreauthorizedAction>,
    /// 声明里被拒/被忽略的条目，带原因。
    pub findings: Vec<Finding>,
}

/// The only tool names that have an intent-narrowed arm in
/// `check_preauthorization`. Granting anything else would mean granting an
/// unbounded tool name, so it stays inert until a narrow arm exists for it —
/// and adding one is a separate decision, not this phase.
pub fn plan_grants(allowed_tools: &[String]) -> GrantPlan {
    const SYS_PROCESS: &str = "sys_process";
    const IR_PERSISTENCE: &str = "ir_persistence";
    const SHELL_EXEC: &str = "shell_exec";

    let mut actions: Vec<PreauthorizedAction> = Vec::new();
    let mut findings: Vec<Finding> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in allowed_tools {
        let raw = entry.trim().to_string();
        if raw.is_empty() || !seen.insert(raw.clone()) {
            continue; // an empty or repeated declaration is evaluated once
        }
        if raw.contains('(') || raw.contains(')') {
            findings.push(Finding::UnsupportedGrantSyntax { raw });
            continue;
        }
        let mapped: Vec<PreauthorizedAction> = match raw.as_str() {
            SYS_PROCESS => vec![PreauthorizedAction::KillProcess],
            IR_PERSISTENCE => vec![PreauthorizedAction::RemovePersistence],
            SHELL_EXEC => vec![
                PreauthorizedAction::KillProcess,
                PreauthorizedAction::StopService,
            ],
            tool => {
                findings.push(Finding::GrantUnsupported {
                    tool: tool.to_string(),
                    reason: "no intent-narrowed arm; a bare name grant is unbounded",
                });
                Vec::new()
            }
        };
        for action in mapped {
            if !actions.contains(&action) {
                actions.push(action);
            }
        }
    }

    GrantPlan { actions, findings }
}

/// The run-scoped profile fragment for one loaded skill. Field obligations are
/// locked here (spec §3.2): `task_id = "skill:<name>"`, `allow_all = false`,
/// no expiry — the run owns its lifetime and dropping it *is* the revocation.
pub fn fragment(skill_name: &str, plan: &GrantPlan) -> PermissionProfile {
    let mut profile = PermissionProfile::new(format!("skill:{skill_name}"));
    for action in &plan.actions {
        profile.authorize(action.clone());
    }
    profile
}

/// The facts a grant decision needs, all taken as already-read strings so the
/// decision itself stays pure and testable.
#[derive(Debug, Clone)]
pub struct GrantFacts<'a> {
    /// SHA-256 of the SKILL.md as it is on disk right now.
    pub current_hash: &'a str,
    /// Hash recorded in `.foxir-source.json` (`None` = unbound).
    pub recorded_hash: Option<&'a str>,
    /// Hash the user approved, from `skills_state.json`.
    pub consented_hash: Option<&'a str>,
    /// Hash the user refused, from `skills_state.json`.
    pub declined_hash: Option<&'a str>,
    /// The skill's declared `allowed-tools`.
    pub tools: Vec<String>,
}

/// Whether a loaded skill's declared grants may be active right now.
#[derive(Debug, Clone, PartialEq)]
pub enum GrantState {
    /// Nothing recorded to bind content to, so nothing can be activated.
    Unbound,
    /// The file changed since the manifest was written: revoke and re-ask.
    Drifted { recorded: String, current: String },
    /// Approved for exactly this content: these actions may be pre-authorized.
    Consent { actions: Vec<PreauthorizedAction> },
    /// Never asked about this content yet: ask once, activate nothing for now.
    Pending { tools: Vec<String> },
    /// Refused for exactly this content: do not ask again, do not activate.
    Declined,
}

/// Decide whether a skill's declared `allowed-tools` may be active.
///
/// Order matters and is the safety property: an unbound or drifted file can
/// never reach the consent check, so a stale approval cannot outlive the content
/// it was given for.
pub fn grant_state(facts: &GrantFacts) -> GrantState {
    let Some(recorded) = facts.recorded_hash else {
        return GrantState::Unbound;
    };
    if recorded != facts.current_hash {
        return GrantState::Drifted {
            recorded: recorded.to_string(),
            current: facts.current_hash.to_string(),
        };
    }
    if facts.consented_hash == Some(facts.current_hash) {
        return GrantState::Consent {
            actions: plan_grants(&facts.tools).actions,
        };
    }
    if facts.declined_hash == Some(facts.current_hash) {
        return GrantState::Declined;
    }
    GrantState::Pending {
        tools: facts.tools.clone(),
    }
}

/// One audit record for a call that went through *without* prompting.
///
/// The judgement carries its own report payload (skill, which content, which
/// tool, why it counted as narrowed). `skill_md_hash` is the first 8 hex chars:
/// an identity for humans and dedup — every security judgement uses the full
/// hash kept in state, never this truncation.
pub fn audit_line(
    skill: &str,
    skill_md_hash: &str,
    tool: &str,
    intent: &str,
    session: &str,
) -> String {
    let record = serde_json::json!({
        "ts": epoch_seconds(),
        "skill": skill,
        "skill_md_hash": skill_md_hash.chars().take(8).collect::<String>(),
        "tool": tool,
        "intent": intent,
        "session": session,
        "decision": "auto-granted",
    });
    let mut line = record.to_string();
    line.push('\n');
    line
}

/// Append one durable line. `write_all` and `flush` are *not* durability; the
/// `sync_all` is what makes the record exist before the privileged call runs.
/// An error here means the caller must fall back to prompting, not proceed.
pub fn append_audit_line(path: &std::path::Path, line: &str) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open {}: {}", path.display(), e))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("write {}: {}", path.display(), e))?;
    file.flush()
        .map_err(|e| format!("flush {}: {}", path.display(), e))?;
    file.sync_all()
        .map_err(|e| format!("sync {}: {}", path.display(), e))?;
    Ok(())
}

fn epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Did this tool result actually load a skill's instructions?
///
/// Only an instruction load makes a grant live: a directory listing, a
/// companion file, or a failed call must not count (spec §3.5).
pub fn loaded_skill_name(tool_name: &str, result: &serde_json::Value) -> Option<String> {
    if tool_name != "skill_read_file" {
        return None;
    }
    let path = result.get("path").and_then(|v| v.as_str())?;
    if !path.eq_ignore_ascii_case("SKILL.md") {
        return None;
    }
    result.get("content").and_then(|v| v.as_str())?;
    result
        .get("skill")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// One skill's live grant inside a single run: which skill, which content,
/// which session, and the intent-narrowed actions it may take.
#[derive(Debug)]
pub struct SkillGrant {
    pub skill: String,
    /// Full `skill_md_hash`, never the 8-char head. The ledger records the head,
    /// but every judgement uses this.
    pub full_hash: String,
    pub session: String,
    pub profile: PermissionProfile,
}

/// Outcome of asking the ledger to honour a call without prompting.
#[derive(Debug, Clone, PartialEq)]
pub enum AuditOutcome {
    /// Covered by a consented grant *and* audited: skip the prompt.
    Bypassed { skill: String },
    /// No consented grant covers this specific call.
    NoGrant,
    /// A grant covers it but the audit record could not be written, so the call
    /// must go through the normal prompt instead of taking effect unrecorded.
    AuditFailed { skill: String, error: String },
}

/// What the permission gate consults for skill-derived bypasses in one run.
///
/// Deliberately *not* merged into the run's pre-authorization profile: a bypass
/// that goes through that path could not be audited, and an un-audited bypass is
/// the exact hole this ledger exists to close.
#[derive(Debug)]
pub struct SkillGrantLedger {
    audit_path: std::path::PathBuf,
    grants: Vec<SkillGrant>,
}

impl SkillGrantLedger {
    pub fn new(audit_path: std::path::PathBuf) -> Self {
        Self {
            audit_path,
            grants: Vec::new(),
        }
    }

    pub fn add(&mut self, grant: SkillGrant) {
        self.grants.push(grant);
    }

    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Audit first, effect after. The narrowing lives in the profile's own
    /// intent arms, so `sys_process(action="suspend")` is not covered by a
    /// `sys_process` kill grant.
    pub fn authorize_with_audit(&self, tool: &str, args: &serde_json::Value, intent: &str) -> AuditOutcome {
        let Some(grant) = self
            .grants
            .iter()
            .find(|grant| crate::managed::permission_profile::check_preauthorization(&grant.profile, tool, args))
        else {
            return AuditOutcome::NoGrant;
        };
        let line = audit_line(&grant.skill, &grant.full_hash, tool, intent, &grant.session);
        match append_audit_line(&self.audit_path, &line) {
            Ok(()) => AuditOutcome::Bypassed {
                skill: grant.skill.clone(),
            },
            Err(error) => AuditOutcome::AuditFailed {
                skill: grant.skill.clone(),
                error,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_audit_line, audit_line, fragment, grant_state, loaded_skill_name, plan_grants,
        AuditOutcome, GrantFacts, GrantPlan, GrantState, SkillGrant, SkillGrantLedger,
    };
    use crate::managed::permission_profile::PreauthorizedAction;
    use crate::managed::permission_profile::PreauthorizedAction as Act;
    use crate::skill::schema::Finding;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    const HASH: &str = "0fe06237ce01efc6e9c656f853ab1f02ee4089038818d347666f4fb8e1d39d74";

    fn temp_grants_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rs_grants_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ledger_with(
        dir: &std::path::Path,
        skill: &str,
        hash: &str,
        tools: &[&str],
        session: &str,
    ) -> SkillGrantLedger {
        let plan = plan_grants(&names(tools));
        let mut ledger = SkillGrantLedger::new(dir.join("grants.jsonl"));
        ledger.add(SkillGrant {
            skill: skill.to_string(),
            full_hash: hash.to_string(),
            session: session.to_string(),
            profile: fragment(skill, &plan),
        });
        ledger
    }

    /// 三条意图臂就是可授予的全部词汇表。
    #[test]
    fn grantable_tools_map_to_intent_actions_without_duplicates() {
        let plan = plan_grants(&names(&["sys_process", "ir_persistence", "shell_exec"]));
        assert!(
            plan.findings.is_empty(),
            "all three sit inside the ceiling: {:?}",
            plan.findings
        );
        assert!(plan.actions.contains(&Act::KillProcess));
        assert!(plan.actions.contains(&Act::StopService));
        assert!(plan.actions.contains(&Act::RemovePersistence));
        assert_eq!(
            plan.actions.len(),
            3,
            "overlapping declarations must collapse to a set: {:?}",
            plan.actions
        );
    }

    /// 天花板外：解析、上报、保持惰性 —— 绝不因为"技能声明了"就放宽。
    #[test]
    fn tools_without_an_intent_arm_stay_inert_and_are_reported() {
        let plan = plan_grants(&names(&["file_delete", "winrm", "cu_process_kill", "file_write"]));
        assert!(
            plan.actions.is_empty(),
            "a bare tool-name grant is unbounded: {:?}",
            plan.actions
        );
        assert_eq!(plan.findings.len(), 4, "{:?}", plan.findings);
        assert!(plan
            .findings
            .iter()
            .all(|finding| matches!(finding, Finding::GrantUnsupported { .. })));
    }

    /// `Bash(...)` 那类外部方言不是我们的语法，别猜它的意思。
    #[test]
    fn parenthesized_grant_syntax_is_rejected_as_unsupported() {
        let plan = plan_grants(&names(&["Bash(git commit)"]));
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
        assert!(
            matches!(
                &plan.findings[..],
                [Finding::UnsupportedGrantSyntax { raw }] if raw == "Bash(git commit)"
            ),
            "{:?}",
            plan.findings
        );
    }

    /// 碎片就是 `PermissionProfile`，但字段义务是写死的：带技能名、永不 allow_all、
    /// 没有到期时间（随 run 结束 drop 即回收）。
    #[test]
    fn fragment_is_scoped_to_the_skill_and_can_never_allow_all() {
        let plan = plan_grants(&names(&["sys_process", "ir_persistence"]));
        let profile = fragment("TriageFlow", &plan);

        assert_eq!(profile.task_id, "skill:TriageFlow");
        assert!(!profile.allow_all, "a skill grant is never a global bypass");
        assert!(profile.preauthorized.contains(&Act::KillProcess));
        assert!(profile.preauthorized.contains(&Act::RemovePersistence));
        assert!(
            !profile.preauthorized.contains(&Act::IsolateHost),
            "an action with no intent arm is never present: {:?}",
            profile.preauthorized
        );
        assert!(profile.expires_at.is_none(), "lifetime is the run, not a clock");
    }

    /// 内容绑定的前半：没有记录过的全长哈希，就绝不激活授权。
    /// consent 已经在册也不行 —— 那正是"文件被换过、授权还在"的洞。
    #[test]
    fn a_skill_without_a_recorded_hash_can_never_be_content_bound() {
        let state = grant_state(&GrantFacts {
            current_hash: "aaa",
            recorded_hash: None,
            consented_hash: Some("aaa"),
            declined_hash: None,
            tools: names(&["sys_process"]),
        });
        assert!(matches!(state, GrantState::Unbound), "{:?}", state);
    }

    /// 哈希漂了 = 撤权并重新征询；即便旧哈希曾获批也不生效。
    #[test]
    fn hash_drift_revokes_an_earlier_consent() {
        let state = grant_state(&GrantFacts {
            current_hash: "bbb",
            recorded_hash: Some("aaa"),
            consented_hash: Some("aaa"),
            declined_hash: None,
            tools: names(&["sys_process"]),
        });
        assert!(
            matches!(&state, GrantState::Drifted { recorded, current }
                if recorded == "aaa" && current == "bbb"),
            "{:?}",
            state
        );
    }

    /// 同意绑的是全长哈希：三个哈希一致才生效，并且只开出挣到的那些动作。
    #[test]
    fn consent_is_bound_to_the_full_hash() {
        let state = grant_state(&GrantFacts {
            current_hash: "aaa",
            recorded_hash: Some("aaa"),
            consented_hash: Some("aaa"),
            declined_hash: None,
            tools: names(&["sys_process", "winrm"]),
        });
        match state {
            GrantState::Consent { actions } => {
                assert_eq!(actions, vec![PreauthorizedAction::KillProcess], "only what the ceiling allows");
            }
            other => panic!("expected a consented grant, got {other:?}"),
        }
    }

    /// 三个哈希对上了但从没征询过 → 问一次（Pending 不激活任何东西）。
    #[test]
    fn a_hash_with_no_decision_on_record_asks_once() {
        let state = grant_state(&GrantFacts {
            current_hash: "aaa",
            recorded_hash: Some("aaa"),
            consented_hash: None,
            declined_hash: None,
            tools: names(&["sys_process"]),
        });
        assert!(
            matches!(&state, GrantState::Pending { tools } if tools == &vec!["sys_process".to_string()]),
            "{state:?}"
        );
    }

    /// 这一份内容被拒过就不再重复弹窗 —— 也不许顺手放行。
    #[test]
    fn a_declined_hash_is_sticky_for_that_exact_hash() {
        let state = grant_state(&GrantFacts {
            current_hash: "aaa",
            recorded_hash: Some("aaa"),
            consented_hash: None,
            declined_hash: Some("aaa"),
            tools: names(&["sys_process"]),
        });
        assert!(matches!(state, GrantState::Declined), "{state:?}");
    }

    /// 同一份内容先拒后批：以同意为准，但只有 consent 的哈希等于当前哈希才放行。
    #[test]
    fn consent_wins_over_an_older_refusal_for_the_same_hash() {
        let state = grant_state(&GrantFacts {
            current_hash: "aaa",
            recorded_hash: Some("aaa"),
            consented_hash: Some("aaa"),
            declined_hash: Some("aaa"),
            tools: names(&["sys_process"]),
        });
        assert!(matches!(state, GrantState::Consent { .. }), "{state:?}");
    }

    /// 一次调用有没有"真的把指令读进来"。只有指令加载才构成授权的起点：
    /// 列目录、读资源文件、失败的结果都不算。
    #[test]
    fn only_an_instruction_load_counts_as_a_skill_being_loaded() {
        use serde_json::json;

        assert_eq!(
            loaded_skill_name("skill_read_file", &json!({"skill":"TriageFlow","path":"SKILL.md","content":"# body"})),
            Some("TriageFlow".to_string()),
            "an instruction body was returned"
        );
        assert_eq!(
            loaded_skill_name("skill_read_file", &json!({"skill":"TriageFlow","path":"skill.md","content":"# body"})),
            Some("TriageFlow".to_string()),
            "the filename is matched case-insensitively, as the reader does"
        );
        assert_eq!(
            loaded_skill_name("skill_read_file", &json!({"skill":"TriageFlow","path":"reference.md","content":"<html>"})),
            None,
            "a companion file is not an instruction load"
        );
        assert_eq!(
            loaded_skill_name("skill_read_file", &json!({"skill":"TriageFlow","files":[]})),
            None,
            "listing the directory is not a load"
        );
        assert_eq!(
            loaded_skill_name("file_read", &json!({"skill":"TriageFlow","path":"SKILL.md","content":"# body"})),
            None,
            "a different tool cannot mark a skill loaded"
        );
    }

    /// 授权的核心：先落审计，再产生免审批效果；而且只放行窄化后的那一次调用。
    #[test]
    fn a_consent_grant_bypasses_only_the_narrowed_call_and_leaves_an_audit_line() {
        let dir = temp_grants_dir("bypass");
        let ledger = ledger_with(&dir, "TriageFlow", HASH, &["sys_process", "shell_exec"], "sess-9");

        let killed = ledger.authorize_with_audit(
            "sys_process",
            &serde_json::json!({ "action": "kill", "name": "evil.exe" }),
            "终止进程 evil.exe",
        );
        match killed {
            AuditOutcome::Bypassed { ref skill } if skill == "TriageFlow" => {}
            other => panic!("expected a bypassed call, got {other:?}"),
        }

        // 受害者样本：同一工具的其他动作不该被顺手放宽
        assert!(matches!(
            ledger.authorize_with_audit(
                "sys_process",
                &serde_json::json!({ "action": "suspend", "name": "x" }),
                "挂起进程",
            ),
            AuditOutcome::NoGrant
        ));
        // 命令头不是 kill/service 的 shell_exec 也不行（P3 入口条件的延伸）
        assert!(matches!(
            ledger.authorize_with_audit(
                "shell_exec",
                &serde_json::json!({ "command": "powershell -c \"echo taskkill\"" }),
                "执行命令",
            ),
            AuditOutcome::NoGrant
        ));
        assert!(matches!(
            ledger.authorize_with_audit(
                "shell_exec",
                &serde_json::json!({ "command": "taskkill /f /im evil.exe" }),
                "执行命令",
            ),
            AuditOutcome::Bypassed { .. }
        ));

        let text = std::fs::read_to_string(dir.join("grants.jsonl")).expect("one line was written");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "exactly the two bypassed calls: {lines:?}");
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["skill"], "TriageFlow");
        assert_eq!(first["tool"], "sys_process");
        assert_eq!(first["session"], "sess-9");
        assert_eq!(first["decision"], "auto-granted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 审计写不进去 = 不免审批。绝不出现"效果已产生而台账无记录"。
    #[test]
    fn an_unwritable_audit_ledger_forces_the_prompt_path() {
        let dir = temp_grants_dir("unwritable");
        // 把 grants.jsonl 的位置占成一个目录，写必然失败
        std::fs::create_dir_all(dir.join("grants.jsonl")).unwrap();
        let ledger = ledger_with(&dir, "TriageFlow", HASH, &["sys_process"], "sess-9");

        let outcome = ledger.authorize_with_audit(
            "sys_process",
            &serde_json::json!({ "action": "kill", "name": "evil.exe" }),
            "终止进程 evil.exe",
        );
        assert!(
            matches!(&outcome, AuditOutcome::AuditFailed { skill, .. } if skill == "TriageFlow"),
            "{outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正对照：空台账一次都不放行。
    #[test]
    fn an_empty_ledger_never_bypasses() {
        let dir = temp_grants_dir("empty");
        let ledger = SkillGrantLedger::new(dir.join("grants.jsonl"));
        assert!(ledger.is_empty());
        assert!(matches!(
            ledger.authorize_with_audit("sys_process", &serde_json::json!({ "action": "kill" }), "kill"),
            AuditOutcome::NoGrant
        ));
        assert!(!dir.join("grants.jsonl").exists(), "no bypass, no audit line");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 台账一行的形状：判定变体必须自带报备载荷（谁、哪份内容、哪个工具、为什么算窄化）。
    /// 记的是哈希前 8 位，仅供人读与去重；安全判断永远用全长。
    #[test]
    fn audit_line_is_one_json_object_carrying_the_decision_context() {
        let full = "0fe06237ce01efc6e9c656f853ab1f02ee4089038818d347666f4fb8e1d39d74";
        let line = audit_line("TriageFlow", full, "sys_process", "终止进程 evil.exe (action=kill)", "sess-1");
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("one JSONL record");

        assert_eq!(parsed["decision"], "auto-granted");
        assert_eq!(parsed["skill"], "TriageFlow");
        assert_eq!(parsed["tool"], "sys_process");
        assert_eq!(parsed["session"], "sess-1");
        assert_eq!(parsed["intent"], "终止进程 evil.exe (action=kill)");
        assert_eq!(parsed["skill_md_hash"], "0fe06237", "the first 8 hex chars only");
        assert!(parsed["ts"].is_i64(), "epoch seconds expected: {parsed}");
    }

    /// 台账写失败必须是"错误"，不能静默 —— 调用方据此回落到审批。
    #[test]
    fn audit_append_reports_failure_instead_of_dropping_the_record() {
        let dir = std::env::temp_dir().join(format!("rs_grants_audit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grants.jsonl");
        let line = audit_line("S", "aaaaaaaaaaaaaaaa", "sys_process", "kill", "s");

        append_audit_line(&path, &line).expect("a plain path appends");
        append_audit_line(&path, &line).expect("appending twice keeps both lines");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "both lines must survive: {text}");

        // 受害者样本：同名位置被目录占住，写不进去就只能报错
        let blocked_dir = dir.join("blocked");
        std::fs::create_dir_all(blocked_dir.join("grants.jsonl")).unwrap();
        let blocked = blocked_dir.join("grants.jsonl");
        let err = append_audit_line(&blocked, &line).expect_err("an unwritable target must error");
        assert!(
            err.contains("grants.jsonl"),
            "the error must name the file: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空声明（或全在天花板外）不该产出一个"看起来像授权"的空碎片。
    #[test]
    fn no_grantable_actions_means_no_fragment() {
        assert!(fragment("TriageFlow", &plan_grants(&names(&["winrm"]))).preauthorized.is_empty());
        assert!(fragment("TriageFlow", &GrantPlan::default()).preauthorized.is_empty());
    }

    /// 同一个名字写两遍不该产生两份动作或两条告警。
    #[test]
    fn repeated_declarations_are_evaluated_once() {
        let plan = plan_grants(&names(&["sys_process", "sys_process", "winrm", "winrm"]));
        assert_eq!(plan.actions, vec![Act::KillProcess], "{:?}", plan.actions);
        assert_eq!(plan.findings.len(), 1, "{:?}", plan.findings);
    }
}
