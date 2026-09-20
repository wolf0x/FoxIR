## FoxIR v1.0.13 — 主 Chat 保护 + 语言规则重构 + 历史防串台 + 技能去重/版本展示

本次发布再归并一批小修复与优化：多会话主 Chat 保护与历史加载防串台、SKILL 重构（同名去重 + 版本徽标）、语言规则持久化。

### 主 Chat 会话保护（会话管理）
- **主 Chat 恒常存在且不可删除**：SessionMeta 新增 main 标记，加载/创建时若无 main 则自动提升最早的非删除会话（ensure_main），soft_delete 拒绝删除 main。
- **Sessions 列表只显示子会话**：sessions_list 暴露 main，前端过滤后只有用户创建的子会话出现在导航栏。
- **启动默认落在主 Chat**：删除正在使用的子会话后自动切回主 Chat。
- 集中点：src/session.rs、src/server.rs、static/index.html。

### 历史加载防串台（多会话隔离）
- **加载过期守卫**：loadHist 用请求时的 SESSION_ID 锚点，中途切换会话时丢弃过期响应，避免 A 对话被渲染到 B 或覆写为空。
- **富快照判定收紧**：switchSession 仅在持有“带工具卡的富快照”时才不重载，浅快照回归服务器权威历史。
- 集中点：static/index.html。

### SKILL 重构（技能管理）
- **同名去重**：SkillManager::reload 抢入 insert_skill_unique，按名（大小写不敏）去重，同名多目录保留最浅下（顶层）规范目录，避免版本副本生成重复卡片。
- **版本标录**：skill 卡片名后追加版本号徽标，未指定时缺省占位 v1.0.0；agent 自改进（bump_version）后自动更新。
- 集中点：src/skill/mod.rs、static/index.html。

### 语言规则持久化（修“推理英文、总结中文”）
- 优先读 USER.md 的 Language: 行；显式申明回写持久化（set_user_md_language）；非打招呼时逐轮镜像用户语言。
- 覆盖思考/推理、工具调用、todo/tasks、权限审批、回复正文，全模式生效。
- 集中点：src/agent/llm_agent.rs、src/config.rs。

### 百度/规格
- 新增技能重构规格文档：docs/specs/adk-skill-mode-a-router-spec.md（ADK 规范 + 模型自路由计划）。

### 验证
- cargo check / cargo build --release 通过（仅剩既有无关 warning）。
- 重新打包 output/FoxIR-v1.0.13-win64.zip（仅含根级 FoxIR.exe）。
