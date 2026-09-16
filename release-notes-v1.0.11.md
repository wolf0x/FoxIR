## FoxIR v1.0.11 — 每会话独立模式 + 会话级 STOP + 更自然的语气

本次发布聚焦多会话的两块收尾（模式隔离与停止控制），并按 v3 政策把 AI 的回复从“生硬播报”改成“温暖帮朋友”。

### 多会话：每会话独立模式（P1）
- **切会话不再串模式**：每个 session 现在有**自己的** Instant/Expert 模式（`managedModes[sessionId]`）。从 A 开 Expert 切到 B 时，B 回到默认 Instant，不再误用 A 的模式。
- **新建会话默认 Instant**：`newSession` / `switchSession` 都按当前会话各自的模式恢复，不再依赖全局开关。
- 现状集中：`static/index.html` 的 `currentSessionMode()` + `setManagedMode()`。

### 会话级 STOP（P1）
- **Instant 与 Expert 都能被单会话 STOP 停住**：`drain_session_stream` 的停止判断改为“会话级标志 **或** 连接级标志”，断开连接仍能终止 Expert 任务，同时会话 STOP 能直达 Expert。
- **每个 run 都注册进会话取消表**：`session_cancel` 不再只对 Instant 生效，Expert 任务也走同一调度；容量满时回退连接级取消。
- **Expert drain 不再阻塞 demux 循环**：Expert（managed）改为 `tokio::spawn` 独立 drain，主循环保持响应，其他会话可继续工作。
- 现状集中：`src/server.rs`。

### 语气优化（v3 温暖日常版）
- `llm_agent.rs` 新增 **`## 像个人一样说话（总纲）`**：结果放前面、语气温暖自然、开口前先在心里过一遍用户要什么。
- 封禁反模式：客服腔（“正在为您查询，请稍候”“这是实时状态，我重新查一下”）、每句喊名字、念内部流程/工具名/文件名/技能名、模板句复用、硬装确定。
- 给出自然带话（“刚看了一眼，现在是……”“我重新确认了下，情况是这样”）与复用依据（“基于刚才查到的厂商公告，结论是……”）。

### 验证
- `cargo check` 通过；`cargo build --release` 成功。
- `cargo test --lib`：278 通过；3 个 `tool::todo_update` 会话隔离用例为既有失败（`todos-main-*.json` 路径断言），与本次改动无关。
