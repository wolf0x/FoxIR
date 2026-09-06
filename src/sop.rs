//! SOP — Standard Operating Procedure「程序性记忆」层。
//!
//! 对应 temm1e/skyclaw 的 Blueprint，本项目中命名为 SOP。
//! 当一类多步骤任务被反复执行并验证后，将其固化为一套可回放、可评分、可淘汰的 SOP；
//! 相似任务再次出现时按标签匹配、按价值排序后注入上下文作为操作指南。
//!
//! 所有学习产物（SOP、经验条目、记忆）共用统一价值函数：
//!     V(a, t) = Q(a) × R(a, t) × U(a)
//!   - Q = Wilson 99% 置信下限(成功率)：小样本保守，避免把偶然当成功
//!   - R = 陈旧衰减 exp(-0.005·天数)，半衰期 ≈ 139 天：太久没用会沉底
//!   - U = 1 + 0.5·ln(1 + 执行次数)：用得越频繁越靠前（使用促进）
//! 保证上下文窗口始终保留「可信、最近在用、用得频繁」的高价值产物。
//!
//! 存储位置：`workspace/knowledge/sops.json`（结构化，便于迁移与审计）。

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

use crate::model::openai::OpenAiProvider;
use crate::model::ChatMessage;

/// 存储（SOP 现持久化于 SQLite `sops` 表；`sops.json` 仅作一次性迁移来源）。
/// 低于该价值的 SOP 在 GC 中被淘汰。
const GC_VALUE_THRESHOLD: f64 = 0.005;
/// proven_bad 判定：执行至少这么多轮.
const PROVEN_BAD_MIN_RUNS: u32 = 5;
/// 且 Wilson 成功率上限低于该值 → 判定为「已证伪」。
const PROVEN_BAD_SUCCESS: f64 = 0.20;

/// 一条 SOP（等价 Blueprint）：结构化、带真实执行统计。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sop {
    pub id: String,
    pub name: String,
    /// 何时用（“触发条件”）。
    pub description: String,
    /// 匹配标签（对任务描述做不区分大小写的子串匹配）。
    pub semantic_tags: Vec<String>,
    /// 有序步骤/阶段。
    pub phases: Vec<String>,
    pub version: u32,
    pub created: u64,
    pub updated: u64,
    pub times_executed: u32,
    pub times_succeeded: u32,
    pub times_failed: u32,
    pub last_executed_at: Option<u64>,
    pub avg_tool_calls: f32,
    pub avg_duration_secs: f32,
}

impl Sop {
    /// 简单的成功率（0..=1）。
    pub fn success_rate(&self) -> f64 {
        if self.times_executed == 0 {
            0.0
        } else {
            self.times_succeeded as f64 / self.times_executed as f64
        }
    }

    /// 粗略 token 估算（用于注入时的上下文硬帽）。
    pub fn rough_tokens(&self) -> usize {
        let chars: usize = self.name.len()
            + self.description.len()
            + self.semantic_tags.iter().map(|s| s.len()).sum::<usize>()
            + self.phases.iter().map(|p| p.len()).sum::<usize>();
        chars / 4
    }

    /// 统一价值分。
    pub fn value(&self, now: u64) -> f64 {
        learning_value(self.times_succeeded, self.times_executed, self.last_executed_at, now)
    }
}

/// 磁盘上的 SOP 集合（带 schema 版本，便于日后迁移）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SopStore {
    pub schema_version: u32,
    pub sops: Vec<Sop>,
}

/// 当前 Unix 时间戳（秒）。
pub fn now_secs() -> u64 {
    crate::value::now_secs()
}

/// SOP 文件路径：`<workspace>/knowledge/sops.json`。
pub fn sop_file(workspace_dir: &str) -> PathBuf {
    Path::new(workspace_dir).join("knowledge").join("sops.json")
}

// ---------------------------------------------------------------------------
// 统一学习产物价值函数  V = Q × R × U
// ---------------------------------------------------------------------------

/// 统一学习产物价值：`V = Q² × R × U`，实现在 crate::value（SOP 用结果导向 Q）。
pub fn learning_value(successes: u32, executed: u32, last_used: Option<u64>, now: u64) -> f64 {
    crate::value::learning_value(successes, executed, last_used, now)
}

// 存取
// ---------------------------------------------------------------------------

/// 打开 SOP 持久化的 SQLite（与浅层/深层同库：`<workspace>/memory/memory.db`）并确保 `sops` 表存在。
/// 同时做一次 `sops.json` → 新表的遗留数据迁移（仅当表为空且 json 存在）。
fn open_sop_conn(workspace_dir: &str) -> Result<Connection, String> {
    let dir = Path::new(workspace_dir).join("memory");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create memory dir: {e}"))?;
    let conn = Connection::open(dir.join("memory.db"))
        .map_err(|e| format!("open memory.db: {e}"))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sops (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            description     TEXT NOT NULL DEFAULT '',
            semantic_tags   TEXT NOT NULL DEFAULT '[]',
            phases          TEXT NOT NULL DEFAULT '[]',
            version         INTEGER NOT NULL DEFAULT 1,
            created         INTEGER NOT NULL,
            updated         INTEGER NOT NULL,
            times_executed  INTEGER NOT NULL DEFAULT 0,
            times_succeeded INTEGER NOT NULL DEFAULT 0,
            times_failed    INTEGER NOT NULL DEFAULT 0,
            last_executed_at INTEGER,
            avg_tool_calls  REAL NOT NULL DEFAULT 0.0,
            avg_duration_secs REAL NOT NULL DEFAULT 0.0
        );
        CREATE INDEX IF NOT EXISTS idx_sops_name ON sops(name);",
    )
    .map_err(|e| format!("sops schema: {e}"))?;
    migrate_legacy_json(&conn, workspace_dir)?;
    Ok(conn)
}

/// 一次性的遗留迁移：`sops.json` 存在且 `sops` 表为空时导入。
fn migrate_legacy_json(conn: &Connection, workspace_dir: &str) -> Result<(), String> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sops", [], |r| r.get(0))
        .unwrap_or(0);
    if count > 0 {
        return Ok(());
    }
    let p = crate::sop::sop_file(workspace_dir);
    if !p.exists() {
        return Ok(());
    }
    let Ok(raw) = std::fs::read_to_string(&p) else { return Ok(()) };
    let Ok(store) = serde_json::from_str::<SopStore>(&raw) else { return Ok(()) };
    if store.sops.is_empty() {
        return Ok(());
    }
    for sop in &store.sops {
        insert_sop(conn, sop)?;
    }
    info!("[sop] migrated {} SOP(s) from {} into memory.db", store.sops.len(), p.display());
    Ok(())
}

/// 将一条 SOP 写入 `sops` 表（INSERT OR REPLACE）。
fn insert_sop(conn: &Connection, s: &Sop) -> Result<(), String> {
    let tags = serde_json::to_string(&s.semantic_tags).unwrap_or_else(|_| "[]".to_string());
    let phases = serde_json::to_string(&s.phases).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        "INSERT OR REPLACE INTO sops
           (id,name,description,semantic_tags,phases,version,created,updated,
            times_executed,times_succeeded,times_failed,last_executed_at,avg_tool_calls,avg_duration_secs)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            s.id, s.name, s.description, tags, phases, s.version as i64,
            s.created as i64, s.updated as i64,
            s.times_executed as i64, s.times_succeeded as i64, s.times_failed as i64,
            s.last_executed_at.map(|v| v as i64),
            s.avg_tool_calls, s.avg_duration_secs,
        ],
    )
    .map_err(|e| format!("insert sop: {e}"))?;
    Ok(())
}

/// SQL 行 → `Sop`。
fn row_to_sop(r: &rusqlite::Row) -> rusqlite::Result<Sop> {
    let tags: String = r.get(3)?;
    let phases: String = r.get(4)?;
    Ok(Sop {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        semantic_tags: serde_json::from_str(&tags).unwrap_or_default(),
        phases: serde_json::from_str(&phases).unwrap_or_default(),
        version: r.get::<_, i64>(5)? as u32,
        created: r.get::<_, i64>(6)? as u64,
        updated: r.get::<_, i64>(7)? as u64,
        times_executed: r.get::<_, i64>(8)? as u32,
        times_succeeded: r.get::<_, i64>(9)? as u32,
        times_failed: r.get::<_, i64>(10)? as u32,
        last_executed_at: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        avg_tool_calls: r.get(12)?,
        avg_duration_secs: r.get(13)?,
    })
}

/// 从 SQLite `sops` 表读取全部 SOP（作为统一工件价值的打分源）。
pub fn load_sops(workspace_dir: &str) -> Vec<Sop> {
    let Ok(conn) = open_sop_conn(workspace_dir) else { return Vec::new() };
    let mut stmt = match conn.prepare(
        "SELECT id,name,description,semantic_tags,phases,version,created,updated,
                times_executed,times_succeeded,times_failed,last_executed_at,avg_tool_calls,avg_duration_secs
         FROM sops",
    ) {
        Ok(st) => st,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], row_to_sop) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.filter_map(|r| r.ok()).collect()
}

/// 整体写回 `sops` 表（清空后事务内重写）。
pub fn save_sops(workspace_dir: &str, sops: &[Sop]) -> Result<(), String> {
    let mut conn = open_sop_conn(workspace_dir)?;
    let tx = conn.transaction().map_err(|e| format!("tx begin: {e}"))?;
    tx.execute("DELETE FROM sops", []).map_err(|e| format!("clear sops: {e}"))?;
    for s in sops {
        let tags = serde_json::to_string(&s.semantic_tags).unwrap_or_else(|_| "[]".to_string());
        let phases = serde_json::to_string(&s.phases).unwrap_or_else(|_| "[]".to_string());
        tx.execute(
            "INSERT OR REPLACE INTO sops
               (id,name,description,semantic_tags,phases,version,created,updated,
                times_executed,times_succeeded,times_failed,last_executed_at,avg_tool_calls,avg_duration_secs)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                s.id, s.name, s.description, tags, phases, s.version as i64,
                s.created as i64, s.updated as i64,
                s.times_executed as i64, s.times_succeeded as i64, s.times_failed as i64,
                s.last_executed_at.map(|v| v as i64),
                s.avg_tool_calls, s.avg_duration_secs,
            ],
        )
        .map_err(|e| format!("sop insert: {e}"))?;
    }
    tx.commit().map_err(|e| format!("tx commit: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 匹配 / 选择
// ---------------------------------------------------------------------------

/// 按 semantic_tags 对任务描述做不区分大小写的子串匹配（与 skill 一致）。
pub fn match_sops(sops: &[Sop], task: &str) -> Vec<Sop> {
    let t = task.to_lowercase();
    sops.iter()
        .filter(|s| s.semantic_tags.iter().any(|tag| t.contains(&tag.to_lowercase())))
        .cloned()
        .collect()
}

/// 从候选里挑价值最高的一条；若其体积超过上下文 1/4 则放弃（只给目录）。
pub fn select_best(sops: &[Sop], now: u64, context_limit: usize) -> Option<Sop> {
    if sops.is_empty() {
        return None;
    }
    let mut c = sops.to_vec();
    c.sort_by(|a, b| {
        b.value(now)
            .partial_cmp(&a.value(now))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let best = &c[0];
    let hard_limit = context_limit / 4;
    if best.rough_tokens() > hard_limit {
        return None;
    }
    Some(best.clone())
}

// ---------------------------------------------------------------------------
// 记录 / 注册 / 淘汰
// ---------------------------------------------------------------------------

/// 记录一次执行结果，更新统计与最后使用时间。
pub fn record_result(
    workspace_dir: &str,
    id: &str,
    success: bool,
    tool_calls: u32,
    duration_secs: u32,
) -> Result<Sop, String> {
    let mut sops = load_sops(workspace_dir);
    let now = now_secs();
    let mut found: Option<Sop> = None;
    for s in sops.iter_mut() {
        if s.id == id {
            s.times_executed += 1;
            if success {
                s.times_succeeded += 1;
            } else {
                s.times_failed += 1;
            }
            s.last_executed_at = Some(now);
            s.updated = now;
            let n = s.times_executed.max(1) as f32;
            s.avg_tool_calls =
                (s.avg_tool_calls * (s.times_executed - 1) as f32 + tool_calls as f32) / n;
            s.avg_duration_secs =
                (s.avg_duration_secs * (s.times_executed - 1) as f32 + duration_secs as f32) / n;
            found = Some(s.clone());
            break;
        }
    }
    match found {
        Some(s) => {
            save_sops(workspace_dir, &sops)?;
            Ok(s)
        }
        None => Err(format!("SOP '{id}' not found")),
    }
}

/// 注册或更新一条 SOP（同名/同 id 覆盖）。
pub fn register_sop(workspace_dir: &str, mut sop: Sop) -> Result<Sop, String> {
    let mut sops = load_sops(workspace_dir);
    let now = now_secs();
    if sop.created == 0 {
        sop.created = now;
    }
    sop.updated = now;
    if sop.id.trim().is_empty() {
        sop.id = gen_id(&sop.name);
    }
    let id = sop.id.clone();
    if let Some(existing) = sops.iter_mut().find(|s| s.id == id) {
        *existing = sop.clone();
    } else {
        sops.push(sop.clone());
    }
    save_sops(workspace_dir, &sops)?;
    Ok(sop)
}

/// 删除一条 SOP。
pub fn delete_sop(workspace_dir: &str, id: &str) -> Result<(), String> {
    let mut sops = load_sops(workspace_dir);
    let before = sops.len();
    sops.retain(|s| s.id != id);
    if sops.len() == before {
        return Err(format!("SOP '{id}' not found"));
    }
    save_sops(workspace_dir, &sops)
}

/// 垃圾回收：淘汰低价值或「已证伪」的 SOP。返回删除数量。
pub fn gc_sops(workspace_dir: &str) -> Result<usize, String> {
    let mut sops = load_sops(workspace_dir);
    let now = now_secs();
    let before = sops.len();
    sops.retain(|s| {
        let v = s.value(now);
        let proven_bad = s.times_executed >= PROVEN_BAD_MIN_RUNS
            && crate::value::wilson_lower(s.times_succeeded, s.times_executed, 2.576) < PROVEN_BAD_SUCCESS;
        !(v < GC_VALUE_THRESHOLD || proven_bad)
    });
    let removed = before - sops.len();
    save_sops(workspace_dir, &sops)?;
    Ok(removed)
}

// ---------------------------------------------------------------------------
// 注入格式化 / 辅助
// ---------------------------------------------------------------------------

/// 把 SOP 格式化为注入上下文的操作指南。
pub fn format_sop_context(sop: &Sop) -> String {
    let mut out = format!(
        "=== SOP: {} (v{}) ===\n{}\n（已验证流程：{} 次执行 / 成功率 {:.0}%）\n",
        sop.name,
        sop.version,
        sop.description,
        sop.times_executed,
        sop.success_rate() * 100.0
    );
    for (i, ph) in sop.phases.iter().enumerate() {
        out.push_str(&format!("Phase {}: {}\n", i + 1, ph));
    }
    out.push_str("按 Phase 顺序执行，仅在条件不同时偏离；完成后回写 SOP 记录结果。\n=== END SOP ===");
    out
}

fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            s.push(c);
        } else if c.is_whitespace() {
            s.push('_');
        }
    }
    s.trim_matches('_').to_string()
}

fn gen_id(name: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    let n = h.finish();
    format!("sop-{}-{:x}", slug(name).chars().take(24).collect::<String>(), n)
}
// ---------------------------------------------------------------------------
// SOP 作者（Authoring）—— 会话结束后把「可复用的多步骤过程」固化成 SOP
// 对应 temm1e 的 author_blueprint。experience 由静态蒸馏改为动态 SOP。
// 用户手动挂接的 knowledge 不变，这里只替换“蒸馏→经验条目”的写路径。
// ---------------------------------------------------------------------------

/// 从会话历史构建紧凑摘要（仅 user/assistant，控制上下文体积）。
pub fn build_session_summary(history: &[ChatMessage]) -> String {
    const MAX_MSG_CHARS: usize = 420;
    const MAX_SUMMARY_CHARS: usize = 8000;
    let mut summary = String::new();
    let mut truncated = false;
    for m in history {
        if m.role != "user" && m.role != "assistant" {
            continue;
        }
        let Some(text) = m.content_as_text() else { continue };
        // 跳过工具调用块本身，只留叙事文本，便于模型判断“过程”而非噪音
        let text = text.split("```json").next().unwrap_or(&text);
        let role_label = if m.role == "user" { "User" } else { "Assistant" };
        let line = if text.chars().count() > MAX_MSG_CHARS {
            let cut: String = text.chars().take(MAX_MSG_CHARS).collect();
            format!("{role_label}: {cut}…
")
        } else {
            format!("{role_label}: {text}
")
        };
        if summary.len() + line.len() > MAX_SUMMARY_CHARS {
            summary.push_str("
[conversation truncated]
");
            truncated = true;
            break;
        }
        summary.push_str(&line);
    }
    if summary.trim().is_empty() {
        summary.push_str("[no narrative content]
");
    }
    if truncated {
        summary.push_str("[conversation may be long; focus on the reusable procedure]
");
    }
    summary
}

/// 统计会话中的工具调用次数（history 里 role=tool/function，或 assistant 内嵌 tool JSON）。
pub fn count_used_tools(history: &[ChatMessage]) -> u32 {
    let mut n = 0u32;
    for m in history {
        match m.role.as_str() {
            "tool" | "function" => n += 1,
            "assistant" => {
                if let Some(t) = m.content_as_text() {
                    if t.contains("```json") && t.contains("\"name\"") {
                        n += 1;
                    }
                }
            }
            _ => {}
        }
    }
    n
}

/// 构建 SOP 作者提示（system + user）。输出 SKIP 或 JSON 对象（无 markdown fence）。
/// 对应 temm1e 的 build_authoring_prompt + author_blueprint 的 system 设定。
pub fn build_authoring_messages(summary: &str, tool_calls: u32) -> Vec<ChatMessage> {
    let system = ChatMessage::system(
        "You are a technical writer. You review a completed task to decide whether it is worth \n\
         capturing as a reusable SOP: a structured, replayable, ordered procedure with concrete \n\
         steps and verification. Output ONLY one of:\n\
         - SKIP  — if the task was pure chat, a single trivial lookup, or produced no reusable \n\
           multi-step procedure that a future run could replay without re-deriving it.\n\
         - A raw JSON object (no markdown fence, no code block) with exactly these fields:\n\
           name: short human-readable title (e.g. Deploy Web App)\n\
           description: one sentence about what this SOP accomplishes and when to use it\n\
           semantic_tags: array of 3-6 lowercase keywords a user request would contain to trigger this SOP\n\
           phases: array of 3-8 ordered step strings. Each step must be SPECIFIC and ACTIONABLE \n\
           (state exactly what to do, with concrete commands/selectors/paths) and include a short \n\
           verification/quality-gate so the executor can confirm the step worked before proceeding.\n\
         Rules:\n\
         - WORTH an SOP: research (search -> fetch -> synthesize), scaffolding (create -> configure -> build -> verify),\n\
           debugging (reproduce -> locate -> patch -> re-test), any procedure with 2+ tool calls.\n\
         - SKIP: a one-line factual answer, a greeting, a single file read with no follow-up.\n\
         - Keep each phase concrete enough that another agent can replay it without trial-and-error.\n\
         - Use the user's language for name/description/phases if the conversation is not English.\n\
         - If the same procedure already exists, keep the SAME name so it is refined, not duplicated.",
    );
    let user = ChatMessage::user(&format!(
        "Completed task (tool calls: {tool_calls}). Decide and, if worth it, output the SOP JSON.\n\nConversation:\n{summary}"
    ));
    vec![system, user]
}

/// 解析作者响应：SKIP → Ok(None)；JSON → Ok(Some(Sop))；否则 Err。
pub fn parse_authored_sop(response: &str) -> Result<Option<Sop>, String> {
    let trimmed = response.trim();
    if trimmed.eq_ignore_ascii_case("skip")
        || trimmed.starts_with("```") && trimmed.to_lowercase().contains("skip")
    {
        return Ok(None);
    }
    let json_str = extract_json(trimmed);
    let v: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| format!("Failed to parse SOP JSON: {e} | raw: {}", &json_str[..200.min(json_str.len())]))?;
    let name = v["name"].as_str().map(String::from).unwrap_or_default();
    if name.trim().is_empty() {
        return Ok(None);
    }
    let desc = v["description"].as_str().map(String::from).unwrap_or_default();
    let tags: Vec<String> = v["semantic_tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let phases: Vec<String> = v["phases"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if phases.is_empty() {
        return Ok(None);
    }
    let now = now_secs();
    Ok(Some(Sop {
        id: String::new(),
        name,
        description: desc,
        semantic_tags: tags,
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
    }))
}

/// 端到端：会话结束后，若存在多步骤可复用过程，则调用 LLM 作者并注册到 sops.json。
/// 返回 Ok(Some(sop)) 表示新增/更新；Ok(None) 表示不值得固化。
pub async fn author_sop_from_session(
    history: &[ChatMessage],
    provider: Arc<OpenAiProvider>,
    model_name: &str,
    workspace_dir: &str,
) -> Result<Option<Sop>, String> {
    let tool_calls = count_used_tools(history);
    if tool_calls == 0 {
        info!("[sop] session has no tool calls, not a procedural candidate");
        return Ok(None);
    }
    let summary = build_session_summary(history);
    let messages = build_authoring_messages(&summary, tool_calls);
    let response = provider.chat_simple(model_name, &messages).await?;
    let sop = match parse_authored_sop(&response)? {
        Some(s) => s,
        None => {
            info!("[sop] author declined (SKIP)");
            return Ok(None);
        }
    };
    let saved = register_sop(workspace_dir, sop)?;
    warn!("[sop] authored SOP '{}' ({} phases)", saved.name, saved.phases.len());
    Ok(Some(saved))
}

/// 从（可能带 ``` 围栏的）文本中提取 JSON。
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
    // Allow the assistant to have emitted stray prose before the JSON.
    if let Some(idx) = trimmed.find('{') {
        let sub = &trimmed[idx..];
        if let Some(end) = sub.rfind('}') {
            return sub[..=end].to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn base() -> Sop {
        Sop {
            id: "s1".to_string(),
            name: "Incident Triage".to_string(),
            description: "Respond to an incident".to_string(),
            semantic_tags: vec!["triage".to_string(), "incident".to_string()],
            phases: vec!["采集证据".to_string(), "分析".to_string(), "遏制".to_string()],
            version: 1,
            created: 1,
            updated: 1,
            times_executed: 10,
            times_succeeded: 8,
            times_failed: 2,
            last_executed_at: Some(now_secs()),
            avg_tool_calls: 12.0,
            avg_duration_secs: 60.0,
        }
    }

    #[test]
    fn learning_value_grows_with_use() {
        let now = now_secs();
        // 失败更多 → 价值更低
        let weak = learning_value(2, 10, Some(now), now);
        let strong = learning_value(8, 10, Some(now), now);
        assert!(weak < strong, "weak={weak} strong={strong}");
        // 用得多 → 价值升高（U 项）
        let fresh_more = learning_value(16, 20, Some(now), now);
        assert!(fresh_more > strong, "fresh={fresh_more} strong={strong}");
    }

    #[test]
    fn stale_decays_value() {
        let now = now_secs();
        let recent = learning_value(8, 10, Some(now), now);
        let stale = learning_value(8, 10, Some(now - 1000 * 86400), now);
        assert!(stale < recent, "stale={stale} recent={recent}");
    }

    #[test]
    fn match_tags_is_case_insensitive() {
        let sop = base();
        let sops = vec![sop];
        assert_eq!(match_sops(&sops, "Handle an INCIDENT right now").len(), 1);
        assert_eq!(match_sops(&sops, "no tag here").len(), 0);
    }

    #[test]
    fn select_best_rejects_oversized() {
        let mut sop = base();
        sop.phases = vec!["x".repeat(4000)];
        let sops = vec![sop];
        // context_limit 很小 → token 帽触发,返回 None
        assert!(select_best(&sops, now_secs(), 1000).is_none());
    }

    #[test]
    fn record_increments_and_persists() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap().to_string();
        register_sop(&ws, base()).unwrap();
        let s = record_result(&ws, "s1", true, 5, 30).unwrap();
        assert_eq!(s.times_executed, 11);
        assert_eq!(s.times_succeeded, 9);
        assert_eq!(s.last_executed_at.is_some(), true);
        // 持久化验证
        let reloaded = load_sops(&ws);
        assert_eq!(reloaded[0].times_executed, 11);
    }

    #[test]
    fn parse_authored_sop_accepts_json() {
        let resp = r#"{
            "name": "Deploy Web App",
            "description": "Build and publish the web app",
            "semantic_tags": ["deploy", "web", "publish"],
            "phases": ["Clone repo", "Run build", "Verify health"
            ]
        }"#;
        let sop = parse_authored_sop(resp).unwrap().expect("should parse");
        assert_eq!(sop.name, "Deploy Web App");
        assert_eq!(sop.semantic_tags.len(), 3);
        assert_eq!(sop.phases.len(), 3);
        assert!(sop.times_executed == 0);
    }

    #[test]
    fn parse_authored_sop_handles_skip_and_fence() {
        assert!(parse_authored_sop("SKIP").unwrap().is_none());
        assert!(parse_authored_sop("```
SKIP
```").unwrap().is_none());
        let fenced = "```json\n{\"name\":\"X\",\"description\":\"Y\",\"semantic_tags\":[\"a\"],\"phases\":[\"p1\",\"p2\"]}\n```";
        let sop = parse_authored_sop(fenced).unwrap().expect("fence should parse");
        assert_eq!(sop.name, "X");
    }

    #[test]
    fn count_used_tools_detects_native_and_json() {
        use crate::model::ChatMessage;
        let mut hist = vec![
            ChatMessage::user("hello"),
            ChatMessage::system("sys"),
        ];
        hist.push(ChatMessage { role: "tool".to_string(), content: None, tool_calls: None, tool_call_id: None, name: None });
        hist.push(ChatMessage::assistant("saw```json\n{\"name\": \"app_launch\"}\n```done"));
        assert_eq!(count_used_tools(&hist), 2);
        assert_eq!(count_used_tools(&[ChatMessage::user("hi")]), 0);
    }

    #[test]
    fn gc_removes_proven_bad() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap().to_string();
        let mut bad = base();
        bad.id = "bad1".to_string();
        bad.times_executed = 5;
        bad.times_succeeded = 0; // 执行 5 次全败 → proven_bad
        let mut good = base();
        good.id = "good1".to_string();
        good.times_executed = 10;
        good.times_succeeded = 9;
        register_sop(&ws, bad).unwrap();
        register_sop(&ws, good).unwrap();
        let removed = gc_sops(&ws).unwrap();
        assert_eq!(removed, 1);
        let left = load_sops(&ws);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "good1");
    }
    /// 真实 SOP 全闭环：创作(parse JSON) → 落库(register_sop) → 命中注入
    /// (match_sops+select_best+format_sop_context) → 调用后更新统计(record_result+重载)。
    #[test]
    fn sop_lifecycle_closed_loop() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap().to_string();

        // 1) 创作：LLM 返回的 authoring JSON，经 parse_authored_sop 解析（stat=0）。
        let authored = parse_authored_sop(r#"{
            "name": "Zero Install Self-Heal Cleanup",
            "description": "当发现 0install 残留目录、Run 键重建或自愈进程复现时彻底清理并验证",
            "semantic_tags": ["0install", "self-heal", "cleanup", "persistence"],
            "phases": ["枚举残留目录与 Run 键", "定位并终止矿工程序", "删除可执行体与临时日志", "移除持久化键(合并保留)", "重启后复跑枚举验证无复现"]
        }"#).unwrap().expect("authoring should parse");

        // 2) 落库：注册进 SQLite sops 表。
        let mut registered = register_sop(&ws, authored).unwrap();
        assert!(!registered.id.is_empty());
        assert_ne!(registered.created, 0);
        let in_db = load_sops(&ws);
        assert_eq!(in_db.len(), 1);
        assert_eq!(in_db[0].name, "Zero Install Self-Heal Cleanup");

        // 3) 命中注入：任务包含 tag → 被 match；select_best 选中；format 可注入。
        let task = "系统上发现了 0install 残留并且 Run 键被反复重建，做一次 self-heal 清理";
        let matched = match_sops(&[registered.clone()], task);
        assert_eq!(matched.len(), 1, "should match by tag");
        assert!(select_best(&matched, crate::sop::now_secs(), 10_000).is_some(), "should be selectable");
        // 用注册后的真实对象走注入（含 created/updated 已填充）
        registered = in_db[0].clone();
        let best = select_best(&[registered.clone()], crate::sop::now_secs(), 10_000).unwrap();
        assert_eq!(best.id, registered.id);
        let ctx = format_sop_context(&best);
        assert!(ctx.contains("Phase 5"), "should contain all 5 phases");
        assert!(ctx.contains("已验证流程"), "should carry stats line");

        // 4) 调用后更新统计：模拟一次成功执行(4 次工具调用,180s)，并持久化重载。
        let after = record_result(&ws, &registered.id, true, 4, 180).unwrap();
        assert_eq!(after.times_executed, 1);
        assert_eq!(after.times_succeeded, 1);
        assert_eq!(after.times_failed, 0);
        assert!((after.avg_tool_calls - 4.0).abs() < 1e-4);
        assert!((after.avg_duration_secs - 180.0).abs() < 1e-4);
        assert!(after.last_executed_at.is_some());

        // 再跑一次失败：累计统计应随之更新。
        let after2 = record_result(&ws, &registered.id, false, 7, 200).unwrap();
        assert_eq!(after2.times_executed, 2);
        assert_eq!(after2.times_failed, 1);
        assert!((after2.avg_tool_calls - 5.5).abs() < 1e-4, "avg={}", after2.avg_tool_calls);
        assert!((after2.avg_duration_secs - 190.0).abs() < 1e-4);

        // 持久化核实：重新打开库可见更新后的统计。
        let reloaded = load_sops(&ws);
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].times_executed, 2);
        assert_eq!(reloaded[0].times_succeeded, 1);
        assert_eq!(reloaded[0].times_failed, 1);
        assert!((reloaded[0].avg_duration_secs - 190.0).abs() < 1e-4);

        // 注入文本也会反映新统计。
        let ctx2 = format_sop_context(&reloaded[0]);
        assert!(ctx2.contains("50%"), "success rate 1/2 should be 50%");
    }
}
