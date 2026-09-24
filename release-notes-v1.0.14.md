## FoxIR v1.0.14 — Tools 页能力中心 + 浏览器可停用/启动诊断 + 上下文缓存批次

本次发布把「能力开关」收拢到导航栏 Tools 页，修掉 Web Browser 在测试机上的启动失败，并归并一批上下文效率、早停补发与技能体系的改动。

### Tools 页能力中心（内置能力逐个启停）
- **Built-in Capabilities 表**：Web Browser / Computer Use / Linux Forensics Tools / Simulated Human Intervention 四行，一行一个开关，改完立刻落盘。
- **两种「关」不再混称**：Web Browser、Computer Use 是真注销（`unregister`，模型彻底看不见）；Linux 工具族是降为按需载入（仍可执行，只把 schema 移出请求前缀），因此该行标签写 "On-demand (still callable)" 而不是 "Disabled"。
- **Web Browser 缺省开启**（`browser_enabled = true`）；关掉时**先注销再关进程**——反过来会留下一个进程持着 `user-data-dir`，下次启动撞 Chromium 单实例互斥。
- 重新启用复用同一个 `BrowserSession`（同一份持久 profile），不会另起一份登录态。
- 集中点：src/server.rs、src/config.rs、src/main.rs、static/index.html。

### Web Browser 启动失败修复与诊断
- 新增 **src/tool/browser_launch.rs**：自己探测可执行文件（Settings 显式路径 → 环境变量 `FOXIR_BROWSER`/`CHROME` → PATH → 注册表 → 常见安装目录），不再依赖 chromiumoxide 内置检测（它在 Windows 上只查 `App Paths\chrome.exe` 且硬编码一个 x86 Edge 路径，`Program Files` 下的 Edge、按用户安装的 Chrome 一律漏掉）。
- 任何一次失败都返回**带路径/来源/版本/模式/profile 状态的诊断文本**，能区分「没装」和「装在我们不找的地方」。
- **`close()` 增加进程退出握手**（宽限 5s 后强杀）：原实现只发一个 CDP `Browser.close` 就 drop，进程还在持有 user-data-dir，导致紧接着的第二次启动「看起来像启动失败」。
- 持久 profile 落在 workspace 内、只增不删；`status()` 自检不叫醒浏览器；报告导出改用共享实例上的 `scratch_page()`，不再为导出另起一个浏览器。
- Settings 保留无头/路径两个子开关，热更新（下一次启动即生效）。

### GUI 配置全部落盘
- 修 **computer_use / human_intervention 改了不存**的真 bug：两者原先只改内存，重启静默回滚；`human_intervention` 此前在 main.rs 里硬编码 false，压根没读 config。
- **External Tools 状态并进 config.toml**（`[agent.external_tools]`），废弃 `tools/tools_state.json`：旧文件只读一次性导入、不删除；sidecar `xxx.json` 的描述仍不进 config（保持 Tools 目录可拷走即用），只有用户手改过的才持久化。
- 新增 setter 一律 `Config::load()?`，load 失败向上报错——绝不拿 `unwrap_or_default()` 续跑，否则一个开关能把整个 config.toml（provider 配置、权限段）抹成默认值。

### 提示词与能力同步
- `browser_cdp` 那段系统提示改为按开关拼接：注销工具的同时撤掉说明，不留「提示词里吹着、工具表里已没有」的口子。
- 子 Agent（`OrchestratorEnv`）继承同一份开关，worker 不会复述父进程已停用的能力。

### 上下文与缓存批次
- prefix-cache 友好的提示切分 + PromptTier；易变状态不再尾随请求（中途停答修复）。
- usage_stats **schema v9**：新增 `cached_tokens`，`NULL`（端点不报缓存账）与 `0`（报了且没命中）严格分开，缓存盲的模型不会把命中率伪造成 0%；迁移幂等、重开不报错。
- 展示层与裁切层读同一组数字，修掉 Memory/Knowledge/SOP 重复计数导致的「used 虚高、free 虚低」。
- 传输超时治理 + 流截断恢复（read timeout 在终止 finish_reason 之后不再算作截断）。

### 早停补发 / 队列 / 收尾固化
- **pending-action nudge**：模型宣告了动作（「我这就去查:」）却以纯文本收场时一次性补发（每 run 上限 1 次），探测器只看末句、只认极短回复，宁可漏判也不烧预算。
- 修 follow-up 队列 busy-spin；运行结束而无客户端时把孤儿队列写进日志而非静默丢弃。
- **debrief**：会话收尾做结论固化（case writeup + 可选 SOP），报告导出链路复用浏览器会话。

### 技能与杂项
- skill 采用纯 agentskills.io schema、模型自路由 + 平台/依赖门控 + 遥测；同名管理大小写不敏感；`install_skill`/`create_skill` 路径穿越修复与去重、自动版本 bump。
- TASKS 上下文改为按需载入（不再逐轮催用户）、问候轮不再泄漏任务信息；用户气泡里的链接不再是主题色叠主题色（看不见 URL）。
- `linux_ir_tool_names()` 从实际工具对象派生并缓存，新增分类工具不会绕过投递门控；`linux_ssh` 刻意不入族，保持 Linux 主机始终可达。

### 验证
- `cargo check --all-targets` 通过（仅剩既有无关 warning）；`cargo test --lib` **364 passed / 0 failed / 3 ignored**；`cargo test --tests` 集成批次全绿。
- 前端三段内联脚本做语法校验通过；确认无 `linuxIrTools`/`note_unused` 等残留引用。
- 新增 4 个 config 单测：开关存后读回、坏 config.toml 时 setter 报错且不抹平原文件、External Tools 全量重写不滞留已删工具、空 map 不产出多余配置段。
- **未做**：`cargo build --release` 与重新打包 zip；Tools 页的真机点击验证（按指示先发布）。`browser_cdp` 的真机起停用例仍以 `#[ignore]` 保留（`cargo test --lib browser_cdp -- --ignored --nocapture`）。
- 可插拔记忆后端（MemoryProvider）本次仅规格、**未实施**，其文档未随本次发布入库。
