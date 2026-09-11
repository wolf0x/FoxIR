# FoxIR 多代理编排 · 软件设计文档（SDD）

| 项 | 内容 |
|---|---|
| 文档 | 多代理 / 子代理 / 编排（Orchestration）子系统设计 |
| 版本 | v1.0（阶段性简单路线） |
| 状态 | 待评审 |
| 适用工程 | FoxIR（Rust 单二进制、Axum 网关、Windows 本地） |
| 本期范围 | 仅 Expert 模式开放自主派生子代理；Instant 模式行为零改动 |

---

## 1. 概述

为 FoxIR 引入多代理编排能力：在 **Expert 模式**下，主代理升级为 **Orchestrator（编排者）**，可规划任务、动态派生临时子代理（sub-agent）、并发调度并聚合结果；在 **Instant 模式**下**保持现状单主代理行为不变**。整套能力构建在 FoxIR 既有的 `Runner` / `LlmAgent` / `ManagedRunner`（Manager–Executor–Auditor）/ 权限 / 记忆 / 检查点 / 证据台账 之上，属"泛化 + 接线"，非新造运行时。

本期遵循**简单路线**：委派能力只开放两态——`off`（Instant 恒定）与 `full`（Expert）。曾评估的 Instant `readonly_only`（主代理现场派只读子代理）作为**未来扩展点**，本期不实现（见 §16）。

---

## 2. 目标与非目标

**目标**
- G1：Expert 主代理可自主 `spawn_subagent` 派生多个临时子代理并行干活并聚合。
- G2：子代理是临时执行单元，**不创建新的用户会话、不新增主会话线程**。
- G3：Instant 模式在功能/配置不变时，与改造前**逐行为等价**（零回归）。
- G4：保持单二进制、单一 SPA HTML、纯 Windows、单进程 Tokio。
- G5：复用现有权限/取消/恢复/台账/预算原语，最小新增依赖。

**非目标（本期）**
- N1：Instant 模式使用子代理（`readonly_only`）。
- N2：代理蜂群（swarm，多假设竞争/投票）——列为后续。
- N3：重写现有 Expert（Manager–Executor–Auditor）语义；仅在其上扩展。
- N4：跨主机分布式子代理。

---

## 3. 设计约束（硬）

| 约束 | 含义 | 满足方式 |
|---|---|---|
| 单一主会话线程 | 任一瞬间仅一个主会话轮次为 owner | 子代理在当前主轮次内以 `tokio::spawn` 派生，父 Orchestrator 任务仍是 owner，事件经父 `tx` 回流 |
| 单一 SPA HTML | 不新增页面/文件 | 同一 `index.html` 内按模式条件渲染 Expert 视图 |
| 纯 Windows | 不依赖 fork/进程并行 | 全 `tokio` async；`JoinSet` 并发；JSONL 每 run 独立文件避免并发写 |
| 单二进制 | 不引入重外部服务 | 不新增 `tokio-util`/`dashmap` 依赖（见 §7.7、§7.9 用现状原语） |
| Instant 零改动 | 见 G3 | 三态门控 + 默认 `off` + 回归门禁（§11） |

---

## 4. 术语

- **Orchestrator / Manager**：Expert 主代理的编排角色（复用/扩展现有 `managed` 模块的 Manager）。
- **子代理 / worker / SubAgent**：被临时派生的执行单元，独立上下文、独立事件流，只回结构化摘要。
- **委派原语（Spawner）**：创建并运行子代理的底层能力，`full` 档唯一入口。
- **能力档位（orchestration level）**：`off | readonly_only | full`；本期仅用 `off`（Instant）与 `full`（Expert）。
- **run_id**：子代理运行标识，直接复用现有 `invocation_id`（UUID），不新造类型。
- **root_invocation_id**：同一主轮次下所有子代理共享的根标识，用于级联取消与预算聚合。

---

## 5. 现状基线

**已具备（复用）**
- 流式事件与溯源：`agent/event.rs` 的 `AgentEvent` + `EventMeta{author, invocation_id}`；`LlmAgent::run` 内 mpsc 事件通道 `tx`。
- 隔离子会话范式：调度器（`scheduler.rs`）以 `cron-` 前缀 `session_id` + `tokio::spawn` 独立执行。
- 主/子会话区分：`is_main_session`（依 `session_id` 前缀判定）。
- 工具向前端推进度：`ToolContext::report_progress` → `progress` 事件。
- 权限预授权：`InvocationContext::preauth_profile` + `managed/permission_profile`。
- 并行只读工具：`ToolExecutionStrategy::{Sequential,Parallel,Auto}` + `Tool::is_read_only`。
- Expert 三角色流水线：`ManagedRunner`（Manager 规划 → 单 Executor 执行 → Auditor 核验，**逐轮串行**）+ `TaskContract`（SQLite 持久化）。
- 恢复：`checkpoint.rs::TaskCheckpointer`（SQLite）；`ConversationLogger`（per-session JSONL）。
- 台账：`evidence` 工具、`ir_case`、`ir_report`。
- 插话：`interject.rs`（按 `session_id` 路由，pending 队列 + insert-now 通道）。

**缺口（本设计补齐）**
- 主代理无法在会话内自主生成/委派/编排任意子代理。
- 无"子代理即工具"原语；无任务谱系（parent/depth/root）；无并行代理团队 + 聚合。
- `budget_sink` 为 Dashboard 单例快照，多代理并发会互相覆盖。

> 重要更正记录：早期探索中一度误判"子代理骨架（blackboard/delegation_callback/create_sub_context/Delegation 事件）已存在"，经干净复核 `context.rs` / `agent/event.rs` 确认**这些均不存在**。本 SDD 以复核后事实为准。

---

## 6. 总体架构

### 6.1 模式即能力边界（本期两态）

| 维度 | Instant（`off`，不变） | Expert（`full`） |
|---|---|---|
| 主代理角色 | 单一 `LlmAgent` | Manager / Orchestrator |
| 编排工具投递 | **全部剔除** | 全部 6 工具可见（仅 depth=0） |
| 子代理 | 禁止 | 动态派生，depth=1 |
| 递归深度 | 0 | 1（子代理不可再派生） |
| 治理栈 | 无 | TaskContract + Auditor + 持久计划 |
| 执行 | `run_single`（现状） | `run_manager`（并行 worker 池 + 聚合） |

### 6.2 分层

```
上下文/谱系层  InvocationContext{mode, depth, can_spawn, root/parent_invocation_id, session_kind}
     │           （判定在此层，透传至 ToolContext 层执行门控）
编排工具层      spawn/wait/list/cancel/get_result/update_plan（两级门控，仅 Expert 可见）
     │
委派原语层      AgentSpawner（tokio::spawn 子 LlmAgent，事件注入父 tx，回 SubAgentResult）
     │
编排治理层      Orchestrator（扩展 ManagedRunner）：规划→并发派生→聚合→重规划
     │
复用底座        permission / policy / memory / checkpoint(SQLite) / evidence / ir_case / ir_report / interject / ConversationLogger
     │
通信/前端       单一 WS 通道（既有 AgentEvent 扩展变体）→ 单 SPA Expert 视图条件渲染
```

### 6.3 运行时数据流

```
用户 → SPA(单 HTML) → Axum WS → Server 分派
  ├─ Instant: Runner::run → LlmAgent(单) → tx → WS          （路径与现状一致）
  └─ Expert:  ManagedRunner(Orchestrator)
        ├ 规划 → 写 TaskContract(SQLite)
        ├ spawn_subagent ×N → 每 worker: 子 LlmAgent
        │     ├ 子 ctx{mode=Expert, depth=1, can_spawn=false, session=sub-<uuid>}
        │     ├ 子事件 author=role, invocation_id=run_id → 注入父 tx
        │     ├ 独立 EventStream / per-run 预算 / checkpoint(sub-*) / evidence 引用
        │     └ 只回 SubAgentResult
        ├ wait_any / cancel / get_result
        ├ Auditor 核验（Manager 层 + 子代理层各自独立）
        └ 聚合 → 最终回答 → tx → WS
```

---

## 7. 详细设计

### 7.1 上下文与谱系

`InvocationContext`（`context.rs`）新增字段，**默认值即最保守（Instant）语义**：
```rust
pub mode: AgentMode,                    // 默认 Instant
pub depth: u8,                          // 默认 0
pub can_spawn: bool,                    // 默认 false
pub root_invocation_id: Option<String>, // 主轮所有子代理共享；Instant/顶层为 None 或自身
pub parent_invocation_id: Option<String>;
pub session_kind: SessionKind,          // Main | Cron | SubAgent（前缀推断，仅审计标识）
```
- `SessionKind::from_session_id`：`sub-`→SubAgent，`cron-`→Cron，否则 Main。
- **depth 权威来源为显式 `parent.depth + 1`**，前缀仅用于人读审计。
- `can_spawn = (mode==Expert) && (depth==0) && level_full`。

**门控终点是 `ToolContext`（关键）**：工具执行签名保持 `execute(args, ctx: &ToolContext)`，其**无法访问** `InvocationContext`。因此 `LlmAgent::run` 构建 `ToolContext` 处（`agent/llm_agent.rs` 约 2521 行）必须把 `mode / depth / can_spawn / root_invocation_id / orchestration_allowset / spawner: Arc<dyn AgentSpawner> / event_tx` 透传进 `ToolContext`（挂 `base` 或新增字段，默认 `None`/空集，不影响既有构造与 `Clone`）。

### 7.2 编排工具集（仅 Expert 注册可见）

| 工具 | 语义 | 阻塞 | 关键参数 |
|---|---|---|---|
| `spawn_subagent` | 派生一个子代理，立即返回 `run_id` | 否（异步） | `role, prompt, system_prompt?, tools_allowlist?, model?, timeout?, max_tokens?` |
| `wait_subagent` | 等待指定或全部子代理完成 | 是 | `run_id \| all, timeout?` |
| `list_subagents` | 列出当前 run 池状态 | 否 | — |
| `cancel_subagent` | 取消指定子代理 | 否 | `run_id` |
| `get_subagent_result` | 取某子代理结构化结果 | 否 | `run_id` |
| `update_plan` | 更新计划/任务树 | 否 | `plan_json` |

**结果契约 `SubAgentResult`（复用现有台账，只存引用）**：
```rust
struct SubAgentResult {
    run_id, role, summary,            // summary 是唯一回传给 Orchestrator 的正文本
    confidence,                        // 枚举：High/Medium/Low
    token_usage,                       // 读自 per-run 预算 sink
    evidence_refs:  Vec<EvidenceId>,   // 指向现有 evidence 台账
    artifact_refs:  Vec<ArtifactId>,   // 指向 ir_case / ir_report 产物
    case_ref:       Option<CaseId>,
    status,                            // ok | failed | cancelled | timeout
}
```

### 7.3 两级门控

**投递期**（`agent/llm_agent.rs` 构建发给模型的工具定义处）：
```rust
const ALL_ORCH: [&str;6] = ["spawn_subagent","wait_subagent","list_subagents",
    "cancel_subagent","get_subagent_result","update_plan"];
// 本期真值：
//   Instant(任意 depth)      → allow = ∅        （无条件剔除）
//   Expert depth==0 && full  → allow = ALL_ORCH
//   Expert depth>=1 / 子代理  → allow = ∅
let allow = orchestration_allowset(ctx);
defs.retain(|d| !ALL_ORCH.contains(&d.function.name.as_str()) || allow.contains(&d.function.name.as_str()));
```
`ALL_ORCH` 为精确名字集合，与现有 Instant/业务工具名**零交集**（防呆断言覆盖）。

**执行期**（每个编排工具 `execute` 首行，二次门控）：
```rust
if !tool_ctx.can_spawn { return Err(NotAvailableInMode); }   // Instant / 子代理直达调用被拒
if tool_ctx.depth >= 1 { return Err(DepthLimitExceeded); }   // 子代理不可再派生
```
"不可见即不可用"（投递）+"可见也不可调"（执行），两层皆过才执行——堵住"绕过模型直接构造 tool_call"的窗口。

### 7.4 委派原语 `AgentSpawner`

```rust
#[async_trait]
pub trait AgentSpawner: Send + Sync {
    async fn spawn(&self, spec: SubAgentSpec,
        event_tx: mpsc::Sender<AgentResult<AgentEvent>>,
        stop: Arc<AtomicBool>) -> AgentResult<SubAgentResult>;
}
```
- 默认实现 `LlmAgentSpawner` 持有 `provider / tools / workspace_dir / checkpointer / budget_sink`（即 `LlmAgent` 构建期已有的一切）。
- 内部：构造子 `InvocationContext{mode=Expert, depth=parent+1, can_spawn=false, session_id="sub-<uuid>", root_invocation_id=父.root或父.invocation}` → `tokio::spawn` 起子 `LlmAgent::run` → 消费子事件流，逐条**覆写 `meta.author=role`、保留子 `invocation_id=run_id`** 后 `send` 进父 `event_tx` → 聚合 `TextDelta` + 收尾 `SubAgentResult` 返回。
- `stop` 由父传入，子循环每轮 `is_ended()` 检查 → 级联取消。
- **单一主会话合规性**：worker 是 Orchestrator 任务派生的 tokio 任务，不新建用户会话/主线程。

### 7.5 Expert Orchestrator（扩展 `ManagedRunner`，不造 ManagerRuntime）

在 `managed/runner.rs::ManagedRunner` 增量扩展，保留 Manager 规划循环 + Auditor + TaskContract：
```rust
plan: Option<OrchestrationPlan>,
subagents: HashMap<String /*run_id*/, SubAgentHandle>,
mode: AgentMode,
```
```rust
struct SubAgentHandle {
    run_id, role,
    join:  JoinHandle<AgentResult<SubAgentResult>>,
    stop:  Arc<AtomicBool>,          // 见 §7.9，非 CancellationToken
    event_rx: mpsc::Receiver<AgentEvent>, // 已转发进父 tx，此处仅按需保留
}
```
**Manager 主循环**：接收目标 → 规划（写 TaskContract）→ 取就绪任务 → `spawn_subagent` 并发派生（受 `max_concurrent_subagents`）→ `wait_any`（`select!` 挂 stop + 插话信号）→ 标记完成 → 未完成则重规划 → 聚合最终回答。
- Instant 分支 = `run_single`（现状单 Executor，**不改**）。
- Expert depth≥1 = 退化单 Executor（仅 system prompt / allowlist 不同）。
- `Auditor` / `TaskContract` 语义不变，Manager 层与子代理层各跑各的。
- 工作流模板（§10）由 Manager 内部按需选择，非用户直接操作。

### 7.6 子代理上下文与隔离

- 独立 `EventStream`：子代理自带 `tx`，事件带 `author` 供前端分泳道。
- 独立 Context Budget：per-run sink（§7.7）。
- 独立 Checkpoint：走 `checkpoint.rs`，`id = session_id = "sub-<uuid>"`。
- **不自动继承主会话历史 / `[会话回顾]` / memory recall**；需主会话某段由 Orchestrator 在 `spawn` 时拼进 `prompt`。
- 跨代理共享走 `evidence_refs`（台账 ID 引用），真实内容留在现有证据台账，避免传历史导致上下文膨胀。

### 7.7 预算 per-run sink（不新增依赖）

现状 `budget_sink: Arc<std::sync::Mutex<Option<BudgetReport>>>`（Dashboard 单例快照）→ 改为按 run 归集：
```rust
pub trait BudgetSink: Send + Sync {
    fn record(&self, run_id: &str, usage: TokenUsage);
    fn snapshot(&self, run_id: &str) -> BudgetSnapshot;   // 保留旧读语义
    fn aggregate(&self) -> AggregateBudget;
}
pub struct DashboardBudgetSink {
    per_run:    std::sync::Mutex<HashMap<String, BudgetSnapshot>>, // 无 dashmap
    aggregate:  AtomicU64,
}
```
- 每个 Executor 持 `run_id + Arc<DashboardBudgetSink>`；Instant 只有一个 run → `per_run[主invocation]` 即旧快照语义，**保留旧 `snapshot()` 接口**，Dashboard 不破。
- WS 推 `budget_update{run_id}`：主视图读 `aggregate`，子卡片读 `per_run[run_id]`。
- Orchestrator 自检：`aggregate > max_total_tokens` → 级联取消所有子代理。

### 7.8 持久化与恢复真源（SQLite 唯一权威）

- **计划真源 = `TaskContract`**（SQLite，已支持 in-flight 计划持久化）；崩溃恢复复用 `TaskCheckpointer`。
- `plan.json` **仅作导出产物**（前端/审计展示），不做真源，避免双恢复源。
- 子代理 `events.jsonl` **复用 `ConversationLogger`**（`session_id="sub-{run_id}"`），天然满足"每 run 独立文件、Windows 下无并发写冲突"。
- 新增落盘仅：`workspace/subagents/plans/{root_id}/plan.json`（导出）；`runs/` 由 ConversationLogger + checkpoint 覆盖，不重复造。

### 7.9 取消与插话（现状原语，不引入 CancellationToken）

- **取消**：一个 `root_invocation_id` 关联**一个** `Arc<AtomicBool>`；Orchestrator 与所有 worker 各持 `clone()`。STOP → `store(true)`；worker 靠 `is_ended()` 轮询退出，`JoinHandle::abort()` 兜底。扩展现有 `server.rs::expert_tasks`（"每会话一 flag"）为"每 root_run 一 flag + 下传"。**不引入 `CancellationToken`，不改 Instant 的 stop 路径**。
- **插话**：`interject.rs` 按 `session_id` 路由，主会话与 `sub-` 天然不通 → 插话默认进 Orchestrator。**补坑**：`wait_subagent` 的 `select!` 必须挂 `drain_insert` 信号与 stop，避免 Orchestrator 阻塞在 wait 时吞插话。Instant 主循环 `drain_insert` 逻辑不动。

### 7.10 权限与安全

- **allowlist 只收缩**：子代理 `tools_allowlist ⊆ 父授权集`，spawn 期门控，继承 `permission` 分类 + `preauth_profile`，绝不放大。
- **深度硬上限**：`depth≥1` 执行期拒绝任何编排工具；`can_spawn_subagent=false`。
- **意图策略继承**：子代理内 `shell_exec/winrm/linux_ssh` 命令仍过 `policy` 引擎（Block/Audit/Pass），Block 类无视授权硬拦截。
- **成本护栏**：`max_concurrent_subagents` + `max_spawn_depth` + `max_tokens_per_run` + `max_total_tokens` + 超时 + 复用 `rabbit_hole`。

---

## 8. 前端与通信协议（单 SPA）

- Instant：仅 Chat（默认，零新增，不产出任何编排事件）。
- Expert 条件渲染（同一 `index.html`，不新增文件）：
  - **Plan 面板**：任务树 / 依赖 / 状态。
  - **Agent Cards**：角色 · 模型 · token（`per_run`）· 状态 · 取消按钮。
  - **Task Queue**：pending / claimed / completed / failed。
  - **日志抽屉**：点击子代理看 `events.jsonl` 摘要。
  - 聚合回答摘要 + `evidence` 引用可点击跳现有台账。
  - 模式切换：Instant 默认，Expert 显式（可提示升级，不自动切）。

**`AgentEvent` 扩展变体**（`agent/event.rs`，加 `#[serde(rename=...)]`；仅 Expert 产出）：
`plan_created / plan_updated / subagent_spawned / subagent_event / subagent_completed / subagent_failed / subagent_cancelled / budget_update`。

WS 消息示例：
```json
{"type":"subagent_spawned","run_id":"sub-…","role":"log_analyst","author":"orchestrator"}
{"type":"subagent_event","run_id":"sub-…","event":{ /* 原 AgentEvent，author=role */ }}
{"type":"subagent_completed","run_id":"sub-…","summary":"…","confidence":"High"}
{"type":"budget_update","run_id":"sub-…","tokens":12345}
```

---

## 9. 配置规格（归并现有 `expert_*`）

```toml
[modes.instant]
orchestration = "off"        # 本期恒定 off；保证 Instant 零改动

[modes.expert]
orchestration = "full"
manager_model = "..."        # 映射现 expert_role_models(Manager)
worker_model  = "..."        # 便宜/专用（RoleModelsConfig）
max_concurrent_subagents = 4
max_spawn_depth = 1          # 代码再夹一层硬上限
default_timeout_secs = 300   # 复用 expert_tool_timeout_secs
max_rounds = 12              # 复用 expert_max_managed_rounds
max_tokens_per_run = 400000
max_total_tokens = 2000000
```
`server.rs` 现有 `expert_max_*`（AtomicUsize）读取路径归并进此段，向后兼容。

---

## 10. 工作流模板（Orchestrator 内部库）

以 ADK-Rust 的 workflow 组合为对齐蓝本（可核实的公开 Rust 实现）：
- **Sequential**：研究 → 分析 → 总结（顺序 `await`）。
- **Parallel**：同时做日志 / 持久化 / 网络多角度检查（`JoinSet` + 聚合）。
- **Loop**：迭代深挖直到条件满足（复用 `ManagedRunner` 轮次 + `update_plan`）。

由 Orchestrator 按任务自动选择，用户不直接操作。

---

## 11. Instant 零改动保证与回归门禁

**结构保证**
1. Instant 默认档 `off`：编排工具在 `off` 下**不注册进注册表**（总开关关闭），或注册但投递期 `allow=∅` + 执行期 `NotAvailableInMode` 双拒。
2. `InvocationContext` 新字段默认值即 Instant 语义；Instant 调用方不传新参数。
3. Instant 分支 = `run_single`，与现状同一代码路径。
4. 预算 sink 单 run 退化等价旧快照；`snapshot()` 读接口保留。
5. 取消/恢复/台账/插话的 Instant 路径不改。

**回归门禁（纳入 CI/验收）**
- **G-instant-tools**：Instant 会话断言模型收到的 tool defs **不含任何 `ALL_ORCH` 名**。
- **G-instant-diff**：同一 Instant 请求，改造前后在"工具集合、系统提示块、取消语义、预算快照、恢复行为"逐项 diff 为空。
- **G-name-disjoint**：断言 `ALL_ORCH` ∩（现有 Instant/业务工具名）= ∅。
- **G-gate-truth**：`orchestration_allowset()` 真值表单测（含 Instant 任意 depth→∅、depth≥1→∅）。

---

## 12. 测试策略

| 层级 | 用例 |
|---|---|
| 单元 | `orchestration_allowset` 真值；`SessionKind::from_session_id`；执行期 `can_spawn/depth` 拒绝；allowlist 只收缩 |
| 集成 | 单 `spawn_subagent` 端到端（子代理跑只读工具链 → 回 `SubAgentResult`）；`wait_all` 聚合 |
| Expert E2E | "分别查异常进程 + 可疑外联" → 并行两 worker → 聚合；预算超限级联取消；STOP 级联 |
| 回归 | §11 四项 Instant 门禁；现有 Instant/Expert 全量集成与单测不破 |
| 前端 | Instant 仅 Chat 且收不到编排事件；Expert 视图渲染/取消/日志抽屉 |

---

## 13. 实施计划（每步可独立合并，含 Instant 门禁）

**Step 1｜模式与上下文骨架（最小侵入）**
- `AgentMode / SessionKind / InvocationContext` 新字段（默认 Instant）+ 透传进 `ToolContext`。
- `core_definitions` 按 `orchestration_allowset` 精确 retain 过滤；`[modes.*]` 配置解析。
- 6 编排工具注册空壳，`execute` 首行 `NotAvailableInMode`。
- 门禁：§11 全绿（Instant 零 diff）。

**Step 2｜委派原语 + Expert Orchestrator 运行时**
- `AgentSpawner` + `LlmAgentSpawner`（子事件注入父 `tx`）。
- `ManagedRunner::run_manager` 循环 + 6 工具真实实现（含异步 `spawn` + `wait_any`）。
- `budget_sink` 改 per-run（保留旧读接口）；`plan`→TaskContract；子代理 `checkpoint(sub-*)` + `ConversationLogger("sub-{run_id}")`。
- 取消 `Arc<AtomicBool>` 树 + `wait_subagent` `select!` 挂插话/stop。
- 门禁：G2（Expert E2E + Instant 零 diff）。

**Step 3｜前端与聚合**
- SPA Expert 视图（Plan / Agent Cards / Task Queue / 预算按 run_id / 日志抽屉）+ 新 WS 事件 + 聚合摘要与证据引用跳转 + 模式切换。
- 工作流模板库（Sequential/Parallel/Loop）接入 Orchestrator。
- 门禁：G3（Instant 仍仅 Chat；切换/降级可用）。

**文件清单（增量）**
- 新增：`src/agent/spawner.rs`、`src/tool/orchestration.rs`、`src/agent/workflow.rs`、`src/budget/sink.rs`（或并入 `context_arbiter.rs`）。
- 修改：`src/context.rs`、`src/agent/llm_agent.rs`、`src/runner.rs`、`src/managed/runner.rs`、`src/tool/mod.rs`、`src/config.rs`、`src/server.rs`、`src/agent/event.rs`、`static/index.html`、`AGENTS.md`/`README.md`。

---

## 14. 错误处理与边界

| 场景 | 处理 |
|---|---|
| Instant/子代理误调编排工具 | 执行期 `NotAvailableInMode` / `DepthLimitExceeded` |
| 子代理 LLM 空响应 | 复用现有"空响应兜底摘要"规范，`status=failed` + 空 summary |
| 子代理超时/卡死 | 工具级 timeout + liveness watchdog（沿用 `TimeoutStage`）；超时置 `timeout` 并 abort |
| 预算超限 | `max_tokens_per_run`（单）→ 终止该 worker；`max_total_tokens`（总）→ 级联取消 |
| 取消传播 | AtomicBool 轮询，退出粒度为"一轮工具"（非即时），超时兜底 |
| 子代理权限请求 | 并行 worker 强制只读或预授权；写/执行类串行逐个授权，避免弹窗风暴 |
| 崩溃恢复 | TaskContract + `sub-*` checkpoint 重放 |

---

## 15. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 上下文丢失（子代理只见 `prompt`） | Orchestrator 显式蒸馏必要上下文进 `prompt`；跨代理共享走 `evidence_refs`（不共享历史） |
| 委派失控/套娃 | depth=1 硬上限 + `can_spawn=false` + 角色/工具白名单 + 默认 `off` |
| Token/时延放大 | 并发+深度+单run+总预算四重上限 + 超时 |
| 权限放大 | allowlist 只收缩 + 意图策略继承 + spawn 期门控 |
| 上下文污染主会话 | 子代理只回 `summary`，完整日志入审计目录 |
| 改 `budget_sink` 波及 Instant 调用点 | 同改 `runner.rs`/`server.rs` + 保留 `snapshot()` + G-instant-diff 锁死 |
| 双编排逻辑漂移 | 不造 `ManagerRuntime`，仅扩展 `ManagedRunner` |
| 与参考项目"逐行依据"错觉 | 概念对标与硬主干分离（§17） |

---

## 16. 未来扩展点（本期不实现）

- **E1｜Instant `readonly_only` 档**：允许 Instant 主代理现场派**只读**子代理（`LIGHT_SPAWN` 工具子集 + 强制 `read_only=true` + 轻并发/预算护栏），不进入 Expert 治理栈。门控真值表与工具子集划分已在本 SDD 预留（§7.2/§7.3 可扩展），届时按 opt-in 开放，默认仍 `off`。
- **E2｜代理蜂群（swarm）**：多假设竞争 / 投票 / 择优聚合。
- **E3｜跨主机子代理**（WinRM/SSH 目标上的分布式采集 worker）。

---

## 17. 参考对标（诚实标注可核实性）

- **硬主干（可核实）**：ADK-Rust `adk-agent` crate 的 `Agent` / `LlmAgent` / workflow 组合（`SequentialAgent` / `ParallelAgent` / `LoopAgent`）与 `sub_agents`/`transfer` 模式；FoxIR 现有 `Agent`/`InvocationContext`/`ToolRegistry` 本就仿 ADK-Rust，对齐成本低。
- **概念对标（不声称逐行依据）**：ZeroClaw 的 spawn/delegate 二分、MicroClaw 的 `run_id` 生命周期、thClaws 分层编排，仅作"业界存在这些模式"的旁证。
- **不采信**：iconclaw / temm1e 未定位到明确的 Rust 多代理实现。
- 本文正文不出现"参考 X 项目 Y 实现"这类不可核实表述；落地硬依据 = ADK-Rust 模式 + FoxIR 现有 Expert 骨架。

---

## 18. 结论

本期以最小侵入实现"Expert 可自主派生子代理，Instant 零改动"：
- **Instant 零回归** = 默认 `off` + 投递/执行两级门控 + 新字段默认最保守 + 精确名字过滤 + 单 run 预算退化 + 复用现状取消/恢复/台账原语。
- **Expert** = 扩展 `ManagedRunner` 为 Orchestrator（并行 worker 池 + 聚合），子代理临时、depth-1、权限只收缩、只回结构化摘要、SQLite 为恢复真源。
- 保持单一主会话线程、单一 SPA HTML、单进程 Tokio、单二进制、纯 Windows。
