# Log — 可插拔记忆后端（MemoryProvider）

## 2026-09-22 — draft 初稿
- 应要求撰写 SDD（spec.md + tasks.md），**仅文档、未实施**。
- 定位：为 FoxIR 提供协议无关 `MemoryProvider` 接入口；MCP 优先 + 声明式连接器实现"不嵌入"；首个非嵌入后端为本地 mem0。
- 关键设计：写方法 `memorize` 返回 `ADD/UPDATE/INVALIDATE/NOOP` 决策（对齐 mem0 与现有 curator 的"更新矛盾事实而非重复存储"）；单一 active 写路径 + 读 fan-out；性能零回归硬约束（异步写、缓存读、门控、预算打包）。
- 基线：`codex/multi-session` @ `b542a35`。