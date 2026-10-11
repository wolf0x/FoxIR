## FoxIR v1.0.23 — 分组视图精修 + 三处交互 Bug 修复（插话去重 / 切换丢历史 / TODO 不实时）

本次是在 v1.0.22 分组（Grouped）视图之上的一次修补发布：把两框文案与折叠行为定型，并修掉三个真实影响使用的交互缺陷——一次插话被送达两次、切换 Turn/Grouped 后历史会话消失、TASKS(TODO) 面板状态不及时更新。无后端协议破坏性变更。

### 分组视图精修（Grouped）
- **文案定稿**：`Thinking` → `Analyzing`（紫，默认折叠），`操作 / N tool calls` → `EXEC` + `Tool call: N`（琥珀，流式期默认展开）；头部把"计数"与"耗时"拆成独立 span（`.gbox-count` / `.gbox-time`），布局更稳。
- **工具卡详情默认折叠**：`tool` 卡的参数与结果全文默认收起，点击卡片展开；EXEC 框本身在流式进行期间保持展开，本轮收束时才随框一起收起。
- **整轮收束折叠 + 新一轮守卫**：`gsClose` 前新增 `gsCollapseBoxes()` 把本轮两框一并折叠，并置 `gs.folded` 标记；下一轮首个 `thinking/tool` 事件命中守卫时先关旧组、独立成新组并重新展开。中间叙述（`text`）不再拆组，本轮保持同组直至 `done`。

### 插话一次却被送达两次（fix，后端）
- **根因**：同一条排队消息被下发两次 `queued_run`——① `chat`-while-running 分支在 `push_pending` 之后**立即**又发了一次 `queued_run`，而客户端此刻已自画了发送气泡；② 派发循环在真正开跑时再回显一次。两者叠加即"相隔数分钟的两条重复用户消息"。
- **修复**：pending 队列改带 `echo` 标记。插话来源 `echo=true`（客户端未画气泡，由派发时才回显一次）；`chat`-while-running 来源 `echo=false`（客户端已画，派发直接执行、不再回显）。删除 chat-while-running 站点那次过早的 `queued_run`。顺带把共用的 `Queue` 类型别名拆为 `PendingQueue`（存 `(String,bool)`）与 `InsertQueue`（存 `String`），插话通道不受影响。

### 切换 Turn/Grouped 后历史会话丢失（fix，前端）
- **根因**：若干"用 `innerHTML` 快路径回填会话内容却不播种 `OUT.log`"的路径（快照/历史恢复、后台文本落地）会让日志为空但 DOM 有内容；一旦切换视图，`rebuild()` 用空日志重绘即把已渲染的历史抹掉。
- **修复**：在 `rebuild()` 入口加不变量守卫——当日志为空但当前会话仍持有持久化消息 `msgHist` 时，先 `seedHistToLog(msgHist)` 再重放。一处覆盖所有绕过日志的路径（这些路径都维护 `msgHist`）。

### TASKS(TODO) 面板不及时更新（fix，前端）
- **根因**：面板此前**仅靠 `setInterval(loadTodos,5000)` 每 5 秒轮询**刷新，无事件驱动，执行中状态最多滞后 5 秒。
- **修复**：`todo_update` 工具在 `execute()` 内即已写盘，其 `tool_result` 到达时当前会话的 `todos-<session>.json` 必已最新——`emit()` 据此对 `name==='todo_update'` 的结果触发 `scheduleTodosRefresh()`，120ms 防抖合并突发；5 秒轮询保留为兜底（覆盖服务端 watchdog 自动 `skip` 等不发事件的路径）。后台会话在 `emit` 中已提前分流，故钩子只对可见会话自身的 todo 变更触发，会话隔离天然成立。

### 已知边界（诚实标注）
- TODO 实时刷新为**前端事件驱动 + 轮询兜底**：模型显式调用 `todo_update` 已做到亚秒级（实测 ~125–134ms）；服务端 watchdog 自动跳过这类无事件变更仍依赖最多 5s 的轮询兜底。如需该路径也秒级内，需后端在写盘后主动推 `todos_updated` 事件（本版未做）。
- `queued_run` 回显不变量（interject 来源回显一次、chat-while-running 不回显、派发时才回显）集中在 `server.rs` 派发循环与 `interject.rs` 的 `echo` 标记，后续改动需注意勿回退。

### 验证
- `cargo test --lib` **523 passed / 0 failed / 20 ignored**（含 `interject` 队列/通道隔离、`todo_update` 会话隔离与 auto-clear 等单测随签名更新）。
- `cargo build --release` 通过，产出发布二进制并打包 `FoxIR-v1.0.23-win64.zip`。
- 浏览器运行时验证（带缓存破坏 URL）：
  - 历史丢失：注入真实事件序列后来回切 Turn/Grouped，切换前后面板内容无损（`before→after` 保留、`logLen` 不回零）。
  - 插话去重：WS 探针实测 `interject` 来源 `queued_run=1 / 气泡=1`，`chat`-while-running 来源 `queued_run=0`，不再重复。
  - TODO 实时：合成 `file_read` 的 tool_result 不触发刷新、`todo_update` 触发一次；真实多步任务面板 `pending→in_progress→completed` 中间态均及时呈现，刷新稳定早于 5s 轮询边界（最大提前 ~1.66s）。
