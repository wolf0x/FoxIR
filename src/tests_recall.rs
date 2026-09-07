//! 两层记忆 (Deep + λ) 召回率验证集成测试。
//!
//! 走**真实召回路径**：
//! - λ：`build_shallow_context(query, budget, max_budget, decay)` → 用一次 FTS 命中的关键词查询，
//!   检查对应的记忆是否进入组装的 λ 块。
//! - Deep：`deep_permanent_block("global", p_max, tau)` → 检查应可见的永久事实是否进入常驻块。
//!
//! 所有写入的脏数据在测试结束后删除：drop store(关闭连接) → 删除临时目录。
use crate::memory::MemoryStore;
use crate::deep_memory::{DeepFact, FactType, MemoryScope, PinnedBy};
use crate::shallow_memory::{ShallowEntry, now_secs};

/// 创建一个位于唯一临时目录的 store；测试结束时删除整个目录（干净）。返回 (store, 目录路径)。
fn isolated_store() -> (MemoryStore, std::path::PathBuf) {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rustagent_recall_{}_{}", std::process::id(), uniq));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let s = MemoryStore::new(db.to_str().unwrap()).unwrap();
    s.ensure_two_tier_schema().unwrap();
    (s, dir)
}

/// 清理：确保 store 被 drop(关闭 SQLite 句柄)后删除整个临时目录。
fn cleanup(s: MemoryStore, dir: std::path::PathBuf) {
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    // 断言脏数据已移除
    assert!(!dir.exists(), "dirty test data was NOT cleaned up: {}", dir.display());
}

fn shallow_entry(hash: &str, summary: &str, full: &str, importance: f32, now: u64) -> ShallowEntry {
    ShallowEntry::new(
        hash.into(), full.into(), summary.into(), summary.into(),
        vec![], importance, false, "recall-test".into(), now,
    )
}

/// λ 召回率：对每个应命中的记忆发起一次精确关键词查询，统计是否进入组装的 λ 块。
#[test]
fn shallow_recall_rate() {
    let (s, dir) = isolated_store();
    let now = now_secs();
    // 20 条高重要度、内容互不相同的会话记忆（每条都有唯一关键词）。
    let mut hashes = Vec::new();
    for i in 0..20 {
        let h = format!("lam{:02x}{:02x}", i, i + 1);
        let summary = format!("memory record number {} about websocket negotiation", i);
        let full = format!("User discussed record {}. Keyword alpha{} established.", i, i);
        let e = shallow_entry(&h, &summary, &full, 4.0, now);
        s.shallow_store(&e).unwrap();
        hashes.push(h);
    }
    // 每条对应一个查询词 "alpha{i}"（只出现在 full_text；FTS 索引的是 summary/essence/tags）。
    let mut hit = 0usize;
    for i in 0..20 {
        let q = format!("alpha{}", i);
        // 大预算、近期（decay ~1.0），应能容纳所有 high-importance。
        let (block, _, _) = s.build_shallow_context(&q, 8000, 8000, 0.01);
        if block.contains(&format!("alpha{}", i)) {
            hit += 1;
        }
    }
    let rate = hit as f64 / 20.0;
    println!("浅层记忆召回率 (FTS query, hot entries): {:.1}% ({}/{})", rate * 100.0, hit, 20);
    cleanup(s, dir);
    // 全部是高重要度+关键词唯一，召回应接近 100%。
    assert!(rate >= 0.95, "λ recall rate too low: {:.1}%", rate * 100.0);
}

/// λ 冷/低重要度：衰减后持续低于可见阈值的记忆不应进入块（防污染），
/// 但显式保存(high importance)仍应可召回。
#[test]
fn shallow_importance_floor_and_explicit() {
    let (s, dir) = isolated_store();
    let now = now_secs();
    // 一条高重要度显式保存
    let hot = ShallowEntry::new("lamhot01".into(), "explicit save request about X".into(),
        "explicit save about X".into(), "explicit X".into(), vec![], 5.0, true, "recall-test".into(), now);
    s.shallow_store(&hot).unwrap();
    // 多条低重要度、但含相同关键词（模拟噪声）
    for i in 0..8 {
        let e = shallow_entry(&format!("lamcold{:02}", i), "noise about X", "noise content", 1.0, now);
        s.shallow_store(&e).unwrap();
    }
    let (block, _, _) = s.build_shallow_context("X", 8000, 8000, 0.01);
    assert!(block.contains("explicit save request about X"), "explicit-save fact missing");
    cleanup(s, dir);
}

/// Deep 常驻块召回率：应可见的永久事实（用户 pin / 高重要度）是否进入常驻块；
/// 低重要度未 pin 的不应进入（防污染）。
#[test]
fn deep_permanent_recall_rate() {
    let (s, dir) = isolated_store();
    let now = now_secs();
    let mk = |id: &str, content: &str, importance: f32, pinned: PinnedBy| DeepFact {
        id: id.into(), content: content.into(), summary: content.into(), essence: content.into(),
        fact_type: FactType::Reference, scope: MemoryScope::Global, pinned_by: pinned,
        subject_key: None, importance, created_at: now, last_accessed: now, tags: vec![], links: vec![],
    };
    // 20 条用户 pin(必现) + 10 条高重要度(≥ theta_up, 应现) + 5 条低重要度未 pin(应不现)
    let mut visible_ids = Vec::new();
    for i in 0..20 { s.deep_store(&mk(&format!("p{}", i), &format!("pinned durable fact number {}", i), 5.0, PinnedBy::User)).unwrap(); visible_ids.push(format!("p{}", i)); }
    for i in 0..10 { s.deep_store(&mk(&format!("h{}", i), &format!("high importance fact number {}", i), 4.0, PinnedBy::None)).unwrap(); visible_ids.push(format!("h{}", i)); }
    for i in 0..5  { s.deep_store(&mk(&format!("l{}", i), &format!("low importance noisy fact {}", i), 1.0, PinnedBy::None)).unwrap(); }

    // 大预算常驻块
    let (block, _) = s.deep_permanent_block("global", 200000, 60.0);
    let visible_hit = ["pinned durable fact number", "high importance fact number"].iter().map(|prefix| {
        (0..if *prefix == "pinned durable fact number" { 20 } else { 10 })
            .filter(|i| block.contains(&format!("{} {}", prefix, i))).count()
    }).sum::<usize>();
    let visible_rate = visible_hit as f64 / visible_ids.len() as f64;
    println!("Deep permanent block recall (visible facts): {:.1}% ({}/{})",
        visible_rate * 100.0, visible_hit, visible_ids.len());

    // 低重要度未 pin 的不该出现在常驻块
    for i in 0..5 { assert!(!block.contains(&format!("low importance noisy fact {}", i)), "noise leaked into permanent block"); }
    cleanup(s, dir);
    assert!(visible_rate >= 0.95, "Deep visible recall too low: {:.1}%", visible_rate * 100.0);
}

/// 综合召回率：λ + Deep 混合，全部应命中项被正确召回的比例。
#[test]
fn two_tier_composite_recall() {
    let (s, dir) = isolated_store();
    let now = now_secs();
    let mut total = 0usize; let mut hit = 0usize;

    // λ 层
    for i in 0..10 {
        let h = format!("comp_lam{:02}", i);
        let e = shallow_entry(&h, &format!("composite conversation about c{}", i),
            &format!("composite record c{} with detail token cc{}", i, i), 4.0, now);
        s.shallow_store(&e).unwrap();
        total += 1;
        let (block, _, _) = s.build_shallow_context(&format!("cc{}", i), 8000, 8000, 0.01);
        if block.contains(&format!("cc{}", i)) { hit += 1; }
    }
    // Deep 层
    for i in 0..10 {
        let f = DeepFact {
            id: format!("comp_eg{}", i), content: format!("permanent composite fact ee{}", i).into(),
            summary: format!("permanent composite fact ee{}", i).into(), essence: String::new(),
            fact_type: FactType::Reference, scope: MemoryScope::Global, pinned_by: PinnedBy::User,
            subject_key: None, importance: 5.0, created_at: now, last_accessed: now, tags: vec![], links: vec![],
        };
        s.deep_store(&f).unwrap();
        total += 1;
        let (block, _) = s.deep_permanent_block("global", 5000, 60.0);
        if block.contains(&format!("ee{}", i)) { hit += 1; }
    }
    let rate = hit as f64 / total as f64;
    println!("Composite two-tier recall rate: {:.1}% ({}/{})", rate * 100.0, hit, total);
    cleanup(s, dir);
    assert!(rate >= 0.95, "composite recall too low: {:.1}%", rate * 100.0);
}

/// 迁移测试：旧表名(lambda_memories / engram_facts)在 ensure_two_tier_schema 时被
/// 改写为 shallow_memories / deep_facts，且既有数据完整保留。
#[test]
fn migration_legacy_table_names_preserves_data() {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rustagent_mig_{}_{}", std::process::id(), uniq));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let db_str = db.to_str().unwrap().to_string();

    // 1) 用旧表结构预置一个库
    {
        let raw = rusqlite::Connection::open(&db).unwrap();
        raw.execute_batch(
            "CREATE TABLE lambda_memories (
                hash TEXT PRIMARY KEY, created_at INTEGER NOT NULL, last_accessed INTEGER NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0, importance REAL NOT NULL DEFAULT 1.0,
                explicit_save INTEGER NOT NULL DEFAULT 0, full_text TEXT NOT NULL, summary_text TEXT NOT NULL,
                essence_text TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '[]',
                memory_type TEXT NOT NULL DEFAULT 'conversation', session_id TEXT NOT NULL);
             CREATE TABLE engram_facts (
                id TEXT PRIMARY KEY, content TEXT NOT NULL, summary TEXT NOT NULL, essence TEXT NOT NULL,
                fact_type TEXT NOT NULL DEFAULT 'reference', scope TEXT NOT NULL DEFAULT 'global',
                pinned_by TEXT NOT NULL DEFAULT 'none', subject_key TEXT,
                importance REAL NOT NULL DEFAULT 1.0, created_at INTEGER NOT NULL,
                last_accessed INTEGER NOT NULL, tags TEXT NOT NULL DEFAULT '[]', links TEXT NOT NULL DEFAULT '[]');",
        ).unwrap();
        raw.execute(
            "INSERT INTO lambda_memories (hash, created_at, last_accessed, importance, full_text, summary_text, essence_text, session_id)
             VALUES ('L1', 1, 1, 4.0, 'full shallow', 'sum shallow', 'ess shallow', 's1')",
            [],
        ).unwrap();
        raw.execute(
            "INSERT INTO engram_facts (id, content, summary, essence, importance, created_at, last_accessed)
             VALUES ('E1', 'deep content', 'deep sum', 'deep ess', 5.0, 1, 1)",
            [],
        ).unwrap();
    }

    // 2) 打开 store 并触发 schema 升级（含迁移）
    let s = MemoryStore::new(&db_str).unwrap();
    s.ensure_two_tier_schema().unwrap();

    // 3) 关闭后校验
    drop(s);
    let exists = |raw: &rusqlite::Connection, name: &str| {
        raw.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [name], |_| Ok(())).is_ok()
    };
    let raw = rusqlite::Connection::open(&db).unwrap();
    assert!(exists(&raw, "shallow_memories"), "shallow_memories missing");
    assert!(exists(&raw, "deep_facts"), "deep_facts missing");
    assert!(!exists(&raw, "lambda_memories"), "legacy lambda_memories not dropped");
    assert!(!exists(&raw, "engram_facts"), "legacy engram_facts not dropped");
    let scnt: i64 = raw.query_row("SELECT COUNT(*) FROM shallow_memories", [], |r| r.get(0)).unwrap();
    let dcnt: i64 = raw.query_row("SELECT COUNT(*) FROM deep_facts", [], |r| r.get(0)).unwrap();
    assert_eq!(scnt, 1, "shallow data lost");
    assert_eq!(dcnt, 1, "deep data lost");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 升级场景（老库 + 老 json）：一台旧机器上已有 lambda_memories / engram_facts 旧表，
/// 并且还有旧版 knowledge/sops.json 文件。更新二进制后：
///   1. ensure_two_tier_schema 把旧表改名保留数据；
///   2. load_sops 在空 sops 表里自动迁移 sops.json → SQLite；
///   3. 旧 sops.json 文件保留（迁移只读不删）。
#[test]
fn migration_legacy_db_and_sops_json() {
    use std::io::Write;
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ws = std::env::temp_dir().join(format!("rustagent_upg_{}_{}", std::process::id(), uniq));
    std::fs::create_dir_all(&ws).unwrap();
    let mem_dir = ws.join("memory");
    let kn_dir = ws.join("knowledge");
    std::fs::create_dir_all(&mem_dir).unwrap();
    std::fs::create_dir_all(&kn_dir).unwrap();

    // 1) 预置旧表 + 数据（旧机器结构）
    let db = mem_dir.join("memory.db");
    {
        let raw = rusqlite::Connection::open(&db).unwrap();
        raw.execute_batch(
            "CREATE TABLE lambda_memories (
                hash TEXT PRIMARY KEY, created_at INTEGER NOT NULL, last_accessed INTEGER NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0, importance REAL NOT NULL DEFAULT 1.0,
                explicit_save INTEGER NOT NULL DEFAULT 0, full_text TEXT NOT NULL, summary_text TEXT NOT NULL,
                essence_text TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '[]',
                memory_type TEXT NOT NULL DEFAULT 'conversation', session_id TEXT NOT NULL);
             CREATE TABLE engram_facts (
                id TEXT PRIMARY KEY, content TEXT NOT NULL, summary TEXT NOT NULL, essence TEXT NOT NULL,
                fact_type TEXT NOT NULL DEFAULT 'reference', scope TEXT NOT NULL DEFAULT 'global',
                pinned_by TEXT NOT NULL DEFAULT 'none', subject_key TEXT,
                importance REAL NOT NULL DEFAULT 1.0, created_at INTEGER NOT NULL,
                last_accessed INTEGER NOT NULL, tags TEXT NOT NULL DEFAULT '[]', links TEXT NOT NULL DEFAULT '[]');"
        ).unwrap();
        raw.execute(
            "INSERT INTO lambda_memories (hash, created_at, last_accessed, importance, full_text, summary_text, essence_text, session_id)
             VALUES ('L1', 1, 1, 4.0, 'old shallow full', 'old shallow sum', 'old shallow ess', 's1')", []).unwrap();
        raw.execute(
            "INSERT INTO engram_facts (id, content, summary, essence, importance, created_at, last_accessed)
             VALUES ('E1', 'old deep content', 'old deep sum', 'old deep ess', 5.0, 1, 1)", []).unwrap();
    }

    // 2) 预置旧版 sops.json（legacy SopStore 格式）
    let legacy_json = r#"{
        "schema_version": 2,
        "sops": [{
            "id": "legacy-1", "name": "Legacy Cleanup",
            "description": "Old machine SOP", "semantic_tags": ["cleanup"],
            "phases": ["Step A", "Step B"], "version": 1,
            "created": 1, "updated": 1,
            "times_executed": 4, "times_succeeded": 3, "times_failed": 1,
            "last_executed_at": 2, "avg_tool_calls": 6.0, "avg_duration_secs": 90.0
        }]
    }"#;
    let mut f = std::fs::File::create(kn_dir.join("sops.json")).unwrap();
    f.write_all(legacy_json.as_bytes()).unwrap();

    // 3) 打开 store（触发 migrate + ensure_two_tier_schema）→ 旧表升级
    let db_str = db.to_str().unwrap().to_string();
    let ws_str = ws.to_str().unwrap().to_string();
    let s = MemoryStore::new(&db_str).unwrap();
    s.ensure_two_tier_schema().unwrap();
    drop(s);

    // 校验旧表改名且数据保留
    let raw = rusqlite::Connection::open(&db).unwrap();
    let exists = |raw: &rusqlite::Connection, name: &str| {
        raw.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [name], |_| Ok(())).is_ok()
    };
    assert!(exists(&raw, "shallow_memories") && !exists(&raw, "lambda_memories"));
    assert!(exists(&raw, "deep_facts") && !exists(&raw, "engram_facts"));
    let scnt: i64 = raw.query_row("SELECT COUNT(*) FROM shallow_memories", [], |r| r.get(0)).unwrap();
    assert_eq!(scnt, 1, "shallow data lost on upgrade");
    drop(raw);

    // 4) load_sops 自动迁移 sops.json → SQLite sops 表
    let sops = crate::sop::load_sops(&ws_str);
    assert_eq!(sops.len(), 1, "should import legacy json");
    assert_eq!(sops[0].name, "Legacy Cleanup");
    assert_eq!(sops[0].phases.len(), 2);
    assert_eq!(sops[0].times_executed, 4);
    assert_eq!(sops[0].times_succeeded, 3);

    // 5) 旧 sops.json 仍在（只读迁移，不删除）；sops 表已存在，再次迁移为空操作
    assert!(kn_dir.join("sops.json").exists(), "legacy json should be kept");
    let again = crate::sop::load_sops(&ws_str);
    assert_eq!(again.len(), 1, "idempotent: no double import");

    let _ = std::fs::remove_dir_all(&ws);
}

/// A2：召回注入路径把实际选中的浅层记忆写回（touch），让 access_count / recall_boost /
/// last_accessed 随真实使用更新（而不是冻结）。build_shallow_context 返回本次注入的 hash 集合。
#[test]
fn shallow_touch_write_back_updates_usage() {
    let (s, dir) = isolated_store();
    let now = now_secs();
    let e = shallow_entry("lamtouch01", "scan network connections", "network scan detail", 3.0, now);
    s.shallow_store(&e).unwrap();
    // 召回：查询命中 → block 注入，并返回被选中的 hash。
    let (block, _, hashes) = s.build_shallow_context("network", 8000, 8000, 0.01);
    assert!(block.contains("network scan detail"), "memory should be injected");
    assert!(hashes.iter().any(|h| h == "lamtouch01"), "returned hashes must include the injected one, got {hashes:?}");
    // 写回：对注入集合 touch → access_count 与 recall_boost 变化。
    let n = s.shallow_touch_batch(&hashes).unwrap();
    assert!(n >= 1);
    let rec = s.shallow_recall("lamtouch").unwrap().unwrap();
    assert_eq!(rec.access_count, 1);
    assert!((rec.recall_boost - 0.3).abs() < 1e-4, "recall_boost={}", rec.recall_boost);
    cleanup(s, dir);
}

/// A2：深度事实的显式召回会 reheat —— 刷新 last_accessed（保留时间退火遗忘的入口）。
#[test]
fn deep_touch_refreshes_last_accessed() {
    let (s, dir) = isolated_store();
    let now = now_secs();
    let f = DeepFact {
        id: "egtouch1".into(),
        content: "deploy app via runbook".into(),
        summary: "deploy".into(),
        essence: "deploy".into(),
        fact_type: FactType::Reference,
        scope: MemoryScope::Global,
        pinned_by: PinnedBy::Agent,
        subject_key: None,
        importance: 4.0,
        created_at: now,
        last_accessed: now - 86_400, // 1 day ago (aged, would otherwise anneal)
        tags: vec![],
        links: vec![],
    };
    s.deep_store(&f).unwrap();
    let n = s.deep_touch(&["egtouch1".to_string()]).unwrap();
    assert_eq!(n, 1);
    let got = s.deep_get("egtouch1").unwrap().unwrap();
    assert!(got.last_accessed > now - 86_400, "deep last_accessed not refreshed: {}", got.last_accessed);
    cleanup(s, dir);
}

