# FoxIR 双模式编排重构 —— 校验加固版设计文档

| 项 | 内容 |
|---|---|
| 文档状态 | **加固版 v1.1-hardened（待评审）** |
| 产出方式 | Superpowers `brainstorming` skill（架构路径）对 v1.0 做代码级校验 + 加固 |
| 基线文档 | [docs/specs/dual-mode-orchestration-refactor-spec.md](file:///e:/VSCode/FoxIR/docs/specs/dual-mode-orchestration-refactor-spec.md)（定稿 v1.0，D1–D9） |
| 本文档定位 | **不推翻 v1.0 方向**；用真实代码逐条校验其「现状事实」，修正 3 个会导致 P1–P3 编译失败/行为不符的硬缺口，补全 4 个连带项，给出可直接实施的完整改动清单 |
| 分叉决策 | 用户取消逐条裁定，3 项按推荐默认采纳（D8′/D10/D11），**可在本文档评审门推翻** |
| 日期 | 2026-09-12 |

---

## 0. 阅读指引

- v1.0 的 §1（背景）、§2（目标/非目标）、§4（目标架构）、§5（D1–D6）、§7（关键细节）**经校验成立，本文档不重复**，仅在下述章节做增量修正。
- 本文档新增/修正的部分以 **「加固」** 标注，均附代码证据行号。
- 实施时以 **本文档 §3 的完整改动清单** 为准（它覆盖并修正了 v1.0 §6 的遗漏），阶段顺序仍沿用 v1.0 §10 的 P1–P8。

### 加固发现索引（H1–H7 → 落点）

| ID | 发现 | 落点 |
|---|---|---|
| H1 | D8 只改 builder 静态 mode，漏运行时硬编码 + `debug_assert` panic | §2.2 → **D8′** |
| H2 | 门控翻转漏 `can_spawn`/`mode`/配置键的根构造耦合 | §2.1 + §2.4（**D11**） |
| H3 | P1 剥离连带引用超出 §6.1/§9.2（orch_selftest / phase0 / `run_loop_collect`） | §3 P1 表 |
| H4 | Risk D 可判定：`parallel_subtasks` 为 `serde(default)`，删除安全 | §3 P1 注 |
| H5 | v1.0 §3.1 构造门表述漏 `can_spawn` 条件 | §1 表 + §2.1 |
| H6 | 翻转后 7 工具常驻投递违背验收 #2 | §2.3 → **D10** |
| H7 | 剥离后 `orchestration_template`/`OrchestrationTemplate` 成死代码 | §2.4 + §3 P1 |

---

## 1. 代码级校验结果（v1.0 承重事实）

对 v1.0 §3「现状架构事实核对」逐条核验，结论：**事实核对可信，可作实施基线**。

| v1.0 声明 | 核验 | 证据 |
|---|---|---|
| `orchestration_allowset`：`Expert && depth==0` → 7 工具，其余空 | ✅ 成立 | [llm_agent.rs#L264-L272](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L264-L272) |
| 编排器构造门：仅 `mode==Expert && depth==0` | ⚠️ 不精确（漏 `can_spawn`）→ 见 §2.1（H5） | [llm_agent.rs#L1399-L1402](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L1399-L1402) |
| 三档权限（只读子集 + allow_write/allow_exec）已实现 | ✅ 成立 | [orchestration.rs#L343-L362](file:///e:/VSCode/FoxIR/src/agent/orchestration.rs#L343-L362) |
| 写/执行 worker 已 `write_gate` 强制串行 | ✅ 成立（构造链确认） | [orchestration.rs#L343-L410](file:///e:/VSCode/FoxIR/src/agent/orchestration.rs#L343-L410) |
| `ManagerPlan.parallel_subtasks` 字段 + 解析 | ✅ 成立 | [manager.rs#L42-L44](file:///e:/VSCode/FoxIR/src/managed/manager.rs#L42-L44)、[#L442-L446](file:///e:/VSCode/FoxIR/src/managed/manager.rs#L442-L446) |
| 执行门控 `gate(ctx)+orch(ctx)` | ✅ 成立，**但 `gate` 只看 `can_spawn`/`depth`，不看 mode** → 见 H2 | [tool/orchestration.rs#L45-L58](file:///e:/VSCode/FoxIR/src/tool/orchestration.rs#L45-L58) |
| 待翻转回归测试 | ✅ 定位准确 | [llm_agent.rs#L3415-L3419](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L3415-L3419)、[#L3474-L3485](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L3474-L3485) |

---

## 2. 加固决策（对 v1.0 的修正与新增）

### 2.1 〔H2 · 最严重〕门控翻转 ≠ 改 `mode==`：`can_spawn`/`mode`/配置键在根构造处耦合

**问题**：v1.0 §6.2 把「门控翻转」表述为改几处 `mode==Expert` 条件。实际上编排能力由 `can_spawn` 门控，而 `can_spawn` 与 `mode` 在启动时由**同一个** `orchestration_enabled` 开关绑定，并被盖章进每一次 Instant 根运行：

- 执行门 `gate(ctx)` 判 `!ctx.can_spawn → not_available_in_mode`、`ctx.depth>=1 → depth_limit`（[tool/orchestration.rs#L45-L53](file:///e:/VSCode/FoxIR/src/tool/orchestration.rs#L45-L53)）——**不看 mode**。
- 编排器构造门要求 `ctx.can_spawn && mode==Expert && depth==0`（[llm_agent.rs#L1399-L1402](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L1399-L1402)）。
- `orchestration_enabled = config.agent.modes.expert.orchestration == "on"`（[main.rs#L540](file:///e:/VSCode/FoxIR/src/main.rs#L540)）。
- 主 agent 静态 mode = `if orchestration_enabled {Expert} else {Instant}`（[main.rs#L545-L548](file:///e:/VSCode/FoxIR/src/main.rs#L545-L548)）。
- 共享 runner `with_can_spawn(orchestration_enabled)` + `with_mode(if orchestration_enabled {Expert} else {Instant})`（[main.rs#L609-L613](file:///e:/VSCode/FoxIR/src/main.rs#L609-L613)）。
- runner 把 `self.can_spawn`/`self.mode` 盖章进**每次** Instant 根运行 ctx（[runner.rs#L281-L282](file:///e:/VSCode/FoxIR/src/runner.rs#L281-L282)），Instant 走此共享 runner（[server.rs#L1794-L1806](file:///e:/VSCode/FoxIR/src/server.rs#L1794-L1806)）。

**结论**：只改 `mode==Expert`→`mode==Instant` 是**必要但远不充分**。若不同步处理 `can_spawn`，Instant 根要么 `can_spawn=false`（编排器不构造、执行门直接拒），要么被误标 mode=Expert。

**修正后的「门控翻转」完整定义（P2 必做项）**：
1. `orchestration_allowset`：`Expert&&depth0` → `Instant&&depth0` 返回 `ALL_ORCH`；Expert 返回空（v1.0 已列，✅）。
2. 编排器构造门 `mode==Expert` → `mode==Instant`（v1.0 已列，✅），并叠加 D10 的预筛门。
3. **【新增】main.rs 共享 runner/agent 解耦**：`state.runner` 服务 Instant 路径 → 静态 mode 恒 `Instant`；`with_can_spawn(instant_orchestration_enabled)`（不再绑定 Expert）。
4. **【新增】配置键归属**：见 D11。
5. **【新增】更新过时注释**：[context.rs#L609-L610](file:///e:/VSCode/FoxIR/src/context.rs#L609-L610) 的 "Gated at runtime by `mode == Expert && depth == 0`"。

> **P2 验证项**：确认 `state.runner` 仅服务 Instant 分支、Expert 由 `managed_runner` 独立服务（[server.rs#L1788](file:///e:/VSCode/FoxIR/src/server.rs#L1788)）。若成立，则 main.rs 共享 agent 静态 mode 塌缩为 `Instant`，当前「orchestration_enabled 时把 state.runner 标成 Expert」属可清理的历史耦合。

### 2.2 〔D8′ · 替代 v1.0 D8〕worker mode 保持 Expert —— 撤销翻转

**问题（H1）**：v1.0 D8 要求把 worker mode 由 Expert 改 Instant，落点写的是 [orchestration.rs#L405-L410](file:///e:/VSCode/FoxIR/src/agent/orchestration.rs#L405-L410)。但那只是 builder 的**静态** mode；worker 的**运行时** ctx.mode 由 `InvocationContext::subagent_child` 独立硬编码为 Expert：

- [context.rs#L410](file:///e:/VSCode/FoxIR/src/context.rs#L410) `ctx.mode = AgentMode::Expert;`
- [context.rs#L411](file:///e:/VSCode/FoxIR/src/context.rs#L411) `debug_assert_eq!(ctx.mode, Expert, "§7.4.1 child mode must be Expert")` —— 改成 Instant 会让 **debug 构建直接 panic**。
- [context.rs#L764](file:///e:/VSCode/FoxIR/src/context.rs#L764) 测试 `subagent_child_keeps_skills_disabled_read_only` 断言 child mode==Expert。

而真正决定投递的是**运行时 ctx.mode**（v1.0 自己的测试 `gate_instrument_delivery_reads_ctx_not_agent_static_mode` [llm_agent.rs#L3474-L3485](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L3474-L3485) 已证明这点）。故 D8 指向的落点既漏改运行时硬编码，又会触发 panic。

**决策 D8′（推荐采纳，撤销 D8）**：worker mode **保持 Expert，不翻转**。
- **理由**：`depth=1` 已保证 `orchestration_allowset(_, 1)` 恒空（[llm_agent.rs#L267](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L267)），`.without_skills()` 已禁用 skill 注入；翻转对安全/功能**零收益**，却要动 3 处 + 放宽 debug_assert，属 YAGNI。v1.0 §6.2 自己也承认「depth=1 时 allowset 恒空，mode 取值不影响安全」。
- **动作**：无代码改动；在 v1.0 标注「D8 已撤销，见加固版 D8′」。
- **后续（可选，非本次范围）**：若追求语义整洁，另起任务引入 `AgentMode::Worker`（v1.0 §6.2 备选），届时统一处理所有 mode 匹配分支。

### 2.3 〔D10 · 新增，解 H6〕投递门与路由门合一，兑现「简单任务零开销」

**问题（H6）**：翻转后 `orchestration_allowset(Instant,0)=ALL_ORCH`，意味着**每个** Instant 运行（含 trivial 闲聊）都会把 7 个编排工具 schema 投递给模型，并为每次运行构造+注册 Orchestrator。这与 v1.0 **验收标准 #2「简单任务零额外开销」** 及 **Risk B「Instant 失去永远快/省」** 直接冲突。根因：v1.0 把「投递门（allowset）」与「路由门（是否扇出，D2/D9）」当作两件事，但前者才是 token 开销来源。

**决策 D10（推荐采纳）**：投递门由路由门的**规则预筛**（D2 第一层，零 LLM 开销）驱动。
- Instant 根运行入口先跑规则预筛（多目标/多数据源/「分别·并行·各自」措辞等信号）。
- **命中** → 打开 allowset（投递 7 工具）+ 构造 Orchestrator + 进入 D2 第二层 LLM 拆解判断。
- **未命中** → allowset 为空、不构造 Orchestrator、直接走原 Instant 主 Loop，**字面零编排 token 开销**。
- **实现提示（细节留 writing-plans）**：预筛结果作为一个 `orchestration_candidate: bool` 信号，在 `LlmAgent::run` 组装工具列表前算出（用户消息在 run 入口即可得），据此决定 `orchestration_allowset` 是否放行 + 是否执行 [llm_agent.rs#L1399](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L1399) 的构造分支。
- **正交性保持**：能力路由（选 skill/工具，v1.0 §7.1）仍独立且先于拆解判断；D10 只把「编排工具是否投递」这一件事也挂到预筛信号上，不改变「skill 触发 ≠ 拆解触发」的不变量。

### 2.4 〔D11 · 新增，解 H2 配置面〕编排配置从 `modes.expert.*` 迁至 `modes.instant.*`

**问题**：整套编排配置现居 `ExpertModeConfig`（[config.rs#L174-L203](file:///e:/VSCode/FoxIR/src/config.rs#L174-L203)）：`orchestration`（门开关）、`max_depth`、`max_concurrent_subagents`、`default_timeout_secs`、`max_tokens_per_run`、`max_total_tokens`、`orchestration_template`。重构后编排归 Instant，`modes.expert.orchestration` 控制 Instant 属语义倒置。已存在 `ModesConfig.instant: InstantModeConfig`（[config.rs#L155-L162](file:///e:/VSCode/FoxIR/src/config.rs#L155-L162)）可承接。

**决策 D11（推荐采纳）**：
1. 将编排相关字段迁至 `InstantModeConfig`：`orchestration`、`max_depth`、`max_concurrent_subagents`、`default_timeout_secs`、`max_tokens_per_run`、`max_total_tokens`。
2. **向后兼容**：读取时若 `modes.instant.orchestration` 缺省，回退读旧 `modes.expert.orchestration`，避免破坏现有 `config.toml`。
3. **不迁移** `orchestration_template` + `OrchestrationTemplate`：它们只喂 Expert 模板收集（`run_template_collect`），P1 剥离后成死代码 → 见 H7，直接删除。
4. `OrchestrationLimits` 的派生来源由 `ExpertModeConfig` 改 `InstantModeConfig`（[config.rs#L232-L243](file:///e:/VSCode/FoxIR/src/config.rs#L232-L243)），并移除 `template` 字段。

---

## 3. 修正后的完整改动清单（覆盖 v1.0 §6 遗漏，按 P1–P8）

### P1 · Expert 剥离（补全 H3/H4/H7 连带范围）

| 文件 | 精确目标 |
|---|---|
| [runner.rs](file:///e:/VSCode/FoxIR/src/managed/runner.rs) | 删 `ParallelOutcome` 枚举（L206-L218）、`bounded_parallel_collect`（L221-L237）、并行分支（L876-L1078）、`audit_subagent_aggregate` 调用点（L968） |
| [manager.rs](file:///e:/VSCode/FoxIR/src/managed/manager.rs) | 删 `ManagerPlan.parallel_subtasks`（L42-L44）、`PendingPlan.parallel_subtasks`（L86-L87）及 clone 链（L99/L112）、解析（L387/L442-L446/L481）、3 个测试（L637/L640-L650/L656-L661）；`ParallelSubtask` 类型若仅此处用则一并删 |
| [parallel.rs](file:///e:/VSCode/FoxIR/src/managed/parallel.rs) | **整文件废弃**：`run_template_collect`、`run_loop_collect`、`ParallelEnv` 及其测试（L259） |
| [task_contract.rs](file:///e:/VSCode/FoxIR/src/managed/task_contract.rs) | 删 `orchestrator_plan` 中 `parallel_subtasks` 持久化 + 测试（L718-L722） |
| [orch_selftest.rs](file:///e:/VSCode/FoxIR/src/orch_selftest.rs) | **整文件废弃**（模板收集自测，依赖 `ParallelEnv`/`run_template_collect`/`run_loop_collect`，L9-L10/L25/L54/L72）+ 移除 mod 声明 |
| [phase0_acceptance.rs](file:///e:/VSCode/FoxIR/src/phase0_acceptance.rs) | 删/迁 3 个测试：`phase0_template_collect_dispatches_sequential_and_parallel`（L1070）、`phase0_run_loop_collect_produces_bounded_unique_rounds`（L1122）、`phase0_plan_bidirectional_sync_seed_and_persist`（L1170）；及 L1175-L1178 的 `parallel_subtasks` seed |
| [config.rs](file:///e:/VSCode/FoxIR/src/config.rs) | **〔H7〕** 删 `orchestration_template` 字段（L193-L195）、`OrchestrationTemplate` 枚举（L206-L230）、`default_orchestration_template`（L230）、`OrchestrationLimits.template`（L241-L242） |

> **〔H4〕Risk D 已判定**：`parallel_subtasks` 是 `#[serde(default)]`（[manager.rs#L43](file:///e:/VSCode/FoxIR/src/managed/manager.rs#L43)），删字段对旧 durable contract **反序列化安全**（旧 JSON 的该键被忽略），在途/续跑任务无需迁移。故 v1.0 Risk D「确认无持久化依赖」结论为：**有依赖但删除安全**。

### P2 · 门控翻转（按 §2.1 完整定义 + D11 配置迁移）
- v1.0 §6.2 原列 4 项 **+ §2.1 新增 3 项（can_spawn 解耦 / 配置键迁移 / 注释更新）**。
- 翻转 [llm_agent.rs#L3415-L3419](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L3415-L3419)、[#L3474-L3485](file:///e:/VSCode/FoxIR/src/agent/llm_agent.rs#L3474-L3485) 及 `gate_open_delivers_all_after_step2a`（L3424-L3433）测试。

### P3 · Instant 编排器接入
- 编排器运行时模式无关，可直接迁移（v1.0 §3.2 已核）。worker mode 按 **D8′ 保持 Expert，不改**。

### P4 · 权限分档（写需授权 + 串行）
- 落点确认：`SpawnSubagentTool::execute`（[tool/orchestration.rs#L78-L87](file:///e:/VSCode/FoxIR/src/tool/orchestration.rs#L78-L87)）；`write_gate` 串行已实现。

### P5 · Instant 路由门（D2 + D9 + **D10**）
- 路由门内置 LlmAgent 主循环入口（D9，`mode==Instant && depth==0`）。
- **D10 合流**：规则预筛信号同时驱动「allowset 投递 + Orchestrator 构造 + 进入 LLM 拆解判断」，未命中零开销。

### P6 · 能力路由提示词框架（v1.0 §6.5，无修正）

### P7 · 取消纪律继承 + 孤儿修复（v1.0 §7.2，无修正）

### P8 · 测试迁移 + 记忆/文档更新（含 P1 已列测试删除 + §4 新增测试）

---

## 4. 测试策略增补（修正 v1.0 §9）

**需删除（随 P1 剥离）**：orch_selftest.rs 全部、phase0_acceptance.rs 3 个模板/loop/plan-sync 测试、manager.rs 3 个 parallel 解析测试、parallel.rs#L259、task_contract.rs#L718-L722、context.rs 中依赖 `orchestration_template` 的用例（如有）。

**需翻转（随 P2）**：v1.0 §9.1 全部 + `gate_open_delivers_all_after_step2a`。

**〔H6/D10〕新增**：
- 预筛未命中 → `orchestration_allowset` 为空、Orchestrator 未构造、无 7 工具投递（守验收 #2）。
- 预筛命中 → allowset=ALL_ORCH、Orchestrator 构造、进入拆解判断。

**〔D11〕新增**：配置读取——`modes.instant.orchestration` 生效；缺省时回退旧 `modes.expert.orchestration`；旧键不报错。

**〔D8′〕保留**：`subagent_child_keeps_skills_disabled_read_only`（context.rs#L740-L767）**不改**（worker 仍 Expert）。

---

## 5. 验收标准增补（修正 v1.0 §11）

- **#2 收紧（D10）**：Instant 简单任务**预筛未命中 → 编排 allowset 为空、不构造 Orchestrator、投递工具集与重构前逐字节一致**（可断言），字面零编排开销。
- **#7 修正（D8′）**：worker（depth≥1）ctx.mode **保持 Expert**、allowset 恒空；不再要求 worker mode=Instant。
- **新增 #9（D11）**：编排配置由 `modes.instant.*` 驱动；旧 `modes.expert.orchestration` 兼容回退。
- 其余 #1/#3/#4/#5/#6/#8 沿用 v1.0。

---

## 6. 风险更新（修正 v1.0 §8）

| # | 风险 | 状态/缓解 |
|---|---|---|
| D | Expert 剥离是行为回归 | **已判定（H4）**：`parallel_subtasks` 为 `serde(default)`，删除对旧 contract 安全 |
| B | Instant 失去「永远快/省」 | **强化（D10）**：投递门与预筛合流，未命中零 token 开销（原 v1.0 仅「不扇出」，未覆盖工具 schema 常驻开销） |
| **H2** | 门控翻转漏改 `can_spawn`/配置键 → Instant 根拿不到编排能力或误标 Expert | **新增**：§2.1 完整清单 + P2 验证项 |
| **H1** | 按 D8 改 worker mode 触发 `debug_assert` panic | **新增/已消解**：D8′ 撤销翻转 |
| **H7** | 剥离后 `orchestration_template`/`OrchestrationTemplate` 成死代码 | **新增**：P1 一并删除 |
| A/C/E/F/G | 沿用 v1.0 | 无修正 |

---

## 7. 待评审确认项（评审门）

以下 3 项按推荐默认采纳，**请确认或推翻**：

1. **D8′**：worker mode 保持 Expert（撤销 v1.0 D8）。备选：彻底履行 D8 / 引入 `AgentMode::Worker`。
2. **D10**：投递门与路由门合一（预筛命中才投递 7 工具）。备选：接受常驻 7 工具 / 单一 `escalate` 工具。
3. **D11**：编排配置迁 `modes.instant.*` 兼容旧键。备选：保留 `modes.expert.*` 键名 / 新建 `modes.orchestration.*` 中立段。

确认后转入 `writing-plans` skill，把 P1–P8 拆成可执行的 TDD 任务清单（每步含文件路径、验证命令）。
