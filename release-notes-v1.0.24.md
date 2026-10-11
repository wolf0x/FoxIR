## FoxIR v1.0.24 — 彻底修复"切换 Turn/Group 丢历史"，并对齐 GDAgent 让 TASKS 真正实时

本次是针对"另一台机器上 v1.0.23 仍复现"的两处问题的一次修复发布：切换 Turn/Grouped 后早期整段会话消失、以及 TASKS(TODO) 面板执行中长期不更新。两者都定位到了 v1.0.23 未覆盖的真实根因。无后端协议破坏性变更。

### 切换 Turn/Grouped 后历史会话丢失（fix，前端）— 真正的根因
- **真实根因**（上一版判断不完整）：`emit()` 把**每一个** `text/thinking/progress` 流式增量都单独 `push` 进 `OUT.log`。有界日志上限 `OUT_LOG_CAP=2000` 一旦被突发打满，即 `splice` 丢弃**最旧**事件；而 `rebuild()` 只重放被裁剪后的尾部日志——切换视图时早期整段会话就被抹掉。v1.0.23 的守卫只兜"日志全空"，而真实长会话是"被裁剪但非空"，故不触发。
- **实测佐证**：一台机器单会话服务端累计推送 **35714 个事件**（几乎全是超长 `reasoning` 流式增量，单轮 reasoning 达 22381 字符），远超 2000 上限 → 约 3.3 万条最旧事件被丢弃。作为对照，服务端持久化的会话日志只有 **157 条逻辑块**——说明正确粒度本就是"块"而非"token"。
- **修复**：`outLogPush()` 入口新增"连续同流增量合并"——当新事件与日志末条同为 `text`/`thinking`（拼接 `content`）或同为 `progress`（保留最后一条状态），且 `invocation_id` 一致时，合并进同一条逻辑记录并 `return`，不再新增条目。`OUT.log` 的长度自此**只随块数增长**，长会话不再触顶，`dropped` 归零。工具/权限/子 Agent 等离散事件仍各占一条，卡片无损。

### TASKS(TODO) 面板执行中不更新（fix + 对齐 GDAgent）
- **真实根因**：从该机日志看，整场 18 分钟 / 33 轮的任务里 `todo_update` **只被调用 3 次**（开场 `set` 一次 + 临近结尾 `update` 两次），**中间整段一次都没写**。面板长时间停在初始 `pending`——不是前端没刷新，而是模型根本没中途更新状态；任何"前端刷新策略"都救不了"后端没有新数据"。
- **对齐 GDAgent 的做法**：GDAgent 的实时靠引擎级 `PlanUpdated` 事件**携带全量任务载荷**、服务端 WS 直接推送、前端拿事件体即时渲染（无 fetch）。FoxIR 的 Instant `todo_update` 的 `tool_result` **本就携带全量 `todos`**（`todos_to_json`），此前却绕一圈去 `fetch('/api/todos')`。
- **修复①（事件载荷直渲）**：`emit()` 对 `name==='todo_update'` 的 `tool_result` 改为调用 `applyTodosFromEvent(msg)`，直接用 `msg.result.todos.items` 渲染 `#todoList`，去掉 fetch 往返；`set/update` 渲染全量、`clear` 与全部完成的 `auto_cleared` 渲染空态；仅在遇到未知载荷时才退回带防抖的 `loadTodos()` fetch。5 秒轮询保留为兜底（覆盖服务端 watchdog 自动 `skip` 不发事件、以及断线重连/切会话后的补齐）。
- **修复②（提示硬红线）**：Instant 系统提示的 `## Task Planning` 契约新增 **"HARD RULE — no silent rounds"**：一旦 TODO 生效，**每个以工具调用收尾的工作轮都必须同时带一次 `todo_update`** 反映当前项的真实状态（开始即 `in_progress`、完成即 `completed`）；明令禁止"开场 `set`、随后多轮不 `update`、结尾批量补状态"这一正是本次故障的反模式；10+ 项的大清单也必须逐项即时更新。

### 已知边界（诚实标注）
- 修复①让"写了就秒显"，修复②让"中途真的有写"——两者叠加才能达成 GDAgent 级实时；但②依赖模型遵循提示，非引擎级强制。若要彻底 GDAgent 化（引擎自动流转计划步骤并推送），需给 Instant 路径补 Plan 状态机，属更大改动，未纳入本版。
- 服务端 watchdog 自动 `skip` 仍无 WS 事件，依赖最多 5s 轮询兜底；如需该路径也秒级，需后端写盘后主动推 `todos_updated`（本版仍未做）。

### 验证
- `cargo test --lib` **523 passed / 0 failed / 20 ignored**（Rust 提示改动随编译校验；`todo_update` 会话隔离与 auto-clear 等单测全绿）。
- 前端内联脚本 `vm.Script` 语法校验全部通过（无 syntax error）。
- `cargo build --release` 通过，产出发布二进制并打包 `FoxIR-v1.0.24-win64.zip`。
- 浏览器运行时验证（确定性注入）：
  - 历史合并：灌入 3000 `text` + 3000 `thinking` 增量 + 4 个工具事件 + 结尾标记，结果 `logLen=8`、`dropped=0`；Turn↔Grouped 来回切换，早期标记与结尾标记均在 DOM 中不丢失，`dropped` 全程 0。
  - TODO 直渲：`set/update` 全量 3 项直接渲染（含 `completed`/`in_progress` 状态类）、`auto_cleared`→空态、`clear`→空态，三条路径均无需 fetch；仅出现注入事件固有的 orphan 警告，无函数级异常。
