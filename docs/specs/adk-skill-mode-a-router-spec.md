# ADK-Skill 模式 A 落地说明书（模型自路由 · 零新依赖）

> 目标：采用 ADK 标准 skill 规范与「语义渐进加载」思路，**不链接 adk-rust 运行时、不引入 embedding 依赖**，
> 用「模型自路由」替代 FoxIR 现有的词面 token 打分，渐进完成对 `src/skill/` 的自研技能体系的完整替换。
> 对照对象：Claude Code / OpenClaw 的技能加载模型（LLM as router + 冷加载）。

---

## 0. 结论摘要

- **必要性与可行性均成立**：现有 `triggers/词面打分` 是封闭自研体系，与 ADK 标准不兼容；改造后与主流智能体对齐，零新依赖。
- **语义由模型承担**：LLM 本身就是语义引擎。给模型一个紧凑目录 `name + description + when_to_use`，由模型自判命中哪个 skill，再按需读正文——这就是 Claude Code/OpenClaw 同款机制，**不需要 embedding**。
- **embedding 只是后续可选优化**（技能规模大、或要离线确定性排序时再引入，feature-flag 灰度）。
- 保留 FoxIR 既有价值：`always`(常驻)、`curated`(只读)、`enabled`(开关)、版本号与同名去重、自改进、skill 激活时抑制同轮 SOP、全模式注入。

---

## 1. 目标与非目标

### 目标
1. schema 统一到 ADK 标准（`name/description/when_to_use/steps/tool`）＋ `X-foxir` 扩展字段，平滑迁移现有 9 个技能。
2. 匹配层从「token 重叠打分」改为「模型自路由」（紧凑目录 + 模型自判 + 按需加载）。
3. 保持注入骨架、冷加载、SOP 抑制、自改进、前端管理不变。
4. 全程零新增运行时依赖（`glob`/`serde_yaml` 沿用）。

### 非目标（本期不做）
- 不链接 adk-rust 运行时。
- 不引入 embedding / 向量库。
- skill 自带 `tool:` 动态注册 与 `before/after_send_hook`（列为后续 Phase 3，单独评审）。

---

## 2. Schema：ADK 标准 + X-foxir 扩展

### 2.1 目标 frontmatter 形态
```yaml
---
name: VulnerabilityPrioritization
description: Use when triaging/scoring/prioritizing CVE vulnerabilities...
when_to_use: CVE intake, scanner results, KEV updates, patch urgency decisions
steps:
  - label: CVE Intake & Enrichment
  - label: Reachability Assessment
  # ...（可选，结构化工作流，利于遵循率）
# X-foxir 扩展（FoxIR 特有，不影响 ADK 标准字段）
x-foxir:
  enabled: true       # 开关
  always: false       # 常驻热注入（IR 关键技能可 true）
  curated: false      # 只读保护（自改进不可改）
---
```

### 2.2 字段映射（新旧对照）
| 现有字段 | ADK 标准 | X-foxir 扩展 | 说明 |
|---|---|---|---|
| `name` | `name` | — | 保留，唯一名（去重依据） |
| `description` | `description` | — | 模型自路由目录的主力 |
| `triggers` | — | `x-foxir.triggers`（弃用） | 仅作旧数据兼容/降级回退，非主选路 |
| `when_to_use` | `when_to_use` | — | 路由目录的补充 |
| `steps` | `steps` | — | 结构化工作流（`src/skill/steps.rs`） |
| `tool` | `tool` | — | 本期仅解析占位，Phase 3 启用 |
| `enabled` | — | `x-foxir.enabled` | 开关 |
| `always` | — | `x-foxir.always` | 常驻热注入 |
| `curated` | — | `x-foxir.curated` | 只读保护 |
| `version` | `version` | — | 版本号（缺省占位 1.0.0） |

### 2.3 类型与解析（`src/skill/types.rs` / `mod.rs`）
- `SkillMetadata` 增补 `when_to_use(已有)`、`steps: Vec<StepItem>`、`tool: Option<String>`，新增嵌套 `XFoxir { enabled, always, curated, version }`。
- `parse_skill_frontmatter` 做**双解析兼容**：优先 ADK 字段；读不到时回退旧字段（`enabled/always/curated/triggers`），保证迁移期零破坏。
- 沿用：`reload()` glob、去重（按 name）、版本缺省 `1.0.0`。

---

## 3. 匹配层：模型自路由（替换词面打分）

### 3.1 现状（要替换）
- `find_matching_with` / `score_skill`：词面 token 重叠（name×4/desc×2.5/triggers×2/body×1）× `sqrt(body_tokens)` 归一化 → top-K hot。
- 依赖 `triggers` 精确词表，近义/隐式意图命中差。

### 3.2 新模式：模型自路由
`build_skills_prompt(strategy)` 改为：
1. **常驻热**：`x-foxir.always == true` 的技能全文内联（IR 关键技能不受路由影响，绝不被“遗忘”）。
2. **路由目录**：其余 enabled 技能输出紧凑目录，每项 `name: <一行 description/when_to_use>`。
3. **指令**：让模型按用户意图自判应加载哪些技能，并用 `skill_read_file` 按需读正文。
4. **task_skill_active 与 SOP 抑制**逻辑原样保留（读到路由命中即激活）。

### 3.3 策略分层与降级
| 路由方式 | 说明 | 用途 |
|---|---|---|
| `always` 常驻 | 固定热注入 | IR 核心技能保底 |
| 模型自路由（默认） | 读目录自判 + 按需读 | 主选路，零依赖 |
| 词面回退（可选） | 保留旧 `tokenize/score_skill` | embedding/LLM 目录不可用时的兜底 |

- `SkillListingStrategy` 四态（Query/NamesOnly/DiscoverToolOnly/Disabled）不变。
- 加 `X-ROUTER` feature-flag：`llm`(默认) / `lexical`(旧逻辑, A/B 对照) —— 便于灰度与指标对比。

---

## 4. 保留不变的流程/规则（回归红线）

- 每轮 `build_system_prompt` 统一注入（Instant / Expert executor / Orchestrator worker / CRON）——`src/agent/llm_agent.rs`。
- hot 内联 + cold 目录 + `skill_read_file` 按需读取的总体编排形态。
- `curated` 只读：自改进 `improve_skill` 拒绝改写 curated。
- `enabled` 开关、版本号显示、同名去重（前端卡片唯一名）。
- skill 激活时抑制同轮 SOP 的去重规则。
- 依赖面不变（无新库）。

---

## 5. 新增/扩展能力（本期可选、收益最大）

- **skill 自带 `tool:`（Phase 3，另评审）**：skill 声明工具 → 模型 load 该 skill 时把工具**动态注册进当前工具集**，纳入 allowlist/权限审批。这是 ADK 相对现状的最大增量，独立灰度（调用成功率风险最高）。
- **`before/after_send_hook`（Phase 3）**：挂在 skill 上，可在每轮进出消息时改写，作为现有“语言规则/语气注入”的抽象收口。

本期先只做 schema + 模型自路由，`tool:`/hook 不进入本次改动，避免一次引入过多调用面风险。

---

## 6. 现有 9 技能迁移清单

目录：`{workspace}/skills/`（`IncidentTriage` / `MalwareAnalysis` / `PcapAnalysis` / `PhishingAnalysis` / `FullHunt` / `VulnerabilityPrioritization` / `browser-skill` / `archify-rs` / `huashu-design`）

1. frontmatter 改 ADK 标准：`description` 精简为一句路由摘要 + `when_to_use`（若原 `/Overview` 已有 When to Use 段落，抽取要点）。
2. 补 `x-foxir` 扩展：`always` 给 IR 核心（IncidentTriage/MalwareAnalysis/PcapAnalysis/PhishingAnalysis/FullHunt）置 `true`（保底常驻），`curated` 按现状。
3. `steps` 若正文已分阶段（如 IncidentTriage 的 Phase 1..4），抽取为结构化 `steps`（利于遵循率与 `verify.rs`）。
4. 迁移后跑 `cargo test skill` + `/api/skills` 与 `loadSkills` 复核卡片、去重、版本显示。

---

## 7. 前端对齐（`static/index.html`）

- 卡片仍按 `name` 去重 + 版本徽标（占位 `v1.0.0`）。
- 卡片新增展示 `when_to_use`（替代仅 description），`steps` 数量/结构摘要可选。
- toggle/delete 仍按（去重后的）`name` 传参，`/api/skills` 输出字段名同步（`x-foxir.enabled` → 旧 `enabled` 兼容映射，前端照旧）。

---

## 8. 指标埋点（三期对比，用数据定论）

| 指标 | 定义 | 埋点 |
|---|---|---|
| 加载率 | 正确技能被选中进入上下文的比例 | 命中/漏加载计数（每轮） |
| 遵循率 | 加载后工作流步骤完成度 | 复用 `src/skill/verify.rs` 步骤校验 |
| 调用成功率 | 技能相关工具调用 ok vs error | 含「权限拒绝/未注册/解析失败」分类 |

三态灰度：`off(现状) → llm-router(默认) → 有 embedding(可选)`，逐态对比上表，量化“A 改造值不值、embedding 值不值”。

---

## 9. 落地顺序（建议）

1. **P1 schema**：frontmatter → ADK + `x-foxir`，双解析兼容，9 技能迁移，前端字段映射。回归 `cargo test skill`。
2. **P2 路由**：`build_skills_prompt` 改模型自路由 + `X-ROUTER` feature-flag（默认 llm，保留 lexical 对照）。加三指标埋点。
3. **P3（另行评审）**：skill 自带 `tool:` 动态注册 + hooks；独立灰度与调用成功率加固。

---

## 10. 风险与对策

| 风险 | 对策 |
|---|---|
| 模型自路由吞 token / 时延 | 目录保持紧凑（一行 description）；`always` 常驻减少路由负担 |
| 假阳性（弱相关卷入） | 靠模型自判 + 用户意图；可选 top-N 提示 |
| 迁移期字段缺失 | 双解析兼容，旧字段兜底 |
| skill 自带工具调用失败（P3） | 独立灰度 + allowlist/权限接线 + 分类埋点 |
| 自改进破坏标准字段 | `curated` 保护 + 自改进器写标准 frontmatter + bump `version` |

---

## 附：零新依赖声明
仅依赖现有 `glob`（发现）、`serde_yaml`（frontmatter，已存在）、`serde`。不新增 embedding/向量/框架依赖；不链接 adk-rust 运行时。
