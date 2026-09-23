# FoxIR TASKS 按需载入与反播报 · 软件设计文档（SDD）

| 项 | 内容 |
|---|---|
| 文档 | TODO/TASKS 上下文从"每轮注入 + 主动提醒"改为"按需载入 + 面板展示" |
| 版本 | v1.0 |
| 状态 | 已落地（commit `ea3e69d`，分支 `codex/multi-session`） |
| 适用工程 | FoxIR（Rust 单二进制、Axum 网关、Windows 本地） |
| 关联文档 | docs/SDD_context_efficiency_and_debrief.md（§3 消息装配约定） |

---

## 1. 概述

旧逻辑把未完成的 TODO 清单**每轮**注入主会话上下文（完整块或一行提醒），注入文本中的规则（"处理完新消息再 RETURN 到未完成清单"、"告诉用户你不再跟踪该 TODO"）诱导模型在普通回复里向用户播报任务关系与清单机制。

典型事故：用户只说一句"访问百度并截图"，模型回复"这个任务和之前那个知识库查询、17 项 IR 清单都不一样——这是一个独立的新任务"——把内部任务管理推理说给了用户听。

本设计将职责拆开：**任务状态展示归前端 TASKS 面板，上下文载入按需触发，模型永不主动提醒**。

---

## 2. 目标与非目标

**目标**
- G1：普通回复的上下文不含未完成任务清单，两者不混杂。
- G2：未完成任务状态始终对用户可见——由前端面板承担，不经模型转述。
- G3：用户明确要续作时（"继续/接着/continue"），完整清单照常载入，逐项执行语义不变。
- G4：崩溃恢复（checkpoint resume）照常载入完整清单（历史可能残缺，需要真相源）。
- G5：模型回复中不再出现任务关系评论与清单机制播报。

**非目标（本期）**
- N1：改动 todo_update 工具本身（set/update/clear/超时看门狗语义不变）。
- N2：改动前端面板展示逻辑（/api/todos 轮询已独立存在，无需改）。
- N3：多会话 TODO 隔离（会话级 todos-<sid>.json 既有行为不变）。

---

## 3. 问题分析（旧逻辑的三重缺陷）

| 缺陷 | 位置（旧代码） | 后果 |
|---|---|---|
| 每轮注入 | `run()` 内 `is_main_session` 分支：无 todo 历史注入完整块，有则注入 reminder | 未完成任务永远占据上下文，干扰无关话题 |
| 提醒式文案 | 注入块规则 "RETURN to this unfinished list"、"tell the user you are detaching" | 模型向用户催促/播报清单状态 |
| 任务关系评论 | 规则 1 要求模型评估"新消息是否新任务" | 模型把评估过程说出来（本次事故的直接原因） |

前端 TASKS 面板（`index.html` `todoList` 轮询 `/api/todos` → `server.rs todos_handler` 读 `todos.json`）一直独立于 prompt 注入，因此每轮注入对"用户可见性"毫无贡献——纯负担。

---

## 4. 设计

### 4.1 载入门控（`llm_agent.rs` run() 的 `is_main_session` 分支）

注入条件收敛为两个，其余一律不注入：

| 条件 | 判定 | 载入内容 |
|---|---|---|
| 崩溃恢复 | `ctx.resume_history.is_some()` | 完整清单块（含逐项状态与执行规则） |
| 明确续作 | `is_task_continuation(user_message)` 且 `todo_list_has_unfinished()` | 完整清单块 |

### 4.2 续作判定（纯函数，全测覆盖）

`is_task_continuation`：消息非空、≤60 字（长消息按新任务处理）、命中续作线索词——`继续 / 接着 / 上次 / 按任务 / 任务列表 / continue / keep going / resume / carry on`。

设计要点：
- **保守取向**：宁漏勿滥。漏判的代价是用户多说一句"继续任务"；误判的代价是普通回复又被清单污染。
- 无线索词时的兜底：即便漏判，`todo_update` 工具对模型始终可用，模型可在需要时自行 `action='list'` 拉取。

### 4.3 提示词反播报

两处文本修正：

1. **稳定提示词 TODO 章节**新增"Intent-first"规则：
   - 每条新消息按字面意图直接执行；新消息就是新任务，**禁止评论它与之前任务或 TODO 清单的关系**；
   - **禁止向用户提及任务清单机制**（TODO/清单/进度同步），状态由面板展示。
2. **续作清单块**（`build_todo_context_block`）规则重写：
   - 删除"RETURN to this unfinished list"、"tell the user you are detaching"；
   - 明确"此块因用户要求续作而载入"，逐项执行规则保留，加"永不向用户播报清单机制"。

### 4.4 不变的既有机制

- `todo_update` 工具与 todos.json 存储：不变。
- 逐项超时看门狗（`apply_todo_timeout`）：不变——它只在 run 内部有 in_progress 项时触发，不属于每轮提醒。
- 前端面板轮询：不变。
- 删除不再使用的 `build_todo_reminder` / `history_has_todo` 及其测试。

---

## 5. 验证

新增测试（`llm_agent.rs`）：

| 测试 | 覆盖 |
|---|---|
| `task_continuation_detection` | 续作线索命中；新任务描述、超长消息、空消息不命中 |
| `todo_unfinished_gate` | 有/无未完成项的判定；todos.json 缺失不命中 |

既有 `todo_block_injected_with_items_and_rules` 等清单块测试保持通过（块内容仅规则文案调整）。全量 337 通过 / 3 个既有 todo_update 本机环境失败（与本次无关）。

行为预期：

| 用户输入 | 行为 |
|---|---|
| "访问百度并截图" | 直接执行，回复不提任务关系 |
| 普通新话题 | 上下文无清单，模型不催促 |
| "继续" / "接着上次" | 载入完整清单，逐项继续 |
| 任何时候 | 未完成任务在 TASKS 面板可见 |

---

## 6. 落地记录

| 项 | 值 |
|---|---|
| commit | `ea3e69d` |
| 分支 | codex/multi-session |
| 改动文件 | src/agent/llm_agent.rs（+69 / −90） |
