# adopt-agentskills-skill-spec · 实施任务清单

> 按 P1→P5 顺序；每项完成后在对应行打勾并记 log.md。
> 记号：待办 [ ] / 进行中 [~] / 完成 [x]

## P1 · schema + 解析（无旧兼容）
- [x] `src/skill/types.rs::SkillMetadata` 重写为纯 agentskills.io（`license/version/platforms/deps/allowed-tools`）；删除 `triggers/always/when_to_use/curated`；`enabled` 保留为运行时开关
- [ ] 新增 `agentskills` frontmatter 结构体（未 tagged `compatibility`，支持 `{os,deps}` 或自由串）
- [x] `src/skill/mod.rs::parse_skill_frontmatter` 走新 schema（serde_yaml，`name` 必填复核）
- [x] 编译：`cargo check` 通过；`cargo test --lib skill::` 28 项全绿

## P2 · 模型自路由
- [x] `build_skills_prompt(Query)`：改为纯目录（`name:description`），模型自路由 + `skill_read_file` 按需读正文；无自动内联
- [x] 移除 `score_skill`/`tokenize`/`HOT_MIN_SCORE` 于主路由；`HOT_MIN_SCORE` 常量删除（`find_matching` 仅保留给 Expert executor 预注）
- [x] 保留 `task_skill_active`（现恒定 false）与 SOP 抑制逻辑
- [x] 保留 `skill_read_file` 按需读取
- [x] 单测：目录仅含 catalog、无正文内联、路由不激活标志（`build_skills_prompt_catalog_only`）

## P3 · 可用性门控 + 前端
- [x] `platforms` 按 OS 过滤（`current_os_platform`）；不匹配则不注册并 warn 原因
- [x] `deps` 存在性探测（PATH/exe 检查，`command_on_path`）；缺失不注册
- [x] 门控原因落地为日志（`unavailable_reason`），不注册不可用技能
- [x] `static/index.html` 卡片对齐：name + version + platforms + deps 徽标（`triggers` 已替换）
- [x] 前端按新字段 toggle/delete（按 name，去重；后端 delete/toggle/improve 统一为大小写不敏感匹配）

## P4 · 迁移现有 9 技能
- [x] 9 个运行时技能（${workspace}/skills/*）frontmatter 已清洗：去 `triggers`，保留 name/description/version；备份于 output/skills-migration-*/
- [x] VulnerabilityPrioritization 保留 version 5.0.0；huashu-design 中文描述完好
- [x] 迁移后验证：`cargo test skill` 29/29；无 `triggers` 残留

## P5 · 指标与灰度
- [x] 遥测计数器：catalog 轮次 / read 调用 / read 失败 / 加载失败 / 自改进（`src/skill/metrics.rs`，持久化 `.metrics.json`）
- [x] 端点：`GET /api/skills/metrics`；技能页渲染遥测行（router: llm）
- [~] feature-flag：遥测含 `router_mode:"llm"`；未重加 `lexical` 回退（与去词面打分方向一致，listing 策略仍可调）
- [~] 灰度对比数据待运行采集后归档到 log.md

## 验收门（对应 spec §7）
- [ ] AC1..AC8 全部通过
