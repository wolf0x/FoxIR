**[English](README.en.md)** | **[中文](#)**

# FoxIR

面向本地 IT 系统工程师的 AI 辅助平台——专注于系统分析、日志调查、事件响应与远程运维。**FoxIR 是原 RustAgent 项目的继任者**，完全本地运行、单二进制部署，具备 WebSocket 网关、多模型支持、双层记忆、动态 SOP、有限脑上下文预算、40+ 内置工具、权限管控、任务调度与远程通道（WinRM / Linux SSH）。面向 Windows 环境，开箱即用。

## 项目定位

FoxIR 专为本地 IT 系统工程师设计，解决日常运维中最耗时的三类工作：**系统状态分析**、**日志调查溯源**、**安全事件响应**，并向上延伸至**远程 Windows（WinRM）与远程 Linux（SSH）** 的故障排查与应急响应。

传统方式下，工程师需要在多个工具间反复切换——事件查看器查日志、PowerShell 查进程、注册表编辑器查配置、netstat 查连接——每条线索都需要手动关联和判断。FoxIR 将这些能力统一到一个 AI Agent 中：工程师用自然语言描述问题现象，Agent 自动编排工具链，采集系统状态、检索相关日志、关联分析异常，最终给出结构化的调查结论和处置建议。

**典型场景**：

- **日志调查**：「最近 24 小时系统日志里有哪些错误和警告？按时间线整理出来」——Agent 自动调用事件日志工具，筛选关键级别，按时间排序，关联相关进程和服务状态
- **系统分析**：「当前有哪些异常进程在占用大量资源？检查它们的启动来源」——Agent 编排进程枚举 + 资源占用分析 + Autoruns 持久化检测，给出完整的进程链分析
- **安全排查**：「检查这台机器是否被植入了持久化后门」——Agent 串联注册表审计、计划任务枚举、服务枚举、Autoruns 检测，输出完整的持久化攻击面报告
- **远程响应**：「用 WinRM 连上 192.168.x.x 做溯源」「SSH 到内网主机检查挖矿进程」——Agent 通过内置 WinRM / Linux SSH 通道直接对远端主机执行命令与取证
- **故障定位**：「服务 XXX 启动失败，帮我查原因」——Agent 查询服务状态、关联事件日志、检查依赖服务、分析配置文件，定位根因

**为什么完全本地**：IT 工程师处理的日志、进程信息、注册表数据往往包含敏感的内网拓扑和凭据信息。FoxIR 的 AI 对话引擎、工具执行、数据存储全部在本地完成，API 密钥 AES-256-GCM 加密存储，只有 LLM 推理请求发往云端模型——原始系统数据不出本机。

单个 Rust 编译产物（`FoxIR.exe`，~30MB+）即包含完整的 AI 对话引擎、工具执行层、WebSocket 网关与 Web Dashboard，无需安装额外运行时或外部服务依赖。灵感来源于 Google ADK 的 Agent → LlmAgent → EventStream 架构模式，在 Rust 生态中实现了完整的 Agentic Loop。

## 核心架构

```
┌──────────────────────────────────────────────────┐
│                    Dashboard SPA                 │
│   (Chat / Dashboard / Skills / Tools / Knowledge )│
│   (CRON / MCP / Settings · Context Budget 视图)  │
└───────────────────────┬──────────────────────────┘
                        │ WebSocket / HTTP
┌───────────────────────┴──────────────────────────┐
│               Axum 0.8 Server                     │
│        (REST API + WS Gateway + SSE)             │
├──────────────────────────────────────────────────┤
│  Runner → LlmAgent (Agentic Loop)                │
│    ├── Agent trait → EventStream                 │
│    ├── 有限脑 Context Budget（价值排序装配）        │
│    ├── 会话回放 / 续接（断线上下文重建）             │
│    ├── Checkpoint 崩溃恢复 + Truncated JSON repair│
│    └── Re-prompt 检测与自愈                       │
├──────────────────────────────────────────────────┤
│  Tool Layer                                      │
│    ├── 40+ 内置工具（Windows IR / Linux IR）       │
│    ├── WinRM（远端 Windows） / Linux SSH（远端）   │
│    ├── MCP Client (stdio + SSE)                  │
│    ├── Skill Manager（热注入 + 步骤契约）          │
│    └── External Tools（workspace/tools/）         │
├──────────────────────────────────────────────────┤
│  Memory & Learning                               │
│    ├── 双层记忆：深层（deep_facts） + 浅层          │
│    ├── 自动记忆 memory.db（SQLite + FTS5 + BM25）  │
│    ├── Knowledge（routing.json + experience.md）  │
│    ├── SOP（动态多步流程，自动蒸馏与回放）           │
│    └── 统一工件价值 V(a,t)=Q×R×U（预算仲裁）        │
├──────────────────────────────────────────────────┤
│  Infrastructure                                  │
│    ├── Permission（分类门控 + 意图策略 Block/Audit）│
│    ├── Intent Policy（Block / Audit / Pass）      │
│    ├── Scheduler（CRON + interval）+ Heartbeat    │
│    ├── Security（AES-256-GCM + 回收站安全删除）     │
│    └── 首次运行内嵌工作区文件提取                   │
└──────────────────────────────────────────────────┘
```

## 核心能力

### 权限系统

FoxIR 实现分类门控（Category-based Gates）+ 意图策略（Intent Policy）的双层安全模型：

- **五级权限分类**：read / write / delete / modify / execute，每种工具调用声明所需权限类别
- **异步用户授权**：当 Agent 请求高权限操作时，通过 WebSocket 向 Dashboard 推送授权请求，用户确认后返回结果，Agent 循环无阻塞等待
- **命令意图策略引擎**：对 shell_exec / winrm / linux_ssh 命令进行语义解析（verb + targets），三级判定：
  - **Block**（绝对禁止）：磁盘格式化、安全日志清除、编码命令等不可逆操作，无视任何授权状态硬拦截
  - **Audit**（审计放行）：文件删除、进程终止、服务停止等高危但合法操作，记录日志后正常执行
  - **Pass**（静默放行）：只读查询等常规操作
- **跨类别绕过检测**：当 shell_exec 已免确认（execute:true）但命令意图映射到被拒绝的权限类别（如 delete:false）时，自动升级为需要用户确认，防止 LLM 通过 shell_exec 绕过 file_delete 权限控制
- **回收站安全删除**：`file_delete` 及简单字面路径的 `Remove-Item` / `del` 等 shell 删除默认走系统回收站，而非硬删除数据
- **权限拒绝强反馈**：拒绝时向 LLM 返回强措辞错误消息，禁止使用替代工具绕过

### 记忆系统（双层记忆）

FoxIR 采用**双层记忆 + 自动记忆 + 知识库**的统一记忆架构：

**深层记忆（Deep Memory）**：SQLite 持久持久层，用 `deep_memory` 工具管理（remember / recall / list / forget / update）。用户陈述的事实被钉住（永不自动遗忘），其余按统一价值函数 V(a,t)=Q×R×U 排序并随时间退火，按上下文预算打包注入。

**浅层记忆（Shallow Memory）**：服务端每轮自动注入有界摘录块（fading summary），可在回复末尾附 `<memory>` 块捕获重要轮次，支持 FTS 相关召回。

**自动记忆（memory.db）**：每轮对话自动持久化，近期摘要以 [Memory Context] / [Memory Recall] 注入，CJK bigram 分词 + BM25 全文搜索，每日自动摘要。

**MEMORY.md**：curated 长期记忆，在双层记忆开启时作为只读投影（dumped from deep_facts），不再作为主记忆来源注入。

**Knowledge 知识库**：`knowledge/routing.json` 将请求路由到对应文档；`experience.md` 汇聚所有蒸馏的经验（facts / lessons / decision / tips 等不再分文件）；同时可挂接多份方法论 / playbook / 过程文档（如钓鱼分析方法论），供每轮预检索与优先引用。

### 调度系统

- **CRON 表达式**：标准 5 字段，支持时区
- **间隔语法**：`every 5m`、`every 2h` 等
- **JSON 持久化**：`cron_tasks.json`，重启不丢失
- **独立会话执行**：到期任务以独立会话运行，不阻塞主会话
- **心跳机制（Heartbeat）**：可通过 Settings 开关，定期做记忆维护与健康检查，仅异常时通知

### 工具系统

40+ 内置工具围绕 IT 工程师的核心工作流设计，从日常系统检查到深度安全分析形成完整工具链：

**文件操作**（5 个）：FileRead / FileWrite / FileDelete（进回收站）/ FileModify / FileList

**执行通道**：ShellExec（PowerShell/CMD，意图策略拦截）、LinuxSSH（远端 Linux/bash）、WinRM（远端 Windows/PowerShell，NTLM/Basic + HTTP5985/HTTPS5986）

**事件响应工具组**（Windows IR + Linux IR，调查核心）：进程分析、网络连接、注册表审计、服务枚举、计划任务、VSS/USN、事件日志、EVTX 解析、内存取证、持久化检测、攻击路径、时间线、流量分析（pcap）、邮件样本（EML）、扫描、案例管理、报告生成等

**恶意软件分析**：YARA（boreal）+ PE（goblin/iced-x86）静态扫描

**远程 Linux IR**：认证/后门/暴力破解/挖矿/持久化/文件/木马/横向/Web 等分主题排查工具

**记忆与知识**：deep_memory（双层记忆）、memory_md（MEMORY.md 投影/回退）、knowledge_search / knowledge_ingest（知识库）

**其余**：browser_cdp（浏览器自动化）、MCP 客户端（stdio+SSE）、cron_manage、todo_update（任务台账）、evidence（证据台账）、外部工具（workspace/tools/）

**SOP（动态流程）**：将成功的多步骤操作（如「部署应用」「应急响应阶段」）蒸馏为结构化、可重用的流程（phases + 验证步骤），再次遇到相似任务时按标签匹配自动回放与优化，几乎不增加额外 Token。

**有限脑 Context Budget**：Block 级价值排序接口，把 System / Tools / Memory / Knowledge / SOP / History 按统一工件价值 V(a,t)=Q(a)×R(a,t)×U(a) 装配进预算，保证上下文窗口始终保留最有价值信息；Dashboard 提供实时预算视图与实测占用。

**Expert 模式**：Manager–Executor–Auditor 三角色分离，TaskContract 状态持久化（SQLite，崩溃可恢复），任务完成自动生成审计报告（HTML）到 `workspace/Expert/`，发现写入 Blackboard 供主会话可见。适用于 15+ 轮的完整 IR 调查。

### 会话与断线续接

- 每轮写入 memory.db，支持会话回放与断线续接（`build_session_recall_block` 重建 [会话回顾]）
- 排队中的插话（interject）在用户 STOP 后进入执行队列，不丢失
- Checkpoint 崩溃恢复：崩溃后可重放历史续跑

### Web Dashboard

- **Chat**：多模型对话、插话（interject）、/skill 命令、发送/回放
- **Dashboard**：Context Budget 视图 + 用法统计
- **Skills / Tools / Knowledge / CRON / MCP / Settings**：技能管理、工具列表、知识库、任务调度、MCP 服务器、设置

## 技术栈

| 组件 | 技术选型 |
|------|----------|
| 运行时 | Tokio (full features) |
| HTTP/WS | Axum 0.8 |
| LLM 协议 | OpenAI-compatible streaming |
| 数据库 | SQLite (rusqlite bundled) + FTS5 |
| 远程 | winrm-rs（WinRM）、ssh（Linux SSH） |
| 回收站 | trash（安全删除） |
| MCP | rmcp (stdio + SSE) |
| 浏览器 | chromiumoxide (CDP) |
| 加密 | aes-gcm (AES-256-GCM) |
| YARA | boreal（规则扫描） |
| PE 解析 | goblin + iced-x86（反汇编） |
| 序列化 | serde + serde_json + serde_yaml + toml |
| 日志分析 | regex + evtx（EVTX 解析） |
| 流量分析 | pcap-parser（pcap/pcapng 离线解析） |
| 日志 | tracing + tracing-subscriber (env-filter) |

## 配置

运行时工作目录：`%USERPROFILE%\.RustAgent\workspace\`（旧版 RustAgent 延续的数据目录）

```
workspace/
├── config.toml          # 主配置（Server / Agent / Model）
├── models.json          # 模型配置（API Key 加密存储）
├── mcp_servers.json     # MCP 服务器配置
├── cron_tasks.json      # 定时任务定义
├── .password            # Dashboard 访问密码
├── memory/
│   └── memory.db        # SQLite 自动记忆数据库（+ deep_facts 双层记忆）
├── knowledge/           # routing.json + experience.md + 方法论文档
├── skills/              # 技能目录
├── tools/               # 外部工具目录
├── Expert/              # Expert 模式 HTML 审计报告
├── logs/                # JSONL 对话日志
├── static/              # Dashboard 静态资源
└── output/              # 工具输出（截图/报告等）
```

## 构建与运行

```bash
# 编译 release 版本（LTO + strip）
cargo build --release

# 二进制产物
target/release/FoxIR.exe

# 首次运行自动创建 workspace 目录结构并提取内嵌文件
.\target\release\FoxIR.exe
```

Release profile 配置：`opt-level = 3`、`lto = true`、`strip = true`，确保最小二进制体积与最优运行性能。

## 项目结构

```
src/
├── main.rs              # 入口：workspace 初始化、依赖装配、启动服务器
├── server.rs            # Axum HTTP/WS 服务器、REST API、SSE、/agent 等 WS 通道
├── config.rs            # TOML 配置加载
├── runner.rs            # 会话管理、Agent 调度
├── context_arbiter.rs   # 有限脑 Context Budget 仲裁器（价值排序装配）
├── deep_memory.rs       # 深层记忆（SQLite deep_facts）
├── shallow_memory.rs    # 浅层记忆（有界摘录 + FTS 召回）
├── memory.rs            # MemoryStore（memory.db + FTS5 + BM25）
├── memory_migrate.rs    # 记忆库迁移
├── sop.rs               # 动态 SOP（蒸馏 / 匹配 / 回放 / 统计）
├── knowledge.rs         # Knowledge（routing.json 路由 + experience 蒸馏）
├── value.rs             # 统一工件价值 V=Q×R×U
├── interject.rs         # 插话队列（排队 / 注入 / STOP 后入队）
├── turn_decision.rs     # 回合决策
├── permission.rs        # 权限检查器（分类门控 + 异步授权 + 绕过检测）
├── policy/              # 命令意图策略引擎（Windows + Linux）
├── agent/               # Agent trait、LlmAgent、AgentEvent
├── tool/                # 40+ 内置工具（file/shell/ir*/malware*/winrm/evidence/...）
├── managed/             # Expert 模式（TaskContract / Blackboard）
├── skill/               # SkillManager（热注入 + 步骤契约）
├── model/               # Llm trait + OpenAI-compatible client
├── scheduler.rs         # CRON 调度器
├── heartbeat.rs         # 心跳健康检查
├── checkpoint.rs        # 对话检查点（崩溃恢复）
├── crypto.rs            # AES-256-GCM 加密
├── event_log/ forensics/ security/ web/   # 事件日志 / 取证 / 安全 / 静态服务
└── tests_recall.rs      # 记忆召回测试
```

## License

MIT
