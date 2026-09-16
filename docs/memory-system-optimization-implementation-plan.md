# FoxIR 记忆系统优化施工方案

## 1. 目标

本方案基于当前 FoxIR 源码中的实际记忆实现制定，目标是把现有“多重写入 + 多重召回”升级为一个可治理、可评估、可控预算的统一记忆系统。

当前系统已经具备：

- `conversations`：自动保存每轮对话。
- `summaries`：每日 extractive summary。
- `conversations_fts`：FTS5 + BM25 检索。
- `deep_facts`：长期事实记忆。
- `spawn_deep_curator`：后台 LLM curator 自动提炼 durable facts。
- `MEMORY.md`：`deep_facts` 的只读投影。
- `context_arbiter.rs`：上下文预算仲裁器雏形。

主要改造目标：

1. 统一写入入口。
2. 明确 User pin / Agent pin / 普通事实的边界。
3. 引入写入评分、敏感信息过滤、去重合并。
4. 改造召回为 hybrid recall。
5. 让 deep memory 注入与当前任务相关。
6. 用统一 Context Arbiter 管理所有记忆块预算。
7. 增加存储 GC。
8. 建立可重复的召回评测集。

## 2. 现状问题

### 2.1 召回相关性不足

当前 `deep_memory` 工具的 `recall` 和 `deep_search_keyword` 主要依赖字符串包含或关键词匹配；自动记忆依赖 FTS5/BM25。没有语义召回，用户换一种说法容易漏召回。

### 2.2 deep permanent block 不看当前任务

`deep_permanent_block(scope_key, p_max, tau_days)` 当前只按 `f.value(now)` 排序，主要由 importance、last_accessed、time decay 决定，不使用当前 user message 的 relevance。

结果：全局重要但当前无关的事实可能常驻注入。

### 2.3 记忆块预算分散

当前运行时分别注入：

- `build_context_string(7)`
- `build_session_recall_block_default`
- `build_auto_recall_block`
- `deep_permanent_block`

这些块各自有上限，但没有进入统一预算池竞争。

### 2.4 缺少长期存储 GC

`deep_facts` 有退火和注入过滤，但没有系统性删除、归档、合并或压缩。`conversations` 和 `summaries` 也没有明确 retention。

### 2.5 pin 策略过强

`deep_memory remember` 默认 `PinnedBy::User`，这适合用户显式要求“记住”，但不适合 agent 自动保存的事实。Agent 自动提炼的事实应默认可退火、可 GC。

### 2.6 curator 缺少写入治理

`spawn_deep_curator` 依赖 LLM 判断 durable facts，最多提炼 3 条，但缺少独立的：

- WriteScore
- 敏感信息拦截
- TTL
- 类别配额
- 总量预算
- 更可靠去重

## 3. 目标架构

推荐改造成以下管线：

```text
Conversation / Tool Result / User Preference
        |
        v
Memory Candidate Queue
        |
        v
Pre-filter
  - transient filter
  - sensitive filter
  - category classifier
  - source classifier
        |
        v
Write Scoring + Dedup + Merge
        |
        v
Memory Store
  - deep_facts
  - conversations
  - summaries
  - optional memory_embeddings
        |
        v
Hybrid Recall
  - BM25
  - keyword/entity
  - vector similarity
  - recency/importance rerank
        |
        v
Context Arbiter
  - relevance
  - value
  - token cost
  - utility density
  - degradation
        |
        v
Prompt Memory Context
```

## 4. 施工阶段

## Phase 1：收敛写入语义

### 目标

先不大改数据库，把写入边界理清，避免无约束增长。

### 改动点

#### 1. 修改 `deep_memory remember` 默认 pin

文件：

- `src\tool\deep_memory.rs`

当前逻辑：

```rust
let pinned = match args["pinned"].as_str() {
    Some("agent") => PinnedBy::Agent,
    _ => PinnedBy::User,
};
```

建议改为：

```rust
let pinned = match args["pinned"].as_str() {
    Some("user") => PinnedBy::User,
    Some("agent") => PinnedBy::Agent,
    _ => PinnedBy::None,
};
```

规则：

- 用户明确说“记住 / 保存这个 / 以后按这个来”：`PinnedBy::User`
- curator 自动写入：`PinnedBy::Agent` 或 `PinnedBy::None`
- 普通推断：默认不 pin

#### 2. 修改工具描述

把 `deep_memory` 描述中的“User-stated facts pin to User”改成更精确：

```text
Only explicitly user-requested persistent facts should be pinned to User.
Agent-curated facts should use Agent or None so they can decay and be garbage-collected.
```

#### 3. 增加写入来源字段

短期可先不改 schema，用 `tags` 存：

```text
source:user_explicit
source:agent_curated
source:tool_observed
```

长期建议给 `deep_facts` 增加字段：

```sql
source TEXT NOT NULL DEFAULT 'agent_curated'
ttl_days INTEGER
access_count INTEGER NOT NULL DEFAULT 0
updated_at INTEGER
```

### 验收标准

- 用户显式保存的事实才是 `pinned_by=user`。
- curator 生成的事实不再不可退火。
- 新写入事实能区分来源。

## Phase 2：候选记忆与写入评分

### 目标

让 curator 不直接写入 `deep_facts`，而是先进入候选评估。

### 新增模块

建议新增：

- `src\memory_policy.rs`

职责：

- transient 判断
- sensitive 判断
- write score 计算
- TTL 分配
- pin 策略
- category 规范化

### 推荐数据结构

```rust
pub enum MemorySource {
    UserExplicit,
    AgentCurated,
    ToolObserved,
}

pub struct MemoryCandidate {
    pub content: String,
    pub fact_type: FactType,
    pub subject_key: Option<String>,
    pub source: MemorySource,
    pub confidence: f32,
    pub importance_hint: f32,
    pub tags: Vec<String>,
}

pub struct WriteDecision {
    pub action: WriteAction,
    pub importance: f32,
    pub pinned_by: PinnedBy,
    pub ttl_days: Option<i64>,
    pub reason: String,
}

pub enum WriteAction {
    Store,
    SessionOnly,
    Reject,
    Merge(String),
}
```

### 写入评分

使用 0-1 归一化即可，不建议使用“100 分预算”。

```text
WriteScore =
  0.30 * long_term_usefulness
  + 0.20 * explicitness
  + 0.20 * stability
  + 0.15 * future_reuse
  + 0.15 * confidence
  - 0.35 * sensitivity_penalty
  - 0.25 * volatility_penalty
```

默认阈值：

```text
>= 0.70: Store
0.50-0.69: SessionOnly
< 0.50: Reject
```

### transient 拦截规则

直接拒绝长期写入：

- 当前进程
- 当前 IP
- 当前磁盘空间
- 当前 CPU/内存
- 一次性 CVE 查询结果，除非用户要求长期保存
- 临时路径
- 临时日志片段
- 单次工具输出

### 敏感信息拦截规则

默认拒绝写入：

- password
- token
- cookie
- api key
- private key
- secret
- credential
- access key
- refresh token
- 钱包私钥

注意：当前 curator prompt 里包含 “credentials, accounts”，这需要收紧。

建议把 prompt 改为：

```text
Do not store secrets, passwords, tokens, private keys, session cookies, wallet credentials, or raw credentials.
For infrastructure references, store only non-secret identifiers needed for future work.
```

### 验收标准

- curator 输出不会直接落库。
- 候选事实经过 policy 决策。
- 敏感信息不会进入 `deep_facts`。
- transient 状态不会进入长期记忆。

## Phase 3：去重、合并与 subject_key 规范化

### 目标

减少重复记忆和同义事实膨胀。

### 当前问题

已有：

- subject_key 覆盖
- Jaccard 近似去重

不足：

- subject_key 由 LLM 自由生成，不稳定。
- Jaccard 对中文、短句、同义改写较弱。

### 推荐 subject_key 生成规则

新增函数：

```rust
fn normalize_subject_key(fact_type: FactType, content: &str, provided: Option<&str>) -> String
```

规则：

```text
preference:<normalized-topic>
project:<project-name>:<topic>
identity:<field>
constraint:<scope>:<topic>
reference:<entity>:<topic>
```

示例：

```text
preference:language
preference:reply_style
project:foxir:memory_policy
reference:cve-2026-51990:affected_version
```

### 去重策略

```text
1. subject_key 完全相同：更新旧事实。
2. normalized content 完全相同：跳过。
3. lexical similarity >= 0.85：合并。
4. vector similarity >= 0.88：合并。
5. 0.75-0.88：标记 related，暂不合并。
```

如果暂时不做 embedding，可以先做：

- 中文 bigram overlap
- 英文 token Jaccard
- subject_key canonicalization

### 验收标准

- 同一偏好不会重复写多条。
- 同一 CVE 同一结论不会重复写多条。
- 新事实会更新旧事实的 `updated_at` 和 `last_accessed`。

## Phase 4：Hybrid Recall

### 目标

召回从“关键词匹配”升级为多信号检索。

Hybrid Recall 必须支持三种运行模式：

| 模式 | 说明 | 适用场景 |
|---|---|---|
| Local Embedding | 使用本地嵌入模型生成向量，本地向量索引检索 | 隐私优先、离线优先、IR 场景 |
| Cloud Embedding API | 使用 OpenAI-compatible embedding API 生成向量，本地保存索引 | 快速落地、模型质量优先 |
| Lexical Fallback | 无嵌入模型时退化为 BM25 + keyword + entity + subject_key | 零依赖、离线、嵌入失败兜底 |

原则：

> embedding 是增强能力，不是记忆系统的硬依赖。任何 embedding 初始化、调用、索引损坏或模型缺失，都必须自动退化到非语义召回。

### 推荐召回源

| 来源 | 方法 |
|---|---|
| conversations | FTS5 BM25 |
| summaries | FTS5 或关键词 |
| deep_facts | keyword + subject_key + optional vector |
| current session tail | recency |
| knowledge | existing knowledge search |

### 嵌入模型抽象

新增 trait：

```rust
pub trait EmbeddingProvider: Send + Sync {
    fn name(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Result<Vec<f32>, String>;
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}
```

建议实现：

```text
NoopEmbeddingProvider      # 禁用 embedding，始终返回 unavailable
LocalEmbeddingProvider     # 本地模型，例如 all-MiniLM-L6-v2 / bge-small / fastembed
CloudEmbeddingProvider     # OpenAI-compatible embeddings endpoint
```

配置决定加载哪一种 provider；加载失败时自动降级到 `NoopEmbeddingProvider`。

### 向量索引抽象

新增 trait：

```rust
pub trait VectorIndex: Send + Sync {
    fn upsert(&self, item: &EmbeddingItem) -> Result<(), String>;
    fn delete(&self, id: &str) -> Result<(), String>;
    fn search(&self, vector: &[f32], top_k: usize) -> Result<Vec<VectorHit>, String>;
    fn rebuild(&self, items: &[EmbeddingItem]) -> Result<(), String>;
}
```

推荐先实现：

```text
SqliteVectorIndex
```

即：SQLite 存 embedding BLOB，查询时 brute-force cosine。原因是 FoxIR 记忆规模通常不大，先保证正确性、可调试、可迁移。后续可替换为：

```text
HnswVectorIndex
ZvecVectorIndex
SqliteVecVectorIndex
```

### 可选新增表

如果引入 embedding：

```sql
CREATE TABLE IF NOT EXISTS memory_embeddings (
    memory_id TEXT PRIMARY KEY,
    memory_kind TEXT NOT NULL,
    embedding BLOB NOT NULL,
    model TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
```

建议增强为：

```sql
CREATE TABLE IF NOT EXISTS memory_embeddings (
    id TEXT PRIMARY KEY,
    memory_kind TEXT NOT NULL,
    memory_id TEXT NOT NULL,
    chunk_id TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    embedding_provider TEXT NOT NULL,
    embedding_model TEXT NOT NULL,
    dim INTEGER NOT NULL,
    embedding BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_mem_embed_kind ON memory_embeddings(memory_kind);
CREATE INDEX IF NOT EXISTS idx_mem_embed_memory ON memory_embeddings(memory_id);
CREATE INDEX IF NOT EXISTS idx_mem_embed_hash ON memory_embeddings(content_hash);
CREATE INDEX IF NOT EXISTS idx_mem_embed_model ON memory_embeddings(embedding_model);
```

注意：

- embedding 表是索引层，不是事实源。
- `deep_facts` / `conversations` / `summaries` 仍然是 source of truth。
- embedding 可删除、可重建。
- `content_hash` 变化时需要重新生成向量。

### Chunk 策略

不同记忆源使用不同 chunk：

| 来源 | Chunk 方式 |
|---|---|
| deep_facts | 一条 fact 一个 chunk |
| summaries | 按日期或段落 chunk |
| conversations | 优先 user+assistant 相邻轮次 chunk，不建议单条碎片过细 |
| knowledge docs | 按标题/段落 chunk |

默认限制：

```text
chunk_chars_min = 80
chunk_chars_max = 1200
chunk_overlap = 100
```

短事实无需 overlap。

### 检索接口

不要让业务代码直接绑定 zvec、hnsw 或 SQLite brute-force。统一抽象：

```rust
trait MemoryRetriever {
    fn retrieve(&self, query: &str, limit: usize) -> Vec<MemoryHit>;
}
```

建议拆分：

```rust
pub struct HybridMemoryRetriever {
    lexical: LexicalRetriever,
    vector: Option<Box<dyn VectorRetriever>>,
    entity: EntityRetriever,
    policy: RecallPolicy,
}
```

### 有 embedding 时的召回评分

```text
RecallScore =
  0.35 * semantic_similarity
  + 0.25 * lexical_score
  + 0.15 * importance
  + 0.10 * recency
  + 0.10 * source_confidence
  + 0.05 * pin_boost
  - 0.30 * conflict_penalty
  - 0.30 * sensitivity_penalty
```

### 无 embedding 时的降级评分

如果没有 embedding、embedding 初始化失败、云 API 不可用、向量索引损坏，则使用非语义召回：

```text
RecallScoreFallback =
  0.45 * bm25_score
  + 0.20 * keyword_overlap
  + 0.15 * exact_entity_match
  + 0.10 * subject_key_match
  + 0.05 * importance
  + 0.05 * recency
  - 0.30 * conflict_penalty
  - 0.30 * sensitivity_penalty
```

其中：

| 信号 | 实现 |
|---|---|
| bm25_score | 现有 FTS5 BM25 |
| keyword_overlap | 中文 bigram + 英文 token overlap |
| exact_entity_match | CVE、IP、域名、文件名、项目名、用户指定关键词 |
| subject_key_match | deep_facts 的 canonical subject_key |
| importance | deep_facts importance 或来源权重 |
| recency | last_accessed / timestamp 衰减 |

降级模式必须在日志和调试信息中可见，但不要影响用户正常使用。

### 价值密度

召回进入上下文时必须考虑 token 成本：

```text
UtilityDensity = RecallScore / max(EstimatedTokens, 50)
```

选择策略：

```text
1. 过滤 RecallScore < 0.55。
2. User pinned 且相关的事实优先。
3. 其余按 UtilityDensity 排序。
4. 在 MemoryContextBudget 内装填。
5. 超长内容降级为 summary 或 pointer。
```

### 验收标准

- 同义问题能召回同一事实。
- 无嵌入模型时系统仍可工作。
- 云 embedding 失败时自动退化为非语义召回。
- 本地 embedding 模型缺失时自动退化为非语义召回。
- 长记忆不会挤掉多个短高价值记忆。
- 召回结果包含来源、分数、token cost，便于调试。

## Phase 5：统一 Context Arbiter

### 目标

把所有记忆块交给 `context_arbiter::assemble` 统一预算，不再分散插入。

### 当前分散注入点

文件：

- `src\server.rs`

当前注入：

```rust
build_context_string(7)
build_session_recall_block_default(&session_id)
build_auto_recall_block(&content, 14, 1800)
deep_permanent_block("global", 1024, 60.0)
```

### 推荐改造

新增：

```rust
pub fn build_memory_artifacts(
    store: &MemoryStore,
    session_id: &str,
    user_message: &str,
    context_window: usize,
) -> Vec<context_arbiter::Artifact>
```

每类记忆生成 Artifact：

| 记忆块 | kind | value | relevance |
|---|---|---:|---:|
| deep facts | DeepFact | RecallScore | query relevance |
| session tail | HistoryTurn | recency/evidence value | high |
| auto recall | HistoryTurn | BM25 score | query relevance |
| daily summary | Knowledge/History | low-medium | query relevance |

统一预算：

```text
MemoryBudget = min(2500, max(500, context_window * 0.05))
Reserve = context_window * 0.20
```

然后：

```rust
let mut arts = build_memory_artifacts(...);
let assembled = context_arbiter::assemble(&mut arts, memory_budget, 0);
for block in assembled.blocks {
    history.insert(0, ChatMessage::system(&block));
}
```

### 改造原则

- 不再固定注入 7 天摘要。
- daily summary 只在相关或新会话时作为低优先级 artifact。
- deep facts 必须基于当前 query 计算 relevance。
- session tail 对“继续 / 刚才 / 上面”类请求高相关。
- 所有 artifact 都要有 full / outline / catalog 三档。

### 验收标准

- 每轮记忆注入总量受统一预算控制。
- Dashboard 能显示各类记忆占用。
- 关闭某类记忆不会影响其他类预算计算。

## Phase 6：存储 GC

### 目标

防止记忆库长期膨胀。

### 新增函数

建议新增到 `MemoryStore`：

```rust
pub fn gc_deep_facts(&self, policy: &MemoryGcPolicy) -> Result<MemoryGcReport, String>
pub fn gc_conversations(&self, retention_days: i64) -> Result<usize, String>
pub fn gc_summaries(&self, retention_days: i64) -> Result<usize, String>
```

### deep_facts GC 评分

```text
KeepScore =
  0.25 * importance
  + 0.20 * recall_count
  + 0.20 * recency
  + 0.15 * confidence
  + 0.10 * stability
  + 0.10 * user_explicitness
  - 0.30 * duplicate_penalty
  - 0.40 * conflict_penalty
  - 0.50 * sensitivity_penalty
```

初期没有 `recall_count` 和 `confidence` 时：

- `recall_count = 0`
- `confidence = importance / 5`

### 默认策略

```text
pinned_by=user: 不自动删除，只可提示用户审计
pinned_by=agent: 可降权、归档、合并
pinned_by=none: 可删除
```

### 建议 retention

| 表 | 默认保留 |
|---|---:|
| conversations | 90-180 天 |
| summaries | 180-365 天 |
| deep_facts user pinned | 不自动删除 |
| deep_facts agent/none | 按 KeepScore |
| task_checkpoints | 已有 cleanup |

### GC 动作

| KeepScore | 动作 |
|---:|---|
| >= 0.70 | 保留 |
| 0.50-0.69 | 压缩 summary |
| 0.30-0.49 | 合并到相似事实后删除 |
| < 0.30 | 删除 |

### 触发方式

- 启动时轻量 GC。
- 每周 heartbeat / cron GC。
- deep_facts 超过上限时立即 GC。
- 用户点击“清理记忆”时 GC。

### 默认上限

```text
deep_facts total <= 300
agent_curated <= 200
reference <= 120
project <= 100
preference <= 80
```

### 验收标准

- `deep_facts` 超过上限会自动清理或压缩。
- GC 输出报告包含删除、合并、保留数量。
- User pinned 不被静默删除。

## Phase 7：curator 改造

### 目标

降低误写、重复写、敏感写。

### 当前 prompt 风险

当前 curator prompt 允许提取：

```text
credentials, accounts, infrastructure references
```

建议改为：

```text
Extract only durable, non-sensitive facts useful across sessions.
Do not store passwords, tokens, cookies, private keys, wallet credentials, raw secrets, or full credential material.
For accounts or infrastructure, store only non-secret identifiers when they are necessary for future work.
Ignore transient local state such as current process lists, current IP, disk usage, temporary paths, and one-time command output.
```

### curator 输出增加字段

```json
{
  "facts": [
    {
      "content": "...",
      "fact_type": "identity|preference|project|constraint|reference",
      "subject_key": "...",
      "confidence": 0.0,
      "ttl_days": 180,
      "sensitivity": "low|medium|high",
      "source": "agent_curated"
    }
  ]
}
```

### 写入限制

- 每轮最多 1-2 条，除非用户明确要求保存。
- assistant reply 太短时不写。
- 工具结果不直接写，必须由用户或 assistant 结论确认。
- 安全 IOC 可以写，但应标记 `fact_type=reference`，TTL 较短，例如 90-180 天。

### 验收标准

- curator 不会保存密码/密钥。
- 每轮写入数量明显下降。
- 同类事实能稳定覆盖。

## Phase 8：评测集

### 目标

让记忆优化可验证，而不是靠主观感觉。

### 新增目录

```text
tests/fixtures/memory/
```

### 测试类型

#### 1. 写入测试

输入：

```text
用户说：以后都用中文简洁回答。
```

期望：

```text
Store, type=preference, pinned_by=user
```

输入：

```text
当前 D 盘剩余 256GB。
```

期望：

```text
Reject transient
```

#### 2. 敏感信息测试

输入：

```text
密码是 abc123
```

期望：

```text
Reject sensitive
```

#### 3. 召回测试

已有记忆：

```text
Gen Digital reported CVE-2026-51990 affects Sogou Input Method before 16.3.0.3498.
```

Query：

```text
搜狗输入法那个漏洞修复版本是多少？
```

期望：

```text
召回该事实
```

#### 4. 预算测试

构造多个长短不同记忆，验证：

- 高相关短事实优先进入上下文。
- 长摘要被降级为 outline。
- 总 token 不超过 MemoryBudget。

### 验收指标

| 指标 | 目标 |
|---|---:|
| Recall@5 | >= 0.85 |
| 错误写入敏感信息 | 0 |
| transient 写入率 | < 2% |
| 重复记忆率 | < 5% |
| 每轮记忆注入 token | <= context_window 5% |

## 5. 推荐实施顺序

建议按低风险到高收益排序：

1. 修改 pin 默认策略。
2. 收紧 curator prompt。
3. 新增 `memory_policy.rs`，实现 transient/sensitive filter。
4. curator 改成 candidate -> policy -> store。
5. subject_key 规范化。
6. 新增 `EmbeddingProvider` / `VectorIndex` / `MemoryRetriever` 抽象，但默认禁用 embedding。
7. 先实现 Lexical Fallback：BM25 + keyword + entity + subject_key。
8. deep recall 增加 relevance 和 utility density。
9. 所有记忆块接入 `context_arbiter`。
10. 增加 deep_facts GC。
11. 增加 conversations/summaries retention。
12. 建立 memory eval 测试集。
13. 可选接入本地 embedding provider。
14. 可选接入云 embedding provider。
15. 可选把 SQLite brute-force 替换为 hnsw/zvec/sqlite-vec。

## 6. 最小可落地版本

如果只做一轮小改，建议优先完成以下 5 项：

1. `deep_memory remember` 默认不再 `PinnedBy::User`。
2. curator prompt 禁止保存 secrets/credentials，过滤 transient state。
3. 增加 `MemoryPolicy::decide_write(candidate)`。
4. 先实现无嵌入的 `HybridMemoryRetriever` fallback：BM25 + keyword overlap + entity + subject_key。
5. `deep_permanent_block` 改为接收 `query`，按 relevance + value + utility density 排序。
6. 增加 `gc_deep_facts`，至少按总量上限和未访问时间删除 `PinnedBy::None` 低价值事实。

这 6 项能显著改善当前“不优秀”的核心问题，且不需要立刻引入 embedding 依赖。

## 7. 关键代码改造清单

| 文件 | 改造 |
|---|---|
| `src\tool\deep_memory.rs` | 修正 remember 默认 pin，增强参数说明 |
| `src\server.rs` | 改造 `spawn_deep_curator`，接入 MemoryPolicy |
| `src\memory.rs` | 增加 GC、subject_key 规范化、deep recall rerank |
| `src\deep_memory.rs` | 增加 access_count / ttl 支持，或配套迁移 |
| `src\context_arbiter.rs` | 扩展 utility density 或在调用侧排序 |
| `src\value.rs` | 复用统一价值函数，避免重复评分逻辑 |
| `tests` | 增加写入、召回、GC、预算测试 |

## 8. 建议新增配置

```toml
[agent.memory]
enabled = true
curator_enabled = true
max_deep_facts = 300
max_agent_facts = 200
memory_context_ratio = 0.05
memory_context_min_tokens = 500
memory_context_max_tokens = 2500
conversation_retention_days = 180
summary_retention_days = 365
gc_on_startup = true
gc_interval_hours = 168

[agent.memory.retrieval]
mode = "hybrid"                 # lexical | hybrid
fallback_mode = "lexical"       # embedding unavailable 时使用
top_k = 12
min_recall_score = 0.55
use_utility_density = true

[agent.memory.embedding]
enabled = false                 # 默认关闭，保证零依赖可运行
provider = "none"               # none | local | cloud
model = ""
dim = 0
batch_size = 16
timeout_secs = 20
cache_embeddings = true

[agent.memory.embedding.local]
backend = "fastembed"           # fastembed | candle | zvec_builtin | custom
model_path = ""                 # 本地模型路径；为空则由 backend 默认查找
model_name = "all-MiniLM-L6-v2"
device = "cpu"

[agent.memory.embedding.cloud]
base_url = ""
api_key_env = "OPENAI_API_KEY"
model = "text-embedding-3-small"
dim = 1536

[agent.memory.vector_index]
backend = "sqlite_bruteforce"   # sqlite_bruteforce | hnsw | zvec | sqlite_vec
path = "memory/vector_index"
rebuild_on_model_change = true
```

配置语义：

- `retrieval.mode=lexical`：完全不使用 embedding。
- `retrieval.mode=hybrid` 且 `embedding.enabled=false`：自动退化为 lexical fallback。
- `embedding.provider=local`：使用本地模型生成 embedding。
- `embedding.provider=cloud`：使用云 embedding API，注意隐私和数据出境风险。
- `vector_index.backend=sqlite_bruteforce`：默认最稳，适合先落地。
- `vector_index.backend=zvec`：仅在 POC 验证 Windows 打包、模型、索引稳定后启用。

## 9. 风险与注意事项

### 9.1 不要一次性引入 embedding 作为硬依赖

embedding 会带来模型依赖、成本、迁移和离线问题。建议先抽象接口，默认 BM25 + lexical + entity，后续再启用。

正确实现应满足：

```text
embedding 初始化失败 -> lexical fallback
embedding API 超时 -> lexical fallback
embedding 维度不匹配 -> 禁用该索引并提示重建
vector index 损坏 -> 删除索引并从 SQLite 重建
模型变更 -> content_hash 不变也需要重新生成 embedding
```

### 9.2 云 embedding 的隐私边界

FoxIR 面向 IR / 运维，本地日志、主机名、IP、路径、IOC、账号名可能敏感。云 embedding 不是不能用，但必须：

- 默认关闭。
- 配置中显式启用。
- UI 标明“会把被索引文本发送到 embedding API”。
- 不发送 secrets。
- 不发送原始大段日志，优先发送摘要或脱敏 chunk。
- 允许只对 `deep_facts` 做云 embedding，不对 `conversations` 做云 embedding。

### 9.3 不要删除 user pinned 记忆

User pinned 应只由用户或显式管理动作删除。GC 可以提示“建议清理”，不能静默删除。

### 9.4 不要把 daily summary 当长期记忆

`MEMORY.md` 建议只投影 deep facts；近期摘要应在 UI 中单独显示，或作为 recall source，而不是混入长期记忆投影。

### 9.5 不要把工具瞬时结果写成长期事实

系统状态类信息默认是 snapshot，不是 durable memory。

## 10. 完成后的理想效果

完成后，FoxIR 的记忆系统应满足：

- 写入少而准。
- 用户显式记忆不会被误删。
- agent 自动记忆会退火、合并、清理。
- 当前任务相关记忆优先进入上下文。
- 长记忆不会挤占上下文。
- 记忆库不会无限增长。
- 召回质量可测试、可量化、可回归。

最终目标不是“记得更多”，而是：

> 只记值得长期复用的内容，只在相关时召回，只用合理 token 进入上下文，并能随时间自动清理。
