# SDD — 可插拔记忆后端（MemoryProvider 接入口）

- **状态**: `draft`（待审核，未实施）
- **分支基线**: `codex/multi-session` @ `b542a35`（8d64571 + skill 修复）
- **目标**: 为 FoxIR 开一个协议无关的 `MemoryProvider` 接入口，用 [MCP 优先 + 声明式连接器] 实现"不嵌入具体服务的可插拔"记忆能力；首个非嵌入后端为本地 mem0。
- **红线**: 性能零回归；本地默认；不改动现有 `deep_memory`/SQLite 行为。

---

## 1. 背景与动机

FoxIR 当前的记忆是**硬编码进程内 SQLite**（`crate::memory::MemoryStore`，即 `deep_memory`/`recall_memory` 那套），自动召回以 `[Memory Context]/[Memory Recall]` 注入发生在 prompt 组装时（`src/agent/llm_agent.rs`）。过去接 MemoryHub 的做法是"每个服务嵌入一段代码"，复用性差、耦合度高，且 MemoryHub 已被回退。

目标转向：**FoxIR 核心不认识任何具体记忆服务**。接 mem0、MemoryHub、以及将来任意后端，都只是"一条配置/一个适配器"，**不改 FoxIR 源码、不重编译**。

关键认知：真实记忆系统（mem0）的写路径是**写时决策**——对每条记忆候选做 `ADD / UPDATE / INVALIDATE / NOOP`，更新矛盾事实、去重、避免重复存储。因此接入口的写方法必须是"声明记忆意图并返回写决策"，而非"追加一条记录"。

---

## 2. 设计原则

1. **核心与后端解耦**：FoxIR 只依赖 `MemoryProvider` trait，不 import 任何 mem0/MemoryHub 类型。
2. **无嵌入**：接入任一服务 = 配置条目（MCP server 注册 或 连接器 spec 文件）。不写任何具体服务的业务代码。
3. **决策在后端内部**：ADD/UPDATE/INVALIDATE/NOOP 由各适配器自行实现（mem0 用其 LLM 管线；本地用现有 curator；无语义的后端做最佳努力去重），核心不规定策略。
4. **本地默认、外部可回落**：`active` 未配置或不可达时回落到本地，agent 不中断。
5. **性能零回归**：写异步化、读缓存化（见 §7 硬约束）。

---

## 3. 接入口：`MemoryProvider` trait

协议无关、最小能力集（写 / 召回 / 计数 / 健康）。

```rust
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    fn id(&self) -> &str; // 显示名，如 "local" / "mem0" / "memoryhub"

    /// 语义写入：声明一条记忆候选，后端自行做 ADD/UPDATE/INVALIDATE/NOOP。
    async fn memorize(&self, draft: &MemoryDraft) -> Result<MemoryWriteDecision, String>;

    /// 自动召回（供每轮注入管线调用）。
    async fn recall(&self, query: &str, top_k: usize, scope: &MemoryScope)
        -> Result<Vec<MemoryHit>, String>;

    /// 计数（健康/容量展示）。
    async fn count(&self, scope: &MemoryScope) -> Result<usize, String>;

    /// 健康状态（含"完整语义写策略"vs"仅去重"降级标记）。
    async fn health(&self) -> ProviderHealth;
}
```

### 3.1 写决策

```rust
pub enum MemoryWriteDecision {
    Added      { id: String },
    Updated    { id: String, superseded: Option<String> }, // 合并/改写旧条目
    Invalidated{ id: String },                             // 旧记录因矛盾被作废
    Noop       { matched_id: String },                     // 已存在等价事实，未写
    Error(String),
}
```

- `superseded` 供审计记录"这次改写/作废了哪条旧记录"，保证可追溯。

### 3.2 记忆候选（draft）

```rust
pub struct MemoryDraft<'a> {
    pub user_id: &'a str,
    pub agent_id: &'a str,        // 跨机分域：hostname
    pub session_id: Option<&'a str>,
    pub subject: Option<&'a str>, // 主题/实体，用于 UPDATE 匹配
    pub content: &'a str,         // 要写入的信息（后端可自行 extract）
    pub kind: MemoryKind,         // preference | convention | fact | conclusion ...
    pub source: &'a str,          // 出处（conversation / meta / explicit-remember）
    pub importance: Option<f32>,  // 可选，供本地规则用
}
```

### 3.3 召回命中与健康

```rust
pub struct MemoryHit { pub id: String, pub content: String, pub score: f32, pub ts_ms: i64 }
pub enum ProviderHealth { Ok(HealthInfo), Degraded(String), Down(String) }
pub struct HealthInfo { pub write_policy: WritePolicy, pub count: usize, pub latency_ms: u64 }
pub enum WritePolicy { FullSemantic, BestEffortDedupe } // 能力降级标记
```

---

## 4. 适配器（"不嵌入"的实现载体）

| 适配器 | 传输 | 何时用 | 写策略 |
|---|---|---|---|
| `LocalProvider` | 进程内 SQLite（现有 `MemoryStore`） | 默认/离线兜底 | 用现有 curator：`subject_key` upsert + importance + pin/decay → 已是"更新而非重复" |
| `McpMemoryAdapter` | MCP（复用 `rmcp`，`src/tool/mcp_client.rs`） | 任何有 MCP 的记忆服务（**mem0**） | 委托后端；工具名可配置映射 |
| `RestMemoryAdapter` | 声明式连接器 spec（JSON，URL/方法/鉴权/字段映射） | 仅 REST、无 MCP 的服务（如 MemoryHub） | 按 subject/recency 最佳努力去重 |

- **MCP 工具名映射做成配置**：不同服务工具名不同（mem0 `search_memories`/`add_memory`，别的可能叫 `search`/`insert`），映射写在 config/注册条目里，**不进代码**。
- 无 MCP 的服务建议：要么用 `RestMemoryAdapter` + connector spec，要么给它套一个极小的 "memory-to-MCP sidecar"（独立小进程，与 FoxIR 二进制无关）。

---

## 5. 配置视图

```toml
[memory]
active  = "local"                 # local | mcp:<server-name> | connector:<file>
fallback = "local"                # 外部不可用时回落到本地
# MCP server 在 mcp_servers.json 注册（现有机制）；
# connector spec 文件放 connectors/ 目录，如 connectors/memoryhub.json
```

每个候选后端即一个"插件条目"，配套一个**记忆/插件管理页**（仅展示与启停，见 P2）。

---

## 6. 拓扑

- **单一 active 写路径**：写入只走一个 provider（写是决策，避免"mem0 作废了旧事实、本地还留矛盾旧条目"漂移）。
- **读可 fan-out**：召回可同时查多个只读后端。
- **审计记录写决策**：把 `MemoryWriteDecision`（含 `superseded`）记入审计日志。
- 本地兜底：外部 Down 时 `recall` 返回本地结果，agent 不中断。

---

## 7. 性能零回归硬约束（P0 验收红线）

1. **异步写**：`memorize` 必须走后台通道（非阻塞 push + 批量 + 网关卡），**绝不**在交互 loop 内同步等待 LLM 抽取/网络。
2. **缓存读 + 门控**：`recall` 结果设 TTL 缓存；只在**用户输入那一轮**触发召回，不在每次模型内部步骤都查；`top_k` 有上限。
3. **写门控/节流**：只有语义上有意义/显式"记住"/满足蒸馏条件的事实才写；不做"每轮全量记忆"。
4. **预算打包**：召回结果沿用现有 `pack_by_budget`，控制注入 token。
5. **懒连接 + 非阻塞健康**：启动不阻塞等待 mem0/MCP；健康走后台轮询。

> 量化目标：本地交互路径增量 ≤ 个位数 ms（缓存未命中时）；写路径阻塞 = 0。

---

## 8. 范围（本 SDD 内/外）

**在本 SDD 内**
- P0–P3 见 `tasks.md`（MemoryProvider + LocalProvider + 注入点改造；McpMemoryAdapter + mem0；RestMemoryAdapter + 管理页；可选的跨机隔离/GC）。

**不在本 SDD 内（明确排除，避免范围蔓延）**
- 不把任何具体服务（MemoryHub/mem0 之外的）写死进二进制。
- 不做"每轮全量对话同步"。
- 跨机共享、团队隔离、远端审计属 P3 可选，需另评。

---

## 9. 风险与权衡

| 风险 | 缓解 |
|---|---|
| 语义写缺失（无 MCP 的傻瓜后端） | `WritePolicy::BestEffortDedupe` 降级标记 + `fallback=local` |
| MCP 工具名不统一 | 工具名映射做成配置 |
| mem0 写路径 LLM 抽取成本 | 后台异步 + 写门控 + 节流 |
| 每轮召回延迟 | TTL 缓存 + 用户轮门控 + top_k 上限 + 预算打包 |
| 双写漂移 | 单一 active 写路径 |
| 凭据泄露 | 沿用现有 MCP 加密存储路径 |

---

## 10. 兼容性与回归

- `LocalProvider` 完全复用现有 `MemoryStore`/`deep_memory`，`active=local` 时行为与现状一致 → **零回归**。
- 注入点（`llm_agent.rs`）改为面向 trait，返回结构保持 `[Memory Context]/[Memory Recall]` 语义不变。
- 反向兼容：未配置 `[memory]` 时默认 local，旧 config 无需改动。

---

*待审核。通过后按 tasks.md 的 P0 开始实施。*