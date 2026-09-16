# 记忆系统收敛 MVP（recall-first，无 embedding）

## Problem Statement
如何在不引入 embedding/vector index 的前提下，让 deep memory 的**召回更准**、且不再只增不减、可控治理？

## Recommended Direction
以「**相关性重排的召回** + **pin 语义收紧** + **软归档 GC**」三件事为核心，本轮不引入 embedding。

1. **召回变准（重点）**：给 `deep_permanent_block` 增加 `query` 参数，排序从「纯价值 V」改为「价值 × 相关性加权」，复用现有 `extract_search_keywords`（CJK bigram 分词）做确定性 keyword-relevance，无 embedding 成本。
2. **pin 语义**：`deep_memory remember` 默认改为 `Agent`（可退火可 GC）；仅当用户显式说"记住/保存"才置 `User`。
3. **解"注入续命"**：`deep_permanent_block` 注入后不再无条件 touch——只 touch「与当前问题相关（relevance≥阈值）」的事实，冷门条目自然退火。
4. **软归档 GC**：新增 `archived` 列，满足 `非User ∧ importance<阈值 ∧ last_accessed<now-tau` 的条目 soft-archive（不物理删，可 `deep_restore` 回滚），并从活跃召回/投影中隐藏。后台 curator 跑完顺带执行。

## Key Assumptions to Validate
- [ ] 无 embedding 的 keyword-relevance 重排在 IR/运维场景能否让召回精度可感知提升（用真实问答人工核对）
- [ ] 解 touch 后，活跃常用事实仍能稳定驻留（不误退火）
- [ ] `value() × (1+2·rel)` 的加权系数是否合理（先给保守默认，可调）

## MVP Scope（本轮 in）
- `deep_permanent_block(scope, query, p_max, tau_days)` 相关性重排注入 + 返回 `relevant_ids`
- `remember` 默认 `Agent` pin + 工具说明更新
- 注入去 touch / 用时才 touch（server 只 touch `relevant_ids`）
- soft-archive GC（`archived` 列 + `deep_archive_stale`）+ MEMORY.md 投影分组 Top-N/折叠/归档区

## Not Doing（本轮 out）
- ❌ embedding / vector index / hybrid vector recall — 用户已确认本轮不引入
- ❌ 召回评测集搭建 — 用户已确认忽略，改用真实问答人工核对
- ❌ 多层记忆/浅层记忆复活 — 维持已删除状态
- ❌ 物理删除 — 一律软归档，保证可恢复

## Open Questions
- 软归档默认 `tau`（建议 60 天）与 `importance` 阈值（建议 <3.0）
- MEMORY.md 折叠后是否需要"展开全部"入口（本轮先给出数量行）
