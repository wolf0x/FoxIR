# Tasks — 可插拔记忆后端（MemoryProvider）

> **状态**: 未实施，等待 spec.md 审核。审核通过后按 P0→P3 顺序执行。
> 每个 P 完成后更新本文件打勾并跑对应测试。

## P0 — 接入口骨架（回归安全基线）
- [ ] 定义 `MemoryProvider` trait（§3）
- [ ] `MemoryDraft` / `MemoryWriteDecision` / `MemoryHit` / `ProviderHealth` 类型
- [ ] `LocalProvider`（包装现有 `MemoryStore`，行为不变）
- [ ] `[memory]` config：`active` / `fallback`（默认 local）
- [ ] 现有注入点（`llm_agent.rs` 的 `[Memory Context]/[Memory Recall]`）改走 trait，`active=local` 时输出与现状一致
- [ ] 单元测试：trait 单测 + LocalProvider 回归（全库现有 memory 测试不破）
- [ ] 验收红线：§7 性能约束落地（异步写通道、缓存读、门控、预算打包）

## P1 — MCP 适配器 + 本地 mem0（第一个非嵌入后端）
- [ ] `McpMemoryAdapter`：把 `recall/count/memorize` 映射到 MCP 工具，工具名可配置
- [ ] mem0 本地部署说明（自托管 Compose / OpenMemory；embedding=本地 Ollama/FastEmbed；LLM=现有 DeepSeek）
- [ ] `mcp_servers.json` 注册 mem0；`[memory].active = "mcp:mem0"`
- [ ] 外部 Down → `fallback=local`，agent 不中断
- [ ] 写门控/节流（只写有意义/显式/蒸馏事实）
- [ ] e2e：真机 mem0 写入→召回→审计记录写决策（含 superseded）

## P2 — 声明式连接器 + 管理页
- [ ] `RestMemoryAdapter` + connector spec（JSON：add/search/count/delete 的 URL/方法/鉴权/字段映射）
- [ ] `connectors/*.json` 样例（接回 MemoryHub 这类 REST 服务，不嵌入）
- [ ] 记忆/插件管理 UI：列出候选后端、连接状态、记忆条数、启停、切换 active

## P3（可选，需另评）— 跨机与运维
- [ ] `agent_id/user_id/team_id` 隔离域；跨机托管
- [ ] GC/去重策略（provider 无关，做在 scope 层）
- [ ] 远端审计/凭据加密走既有路径