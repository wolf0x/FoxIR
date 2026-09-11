# Log · 多代理编排（multi-agent-orchestration）

> `log.md` 实时记录本 change 的关键变更、验证结果与新增知识；只记录事实与证据。

## 2026-09-11 · 接入 v1.5 spec

- **事件**：将 v1.5 版《FoxIR 多代理编排 · 软件设计文档（SDD）》接入本 change 目录，作为 `spec.md`。
- **来源**：`C:/Users/BUWO/Downloads/deepseek_markdown_20260911_074023.md`（82,499 字节）。
- **版本演进**：仓库 `docs/SDD_multi_agent_orchestration.md` 为 v1.0（待评审）；本次接入为 v1.5（待评审），原 v1.0 文件保留未动。
- **任务拆解**：基于 spec §13 生成 `tasks.md`，分为 P0（评审基线）、P1（Step 1 骨架）、P2（Step 2a Phase 0）、P3（Step 2b Phase 1）、P4（Step 3 Phase 2）。
- **隐含规则/知识**：
  - 委派能力仅两态：`off`（Instant 恒定）与 `full`（Expert）。
  - 子代理 = 单 `LlmAgent`，无 Manager / Auditor / TaskContract；审计收敛到 Manager 层。
  - 默认无 Skill 机制（§20.3 B4）：`.without_skills()` 为主 + `SkillListingStrategy::Disabled` 为辅。
- **状态**：spec 待评审；tasks 为提案态，尚未进入 Apply。
- **下一步建议**：执行 `$spec-review` 核对 v1.5 spec 落地前提，或 `$spec-propose` 收敛待澄清项后进入 `$spec-apply`。

---

## 2026-09-11 · 收尾回顾审查 + G-instant 门控修复

- **事件**：对所有 spec 落地代码做阶段一（Spec Compliance）回顾审查，主线为「Expert 模式能否保存 long-horizon 专注力」。
- **审查结论（阶段一 FAIL → 本处修复其中一项）**：
  - **Critical 1（G-instant-tools / G-instant-diff 违约，已修复）**：`src/agent/llm_agent.rs` 投递门控 `orchestration_allowset(self.mode, self.depth)` 与 Orchestrator 创建条件 `self.mode==Expert` 误用 **agent 静态字段**（`main.rs` 在 orchestration 开启时把唯一共享 agent 建成 `mode=Expert`），而运行时 `InvocationContext.mode` 恒为 `Instant`。结果是**普通 Instant 聊天也会投递全部 7 个编排工具并实例化 Orchestrator**，破坏「Instant 零改动」承诺，也让 ManagedRunner 每轮 Executor 都意外成为编排根（双轨）。
  - **修复（llm_agent.rs，3 处）**：投递门控、`mode/depth` 平铺副本、Orchestrator 创建条件**全部改读运行时 `ctx.mode`/`ctx.depth`**；新增内嵌回归测试 `gate_instrument_delivery_reads_ctx_not_agent_static_mode` 锁定「投递门控读 ctx 而非 agent 静态值」。
  - **Critical 2（未擅自改动，标注待决策）**：`Runner::run` 从不设 `ctx.mode=Expert`（唯一 Expert 是 `subagent_child`，其 `can_spawn=false`），故 `ctx.mode==Expert && can_spawn` 组合在既有代码里从不成立——新的 Orchestrator 子代理能力实际是「未激活的孤儿代码」，与 ManagedRunner(TaskContract) 完全解耦。审查确认旧 ManagedRunner 的长期专注力（Manager 按 TaskContract 重规划、Executor fresh context）**未回归**；是否将 Orchestrator 全量接入 Manager 主循环属于大 scope 决策，本处不做，待定案。
- **验证**：`cargo test --bin FoxIR` → 264 passed / 1 环境性 `test_shimcache` fails（与基线一致，无关本改动）；`gate_*` 子集 10/10 通过含新增回归。
- **影响 task**：T1.5（投递期门控）修正为以 ctx 为准；T1.10（门禁）补回归；T2.5/T3.3（Manager 主循环接线）标注为待决策的安全范围外项（未接入）。

---

## 2026-09-11 · v1.5 符合性核对（现场代码 vs spec）

- **方式**：对照 spec §7.2/§7.3/§7.4/§7.8 与 §13 Step 1/2a，逐一核验 `src/agent/orchestration.rs`、`event_pump.rs`、`exclusivity.rs`、`src/tool/orchestration.rs`、`src/phase0_acceptance.rs` 及已改的 `context.rs`/`config.rs`/`skill/*`/`llm_agent.rs`/`tool/mod.rs`/`memory.rs`/`server.rs`。
- **总体**：Step 1 骨架与 Phase 0 主体已实现，但存在 1 处「模式相反」硬偏差与多个「未完整落地」项。结论：**不能判定为与 v1.5 完全一致**，建议先 `$spec-review` 收敛再进入 Apply。

### 与 spec 不一致 / 缺失（按严重度）
- [高] **子代理 mode 相反**：spec §7.4.1 明确 `child_ctx.mode = AgentMode::Expert`（worker 用 Expert 语义）；实现两处设为 `AgentMode::Instant`（`orchestration.rs` spawn + builder）。虽 `can_spawn=false` 已隔离开，但与设计相反。
- [高] **EventPump 未按设计实现**（§7.4.2）：无 per-worker channel(128) + FuturesUnordered 动态注册 + 分级背压；仅一个 `broadcast_subagent_result` 简单广播，事件被 run_worker 直连 `try_send` 转发。
- [高] **执行期门控只做一半**（§7.3）：仅 `spawn_subagent` 首行检查 `can_spawn`；其余 6 工具有 `get_orchestrator` 隔离兜底，但无 spec 要求的首行 `can_spawn`+`depth>=1` 双检查。
- [中] **编排工具 Scope 超前**：7 个工具做成全量实现，且把本属 Step 2b 的 `cancel/get_result/update_plan/read_log` 提前实现；跳过 Phase 0“壳工具、Expert allow=∅”阶段。
- [中] **无 timeout 包装**：spec §7.4.1/2.1 要求 `tokio::time::timeout`，`SubAgentStatus::Timeout` 现不可达。
- [中] **预算 per-run 未实现**（§7.7/2.7）：`token_usage` 恒 0，无 budget_map。
- [中] **无 Auditor 环节**（§7.5/2.5）：写终态后直接落，缺「先 Auditor、Pass 落盘/Fail 降级」。
- [低] **Exclusivity 接口偏离**（§7.4.3）：`acquire` 返回 `Arc<Mutex>` 而非 async `OwnedMutexGuard`。
- [低] **结构偏离**：无 spec 的 `AgentSpawner` trait / `LlmAgentSpawner`（改为 `Orchestrator::spawn` 直接实现）。
- [低] `skill_tool_names()` 用编译期静态数组，非 V.4 的「从 `build_meta_tools()` 动态求并集」（已含 `improve_skill`）。
- [低] `ToolRegistry::iter`/`register_arc` 缺失；Phase 0 测试内嵌 `src/phase0_acceptance.rs`（有效），无独立 `tests/step1_gates.rs`/`phase0_e2e.rs`/`mock_llm.rs`。

### 与 spec 一致（要点）
- `is_main_session` 排除 `sub-`/`cron-`；`ALL_ORCH`+`orchestration_allowset` 真值表门控（Instant=∅ / Expert0=7 / Expert>=1=∅）；`without_skills()`+风险 2 兼容；`Disabled` 三映射+早退；`subagent_results` v8 表+终态写入；kill-9 恢复不重 spawn（try_reuse）；skill 工具动态剔除 + 双关闭；Phase 0 四门禁验收脚本（wall-clock/token/20runs/kill-9）。

### 后续建议
1. `$spec-review` 或人工评审先定案（尤其子代理 mode、EventPump 是否本期必须按 §7.4.2 全量建）。
2. 若按 v1.5 严格推进：补 EventPump、timeout、预算 sink、Auditor 环节，收敛 7 工具为 Phase 0 壳工具范围。
3. 现状不可直接宣称“Step 2a 完成”；待澄清项收敛后再按 Apply 逐步推进。


---

## 2026-09-11 · spec-apply 紧急停车 → Reverse Sync

- **动作**：按 `spec-apply` skill 进入 Apply 阶段。入口条件核对：`spec.md`/`tasks.md`/`log.md` 齐全 ✅；已切分支 `feature/multi-agent-orchestration`（非 main）✅（T0.2 已完成）。
- **停车原因（紧急停车规则 2：当前代码与已确认 spec 明显冲突，且 P0 评审 T0.1 未收敛）**：
  - **[高] 子代理 mode 相反（已现场复核）**：spec §7.4.1 明确 `child_ctx.mode = AgentMode::Expert`（§:429）且 worker builder `.mode(AgentMode::Expert)`（§:459）；当前实现 `src/agent/orchestration.rs` 为 `child_ctx.mode = AgentMode::Instant`（:241）、`.mode(AgentMode::Instant)`（:270）。与设计相反。
  - **[高] EventPump 未按 §7.4.2 实现**：spec 要求 per-worker channel(cap=128) + `FuturesUnordered` 动态注册 + 分级背压；当前仅 `event_pump.rs::broadcast_subagent_result` 简单广播，由 run_worker 直连 `parent_tx.try_send`。
  - **[中] 预算 per-run（§7.7）未实现**：`run_worker` `token_usage` 恒 0（`orchestration.rs:384/424`），无 `budget_map` / per-run sink。
  - **[中] 无 timeout 包装（§7.4.1/2.1）**：`SubAgentStatus::Timeout` 不可达。
  - **[中] 无 Auditor 环节（§7.5/2.5）**：写终态后直接落，缺「先 Auditor、Pass 落盘/Fail 降级」。
  - **P0 评审（T0.1）未完成**：log 早前结论为「不能判定与 v1.5 完全一致，建议先 `$spec-review` 收敛再进入 Apply」。
- **受影响 task**：T0.1（评审）、T2.1（child mode）、T2.3（EventPump）、T2.5（Auditor）、T2.6（取消树）、T2.7（预算）、T2.8（Exclusivity 接口）。
- **建议回写/待决**（不擅自选择新方案）：
  1. 子代理 `mode` 按 §7.4.1 改为 `Expert`（改 `orchestration.rs:241` 与 `:270`），或由评审确认以 `Instant+can_spawn=false` 为准并回写 spec。
  2. 本期是否严格按 §7.4.2 建 per-worker channel + EventPump；否则回写为「简化广播」为已接受偏差。
  3. 需收敛 T0.1 后才可进入 Apply 逐 task 实施。


---

## 2026-09-11 · 待澄清项收敛（Q1/Q2 决策锁定）

- **[Q1] 子代理 mode → 用 Expert（对齐 spec §7.4.1）**：子代理仅在 Expert 模式被调度；`child_ctx.mode` 与 worker builder 均设为 `AgentMode::Expert`（改 `src/agent/orchestration.rs` :241 与 :270）。可用性不变：`can_spawn=false` 封死孙代理；worker depth>=1 使 `orchestration_allowset(Expert, 1)=∅`，不投递编排工具。
- **[Q2] EventPump 选 A（严格 §7.4.2），并采纳注册通道有界化改进（spec 增量，已接受）**：
  - per-worker `mpsc::channel(cap=128)` + 单一 `EventPump` task（`FuturesUnordered` 动态注册）+ 分级背压（TextDelta 可丢 / ToolCall·ToolResult·SubAgentResult `send().await`）。
  - **注册通道不用 spec 的 `UnboundedReceiver`**（无界=背压失效，与「单点背压」矛盾）；改 `mpsc::channel(cap = max_concurrent_subagents * 2)`，`spawn` 时 `send().await`，使注册一跳也有明确上界背压。
- **待办再确认**：timeout 包装（§7.4.1/2.1）、预算 per-run（§7.7）、Auditor 环节（§7.5）仍属待澄清；P0 评审（T0.1）尚未整体闭合。


---

## 2026-09-11 · Apply 任务 T2.1-子代理 mode：完成（§7.4.1 对齐）

- **改动**：`src/agent/orchestration.rs` — `child_ctx.mode = AgentMode::Expert`（:241）、worker builder `.mode(AgentMode::Expert)`（:270），并加 `debug_assert_eq!(child_ctx.mode, Expert)`（:243 后）守卫。
- **验证证据**：`cargo check --bin FoxIR` 通过；`cargo test --bin FoxIR phase0_` 4 项验收（wall-clock/token/20-runs/kill-9）全绿 —— worker 以 Expert 语义运行无回归。
- **门控推演（安全）**：worker `depth=1` ⇒ `orchestration_allowset(Expert,1)=∅` 不投递编排工具；`can_spawn=false` ⇒ 不派生孙代理；`run()` 建 Orchestrator 需 `depth==0` ⇒ worker 不会自建编排器。
- **状态**：本 task 完成，等待确认后进入下一项（EventPump §7.4.2 A 方案）。


---

## 2026-09-11 · Apply 任务 T2.3（EventPump §7.4.2 A）：完成

- **改动**：重写 `src/agent/event_pump.rs` —— `EventPump{reg_rx, ws_tx}` + `run()`（`FuturesUnordered` 动态汇聚，select 注册/事件），`forward()` 分级背压（TextDelta `try_send`，其它 `send().await`）；`broadcast_subagent_result` 改为写 worker 自有通道。
- **Orchestrator 接线**（`src/agent/orchestration.rs`）：`parent_tx` 字段 → 由 `new()` 起 `EventPump` task、存**有界注册通道** `reg_tx`（cap=max_concurrent*2）；`spawn()` 建 per-worker `channel(128)` 并 `reg.send(rx).await`；`run_worker` 参数 `parent_tx` → `worker_tx`（写自有通道，非阻塞）。
- **spec 增量落地**：注册通道为有界（非 Unbounded），注册一跳也有背压（Q2 已确认采纳）。
- **新增 env 字段**：`OrchestratorEnv.max_concurrent_subagents`（llm_agent 与 phase0 验收均设 8）。
- **验证证据**：`cargo check --bin FoxIR --tests` 通过；`cargo test --bin FoxIR phase0_` 4 项全绿 —— EventPump 转发 20-run 全程 60 条广播无丢失（进程 apply 了分级背压）；wall-clock 5.13× / token +0% / kill-9 不重 spawn 均保持；全量 254 通过（1 环境性 shimcache 失败）。
- **状态**：本 task 完成。

---

## 2026-09-11 · Apply 任务 T1.9（ToolRegistry `iter`/`register_arc`）：完成

- **改动**（`src/tool/mod.rs`）：
  - 新增 `pub fn register_arc(&mut self, tool: Arc<dyn Tool>)` 作为规范注册入口，`register` 改为委托它（spec §13 Step 1.9 / §21 附录C 第 9 行）。
  - 新增 `pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Tool>)>`，用于注册表自省与跨注册表拷贝。
  - `ToolError` 变体部分：本仓库错误类型为 `AgentError`，`not_available_in_mode` / `depth_limit` 已存在（`error.rs:162/166`），spec 的「若不存在则补」前提不成立，无需新增。
- **新增测试**：`tool::tests::iter_covers_registry_and_register_arc_inserts` —— 校验 `iter` 覆盖全部注册项、按名可取回且 `Arc::ptr_eq` 同源。
- **验证证据**：
  - `cargo test --bin FoxIR tool::tests` —— 3/3 通过。
  - `cargo test --bin FoxIR phase0_` —— 4/4 验收门禁全绿（wall-clock / token / 20-runs / kill-9）。
  - `cargo test --bin FoxIR` 全量 —— 255 通过，1 失败为既有环境性 `forensics::live_tests::test_shimcache`（与 T2.3 时基线一致，非本次回归）。
- **踩坑/发现（重要）**：
  - **工作树含未记录的 T2.7 进度**：编译时报 `orchestration.rs:468` `token_usage = total_tokens` 类型错误（`match &ev` 下 `total_tokens` 为 `&u64`）。`run_worker` 已带 `budgets: Arc<Mutex<HashMap<String, BudgetSnapshot>>>` 参数与 `Usage` 事件 token 追踪 —— 即预算 per-run（§7.7）已被某次未落 log 的会话实现，但 `tasks.md` 仍标「未实现」且代码无法编译。本次仅做最小修复（`*total_tokens` 解引用）以解锁验证，**未核验预算行为正确性**。
  - 已将 `tasks.md` T2.7 状态由 `[ ]` 调整为 `[>]`（代码存在、行为未核验），避免状态源与代码继续漂移。
- **状态**：T1.9 完成，等待确认。下一步候选（按依赖与待澄清）：
  1. 核验并闭合 T2.7（预算 sink 行为 + 补 log 溯源）；
  2. T2.1 剩余项：timeout 包装使 `SubAgentStatus::Timeout` 可达（log 中标记为待澄清）；
  3. T1.4 ToolContext 统一透传路径；T1.10 `tests/step1_gates.rs`。
  其中 timeout / Auditor（T2.5）在早前列为「待澄清」，进入前需用户定夺。

---

## 2026-09-11 · Apply 任务 T2.8（Exclusivity §7.4.3）：完成

- **改动**（已由前序会话部分落地，本轮补齐收尾）：
  - `src/agent/exclusivity.rs`：`acquire` 为 `async fn acquire(&self, name: &str) -> tokio::sync::OwnedMutexGuard<()>`（§7.4.3 精确对齐）——按工具名取/建 `Arc<tokio::sync::Mutex<()>>` 槽位并以 `lock_owned().await` 返回自有 guard。
  - `src/agent/llm_agent.rs:2690-2697`：工具执行前按 `tool.exclusivity()` 判 `Exclusive`，命中则 `global().acquire(tool_name).await` 并将 guard 存入 `_excl_guard` 跨整个工具执行期持有（drop 即释放锁槽）。`Exclusivity::Shared` 默认不触碰锁。
  - `src/tool/mod.rs:160`：`Tool::exclusivity()` 默认返回 `Shared`。
- **文件写入完整性修复（中断遗留）**：`LlmAgent` 结构体被前序会话加入了 `orchestration_limits` 字段（struct/builder/`new()` 均已写入），但 `build()` 的 struct 字面量漏写该字段，导致 `E0063: missing field` 无法编译。本轮在 `src/agent/llm_agent.rs` `build()` 补齐 `orchestration_limits: self.orchestration_limits`（字段当前无 setter、恒取 `default()`，保持与既有默认一致，属后续 §9 接线前置位）。
- **验证证据**：
  - `cargo check --bin FoxIR --tests` 通过（34 warnings，均为既有未用 import/变量，无本次引入）。
  - `cargo test --bin FoxIR phase0_` 4/4 验收门禁全绿：wall-clock 5.67x / token +0% / 20-run 60 条无丢失 / kill-9 不重 spawn（3 结果复用）。
- **状态**：本 task 完成。


---

## 2026-09-11 · Apply 任务 T2.1 剩余项（timeout 包装 §7.4.1/2.1）：完成

- **改动**：
  - `src/agent/orchestration.rs` `spawn`：以 `spec.timeout.unwrap_or(self.env.default_timeout_secs)` 作为单 worker 截止，`tokio::time::timeout` 包裹 `run_worker` future；超时分支把 handle 终态置为 `SubAgentStatus::Timeout`，写入 `SubAgentResult`、落盘（memory_store）、补 0 预算快照并 `notify_waiters`，使 `wait`/`get_subagent_result` 可解析。
  - `OrchestratorEnv` 新增 `default_timeout_secs: u64`；`llm_agent.rs` 从 `self.orchestration_limits.default_timeout_secs` 注入（顺带把 `max_concurrent_subagents` 改为读 `orchestration_limits`，不再硬编码 8）；`phase0_acceptance.rs` test env 补 `default_timeout_secs: 300`。
- **验证证据**：
  - 新增 `cargo test --bin FoxIR phase0_timeout_marks_worker_as_timeout`：high-latency mock（5s）+ `spec.timeout=Some(1)` ⇒ 状态 `Timeout`，且持久化行仍为 `Timeout`。通过。
  - `cargo test --bin FoxIR phase0_` 5/5 全绿（新增 timeout 项）：wall-clock 4.83x / token +0% / 20-run 60 条 / kill-9 不重 spawn / timeout。无回归。
- **状态**：T2.1 剩余项完成，T2.1 标记 [x]。


---

## 2026-09-11 · Apply 任务 T2.7（预算 per-run §7.7）：行为核验完成

- **背景**：`tasks.md` 记载「代码已含 budgets 但来源不明、行为未核验」。本轮补行为证据闭合。
- **改动**：无新增实现改动；对既有 `Orchestrator.budgets` / `run_worker` Usage 累加路径做专项测试。
- **验证证据**：新增 `cargo test --bin FoxIR phase0_budget_per_run_populated` —— 单 worker 跑完后 `orch.budget(run_id)` 返回 `BudgetSnapshot{total=120, prompt=100, completion=20}`（与 mock Usage 一致），且 `budgets()` 列表可查。通过。
- **状态**：T2.7 标记 [x]（行为已核验）。


---

## 2026-09-11 · Apply 任务 T2.6（三层取消树 §7.9 / 2.6）：行为核验完成

- **说明**：Phase 0 为 depth-1（worker 无孙代理），三层折叠为两层：`root_ended`（用户 STOP 级联全部 worker，经 `ctx.ended_flag`）+ 每 worker `SubAgentHandle.cancel`（`worker_ended_i` 等价物）。`cancel_subagent(run_id)`→`Orchestrator::cancel(run_id)` 置该 worker flag；`run_worker` 的消费循环 `handle.cancel || child_ctx.is_ended()` 早退并判 `Cancelled`。
- **验证证据**：新增 `cargo test --bin FoxIR phase0_cancel_worker_marks_cancelled` —— 长延迟 mock(5s) + 200ms 后 `cancel(run_id)` ⇒ `wait` 返回 `SubAgentStatus::Cancelled`。通过。
- **状态**：T2.6 标记 [x]。


---

## 2026-09-11 · Apply 任务 T3.1 / T3.2（Step 2b：全量编排起步）：完成

- **T3.1**：4 个编排工具（`cancel_subagent`/`get_subagent_result`/`update_plan`/`read_subagent_log`）已提前实现于 `src/tool/orchestration.rs`，标记 [x]。
- **T3.2 `allow_write`/`allow_exec` + 写/执行 worker 串行**：
  - `src/tool/mod.rs` 新增 `ToolRegistry::write_exec_names(allow_write, allow_exec)`：按 §7.10 layer 2 分档（allow_write→write/modify/delete；allow_exec→execute）。
  - `src/agent/orchestration.rs` `spawn`：`tools_allowlist` 空且 opt-in 时在只读子集上扩充 write/exec 工具；新增 `Orchestrator.write_gate: Arc<tokio::sync::Mutex<()>>`，`run_worker` 开头对写/执行 worker `write_gate.lock().await`（单写者串行），只读 worker 不触碰。
  - `src/tool/orchestration.rs` `spawn_subagent`：移除「Phase 0 拒绝写/执行」守卫（Step 2b 放行）。
- **验证证据**：
  - `cargo check --bin FoxIR --tests` 通过。
  - `cargo test --bin FoxIR tool::tests` 4/4：新增 `write_exec_names_admits_optin_tiers`（allow_write 只入 file_write 不入 shell_exec；allow_exec 反之；both 全入；none 为空）。
- **状态**：T3.1 / T3.2 标记 [x]。


---

## 2026-09-11 · Apply 任务 T3.4 / T3.5 部分（Step 2b）：完成

- **T3.4 工作流模板**：新增 `src/agent/workflow.rs`（`pub mod workflow` 于 `src/agent/mod.rs`）：
  - `WorkflowStep{role, prompt, max_iterations}` + `WorkflowStep::new`。
  - `sequential(orch, root, steps)`：顺序 await，fail-fast（非 Ok 即停）。
  - `parallel(orch, root, steps)`：全量 spawn 后 `join_all` 聚合。
  - `loop_until(orch, root, role, plan_prompt, max_rounds, stop)`：轮间 `update_plan` + `stop` 谓词早停。
  - 不引入新 Agent 类型（§10），仅复用 `Orchestrator::spawn/wait`。
- **T3.5 门禁**：
  - G2 新增 `phase0_g2_subagent_is_temporary_no_main_session`（worker 为临时执行单元、拦在 root 之下、`list()` 可查、无主会话线程）。
  - G-concurrency / G-resume 由 Phase 0 既有验收覆盖（20-run 60 条无丢失 / kill-9 不重 spawn）。
  - §20.7 Skill opt-in Review 清单：未触发（worker 维持无 Skill 双关闭）。
- **验证证据**：`cargo test --bin FoxIR phase0_workflow_sequential_then_parallel_then_loop` 通过（seq=3/par=3/loop=2）；`phase0_g2_*` 通过。
- **状态**：T3.4 [x]；T3.5 [>]（G2 落地、G-concurrency/G-resume 已有覆盖、§20.7 未触发）。
- **注**：`task_contracts.parent_task_id / depends_on` 扩展与 §7.5 Manager 主循环（Auditor 收敛点）耦合，留待全量 Manager 编排接线（T2.5/T3.3）时统一落实。


---

## 2026-09-11 · Apply 任务 T3.3 / T2.5（Auditor §7.5 确定性层）：完成

- **改动**（`src/agent/orchestration.rs`）：
  - 新增 `pub enum AuditVerdict { Pass, Fail(String), Uncertain(String) }`。
  - 新增 `pub fn audit_aggregate(results: &[SubAgentResult]) -> AuditVerdict`：确定性聚合审计（§7.5 默认行为）——
    - 任何非 High confidence 结果必须带 ≥1 `evidence_refs` 否则 `Fail`；
    - `proposed_writes.target` 非空否则 `Fail`；
    - 空集 `Uncertain`；重复 role 导致聚合歧义 `Uncertain`。
  - 该函数供调用方在**落盘 `proposed_writes` 之前**调用（§7.5「Auditor 先于落盘」的确定性层）。
- **验证证据**：`cargo test --bin FoxIR phase0_audit_aggregate_verdicts` 通过 —— Pass（medium+evidence）/ Fail（low 无 evidence）/ Uncertain（空集、重复 role）全分类正确。
- **范围边界**：§7.5 的 **LLM 语义层**（拉证据内容）与 **Manager 主循环**（聚合→写表→Auditor→Pass 单写者落 proposed_writes / Fail 降级）属 `ManagedRunner`/Manager 层的完整编排，与当前 tool-driven Orchestrator 架构耦合，留待全量 Manager 编排接线时统一落实（与 T2.5 同注）。
- **状态**：T3.3 [>]（确定性层完成）；T2.5 注记更新。


---

## 2026-09-11 · Apply 任务 T4.1 / T4.2（Step 3 Phase 2 前端）：完成

- **T4.1 新 WS 事件 + Agent Card（Phase 0 精简版）**：
  - `src/agent/orchestration.rs` 新增 `budget_update_ws(BudgetSnapshot) -> json` 与 `subagent_ws(SubAgentResult) -> json`（Step 3 事件变体；`budget_update{run_id}` 供子卡片读 per-run budget，`subagent` 供 Agent Card 渲染）。
  - `static/app.js`：新增 `case 'budget_update'` / `case 'subagent'` 事件处理；Agent Card 渲染（`upsertSubagentCard`：role/status/summary/budget + Cancel 按钮，终态隐藏 Cancel）；`setSubagentBudget`；`cancelSubagent(run_id)` 发送 `{type:'cancel_subagent', run_id}`。
  - `src/agent/orchestration.rs` 新增 `cancel_subagent_any(run_id)`（跨所有 live root orchestrator 取消）。
  - `src/server.rs` WS 新增 `cancel_subagent` 客户端命令（调 `cancel_subagent_any` 并回 `subagent_cancelled` 事件）。
- **T4.2 门禁 G3（Instant 逐行为等价）**：Phase 0 验收与全量回归覆盖——Instant 零变化路径（G-instant-tools / G-instant-diff / G4 单二进制纯 Windows）。
- **验证证据**：
  - 新增 `cargo test --bin FoxIR phase0_ws_payload_builders_shape` 通过（budget_update / subagent 形状断言）。
  - `cargo test --bin FoxIR phase0_` 11/11 全绿（含新增 ws / audit / g2 / budget / workflow 项）。
  - `cargo test --bin FoxIR` 全量 264 通过，仅 1 环境性 `forensics::live_tests::test_shimcache` 失败（既有基线，非回归）。
- **状态**：T4.1 / T4.2 标记 [x]（后端 + WS + 精简 Agent Card/cancel 接线完成；index.html 容器级 Expert 视觉打磨属 UI 层，留待人工/后续迭代）。

---

## 2026-09-11 · 编排激活接线（P5 / T5.1–T5.4）：完成

**背景（审查遗留 Critical 2）**：`Runner::run` 构建 ctx 时从不设 `ctx.mode`（恒默认 Instant），唯一设 Expert 的 `context.rs::subagent_child` 其 `can_spawn=false`。故审查断言 `ctx.mode==Expert && ctx.can_spawn` 组合在既有代码中**从不成立**，`LlmAgent` 的 Orchestrator 创建分支（`llm_agent.rs:1379`，门控 `ctx.can_spawn && ctx.mode==Expert && ctx.depth==0`）实际永不触发——整套已实现的编排能力（Phase 0 门禁全绿）处于未激活孤儿态。

**改动**：
1. `src/context.rs` 新增 `InvocationContext::with_mode(AgentMode)` 构造器（`context.rs:618`）。
2. `src/runner.rs`：`Runner`/`RunnerBuilder` 新增 `mode: AgentMode` 字段（默认 `Instant`）+ `with_mode()`；`Runner::run` 链式 `.with_mode(self.mode)` 将运行时模式写入 ctx（`:282`）。
3. `src/main.rs`：Runner 与主代理（`LlmAgent`）同源按 `orchestration_enabled` 构建 Expert/Instant（`:574`）——编排根在 depth 0 真实解锁 7 编排工具投递 + 触发 Orchestrator 创建；关闭时保持 Instant 零 diff。
4. 回归测试（`src/runner.rs` tests）：
   - `runner_expert_mode_propagates_to_runtime_ctx`：Expert+can_spawn → ctx.mode==Expert && depth==0 && can_spawn==true（Orchestrator 分支可达）。
   - `runner_default_stays_instant_non_spawning`：默认 → (Instant, 0, false)，legacy 零变化。

**效果**：主代理交互会话与 ManagedRunner 执行轮共用同一 `state.runner`——激活后两者均按 Expert 根触发编排；子代理仍由 Orchestrator 强制 depth+1 且 can_spawn=false（防碎片化/单写者约束不变）。

**验证证据**：
- `cargo test --bin FoxIR runner_` 2/2 通过。
- `cargo test --bin FoxIR phase0_` 11/11 全绿。
- `cargo test --bin FoxIR` 全量 **267 passed / 1 failed**（唯一失败 `forensics::live_tests::test_shimcache` 为既有环境性基线，非本改动引入；基线 265 + 新增 2）。
- **状态**：T5.1–T5.4 [x]；T5.5（全量 Manager 主循环接线：Manager 规划识别可并行子任务 → `spawn_subagent` 派发 → Auditor 聚合 → 单写者落 TaskContract，TaskContract↔`update_plan`/`proposed_writes` 双向同步，`parent_task_id/depends_on` 扩展）列为下一步蓝图，待本激活确认后实施。

---

## 2026-09-11 · T5.5 并行采集层接线（Manager 主循环接入 Orchestrator）：骨架完成

**目标**：让 ManagedRunner 的 Manager 显式识别可并行子任务，用 Orchestrator 并发跑只读 worker（分散上下文 + 并行加速），并由 Manager（单写者）把结果收敛进本轮（注意力锚点 = TaskContract）。遵循三约束：单写者 / TaskContract 锚点 / 防碎片化（depth-1，can_spawn=false）。

**改动**：
1. `src/managed/manager.rs`：
   - 新增 `ParallelSubtask { role, task }`；`ManagerPlan.parallel_subtasks: Vec<ParallelSubtask>`（`#[serde(default)]`，向后兼容）。
   - `PendingPlan` 同步携带（from_plan/to_plan/roundtrip 保留）。
   - `parse_manager_plan` 解析 `Parallel Subtasks:` 区块（`- role | task`），缺失/空 → 空（legacy 单 Executor 零变化）。
   - Manager system prompt 加第 19 条并行派发规则（2..并发上限的只读无依赖任务；否则省略）。
2. 新增 `src/managed/parallel.rs`：
   - `ParallelEnv`（组装 `OrchestratorEnv`，parent_tx=None 不转发 UI）。
   - `run_parallel_collect`：对每个并行子任务 `spawn`（只读 allowlist、`allow_write/exec=false`、`timeout=default`）→ `wait` 聚合 `SubAgentResult`。
   - `render_collect_brief`：`audit_aggregate` 判定 + 每 worker `[role/status]` 简报。
3. `src/managed/runner.rs` 主循环：Executor 轮前插入并行采集阶段——`plan.parallel_subtasks` 非空时并发采集，简报并入 Executor brief，每 worker summary 写 `contract.manager_notes`（注意力锚点）；verified 状态仍严格由 Executor→Auditor 路径落（单写者）。goods 并行完全等价 legacy。
4. `src/managed/mod.rs`：注册 `pub mod parallel` + `run_parallel_collect` 导出。

**验证证据**：
- `cargo test --bin FoxIR managed::manager::tests` 3/3（向后兼容 / parallel 解析 / PendingPlan roundtrip）。
- `cargo test --bin FoxIR managed::parallel::tests` 4/4（spec 只读 / 空简报 / Pass 聚合 / low-无证据不 Pass）。
- `cargo test --bin FoxIR` 全量 **274 passed / 1 failed**（唯一失败 `test_shimcache` 为既有环境性基线；+9 新增：2 runner wiring + 3 manager + 4 parallel）。
- **默认零变化**：parallel_subtasks 缺省为空 → 主循环直接走原单 Executor 路径。

**明确未做（诚实标注）**：
- `task_contracts.parent_task_id/depends_on` 任务树字段（涉及 DB schema v8 迁移，需独立轮次并跑迁移测试）。
- `OrchestrationLimits` 目前用 `::default()`，尚未从 `config.agent.modes.expert` 透传（server.rs 构造参数未扩）。
- 真实 LLM 端到端 Phase 0 指标（wall-clock ≥30% / token ≤50%）需真实会话验证——当前单测用 mock 校验逻辑层。
- 语义层 Auditor 收敛（§7.5 LLM 部分）与 TaskContract↔`update_plan` 双向同步未做（与任务树耦合，留后续）。

---

## 2026-09-11 · ③ 端到端指标 + 独立测试组织（T2.9/T3.5/T1.10）：指标落盘 + 可行性结论

**落地**（`src/phase0_acceptance.rs`）：新增自包含报告测试 `phase0_report_generates_artifact`，单次跑完四道门禁（wall-clock / token / 稳定性采样 mock n=5 + 5x2 runs），结果序列化并写入 `output/phase0_acceptance.json`（可复现"跑一次看指标"），并断言阈值。

**mock 实测指标（无外部 LLM，可复现）**：
- wall-clock：serial 0.43s → parallel 0.09s，**speedup 4.76x**（门槛 ≥30% 削减，远超）。
- token：parallel_delta vs serial **+0.0%**（门槛 ≤50% 增幅，通过识别到 mock 每次固定 usage）。
- 稳定性：10 worker runs，**0 丢失**。

**真实 LLM 端到端（诚实结论）**：本环境**无配置的 OpenAI 兼容端点**（仓库无 `config.toml`，`api_base`+`api_key` 均未设置），无法对真实模型实跑 wall-clock/token 指标。报告 `provider:"mock"`、`status:"ok"`；如需真实端点验收，需在 workspace `config.toml`（或环境变量）配置 `api_base` + `api_key` 后复跑。

**独立测试文件可行性结论（诚实标注）**：`tests/phase0_e2e.rs`/`mock_llm.rs`/`step1_gates.rs` 在**当前纯 bin crate（无 `src/lib.rs`）下架构不可行**——Rust 集成测试 target 只能链接 lib target，无法 `use foxir::agent::…` 访问 crate 内部（T2.9 标 [>] 的根因，非遗漏）。要真正拆独立 tests/ 需先建 lib target（把 `main.rs` 的 `mod` 声明与主逻辑迁至 `src/lib.rs`，`main.rs` 变薄壳），属大重构、独立轮次。

**全量回归**：`cargo test --bin FoxIR` → **275 passed / 1 failed**（唯一 `test_shimcache` 为既有环境性基线；+1 新增 report 测试）。

---

## 2026-09-11 · T5.5 config 透传：OrchestrationLimits 从 config 进入 ManagedRunner

- `src/managed/runner.rs`：`ManagedRunner` 新增 `orchestration_limits: OrchestrationLimits` 字段 + `new()` 参数；`run()` 主循环在 spawn 前捕获 local `orchestration_limits`，并行采集块用其替代 `::default()`（async move 内不能引用 self，故先行拷贝）。
- `src/server.rs`：`AppState` 新增 `orchestration_limits: Arc<OrchestrationLimits>`，`ManagedRunner::new` 调用传 `*state.orchestration_limits`。
- `src/main.rs`：`orchestration_limits: Arc::new(OrchestrationLimits::from(&config.agent.modes.expert))`（复用已有 `From<&ExpertModeConfig>`）。
- **效果**：并行采集的真实并发上限（`max_concurrent_subagents`）与默认 worker 超时（`default_timeout_secs`）由 `config.agent.modes.expert` 驱动，而非硬编码 default。
- **验证**：`cargo build` 通过；全量 `cargo test --bin FoxIR` → 275 passed / 1 failed（唯一 `test_shimcache` 环境基线）。

---

## 2026-09-11 · T5.5 任务树字段（§7.8.1）：parent_task_id / depends_on

- `src/managed/task_contract.rs`：`TaskRecord` 新增 `parent_task_id: Option<String>` 与 `depends_on: Vec<String>`（均 `#[serde(default)]`，旧 JSON blob 反序列化向后兼容，**无需 DB 迁移**——contract 以 JSON 存于 `task_contracts` 表）。
- `src/managed/runner.rs`：
  - 并行采集块为每个 worker 追加 `TaskRecord{kind:"subtask", parent_task_id: Some("round-{round}")}`，status/integrity 由 worker 结果决定（Ok→completed/clean，否则 untrusted/suspect）；verified 提升仍由 Executor→Auditor 单写者把关。
  - 既有 Auditor evidence artifact 记录（completed 与 untrusted/pending 两处）补 `parent_task_id: Some("round-{round}")`。
  - `seed_untrusted_handoff` 记录补 `parent_task_id: None`。
- **验证**：`managed::task_contract::tests` 2/2（旧 JSON 兼容 + 新字段 roundtrip）；全量 `cargo test --bin FoxIR` → **277 passed / 1 failed**（唯一 `test_shimcache` 环境基线）。
- **T5.5 收口**：并行采集层 + config 透传 + 任务树字段全部完成；**仅剩真实 LLM 端到端指标验证**（依赖可配置的 OpenAI 兼容端点，当前环境无）。

---

## 2026-09-11 · 收口 T0.1（spec 评审关闭）与 T1.4（ToolContext 透传核实）

- **T0.1 → [x]**：评审在实施中完成并关闭——G-instant 门控修复、Critical 2 激活接线、T5.5 三项（并行采集 / config 透传 / 任务树字段）等所有待澄清项均已收敛并记录（见 2026-09-11 各轮 log）。注：v1.5 spec 自身状态仍为"待评审"，此处关闭的是"本 change 的评审/澄清口径"。
- **T1.4 → [x]**：核实 worker 已走统一透传——`Orchestrator::spawn` 用 `InvocationContext::subagent_child(SubagentChildParams)`（mode=Expert / depth+1 / can_spawn=false / session_kind=SubAgent / skill Disabled / run_id）；工具执行点（`llm_agent.rs:2680-2696`）把运行时 `ctx` 派生的 `mode/depth/can_spawn/run_id` 写入 `ToolContext`（非 agent 静态字段）。标注 [>] 的旧措辞（"逐字段重建、未走统一路径"）已过时。
- **验证**：`cargo test --bin FoxIR` → 277 passed / 1 failed（唯一 `test_shimcache` 环境基线）。

---

## 2026-09-11 · T5.5 真实 LLM 端到端验证（配置解密后）：完成

- **解密**：`models.json` 里 `api_key` 为 AES-256-GCM 密文（密钥由 Windows `MachineGuid` 派生，`src/crypto.rs`/`src/model_store.rs`）。lifecycle 测试改用 `model_store::load_configs` 在**进程内解密**（key 不出日志），替代手写 `serde_json` 解析导致 401/403 的错误尝试。
- **live e2e**：新增 `phase0_live_end_to_end`（`src/phase0_acceptance.rs`，`FOXIR_LIVE_E2E=1` 门控——默认 skip 以免常规回归烧 token）。从 workspace `models.json` 载入模型、probe 可达性（`tokio::time::timeout` 包裹）、跑 3 worker 真实并行 collect。
- **真实实测（`deepseek-v4-flash-0731`）**：
  - wall-clock：serial `7.25s` → parallel `2.36s`，**speedup 3.08x**（≥30% 门槛，达标）。
  - tokens：serial `15374` vs parallel `15298`，增幅 **-0.5%**（≤50% 门槛，达标）。
  - 结果留存 `output/phase0_live.json`（`status:"ok"`）。
- **验证**：全量 `cargo test --bin FoxIR` → **278 passed / 1 failed**（唯一 `test_shimcache` 环境基线；live 测试 gate 后默认 skip）。
- **T5.5 收口为 [x]**：并行采集层 + config 透传 + 任务树字段 + 真实端到端全部完成。


## 2026-09-11 · §7.5 单写者审计门控（T2.5 / T3.3 收口）

- **确定性审计门控接线**（`src/managed/parallel.rs` + `src/managed/runner.rs`）：新增 `CollectDisposition{Promote, Degrade(String)}` 与 `collect_disposition(results)`（内部调用 `audit_aggregate`）。Manager 并行采集块在**落盘前**调用它决定处置：
  - `Promote` → worker `TaskRecord` status=`completed` / integrity=`clean`，进入可信状态；
  - `Degrade` → 降为 `untrusted`/`suspect`，并 `contract.add_lead("Parallel collect not certified", reason)` + manager note（§7.5 方案 B：部分结果+分歧说明），绝不进入 verified。
- **证据采集**（`src/agent/orchestration.rs`）：新增 `pub fn string_leaves(value, &mut Vec<String>)`，递归收集 ≤300 字符字符串叶子 + 对象 `path` 字段；`run_worker` 从 `ToolResult` 事件填充 `result.evidence_refs`（cap 24、去重）。修复了非 High 置信 worker 因硬编码空 `evidence_refs` 而必然 Degrade 的问题——现在有真实引用可审计，`collect_disposition` 可对 medium+evidence 返回 Promote。
- **测试**：新增 4 测（`collect_disposition_promotes_evidence_backed_results` / `degrades_without_evidence` / `degrades_empty_proposed_write_target` / `degrades_empty_results`），移入 `managed::parallel::tests` 模块。`managed::parallel::tests` 8/8 绿；全量 `cargo test --bin FoxIR` → **282 passed / 1 failed**（唯一 `test_shimcache` 环境基线，非回归）。
- **T2.5 / T3.3 → [x]**。**诚实标注未做**：① §7.5 降级方案 A（`update_plan` 重规划再跑）未实现，当前仅 B；② LLM 语义层 Auditor（`managed/auditor.rs` 扩输入）未实现，当前为确定性 `audit_aggregate` 门控。


## 2026-09-12 · §10 编排模板对应性验证（ADK-Rust 对齐）+ 数据驱动测试

- **能力核实**：以 crates.io/docs.rs 已发布 `adk-agent`（v2.2.0）为准确认 ADK-Rust Agent 类型：`SequentialAgent`（sequence）/ `ParallelAgent`（concurrent）/ `LoopAgent`（until exit condition, `with_max_iterations`）/ `CustomAgent` / `ConditionalAgent`。
- **对应性映射**：`workflow::sequential` ↔ SequentialAgent（保序 + 错误短路）；`workflow::parallel` ↔ ParallelAgent（并发 + 聚合）；`workflow::loop_until` ↔ LoopAgent（退出谓词 / max_rounds 上限）。**差距**：`ConditionalAgent` 未实现。
- **代码**：`src/agent/workflow.rs` 增 `WorkflowStep.timeout: Option<u64>` + `with_timeout()` 构造器 + `into_spec` 透传（使模板测试能触发 Sequential 短路）；`src/phase0_acceptance.rs` 增 4 个数据驱动模板一致性测试（多组数据集/样例）。
- **测试证据（实测）**：4/4 模板测试通过；全量 286 passed / 1 failed（唯一 `test_shimcache` 环境基线，非回归）。
  - Parallel 并发：serial 760ms vs parallel 187ms → **4.06x**（3 台 mock 置信）。
  - Sequential 保序：S1 3 步 / S2 4 步全保序且 Ok。
  - Sequential 短路：第 2 步 Timeout → len=2 立即停（summarize 未跑）。
  - Loop：L1=2 谓词早停 roles 唯一；L2=3 max_rounds cap。
- **报告**：`output/orchestrator_template_conformance.md`。


## 2026-09-12 · T6.4 `authorized_tools()` 占位实现 + 接线（§7.10 / Step 2a/2b）

- **映射（grounded）**：`permission_profile.rs` 新增 `pub fn authorized_tool_names(profile) -> Vec<String>`，为 `check_preauthorization` 的**反向映射**，只包含它已识别的 tool name arm：KillProcess/KillProcessNamed→`shell_exec`+`sys_process`；StopService→`shell_exec`；RemovePersistence→`ir_persistence`。IsolateHost / DeleteFilesInPath / DisableAccount / `allow_all` 无工具授予（`allow_all` 是权限门旁路，非工具集授权）。已去重。
- **委托**：`context.rs::InvocationContext::authorized_tools()` 从 `preauth_profile` 调 `authorized_tool_names`（原空占位）。
- **接线（安全）**：`orchestration.rs` 在 `spec.allow_write || spec.allow_exec` 且 `tools_allowlist` 为空时，将授权工具名追加进 worker `allow`；Phase 0 只读 worker（双 allow=false）保持纯只读，读保护不变。
- **测试**：`permission_profile::tests::authorized_tool_names_*`（映射 / 通配 / 未映射 / 空与 allow_all / 去重，5 测）+ `context::tests::authorized_tools_empty_without_profile_and_derived_with_containment`。全量 `cargo test --bin FoxIR` → **292 passed / 1 failed**（唯一 `test_shimcache` 环境基线，非回归；+6 新测）。
