//! Debrief（结论固化）— 会话结束后的「案例 writeup + 可选 SOP」双轨沉淀。
//!
//! 与旧 SOP 作者化的触发哲学不同：不问「是否含可复用流程」，而问
//! 「是否一个明确问题经过实质排查后收敛出明确结果」。门控全过才调 LLM，
//! 大部分闲聊/一次性查询零成本通过。
//!
//! 产物双轨：
//! - Case note（案例）：问题 → 根因 → 解法链路 → 弯路 → 证据，写
//!   `knowledge/writeups/<slug>.md`，自动进每轮知识预检索。
//! - SOP（流程）：仅当过程可复用才产出，走现有 `sops.json` + Wilson 评分。
//!
//! 纯确定性部分（门控/解析/渲染/归一化）无 I/O、无 LLM，全部可测；
//! LLM 编排集中在 [`run`]。

use std::sync::Arc;

use crate::model::openai::OpenAiProvider;
use crate::model::ChatMessage;

/// 实质过程的最低工具调用数（权威口径：事件流的 ToolResult 计数）。
pub const MIN_TOOL_CALLS: usize = 3;
/// 收敛结论的最低长度（chars）：过短的"回答"不算结论。
pub const MIN_ANSWER_CHARS: usize = 40;

/// 一次会话的结算信号（由 server 在会话结束时从历史与工具日志收集）。
#[derive(Debug, Clone, Default)]
pub struct SessionSignals {
    /// 会话首条用户消息（问题定义）。
    pub first_user_message: String,
    /// 会话尾部是否有用户确认（"解决了/好了/谢谢" 等短确认）。
    pub user_confirmed: bool,
    /// 实质工具调用数。
    pub tool_calls: usize,
    /// 用过的工具名（去重、保序）。
    pub tools_used: Vec<String>,
    /// 最后一次 assistant 文本回答（收敛结论）。
    pub final_answer: String,
    /// 最后一轮是否以文本回答结束（无未完成的工具调用）。
    pub ended_with_answer: bool,
}

/// 收敛结论判定：足够长，或较短但带明确结论标记。
/// 中文结论往往紧凑（"结论：暴力破解成功，已封禁"不到 30 字），硬字数阈值会误杀，所以标记词优先。
pub fn has_conclusion(answer: &str) -> bool {
    let n = answer.trim().chars().count();
    if n >= MIN_ANSWER_CHARS {
        return true;
    }
    const MARKERS: &[&str] = &[
        "结论", "根因", "原因是", "已修复", "已解决", "答案", "查明",
        "conclusion", "root cause", "fixed", "resolved", "answer is", "turns out",
    ];
    let l = answer.to_lowercase();
    n >= 15 && MARKERS.iter().any(|m| l.contains(m))
}

/// 门控：过程实质 + 结果收敛 +（问题明确 或 用户确认）。
pub fn should_debrief(sig: &SessionSignals) -> bool {
    if sig.tool_calls < MIN_TOOL_CALLS {
        return false;
    }
    if !sig.ended_with_answer {
        return false;
    }
    if !has_conclusion(&sig.final_answer) {
        return false;
    }
    is_problem_query(&sig.first_user_message) || sig.user_confirmed
}

/// 问题型任务判定：排查/分析/调查类触发词，或带具体对象（IP/路径/错误码）。
pub fn is_problem_query(msg: &str) -> bool {
    const ZH: &[&str] = &[
        "分析", "调查", "排查", "为什么", "如何修复", "怎么解决", "怎么回事", "失败",
        "报错", "异常", "不生效", "连不上", "打不开", "应急响应", "取证", "入侵",
    ];
    const EN: &[&str] = &[
        "investigate", "analyze", "analyse", "troubleshoot", "debug", "why does",
        "why is", "how to fix", "root cause", "not working", "fails", "failed",
    ];
    if ZH.iter().any(|w| msg.contains(w)) {
        return true;
    }
    let lower = msg.to_lowercase();
    if EN.iter().any(|w| lower.contains(w)) {
        return true;
    }
    has_concrete_target(msg)
}

/// 具体对象信号：IPv4、Windows/Unix 路径、0x 错误码。
fn has_concrete_target(msg: &str) -> bool {
    for tok in msg.split_whitespace() {
        let t = tok.trim_matches(|c: char| !(c.is_alphanumeric() || c == '.' || c == ':' || c == '\\' || c == '/' || c == '_'));
        // IPv4
        let parts: Vec<&str> = t.split('.').collect();
        if parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit())) {
            return true;
        }
        // 路径
        if (t.contains(":\\") || t.starts_with('/')) && t.chars().count() >= 6 {
            return true;
        }
        // 0x 错误码
        if t.len() >= 6 && t.starts_with("0x") && t[2..].chars().all(|c| c.is_ascii_hexdigit()) {
            return true;
        }
    }
    false
}

/// 用户短确认判定：去标点后等于确认词，或以确认词开头且最多带 2 个语气字（"好了哦"）。
/// "解决了，但还有个问题"不算确认。
pub fn is_ack(msg: &str) -> bool {
    let t = msg.trim();
    if t.is_empty() || t.chars().count() > 20 {
        return false;
    }
    let cleaned: String = t
        .chars()
        .filter(|c| c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(c))
        .collect::<String>()
        .to_lowercase();
    if cleaned.is_empty() {
        return false;
    }
    const ACKS: &[&str] = &[
        "解决了", "好了", "可以了", "谢谢", "多谢", "完美", "搞定", "对了", "是的",
        "solved", "thanks", "thank you", "fixed", "works", "great", "perfect",
    ];
    ACKS.iter().any(|a| {
        let a = a.to_lowercase();
        cleaned == a
            || (cleaned.starts_with(&a)
                && cleaned.chars().count() <= a.chars().count() + 2)
    })
}

/// 从历史消息构建结算信号。`tool_log` 为 (工具名, 成功) 序列（事件流口径）。
pub fn signals_from_history(history: &[ChatMessage], tool_log: &[(String, bool)]) -> SessionSignals {
    let mut first_user = String::new();
    let mut final_answer = String::new();
    let mut ended_with_answer = false;
    let mut user_confirmed = false;
    for m in history {
        let Some(text) = m.content_as_text() else { continue };
        if text.trim().is_empty() {
            continue;
        }
        match m.role.as_str() {
            "user" => {
                if first_user.is_empty() {
                    first_user = text.trim().to_string();
                }
                // 尾部的用户短确认（在最后一轮 assistant 回答之后出现）：
                // 提升置信度，且不打断"已收敛"判定。
                if !final_answer.is_empty() && is_ack(&text) {
                    user_confirmed = true;
                } else {
                    ended_with_answer = false;
                }
            }
            "assistant" => {
                final_answer = text.trim().to_string();
                ended_with_answer = true;
            }
            _ => {}
        }
    }
    let mut tools_used: Vec<String> = Vec::new();
    for (name, _) in tool_log {
        if !tools_used.iter().any(|t| t == name) {
            tools_used.push(name.clone());
        }
    }
    SessionSignals {
        first_user_message: first_user,
        user_confirmed,
        tool_calls: tool_log.len(),
        tools_used,
        final_answer,
        ended_with_answer,
    }
}

/// 案例 writeup（结构化候选；渲染成固定模板 Markdown 落 knowledge/writeups/）。
#[derive(Debug, Clone, Default)]
pub struct CaseNote {
    pub title: String,
    /// 题型/问题分类键（聚合用，归一化小写连字符）。
    pub problem_type: String,
    pub problem: String,
    pub root_cause: String,
    pub solution_steps: Vec<String>,
    pub detours: Vec<String>,
    pub evidence: Vec<String>,
    pub reuse_signals: Vec<String>,
    pub tags: Vec<String>,
}

/// Debrief 作者化产出。
#[derive(Debug, Default)]
pub struct DebriefOutput {
    pub case_note: Option<CaseNote>,
    pub sop: Option<crate::sop::Sop>,
}

/// 落库结果（路径/ID 供日志与审计）。
pub struct DebriefOutcome {
    pub case_note_path: Option<String>,
    pub sop_id: Option<String>,
}

/// 归一化 problem_type：小写、空白→连字符、只留字母数字/CJK/连字符，最长 40。
pub fn normalize_problem_type(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.trim().to_lowercase().chars() {
        if ch.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-');
    out.chars().take(40).collect()
}

/// 渲染案例 writeup 正文（固定模板；frontmatter 由 knowledge::write_entry 生成）。
pub fn render_case_note_body(note: &CaseNote, sig: &SessionSignals) -> String {
    let mut md = String::new();
    md.push_str(&format!("**problem_type**: `{}`\n\n", note.problem_type));
    md.push_str(&format!("## 问题\n{}\n\n", note.problem.trim()));
    md.push_str(&format!("## 根因\n{}\n\n", note.root_cause.trim()));
    md.push_str("## 解法链路\n");
    for (i, s) in note.solution_steps.iter().enumerate() {
        md.push_str(&format!("{}. {}\n", i + 1, s.trim()));
    }
    if !note.detours.is_empty() {
        md.push_str("\n## 走过的弯路\n");
        for d in &note.detours {
            md.push_str(&format!("- {}\n", d.trim()));
        }
    }
    if !note.evidence.is_empty() {
        md.push_str("\n## 关键证据\n");
        for e in &note.evidence {
            md.push_str(&format!("- {}\n", e.trim()));
        }
    }
    if !note.reuse_signals.is_empty() {
        md.push_str("\n## 复用触发信号\n");
        for r in &note.reuse_signals {
            md.push_str(&format!("- {}\n", r.trim()));
        }
    }
    if !sig.tools_used.is_empty() {
        md.push_str(&format!("\n## 使用工具\n{}\n", sig.tools_used.join(", ")));
    }
    md
}

/// 构建 Debrief 作者提示（system + user）。输出 SKIP 或 JSON（无 fence）。
pub fn build_debrief_messages(summary: &str, sig: &SessionSignals) -> Vec<ChatMessage> {
    let tools = if sig.tools_used.is_empty() {
        "(none)".to_string()
    } else {
        sig.tools_used.join(", ")
    };
    let system = ChatMessage::system(
        "You are a senior incident-response / operations engineer writing up a COMPLETED session.\n\
         The session answered a definite question after real investigation. Produce a JSON object\n\
         (raw JSON, no markdown fence) with exactly these fields:\n\
         - \"case_note\": an object capturing THIS case, or null if the session produced no durable\n\
           insight worth keeping (pure chatter, trivial lookup, or task abandoned without conclusion):\n\
           title: short descriptive title\n\
           problem_type: lowercase hyphenated category key for aggregation (e.g. \"winrm-auth-failure\",\n\
             \"webshell-trace\", \"disk-space-cleanup\"); same root-cause class MUST reuse the same key\n\
           problem: what the user asked / what was broken (1-2 sentences)\n\
           root_cause: the definitive root cause found (1-2 sentences; empty if none established)\n\
           solution_steps: ordered array of the DECISIVE steps that led to the answer (3-8 items,\n\
             each specific: exact tool/command/query used and what it revealed)\n\
           detours: array of approaches that FAILED and why (e.g. auth errors, wrong tool) — keep\n\
             the instructive ones, drop noise\n\
           evidence: array of concrete proof points (file paths, hashes, IPs, timestamps, log lines)\n\
           reuse_signals: array of symptom strings that should trigger this note in the future\n\
           tags: array of 3-6 lowercase keywords\n\
         - \"sop\": an object capturing the REUSABLE procedure, or null if the steps are specific to\n\
           this one case and not worth replaying:\n\
           name: short title; description: when to use; semantic_tags: 3-6 trigger keywords;\n\
           phases: 3-8 ordered, concrete, replayable steps, each with a verification hint\n\
         Output ONLY the JSON object, or the single word SKIP if nothing is worth preserving.\n\
         Use the user's language for title/problem/root_cause/steps if the conversation is not English.",
    );
    let user = ChatMessage::user(&format!(
        "Session verdict: {} tool call(s); tools used: {}.\n\nConversation:\n{}",
        sig.tool_calls, tools, summary
    ));
    vec![system, user]
}

fn json_str_array(v: &serde_json::Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// 解析作者响应：SKIP → Ok(None)；JSON → Ok(Some(..))；否则 Err。
pub fn parse_debrief(response: &str) -> Result<Option<DebriefOutput>, String> {
    let trimmed = response.trim();
    if trimmed.eq_ignore_ascii_case("skip")
        || (trimmed.starts_with("```") && trimmed.to_lowercase().contains("skip"))
    {
        return Ok(None);
    }
    let json_str = extract_json(trimmed);
    let v: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| format!("Failed to parse debrief JSON: {e}"))?;

    let mut out = DebriefOutput::default();

    let cn = &v["case_note"];
    if cn.is_object() {
        let title = cn["title"].as_str().unwrap_or("").trim().to_string();
        let problem = cn["problem"].as_str().unwrap_or("").trim().to_string();
        if !title.is_empty() && !problem.is_empty() {
            out.case_note = Some(CaseNote {
                title,
                problem_type: normalize_problem_type(cn["problem_type"].as_str().unwrap_or("general")),
                problem,
                root_cause: cn["root_cause"].as_str().unwrap_or("").trim().to_string(),
                solution_steps: json_str_array(cn, "solution_steps"),
                detours: json_str_array(cn, "detours"),
                evidence: json_str_array(cn, "evidence"),
                reuse_signals: json_str_array(cn, "reuse_signals"),
                tags: json_str_array(cn, "tags"),
            });
        }
    }

    let sp = &v["sop"];
    if sp.is_object() {
        let name = sp["name"].as_str().unwrap_or("").trim().to_string();
        let phases = json_str_array(sp, "phases");
        if !name.is_empty() && phases.len() >= 2 {
            let now = crate::deep_memory::now_secs();
            out.sop = Some(crate::sop::Sop {
                id: String::new(), // register_sop assigns from name
                name,
                description: sp["description"].as_str().unwrap_or("").trim().to_string(),
                semantic_tags: json_str_array(sp, "semantic_tags"),
                phases,
                version: 1,
                created: now,
                updated: now,
                times_executed: 0,
                times_succeeded: 0,
                times_failed: 0,
                last_executed_at: None,
                avg_tool_calls: 0.0,
                avg_duration_secs: 0.0,
            });
        }
    }

    if out.case_note.is_none() && out.sop.is_none() {
        return Ok(None);
    }
    Ok(Some(out))
}

/// 从（可能带 ``` 围栏的）文本中提取 JSON 对象。
fn extract_json(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        return trimmed.to_string();
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let content_start = if after.trim_start().starts_with("json") {
            after.find("json").map(|p| p + 4).unwrap_or(0)
        } else {
            0
        };
        let content = &after[content_start..];
        if let Some(end) = content.find("```") {
            return content[..end].trim().to_string();
        }
        return content.trim().to_string();
    }
    trimmed.to_string()
}

/// 端到端：会话结束后，门控通过则调用 LLM 作者化并落库（案例 + 可选 SOP）。
/// 返回 Ok(Some(outcome)) 表示有产物；Ok(None) 表示门控未过或作者 SKIP。
pub async fn run(
    provider: Arc<OpenAiProvider>,
    model_name: &str,
    workspace_dir: &str,
    history: &[ChatMessage],
    tool_log: &[(String, bool)],
) -> Result<Option<DebriefOutcome>, String> {
    let sig = signals_from_history(history, tool_log);
    if !should_debrief(&sig) {
        tracing::info!(
            "[debrief] gate closed (tools={}, answer={}, problem={}, confirmed={})",
            sig.tool_calls,
            sig.final_answer.chars().count(),
            is_problem_query(&sig.first_user_message),
            sig.user_confirmed
        );
        return Ok(None);
    }
    let summary = crate::sop::build_session_summary(history);
    let messages = build_debrief_messages(&summary, &sig);
    let response = provider.chat_simple(model_name, &messages).await?;
    let output = match parse_debrief(&response)? {
        Some(o) => o,
        None => {
            tracing::info!("[debrief] author declined (SKIP)");
            return Ok(None);
        }
    };

    let mut outcome = DebriefOutcome {
        case_note_path: None,
        sop_id: None,
    };

    if let Some(note) = &output.case_note {
        let body = render_case_note_body(note, &sig);
        let mut tags = note.tags.clone();
        if !note.problem_type.is_empty() && !tags.iter().any(|t| t == &note.problem_type) {
            tags.push(note.problem_type.clone());
        }
        match crate::knowledge::write_entry(workspace_dir, "writeups", &note.title, &tags, &body) {
            Ok(path) => {
                tracing::info!("[debrief] case note written: {}", path);
                outcome.case_note_path = Some(path);
            }
            Err(e) => tracing::warn!("[debrief] case note write failed: {e}"),
        }
    }

    if let Some(sop) = output.sop {
        match crate::sop::register_sop(workspace_dir, sop) {
            Ok(saved) => {
                tracing::info!("[debrief] SOP registered: '{}' ({} phases)", saved.name, saved.phases.len());
                outcome.sop_id = Some(saved.id.clone());
                // 作者化（低频事件）是天然 drain 点：注册后执行一次 GC。
                if let Ok(n) = crate::sop::gc_sops(workspace_dir) {
                    if n > 0 {
                        tracing::info!("[debrief] GC removed {} SOP(s)", n);
                    }
                }
            }
            Err(e) => tracing::warn!("[debrief] SOP register failed: {e}"),
        }
    }

    if outcome.case_note_path.is_none() && outcome.sop_id.is_none() {
        return Ok(None);
    }
    Ok(Some(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> ChatMessage {
        match role {
            "user" => ChatMessage::user(text),
            _ => ChatMessage::assistant(text),
        }
    }

    #[test]
    fn gate_requires_problem_process_and_convergence() {
        let mut sig = SessionSignals {
            first_user_message: "排查 192.168.52.138 的入侵痕迹".into(),
            tool_calls: 5,
            final_answer: "结论：攻击者通过 Ueditor 上传 webshell，随后横向移动到内网主机。".repeat(2),
            ended_with_answer: true,
            ..Default::default()
        };
        assert!(should_debrief(&sig));
        // 工具调用太少 → 不触发
        sig.tool_calls = 1;
        assert!(!should_debrief(&sig));
        sig.tool_calls = 5;
        // 未以文本回答收敛 → 不触发
        sig.ended_with_answer = false;
        assert!(!should_debrief(&sig));
        sig.ended_with_answer = true;
        // 结论太短 → 不触发
        sig.final_answer = "好了。".into();
        assert!(!should_debrief(&sig));
    }

    #[test]
    fn gate_accepts_user_confirmation_without_trigger_words() {
        let sig = SessionSignals {
            first_user_message: "把桌面上那个报错处理一下".into(),
            user_confirmed: true,
            tool_calls: 4,
            final_answer: "已修复：服务被禁用导致启动失败，已改回自动启动并验证通过。".repeat(2),
            ended_with_answer: true,
            ..Default::default()
        };
        assert!(should_debrief(&sig));
    }

    #[test]
    fn problem_query_detection() {
        assert!(is_problem_query("帮我分析这个日志"));
        assert!(is_problem_query("investigate the suspicious process"));
        assert!(is_problem_query("看下 192.168.52.138 的情况")); // IPv4 具体对象
        assert!(is_problem_query("C:\\Windows\\Temp 下多了奇怪文件"));
        assert!(!is_problem_query("你好"));
        assert!(!is_problem_query("今天天气怎么样"));
    }

    #[test]
    fn ack_detection_is_short_only() {
        assert!(is_ack("解决了"));
        assert!(is_ack("thanks!"));
        assert!(!is_ack("解决了，但是还有一个问题"));
        assert!(!is_ack(""));
    }

    #[test]
    fn signals_built_from_history() {
        let h = vec![
            msg("user", "调查 10.0.0.1 的异常登录"),
            msg("assistant", "我来查一下"),
            msg("assistant", "结论：暴力破解成功，来源 10.0.0.9。已封禁。"),
            msg("user", "解决了"),
        ];
        let log = vec![
            ("ir_eventlog".to_string(), true),
            ("ir_account".to_string(), true),
            ("shell_exec".to_string(), false),
            ("ir_eventlog".to_string(), true),
        ];
        let sig = signals_from_history(&h, &log);
        assert_eq!(sig.first_user_message, "调查 10.0.0.1 的异常登录");
        assert!(sig.user_confirmed);
        assert!(sig.ended_with_answer); // ack 之后的 assistant 观点保持：尾部 user ack 不改变 ended 语义
        assert_eq!(sig.tool_calls, 4);
        assert_eq!(sig.tools_used, vec!["ir_eventlog", "ir_account", "shell_exec"]);
        assert!(should_debrief(&sig));
    }

    #[test]
    fn parse_skip_and_json() {
        assert!(parse_debrief("SKIP").unwrap().is_none());
        assert!(parse_debrief("```json\nSKIP\n```").unwrap().is_none());
        let json = r#"{
            "case_note": {
                "title": "WinRM 认证失败修复",
                "problem_type": "WinRM Auth Failure",
                "problem": "winrm 工具 401",
                "root_cause": "Basic 认证与 AllowUnencrypted 被关闭",
                "solution_steps": ["用 PS remoting 直连", "启用 Basic"],
                "detours": ["winrm 工具 401"],
                "evidence": ["winrm get winrm/service"],
                "reuse_signals": ["winrm 401"],
                "tags": ["winrm", "auth"]
            },
            "sop": null
        }"#;
        let out = parse_debrief(json).unwrap().expect("should parse");
        let note = out.case_note.expect("case note");
        assert_eq!(note.problem_type, "winrm-auth-failure");
        assert!(out.sop.is_none());
    }

    #[test]
    fn parse_rejects_garbage_and_empty() {
        assert!(parse_debrief("I have no idea").is_err());
        // JSON 但两个产物都为空 → None
        assert!(parse_debrief(r#"{"case_note": null, "sop": null}"#).unwrap().is_none());
    }

    #[test]
    fn problem_type_normalized() {
        assert_eq!(normalize_problem_type("WinRM Auth Failure"), "winrm-auth-failure");
        assert_eq!(normalize_problem_type("  Webshell  排查 "), "webshell-排查");
        assert!(normalize_problem_type("").is_empty());
    }

    #[test]
    fn render_template_has_all_sections() {
        let note = CaseNote {
            title: "t".into(),
            problem_type: "x".into(),
            problem: "p".into(),
            root_cause: "r".into(),
            solution_steps: vec!["s1".into(), "s2".into()],
            detours: vec!["d1".into()],
            evidence: vec!["e1".into()],
            reuse_signals: vec!["sig1".into()],
            tags: vec![],
        };
        let sig = SessionSignals {
            tools_used: vec!["ir_scan".into()],
            ..Default::default()
        };
        let body = render_case_note_body(&note, &sig);
        for section in ["## 问题", "## 根因", "## 解法链路", "## 走过的弯路", "## 关键证据", "## 复用触发信号", "## 使用工具"] {
            assert!(body.contains(section), "missing section: {section}");
        }
        assert!(body.contains("problem_type"));
    }
}
