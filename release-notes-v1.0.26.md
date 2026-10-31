## FoxIR v1.0.26 — 模型配置补上「Vision」开关；Hindsight 端点与凭据去硬编码，改由 config.toml 提供

本版两件事：给 `supports_vision` 补上界面上唯一缺失的一环（无法自助开关）；把 Hindsight 远端记忆的端点与 API key 从源码常量里移出去，改由部署方的 `config.toml` 提供、凭据密文落盘。同时本版起版本号回归纯语义化命名（不再带 `-GISC` 后缀）。

### Hindsight 端点与凭据外置（breaking for deployers）
- **源码不再内置任何端点、凭据或 bank 标识**：删除 `HindsightConfig::builtin()` 与 `BUILTIN_BASE_URL` / `BUILTIN_API_KEY` / `BUILTIN_BANK_ID` 三个常量。
- **改由 `config.toml` 的 `[agent.hindsight]` 提供**：`base_url` / `api_key` / `bank_id` / `enabled` 全部读配置文件；`Config::load` 不再覆盖这些值。
- **凭据密文落盘**：`Config::save` 早已对 `api_key` 加 `ENC:`（AES-256-GCM，机器绑定密钥）；本版补上对称的一半——`Config::load` 读回时自动解密，密文不会以 `ENC:` 原样进入运行期。
- **缺值即停用，且会说出来**：`base_url` 或 `api_key` 任一为空 → `enabled` 归为 `false` 并打一条 WARN（"config.toml is missing …; skipping remote memory"），不会留下"看起来启用、实则必 401"的状态。默认构造为停用 + 三个空串。
- **Settings 侧行为不变**：`POST` 保存仍被拒绝（值归部署文件管），GET 继续展示当前生效值且 key 掩码；"测试连接"只测配置里生效的端点。
- **部署要做的事**：在目标机的 `workspace/config.toml` 里写入 `[agent.hindsight]` 的 `base_url` / `api_key` / `bank_id` 并置 `enabled = true`。缺这三项的机器只是不做远端记忆同步，本地记忆不受影响。

### 新增：Provider 表单的 Vision 开关（feat，前后端 + 持久化）
- **UI**：Settings → Provider 表单在 `Temp` 右侧新增 `Vision` 滑块开关，沿用页面既有 `perm-toggle` 样式（亮暗主题一致），默认开启。
- **默认值**：`ModelConfig.supports_vision` 的 serde 默认由 `false` 改为 `true`（`src/config.rs`）。**但存量模型不会被自动打开**——`models.json` 每条都显式持久化了该键，因此用户明确关掉的模型保持关闭，需要自助开启时在界面上点一次即可。
- **API**：`POST /api/providers` 缺省该字段时按 `true` 处理；`PUT` 仍保持"未提供则沿用原值"；`GET /api/providers` 现在返回 `supports_vision`（编辑表单回填的前提，此前该字段在列表响应里缺失）。
- **回填语义从严**：前端编辑时用 `=== true` 严格判定，读不到即显示为关——避免"界面显示开、保存时把用户关掉的模型静默打开"。

### 修复：聊天里的截图/产物链接显示为破图（fix，前端 + 工具返回）
- **症状**：回复里 `![Cover](en-01-cover.png)` 这类图片渲染成破图图标。当天 8 条带图回复里 5 条如此；同族实现也发作过一次（写法不同：`C:/Users/…/output/x.jpg`）。
- **根因**：web 端只有 `/workspace/{*path}` 一条能服务工作区文件的路由，而模型在 markdown 里写的是裸文件名或盘符绝对路径 —— 浏览器按页面 origin 解析成 `/en-01-cover.png` → 404。提示词里其实**早已**写明"必须用工具返回的 `url`、不要用本地路径"（`llm_agent.rs:1056`），但模型仍高频违反，所以本版不再依赖模型自觉。
- **前端兜底（主修）**：`normalizeMediaUrls` 在渲染出口把不可解析的 src 归一化 —— 盘符绝对路径若含 `/workspace/` 段则保留其后完整路径，否则退到文件名；裸文件名与相对子目录补 `/workspace/output/` 前缀；`http(s):` / `data:` / `mailto:` / `#锚点` / 根路径一律不动；无已知文件扩展名的字符串不动，避免误伤普通链接。
- **顺序修正**：`renderMd` 原先是"先净化后归一"，而 `sanitizeHtml` 会把 `C:/…` 当作未知协议**整段剥掉 src**（表现为图片完全不显示，连破图都没有）。改为"先归一后净化"，净化仍是最后一步，XSS 防护不变。
- **工具侧**：`browser_cdp` 的 screenshot 与 `cu_screenshot` 返回体新增 `markdown` 字段（`![文件名](/workspace/output/文件名.png)`），提示词改为整段复制，减少模型自己拼路径的机会。
- **顺带修掉一个同类隐患**：`cu_screenshot` 过去把 url 写死为 `/workspace/output/<name>`，但 Expert 轮次产物实际落在 `managed/<合同>/round_NNN/`，那种情况下链接必然 404。`display_url` 提升为 `src/tool/mod.rs` 共享实现，两处统一由文件真实落点推导 URL。

### 已知边界（诚实标注）
- **`supports_vision` 是人工信任标记**，不代表端点真实具备视觉能力。标成 `true` 而实际无视觉编码器的端点会绕过本地拦截并让模型"装作知道"（凭注入的历史/记忆去猜图内容），这比标错的 `false`（会明确报 `does not support image input`）更坏。本版未引入能力探测。
- **跨轮次不保留图片**：`src/server.rs` 把用户轮写入会话历史时只存文本（`ChatMessage::user(&content)`），因此同会话的**后续追问**请求体里不再包含 `image_url`。同一回合内的多轮（工具调用后）不受影响。本版未修。
- 前端 `modelVisionMap` 仍是赋值后未被读取的死值，模型选择器不会据此提示"该模型无视觉"。
- 链接归一化的兜底目标是 `workspace/output`（截图与导出物的实际落点）。若将来有产物写到别处，必须由工具侧 `display_url` 返回正确 URL，不能指望前端猜。

### 验证
- 新增 7 条单测。Hindsight 四条：值只来自 `config.toml`；`api_key` 密文落盘且读回为明文；缺端点或缺 key 时停用；默认构造不含端点与凭据。（TDD 轨迹：这四条先写成红——失败原因正是"load 返回了内置端点/内置 key"，随后才改实现。）
- Vision 三条：默认值必须为 `true`；显式 `false` 不得被新默认翻转；`models.json` 往返持久化保持两侧取值。
- `cargo test --lib` **535 passed / 0 failed / 20 ignored**。
- 前端归一化：`tests/js/normalize_media_urls.mjs`（`node tests/js/normalize_media_urls.mjs`）从 `static/index.html` 抽出真实函数体跑 **19 用例全绿**（含日志里 4 种真实错法与 9 条"必须原样不动"）。TDD 轨迹：先 8 红 10 绿，实现后 18/18；浏览器端到端又抓到 `marked` 把反斜杠转义成 `%5C` 的漏网用例，补测后 19/19。
- 渲染顺序修正经浏览器实测确认：`C:/…` 写法在旧顺序下会被 `sanitizeHtml` 连 `src` 一起剥掉（图片完全不显示），改序后正确归一。
- 端到端（隔离实例，端口 7795，`output/` 放一张真实 PNG）：`![Cover](en-01-cover.png)`、`C:/…/workspace/output/x.jpg`、`C:%5CUsers%5C…`、`D:/shots/shot.png` 四种写法渲染后均为 `/workspace/output/<文件>`；`/workspace/…`、`data:`、无扩展名链接保持原样不动；改写后的 URL 实际加载成功（`naturalWidth=32`）。对照组：直接请求 `/en-01-cover.png` 返回 **404**，正是裂图成因。
- `cargo build --release` 通过；`/api/health` 返回 `"version":"1.0.26"`；产物字节扫描确认 **不含**原内置端点与凭据（两项均 0 命中）。
- 全树扫描确认：源码与文档中已无原内置端点地址，也无任何凭据明文（`git grep` 对二者均无命中）。
- 隔离实例（重定向 `USERPROFILE`，不触碰真实工作区）实测：不带键新建 → `true`；显式 `false` → 保持 `false`；`PUT` 不带键 → 沿用原值；**杀进程重启后开关值不变**；UI 点开关关掉再"Add Model" → 落盘 `supports_vision: false`；编辑存量关闭模型 → 开关正确显示为关。
- 用一个假 OpenAI 端点把请求体原样落盘，实测确认：**首轮请求含 `{"type":"image_url","image_url":{"url":"data:image/png;base64,…"}}`，同回合第二轮仍含**；上面第二条边界即由此暴露，而非推测。
