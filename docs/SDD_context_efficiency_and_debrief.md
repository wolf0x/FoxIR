# FoxIR 上下文经济性与结论固化 · 软件设计文档（SDD）

| 项 | 内容 |
|---|---|
| 文档 | 前缀缓存友好化 + PromptTier 分层 + 记忆 KPI + 只读并行 + 估算器统一；follow-up 队列自旋修复；SOP 作者化 → Debrief 结论固化管线 |
| 版本 | v1.0 |
| 状态 | 已落地（commit `3542011` / `95d8160` / `76e1818`，分支 `codex/multi-session`） |
| 适用工程 | FoxIR（Rust 单二进制、Axum 网关、Windows 本地） |
| 灵感来源 | small Hermes 架构文章（省 token / 安全 / 性能 / 越用越懂你）与另一项目的优先级预算 + PromptTier 方案的对照分析 |

---

## 1. 概述

本 SDD 记录一次围绕「harness 经济性」的集中改造，三条主线：

1. **省 token**：system prompt 拆分为字节级稳定的 head + 尾随 volatile state 消息，使 DeepSeek/Anthropic 前缀缓存跨 run 可命中；叠加 PromptTier 分层，纯问候回合的 system prompt 从 ~3-5K token 降到 ~800。
2. **修两个真实缺陷**：follow-up 队列在 run 进行中的 pop→push 忙等自旋；SOP 作者化因工具统计口径与原生 function calling 不匹配而永不产出。
3. **越用越懂你**：`deep_facts` 增加 Loaded/Referenced 有效性 KPI 并接入统一价值函数 U 分量；SOP 作者化泛化为 Debrief 结论固化管线（案例 writeup + 可选 SOP 双轨产出）。

---

## 2. 目标与非目标

**目标**
- G1：跨 run 前缀缓存可命中——system prompt + history 前缀字节级稳定。
- G2：琐碎回合（纯问候）system prompt 体积下降 ≥80%，且 Minimal 是 Full 的严格字节前缀（层间切换只失效一次）。
- G3：记忆注入可度量——每条深层事实追踪 loaded/referenced，低效事实降权但不删除（自然选择，非清除）。
- G4：只读工具批量调用并行化，不破坏可变状态工具的串行语义。
- G5：SOP 永不产出的根因修复，并泛化为「明确问题 → 实质排查 → 收敛结论 → 固化」的 Debrief 管线。
- G6：follow-up 队列忙等自旋消除，排队语义（FIFO、Stop 后仍执行）不变。

**非目标（本期）**
- N1：多用户/团队协作记忆隔离（FoxIR 已有 `MemoryScope`，本期不动）。
- N2：Debrief 产物的审批 UI（当前 auto-accept + confidence 元数据，审批列为后续）。
- N3：`problem_type` 聚合视图（根源解法条目合成）——待案例数据积累后做。
- N4：二进制瘦身（取证依赖为刚需，不为没出现的问题做优化）。

---

## 3. 前缀缓存友好化 + PromptTier（commit `3542011`）

### 3.1 问题

`build_system_prompt`（`src/agent/llm_agent.rs`）每轮重建 system prompt，且混入每轮必变的内容：当前日期、按用户消息语言变化的 LANGUAGE RULE、知识预检索指针块、SOP reminder、CONTEXT BUDGET 仪表盘数字。由于 messages 布局是 `[system, ...history]`，system 尾部的任何变化都会使**整个 history** 失去前缀缓存——这正是"每天几毛钱"与"每天几十美元"账单差别的来源。

### 3.2 设计

**Stable/Volatile 二分**：

- **Stable head**（`build_system_prompt` 输出）：身份、User Identity、温暖总纲、问候规则、路由、工具规则、workspace 配置文件、skills 目录——所有不随单轮变化的内容。
- **Volatile state**：日期、本轮 LANGUAGE RULE、TODO 块、证据台账、记忆/知识/SOP 混合价值池（Context Guidance）、CONTEXT BUDGET 仪表盘——收集到 `state_core` / `volatile_state`，在消息装配时作为独立 system 消息**插入到最后一条 user 消息之前**（`llm_agent.rs` 的 `assemble_messages`）。**不得追加在末尾**：尾部 system 块会把预算提示等文本放到近因最热的位置，弱指令层级模型会把它误当作当前操作指令（实战教训：DeepSeek-V4-Flash 读到末尾的 "trim/stop" 后叙述一半就提前收尾）。缓存前缀不受影响。

预算会计同步调整：`system_tokens = estimate_tokens(stable) + estimate_tokens(volatile)`，history 预算按真实总占用扣减；`/api/budget` 仪表盘数据源不变。

**PromptTier 分层**（嵌套前缀，Minimal ⊂ Full）：

- `PromptTier::Minimal`：人设头（身份 + 用户名 + 温暖总纲 + 问候规则），约 ~800 token，供纯问候回合。
- `PromptTier::Full`：完整规则书，Minimal 是其严格字节前缀——同 tier 连续回合缓存照常命中，tier 升级只失效一次。
- 判定器 `is_pure_greeting` 刻意严格：整条消息去标点后必须**只含**问候词（"你好"、"good morning"、"hi hi" 等），长度 ≤30。"hi, 查一下我的 IP" 这类混合消息不会误判降级。
- 既有 `looks_like_greeting`（语言规则用）保持不动，两者职责不同。

### 3.3 验证

- 测试 `minimal_prompt_is_strict_prefix_of_full`：断言 Minimal 是 Full 的字节前缀，且 head 不含 `Current date:` / `LANGUAGE RULE`。
- 测试 `pure_greeting_detection_is_strict`：纯问候 → Minimal；混合任务消息 → Full。
- 上线后观测：DeepSeek 响应的 `prompt_cache_hit_tokens` 应显著上升。

---

## 4. 记忆有效性 KPI（commit `3542011`）

### 4.1 问题

`deep_facts` 只追踪 `last_accessed`（注入时刻），退火与排序都基于"何时注入"，**不知道注入后 LLM 是否真的用了它**。常驻永久块每轮烧 token，低价值事实可能长期占位。统一价值函数 `V = Q²·R·U` 的 U 分量此前恒为 1（`deep_memory.rs` `DeepFact::value`）。

### 4.2 设计

- **Schema**：`deep_facts` 增加 `loaded` / `referenced` 两列（新库 CREATE TABLE 内置；老库按 `archived` 列同款 `pragma_table_info` + ALTER 模式迁移，`memory.rs` `ensure_two_tier_schema`）。
- **计数**：注入点（`server.rs` O2 注入处）把 `deep_permanent_block` 返回的 picked_ids 与块内事实行 zip 成 `(fact_id, 注入行)`，随 `drain_session_stream` 传入；回复落地后 `loaded += 1`（全部注入条目），对回复文本做**确定性关键词匹配**（不调 LLM）命中的条目 `referenced += 1`（`deep_record_usage`，只增不减）。
- **匹配器**（`deep_memory.rs`，纯核）：`reference_keywords` 提取区分度关键词（ASCII 词 ≥5 字符、CJK 连串 ≥4 字，按长度降序取前 8）；`body_referenced` 要求命中 ≥min(2, 候选数)；CJK 长串允许任意 4 字窗口命中，容忍中文转述的改序/插字（"认证模块正在重构" → "认证模块重构"）。
- **有效性因子**：`factor = 0.5 + 0.5 × (referenced/loaded)`，钳制在 [0.5, 1.0]——只降权不归零；从未注入过的事实不受惩罚（1.0）。乘进 `DeepFact::value()` 的 U 分量。

### 4.3 验证

测试覆盖：factor 边界（0 注入、全引用、半引用、异常数据）、关键词匹配、CJK 窗口容忍转述。

---

## 5. 只读工具并行推广（commit `3542011`）

### 5.1 问题

并行执行仅限 IR 采集工具名白名单（`is_ir_collection_batch`），日常运维场景一轮 3 个 `file_read`/`knowledge_search` 也排队。而 `Tool::is_read_only()` trait 分类**早已存在**且在 `Parallel`/`Auto` 策略分支中使用——只是默认的 `Sequential` 策略没用上。

### 5.2 设计

Sequential 分支门控从名字白名单改为 trait 判定：批量 ≥2 且**每个**工具 `is_read_only()` 为真（IR 白名单保留为无锁快速路径），则走既有的 `execute_tools_concurrent`（`join_all`）。混合或可变状态工具（`deep_memory`、`sys_process`、`todo_update`、`browser_cdp` 等）保持串行，permission 闸语义不变。

### 5.3 验证

测试 `parallel_gate_accepts_ir_and_rejects_mixed`：IR 批 → 并行；单个调用 → 不并行；混入非只读 → 串行。

---

## 6. 估算器统一（commit `3542011`）

**问题**：三处 token 估算口径不一致——agent 循环 CJK 感知（CJK/1.5 + 其他/4），`deep_memory::estimate_tokens` 与 `context_arbiter::crate_cost` 为裸 chars/4，中文场景下记忆块与仲裁器的预算会计失真（低估 ~3 倍）。

**修复**：`deep_memory::estimate_tokens` 升级为唯一 CJK 感知实现；agent 循环与 context_arbiter 均委托给它。单一事实源，全库预算口径一致。

---

## 7. follow-up 队列忙等自旋修复（commit `95d8160`）

### 7.1 现象

日志中同一 session 每 ~8ms 重复打印 `Dispatching queued follow-up task` + 全套记忆注入（session recall / auto-recall / deep permanent block + SQLite touch），且交替出现两组不同数据（432/864 chars、touched 0/12）。

### 7.2 根因

WS 主循环（`server.rs`）顶部每轮 `pop_pending` 弹出排队的 follow-up 任务，进入 chat 处理后才发现 `session_is_running == true`（前一个 run 还在跑），于是 `push_pending` **原样推回队尾**；循环立即回到顶部再次弹出——形成 pop → push → pop 忙等。队列里两条消息 FIFO 轮转，即日志中的交替模式。在跑的 run 不结束，自旋就永远不停；每轮空转还重复执行全部记忆注入。

### 7.3 修复

只在**没有 run 在跑时**才 pop 队列；run 在跑时用 100ms 超时的 `ws_rx.recv()` 等待（stop / interject / 新消息照常处理），超时后重新检查 running 标志。排队语义不变：FIFO、Stop 只取消在跑任务、已排队消息仍会执行；run 结束后最多 100ms 内派发下一条。

---

## 8. SOP 作者化 → Debrief 结论固化管线（commit `76e1818`）

### 8.1 根因分析（"为什么 SOP 从未产出"）

来自另一台机器（PCBuddy 工作区）日志的三层叠加原因：

1. **触发点单一**：作者化只挂 WS 客户端断开（`server.rs` 主循环 `Message::Close` 之后）。应用被杀或长开不断开则永不触发；`history < 4` 时静默跳过（无日志）。
2. **统计口径 bug（真 bug）**：作者化用的 history 是内存 `state.sessions`——只有 user/assistant 纯文本，无 tool 角色消息。而 `count_used_tools` 只数 role=="tool" 的消息或 assistant 文本里的 ```` ```json ```` 工具块。DeepSeek 走**原生 function calling** 时两者都不存在 → 统计恒为 0 → 每次都被判为 "session has no tool calls, not a procedural candidate"。日志佐证：会话 52417877 有 4 次 Runner dispatch、大量 winrm/linux_ssh 调用，仍在 16:11:46 被判无工具调用。
3. **回放侧无从激活**：`sops.json` 从未创建，replay 匹配无从谈起（配置 `sop_replay = true` 本身无问题）。

### 8.2 设计：Debrief 管线（`src/debrief.rs`）

触发哲学从「是否含可复用流程」改为「**是否一个明确问题经过实质排查后收敛出明确结果**」——覆盖 CTF flag 解题，也覆盖桌面运维与应急响应（"分析 xxx"、"调查 xxx"、"为什么 xxx 失败"）。

**门控**（`should_debrief`，纯确定性、全测覆盖）：

- 过程实质：工具调用 ≥3，**权威口径**——`drain_session_stream` 从事件流 `ToolResult` 逐条计入 `AppState.session_tool_log`（每会话上限 200 条），不再从文本猜；
- 结果收敛：`has_conclusion`——回答足够长（≥40 字），或较短但带结论标记（"结论/根因/已修复/root cause/fixed" 等，≥15 字；中文结论紧凑，硬字数阈值会误杀）；
- 问题明确（`is_problem_query`：分析/调查/排查/investigate 等触发词，或 IPv4/路径/0x 错误码等具体对象）**或** 用户确认（`is_ack`：严格匹配，"解决了，但还有个问题"不算）。

**双轨产出**（一次 LLM 作者化，`parse_debrief` 解析 SKIP 或 JSON）：

| 产物 | 条件 | 落点 |
|---|---|---|
| **Case note（案例）** | 几乎总是有 | `knowledge/writeups/<slug>.md`，固定模板（问题 / 根因 / 解法链路 / 走过的弯路 / 关键证据 / 复用触发信号 / 使用工具），frontmatter 由 `knowledge::write_entry` 生成并自动重建索引——**每轮知识预检索自动浮现** |
| **SOP（流程）** | 仅当过程可复用 | 现有 `sops.json` 管线（`register_sop` 合并语义 + Wilson 评分 + 作者化后 GC） |

聚合键 `problem_type`（归一化：小写、空白→连字符、≤40 字符）写入正文与 tags，为后续"同题型聚合成根源解法条目"（N3）预留。

**接线与开关**：WS 断开的会话结算从单一 SOP 作者化替换为 `debrief::run`；`[agent] debrief_enabled`（默认开）；skill 驱动会话仍跳过（技能已自带流程）；`history < 4` 门控保留。

### 8.3 验证

9 个新测试覆盖：门控三要素、确认豁免触发词、问题判定（含具体对象）、严格 ack、历史信号构建、SKIP/JSON 解析、垃圾拒绝、problem_type 归一化、模板完整性。全量测试 327 通过（3 个 `todo_update` 失败为既有的本机环境问题，干净代码上同样失败，与本次改动无关）。

---

## 9. 遗留与后续

1. Debrief 触发点仍只有 WS 断开——`/debrief` 手动命令、空闲超时触发列为下一步。
2. Debrief 产物的审批队列（真实 IR 场景防错误结论固化）与 `reflect-log` 式审计。
3. `problem_type` 聚合视图：攒够案例后合成"根源解法"条目，复用 SOP 价值函数排名。
4. MEMORY.md 投影暂不含 loaded/referenced KPI 数值，需要时加列。
5. 上线观测指标：`prompt_cache_hit_tokens`（G1）、writeups 产出率与回放命中率（G5）。
