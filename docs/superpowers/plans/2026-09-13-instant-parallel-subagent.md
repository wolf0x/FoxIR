# Instant 常态多子 Agent 并行 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 FoxIR 的 Instant 模式常态具备「一轮并发发射多个只读子 Agent、一次收敛回收」的并行能力，且不触碰 Expert 串行链、不削弱 agent loop。

**Architecture:** 保留每运行的编排器实例（借 PCBuddy 的「工具常态可用」而非其单例物理形态）。改动集中在三处：① 拆除规则预筛硬门，交付/构建改由 `can_spawn && Instant && depth==0` 常态决定；② 事件泵由入口急起改首次发射懒起（守住无常驻任务）；③ 回收侧补 `wait_all` + 等待类工具的看门狗超时档豁免（拆解阻塞主循环时的 60s 静默/300s 墙钟掐断雷）。写/exec 子代理仍全局写锁串行、单独授权，不入并发扇出。

**Tech Stack:** Rust + tokio（`tokio::spawn` / `Notify` / `mpsc`）、`futures::future::join_all`、`#[tokio::test]`、cargo。

**Spec:** [docs/superpowers/specs/2026-09-13-instant-parallel-subagent-design.md](file:///e:/VSCode/FoxIR/docs/superpowers/specs/2026-09-13-instant-parallel-subagent-design.md)（评审通过 v0.2：契约选 A、并行度 4、反向审阅 3 硬伤）

## Global Constraints

- **Expert 零改动**：所有新增判定一律限定 `mode == Instant && depth == 0`；`orchestration_allowset` 对非 Instant / depth≥1 恒返回空。
- **不碰主循环工具执行主干**：[llm_agent.rs#L2736-L2810](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L2736-L2810)（并发/串行分支）、[#L3443-L3594](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L3443-L3594)（超时 select）不改；本计划只新增工具自描述与编排子系统内部逻辑。
- **并行度**：`max_concurrent_subagents` 保持默认 **4**。
- **并行范围**：常态并行**仅只读**子代理；`allow_write || allow_exec` 子代理维持全局写锁串行 + `request_spawn_authorization` 单独授权。
- **回收范式**：维持拉取式（`wait` / `wait_all`），不引入推送式 announce。
- **一级回滚点**：[llm_agent.rs#L1720-L1723](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L1720-L1723) 恢复 `&& prefilter` 一行即回到 v1.1-hardened 行为；Task 2 明确标注此回滚。
- **中文注释**；**PowerShell 命令用 `;` 分隔，禁用 `&&`**；每个 Step 附验证命令，绿了才 Commit。
- **工具定义 token** 计入既有预算账本，不新增账本外扣减。

---

## 文件结构（改动地图）

| 文件 | 职责 | 本计划动作 |
|---|---|---|
| [src/agent/orchestration.rs](file:///e:/VSCode/FoxIR/src/agent/orchestration.rs) | 编排器内核 | 事件泵懒起；新增 `wait_many`；发射时角色唯一校验 |
| [src/agent/llm_agent.rs](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs) | 交付/构建门 + 提示词 | 去预筛硬门；预筛降级软提示；`ALL_ORCH` 接 `wait_all`；扇出提示词 |
| [src/tool/orchestration.rs](file:///e:/VSCode/FoxIR/src/tool/orchestration.rs) | 编排工具定义 | 等待类工具超时档豁免；新增 `WaitAllSubagentsTool`；发射描述改；`ORCH_TOOL_NAMES`/注册表接入 |

---

## 测试策略修正（执行前必读，覆盖下文各 Task 的测试片段）

> 建 worktree 后取证发现仓库已有可复用的并行验收夹具，据此修正测试接线（**不改**已批准的架构/契约）。

- **夹具复用既有 `phase0_acceptance.rs`（`#[cfg(test)] mod`，见 [main.rs#L45-L46](file:///e:/VSCode/FoxIR/src/main.rs#L45-L46)）**：其中 `start_mock(latency_ms)`（axum 假 `/v1/chat/completions`）、`Bench::new`、`Bench::spec(role)`、`make_orch(env, root)`、`spawn_wait` 已构成一套 in-process mock provider，worker 可确定性跑到 `SubAgentStatus::Ok`，无网络/无真 LLM。计划里悬空的 `make_test_orchestrator` / `read_only_spec` / `PER_WORKER_TEST_SECS` **一律改用这里的同型夹具**（`make_orch` / `Bench::spec`），不要新造。
- **`spawn` 收 `&SubAgentSpec`**（[orchestration.rs#L452](file:///e:/VSCode/FoxIR/src/agent/orchestration.rs#L452)）：下文所有 `orch.spawn(read_only_spec(..), 0, ..)` 片段须写成 `orch.spawn(&spec, 0, ..)`。
- **单元测试放对模块**：
  - 触碰 `Orchestrator` 私有字段/方法（`pump_started`、`children`、`wait_many`、角色唯一校验）的测试，写进 `orchestration.rs` 自身的 `#[cfg(test)] mod tests`。这类测试**不需要 worker 真跑完**：`spawn().await` 在 `tokio::spawn(run_worker)` 之前同步完成泵懒起、角色校验、限流判定；provider 只在被 dispatch 的 worker 任务里才触达。因此用最小 env（`OpenAiProvider::new(vec![])` + 空 `ToolRegistry`）即可测发射路径；需要终值时对 `children` **直插合成 handle**（预置 `result=Some(终值)` + 终态 `status`）验证 `wait_many` 快路径收敛，不依赖网络。
  - 需要 worker 真并发跑完/计时的端到端测试，写进 `phase0_acceptance.rs`（夹具同作用域），复用 `make_orch` + `Bench`。
- **§10 收益门槛已存在**：`phase0_wallclock_parallel_ge_30pct`（[phase0_acceptance.rs#L234](file:///e:/VSCode/FoxIR/src/phase0_acceptance.rs#L234)）就是「并行比串行快 ≥30%」的可复现门。Task 8 **不重造计时测试**，改为「确认本改动后该门仍绿」+ 用 `make_orch`+`Bench` 补一条 `wait_many` 端到端。
- **交付测试硬编码**：既有 [llm_agent.rs#L4418](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L4418) 已用 `ALL_ORCH.len()` 动态断言（无需改）；真正需随 7→8 更新的是 `tool/orchestration.rs` 注册表 `assert_eq!(reg.len(), 7)`（并入 Task 5）。

---

## Task 1: 事件泵懒起（编排器构造不再立即起后台任务）

**Files:**
- Modify: `src/agent/orchestration.rs`（`Orchestrator` 结构体约 L184-L209、`new` L232-L241、`spawn` L589 前）

**Interfaces:**
- Consumes: `EventPump::new(reg_rx).run()`、`self.reg_tx`、`WORKER_CHANNEL_CAP`
- Produces: `Orchestrator::ensure_event_pump(&self)`；字段 `pending_reg_rx: Arc<Mutex<Option<mpsc::Receiver<mpsc::Receiver<AgentEvent>>>>>`、`pump_started: Arc<AtomicBool>`

- [ ] **Step 1: 写失败测试**（构造编排器后不启泵；首次发射后启泵）

`src/agent/orchestration.rs` 测试模块新增：
```rust
#[tokio::test]
async fn event_pump_is_lazy_started_on_first_spawn() {
    // 构造 Instant 根编排器（parent_tx=Some），断言 new() 后 pump_started=false
    let (orch, _parent_rx) = make_test_orchestrator(); // 复用/新增测试工厂，见 Step3
    assert!(!orch.pump_started.load(std::sync::atomic::Ordering::SeqCst),
        "构造编排器不得立即起事件泵");
    // 首次 spawn 一个只读 worker 后，泵应已起
    let _rid = orch.spawn(read_only_spec("probe"), 0, "inv-root", "sess-root", "author").await.unwrap();
    // 让被 dispatch 的 worker task 有机会跑到 ensure_event_pump
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(orch.pump_started.load(std::sync::atomic::Ordering::SeqCst),
        "首次发射必须懒起事件泵");
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib event_pump_is_lazy_started_on_first_spawn`
Expected: 编译失败（无 `pump_started` 字段 / 无测试工厂）或断言失败。

- [ ] **Step 3: 加字段 + 拆分 `new` + `ensure_event_pump`**

结构体新增两字段（与 `reg_tx` 相邻）：
```rust
pending_reg_rx: std::sync::Arc<std::sync::Mutex<Option<
    tokio::sync::mpsc::Receiver<tokio::sync::mpsc::Receiver<AgentEvent>>>>>,
pump_started: std::sync::Arc<std::sync::atomic::AtomicBool>,
```
`new`（现 L232-L241）改为：建通道但**不** `tokio::spawn`，把 `reg_rx` 存入 `pending_reg_rx`：
```rust
let event_tx = parent_tx.clone();
let (reg_tx, pending_reg_rx) = if parent_tx.is_some() {
    let cap = env.max_concurrent_subagents.max(1) * 2;
    let (tx, rx) = tokio::sync::mpsc::channel::<
        tokio::sync::mpsc::Receiver<AgentEvent>>(cap);
    (Some(tx), std::sync::Arc::new(std::sync::Mutex::new(Some(rx))))
} else {
    (None, std::sync::Arc::new(std::sync::Mutex::new(None)))
};
```
`Self { ... reg_tx, pending_reg_rx, pump_started: std::sync::Arc::new(AtomicBool::new(false)), ... }`

新增方法：
```rust
/// 首次发射时懒起事件泵；并发安全（仅一个调用者 take 到 rx）。
fn ensure_event_pump(&self) {
    if self.pump_started.load(std::sync::atomic::Ordering::SeqCst) { return; }
    let rx = self.pending_reg_rx.lock().unwrap().take();
    if let Some(rx) = rx {
        tokio::spawn(EventPump::new(rx).run());
        self.pump_started.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
```

- [ ] **Step 4: 在 `spawn` 注册 worker 前调用**（现 L589 `if self.reg_tx.is_some()` 之前）
```rust
self.ensure_event_pump();
```

- [ ] **Step 5: 运行验证通过 + 提交**

Run: `cargo test --lib event_pump_is_lazy_started_on_first_spawn`（Expected PASS）；`cargo test --lib orchestration`（既有编排测试不回归）
```powershell
git add src/agent/orchestration.rs ; git commit -m "feat(orch): 事件泵改为首次发射懒起"
```

---

## Task 2: 拆除预筛硬门 → 常态候选（含一级回滚点）

**Files:**
- Modify: `src/agent/llm_agent.rs`（`orch_candidate` L1719-L1734、交付门注释 L1739-L1742、构建门 L1848-L1852、`orchestration_delivered_for` L492-L498、预筛函数保留但降级为软提示）

**Interfaces:**
- Consumes: `ctx.can_spawn`、`ctx.mode`、`ctx.depth`、`orchestration_prefilter`（降级后仅作提示）
- Produces: `orch_candidate = can_spawn && Instant && depth==0`（不再依赖 prefilter）

- [ ] **Step 1: 写失败测试**（trivial 消息也候选；Expert/depth≥1 不候选）

`src/agent/llm_agent.rs` 测试模块新增（若已有相似门测试则并入翻转）：
```rust
#[test]
fn orch_candidate_is_steady_for_instant_root_regardless_of_prefilter() {
    // trivial 措辞（预筛=false）也必须为候选：证明硬门已拆
    assert!(!orchestration_prefilter("你好"));
    let candidate = true /* can_spawn */ && /* mode=Instant */ true && /* depth==0 */ true;
    // 直接断言交付函数在 candidate=true 时对 Instant/0 放行全部编排工具
    let allow = orchestration_delivered_for(crate::context::AgentMode::Instant, 0, true);
    assert_eq!(allow.len(), ALL_ORCH.len());
    // Expert 即便 candidate 误传真，allowset 仍空（隔离红线）
    assert!(orchestration_delivered_for(crate::context::AgentMode::Expert, 0, true).is_empty());
    assert!(orchestration_delivered_for(crate::context::AgentMode::Instant, 1, true).is_empty());
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib orch_candidate_is_steady_for_instant_root_regardless_of_prefilter`
Expected: 断言/编译失败（当前 `orch_candidate` 仍含 prefilter）。

- [ ] **Step 3: 改候选计算 + 降级预筛为软提示 + 构建门**

L1719-L1723 改为（**回滚点：恢复末尾 `&& prefilter` 即回到 v1.1**）：
```rust
let prefilter = orchestration_prefilter(user_message); // 保留计算，仅供提示/日志
let orch_candidate = ctx.can_spawn
    && ctx.mode == crate::context::AgentMode::Instant
    && ctx.depth == 0; // 去掉 `&& prefilter`：常态候选
```
L1726-L1734 日志行文案改述 `prefilter` 为「扇出提示」而非「门」。构建门 L1848-L1852 的条件由 `can_spawn && Instant && depth==0 && orch_candidate` 简化为 `orch_candidate`（已内含三者），保持不变语义即可：
```rust
let orch: Option<Arc<...::Orchestrator>> = if orch_candidate { /* env 构造 + new + register 原样 */ } else { None };
```
交付门 L1742 与 `orchestration_delivered_for`（L494-L498）签名不变（candidate 语义变宽）。更新 L492-L493 注释：投递不再受预筛硬门，改由 mode/depth/can_spawn 常态决定。

- [ ] **Step 4: 运行验证通过**

Run: `cargo test --lib orch_candidate_is_steady_for_instant_root_regardless_of_prefilter`（PASS）；`cargo test --lib orchestration`（含 L4409 既有交付断言不回归）

- [ ] **Step 5: 提交**
```powershell
git add src/agent/llm_agent.rs ; git commit -m "feat(orch): 拆预筛硬门，Instant 常态交付编排能力"
```

---

## Task 3: 等待类工具的看门狗超时档豁免（拆 F5 雷）

**Files:**
- Modify: `src/tool/orchestration.rs`（`WaitSubagentTool` impl L104-L117 增 `timeout_stage`）

**Interfaces:**
- Consumes: `Tool::timeout_stage`、`crate::tool::TimeoutStage::Watchdog`
- Produces: 等待类工具执行时获得 ≈24h 墙钟 + 600s 静默容忍（[llm_agent.rs#L3443-L3449](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L3443-L3449)、[tool/mod.rs#L113/L124](file:///e:/VSCode/FoxIR/src/tool/mod.rs#L113-L124)）

- [ ] **Step 1: 写失败测试**

`src/tool/orchestration.rs` 测试模块：
```rust
#[test]
fn wait_subagent_uses_watchdog_timeout_stage() {
    use crate::tool::{Tool, TimeoutStage};
    assert_eq!(WaitSubagentTool.timeout_stage(), TimeoutStage::Watchdog);
    assert!(WaitSubagentTool.timeout_secs().is_none(), "看门狗档无硬墙钟");
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib wait_subagent_uses_watchdog_timeout_stage`
Expected: FAIL（默认 `Normal`）。

- [ ] **Step 3: 覆盖 `timeout_stage`**

`impl Tool for WaitSubagentTool` 内加：
```rust
fn timeout_stage(&self) -> crate::tool::TimeoutStage {
    // 等待子代理回收：子代理默认存活上限 300s（default_timeout_secs），
    // 看门狗档静默容忍 600s > 300s，且无硬墙钟，避免等待被主循环工具超时掐断。
    crate::tool::TimeoutStage::Watchdog
}
```

- [ ] **Step 4: 运行验证通过 + 提交**

Run: `cargo test --lib wait_subagent_uses_watchdog_timeout_stage`（PASS）
```powershell
git add src/tool/orchestration.rs ; git commit -m "fix(orch): wait_subagent 用看门狗超时档，避免被循环工具超时掐断"
```

---

## Task 4: `wait_many` 内核（并发收敛多个子代理结果）

**Files:**
- Modify: `src/agent/orchestration.rs`（`wait` L671-L691 之后新增 `wait_many`）

**Interfaces:**
- Consumes: `self.wait(run_id)`（已含"先查终值快路径、否则订阅 `done.notified()`"）
- Produces: `Orchestrator::wait_many(&self, run_ids: &[String]) -> Vec<AgentResult<SubAgentResult>>`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn wait_many_collects_all_terminal_results() {
    let (orch, _rx) = make_test_orchestrator();
    let a = orch.spawn(read_only_spec("role-a"), 0, "inv", "s", "au").await.unwrap();
    let b = orch.spawn(read_only_spec("role-b"), 0, "inv", "s", "au").await.unwrap();
    let res = orch.wait_many(&[a.clone(), b.clone()]).await;
    assert_eq!(res.len(), 2);
    // 每个都应是终值（成功/超时/失败均终态），不得有因丢通知而永久挂起
    assert!(res.iter().all(|r| r.is_ok()));
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib wait_many_collects_all_terminal_results`
Expected: FAIL（`wait_many` 未定义）。

- [ ] **Step 3: 实现（复用 wait 的短路，天然防丢通知）**

`impl Orchestrator` 内，`wait` 之后：
```rust
/// 并发收敛多个子代理终值。复用单 wait 的「先查终值、否则订阅完成通知」，
/// 故对已完成/后到者均不丢通知；join_all 让多路等待真正并发。
pub async fn wait_many(&self, run_ids: &[String]) -> Vec<AgentResult<SubAgentResult>> {
    let futs = run_ids.iter().map(|rid| self.wait(rid));
    futures::future::join_all(futs).await
}
```

- [ ] **Step 4: 运行验证通过 + 提交**

Run: `cargo test --lib wait_many_collects_all_terminal_results`（PASS）
```powershell
git add src/agent/orchestration.rs ; git commit -m "feat(orch): 新增 wait_many 并发收敛多子代理结果"
```

---

## Task 5: `WaitAllSubagentsTool` 接入（新工具 + 三处登记）

**Files:**
- Modify: `src/tool/orchestration.rs`（新增结构体 + `ORCH_TOOL_NAMES` L21-L29 扩为 8 + `register_orchestration_tools` L32-L37 注册）
- Modify: `src/agent/llm_agent.rs`（`ALL_ORCH` L466-L474 扩为 8）

**Interfaces:**
- Consumes: `o.wait_many(&run_ids)`；`gate(ctx)`、`orch(ctx)`
- Produces: 工具名 `wait_all_subagents`，入参 `{ "run_ids": [string] }`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn wait_all_is_registered_and_delivered() {
    use crate::tool::Tool;
    assert_eq!(WaitAllSubagentsTool.name(), "wait_all_subagents");
    assert!(ORCH_TOOL_NAMES.contains(&"wait_all_subagents"));
    assert!(crate::agent::llm_agent::ALL_ORCH.contains(&"wait_all_subagents"));
    assert_eq!(WaitAllSubagentsTool.timeout_stage(), crate::tool::TimeoutStage::Watchdog);
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib wait_all_is_registered_and_delivered`
Expected: FAIL（类型/名字未定义）。

- [ ] **Step 3: 定义工具 + 扩两数组 + 注册**

`src/tool/orchestration.rs` 新增（紧邻 `WaitSubagentTool`）：
```rust
pub struct WaitAllSubagentsTool;
#[async_trait]
impl Tool for WaitAllSubagentsTool {
    fn name(&self) -> &str { "wait_all_subagents" }
    fn description(&self) -> &str {
        "并发等待多个已派遣子代理到达终态，一次性返回全部 SubAgentResult。Args: {run_ids: [string]}."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type":"object","properties":{
            "run_ids": { "type":"array", "items": { "type":"string" } } },"required":["run_ids"] })
    }
    fn timeout_stage(&self) -> crate::tool::TimeoutStage { crate::tool::TimeoutStage::Watchdog }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        gate(ctx)?;
        let o = orch(ctx)?;
        let ids: Vec<String> = args.get("run_ids").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let results = o.wait_many(&ids).await;
        let arr: Vec<Value> = results.into_iter().map(|r| match r {
            Ok(res) => serde_json::to_value(res).unwrap_or(json!({})),
            Err(e) => json!({ "error": e.to_string() }),
        }).collect();
        Ok(json!({ "results": arr }))
    }
}
```
`ORCH_TOOL_NAMES`（L21-L29）`[&str; 7]` → `[&str; 8]`，追加 `"wait_all_subagents"`；`register_orchestration_tools`（L32-L37）加 `registry.register(Arc::new(WaitAllSubagentsTool));`。`src/agent/llm_agent.rs` 的 `ALL_ORCH`（L466-L474）同步 `[&str; 7]`→`[&str; 8]` 追加 `"wait_all_subagents"`。

- [ ] **Step 4: 运行验证通过 + 提交**

Run: `cargo test --lib wait_all_is_registered_and_delivered`（PASS）；`cargo build`（两数组长度一致，无越界）
```powershell
git add src/tool/orchestration.rs src/agent/llm_agent.rs ; git commit -m "feat(orch): 接入 wait_all_subagents 工具"
```

---

## Task 6: 角色唯一校验 + 发射描述改（拆 F7 雷）

**Files:**
- Modify: `src/agent/orchestration.rs`（`spawn` L459 之后、限流 L465 之前插入角色唯一校验）
- Modify: `src/tool/orchestration.rs`（`SpawnSubagentTool::description` 与 `execute` L79-L101）

**Interfaces:**
- Consumes: `self.children`（handle 的 `role` + `status`）、`SubAgentSpec.role`
- Produces: 同运行内非终值重名角色 → 返回明确错误（不静默复用）

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn spawn_rejects_duplicate_nonterminal_role() {
    let (orch, _rx) = make_test_orchestrator();
    let _ = orch.spawn(read_only_spec("same-role"), 0, "inv", "s", "au").await.unwrap();
    // 同名角色仍在飞行 → 第二次应被拒（而非 try_reuse 误命中或并行歧义）
    let err = orch.spawn(read_only_spec("same-role"), 0, "inv", "s", "au").await;
    assert!(err.is_err(), "非终值重名角色必须被拒");
    assert!(err.unwrap_err().to_string().contains("duplicate role"));
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib spawn_rejects_duplicate_nonterminal_role`
Expected: FAIL（当前无该拒绝逻辑，或 `try_reuse` 对未终值不命中后直接并行派遣）。

- [ ] **Step 3: 插入角色唯一校验**

`spawn`（L459 `try_reuse` 之后、并发限流块之前）加：
```rust
// 角色唯一（D-E / F7）：非终值同角色拒绝，避免误复用与聚合审计歧义。
{
    let map = self.children.lock().unwrap();
    if map.values().any(|h| h.role == spec.role && {
        let s = h.status.lock().unwrap();
        matches!(*s, SubAgentStatus::Pending | SubAgentStatus::Running)
    }) {
        return Err(AgentError::agent(format!(
            "spawn_subagent: duplicate role '{}'; use a unique role per worker", spec.role)));
    }
}
```
`SpawnSubagentTool::description` 改为引导一轮连发只读 worker + 角色唯一：
```rust
fn description(&self) -> &str {
    "派遣一个子 Agent（fire-and-forget，返回 run_id）。复杂多目标任务：一轮内连续多次调用派遣多个【只读】worker（每个 role 必须唯一），随后用 wait_all_subagents 一次收齐。写/执行需 allow_write/allow_exec 并单独授权，且串行执行。"
}
```

- [ ] **Step 4: 运行验证通过 + 提交**

Run: `cargo test --lib spawn_rejects_duplicate_nonterminal_role`（PASS）
```powershell
git add src/agent/orchestration.rs src/tool/orchestration.rs ; git commit -m "feat(orch): 角色唯一校验 + 发射描述引导一轮连发"
```

---

## Task 7: 扇出提示词引导（软提示，替代被拆的硬门）

**Files:**
- Modify: `src/agent/llm_agent.rs`（扇出提示段 L1041-L1045；预筛命中的软提示注入）

**Interfaces:**
- Consumes: `orchestration_prefilter(user_message)`（现仅用于生成提示）
- Produces: system prompt 里一条「本任务适合并行扇出」软引导（非硬门）

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn fanout_hint_present_when_prefilter_hits() {
    // 预筛命中 → 提示串包含扇出引导；未命中 → 不含（软提示，不改工具交付）
    assert!(fanout_prompt_hint("分别检查这3台主机 1.1.1.1 2.2.2.2 3.3.3.3").contains("并行"));
    assert!(!fanout_prompt_hint("你好").contains("并行"));
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib fanout_prompt_hint`
Expected: FAIL（辅助函数未定义）。

- [ ] **Step 3: 新增软提示函数 + 注入**

`llm_agent.rs` 新增（复用预筛信号）：
```rust
/// 预筛命中时给模型一条并行扇出的软引导；不决定工具交付（交付已常态开启）。
pub fn fanout_prompt_hint(user_message: &str) -> String {
    if orchestration_prefilter(user_message) {
        "检测到多目标/多数据源信号：优先用 spawn_subagent 一轮连发多个只读 worker，再 wait_all_subagents 收齐。".to_string()
    } else { String::new() }
}
```
在装配 system prompt 处（L1041-L1045 扇出提示区）于 `Instant && depth==0` 时追加 `fanout_prompt_hint(user_message)`。

- [ ] **Step 4: 运行验证通过 + 提交**

Run: `cargo test --lib fanout_prompt_hint`（PASS）
```powershell
git add src/agent/llm_agent.rs ; git commit -m "feat(orch): 预筛降级为并行扇出软提示"
```

---

## Task 8: 回归翻转 + 端到端冒烟 + 灰度/回滚验证

**Files:**
- Modify: `src/agent/llm_agent.rs`（L4409 附近既有交付/门控测试按新语义核对）
- Test: 端到端冒烟（集成，不新增测试文件则写入现有集成测试处）

**Interfaces:**
- Consumes: 前 7 个 Task 全部产出
- Produces: 并行路径绿灯 + 回滚验证记录

- [ ] **Step 1: 核对既有编排测试语义一致**

检查 L4409-L4418 断言：`orchestration_delivered(name,&empty)==false`（allowset 空仍隐藏，成立）、`orchestration_allowset(Instant,0)==ALL_ORCH.len()`（**更新为 8**）。运行：
Run: `cargo test --lib orchestration`
Expected: 若因 `ALL_ORCH` 变 8 而硬编码 7 处失败，改为 `ALL_ORCH.len()` 动态断言后重跑。

- [ ] **Step 2: 端到端并行冒烟**

```rust
#[tokio::test]
async fn instant_fanout_runs_workers_concurrently() {
    let (orch, _rx) = make_test_orchestrator();
    let t0 = std::time::Instant::now();
    let ids = vec![
        orch.spawn(read_only_spec("a"), 0, "inv", "s", "au").await.unwrap(),
        orch.spawn(read_only_spec("b"), 0, "inv", "s", "au").await.unwrap(),
    ];
    let res = orch.wait_many(&ids).await;
    assert_eq!(res.len(), 2);
    // 并行：总耗时应≈最慢单 worker，而非两者之和（放宽阈值断言 < 串行上界）
    assert!(t0.elapsed() < std::time::Duration::from_secs(2 * PER_WORKER_TEST_SECS));
}
```
Run: `cargo test --lib instant_fanout_runs_workers_concurrently`
Expected: PASS。

- [ ] **Step 3: 全量测试 + 构建**

Run: `cargo build ; cargo test --lib`
Expected: 全绿；无 unused 警告回归。

- [ ] **Step 4: 一级回滚演练（可回滚性验证）**

临时把 L1720-L1723 恢复 `&& prefilter`，运行 `cargo test --lib orch_candidate_is_steady_for_instant_root_regardless_of_prefilter`
Expected: 该新测试转为红（证明回滚门有效），其余编排测试仍绿。确认回滚后**撤销**本次演练改动，恢复常态并行开关。

- [ ] **Step 5: 提交 + 记录灰度门槛**

```powershell
git add -A ; git commit -m "test(orch): 并行扇出回归与冒烟 + 回滚演练"
```
按 spec §10 测量方法上灰度：同一多目标取证任务 `N≥3`、串行/并行各 ≥3 次取端到端墙钟中位数，并行下降 ≥30% 且回收错误=0、无半写/证据损失、Expert 无回归 → 判定收益为正，保留；任一不达标 → 触发一级回滚（Step 4 那一行）。

---

## 自审（对照 spec）

- **spec 覆盖**：D-A 常态投递+事件泵懒起 → Task 2+1；D-B 仅只读并行（写串行沿用现状，无需改）→ Task 6 描述约束；D-C `wait_all`+看门狗豁免+防丢通知 → Task 3/4/5；D-E 角色唯一 → Task 6；D-F 开关/并行度（保持 4，无改动）→ Global Constraints；D-G 前端事件照发（现状已发，未动）→ 无需任务；§9 回滚 → Task 2 回滚点 + Task 8 Step 4；§10 收益门槛 → Task 8 Step 5。
- **占位扫描**：各步均给真实函数名/字段/行号/命令；`make_test_orchestrator` / `read_only_spec` / `PER_WORKER_TEST_SECS` 为测试夹具，Task 1 Step 1 起复用，执行者在测试模块内以现有编排测试的同型构造实现（若不存在则在该 Task 首个测试步骤补夹具，属测试脚手架）。
- **类型一致**：`wait_many(&[String]) -> Vec<AgentResult<SubAgentResult>>`（Task 4 定义、Task 5 调用一致）；`TimeoutStage::Watchdog`（Task 3/5 一致）；`pump_started`/`pending_reg_rx`（Task 1 定义、Step 4 使用一致）；`ALL_ORCH`/`ORCH_TOOL_NAMES` 同步扩为 8（Task 5）。

> 修正项：若执行 Task 8 Step 1 发现 `ALL_ORCH` 由 7→8 波及更多硬编码测试断言，统一改为 `ALL_ORCH.len()` 动态断言，勿逐处写死 7。
