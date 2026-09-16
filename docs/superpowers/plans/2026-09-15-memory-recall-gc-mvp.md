# Memory Recall + Soft-GC MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 deep memory 召回更准（query 相关性重排）、pin 语义收紧（仅显式记住才 User）、并引入软归档 GC，使 `deep_facts` 由"只增不减"变为可治理，全程无 embedding。

**Architecture:** 纯确定性的 keyword-relevance（复用 CJK bigram 分词）+ `deep_facts.archived` 软删除列 + 注入去 touch。召回排序从「纯价值 V」改为「价值 + 4×相关性」；GC 只在后台 curator 结束时顺带跑，软删不物理删。

**Tech Stack:** Rust 2021 · rusqlite (bundled) · serde_json · tracing · tokio（后台任务）。

**Spec:** `docs/ideas/memory-recall-gc-mvp.md`（本计划依据）。

## Global Constraints
- 不引入 embedding/vector index 依赖，不新增 crate。
- `deep_facts` 使用软归档（`archived` 列），禁止物理 DELETE（符合 AGENTS.md 可恢复红线）。
- 用户 pin（`PinnedBy::User`）永不归档、永不退火。
- `src/server.rs` 用 **Tab 缩进**、`static/index.html` 用空格缩进；apply_patch 前先 `git checkout` 相关文件备份（此前有截断事故）。
- 每次任务结束必须 `cargo test` 通过 + `cargo check --lib` 通过后 commit。

---

### Task 1: deep_facts 增加 archived 列 + 软归档/恢复/批量归档

**Files:**
- Modify: `src/memory.rs:1480-1545`（`ensure_two_tier_schema`）
- Modify: `src/memory.rs:1566-1600`（`deep_store`）
- Modify: `src/memory.rs:1603-1620`（`deep_list`）
- Modify: `src/deep_memory.rs`（`DeepFact` 加 `archived`）
- Modify: `src/memory.rs`（`deep_row`、新增 `deep_list_archived` / `deep_archive` / `deep_restore` / `deep_archive_stale`）
- Test: `src/memory.rs` `mod tests_two_tier`

**Interfaces:**
- Consumes: `crate::deep_memory::DeepFact`（现有字段）、`tempfile`（测试已有 `tmp_store`）。
- Produces:
  - `DeepFact` 新增 `pub archived: bool`（`#[serde(default)]`）
  - `MemoryStore::deep_archive(id: &str) -> Result<bool, String>`
  - `MemoryStore::deep_restore(id: &str) -> Result<bool, String>`
  - `MemoryStore::deep_archive_stale(tau_days: f32, importance_threshold: f32) -> Result<usize, String>`
  - `MemoryStore::deep_list(scope_key) -> Result<Vec<DeepFact>, String>` 现在只返回 `archived=0` 的行
  - `MemoryStore::deep_list_archived(scope_key) -> Result<Vec<DeepFact>, String>`

- [ ] **Step 1: 写失败测试（tests_two_tier）**

```rust
#[test]
fn deep_archive_soft_gc_preserves_user_and_returns_rows() {
    let store = tmp_store();
    let now = crate::deep_memory::now_secs();
    let mk = |id: &str, importance: f32, pinned: &str, last: u64| {
        let fact = crate::deep_memory::DeepFact {
            id: id.to_string(),
            content: format!("fact {id}"),
            summary: String::new(),
            essence: String::new(),
            fact_type: crate::deep_memory::FactType::Reference,
            scope: crate::deep_memory::MemoryScope::Global,
            pinned_by: match pinned {
                "user" => crate::deep_memory::PinnedBy::User,
                "agent" => crate::deep_memory::PinnedBy::Agent,
                _ => crate::deep_memory::PinnedBy::None,
            },
            subject_key: None,
            importance,
            created_at: 1,
            last_accessed: last,
            tags: vec![],
            links: vec![],
            archived: false,
        };
        store.deep_store(&fact).unwrap();
    };
    // stale + low importance + non-user -> should be archived
    mk("a", 2.0, "agent", now - 100 * 86_400);
    // stale + high importance -> keep
    mk("b", 4.5, "agent", now - 100 * 86_400);
    // stale + low + user pin -> keep
    mk("c", 2.0, "user", now - 100 * 86_400);
    // fresh + low -> keep
    mk("d", 2.0, "agent", now);

    let n = store.deep_archive_stale(60.0, 3.0).unwrap();
    assert_eq!(n, 1, "only 'a' should be archived");

    let active = store.deep_list("global").unwrap();
    let ids: Vec<String> = active.iter().map(|f| f.id.clone()).collect();
    assert!(!ids.contains(&"a".to_string()));
    assert!(ids.contains(&"b".to_string()));
    assert!(ids.contains(&"c".to_string()));
    assert!(ids.contains(&"d".to_string()));

    assert!(store.deep_restore("a").unwrap());
    let after_restore = store.deep_list("global").unwrap();
    assert!(after_restore.iter().any(|f| f.id == "a"));

    let archived = store.deep_list_archived("global").unwrap();
    assert!(archived.is_empty(), "restore removed it from archived view");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test deep_archive_soft_gc --lib`
Expected: FAIL — `no method named deep_archive_stale` / 字段 `archived` 不存在。

- [ ] **Step 3: 最小实现**

`src/deep_memory.rs` → `DeepFact` 加字段（最后、`#[serde(default)]`）：
```rust
    pub tags: Vec<String>,
    pub links: Vec<String>,
    /// 软归档标记：归档事实默认不出现在活跃召回/投影中，可 deep_restore 恢复。
    #[serde(default)]
    pub archived: bool,
```

`src/memory.rs` `ensure_two_tier_schema` 的 `execute_batch` 里，`links TEXT NOT NULL DEFAULT '[]'` 之后追加一行：
```sql
                archived  INTEGER NOT NULL DEFAULT 0
```
并在 `execute_batch(...)` 之后、`Ok(())` 之前，加幂等迁移（兼容老库）：
```rust
        {
            let has_archived: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('deep_facts') WHERE name='archived'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if has_archived == 0 {
                conn.execute_batch(
                    "ALTER TABLE deep_facts ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;",
                )
                .map_err(|e| format!("Two-tier schema add archived: {e}"))?;
            }
        }
```

`deep_store` 的 INSERT 保持 13 列不变（`archived` 由 `DEFAULT 0` 兜底；写入=活跃刷新，语义正确）。

`deep_row`（`src/memory.rs` 末）在 `links: ...` 行之前补：
```rust
        archived: r.get::<_, i64>(13)? != 0,
```

`deep_list` 过滤归档：
```rust
            .prepare("SELECT * FROM deep_facts WHERE (scope = 'global' OR scope = ?1) AND archived = 0")
```

新增三个方法（放在 `deep_forget` 之后）：
```rust
    /// 软归档单条：置 archived=1（不物理删除）。
    pub fn deep_archive(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute("UPDATE deep_facts SET archived = 1 WHERE id = ?1", params![id])
            .map_err(|e| format!("deep_archive: {e}"))?;
        Ok(n > 0)
    }

    /// 撤销软归档：置 archived=0。
    pub fn deep_restore(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute("UPDATE deep_facts SET archived = 0 WHERE id = ?1", params![id])
            .map_err(|e| format!("deep_restore: {e}"))?;
        Ok(n > 0)
    }

    /// 批量软归档：非 User pin 且 importance<threshold 且 last_accessed<now-tau 的事实置 archived=1。
    pub fn deep_archive_stale(&self, tau_days: f32, importance_threshold: f32) -> Result<usize, String> {
        let conn = self.conn.lock().unwrap();
        let now = crate::deep_memory::now_secs();
        let cutoff = (now as i64) - (tau_days as i64) * 86_400;
        let n = conn
            .execute(
                "UPDATE deep_facts SET archived = 1
                 WHERE archived = 0 AND pinned_by != 'user'
                   AND importance < ?1 AND last_accessed < ?2",
                params![importance_threshold as f64, cutoff],
            )
            .map_err(|e| format!("deep_archive_stale: {e}"))?;
        Ok(n)
    }

    /// 列出已归档事实（投影用/审计用）。
    pub fn deep_list_archived(&self, scope_key: &str) -> Result<Vec<crate::deep_memory::DeepFact>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT * FROM deep_facts WHERE (scope = 'global' OR scope = ?1) AND archived = 1")
            .map_err(|e| format!("deep_list_archived prepare: {e}"))?;
        let rows = stmt
            .query_map(params![scope_key], deep_row)
            .map_err(|e| format!("deep_list_archived query: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("deep row: {e}"))?);
        }
        Ok(out)
    }
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test deep_archive_soft_gc --lib`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src/deep_memory.rs src/memory.rs
git commit -m "feat(memory): add archived column + soft-archive/restore/stale-GC"
```

---

### Task 2: keyword 相关性重排（召回变准，无 embedding）

**Files:**
- Modify: `src/memory.rs`（新增自由函数 `keyword_relevance`，靠近 `extract_search_keywords`）
- Modify: `src/memory.rs:1740`（`deep_permanent_block` 加 `query` 参数、改排序、返回 `relevant_ids`）
- Test: `src/memory.rs` `mod tests_two_tier` + `src/deep_memory.rs` 纯测试

**Interfaces:**
- Consumes: `extract_search_keywords(&str) -> Vec<String>`（memory.rs:205）、`DeepFact::value(now) -> f64`（deep_memory.rs）、`pack_by_budget`（deep_memory.rs）。
- Produces:
  - 自由函数 `pub fn keyword_relevance(query: &str, fact: &DeepFact) -> f64`
  - `MemoryStore::deep_permanent_block(scope_key, query, p_max, tau_days) -> (String, usize, Vec<String>, Vec<String>)`——新签名：第 2 参 `query`，返回第 4 项 `relevant_ids`。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn keyword_relevance_ranks_related_higher() {
    let fact = |content: &str| crate::deep_memory::DeepFact {
        id: "x".into(), content: content.into(), summary: String::new(), essence: String::new(),
        fact_type: crate::deep_memory::FactType::Reference, scope: crate::deep_memory::MemoryScope::Global,
        pinned_by: crate::deep_memory::PinnedBy::Agent, subject_key: None,
        importance: 4.0, created_at: 1, last_accessed: 1, tags: vec![], links: vec![], archived: false,
    };
    let related = fact("Process count is 42, running svchost and explorer");
    let unrelated = fact("Oracle database backup completed at midnight");
    assert!(
        keyword_relevance("how many processes are running?", &related)
            > keyword_relevance("how many processes are running?", &unrelated)
    );
    assert_eq!(keyword_relevance("", &related), 0.0);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test keyword_relevance --lib`
Expected: FAIL — `keyword_relevance not found`.

- [ ] **Step 3: 最小实现**

在 `src/memory.rs` `extract_search_keywords` 之后（`impl MemoryStore` 之前）新增自由函数：
```rust
/// 用户查询与 deep fact 的确定性 keyword 相关度 ∈ [0,1]。
/// 复用 extract_search_keywords（CJK bigram 感知）；无 I/O、无 LLM。
pub fn keyword_relevance(query: &str, fact: &crate::deep_memory::DeepFact) -> f64 {
    let kws = extract_search_keywords(query);
    if kws.is_empty() {
        return 0.0;
    }
    let hay = format!(
        "{} {} {} {}",
        fact.content, fact.summary, fact.essence, fact.tags.join(" ")
    )
    .to_lowercase();
    let hits = kws.iter().filter(|k| hay.contains(k.as_str())).count();
    hits as f64 / kws.len() as f64
}
```

重写 `deep_permanent_block`（`src/memory.rs:1740`）：
```rust
    pub fn deep_permanent_block(
        &self,
        scope_key: &str,
        query: &str,
        p_max: usize,
        tau_days: f32,
    ) -> (String, usize, Vec<String>, Vec<String>) {
        use crate::deep_memory::{DeepParams, PinnedBy};
        let now = crate::deep_memory::now_secs();
        let params = DeepParams { tau_days, ..Default::default() };
        let facts = match self.deep_list(scope_key) {
            Ok(f) => f,
            Err(_) => return (String::new(), 0, Vec::new(), Vec::new()),
        };
        // lines: (渲染行, 排序分, token 成本, 事实 id)
        let mut lines: Vec<(String, f32, usize, String)> = Vec::new();
        let mut relevant_ids: Vec<String> = Vec::new();
        for f in &facts {
            let pin_user = f.pinned_by == PinnedBy::User;
            let pin_agent = f.pinned_by == PinnedBy::Agent;
            let i_eff = crate::deep_memory::effective_importance(
                f.importance,
                f.last_accessed,
                now,
                pin_user,
                tau_days,
            );
            if !crate::deep_memory::is_visible_permanent(i_eff, pin_user, pin_agent, &params) {
                continue;
            }
            let rel = keyword_relevance(query, f);
            if rel >= 0.25 {
                relevant_ids.push(f.id.clone());
            }
            let body = if f.summary.trim().is_empty() { f.content.trim() } else { f.summary.trim() };
            let line = format!("- [{}] {}", f.fact_type.as_str(), body);
            let cost = crate::deep_memory::estimate_tokens(&line);
            // 召回更准：排序分 = 价值 + 4×相关性；query 为空时纯价值（向后兼容）。
            let score = 4.0 * (rel as f32) + (f.value(now) as f32);
            lines.push((line, score, cost, f.id.clone()));
        }
        if lines.is_empty() {
            return (String::new(), 0, Vec::new(), relevant_ids);
        }
        let header = "## Permanent Memory (Deep) — durable facts about this user/project\n";
        let header_cost = crate::deep_memory::estimate_tokens(header);
        let body_budget = p_max.saturating_sub(header_cost);
        let items: Vec<(f32, usize)> = lines.iter().map(|(_, s, c, _)| (*s, *c)).collect();
        let picked = crate::deep_memory::pack_by_budget(&items, body_budget);
        if picked.is_empty() {
            return (String::new(), 0, Vec::new(), relevant_ids);
        }
        let mut out = String::from(header);
        let mut toks = header_cost;
        let mut picked_ids = Vec::with_capacity(picked.len());
        for idx in &picked {
            out.push_str(&lines[*idx].0);
            out.push('\n');
            toks += lines[*idx].2;
            picked_ids.push(lines[*idx].3.clone());
        }
        (out, toks, picked_ids, relevant_ids)
    }
```

再加排序断言测试：
```rust
#[test]
fn deep_permanent_block_ranks_relevant_first_and_flags_rel_ids() {
    let store = tmp_store();
    let now = crate::deep_memory::now_secs();
    let mk = |id: &str, content: &str, imp: f32| {
        store
            .deep_store(&crate::deep_memory::DeepFact {
                id: id.into(), content: content.into(), summary: String::new(), essence: String::new(),
                fact_type: crate::deep_memory::FactType::Reference, scope: crate::deep_memory::MemoryScope::Global,
                pinned_by: crate::deep_memory::PinnedBy::Agent, subject_key: None,
                importance: imp, created_at: now, last_accessed: now, tags: vec![], links: vec![], archived: false,
            })
            .unwrap();
    };
    mk("big-unrelated", "Database index maintenance scheduling", 5.0);
    mk("small-related", "Running processes: svchost, explorer, csrss", 2.5);
    let (block, _toks, _picked, rel) =
        store.deep_permanent_block("global", "what processes are running", 100_000, 60.0);
    let idx_small = block.find("Running processes").unwrap_or(usize::MAX);
    let idx_big = block.find("Database").unwrap_or(usize::MAX);
    assert!(idx_small < idx_big, "relevant should rank above high-importance unrelated: {block}");
    assert!(rel.contains(&"small-related".to_string()));
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test keyword_relevance deep_permanent_block --lib`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src/memory.rs
git commit -m "feat(memory): keyword-relevance rerank in deep_permanent_block (recall accuracy)"
```

---

### Task 3: 注入去"无条件 touch"——server 只 touch 相关 ID

**Files:**
- Modify: `src/server.rs:1801-1808`（deep 块注入点）
- Test: `cargo check --lib`（行为在 Task 2 已测；此处只改调用）

**Interfaces:**
- Consumes: `deep_permanent_block(scope, query, p_max, tau_days) -> (String, usize, Vec<String>, Vec<String>)`（Task 2 产出）；局部变量 `content: String`（用户消息，`is_recall_query(&content)` 处已在使用）。
- Produces: 无新签名；`deep_touch_batch` 只接收 `relevant_ids`，不再接收全部 picked。

- [ ] **Step 1: 修改调用点**

将 `src/server.rs:1801` 的：
```rust
let (eg_block, _eg_tok, eg_ids) = state.memory_store.deep_permanent_block("global", 1024, 60.0);
if !eg_block.trim().is_empty() {
    info!("Injected deep permanent block ({} chars)", eg_block.len());
    history.insert(0, ChatMessage::system(&eg_block));
    if let Ok(touched) = state.memory_store.deep_touch_batch(&eg_ids) {
        info!("Deep memory recall touched {} entries", touched);
    }
}
```
改为：
```rust
let (eg_block, _eg_tok, _eg_ids, rel_ids) =
    state.memory_store.deep_permanent_block("global", &content, 1024, 60.0);
if !eg_block.trim().is_empty() {
    info!("Injected deep permanent block ({} chars)", eg_block.len());
    history.insert(0, ChatMessage::system(&eg_block));
    // 仅"与当前问题相关"的事实才 touch，冷门条目自然退火（解"注入即续命"）。
    if let Ok(touched) = state.memory_store.deep_touch_batch(&rel_ids) {
        info!("Deep memory recall touched {} entries", touched);
    }
}
```

- [ ] **Step 2: 编译验证**

Run: `cargo check --lib`
Expected: 编译通过，无未使用变量告警（`_eg_ids` 下划线前缀）。

- [ ] **Step 3: 提交**

```bash
git add src/server.rs
git commit -m "fix(memory): touch only query-relevant deep facts on inject (break self-renewal)"
```

---

### Task 4: remember 默认 pin → Agent（仅显式记住才 User）

**Files:**
- Modify: `src/tool/deep_memory.rs`（新增 `parse_pin_and_importance`，remember 分支改用）
- Test: `src/tool/deep_memory.rs` 新增 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 无。
- Produces: `fn parse_pin_and_importance(args: &Value) -> (PinnedBy, f32)`——`"user"→(User,5.0)`、`"none"→(None,4.0)`、其余（含缺省）→`(Agent,4.0)`。供 remember 复用。注意 `importance` 参数仍可在 caller 覆写。

- [ ] **Step 1: 写失败测试（追加到文件末尾）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remember_defaults_to_agent_and_can_pin_user() {
        let mut map = serde_json::Map::new();
        let (p, imp) = DeepMemoryTool::parse_pin_and_importance(&serde_json::Value::Object(map));
        assert_eq!(p, PinnedBy::Agent);
        assert_eq!(imp, 4.0);

        let mut with_user = serde_json::Map::new();
        with_user.insert("pinned".into(), json!("user"));
        let (p2, imp2) = DeepMemoryTool::parse_pin_and_importance(&serde_json::Value::Object(with_user));
        assert_eq!(p2, PinnedBy::User);
        assert_eq!(imp2, 5.0);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test remember_defaults --lib`
Expected: FAIL — `parse_pin_and_importance not found`.

- [ ] **Step 3: 最小实现**

在 `src/tool/deep_memory.rs` 的 `impl DeepMemoryTool` 内新增：
```rust
    /// pin 与默认 importance：仅显式 pinned="user" 才 User(sacrosanct)；
    /// 其余默认 Agent(可退火可 GC)。caller 仍可用 importance 参数覆写。
    fn parse_pin_and_importance(args: &serde_json::Value) -> (PinnedBy, f32) {
        match args["pinned"].as_str() {
            Some("user") => (PinnedBy::User, 5.0),
            Some("none") => (PinnedBy::None, 4.0),
            _ => (PinnedBy::Agent, 4.0),
        }
    }
```

将 `remember` 分支的 pin 解析替换为调用它：
```rust
let (pinned, default_imp) = Self::parse_pin_and_importance(&args);
let importance = args["importance"]
    .as_f64()
    .map(|x| x as f32)
    .unwrap_or(default_imp)
    .clamp(deep_memory::IMPORTANCE_MIN, deep_memory::IMPORTANCE_MAX);
```
（删除原 `let pinned = match ...` 与旧的 `let importance = ...unwrap_or(if pinned == PinnedBy::User {...})` 两段。）

同步更新该工具文档字符串（`src/tool/deep_memory.rs` 顶部 action 说明）：
```
remember: content, summary?, essence?, type?, pinned?("user"|"agent"|"none",
          default "agent"), importance?(0-5, default: user=5, else 4), subject_key?, tags?[].
          仅当用户显式要求"记住/保存/长期保留"时才置 pinned="user"；
          其余会话结论建议默认(agent, 可 GC) 或由 curator 蒸馏。
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test remember_defaults --lib`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src/tool/deep_memory.rs
git commit -m "fix(memory): remember defaults to Agent pin; User only on explicit request"
```

---

### Task 5: 软归档接入后台 + MEMORY.md 投影分组/Top-N/折叠/归档区

**Files:**
- Modify: `src/server.rs`（`spawn_deep_curator` 结束时调 `deep_archive_stale`）
- Modify: `src/memory.rs:1646`（`write_memory_projection`：分组 Top-N + 归档区）
- Test: `src/memory.rs` `mod tests_two_tier`（投影分组逻辑不宜依赖文件，抽纯函数测分组/折叠计数）

**Interfaces:**
- Consumes: `deep_archive_stale(tau, thresh)`（Task 1）、`deep_list` / `deep_list_archived`（Task 1）、`FactType` 枚举（已有 `as_str`）。
- Produces:
  - 纯函数 `fn projection_groups(facts: &[DeepFact], top_per_group: usize) -> Vec<(FactType, usize, usize)>`——`(类型, 展示数, 折叠数)`。用于投影。
  - server curator 末尾新增 GC 调用。

- [ ] **Step 1: 写失败测试（投影分组纯函数）**

```rust
#[test]
fn projection_groups_caps_per_group_and_counts_collapsed() {
    let f = |id: &str, t: crate::deep_memory::FactType| crate::deep_memory::DeepFact {
        id: id.into(), content: id.into(), summary: String::new(), essence: String::new(),
        fact_type: t, scope: crate::deep_memory::MemoryScope::Global,
        pinned_by: crate::deep_memory::PinnedBy::Agent, subject_key: None,
        importance: 3.0, created_at: 1, last_accessed: 1, tags: vec![], links: vec![], archived: false,
    };
    let mut facts = Vec::new();
    for i in 0..10 {
        facts.push(f(&format!("r{i}"), crate::deep_memory::FactType::Reference));
    }
    facts.push(f("p1", crate::deep_memory::FactType::Preference));
    let groups = projection_groups(&facts, 8);
    let refs = groups.iter().find(|g| g.0 == crate::deep_memory::FactType::Reference).unwrap();
    assert_eq!(refs, &(crate::deep_memory::FactType::Reference, 8, 2));
    let prefs = groups.iter().find(|g| g.0 == crate::deep_memory::FactType::Preference).unwrap();
    assert_eq!(prefs, &(crate::deep_memory::FactType::Preference, 1, 0));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test projection_groups --lib`
Expected: FAIL — `projection_groups not found`.

- [ ] **Step 3: 最小实现**

在 `src/memory.rs`（`write_memory_projection` 之前的自由函数区）新增：
```rust
/// 投影分组统计：按 FactType 分组，返回 (类型, 展示数, 折叠数)。
pub fn projection_groups(
    facts: &[crate::deep_memory::DeepFact],
    top_per_group: usize,
) -> Vec<(crate::deep_memory::FactType, usize, usize)> {
    use crate::deep_memory::FactType;
    let order = [
        FactType::Identity,
        FactType::Preference,
        FactType::Project,
        FactType::Constraint,
        FactType::Reference,
    ];
    let mut out = Vec::new();
    for t in order {
        let mut in_group: Vec<_> = facts.iter().filter(|f| f.fact_type == t).collect();
        if in_group.is_empty() {
            continue;
        }
        // User pin 优先，其次 importance 降序
        in_group.sort_by(|a, b| {
            let pa = matches!(a.pinned_by, crate::deep_memory::PinnedBy::User) as u8;
            let pb = matches!(b.pinned_by, crate::deep_memory::PinnedBy::User) as u8;
            pb.cmp(&pa)
                .then(b.importance.partial_cmp(&a.importance).unwrap_or(std::cmp::Ordering::Equal))
        });
        let shown = in_group.len().min(top_per_group);
        out.push((t, shown, in_group.len() - shown));
    }
    out
}
```

改写 `write_memory_projection` 的 "durable facts" 渲染段（从 `let mut sorted = facts;` 到该循环结束）为分组渲染：
```rust
    // 每组最多展示 TOP_PER_GROUP 条，超出的折叠为计数行；末尾附归档区。
    const TOP_PER_GROUP: usize = 8;
    let groups = projection_groups(&facts, TOP_PER_GROUP);
    let archived_facts = self.deep_list_archived("global").unwrap_or_default();
    md.push_str(&format!(
        "_Generated from `deep_facts` · {} active + {} archived fact(s)_.  \n",
        facts.len(),
        archived_facts.len()
    ));
    for (t, shown, collapsed) in &groups {
        md.push_str(&format!("### {}\n", t.as_str()));
        let mut in_group: Vec<_> = facts
            .iter()
            .filter(|f| f.fact_type == *t)
            .collect();
        in_group.sort_by(|a, b| {
            let pa = matches!(a.pinned_by, crate::deep_memory::PinnedBy::User) as u8;
            let pb = matches!(b.pinned_by, crate::deep_memory::PinnedBy::User) as u8;
            pb.cmp(&pa)
                .then(b.importance.partial_cmp(&a.importance).unwrap_or(std::cmp::Ordering::Equal))
        });
        for f in in_group.iter().take(shown) {
            let pinned = match f.pinned_by {
                crate::deep_memory::PinnedBy::User => "user",
                crate::deep_memory::PinnedBy::Agent => "agent",
                crate::deep_memory::PinnedBy::None => "none",
            };
            md.push_str(&format!(
                "- **`{}`** — importance {} · pinned `{}`\n  {}\n",
                f.fact_type.as_str(),
                f.importance,
                pinned,
                f.content
            ));
            if let Some(k) = &f.subject_key {
                md.push_str(&format!("  key: `{}`\n", k));
            }
        }
        if *collapsed > 0 {
            md.push_str(&format!("- …and {} more (importance-led, view via deep_memory recall/list)\n", collapsed));
        }
        md.push('\n');
    }
    if !archived_facts.is_empty() {
        md.push_str(&format!(
            "\n## Archived (soft-GC, recoverable)\n_{} fact(s) archived; restore with the `deep_memory` tool (`update`) or `deep_restore`._\n",
            archived_facts.len()
        ));
    }
```
（保留下方 "## Recent Conversation Highlights" 段不变。注意原代码里 `let mut sorted = facts;` 后变量名与新增 `groups`/`archived_facts` 避免命名冲突——删除旧 `sorted` 循环即可。）

在 `src/server.rs` `spawn_deep_curator` 末尾（`write_memory_projection` 调用之后）追加 GC：
```rust
        // 软归档 GC：非 User、低重要度、久未访问的事实归档（可恢复，不物理删）。
        if let Ok(n) = state.memory_store.deep_archive_stale(60.0, 3.0) {
            if n > 0 {
                tracing::info!("[deep-curator] soft-archived {n} stale low-importance facts");
            }
        }
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test projection_groups --lib && cargo check --lib`
Expected: PASS + 编译通过

- [ ] **Step 5: 提交**

```bash
git add src/memory.rs src/server.rs
git commit -m "feat(memory): soft-GC in curator + grouped/Top-N/archived MEMORY.md projection"
```

---

## Self-Review

- **Spec 覆盖**：相关性重排(Task2/3)、pin 收紧(Task4)、软归档 GC(Task1/5)、MEMORY.md 治理(Task5) 均有对应任务。"解注入续命"由 Task3 落地；"召回更准"由 Task2 落地；"仅显式记住才 User"由 Task4 落地；评测集忽略、无 embedding 由 Global Constraints 固化。✅
- **占位符扫描**：本计划无 "TBD/TODO/implement later"；每步附完整可粘贴代码与 Run 命令。✅
- **类型一致性**：`deep_permanent_block` 新返回 4 元组 `(String, usize, Vec<String>, Vec<String>)` 在 Task2 定义、Task3 消费，字段一致；`parse_pin_and_importance` 返回 `(PinnedBy, f32)` 在 Task4 定义并使用；`projection_groups` 返回 `Vec<(FactType, usize, usize)>` 在 Task5 定义并使用。`deep_archive_stale(60.0, 3.0)` 调用与 Task1 签名 `(f32, f32)` 一致。✅

**已发现并内联修正的一点**：最初设想 `score = value × (1+2·rel)` 会导致"高重要度不相关"仍压过"低重要度相关"（5.0 不相关 vs 2.5 相关，q² 差异过大）。已改为加法混合 `score = value + 4·rel`，query 为空时退化为纯 `value`，既保证"召回更准"又保持无查询时的向后兼容。

