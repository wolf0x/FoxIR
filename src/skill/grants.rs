//! P3 — `allowed-tools` → run 局部免审批。
//!
//! 免审批的词汇表是**工具名**：声明里除"任意代码执行"之外的任何名字都可以被
//! 授权，包括外设化之后的 `ext_*` 和 MCP 名字（名字必须先存在于注册表才会被
//! 调用，所以拼错的声明只是不生效，不会放大射程）。
//!
//! 两类例外不走裸名：
//! - `sys_process` / `ir_persistence` / `shell_exec` 已有意图臂，按臂窄化授权
//!   （`check_preauthorization`），裸名不会比臂更宽。
//! - `app_launch` / `winrm` 的射程由命令文本决定，而兜底只有 Delete/Format 两个
//!   动词可用，不足以覆盖任意代码执行 ⇒ 声明被拒并上报。
//!
//! 最终判定仍归五类：`PermissionChecker` 在采用任何技能授权之前先查
//! [`crate::permission::grant_blocked_by_category`]，意图落进用户已关掉的类别
//! 就不使用授权（也不写台账）。
//!
//! frontmatter 层的键名是 `allowed-tools`；Rust 侧字段是 `allowed_tools`。

use crate::managed::permission_profile::{PermissionProfile, PreauthorizedAction};
use super::schema::Finding;

/// 一份技能声明的 `allowed-tools` 经过天花板后的结果。
#[derive(Debug, Default)]
pub struct GrantPlan {
    /// 能安全落地的动作类（喂给 `PermissionProfile`）。
    pub actions: Vec<PreauthorizedAction>,
    /// 按名字免审批的工具（`PermissionChecker` 逐名比对）。
    pub names: Vec<String>,
    /// 声明里被拒/被忽略的条目，带原因。
    pub findings: Vec<Finding>,
}

/// 有意图臂的三个名字：授权按臂生效，裸名不再额外放宽。
const NARROWED_TOOLS: [&str; 3] = ["sys_process", "ir_persistence", "shell_exec"];
/// 任意代码执行，且没有可用的窄臂：裸名绝不生效。
const ARBITRARY_CODE_TOOLS: [&str; 2] = ["app_launch", "winrm"];

/// Turn a skill's declared `allowed-tools` into the grants it may actually hold.
pub fn plan_grants(allowed_tools: &[String]) -> GrantPlan {
    let mut actions: Vec<PreauthorizedAction> = Vec::new();
    let mut names: Vec<String> = Vec::new();
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
        if ARBITRARY_CODE_TOOLS.contains(&raw.as_str()) {
            findings.push(Finding::GrantUnsupported {
                tool: raw,
                reason: "arbitrary code execution: a bare name has no bound",
            });
            continue;
        }
        if NARROWED_TOOLS.contains(&raw.as_str()) {
            let mapped: Vec<PreauthorizedAction> = match raw.as_str() {
                "sys_process" => vec![PreauthorizedAction::KillProcess],
                "ir_persistence" => vec![PreauthorizedAction::RemovePersistence],
                _ => vec![
                    PreauthorizedAction::KillProcess,
                    PreauthorizedAction::StopService,
                ],
            };
            for action in mapped {
                if !actions.contains(&action) {
                    actions.push(action);
                }
            }
            continue;
        }
        if !names.contains(&raw) {
            names.push(raw);
        }
    }

    GrantPlan { actions, names, findings }
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
    /// Approved for exactly this content: these grants may be active.
    Consent {
        actions: Vec<PreauthorizedAction>,
        names: Vec<String>,
    },
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
        let plan = plan_grants(&facts.tools);
        return GrantState::Consent {
            actions: plan.actions,
            names: plan.names,
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
/// which session, and the two shapes of what it may take — intent-narrowed
/// actions and bare tool names.
#[derive(Debug)]
pub struct SkillGrant {
    pub skill: String,
    /// Full `skill_md_hash`, never the 8-char head. The ledger records the head,
    /// but every judgement uses this.
    pub full_hash: String,
    pub session: String,
    pub profile: PermissionProfile,
    /// Tool names this skill exempts, verbatim from its declaration. A name
    /// that is not in the registry can never be called, so a wrong declaration
    /// is inert rather than dangerous.
    pub names: Vec<String>,
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

    /// Audit first, effect after. A call is covered either by a bare name from
    /// the declaration, or by one of the intent arms — `sys_process(action =
    /// "suspend")` is still not covered by a `sys_process` kill grant.
    pub fn authorize_with_audit(&self, tool: &str, args: &serde_json::Value, intent: &str) -> AuditOutcome {
        let Some(grant) = self
            .grants
            .iter()
            .find(|grant| {
                grant.names.iter().any(|name| name == tool)
                    || crate::managed::permission_profile::check_preauthorization(
                        &grant.profile, tool, args,
                    )
            })
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
            names: plan.names,
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

    /// 免审批的词汇表就是工具名：`ext_ir_scan` 这类外设化之后的 IR 工具、
    /// `file_delete` 这类整组动作都可接受的工具，都靠名字进名单。
    #[test]
    fn any_declared_name_becomes_a_live_grant_except_the_code_exec_trio() {
        let plan = plan_grants(&names(&["ir_memdump", "file_delete", "ext_ir_scan", "winrm"]));
        assert_eq!(
            plan.names,
            names(&["ir_memdump", "file_delete", "ext_ir_scan"]),
            "declared names are the grant vocabulary: {:?}",
            plan.names
        );
        assert!(
            !plan.names.contains(&"winrm".to_string()),
            "winrm runs arbitrary remote code, so a bare name must not grant it"
        );
        assert!(
            plan.findings.iter().any(
                |finding| matches!(finding, Finding::GrantUnsupported { tool, .. } if tool == "winrm")
            ),
            "a refused declaration must be reported, not swallowed: {:?}",
            plan.findings
        );
    }

    /// 三件套的射程由命令文本决定，而五类兜底只认 Delete/Format 两个动词，
    /// 不足以覆盖任意代码执行 —— 所以它们只走窄臂。
    #[test]
    fn code_execution_tools_are_never_granted_by_a_bare_name() {
        let plan = plan_grants(&names(&["shell_exec", "app_launch", "winrm"]));
        assert!(plan.names.is_empty(), "{:?}", plan.names);
        assert!(
            plan.actions.contains(&Act::KillProcess) && plan.actions.contains(&Act::StopService),
            "shell_exec keeps its narrowed arms: {:?}",
            plan.actions
        );
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
            GrantState::Consent { actions, names } => {
                assert_eq!(actions, vec![PreauthorizedAction::KillProcess], "only what the ceiling allows");
                assert!(names.is_empty(), "winrm is refused, so it cannot be a live name grant: {names:?}");
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

    /// 名字级授权同样"先落审计再放行"，而且作用域只到声明的那个名字。
    #[test]
    fn a_name_level_grant_bypasses_only_its_own_tool_and_leaves_an_audit_line() {
        let dir = temp_grants_dir("name_bypass");
        let ledger = ledger_with(&dir, "TriageFlow", HASH, &["ext_ir_scan"], "sess-9");

        match ledger.authorize_with_audit("ext_ir_scan", &serde_json::json!({ "args": "-s" }), "外部扫描") {
            AuditOutcome::Bypassed { ref skill } if skill == "TriageFlow" => {}
            other => panic!("expected a name-level bypass, got {other:?}"),
        }
        assert!(matches!(
            ledger.authorize_with_audit("ext_other_tool", &serde_json::json!({}), "别的工具"),
            AuditOutcome::NoGrant
        ), "the grant is scoped to the declared name");

        let text = std::fs::read_to_string(dir.join("grants.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1, "one bypass, one audit line: {text}");
        assert!(text.contains("\"tool\":\"ext_ir_scan\""), "{text}");
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
