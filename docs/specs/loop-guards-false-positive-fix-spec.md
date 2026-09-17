# FoxIR 循环护栏（Rabbit Hole / Auto-stall / Text-loop）过度触发修复 Spec

| 项 | 内容 |
|---|---|
| 文档状态 | **待人工审查 v0.9** |
| 范围 | Instant/Expert agent 主循环的三处死循环护栏误报修复 + 配置拆分 |
| 触发原因 | 用户反馈 rabbit hole 与 auto-stall "太容易触发"，审查确认 4 处过度触发缺陷 |
| 关联文档 | 前端审查报告（计划文件 wildcat-plastic-man-guy-gardner.md）、Expert 多会话修复方案（plastic-man-green-arrow-pantha.md） |

---

## 1. 背景与问题

### 1.1 护栏机制现状（三处，均在 `src/agent/llm_agent.rs` 主循环）

| 护栏 | 位置 | 现有语义 | 默认值 |
|---|---|---|---|
| Text-loop 检测 | `:1827-1843` | 每轮 LLM 响应算 `digest(content+"\n"+reasoning)`，连续 6 次相同 → 终止运行 | `TEXT_REPEAT_LIMIT = 6`（硬编码） |
| Rabbit hole 检测 | `:2090-2130` + `rabbit_hole_check:3148-3167` | 签名 = `工具名:原始参数串`，计数器**终身累积**（仅触发时清零），≥ 阈值 → 跳过执行并注入纠正消息 | `rabbit_hole_threshold = 5`（可配） |
| Auto-stall 自愈 | `:2229-2312` | ①同工具连续返回相同内容 ≥5 次，或 ②连续 5 轮"无新状态" → 合并重复结果 + 注入 reconsider；`reconsider_events` **终身累计** ≥3 → 终止运行 | 阈值派生自 rabbit 旋钮；`MAX_AUTO_RECONSIDERS = 3`（硬编码） |

### 1.2 审查确认的缺陷

**缺陷 1（严重）· text-loop 误杀正常工具流**：对非推理模型，纯工具调用轮的
content 与 reasoning 均为空串 → digest 恒等 → **连续 6 轮纯工具调用（多步工作流的
正常形态）即被 "Auto-stop" 终止**。检测对 `tool_calls` 非空的轮次不过滤。

**缺陷 2 · rabbit hole 非连续累积、不看结果**：
- 计数器跨整个 run 累积且永不衰减——长任务中第 3/17/42/60 轮各读一次同一文件即触发；
- 只查 `name+args` 不看结果——结果逐次变化的 wait-for-change 轮询（等进程退出、
  等文件出现）照样计数；
- 只读幂等工具与有副作用工具一视同仁。

**缺陷 3 · auto-stall "相同结果"判定过敏**：
- 判定键只有工具名、**不含参数**——`Test-Path A` 与 `Test-Path B` 同返 `"False"`
  被算作"状态没变"；
- 短输出字母表工具（True/False、exit code、"OK"）digest 碰撞是常态而非 stall；
- 有意为之的监控轮询连续 5 次相同结果即被强制 reconsider。

**缺陷 4 · `reconsider_events` 终身累计不衰减**：长任务中 3 次互不相干的
（可能是误报的）stall，跨 50 轮累积后直接终止任务，尽管期间有大量正常进展。

**附带观察**：`stall_repeat_threshold = rabbit_hole_threshold.max(2)`（`:1708`）
把三个护栏耦合在同一个旋钮上；`TEXT_REPEAT_LIMIT`、`MAX_AUTO_RECONSIDERS`
为硬编码常量，不可调。

---

## 2. 目标与非目标

### 2.1 目标
1. 消除缺陷 1-4 的误报路径，**护栏只拦真死循环**：精确定义为
   "连续 N 轮、相同动作、相同结果"。
2. 保留并强化对真死循环的拦截能力（同动作+同结果必然连续，拦截语义不退化）。
3. 护栏阈值解耦为独立配置项，老配置无感迁移。
4. 全部改动配单测 + mock 回归，防"修好误报、放开真循环"。

### 2.2 非目标
- 不改动 Expert 模式的 human gate（"连续 3 轮未取得进展"是 ManagedRunner 的
  独立机制，属另一议题）。
- 不引入 LLM 判定的语义级循环检测（保持确定性、零额外 token 开销）。
- 不调整工具超时/重试机制（`timeout_stage` 体系不动）。
- 不做"间歇性偏执重复"（每隔几轮重犯一次）的主动拦截——见 §6 取舍声明。

---

## 3. 设计

### 3.1 F1 · text-loop：工具调用轮不参与统计（P0）

位置：`:1827-1843`。仅当本轮 `tool_calls` 为空时才更新 digest 计数：

```rust
if tool_calls.is_empty() {
    let resp_digest = content_digest(&format!("{}\n{}", content, reasoning));
    if resp_digest == last_resp_digest { consecutive_resp += 1; }
    else { last_resp_digest = resp_digest; consecutive_resp = 1; }
    if consecutive_resp >= text_repeat_limit {
        // 现有终止逻辑不变
    }
}
```

边界语义：`tool_calls` 空 + content 空 的 API 异常轮仍计数——这正是该拦的空转。

### 3.2 F2 · rabbit hole：连续同批次 + 同结果（P1）

新判定语义：**"连续 N 轮发出完全相同的调用批次，且相邻两批的结果完全相同"**。

状态替换（`:1700` 附近）：

```rust
// 旧：call_signatures: HashMap<String, usize>（终身累积）→ 删除
let mut last_batch_sig: Option<String> = None;        // 上一轮批次签名
let mut last_batch_results: Vec<u64> = Vec::new();    // 再上一批结果 digest（排序）
let mut pending_batch_results: Vec<u64> = Vec::new(); // 上一批结果 digest
let mut consecutive_same_batch: usize = 0;
```

- **批次签名**：本轮全部 `name:args` 排序后 join（消除批内顺序差异）。
- **检测**（替换 `:2090-2130` 整段）：签名与上轮相同 **且** `pending == last`
  （结果也没变）→ 计数 +1；结果在变 → 重置为 1（wait-for-change 豁免）；
  签名不同 → 重置为 1。计数 ≥ `rabbit_hole_threshold` → 走现有纠正消息 +
  `continue`（跳过执行），计数清零。
- **结果更新**：工具执行后（`:2229` 前）
  `last_batch_results = std::mem::take(&mut pending_batch_results)`，
  `pending_batch_results = 本批结果 digest 排序`。

效果：跨轮穿插调用自动清零（合法重读不再误伤）；结果变化的轮询豁免；
"同动作+同结果+连续"是真死循环的精确定义，拦截不退化。

### 3.3 F3 · auto-stall：判定键带参数 + 短结果降权 + LRU 新状态判定（P2）

位置：`:2229-2262`；`collect_tool_results:3104-3115`。

- **3a 键升级**：`collect_tool_results` 从 history 匹配 assistant 的 tool_calls，
  产出 `(name, args_digest, content)`；`last_result` 键改为 `(name, args_digest)`。
  `Test-Path A`/`Test-Path B` 不再互相计入。
- **3b 短结果降权**：结果 < 32 字符时，要求的连续相同次数 ×2
  （短输出字母表小，"相同"信息量低）。
- **3c LRU 新状态判定**：维护最近 20 条 `(name, args_digest, result_digest)`
  三元组集合；"本轮所有三元组均已见于集合"才算无新状态。周期 ≤20 的循环
  （A,B,A,B）仍可被抓；偶发短结果碰撞不再误判。

### 3.4 F4 · reconsider_events 有进展即重置（P1）

`:2248-2254` 的"有真实进展"分支（F3 后为新三元组分支）加：

```rust
reconsider_events = 0;  // 只计连续 stall 事件
```

`MAX_AUTO_RECONSIDERS` 语义从"终身 3 次"变为"**连续** 3 次无进展 reconsider 才终止"。

### 3.5 F5 · 配置拆分（P2）

`AgentConfig` 新增（均 `#[serde(default)]`，老配置无感迁移）：

| 字段 | 默认 | 语义 |
|---|---|---|
| `rabbit_hole_threshold` | 5（已有） | 连续同批次同结果触发次数 |
| `stall_result_threshold` | 8 | auto-stall 结果/状态窗口（替代派生公式） |
| `text_repeat_limit` | 6 | 纯文本轮重复上限（替代硬编码） |
| `max_auto_reconsiders` | 3 | 连续 reconsider 上限（替代硬编码） |

配套：
- `config.rs`：default 函数 + 模板（`:761`）+ `save_agent_settings`（`:850`）
- `server.rs`：AppState 原子量（`:163`）、settings 读写（`:829, :3206-3310`）、
  chat 消息级覆盖（`:1610`）
- `runner.rs`：四个阈值收进 `LoopGuards` 结构体传入（避免参数继续膨胀）
- 前端：设置页 Agent 卡片仿现有 `rabbit_hole_threshold` 输入框加 3 个字段
  （沿用"前端改动最小化"约束）

---

## 4. 改造范围（分模块）

| # | 项 | 文件 | 改动量 | 批次 |
|---|---|---|---|---|
| F1 | text-loop 跳过工具轮 | `src/agent/llm_agent.rs` | ~5 行 | 一 |
| F2 | rabbit 连续同批同结果 | `src/agent/llm_agent.rs` | ~40 行 | 一 |
| F4 | reconsider 有进展重置 | `src/agent/llm_agent.rs` | ~2 行 | 一 |
| F6a | F1/F2/F4 单测 + mock 回归 | `llm_agent.rs` tests / `tests/` | ~60 行 | 一 |
| F3 | stall 键升级 + 降权 + LRU | `src/agent/llm_agent.rs` | ~35 行 | 二 |
| F5 | 配置拆分 + 前端字段 | `config.rs`/`server.rs`/`runner.rs`/`static/index.html` | ~60 行 | 二 |
| F6b | F3 单测 + 全量回归 | 同上 | ~20 行 | 二 |

---

## 5. 测试策略

### 5.1 单测（`llm_agent.rs` tests 模块）
- 批次签名排序稳定性（同批不同序 → 同签名）。
- F2：连续同批同结果达阈值触发；结果变化豁免；穿插调用清零。
- F3：同工具不同参数不互相计入；短结果阈值翻倍；LRU 周期循环可被抓。
- F1：`update_text_repeat(has_tool_calls, digest, &mut state)` 提取为纯函数单测。

### 5.2 mock 回归（仿 `tests/mock_llm.rs`）
- **缺陷 1 回归**：脚本化 mock 连续 8 轮纯工具调用 → 运行**不被** text-loop 终止。
- **轮询豁免回归**：同调用、结果逐次变化 → 不触发 rabbit。
- **真循环拦截回归**：同调用同结果 5 连 → 触发 rabbit 纠正（护栏未死）。

### 5.3 既有门禁
- `cargo test`、`tests/step1_gates.rs`、`tests/phase0_e2e.rs` 全绿。

---

## 6. 取舍与风险声明

1. **有意取舍**：F2 把 rabbit 语义从"累计同名"改为"连续同批同结果"。
   "间歇性偏执重复"（每隔几轮重犯一次）不再触发 rabbit，改由 F3 的
   no-new-state LRU 窗口兜底。原则：**宁可漏拦慢性低效（浪费 token），
   不可误杀正常长任务**。
2. **R1 · 真循环漏拦风险**：若模型每轮微调参数无效字符（空格/大小写）规避
   签名相同——可接受，此类case由 text-loop（F1 后仍生效）与 stall 窗口兜底。
3. **R2 · F3c LRU 容量**：20 条三元组对超长 run 可能偏短，周期 >20 的循环漏检——
   可接受（此类循环已接近"正常工作"），容量后续可配。
4. **R3 · 行为可感知变化**：过去被误杀的任务现在会跑完；过去依赖"累计 5 次"
   拦住的边缘场景将多消耗若干轮 token。均属预期。

---

## 7. 施工顺序

1. **第一批（P0+P1，约 1 小时）**：F1 + F2 + F4 + F6a → `cargo test` + mock 回归。
   止住全部误杀场景；配置暂用现有旋钮。
2. **第二批（P2，约 1-2 小时）**：F3 + F5 + F6b + 前端字段 + 全量回归。

---

## 8. 待审查确认点

1. 阈值默认值是否认可（rabbit 5 / stall 8 / text 6 / reconsider 3）？
2. F2 语义变更（累计 → 连续同批同结果）及 §6.1 的取舍是否接受？
3. 配置项是否暴露到设置页 UI，还是仅 config.toml（第一批可先不做 UI）？
4. 短结果降权的 32 字符界限与 ×2 系数是否合适？
