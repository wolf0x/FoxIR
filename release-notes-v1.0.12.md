## FoxIR v1.0.12 — 主 Chat 会话保护 + 语言规则重构 + 历史加载防串台

本次发布聚焦三块：一个不可删除的主 Chat 会话、统一的语言规则（修掉“推理英文、总结却中文”）、以及多会话切换时历史/工具卡不再串台或丢失。

### 主 Chat 会话保护（会话管理）
- **首个会话成为永久“主 Chat”**：SessionMeta 新增 main 标记，最早创建的会话被锁定为主会话，soft_delete 拒绝删除它。
- **Sessions 列表只显示用户额外创建的子会话**：sessions_list 暴露 main，前端 loadSessions 过滤掉主会话，只有子会话出现在导航栏。
- **启动与删除都回到主 Chat**：activateChatView/goChat 保证启动默认落在主 Chat 页；删除正在使用的子会话后自动切回主 Chat。
- 集中点：src/session.rs、src/server.rs、static/index.html。

### 历史加载防串台（多会话隔离）
- **加载过期守卫**：loadHist 用请求时的 SESSION_ID 锚点，中途切换会话时丢弃过期响应，避免 A 的对话被渲染到 B 或覆写为空。
- **富快照判定收紧**：switchSession 仅在持有“带工具卡的富快照”时才不重载，浅快照回归服务器权威历史，避免切回时清空对话。
- 集中点：static/index.html。

### 语言规则重构（修“推理英文、总结中文”）
- **根因**：旧 resolve_default_language 只按字面 English/中文 关键词匹配，内置英文 USER.md 无该词被误判为中文，注入“整篇用中文”；模型推理（内部 CoT）不受其约束，故出现推理英文、总结切中文。
- **新规则**（resolve_language_rule）：① 用户显式申明语言 → 回写持久化到 USER.md（set_user_md_language），并按该语言回复；② 非打招呼时逐轮镜像用户消息语言；③ 打招呼/无信号时兜底 USER.md 默认（user_md_language 读 Language: 行）。
- **覆盖全表面**：思考/推理、工具调用、todo/tasks、权限审批、回复正文（标题/列表/表格/问候/收尾）统一同语言，且删除“强制整篇某语言”的写死句。
- **全模式生效**：Instant(主 Runner)、Expert(ManagedRunner Executor)、Orchestrator worker 均经 LlmAgent::run → build_system_prompt 同一条注入点。
- 集中点：src/agent/llm_agent.rs、src/config.rs。

### 验证
- cargo check --release 通过（清除 4 处非法 \u 转义，\r\r\n 归零）。
- cargo build --release 成功（仅剩既有无关 warning）。
- 重新打包 output/FoxIR-v1.0.12-win64.zip（仅含根级 FoxIR.exe）。
