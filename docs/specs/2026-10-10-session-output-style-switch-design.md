# FoxIR 会话输出样式切换（回合式 / 组合式）设计

日期：2026-10-10
状态：待审
作者：FoxIR agent（brainstorming 产出）
批准记录：方案 A + 4 条决策取推荐值，由用户以「按推荐」批准（2026-10-10）

---

## 1. 目标

给 FoxIR 的会话界面增加**一个切换按钮**，在两种输出装配之间来回切：

- **回合式**（现状）：工具卡、思考块、权限面板按到达顺序平铺在正文流里。
- **组合式**：一个 invocation 一个组，组头汇总「N 次工具调用 / 区域直方图 / N 次本可用现成工具 / 总时长」，思考行折进可折叠条，卡片带状态与时长。

关键要求：**切换是无损的**——切走再切回，会话内容与不切换时逐节点一致；且换会话再换回来也不能丢。

## 2. 非目标

- 不替换现有亮/暗主题。新样式必须走现有 token（`--bg2/--border/--t1…`），两套主题都可用。
- 不降级权限交互。现有的橙色大面板 + Allow/Deny（`renderPermRequest`，`static/index.html:4457,4474`）比 PCBuddy 的小 pill 强，组合式里原样保留，只改变它出现的位置。
- 不做后端事件存储改造。日志只在浏览器内存里，持久化仍走现有 `/api/history` + `localStorage`。
- 不在本设计内做 Activity 能力 / fs watcher / JSONL→SQLite。

## 3. 证据基线（全部 2026-10-10 实测）

| # | 事实 | 证据 |
|---|---|---|
| E1 | 会话恢复的**主路径是 DOM 字符串**：快照存 `convArea.innerHTML`，恢复时优先用它 | `static/index.html:1961`（`html: streamingNow ? '' : innerHTML`）、`:1970-1972`（`if(s.html) conv.innerHTML=s.html`，注释自称 "Normal case"） |
| E2 | 事件层已存在但被削窄：只保留 `tool_call/tool_result` | `:2397-2402`（filter + 注释「re-playing text events would render it twice」） |
| E3 | `#convArea` 的写入点 **23 处命中**：16 个 append 站点（4 处经 `appendToConv`，12 处直接 `appendChild`）+ 7 个 innerHTML 站点（6 处已确认为清空/恢复，1 处未逐行确认读写属性），跨 ≥9 个函数 | 函数归属：`switchSession:2063`、`handleMsg:2431,2443`、`replaySessionEvents:2716`、`showExpertPrompt:3090,3112`、`showNotification:4187`、`renderPermRequest:4457,4474`、`handleSubAgentCard:5136`、`showAssistantMsg:4177` |
| E4 | 子代理事件抑制发生在**入口第一行**，日志若沿用会永久丢内容 | `:2365` `if(isWorkerFragment(msg))return;` |
| E5 | 已有一个复位函数，重建必须复用它 | `clearCurrentSessionUI()` `:1985-1988`（`curToolMap/curThinkEl/curTextEl/textAccum/lastInvId/subAgent*`） |
| E6 | 回合开始前到达的状态行被**静默丢弃** | `case 'progress'` 写进 `curToolMap[Object.keys(curToolMap).pop()]`；无卡时 `pop()`→`undefined`→什么都不做。真机验证：喂 3 条 progress，DOM 里 0 条 |
| E7 | 失败与成功在 DOM 上不可区分 | 真机喂 10 次调用（含超时、被杀、未获批），10 张卡 class 全是 `tool-block done`，`.pill` 数 = 0，无任何时长 |
| E8 | 思考块渲染出来但看不见 | 真机测得 `.think-block` `offsetHeight = 2` |
| E9 | 量级底数：当前约 **390 事件/天**，但**单条 `tool_result` 最大 1,994,786 B** | `~/.RustAgent/workspace/logs/` 逐文件统计（09-24 后聚合已生效，thinking 平均 500–1,700 字符） |
| E10 | 现有持久化用 `slice(-200)` 且剥掉图片，说明"有界 + 剥大字段"是本文件既有惯例 | `saveHist()` `:4173` |
| E11 | 偏好存储已有先例：主题存 `localStorage` | `:8` `localStorage.getItem('foxir.theme')` |

## 4. 决策（已批准）

| 决策 | 取值 | 理由 |
|---|---|---|
| 总方案 | **A：事件日志为真相，HTML 快照降级为缓存** | B（双树并行）写入点翻倍且解决不了 E2 的丢失；C（纯 CSS）做不出 `.turngroup` 层级 |
| 一个组 = ？ | **一个 `invocation_id`**；**当 `invocation_id` 缺失或恒定时（Instant 模式），退化为"一个用户回合一个组"** | 与现有「换段即 finalize 文本块」语义一致（`:2412`）。不写这条退化规则，Instant 模式下"每组一个 invocation"会让整段对话全挤进一个组或每组零长——两种读法都成立，属歧义 |
| `html` 快速路径 | **保留**，仅当 `style === capturedStyle && !streamingNow` 时使用；风格变了必须重放 | 我们自己的重放耗时**尚未测量**（PCBuddy 注释里的 98 气泡/1.3 s 是它的数），先用快速路径兜住，测出自己的数再议是否拆 |
| 日志上限 | **每会话 2,000 事件**，单条正文 >64 KB 只存引用与摘要，**日志不落 localStorage** | E9：2 MB 级结果塞进 localStorage 会撞配额；E10 是本文件既有惯例 |
| 后端字段 | **后置为第二笔**；缺失期间组头时长与卡片时长显示 `—`，**不估算不编数** | 补字段要重编（约 10 分钟）且需用户先关闭运行中的 FoxIR 才能覆盖 exe |

## 5. 架构

```
ws.onmessage
   └─> emit(msg)                       [唯一入口]
         ├─> S.log.push(msg)           [原始事件，不做任何抑制]
         └─> paintOne(msg)             [按当前风格装配一条]

切换按钮 ──> setStyle(s) ──> rebuild()
rebuild() ──> clearCurrentSessionUI()  [复用 E5]
           ──> conv.innerHTML = ''
           ──> for m of S.log: paintOne(m)   [装配器内部自行决定抑制/折叠/分组]

快照 saveSession() ──> { log: S.log.slice(), style: S.style, html: 非流式时的 innerHTML }
快照 restoreSession() ──> style 相同且 html 非空 ? 用 html : rebuild()
```

### 5.1 组件与接口

| 单元 | 职责 | 对外接口 | 依赖 |
|---|---|---|---|
| `SessionLog` | 有界保存当前会话原始事件 | `push(m)` / `all()` / `bytes()` / `drop()` | 无 |
| `emit(msg)` | 唯一入口：入日志 + 增量装配 | `emit(msg)` | `SessionLog`, `renderers` |
| `renderers.turn` | 回合式装配（保持现状 DOM 与类名） | `paint(msg, ctx)`，由 `paintOne` 分派 | 现有 `createToolBlock` 等 |
| `renderers.grouped` | 组合式装配（组头/思考条/状态 pill） | `paint(msg, ctx)`，由 `paintOne` 分派 | 同一批底层构件 |
| `rebuild()` | 全量重建 | `rebuild()` | `clearCurrentSessionUI`, `renderers` |
| `sessionSnapshots[sid]` | 会话快照，新增 `log`/`style` | 不变 | — |

`ctx` 是装配器的可变状态袋（`curToolMap/curThinkEl/curTextEl/group/…`），由 `clearCurrentSessionUI()` 复位。**装配器不得直接读 DOM 反推状态**——这是 E7 那类"状态丢失"的根因。

### 5.2 三条硬约束（写进实现，不可妥协）

1. **抑制留在装配层**：`isWorkerFragment(msg)`（E4）从 `handleMsg` 顶部移到 `paintOne` 内。日志永远记原始事件，否则子代理场景切回来思考条永久缺内容。
2. **重建必须先复位**：`rebuild()` 第一步调 `clearCurrentSessionUI()`（E5），否则 `tool_result` 会往上一轮的旧卡片追加。
3. **类名隔离**：组合式一律 `.turngroup / .tcard / .tevt` 前缀，**不复用 `.tool-block`**。本项目已因共享类名踩过两次（`.copy` 的 `opacity:0`、`.live` 的 `margin-bottom`）。

## 6. 数据流：一次工具调用在两种风格下的差异

```
{type:'tool_call', name:'shell_exec', call_id:'a4', args:{…}}
  回合式  → .tool-block（现状，不变）
  组合式  → 确保本 invocation 的 .turngroup.tools 存在 → .tcard
             左标签 = 区域码（前端映射表算），宽标签 = 子句
             pill: 运行中 → 完成后 正常/失败/退出码 c/已超时 + 时长（缺字段则 —）
             组头重算：次数 / 区域直方图 / bypass 计数 / 总时长

{type:'tool_result', call_id:'a4', result:'…'}
  两风格都按 call_id 找卡；找不到时（E7 类问题）→ 记一条 warn 并新建，不静默丢
```

区域直方图与「N 次本可用现成工具」的判据：子句以 `/^[A-Z]{1,2}·/` 开头（`PS·`/`SH·` 等来源标记）且该区域有对应领域工具 → 计一次 bypass。**该前缀由后端在 summary 里给出，属第二笔**；第一笔期间 bypass 段隐藏（不显示 0，因为 0 是假数）。

## 7. 错误处理

- **单条事件装配抛错**：捕获、记 `console.error` + 一条可见的降级行（`⚠ 该事件渲染失败，已跳过`），继续后面的事件。绝不因一条坏事件白屏。
- **重放中途抛错**：`rebuild()` 整体 try/catch；失败则回退到"当前风格 = 上一个成功风格"并提示，不留半棵树。
- **找不到 `call_id` 的结果事件**：按第 6 节新建卡片（今天的行为是静默丢弃，见 E7）。
- **日志溢出**：丢最旧事件但在组头显示「已省略 N 条早期事件」——丢弃必须是可见的。
- **页面刷新后无法重建**（已知边界，不是 bug）：`S.log` 只在内存，刷新即空。刷新后仍走现有 `/api/history` + `msgHist` 恢复，此时**组合式没有可重放的事件**，只能按回合式呈现，并在按钮上标注「本会话从历史恢复，无事件可重组」。不做这个标注就会让人以为切换坏了。

## 8. 样式与主题

- 全部颜色取自现有 token；新增的只有 `--panel/--panel-2/--panel-3/--muted/--muted-2/--amber/--violet` 这层**别名映射**（已在 demo 中验证），不改现有值。
- 验收按四维扫：`backgroundColor` / `background-image`(gradient) / JS 内联 `style` / 前景色，亮暗各测一遍。
- 折叠语义沿用原生 `<details>`：回合结束若有失败则**不折叠**（"失败就是新闻"），无失败则折叠。

## 9. 测试与验收

| 类型 | 断言 |
|---|---|
| 正对照 | 同一会话 切→切回，逐节点 `outerHTML` 与"从不切换"一致；覆盖 `thinking / progress / permission_request / tool_call / tool_result / text` 六类。**测试必须在关闭 `html` 快速路径的情况下跑**——否则恢复走的是字符串快照，断言会白过（不重建也"一致"） |
| 反例守卫 | 切会话再切回，工具卡不丢（今天会丢，E2）；回合开始前的 `progress` 有落点（今天静默丢，E6） |
| 快速路径 | 风格未变 → 走 `html`；风格已变 → 必须走重放。两条分别有断言 |
| 有界性 | 注入 2,500 事件 + 一条 2 MB 结果 → 日志条数 ≤2,000、正文被引用化、组头显示省略计数 |
| 隔离性 | 组合式节点上不得出现 `.tool-block` 类名（grep 断言） |
| 主题 | 亮/暗四维扫各一次 |
| 性能 | 记录一次完整 `rebuild()` 的耗时（这是**首次拿到我们自己的数**，用来决定第 4 节快速路径是否保留） |

## 10. 分步交付

| 步 | 内容 | 是否动后端 | 可独立验收 |
|---|---|---|---|
| P1 | `SessionLog` + `emit` + `rebuild` + 收口写入点（16 个 append + 7 个 innerHTML，共 23 处 grep 命中）+ 快照改造 | 否 | ✅ 不加任何新样式，只证明"切换前后等价" |
| P2 | 修 E6/E7/E8 三个既有缺陷（两种风格都受益） | 否 | ✅ |
| P3 | 组合式装配器 + 切换按钮 + 主题扫 | 否 | ✅ 组头时长/bypass 此时显示 `—` 或隐藏 |
| P4 | 后端补 `duration_ms / ok / exit_code / summary 来源前缀` + jsonl 落盘 + 测试 | **是**（需重编 + 用户关闭运行实例） | ✅ 数字落地 |

回滚：`S.style` 与快照里的 `style` 字段是纯增量；P1–P3 出问题可把按钮隐藏、恢复直接渲染路径，不动后端。

## 11. 明确排除

- 方案 B（两棵树并行）：写入点翻倍、E2 不解决。
- 方案 C（纯 CSS 切换）：造不出父节点。
- 把日志持久化到 localStorage：与 E9 的 2 MB 级结果冲突。
- 组合式里把权限降级成 pill：见 §2。

## 12. 未决 / 后续

- P4 落地后，重放耗时若显著（>150 ms/会话），再议是否彻底去掉 `html` 快速路径。
- `Expert` 多 invocation 的组头是否要显示 invocation 序号标签，等 P3 真机看了再说。
- 与本设计无关但同批挂起的事：Windows-MCP 对照实验（解释器与包已就位，未对桌面发起任何调用）、Activity 能力的 24 h 量级评估结论（事件驱动 + 案卷作用域 jsonl）、目标机 Python 的 runtime 探测方案。
