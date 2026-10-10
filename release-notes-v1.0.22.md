## FoxIR v1.0.22 — 系统变更独立权限门 + 分组工具破坏性动作参数级分类 + 子 Agent 派生总开关

本次发布给权限体系补了两处真实洞：新增第六类「系统变更」权限门（经 shell 意图识别抓取注册表/服务/计划任务/防火墙/杀软/软件装卸），并把分组管理工具的破坏性子动作（杀进程、停服务）从默认直通的 modify 类下压到需背书的 delete 类；同时落地「子 Agent 派生」进程级总开关（决策 B）。

### 系统变更独立权限门（system 第六类）
- **新增 `system` 类别，默认需背书**：不依赖工具名，靠命令意图识别把「改系统配置」从「跑命令」里拆出来——注册表写入、服务与计划任务配置、防火墙/杀软变更、软件安装卸载。
- **抓取范围**（`reaches_system_config` 高信号特征表）：`reg add/import/delete`、`sc config/create/delete`、`new-service`、`schtasks` 写操作、`unregister-scheduled*`、`netsh advfirewall/firewall`、`winget/choco install|uninstall`、`msiexec /i|/x`、`Set/Add-MpPreference`（Defender）、`disable-netadapter`；以及 PowerShell 提供者写路径的**双词组合**判定（写类 cmdlet 与 HKLM/HKCU hive 同现才算，`Set-Content 写普通文件` 不误伤）。
- **即使 execute 类已被允许也升级弹审批**：借道被放开的 shell 执行做持久化（如写 `...\Run` 启动键）不再静默通过。
- 复用既有 `parse_intent` 产物，**未改动 policy 解析器**；特征表放权限层，影响面更小。

### 分组工具破坏性动作参数级分类（封堵 containment 直通洞）
- 新增 `effective_category(工具名, 参数)`：`sys_process`/`sys_service`/`ir_process`/`cron_manage` 的 `kill`/`stop`/`restart`/`remove`/`delete`/`disable` 等破坏性子动作（首词匹配）归 **delete 类**，闸门从此对「杀进程、停服务」逐次确认。
- 检查闸门、技能授权旁路、决议卡文案三处调用点统一走它；`list`/`start`/`add` 等非破坏性子动作维持直通。
- 修复前：这些工具整体落 modify（默认直通），杀进程/停服务不弹审批，违反 AGENTS 的 containment 需确认红线。

### 旁路机制零特例继承
- `permission_locks`、技能授权台账、Expert 预授权 profile 均按类别驱动，新增 `system`/`delete` 分类**自动被覆盖**，无一行特例代码。
- WS `permissions` 热更新类别表补上 `system`；Settings 权限区新增 System 行；config 模板注释同步示例。

### 子 Agent 派生进程级总开关（决策 B）
- 新增 `SUBAGENT_ENABLED` 进程级静态门：`main.rs` 从 config 初始化，`server.rs` toggle 热写，投递层（`llm_agent`）与执行层（`spawn_subagent`）共用。
- **语义边界（决策 B）**：只管 Instant 编排链路——Off 时 `spawn_subagent` 拒绝执行且 Instant 根不投递编排工具集；**不门控 Expert 模式**（独立 Manager-Executor 串行路径，由自己的开关管）；已在飞的 worker 不强杀，`wait`/`cancel` 仍可收尾。
- 扇出提示日志格式串中英文化；附「开关关闭时派生被拒」单测。

### 已知边界（诚实标注）
- **识别是尽力而为，非对抗性防线**：`-EncodedCommand`、变量拼接、PsExec 类第三方工具绕得过字符串匹配；此门防误执行面，对抗仍靠审计与红线。
- `ext_*`/MCP 工具不参与意图检测（沿用既有边界，只覆盖 `shell_exec`/`app_launch`/`winrm`）。
- HTTP `/api/settings/agent/extended` 保存后运行时权限 map 的同步为改动前既有行为，本次未扩大。

### 未包含在本版本
- **P0-1 授权范围（Once/Session/Always + Always 落盘 + 超时 120s）**：未做，下一版实施。系统门已就位，工具名级授权不再放大漏洞，届时可安全引入。
- **技能兼容性对齐特性**（`align.rs` + `skill/mod.rs`）：与本次权限工作无关，已单独隔离，留给其会话独立成版。

### 验证
- `cargo test --lib` **514 passed / 0 failed / 20 ignored**（本版本不含技能对齐特性，其 8 条测试随之隔离；权限新增 5 条：分组动词分类、系统意图 12 正例 + 6 正对照、execute 允许下系统门端到端弹卡、containment 直通洞封堵、锁与授权覆盖）。
- Debug 构建并本机启动验证：Settings → Permissions 可见 System 行（默认 Endorsement）；`reg add`/杀进程弹审批，`reg query`/`ipconfig` 直通。
- **未做**：`cargo build --release` 与真机点击全链路验证。
