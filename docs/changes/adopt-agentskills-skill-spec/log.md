# adopt-agentskills-skill-spec · 执行日志

> 记录每次实施的开始/结束、证据、发现与回退。

| 时间 | Phase | 动作 | 结果/证据 | 备注 |
|---|---|---|---|---|
| 2026-09-21 | 决策 | 用户指示不保留任何 `x-foxir` 旧字段 | spec 重写为纯 agentskills.io schema | 覆盖原“x-foxir 扩展”方案 |
| 2026-09-21 | P1 | `SkillMetadata` 重写为纯 agentskills.io | `license/version/platforms/deps/allowed-tools`；删除 `triggers/always/when_to_use/curated`；`enabled` 保留为运行时开关 | types.rs |
| 2026-09-21 | P1 | 移除 `always`/`when_to_use` 路由、`triggers` 打分；`HOT_MIN_SCORE` 降至 2.0 | `build_skills_prompt` / `score_skill` / install / list 工具 | mod.rs |
| 2026-09-21 | P1 | 移除 `curated` 只读保护 | `should_improve`、`improve_skill`、相关单测 | self_improve.rs |
| 2026-09-21 | P1 | 管理侧 + 前端去 `triggers` | `manager.rs` 去掉 triggers；`index.html` 卡片改 platforms/deps | — |
| 2026-09-21 | P1 | 编译与单测验证 | `cargo check` 通过；`cargo test --lib skill::` 28/28 通过 | — |
| 2026-09-21 | P2 | `build_skills_prompt(Query)` 改纯目录模型自路由 | 无正文内联；`HOT_MIN_SCORE` 删除；`task_skill_active` 恒 false | mod.rs |
| 2026-09-21 | P2 | 保留 `find_matching` 仅给 Expert executor 预注 | `score_skill`/`tokenize` 仍在 Expert 路径使用；主 Instant 路径不再用词面打分 | runner.rs |
| 2026-09-21 | P3 | `platforms`/`deps` 可用性门控 | `unavailable_reason` + 注册时跳过不兼容技能；29/29 测试 | mod.rs |
| 2026-09-21 | P4 | 迁移 9 个运行时技能 frontmatter（去 triggers） | 备份 output/skills-migration-*/；huashu 中文/版本完好；无 triggers 残留 | ${workspace}/skills |
| 2026-09-21 | P5 | 遥测计量 + 端点 + 前端 | `src/skill/metrics.rs`；`GET /api/skills/metrics`；技能页遥测行 | metrics.rs / server.rs |
| 2026-09-21 | 决策 | 不重加 `lexical` 回退模式 | 与移除词面打分方向一致；`router_mode:"llm"`；listing 策略仍可调 | — |
| 2026-09-21 | 外部 | 安装 anthropics skill-creator（去 Claude 化） | 改写为 agentskills.io frontmatter（name/description/version/platforms）；安装于 workspace/skills/skill-creator；结构校验 + FoxIR 解析单测 31/31 通过 | workspace/skills/skill-creator |

## 数据（灰度对比）

| 状态 | 加载率 | 遵循率 | 调用成功率 | 备注 |
|---|---|---|---|---|
| off(现状) | — | — | — | 待采集 |
| llm(自路由) | — | — | — | 待采集 |

## 回退记录
（无）
