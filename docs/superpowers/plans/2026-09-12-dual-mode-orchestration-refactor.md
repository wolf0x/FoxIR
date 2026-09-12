# 双模式编排重构 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把子 Agent 编排能力从 Expert 模式剥离并迁移到 Instant 模式，Expert 回归纯串行 durable loop，Instant 获得「规则预筛门控的并行编排」且简单任务零额外开销。

**Architecture:** 全局只保留一套编排运行时（`Orchestrator`，模式无关），门控方向由 `Expert&&depth0` 翻转为 `Instant&&depth0`；剥离 Expert 的计划声明式并行收集（`parallel.rs`/`parallel_subtasks`）；Instant 新增「规则预筛 → 命中才投递编排工具 + 构造 Orchestrator → LLM 拆解判断」的路由门；编排配置从 `modes.expert.*` 迁至 `modes.instant.*` 并兼容旧键。

**Tech Stack:** Rust 2021、tokio、serde、rusqlite；测试用内置 `#[cfg(test)]` 单元测试 + `tests/` 集成套件（`cargo test`）。

**Spec:**
- 加固版（本计划主要依据）：[docs/superpowers/specs/2026-09-12-dual-mode-orchestration-refactor-design.md](file:///e:/VSCode/FoxIR/docs/superpowers/specs/2026-09-12-dual-mode-orchestration-refactor-design.md)
- 基线 v1.0：[docs/specs/dual-mode-orchestration-refactor-spec.md](file:///e:/VSCode/FoxIR/docs/specs/dual-mode-orchestration-refactor-spec.md)

## Global Constraints

- **双 target 同步**：`src/lib.rs` 与 `src/main.rs` 各自声明同一模块树；增删 `mod` 声明必须同步两处（如 `orch_selftest` 见 [lib.rs#L30](file:///e:/VSCode/FoxIR/src/lib.rs#L30) 与 [main.rs#L30](file:///e:/VSCode/FoxIR/src/main.rs#L30)）。
- **每任务收口**：先 `cargo build` 绿，再跑相关 `cargo test` 绿，然后 commit；不允许留下编译不过的中间态提交。
- **worker 不变量**：`depth=1`、`can_spawn=false`、`.without_skills()`；worker `ctx.mode` 保持 `Expert`（加固版 D8′，**不翻转**）。
- **取消纪律不回退**：`cancelled_fut` + `tokio::select!` 竞速必须保留（v1.0 §3.6）。
- **编排默认值现状**：`default_expert_orchestration_state()="on"`、`default_orchestration_state()="off"`、`default_zero_u8()=0`（[config.rs#L361-L366](file:///e:/VSCode/FoxIR/src/config.rs#L361-L366)）。
- **零新依赖**：复用现有基建，不新建代码级 capability router（v1.0 §2.2）。
- **提交信息**：`refactor:` / `test:` / `feat:` 前缀，一任务一提交（或按步骤细粒度提交）。
- **行号会漂移**：计划中的行号为编写时快照；实施时以**符号/代码内容锚定**定位，不得依赖绝对行号（前序任务的增删会使后续行号偏移）。

---

## File Structure（改动地图）

| 文件 | 责任 | 阶段 |
|---|---|---|
| [src/managed/runner.rs](file:///e:/VSCode/FoxIR/src/managed/runner.rs) | 删并行收集链路（`ParallelOutcome`/`bounded_parallel_collect`/L876-L1078 分支） | P1 |
| [src/managed/manager.rs](file:///e:/VSCode/FoxIR/src/managed/manager.rs) | 删 `parallel_subtasks` 字段/解析/`ParallelSubtask` 结构 + 测试 | P1 |
| [src/managed/parallel.rs](file:///e:/VSCode/FoxIR/src/managed/parallel.rs) | 整文件删除 | P1 |
| [src/managed/mod.rs](file:///e:/VSCode/FoxIR/src/managed/mod.rs) | 删 `pub mod parallel;`（L62） | P1 |
| [src/orch_selftest.rs](file:///e:/VSCode/FoxIR/src/orch_selftest.rs) | 整文件删除 + 删两处 mod 声明 | P1 |
| [src/phase0_acceptance.rs](file:///e:/VSCode/FoxIR/src/phase0_acceptance.rs) | 删 3 个模板/loop/plan-sync 测试 | P1 |
| [src/managed/task_contract.rs](file:///e:/VSCode/FoxIR/src/managed/task_contract.rs) | 删 `parallel_subtasks` 持久化 + 测试（L718-L722） | P1 |
| [src/config.rs](file:///e:/VSCode/FoxIR/src/config.rs) | 删 `orchestration_template`/`OrchestrationTemplate`（H7）；`InstantModeConfig` 扩字段 + `From` impl（D11） | P1/P3 |
| [src/agent/llm_agent.rs](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs) | 翻转 `orchestration_allowset`（L264）+ 构造门（L1400）+ 投递门接线（L1308）+ 预筛（D10）+ 测试翻转 | P2/P5 |
| [src/main.rs](file:///e:/VSCode/FoxIR/src/main.rs) | `can_spawn`/`mode` 解耦（L540/L609-613）+ `OrchestrationLimits` 源迁移（L761） | P2/P3 |
| [src/runner.rs](file:///e:/VSCode/FoxIR/src/runner.rs) | 复核 Instant 根 ctx 的 `can_spawn`/`mode` 盖章（L281-282） | P2 |
| [src/tool/orchestration.rs](file:///e:/VSCode/FoxIR/src/tool/orchestration.rs) | 写/执行 worker 授权闸（L78-87） | P4 |
| [src/context.rs](file:///e:/VSCode/FoxIR/src/context.rs) | 更新过时注释（L609-610）；worker mode 保持 Expert（**不改** L410） | P2 |

---

## P1 · Expert 剥离（并行机器移除）

> 删除顺序按「先消费者后生产者」，保证每步 `cargo build` 绿。crate 级 `#![allow(dead_code)]`（[lib.rs#L7](file:///e:/VSCode/FoxIR/src/lib.rs#L7)）容忍中间态的未用代码。

> **【预检裁定 R1】Task 2 与 Task 3 执行顺序对调**：先执行原 Task 3（删 `parallel.rs`/`orch_selftest.rs`/`phase0_acceptance` 3 测试/`task_contract` 持久化 + 双 target mod 声明），再执行原 Task 2（删 `manager.rs` 的 `parallel_subtasks`/`ParallelSubtask`）。原因：`orch_selftest.rs`（lib target）与 `phase0_acceptance.rs` 均 `use crate::managed::manager::ParallelSubtask`，若先删该类型会使两个 target 编译失败。下文任务编号保持不变，但**实际执行序为 Task 1 → Task 3 → Task 2 → Task 4**。

### Task 1: 移除 runner.rs 并行收集链路

**Files:**
- Modify: [src/managed/runner.rs](file:///e:/VSCode/FoxIR/src/managed/runner.rs) — 删 `ParallelOutcome` 枚举（L206-L218）、`bounded_parallel_collect`（L221-L237）、并行分支（L876-L1078，含 `parl` 构造 L897、两处 `bounded_parallel_collect` 调用 L942/L982、`audit_subagent_aggregate` 调用 L968、`ParallelOutcome::*` 匹配臂）

**Interfaces:**
- Consumes: `ManagerPlan`（Task 2 前的旧结构仍含 `parallel_subtasks`，本任务只删使用点）
- Produces: 一个不含任何并行收集调用的串行 round 循环；`bounded_parallel_collect`/`ParallelOutcome` 符号消失

- [ ] **Step 1: 定位并删除并行分支**

删除 L876-L1078 中依赖 `plan.parallel_subtasks` 的整段（`if !plan.parallel_subtasks.is_empty() { ... }` 及 `parl`/`bounded_parallel_collect`/降级重跑/`audit_subagent_aggregate`），保留 `brief` 的其余构造与串行 Executor 派发。

- [ ] **Step 2: 删除 `ParallelOutcome` 与 `bounded_parallel_collect`**

删除 L206-L237 的枚举与异步函数定义（Step 1 后已无调用点）。

- [ ] **Step 3: 验证编译**

Run: `cargo build`
Expected: 成功；若报 `parallel_subtasks` 未使用/`unused import`，记录留待 Task 2/3 清理，不得留 `error`。

- [ ] **Step 4: 验证 Expert 串行回归测试**

Run: `cargo test --lib managed::runner`
Expected: PASS（无并行相关断言残留）

- [ ] **Step 5: Commit**

```bash
git add src/managed/runner.rs
git commit -m "refactor: strip parallel collect chain from managed runner (P1)"
```

### Task 2: 移除 manager.rs 的 parallel_subtasks

**Files:**
- Modify: [src/managed/manager.rs](file:///e:/VSCode/FoxIR/src/managed/manager.rs) — 删 `ManagerPlan.parallel_subtasks`（L41-L44）、`PendingPlan.parallel_subtasks`（L86-L87）及 clone 链（L99/L112）、`parse_manager_plan` 内 `parallel_subtasks` 局部变量与 "Parallel Subtasks" 段解析（L387/L442-L446/L481）、`ParallelSubtask` 结构（L51-L52+）、3 个测试（L637 断言、`parse_plan_parallel_subtasks_section` L640-L650、PendingPlan 往返 L656-L661）

**Interfaces:**
- Consumes: 无（Task 1 已移除全部使用点）
- Produces: 不含 `parallel_subtasks`/`ParallelSubtask` 的 `ManagerPlan`/`PendingPlan`；`parse_manager_plan` 对 "Parallel Subtasks:" 段直接忽略

- [ ] **Step 1: 删除字段与解析**

删除 `ManagerPlan.parallel_subtasks`、`PendingPlan.parallel_subtasks`、`ParallelSubtask` 结构、`parse_manager_plan` 中相关局部变量/段解析/结构体初始化字段，以及 `to_plan`/clone 链中的对应行。

- [ ] **Step 2: 删除/改写受影响测试**

删除 `parse_plan_parallel_subtasks_section`（L640-L650）；从 `parse_plan_*`（L637）与 PendingPlan 往返测试（L656-L661）中移除 `parallel_subtasks` 断言。

- [ ] **Step 3: 验证编译 + 测试**

Run: `cargo build; cargo test --lib managed::manager`
Expected: build 成功；manager 测试 PASS

- [ ] **Step 4: Commit**

```bash
git add src/managed/manager.rs
git commit -m "refactor: remove parallel_subtasks from ManagerPlan/PendingPlan (P1)"
```

### Task 3: 删除 parallel.rs / orch_selftest.rs 及连带测试

**Files:**
- Delete: [src/managed/parallel.rs](file:///e:/VSCode/FoxIR/src/managed/parallel.rs)
- Modify: [src/managed/mod.rs](file:///e:/VSCode/FoxIR/src/managed/mod.rs) — 删 `pub mod parallel;`（L62）
- Delete: [src/orch_selftest.rs](file:///e:/VSCode/FoxIR/src/orch_selftest.rs)
- Modify: [src/lib.rs](file:///e:/VSCode/FoxIR/src/lib.rs) — 删 `pub mod orch_selftest;`（L30）
- Modify: [src/main.rs](file:///e:/VSCode/FoxIR/src/main.rs) — 删 `#[allow(dead_code)] mod orch_selftest;`（L29-L30）
- Modify: [src/phase0_acceptance.rs](file:///e:/VSCode/FoxIR/src/phase0_acceptance.rs) — 删 `phase0_template_collect_dispatches_sequential_and_parallel`（L1070）、`phase0_run_loop_collect_produces_bounded_unique_rounds`（L1122）、`phase0_plan_bidirectional_sync_seed_and_persist`（L1170，含 L1175-L1178 的 `parallel_subtasks` seed）
- Modify: [src/managed/task_contract.rs](file:///e:/VSCode/FoxIR/src/managed/task_contract.rs) — 删 `orchestrator_plan` 中 `parallel_subtasks` 持久化 + 测试（L718-L722）

**Interfaces:**
- Consumes: 无
- Produces: 代码库中不再存在 `run_template_collect`/`run_loop_collect`/`ParallelEnv`/`orch_selftest`

- [ ] **Step 1: 删除 parallel.rs 与 mod 声明**

删除文件 `src/managed/parallel.rs`；在 `src/managed/mod.rs` 删除 L62 `pub mod parallel;`。

- [ ] **Step 2: 删除 orch_selftest.rs 与两处 mod 声明（双 target 同步）**

删除文件 `src/orch_selftest.rs`；在 `src/lib.rs` 删 L30、在 `src/main.rs` 删 L29-L30。

- [ ] **Step 3: 删除 phase0_acceptance 的 3 个测试 + task_contract 测试**

删除 Step 中列出的 3 个 `phase0_*` 测试函数；在 `task_contract.rs` 删除 L718-L722 中 `parallel_subtasks` 相关断言/seed（保留 `orchestrator_plan` 其余往返测试）。

- [ ] **Step 4: 验证编译 + 全量测试**

Run: `cargo build; cargo test`
Expected: build 成功；无对已删符号的引用错误；测试套件 PASS

- [ ] **Step 5: Commit**

```bash
git add -A src/managed/parallel.rs src/managed/mod.rs src/orch_selftest.rs src/lib.rs src/main.rs src/phase0_acceptance.rs src/managed/task_contract.rs
git commit -m "refactor: delete parallel.rs/orch_selftest and dependent tests (P1)"
```

### Task 4: 移除 config.rs 的 orchestration_template 死代码（H7）

**Files:**
- Modify: [src/config.rs](file:///e:/VSCode/FoxIR/src/config.rs) — 删 `ExpertModeConfig.orchestration_template`（L193-L195）、`OrchestrationTemplate` 枚举（L206-L230）、`default_orchestration_template`（L230）、`OrchestrationLimits.template`（L241-L242）及 `From`/`Default` impl 中的 `template` 赋值（L245-L263）

**Interfaces:**
- Consumes: 无（Task 1 已删唯一消费者 `bounded_parallel_collect`）
- Produces: 不含 `template` 字段的 `OrchestrationLimits`

- [ ] **Step 1: 删除模板相关定义**

删除 `orchestration_template` 字段、`OrchestrationTemplate` 枚举及其 `impl`、`default_orchestration_template`、`OrchestrationLimits.template` 字段与所有 `template:` 初始化。

- [ ] **Step 2: 验证编译 + 配置测试**

Run: `cargo build; cargo test --lib config`
Expected: build 成功；config 测试 PASS

- [ ] **Step 3: Commit**

```bash
git add src/config.rs
git commit -m "refactor: remove dead orchestration_template config (P1/H7)"
```

---

## P2 · 门控方向翻转（Expert → Instant）

### Task 5: 翻转 allowset + 构造门 + 全部门控测试

**Files:**
- Modify: [src/agent/llm_agent.rs](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs) — `orchestration_allowset`（L264-L272）、编排器构造门（L1399-L1402）、测试 `gate_gate_truth_open_and_closed`（L3405-L3420）、`gate_open_delivers_all_after_step2a`（L3424-L3433）、`gate_instrument_delivery_reads_ctx_not_agent_static_mode`（L3468-L3485）
- Modify: [tests/step1_gates.rs](file:///e:/VSCode/FoxIR/tests/step1_gates.rs) — `g_instant_tools_is_empty`（L14-L18）、`g_expert_root_full_allowset`（L27-L31）、`g_name_disjoint_all_tools_delivered`（L33-L40）

**Interfaces:**
- Produces: `orchestration_allowset(Instant,0)==ALL_ORCH`；`orchestration_allowset(Expert,*)==空`；`orchestration_allowset(_,depth>=1)==空`。构造门条件变为 `ctx.can_spawn && ctx.mode==Instant && ctx.depth==0`。

- [ ] **Step 1: 先翻转测试断言（红）**

在 llm_agent.rs 三个 gate 测试与 tests/step1_gates.rs 中，把「Expert 根非空 / Instant 空」改为「Instant 根非空（`==ALL_ORCH.len()`）/ Expert 空」；`depth>=1` 两侧仍断言空。`gate_instrument_delivery_reads_ctx_not_agent_static_mode` 的叙事反转为：Instant ctx → 全量，Expert ctx → 空。

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib gate_ ; cargo test --test step1_gates`
Expected: FAIL（`orchestration_allowset` 仍 Expert 门控，新断言不成立）

- [ ] **Step 3: 翻转实现**

`orchestration_allowset` L267 改为 `if mode == crate::context::AgentMode::Instant && depth == 0`；构造门 L1400 改为 `&& ctx.mode == crate::context::AgentMode::Instant`。同步更新 L265-L266 注释。

- [ ] **Step 4: 运行验证通过**

Run: `cargo test --lib gate_ ; cargo test --test step1_gates`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/agent/llm_agent.rs tests/step1_gates.rs
git commit -m "refactor: flip orchestration gate Expert->Instant + tests (P2)"
```

---

## P5 · Instant 路由门（D2 + D9 + D10）

> D10 关键：**投递门与路由门合一**。预筛（零 LLM 开销）未命中 → allowset 空 + 不构造 Orchestrator，兑现验收 #2「简单任务字面零编排开销」。先落预筛再接线，确保 P3 打开 `can_spawn` 时投递已被门控。

### Task 6: 规则预筛纯函数 `orchestration_prefilter`（TDD）

**Files:**
- Modify: [src/agent/llm_agent.rs](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs) — 新增 `pub fn orchestration_prefilter(user_message: &str) -> bool` + `#[cfg(test)]` 单测

**Interfaces:**
- Produces: `orchestration_prefilter(&str) -> bool`——命中「多目标 / 多数据源 / 显式并行措辞」返回 true；保守，false 仅意味着走主 Loop。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn prefilter_simple_task_is_false() {
    assert!(!orchestration_prefilter("总结一下这个进程在做什么"));
}
#[test]
fn prefilter_parallel_wording_is_true() {
    assert!(orchestration_prefilter("分别检查这两台主机的持久化项"));
}
#[test]
fn prefilter_multi_ip_is_true() {
    assert!(orchestration_prefilter("扫描 10.0.0.5 和 10.0.0.6 的开放端口"));
}
#[test]
fn prefilter_many_sources_is_true() {
    assert!(orchestration_prefilter("把进程、服务、注册表和日志都拉一遍做时间线"));
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib prefilter_`
Expected: FAIL（`orchestration_prefilter` 未定义）

- [ ] **Step 3: 实现最小逻辑**

```rust
/// Cheap rule-based pre-filter (D2 layer 1 / D10). Zero LLM cost. Conservative:
/// a false negative only means "stay on the main loop", never a wrong fan-out.
pub fn orchestration_prefilter(user_message: &str) -> bool {
    use std::sync::OnceLock;
    static IP_RE: OnceLock<regex::Regex> = OnceLock::new();
    let lower = user_message.to_lowercase();
    const PARALLEL_WORDS: &[&str] =
        &["分别", "并行", "各自", "同时", "逐个", "respectively", "in parallel", "each of"];
    if PARALLEL_WORDS.iter().any(|w| lower.contains(w)) {
        return true;
    }
    let ip_re = IP_RE.get_or_init(|| regex::Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").unwrap());
    let ips: std::collections::HashSet<&str> =
        ip_re.find_iter(&lower).map(|m| m.as_str()).collect();
    if ips.len() >= 2 {
        return true;
    }
    const SOURCES: &[&str] = &[
        "进程", "服务", "注册表", "日志", "文件", "网络", "内存",
        "prefetch", "evtx", "pcap", "process", "service", "registry", "log",
    ];
    SOURCES.iter().filter(|s| lower.contains(**s)).count() >= 3
}
```

- [ ] **Step 4: 运行验证通过**

Run: `cargo test --lib prefilter_`
Expected: PASS（4 个用例全绿）

- [ ] **Step 5: Commit**

```bash
git add src/agent/llm_agent.rs
git commit -m "feat: add orchestration_prefilter rule-based router signal (P5/D10)"
```

### Task 7: 接线预筛 → 投递门 + 构造门（TDD）

**Files:**
- Modify: [src/agent/llm_agent.rs](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs) — 新增 `pub fn orchestration_delivered_for(mode, depth, candidate: bool) -> Vec<String>`；`run()` 入口算 `orch_candidate`；投递门 L1308 改用新 helper；构造门 L1399 追加 `&& orch_candidate`

**Interfaces:**
- Consumes: `orchestration_prefilter`（Task 6）、`orchestration_allowset`（Task 5）
- Produces: `orchestration_delivered_for(Instant,0,false)==空`、`(Instant,0,true)==ALL_ORCH`、`(Expert,0,true)==空`、`(_,1,_)==空`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn delivered_for_gates_on_candidate() {
    use crate::context::AgentMode::*;
    assert!(orchestration_delivered_for(Instant, 0, false).is_empty());
    assert_eq!(orchestration_delivered_for(Instant, 0, true).len(), ALL_ORCH.len());
    assert!(orchestration_delivered_for(Expert, 0, true).is_empty());
    assert!(orchestration_delivered_for(Instant, 1, true).is_empty());
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib delivered_for_`
Expected: FAIL（helper 未定义）

- [ ] **Step 3: 实现 helper + 接线**

```rust
/// D10: delivery gate AND route gate are one. Orchestration tools are delivered
/// only when the cheap pre-filter flags the run as a fan-out candidate.
pub fn orchestration_delivered_for(
    mode: crate::context::AgentMode, depth: u8, candidate: bool,
) -> Vec<String> {
    if candidate { orchestration_allowset(mode, depth) } else { Vec::new() }
}
```
在 `run()` 入口（L1301 之前）加：
```rust
let orch_candidate = ctx.mode == crate::context::AgentMode::Instant
    && ctx.depth == 0
    && orchestration_prefilter(user_message);
```
L1308 改为 `let orch_allowset = orchestration_delivered_for(ctx.mode, ctx.depth, orch_candidate);`；构造门 L1399-L1402 追加 `&& orch_candidate`。

- [ ] **Step 4: 运行验证通过 + 编译**

Run: `cargo test --lib delivered_for_ ; cargo build`
Expected: PASS + build 成功

- [ ] **Step 5: Commit**

```bash
git add src/agent/llm_agent.rs
git commit -m "feat: gate orchestration delivery+construction on prefilter (P5/D10)"
```

---

## P3 · Instant 编排接入 + D11 配置迁移

### Task 8: 编排配置迁至 `modes.instant.*` 并兼容旧键（D11，TDD）

**Files:**
- Modify: [src/config.rs](file:///e:/VSCode/FoxIR/src/config.rs) — `InstantModeConfig`（L316-L333）新增 `max_concurrent_subagents`/`default_timeout_secs`/`max_tokens_per_run`/`max_total_tokens`；`orchestration` 由 `String` 改 `Option<String>`（None=未设）；新增 `impl From<&InstantModeConfig> for OrchestrationLimits`；新增 `ModesConfig::orchestration_enabled()`
- Modify: [src/main.rs](file:///e:/VSCode/FoxIR/src/main.rs) — L540 改读 `config.agent.modes.orchestration_enabled()`；L761 改 `OrchestrationLimits::from(&config.agent.modes.instant)`

**Interfaces:**
- Produces: `OrchestrationLimits::from(&InstantModeConfig)`；`ModesConfig::orchestration_enabled() -> bool`（`instant.orchestration` 为 `Some` 则用它，`None` 则回退 `expert.orchestration`）

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn instant_limits_come_from_instant_config() {
    let mut instant = InstantModeConfig::default();
    instant.max_concurrent_subagents = 4;
    instant.default_timeout_secs = 90;
    let lim = OrchestrationLimits::from(&instant);
    assert_eq!(lim.max_concurrent_subagents, 4);
    assert_eq!(lim.default_timeout_secs, 90);
}
#[test]
fn orchestration_enabled_falls_back_to_expert_when_instant_unset() {
    let mut modes = ModesConfig::default();
    modes.instant.orchestration = None;            // 未显式设置
    modes.expert.orchestration = "on".to_string();  // 旧键
    assert!(modes.orchestration_enabled());
    modes.instant.orchestration = Some("off".to_string());
    assert!(!modes.orchestration_enabled());        // 显式 instant 优先
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib config`
Expected: FAIL（新字段/`From`/`orchestration_enabled` 未定义）

- [ ] **Step 3: 实现**

给 `InstantModeConfig` 加四个数值字段（复用现有 `default_max_concurrent_subagents`/`default_subagent_timeout_secs`/`default_max_tokens_per_run`/`default_max_tokens`）；`orchestration` 改 `#[serde(default)] pub orchestration: Option<String>` 并同步 `Default` impl；新增 `From<&InstantModeConfig> for OrchestrationLimits`（拷贝四值，无 `template`——已在 Task 4 删）；新增：
```rust
impl ModesConfig {
    /// D11: orchestration now belongs to Instant; fall back to the legacy
    /// `modes.expert.orchestration` key only when instant is unset.
    pub fn orchestration_enabled(&self) -> bool {
        match &self.instant.orchestration {
            Some(v) => v == "on",
            None => self.expert.orchestration == "on",
        }
    }
}
```
排查并修正 `InstantModeConfig.orchestration` 的现有读取点（String→Option 带来的类型变化）。

- [ ] **Step 4: 运行验证通过**

Run: `cargo test --lib config`
Expected: PASS

- [ ] **Step 5: 接线 main.rs**

L540 改 `let orchestration_enabled = config.agent.modes.orchestration_enabled();`；L761 改 `OrchestrationLimits::from(&config.agent.modes.instant)`。

- [ ] **Step 6: 编译 + 提交**

Run: `cargo build`
```bash
git add src/config.rs src/main.rs
git commit -m "refactor: migrate orchestration config to modes.instant with legacy fallback (P3/D11)"
```

### Task 9: main.rs `can_spawn`/`mode` 解耦 + 集成收口（H2）

**Files:**
- Modify: [src/main.rs](file:///e:/VSCode/FoxIR/src/main.rs) — 主 agent 静态 mode（L545-L548）恒 `Instant`；共享 runner（L609-L613）`.with_can_spawn(orchestration_enabled).with_mode(Instant)`
- Modify: [src/context.rs](file:///e:/VSCode/FoxIR/src/context.rs) — 更新过时注释 L609-L610（`mode == Expert` → `mode == Instant`）；**不改** L410 worker 硬编码（D8′）
- Verify: [src/runner.rs](file:///e:/VSCode/FoxIR/src/runner.rs) L281-L282（无需改，确认盖章 `self.can_spawn`/`self.mode`）

**Interfaces:**
- Consumes: `orchestration_enabled`（Task 8）、翻转后的门（Task 5）、预筛接线（Task 7）
- Produces: Instant 根 ctx = `mode:Instant` + `can_spawn:orchestration_enabled`；预筛命中时构造 Orchestrator

- [ ] **Step 1: 确认路由（验证项）**

读 [server.rs#L1788](file:///e:/VSCode/FoxIR/src/server.rs#L1788)（Expert → `managed_runner`）与 [server.rs#L1794-L1806](file:///e:/VSCode/FoxIR/src/server.rs#L1794-L1806)（Instant → `state.runner`），确认共享 runner 仅服务 Instant 分支。若成立，静态 mode 塔缩为 `Instant` 安全。

- [ ] **Step 2: 改 main.rs 解耦**

L545-L548 静态 agent mode 改为恒 `crate::context::AgentMode::Instant`；L609-L613 改为 `.with_can_spawn(orchestration_enabled)` + `.with_mode(crate::context::AgentMode::Instant)`。

- [ ] **Step 3: 更新注释**

[context.rs#L609-L610](file:///e:/VSCode/FoxIR/src/context.rs#L609-L610) 注释改为 "Gated at runtime by `mode == Instant && depth == 0`"。

- [ ] **Step 4: 全量验证（验收 #8）**

Run: `cargo build --release; cargo test`
Expected: release 编译成功；全部测试（gate_/prefilter_/delivered_for_/config/managed）PASS

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/context.rs
git commit -m "refactor: decouple can_spawn/mode from Expert; Instant root orchestrates (P2/P3/H2)"
```

---

## Plan 1 范围与后续（Plan 2）

**本计划（Plan 1 = Task 1–9）交付一个可独立编译/测试的完整增量**：Expert 纯串行、门控翻转、Instant 编排接入、配置迁移、路由门 + 零开销投递。

**以下归 Plan 2（后续）**，因各自需要专门的上下文探查，强行写入会引入占位符：
- **P4 写/执行 worker 授权闸**：需先探查交互式权限请求流（`PendingMap` 往返 + `check_preauthorization`）才能具体化。**安全性说明**：延后不留漏洞——worker 每次写/执行工具调用仍过现有权限系统（[orchestration.rs#L343-L362](file:///e:/VSCode/FoxIR/src/agent/orchestration.rs#L343-L362) 三档权限 + `write_gate` 串行），P4 只是把授权提前到 spawn 时。
- **P6 能力路由提示词**（v1.0 §6.5）：提示词两层决策段，属文本评审类任务。
- **P7 取消纪律孤儿修复**（v1.0 §7.2）：`SubAgentHandle.cancel`（[orchestration.rs#L530](file:///e:/VSCode/FoxIR/src/agent/orchestration.rs#L530)）+ `root_ended` 联动；取消竞速本身随 Orchestrator 整体迁移已继承，孤儿修复为增强项。
- **P8 记忆/文档更新**（v1.0 §12）：废止旧「Instant 禁止编排」决策、翻转门控规范记忆。

---

## Self-Review（writing-plans 自检结果）

- **Spec 覆盖**：加固版 §2.1(H2)→Task 9；§2.2(D8′)→Global Constraints+Task 9 Step 3（不改 L410）；§2.3(D10)→Task 6/7；§2.4(D11)→Task 8；§3 P1→Task 1–4；P2→Task 5/9；P3→Task 8/9；P5→Task 6/7。P4/P6/P7/P8 显式归 Plan 2 并说明理由——**无遗漏**。
- **占位符扫描**：无 TBD/TODO；新行为任务均附真实 Rust 测试+实现代码；删除任务附精确行号目标 + `cargo build`/`cargo test` 验证。
- **类型一致性**：`orchestration_prefilter(&str)->bool`、`orchestration_delivered_for(mode,depth,bool)->Vec<String>`、`OrchestrationLimits::from(&InstantModeConfig)`、`ModesConfig::orchestration_enabled()->bool` 在定义与使用处签名一致；`orchestration_allowset` 签名不变（仅改门控方向）。

---

## Execution Handoff

Plan 1 完成。两种执行方式：
1. **Subagent-Driven（推荐）**——每任务派发独立子代理 + 两阶段评审（先 spec 合规、后代码质量）。
2. **Inline Execution**——当前会话按批执行 + 检查点。
