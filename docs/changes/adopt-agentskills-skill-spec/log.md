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

## 数据（灰度对比）

| 状态 | 加载率 | 遵循率 | 调用成功率 | 备注 |
|---|---|---|---|---|
| off(现状) | — | — | — | 待采集 |
| llm(自路由) | — | — | — | 待采集 |

## 回退记录
（无）
