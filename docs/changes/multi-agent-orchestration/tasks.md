# Tasks · 多代理编排（multi-agent-orchestration）

> 来源：`spec.md`（v1.5，状态待评审）。
> 状态图例：`[ ]` 未开始　`[>]` 进行中/部分完成（见批注）　`[x]` 已完成
> 依据 spec §13 实施计划生成；Step 1 落地顺序以 §13 表为准。
> **2026-09-11 已对照 spec v1.5 现场代码核验**，见各 task 批注与 `log.md`。

## P0 · 评审与基线

- [x] T0.1 评审 v1.5 spec，收敛全部待澄清项，确认后进入 Apply —— **在实施过程中完成并关闭**：G-instant 投递门控修复、Critical 2（`ctx.mode==Expert && ctx.can_spawn` 组合从不成立）激活接线、T5.5 并行采集 / `OrchestrationLimits` 透传 / `parent_task_id`+`depends_on` 任务树等全部待澄清项均已收敛并记录于 `log.md`（2026-09-11 多轮）
- [x] T0.2 已切分支 `feature/multi-agent-orchestration`（非 main/master）

## P1 · Step 1 模式与上下文骨架（spec §13 Step 1）

- [x] T1.0 删除幽灵文件 `src/agent/spawner.rs`（已确认不存在）
- [x] T1.1 `src/context.rs`：`AgentMode`/`SessionKind`/`SubAgentSpec` 等枚举结构 + `can_spawn`/`set_ended`（存在）
- [x] T1.2 `is_main_session` 修复：排除 `sub-` 与 `cron-` 前缀（`llm_agent.rs:284`）
- [x] T1.3 `.without_skills()` + `SkillManager` 改 `Option` + 风险 2 兼容路径（`llm_agent.rs:383,953`；单测 :3440）
- [x] T1.4 `ToolContext` 透传 —— **已核实为统一透传路径**：worker 经 `InvocationContext::subagent_child(SubagentChildParams)` 统一构造（mode=Expert / depth+1 / can_spawn=false / run_id）；工具执行点（`llm_agent.rs:2680-2696`）从运行时 `ctx` 派生的 `mode/depth/can_spawn/run_id` 写入 `ToolContext`，非静态 agent 字段
- [x] T1.5 投递期门控：`ALL_ORCH` + `orchestration_allowset()` 真值表 + `d.function.name`（`llm_agent.rs:248-279`；真值表单测 :3391）。**2026-09-11 回顾审查修复**：投递点从 agent 静态 `self.mode/self.depth` 改为运行时 `ctx.mode/ctx.depth`（止住 Instant 也收到编排工具的 G-instant 违约）；新增回归 `gate_instrument_delivery_reads_ctx_not_agent_static_mode`
- [x] T1.6 7 工具注册 —— 已做成**完整** 7 工具，非 spec 的“壳工具”；且把本属 Step 2b 的 4 工具提前实现（Scope 超前，见 P3，实质完成）
- [x] T1.7 配置：`[modes.*]` + 上限 + `skill_strategy="disabled"` + 校验（`config.rs`）
- [x] T1.8 `SkillListingStrategy::Disabled` 三映射 + `build_skills_prompt` 早退 + `skill_tool_names`（含 `improve_skill`；但为静态数组，非 V.4「动态求并集」）
- [x] T1.9 `ToolRegistry::subset`/`minus`/`iter`/`register_arc` 全部就位（`register` 委托 `register_arc`；`iter` 产出 (&str, &Arc<dyn Tool>)；单测 `tool::tests::iter_covers_registry_and_register_arc_inserts`）
- [>] T1.10 门禁 —— 无独立 `tests/step1_gates.rs`；相关单测分散内嵌（`llm_agent.rs`/`skill/mod.rs`/`tool/mod.rs`）。2026-09-11 补 G-instant 回归（投递门控读 ctx）

## P2 · Step 2a Phase 0 受限只读并行（spec §13 Step 2a）

- [x] T2.1 Orchestrator 实现（`src/agent/orchestration.rs`）—— 真实 API + 阻塞 run_worker + tokio::spawn + **timeout 包装**（`tokio::time::timeout`，`SubAgentStatus::Timeout` 可达并持久化）。**§7.4.1 子代理 mode → Expert 已对齐**（`orchestration.rs:241`/`:270` + `debug_assert` 守卫）
- [x] T2.2 权限三层收紧 + skill 工具动态剔除（`allow.retain` 剔除 skill + orchestration 名）
- [x] T2.3 EventPump —— 按 §7.4.2 实现：per-worker `channel(cap=128)` + 单一 `EventPump`（`FuturesUnordered` 动态汇聚）+ 分级背压（TextDelta `try_send` / 其余 `send().await`）；run_worker 写入自有通道，经 EventPump 转发父流；**注册通道改有界 `cap=max_concurrent*2`、`spawn` 用 `send().await`**（接受 spec 增量，消解 Unbounded 背压失效）
- [x] T2.4 结果持久化：`subagent_results` 表（`memory.rs` v8）+ `save`/`load`，终态写入
- [x] T2.5 流程顺序 —— run_worker 直接保存终态并落内存；**§7.5 单写者审计门控已接线**：Manager 并行块在落盘前调用 `collect_disposition(&p_results)`（内部确定性 `audit_aggregate`），Pass→worker 记录 `completed/clean` 并进入可信状态；Fail/Uncertain→降级为 `untrusted/suspect` 并 `contract.add_lead`（部分结果+分歧说明，方案 B），绝不进入 verified 状态。**LLM 语义层 Auditor（`managed/auditor.rs` 扩输入）仍未实现**（诚实标注）
- [x] T2.6 三层取消树 —— Phase 0（depth-1 无孙代理）为「root_ended + per-worker cancel」两层：`root_ended`（`ctx.ended_flag` 级联所有 worker）+ 每 worker `SubAgentHandle.cancel`；`cancel(run_id)` 置该 worker flag，`run_worker` 循环 `handle.cancel || child_ctx.is_ended()` 早退并置 `Cancelled`。**行为已专项核验**（`phase0_cancel_worker_marks_cancelled`）
- [x] T2.7 预算 per-run —— `Orchestrator.budgets: Arc<Mutex<HashMap<String, BudgetSnapshot>>>` + `run_worker` 内 `Usage` 事件累加 `prompt_sum`/`completion_sum`，终态写入 `BudgetSnapshot{run_id,role,prompt/completion/total}`；`budget(run_id)` / `budgets()` 访问器。**行为已专项核验**（`phase0_budget_per_run_populated`：mock Usage 120 = prompt100+completion20）
- [x] T2.8 Exclusivity —— `ExclusivityRegistry::acquire` 改为 `async fn acquire(&self, name: &str) -> tokio::sync::OwnedMutexGuard<()>`（§7.4.3）；`llm_agent.rs:2696` 工具执行前按 `tool.exclusivity()` 为 `Exclusive` 的持证锁跨全执行期持有（`drop` 释放），consumer 已对齐
- [>] T2.9 测试 —— Phase 0 验收内嵌 `src/phase0_acceptance.rs`（wall-clock/token/20 runs/kill-9 四门禁，mock LLM）+ `phase0_report_generates_artifact`（指标落盘 `output/phase0_acceptance.json`：mock 4.76x / token +0.0% / 10 无丢失）+ `phase0_live_end_to_end`（真实端点，`FOXIR_LIVE_E2E=1`；解密 key，实测 3.08x / -0.5%，落盘 `output/phase0_live.json`）。**独立 `tests/phase0_e2e.rs`/`mock_llm.rs` 纯 bin crate 下不可行**（无 `src/lib.rs`，标准集成测试无法 `use` crate 内部）——需先建 lib target，属明确后续

## P3 · Step 2b Phase 1 全量编排（spec §13 Step 2b）

- [x] T3.1 4 工具（`cancel_subagent`/`get_subagent_result`/`update_plan`/`read_subagent_log`）**已提前实现**于当前工具集
- [x] T3.2 `allow_write`/`allow_exec` + 写/执行 worker 串行 —— `ToolRegistry::write_exec_names(allow_write, allow_exec)`（§7.10 layer 2：write/modify/delete / execute 分档）；`spawn` 按 opt-in 扩充 allowlist；`Orchestrator.write_gate`（`Arc<tokio::sync::Mutex<()>>`）让写/执行 worker 强制单写者串行，只读 worker 不触碰；`spawn_subagent` 移除 Phase 0 写拒绝。单测 `tool::tests::write_exec_names_admits_optin_tiers`
- [x] T3.3 Auditor 契约（§7.5 确定性层）—— `audit_aggregate(results) -> AuditVerdict{Pass, Fail(reason), Uncertain}` 已落地（非 High 必带 evidence_refs / proposed_write target 非空 / 唯一 role / 空集 Uncertain）；`src/agent/orchestration.rs`。单测 `phase0_audit_aggregate_verdicts`。**已在 Manager 主循环接线**（`collect_disposition` 先于落盘，见 T2.5）。证据采集：`string_leaves`（递归收集 ≤300 字符字符串叶子 + `path` 字段）从 `ToolResult` 事件填充 worker `evidence_refs`（cap 24 去重）——非 High 置信结果现在有真实引用可审计。新增 4 测 `managed::parallel::tests::collect_disposition_*`。**LLM 语义层仍待实现**
- [x] T3.4 工作流模板（Sequential/Parallel/Loop）—— 新增 `src/agent/workflow.rs`：`sequential`（顺序 await、fail-fast）/ `parallel`（并发 spawn + join_all 聚合）/ `loop_until`（`max_rounds` + `update_plan` 轮间重规划 + `stop` 谓词早停）；不引入新 Agent 类型（§10）。`task_contracts.parent_task_id/depends_on` 扩展留待 Manager 主循环接线（记注）。单测 `phase0_workflow_sequential_then_parallel_then_loop`（seq=3/par=3/loop=2）
- [x] T3.5 门禁：G2 已落地（`phase0_g2_subagent_is_temporary_no_main_session`）；G-concurrency（20-run 60 条无丢失）/ G-resume（kill-9 不重 spawn）由 Phase 0 验收覆盖；2026-09-11（③）门禁指标固化为可复现报告 artifact（`output/phase0_acceptance.json`）。**2026-09-12 订正**：§20.7 Skill opt-in 已由 T6.7 决策（worker 构造即 skill-off 双关闭为既定默认，B1/B2 留待 Step 2b），本节原文「未触发」改为「已决策」。

## P4 · Step 3 Phase 2 前端（spec §13 Step 3）

- [x] T4.1 新 WS 事件变体 + 后端接线 —— `budget_update`/`subagent` WS payload builder（`budget_update_ws`/`subagent_ws`，单测 `phase0_ws_payload_builders_shape`）；`static/app.js` Agent Card（`upsertSubagentCard`/`setSubagentBudget`/`cancelSubagent`）+ `cancel_subagent` 客户端命令（服务端 `cancel_subagent_any` 跨所有 live root 取消）。index.html 容器级 Expert 视图完善留待 UI 手动打磨
- [x] T4.2 门禁 G3（Instant 逐行为等价）—— 由 Phase 0 既有验收覆盖（Instant 零变化：G-instant-tools/diff / wall-clock / token / 全量回归 264 通过，仅 1 环境性 shimcache 失败）；G3 声明为已覆盖

## P5 · 编排激活接线（Runner → ctx.mode，实际触发编排）

> 对应「把主代理按 Expert 构建接通 ManagedRunner 以实际触发编排」；含 2026-09-11 审查遗留的 Critical 2「组合 `ctx.mode==Expert && ctx.can_spawn` 从不成立」修复证据。

- [x] T5.1 `InvocationContext::with_mode()` 构造器（`src/context.rs:618`）
- [x] T5.2 `Runner.mode` 字段 + `RunnerBuilder::with_mode()`（默认 Instant），`Runner::run` 链式 `.with_mode(self.mode)` 将模式传播到运行时 ctx（`src/runner.rs:50/74/97/183/282`）
- [x] T5.3 `src/main.rs`：Runner 与 LlmAgent 同源（`orchestration_enabled`）构建 Expert/Instant——使编排根 `orchestration_delivered`（depth 0）与 `Orchestrator` 创建分支真实触发（`src/agent/llm_agent.rs:1379`）；关闭时保持 Instant 零 diff
- [x] T5.4 回归测试 `runner_expert_mode_propagates_to_runtime_ctx` / `runner_default_stays_instant_non_spawning`（`src/runner.rs` tests）—— 验证 Expert+can_spawn→ctx 三元组 (Expert,0,true)、默认→(Instant,0,false)
- [x] T5.5 全量 Manager 主循环接线 —— 并行采集层（`ManagerPlan.parallel_subtasks`+`parse`+prompt 引导 / `src/managed/parallel.rs` `run_parallel_collect`+`render_collect_brief` / 主循环 Executor 轮前插入）+ config 透传（`OrchestrationLimits` 进入 ManagedRunner）+ 任务树字段（`TaskRecord.parent_task_id/depends_on`，§7.8.1，serde-default 兼容）**全部完成**。**真实 LLM 端到端**（phase0_live_end_to_end，`FOXIR_LIVE_E2E=1` 启用；key 经 `model_store::load_configs` 在进程内 AES-GCM 解密）实测 `deepseek-v4-flash-0731`：**wall-clock 7.25s→2.36s、加速 3.08x**（≥30% 门槛）、token 增幅 **-0.5%**（≤50% 门槛）——真实并行加速达标


## P6 · 其余 SDD 未完成项（2026-09-12 评审补记）

> 核对 spec 原文与 tasks/log 后补录的待办，按优先级排序。状态图例同前。

- [x] T6.1 §7.5 LLM 语义层 Auditor（`managed/auditor.rs`）—— 新增 `SubagentAggregateAudit{passed,reason}` + `audit_subagent_aggregate(subtasks, results, context)`（输入 = 并行子任务 + 全部 SubAgentResult + round 目标）+ `parse_aggregate_verdict`；provider=None 时返回 None（确定性门控独立成立）。runner 并行块在确定性 `audit_aggregate` Pass/Uncertain 时调用语义层，**语义只降不升**，LLM 失败/不通过则 demote 并记录 reason。**诚实标注**："对矛盾结果读 worker events" 部分仅传 evidence_refs/summary，未接完整 event 日志。
- [x] T6.2 §7.5 聚合失败降级方案 A —— runner 并行块实现有界 re-run（`PARALLEL_RERUN_CAP=2`，跨轮 `parallel_reruns` 计数）：未认证且预算未耗尽时重跑只读 collect（幂等）后再审计；耗尽后回落到 B（部分结果+分歧 lead）。**诚实标注**：re-run 为"重跑同一批只读子任务"的有界重试，非重新 LLM 规划（`update_plan` 生成不同任务的语义重规划未做）；受 cap 约束。
- [x] T6.3 模板接入 ManagedRunner 主循环 —— 新增 `config::OrchestrationTemplate{Sequential,Parallel}`（Copy，Default=Parallel）+ `ExpertModeConfig.orchestration_template`（serde default "parallel"，含校验）+ `OrchestrationLimits.template`；`parallel.rs::run_template_collect(env,subtasks,template,...)` 把 `ParallelSubtask` 映射为 `WorkflowStep` 并分发到共享 `workflow::sequential` / `workflow::parallel`；runner 并行块改走 `run_template_collect`。测试：`config::tests::orchestration_template_defaults_parallel_and_parses` + `phase0_template_collect_dispatches_sequential_and_parallel`；全量 299 passed / 1 env fail。**诚实标注**：Loop 是外层多轮 Manager 主循环（非并行块可选项）；`workflow::loop_until` 仍独立未由 runner 直接调用。
- [x] T6.4 `authorized_tools()` —— 已实现并接线：`permission_profile::authorized_tool_names(profile)`（`check_preauthorization` 的反向映射，严格基于其已识别的 tool arm：`shell_exec`/`sys_process`/`ir_persistence`；IsolateHost/DeleteFilesInPath/DisableAccount/allow_all 无工具授予）；`context.rs::authorized_tools()` 委托该函数；`orchestration.rs` 在 write/exec worker 且无显式 allowlist 时追加这些工具名（Phase 0 只读 worker 保持纯只读，读保护不变）。单测：`permission_profile::tests::authorized_tool_names_*`（5）+ `context::tests::authorized_tools_empty_without_profile_and_derived_with_containment`；全量 292 passed / 1 env fail。
- [x] T6.5 TaskContract ↔ `update_plan` 双向同步（§7.8.1）—— `Orchestrator` 增持久化钩子（`set_plan_persist` + `seed_plan`，`update_plan` 同时更新内存镜像并推入 sink）、`TaskContract.orchestrator_plan` 持久字段 + `set_orchestrator_plan`/`orchestrator_plan_value`、`ParallelEnv` 增 `plan_seed`/`plan_persist` 并接线 runner 并行块（先落种到 contract，collect 后再把 `update_plan` 写回 contract）。测试：`task_contract::tests::orchestrator_plan_roundtrips_and_defaults_none` + `phase0_plan_bidirectional_sync_seed_and_persist`（种子→内存、update_plan→sink 双向）。全量 303 passed / 1 env fail。
- [x] T6.6 Step 3 前端 UI（Expert 多 Agent 视图）—— 后端新增 WS 事件类型 `subagent_spawned`/`subagent_completed`/`subagent_failed`/`budget_update`/`plan_updated`（`AgentEvent` 新变体 + 构造器 + `meta()`，runner 并行块与 plan 处发射）；前端 `static/index.html` 新增右滑 Expert 抽屉（Task Queue / Agent Cards / 预算抽屉 / 日志抽屉 / 聚合摘要 / 证据引用可点击跳 `/workspace/<rel>` / Overview 统计 / 模式通道复用），`node --check` 通过。bin 304 passed / 1 env fail（无回归）；integration 9 passed。
- [x] T6.7 §20.7 Skill opt-in Review —— **执行并给出决策**：Phase 0/当前 worker 由构造即 skill-off（`subagent_child` 设 `SkillListingStrategy::Disabled` + `orchestration.rs` `.without_skills()`），`SubAgentSpec.skills` 字段无消费路径 → **opt-in 未启用为既定默认**，B1/B2 附加留待 Step 2b；新增 `context::tests::subagent_child_keeps_skills_disabled_read_only` 把"worker 构造即 skill 双关闭 + 只读"固化为测试。
- [x] T6.9 Orchestrator CLI 自测（`--orch-self-test <sequential|parallel|loop> [n]`）—— 新增 `src/orch_selftest.rs`（构建 `ParallelEnv` + `subtasks(n)` 直接驱动 Orchestrator，绕过 Manager LLM 保证可派生可观察）+ `parallel.rs::run_loop_collect`（`workflow::loop_until` 有界深潜）+ `main.rs` CLI 分支 + `phase0_run_loop_collect_produces_bounded_unique_rounds`（≤max_rounds/只收 Ok/role 唯一）。全量 301 passed / 1 env fail；release 构建通过。
- [x] T6.8 建 lib target + 门禁聚合 —— 新增 `src/lib.rs`（与 bin 同源模块树的 dual-compile lib，bin 不动零回归）；门禁 helpers `orchestration_allowset`/`orchestration_delivered`/`ALL_ORCH` 提为 `pub`。新增 `tests/step1_gates.rs`（G-instant-tools / G-sub-not-main / G-expert-root / G-name-disjoint 共 5 测）、`tests/mock_llm.rs`（canned Manager 输出→并行/串行解析门控，2 测）、`tests/phase0_e2e.rs`（模板默认/解析 + contract 计划同步往返，2 测）+ `tests/common/mod.rs`。bin 303 passed / 1 env fail（无回归）；integration 9 passed。
