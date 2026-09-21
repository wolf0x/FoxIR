# adopt-agentskills-skill-spec · 实施任务清单

> 按 P1→P5 顺序；每项完成后在对应行打勾并记 log.md。
> 记号：待办 [ ] / 进行中 [~] / 完成 [x]

## P1 · schema + 解析（无旧兼容）
- [x] `src/skill/types.rs::SkillMetadata` 重写为纯 agentskills.io（`license/version/platforms/deps/allowed-tools`）；删除 `triggers/always/when_to_use/curated`；`enabled` 保留为运行时开关
- [ ] 新增 `agentskills` frontmatter 结构体（未 tagged `compatibility`，支持 `{os,deps}` 或自由串）
- [x] `src/skill/mod.rs::parse_skill_frontmatter` 走新 schema（serde_yaml，`name` 必填复核）
- [x] 编译：`cargo check` 通过；`cargo test --lib skill::` 28 项全绿

## P2 · 模型自路由
- [ ] `build_skills_prompt(Query)`：命中技能全文内联 + 其余输出 `name:description` 紧凑目录（当前：词面命中内联保留，`always` 已移除）
- [ ] 移除 `score_skill` / `tokenize` / `HOT_MIN_SCORE`；`find_matching` 归档或删除
- [ ] 保留 `task_skill_active` 与 SOP 抑制逻辑
- [ ] 保留 `skill_read_file`/`read_skill` 按需读取
- [ ] 单测：命中内联、目录不含正文、路由命中激活标志

## P3 · 可用性门控 + 前端
- [ ] `platforms` 按 OS 过滤（windows/macos/linux 判定）；不匹配则不注册
- [ ] `deps` 存在性探测（PATH/exe 检查）；缺失不计入目录
- [ ] 门控结果透出（不可用原因）到 `/api/skills`
- [x] `static/index.html` 卡片对齐：name + version + platforms + deps 徽标（`triggers` 已替换）
- [ ] 前端按新字段 toggle/delete（按 name，去重）

## P4 · 迁移现有 9 技能
- [ ] VulnerabilityPrioritization 改 agentskills.io frontmatter（去 triggers）
- [ ] IncidentTriage / MalwareAnalysis / PcapAnalysis / PhishingAnalysis / FullHunt（IR 核心）
- [ ] browser-skill / archify-rs / huashu-design（清洗多余字段）
- [ ] 迁移后跑 `cargo test skill` + `/api/skills` + `loadSkills` 回归（去重/版本）

## P5 · 指标与灰度
- [ ] 加载率埋点（正确命中/漏加载）
- [ ] 遵循率埋点（复用 `verify.rs` 步骤校验）
- [ ] 调用成功率埋点（含权限/未注册/解析失败分类）
- [ ] feature-flag：`X-ROUTER = llm(default) | lexical`，可一键回退
- [ ] 三态灰度对比数据归档到 log.md

## 验收门（对应 spec §7）
- [ ] AC1..AC8 全部通过
