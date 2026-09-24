//! Browser CDP tool — Chrome DevTools Protocol browser automation via chromiumoxide.
//!
//! Actions:
//! - `navigate`: Go to a URL
//! - `get_text`: Get page or element text
//! - `click`: Click an element by CSS selector
//! - `type_text`: Type text into an element
//! - `screenshot`: Take a screenshot, save to workspace
//! - `get_url`: Get current page URL
//! - `get_html`: Get page or element HTML
//! - `execute_js`: Execute JavaScript and return result
//! - `find_element`: Find element and return its attributes
//! - `list_tabs`: read-only inventory of every page the browser has
//! - `probe`: Report which browser would be used + current session state
//! - `close`: Close the browser session
//!
//! One page at a time: the session owns exactly one page and every page-level action
//! runs on it. Pages opened by the site itself are visible through `list_tabs` /
//! `tab_count` but cannot be driven (chromiumoxide 0.9 gives no reliable handle for a
//! target we did not create) — bring the URL back with `navigate` instead.

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams,
};
use chromiumoxide::cdp::browser_protocol::target::{GetTargetsParams, TargetId, TargetInfo};
use chromiumoxide::page::Page;
use chromiumoxide::Element;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard};
use tracing::{info, warn};

use super::{TimeoutStage, Tool};
use super::browser_launch;
use crate::context::ToolContext;
use crate::error::AgentResult;

/// 关闭会话时等待浏览器进程真正退出的宽限，超时才强杀。
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// selector 等待上限。chromiumoxide 0.9 没有 `wait_for_selector`（只有
/// `wait_for_navigation`），所以"元素还没渲染"只能自己轮询。
const SELECTOR_WAIT: Duration = Duration::from_millis(8_000);

/// 点击之后给导航留的等待窗口：真导航在飞就等它落地，静态页面会立刻返回。
const CLICK_NAV_WAIT: Duration = Duration::from_millis(2_000);

/// 一次调用内的时间戳后缀。原来只到秒，同一秒里两张截图会互相覆盖。
fn stamp() -> String {
    let now = chrono::Local::now();
    format!("{}_{:06}", now.format("%Y%m%d_%H%M%S"), now.timestamp_subsec_micros())
}

/// 一段正文的切片结果（见 `slice_text`）。
struct Slice {
    window: String,
    offset: usize,
    total: usize,
    truncated: bool,
    next_offset: usize,
    path: Option<String>,
}

/// 截断 + 全文落盘 + 续读偏移。
///
/// 只回头部会把整页文章腰斩，而调用方除了整页重取没有第二条路——那一页又要重取一遍、
/// 再烧一次上下文。所以超阈值时把全文写进 output/，返回 path，同时给出 next_offset
/// 让调用方按字符续读。
fn slice_text(full: &str, offset: usize, max: usize, output_dir: &str, file_name: &str) -> Slice {
    let total = full.chars().count();
    let window: String = full.chars().skip(offset).take(max.max(1)).collect();
    let end = offset + window.chars().count();
    let truncated = end < total;
    let mut path = None;
    if truncated {
        let p = PathBuf::from(output_dir).join(file_name);
        match std::fs::write(&p, full.as_bytes()) {
            Ok(()) => path = Some(p.to_string_lossy().to_string()),
            Err(e) => warn!("Browser CDP: cannot spill full text to {}: {}", p.display(), e),
        }
    }
    Slice { window, offset, total, truncated, next_offset: end, path }
}

/// 截图给人看要的是一个真能打开的 URL。
///
/// `/workspace/{*path}` 服务的是整个 workspace，而 `ctx.output_dir()` 在 Expert/managed
/// run 里指向 `managed/<contract>/round_NN/`，并不等于 `workspace/output` —— 硬编码前缀
/// 会让模型贴出来的图片链接 404。所以由真实落盘路径反推。
fn display_url(path: &Path, workspace_dir: &str) -> String {
    if let (Ok(ws), Ok(p)) = (
        std::fs::canonicalize(workspace_dir),
        std::fs::canonicalize(path),
    ) {
        if let Ok(rel) = p.strip_prefix(&ws) {
            return format!("/workspace/{}", rel.to_string_lossy().replace('\\', "/"));
        }
    }
    path.to_string_lossy().to_string()
}

/// `page.content()` 是原始 HTML：script / style / 注释 / 内联 base64 常常占掉大半，
/// 提取正文时它们既挤占上下文限额又是噪声。粗粒度剥掉，保留标签结构与文本。
/// 需要原文时传 `raw: true`。
fn strip_html_noise(html: &str) -> String {
    const PATTERNS: [&str; 4] = [
        r"(?is)<script\b.*?</script\s*>",
        r"(?is)<style\b.*?</style\s*>",
        r"(?s)<!--.*?-->",
        r#"(?i)data:[^"'()\s<]{80,}"#,
    ];
    let mut out = html.to_string();
    for pat in PATTERNS {
        if let Ok(re) = regex::Regex::new(pat) {
            out = re.replace_all(&out, "").into_owned();
        }
    }
    out
}

/// 等元素出现，最多 `SELECTOR_WAIT`。
///
/// 库里 `find_element` 是一次性 DOM 查询，在 SPA / 懒加载页面上"还没渲染出来"和
/// "确实不存在"报的是同一个错——调用方只能靠重试瞎猜。轮询到超时才把最后一条错误
/// 抛出去，两者就分开了；也不再需要固定 sleep。
async fn find_element_wait(page: &Page, selector: &str) -> Result<Element, String> {
    let deadline = tokio::time::Instant::now() + SELECTOR_WAIT;
    loop {
        match page.find_element(selector).await {
            Ok(elem) => return Ok(elem),
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(e.to_string());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// 活动 tab：id 与句柄一起缓存。
/// 我们自己创建、并且握有可用句柄的那一页。
///
/// 只有一页，这是刻意的收缩——FoxIR 的用法就是"一次盯一页"。完整的多 tab（`new_tab`/
/// `switch_tab`/`close_tab` + 按 target id 现取句柄）实现过，然后被两条真机事实否掉：
/// 1. `Target.getTargets` 的返回顺序不稳定（最后开的会排在第一个），所以列表下标不是
///    可靠的 tab 标识；
/// 2. 用 `Browser::get_page(target_id)` 现取的句柄发命令会 `channel disconnected`
///    （handler 只在 target 已经有 session 时才建 PageHandle，`handler/target.rs:162-169`），
///    连我们自己从 `new_page` 拿到的句柄、跨调用再持有也会失效。
/// 结论：在这一层上"驱动不是我们自己创建的那一页"没有可靠做法，于是只保留我们握得住的
/// 一页。站点自己开新页时的需求（"我现在脚下是哪一页、那新页是什么"）由只读的 `list_tabs`
/// 覆盖，要把内容拿回来就用 `navigate`——那条路每一环都验证过。
struct ActiveTab {
    id: TargetId,
    page: Page,
}

/// Inner state holding the browser connection.
struct BrowserInner {
    browser: Browser,
    /// 唯一那一页。它被站点关掉时我们新开一页顶上（见 `replace_tab`），不去接手别人的页。
    tab: Option<ActiveTab>,
}

impl BrowserInner {
    fn tab_id(&self) -> Option<&TargetId> {
        self.tab.as_ref().map(|t| &t.id)
    }

    fn page(&self) -> Option<Page> {
        self.tab.as_ref().map(|t| t.page.clone())
    }

    /// 认领一页作为我们唯一那一页，并把句柄交回给调用方。
    fn claim(&mut self, page: Page) -> Page {
        self.tab = Some(ActiveTab {
            id: page.target_id().clone(),
            page: page.clone(),
        });
        page
    }
}

/// 问浏览器现在有哪些页。
///
/// **不要用 `Browser::fetch_targets()`**：0.9.1 里它除了返回列表，还会对每个 target 重放
/// 一次 `on_target_created`（`handler/mod.rs:216-233`），而那个函数是无条件 `targets.insert`
/// —— 已在册的 Target 对象被整个换掉，我们手上每一个 `Page` 句柄的通道随之作废，下一条
/// 命令必然是 `channel disconnected`。今天所有的句柄失效现象都是这一处造成的。走普通命令
/// 路径拿同一份列表，不碰它的注册表。
async fn list_pages(browser: &mut Browser) -> Result<Vec<TargetInfo>, String> {
    let resp = browser
        .execute(GetTargetsParams { filter: None })
        .await
        .map_err(|e| format!("Failed to list browser pages: {}", e))?;
    Ok(resp.result.target_infos)
}

/// `Target.getTargets` 里只有 `type == "page"` 且不是预渲染的才算一个页。
fn page_targets(targets: Vec<TargetInfo>) -> Vec<TargetInfo> {
    targets
        .into_iter()
        .filter(|t| t.r#type == "page" && t.subtype.as_deref() != Some("prerender"))
        .collect()
}

/// tab 列表的 JSON 视图（`active` 标出的是我们唯一能驱动的那一页）。
fn tab_view(live: &[TargetInfo], active: Option<&TargetId>) -> Vec<Value> {
    live.iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
                "index": i,
                "url": t.url,
                "title": t.title,
                "active": active.map(|a| a == &t.target_id).unwrap_or(false),
                // 看得见但挂不上的条目（预渲染之类）要标出来：否则调用方只会拿到一句
                // 冷冰冰的 NotFound，不知道为什么"列表里明明有"。
                "attachable": t.attached,
            })
        })
        .collect()
}

/// CDP 报"这个页面/目标已经没了"的错误文案。库里没有稳定错误码，只能按文案识别；
/// 命中就走 `refresh_tabs` 重解析，而不是直接把这轮失败丢给调用方。
fn is_target_gone(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("requested value not found")
        || e.contains("target is gone")
        || e.contains("no such target")
        || e.contains("no such executed frame")
        || e.contains("cannot find context")
        || e.contains("execution context was destroyed")
        || e.contains("detached while handling command")
        || e.contains("session with given id not found")
}

/// 让浏览器进程真的退出：CDP close（带超时）→ 等退出（带超时）→ 强杀。
///
/// `Browser::close()` 只是把关闭请求发出去就返回，进程还在刷盘、还在写 profile；光靠
/// `Drop` 的 `kill_on_drop` 也不行——它只发信号不等回收，而且只针对父进程（Chromium 的
/// renderer/gpu 子进程是否随之退出，*待真机验证*）。Chromium 的单实例互斥按
/// user-data-dir 划分，"下一个实例起不来"的根因就是这个目录还被上一个实例持有。
/// 所以 `close` / `clear_state` / launch 失败分支 / 报告导出的自起实例，四条路全部收敛
/// 到这里，不再有哪条路是"直接 drop 走人"。
pub(crate) async fn shutdown_browser(mut browser: chromiumoxide::browser::Browser) {
    match tokio::time::timeout(CLOSE_GRACE, browser.close()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => warn!("Browser CDP: Browser.close failed: {}", e),
        Err(_) => warn!(
            "Browser CDP: Browser.close did not return within {}s",
            CLOSE_GRACE.as_secs()
        ),
    }
    match tokio::time::timeout(CLOSE_GRACE, browser.wait()).await {
        Ok(Ok(status)) => info!("Browser CDP: browser process exited ({:?})", status),
        Ok(Err(e)) => {
            warn!("Browser CDP: waiting for browser exit failed: {}", e);
            let _ = browser.kill().await;
        }
        Err(_) => {
            warn!(
                "Browser CDP: browser still alive after {}s, killing it",
                CLOSE_GRACE.as_secs()
            );
            let _ = browser.kill().await;
        }
    }
}

/// Shared browser session with lazy initialization and auto-recovery.
pub struct BrowserSession {
    inner: Mutex<Option<BrowserInner>>,
    workspace_dir: String,
    /// 持久浏览器 profile：跨启动保留，登录态就住在这里，绝不删除。
    /// 位置由 `default_profile_dir` 决定（在 workspace 之外），测试可用
    /// `with_profile_dir` 指到临时目录。
    profile_dir: PathBuf,
    /// 无头开关（Settings 热更）。true = 无头（缺省）。
    headless: Arc<AtomicBool>,
    /// Settings 里显式指定的浏览器可执行文件路径，空串 = 自动探测。
    executable_override: Arc<RwLock<String>>,
    /// Set to false when the handler event stream ends (browser closed/crashed).
    browser_alive: Arc<AtomicBool>,
    /// Generation counter: incremented on every launch/close. A handler task only
    /// marks the session dead if its generation still matches — this prevents a
    /// stale handler (from a crashed browser) from killing a freshly re-launched one.
    generation: Arc<AtomicU64>,
}

/// 浏览器 profile 的缺省位置：`<本地应用数据目录>/FoxIR/browser_profiles/<案名>-<hash8>`
/// （Windows 即 `%LOCALAPPDATA%`，Linux 即 `$XDG_DATA_HOME`）。
///
/// 不能放在 workspace 里，两个理由：
/// 1. `/workspace/{*path}` 是免鉴权的静态文件路由（只防 traversal），而 profile 里装的是
///    登录后的 cookie —— 放在里面等于把凭据挂在一个可读目录上；
/// 2. case 目录要被打包、移交，profile 跟进去就是证据包里夹带活凭据。
///
/// 末尾的 hash 让每个 workspace 各拿一份目录：Chromium 的单实例互斥按 user-data-dir
/// 划分，两个案子共用一份 profile 会直接起不来。取 canonical 路径做 hash，所以同一个
/// 目录写成 `g:/x` 或 `G:\X` 都落在这一个 profile 上。
pub fn default_profile_dir(workspace_dir: &str) -> PathBuf {
    let canonical = std::fs::canonicalize(workspace_dir)
        .unwrap_or_else(|_| PathBuf::from(workspace_dir));
    let base = dirs_next::data_local_dir().unwrap_or_else(std::env::temp_dir);
    let label: String = canonical
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "case".to_string())
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(24)
        .collect();
    let label = if label.is_empty() { "case".to_string() } else { label };
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let suffix: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    base.join("FoxIR")
        .join("browser_profiles")
        .join(format!("{label}-{suffix}"))
}

/// 旧版本把 profile 建在 workspace 里（`<workspace>/.browser_profile`），第一次启动时
/// 搬到新位置，登录态跟着走。搬不动（最常见是跨盘）只警告，旧目录原样留着——绝不删。
fn migrate_legacy_profile(workspace_dir: &str, profile_dir: &Path) {
    let legacy = PathBuf::from(workspace_dir).join(".browser_profile");
    if !legacy.is_dir() || profile_dir.exists() {
        return;
    }
    let Some(parent) = profile_dir.parent() else { return };
    if let Err(e) = std::fs::create_dir_all(parent) {
        warn!("Browser CDP: cannot create {}: {}", parent.display(), e);
        return;
    }
    match std::fs::rename(&legacy, profile_dir) {
        Ok(()) => info!(
            "Browser CDP: moved the browser profile {} -> {}",
            legacy.display(),
            profile_dir.display()
        ),
        Err(e) => warn!(
            "Browser CDP: an old in-workspace profile exists at {} but could not be moved ({}); \
             leaving it in place, so the new profile at {} starts logged out.",
            legacy.display(),
            e,
            profile_dir.display()
        ),
    }
}

impl BrowserSession {
    /// `headless` / `executable_override` 是 Settings 的热更开关：会话只持有原子和锁的
    /// 引用，切换不需要重启进程，下一次启动浏览器时即生效。
    pub fn new(
        workspace_dir: String,
        headless: Arc<AtomicBool>,
        executable_override: Arc<RwLock<String>>,
    ) -> Arc<Self> {
        let profile_dir = default_profile_dir(&workspace_dir);
        Self::with_profile_dir(workspace_dir, headless, executable_override, profile_dir)
    }

    /// 指定 profile 目录的构造器。`new` 之外还要一个，是因为测试不能往
    /// `%LOCALAPPDATA%` 里种真实 profile。
    pub(crate) fn with_profile_dir(
        workspace_dir: String,
        headless: Arc<AtomicBool>,
        executable_override: Arc<RwLock<String>>,
        profile_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(None),
            workspace_dir,
            profile_dir,
            headless,
            executable_override,
            browser_alive: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Settings 里的显式路径（读锁被 poison 时按“自动探测”处理，不因此报错）。
    fn override_path(&self) -> String {
        self.executable_override
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// 统一出口：写日志并返回同一份诊断文本，保证 UI 看到的就是日志里的。
    fn launch_failed(
        &self,
        ctx: browser_launch::LaunchContext,
        raw: &str,
        waited: u64,
    ) -> String {
        let msg = browser_launch::describe_failure(&ctx, raw, waited);
        warn!("Browser CDP: {}", msg);
        msg
    }

    /// Check if the browser process is still alive.
    fn is_alive(&self) -> bool {
        self.browser_alive.load(Ordering::Relaxed)
    }

    /// 丢掉会话状态：先把进程真正送走，再标死。
    async fn clear_state(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(inner) = guard.take() {
            shutdown_browser(inner.browser).await;
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.browser_alive.store(false, Ordering::Relaxed);
    }

    /// Remove Chrome Singleton* files from a profile dir.
    /// After a hard process kill these stale files can block Chrome re-launch.
    ///
    /// 只在 Unix 上有意义：Windows 上的 Chrome/Edge 不创建这几个文件（实测 0 命中），
    /// 那里真正的保护是 close() 里的进程退出握手。留着是为了 Linux 侧复用同一套逻辑。
    fn clean_profile_locks(profile_dir: &PathBuf) {
        for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            let p = profile_dir.join(name);
            if p.exists() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    /// 保证有一个活着的浏览器，并返回持有它的锁守卫。
    ///
    /// 整段（检查 + 启动）都持锁，所以并发调用不会起出两个实例。
    async fn lock_ready(&self) -> Result<MutexGuard<'_, Option<BrowserInner>>, String> {
        let mut guard = self.inner.lock().await;
        if !self.is_alive() || guard.is_none() {
            // Slow path: clear stale state and (re-)launch while holding the lock
            guard.take();
            self.launch_locked(&mut guard).await?;
        }
        Ok(guard)
    }

    /// 我们那一页的句柄（快路径，不碰 CDP）；浏览器没起来就先起来。
    async fn get_or_init(&self) -> Result<Page, String> {
        let mut guard = self.lock_ready().await?;
        let inner = guard
            .as_mut()
            .ok_or_else(|| "Browser session is not running".to_string())?;
        inner.page().ok_or_else(|| "Browser session has no page".to_string())
    }

    /// 我们那一页被站点关掉时，新开一页顶上。
    ///
    /// 只新开、不接手：不去驱动不是我们自己创建的页（原因见 `ActiveTab`）。
    /// Caller MUST hold the inner lock.
    async fn replace_tab(&self, inner: &mut BrowserInner) -> Result<Page, String> {
        let page = inner
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| format!("Browser is running but no page could be opened: {}", e))?;
        Ok(inner.claim(page))
    }

    /// 我们那一页在 CDP 侧的 (url, title)。
    ///
    /// 为什么要这份兜底：`Page::url()` / `get_title()` 读的是 handler 里的 frame 状态，
    /// 可能比 CDP 的说法慢、甚至为空（真机实测过两次）。以 CDP 的说法为准，句柄自己的
    /// 说法为辅。
    async fn active_tab_info(&self) -> Option<(String, String)> {
        let mut guard = self.inner.lock().await;
        let inner = guard.as_mut()?;
        let id = inner.tab_id()?.clone();
        let live = self.live_pages_locked(inner).await.ok()?;
        live.iter()
            .find(|t| t.target_id == id)
            .map(|t| (t.url.clone(), t.title.clone()))
    }

    /// 失败恢复用：确认我们那一页还在不在，不在就新开一页。返回是否换过一页
    /// （换过就意味着页面状态没了，只有 `navigate` 能直接重试）。
    async fn recover_page(&self) -> Result<bool, String> {
        let mut guard = self.inner.lock().await;
        let inner = match guard.as_mut() {
            Some(inner) => inner,
            None => return Ok(false),
        };
        if let Some(id) = inner.tab_id().cloned() {
            let live = self.live_pages_locked(inner).await?;
            if live.iter().any(|t| t.target_id == id) {
                return Ok(false);
            }
        }
        warn!("Browser CDP: our page is gone, opening a fresh one");
        self.replace_tab(inner).await?;
        Ok(true)
    }

    /// 浏览器里现在有几个页（只读）。我们只能知道有这回事，不能切过去驱动别人开的页，
    /// 所以动作结果里带一个 `tab_count`，具体是什么用 `list_tabs` 看。
    async fn tab_count(&self) -> usize {
        let mut guard = self.inner.lock().await;
        match guard.as_mut() {
            Some(inner) => self
                .live_pages_locked(inner)
                .await
                .map(|l| l.len())
                .unwrap_or(0),
            None => 0,
        }
    }

    /// Launch a fresh browser instance. Caller MUST hold the inner lock.
    ///
    /// 可执行文件由 `browser_launch::discover` 自己探测（Settings 显式路径 → 环境变量
    /// → PATH → 注册表 → 常见安装目录），不依赖 chromiumoxide 的内置检测；任何一步
    /// 失败都返回带路径/来源/版本/模式/profile 状态的诊断文本。
    async fn launch_locked(
        &self,
        guard: &mut MutexGuard<'_, Option<BrowserInner>>,
    ) -> Result<(), String> {
        let headless = self.headless.load(Ordering::Relaxed);
        let discovery = browser_launch::discover(&self.override_path());

        let chosen = match discovery.chosen.clone() {
            Some(c) => c,
            None => {
                let summary = discovery.summary();
                let ctx = browser_launch::LaunchContext {
                    chosen: None,
                    tried: discovery.tried,
                    profile_dir: self.profile_dir.clone(),
                    headless,
                };
                return Err(self.launch_failed(ctx, &summary, 0));
            }
        };
        let diag_ctx = browser_launch::LaunchContext {
            chosen: Some(chosen.clone()),
            tried: discovery.tried.clone(),
            profile_dir: self.profile_dir.clone(),
            headless,
        };
        info!(
            "Browser CDP: launching {} (v{}) headless={} ...",
            chosen.describe(),
            browser_launch::version_from_layout(&chosen.path),
            headless
        );

        migrate_legacy_profile(&self.workspace_dir, &self.profile_dir);
        if let Err(e) = std::fs::create_dir_all(&self.profile_dir) {
            warn!(
                "Browser CDP: cannot create profile dir {}: {}",
                self.profile_dir.display(), e
            );
        }
        Self::clean_profile_locks(&self.profile_dir);

        // no_sandbox: prevents exit code 21 (sandbox init failure on some Windows configs).
        // user_data_dir: 持久目录，登录态住在这里，跨启动保留，绝不删除。
        // no-startup-window: Edge 即使带了 --no-first-run（chromiumoxide 的默认参数里就有）
        //   也会自己开一个 edge://newtab/ 的 page target，白占一个 tab 位；让它零 tab 起来，
        //   第一个 tab 由下面 new_page 建出来。
        let mut builder = BrowserConfig::builder()
            .no_sandbox()
            .chrome_executable(&chosen.path)
            .user_data_dir(&self.profile_dir)
            .launch_timeout(Duration::from_secs(browser_launch::LAUNCH_WAIT_SECS))
            .arg("no-startup-window")
            .viewport(chromiumoxide::handler::viewport::Viewport {
                width: 1920,
                height: 1080,
                device_scale_factor: Some(1.0),
                emulating_mobile: false,
                is_landscape: true,
                has_touch: false,
            });

        // 缺省无头：没有可见窗口，用户误关不了，也不会把窗口弹在取证桌面上。
        // Settings 勾掉无头后走 with_head()（HeadlessMode::False）——这是“登录一次、
        // 长期复用”能成立的前提：无头下没人能输密码 / 走 2FA / 扫码。
        if !headless {
            builder = builder.with_head();
        }

        let config = match builder.build() {
            Ok(c) => c,
            Err(e) => {
                return Err(self.launch_failed(
                    diag_ctx,
                    &format!("Failed to build browser config: {}", e),
                    0,
                ));
            }
        };

        let started = std::time::Instant::now();
        let (browser, mut handler) = match Browser::launch(config).await {
            Ok(v) => v,
            Err(e) => {
                return Err(self.launch_failed(
                    diag_ctx,
                    &format!("{}", e),
                    started.elapsed().as_secs(),
                ));
            }
        };

        // New generation for this launch
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.browser_alive.store(true, Ordering::Relaxed);

        // Spawn the handler task — when the stream ends, mark browser as dead,
        // but ONLY if this is still the current generation (prevents a stale
        // handler from a crashed browser killing a freshly re-launched one).
        let alive = self.browser_alive.clone();
        let gens = self.generation.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(_event) = handler.next().await {
                // Events are processed internally by the handler
            }
            // Stream ended => browser process exited or was closed by user
            if gens.load(Ordering::SeqCst) == gen {
                warn!("Browser CDP: handler stream ended (gen {}) — browser closed or crashed", gen);
                alive.store(false, Ordering::Relaxed);
            } else {
                info!("Browser CDP: stale handler (gen {}) ended, current gen newer — ignored", gen);
            }
        });

        let page = match browser.new_page("about:blank").await {
            Ok(p) => p,
            Err(e) => {
                // 起都起来了却拿不到第一个 tab：这条路上浏览器进程还活着、还占着
                // user-data-dir，直接返回错误等于把下一次 launch 也废掉。
                shutdown_browser(browser).await;
                return Err(self.launch_failed(
                    diag_ctx,
                    &format!("Browser started but the initial tab failed: {}", e),
                    started.elapsed().as_secs(),
                ));
            }
        };

        let id = page.target_id().clone();
        info!("Browser CDP: browser launched successfully (gen {}, 1 tab)", gen);

        let mut inner = BrowserInner { browser, tab: None };
        drop(id);
        inner.claim(page);
        **guard = Some(inner);

        Ok(())
    }

    /// 关掉整个会话。等进程真退出后才返回（握手过程见 `shutdown_browser`）。
    pub async fn close(&self) -> Result<(), String> {
        let mut guard = self.inner.lock().await;
        if let Some(inner) = guard.take() {
            info!("Browser CDP: closing browser");
            shutdown_browser(inner.browser).await;
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.browser_alive.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// 在已运行的共享实例上开一个临时 tab（报告导出用），调用方用完自己关 tab。
    ///
    /// 不要为了导出另起一个浏览器：同一台机器上两个实例抢同一个 user-data-dir 会直接
    /// 失败。也没必要去碰 `Browser::close()`——那会关掉整个会话。
    ///
    /// 这个 tab 故意**不**接管活动位：导出报告不该把会话正在浏览的那一页挤成后台。
    pub async fn scratch_page(&self) -> Result<Page, String> {
        let mut guard = self.lock_ready().await?;
        let inner = guard
            .as_mut()
            .ok_or_else(|| "Browser session is not running".to_string())?;
        inner
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| format!("Failed to open tab: {}", e))
    }

    /// 列出浏览器里现在的每一个页（`list_tabs` 动作，只读诊断）。
    ///
    /// 我们只能"知道有这些页"，不能切过去驱动不是我们自己创建的那一页（原因见
    /// `ActiveTab`）。要把某个页的内容拿回来，用 `navigate` 指到它的 url。
    pub async fn list_tabs(&self) -> Result<Value, String> {
        let mut guard = self.lock_ready().await?;
        let inner = guard
            .as_mut()
            .ok_or_else(|| "Browser session is not running".to_string())?;
        let live = self.live_pages_locked(inner).await?;
        let ours = inner.tab_id().cloned();
        Ok(json!({
            "success": true,
            "action": "list_tabs",
            "tab_count": live.len(),
            "note": "only the page marked active=true can be driven by the other actions",
            "tabs": tab_view(&live, ours.as_ref()),
        }))
    }

    /// CDP 眼里现在的 page 列表（调用方持有 inner 锁）。
    /// Caller MUST hold the inner lock.
    async fn live_pages_locked(&self, inner: &mut BrowserInner) -> Result<Vec<TargetInfo>, String> {
        Ok(page_targets(list_pages(&mut inner.browser).await?))
    }

    /// 自检报告：会用哪个浏览器、现在在不在跑、什么模式、profile 在哪。
    ///
    /// 不启动浏览器——它存在的意义正是“起不来的时候能问出为什么”。
    pub async fn status(&self) -> Value {
        let headless = self.headless.load(Ordering::Relaxed);
        let discovery = browser_launch::discover(&self.override_path());

        // 一次 fetch_targets 就够：tab 列表和活动页 URL 都在里面，不用为每个 tab 建句柄。
        let mut guard = self.inner.lock().await;
        let (running, tabs, current_url) = match guard.as_mut() {
            Some(inner) if self.is_alive() => match list_pages(&mut inner.browser).await {
                Ok(targets) => {
                    let live = page_targets(targets);
                    let active = inner.tab_id().cloned();
                    let url = live
                        .iter()
                        .find(|t| Some(&t.target_id) == active.as_ref())
                        .map(|t| t.url.clone())
                        .unwrap_or_default();
                    (true, tab_view(&live, active.as_ref()), url)
                }
                Err(e) => {
                    // 活着却问不到 tab —— 这正是"卡死但还没被判死"的窗口，要让 probe 说出来。
                    warn!("Browser CDP: probe could not list tabs: {}", e);
                    (true, Vec::new(), format!("unavailable: {}", e))
                }
            },
            _ => (false, Vec::new(), String::new()),
        };
        drop(guard);

        let (browser, browser_source, version) = match &discovery.chosen {
            Some(c) => (
                c.path.to_string_lossy().to_string(),
                c.source.label().to_string(),
                browser_launch::version_from_layout(&c.path),
            ),
            // 显式路径填错时的"没找到"不是没装浏览器，Tools 页要说的是这一件，
            // 否则运维会去查"为什么系统找不到 Edge"。
            None if discovery.override_missing.is_some() => {
                (String::new(), "configured path missing".to_string(), String::new())
            }
            None => (String::new(), "not found".to_string(), String::new()),
        };

        json!({
            "success": true,
            "action": "probe",
            "running": running,
            "current_url": current_url,
            "tab_count": tabs.len(),
            "tabs": tabs,
            "mode": if headless { "headless" } else { "visible" },
            "configured_path": self.override_path(),
            "browser": browser,
            "browser_source": browser_source,
            "version_on_disk": version,
            "profile_dir": self.profile_dir.to_string_lossy().to_string(),
            "profile_state": browser_launch::describe_profile(&self.profile_dir),
            "searched": discovery
                .tried
                .iter()
                .map(|c| c.describe())
                .collect::<Vec<_>>(),
        })
    }
}

/// The browser CDP tool — single tool with multiple actions.
pub struct BrowserCdpTool {
    session: Arc<BrowserSession>,
}

impl BrowserCdpTool {
    pub fn new(session: Arc<BrowserSession>) -> Self {
        Self { session }
    }
}

#[async_trait]
impl Tool for BrowserCdpTool {
    fn name(&self) -> &str { "browser_cdp" }

    fn description(&self) -> &str {
        "Browser automation via CDP (Chrome DevTools Protocol). \
         Runs hidden by default (no visible window); a visible window can be enabled in Settings. \
         Use this for: screenshots, web scraping, checking URLs, extracting page content. \
         It drives its own browser profile kept in the local application-data directory \
         (one per workspace), so it does not start with the cookies of an everyday browser; \
         a site signed into once in this profile stays signed in for later sessions.\n\
         A login that needs a password, 2FA or a QR scan must be done once in the \
         visible-window mode available in Settings; headless runs then inherit that state.\n\
         One page at a time: every page action runs on the single page this session owns. If the \
         site opens a tab of its own you will see it in 'list_tabs' (and in a raised 'tab_count'), \
         but it cannot be driven - take its url and 'navigate' to it instead.\n\
         Selectors are waited for (up to 8s), so 'Element not found' means the element really is \
         absent, not 'not rendered yet'. Long texts come back capped: continue with 'offset' (see \
         'next_offset'), or open 'full_text_path', which holds the whole page.\n\
         Actions:\n\
         - 'navigate': Go to a URL on our page. Provide 'url'. Reports 'loaded'.\n\
         - 'get_text': Get page text or element text. Optional 'selector' (CSS), 'offset', 'max_chars'.\n\
         - 'click': Click an element. Provide 'selector' (CSS). Reports 'navigated' and the new 'url'.\n\
         - 'type_text': Type into an element. Provide 'selector' and 'text'.\n\
         - 'screenshot': Take a screenshot; 'full_page' captures the whole scrollable page.\n\
         - 'get_url': Get our page's URL and title.\n\
         - 'get_html': Get page or element HTML, script/style stripped unless 'raw'. Optional 'offset'.\n\
         - 'execute_js': Run JavaScript on our page. Provide 'js'.\n\
         - 'find_element': Find element and return its attributes. Provide 'selector'.\n\
         - 'list_tabs': Read-only inventory of every page in the browser (url, title, which one is ours).\n\
         - 'probe': Report the detected browser executable, mode, page list and session state \
           without launching anything. Use it first when a launch fails.\n\
         - 'close': Close the browser session."
    }

    fn is_builtin(&self) -> bool { true }
    fn is_read_only(&self) -> bool { false }
    fn timeout_stage(&self) -> TimeoutStage { TimeoutStage::Long }
    fn category(&self) -> &str { "write" }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["navigate", "get_text", "click", "type_text", "screenshot",
                             "get_url", "get_html", "execute_js", "find_element",
                             "list_tabs", "probe", "close"],
                    "description": "Which browser action to perform"
                },
                "url": {
                    "type": "string",
                    "description": "URL to navigate to ('navigate' action)"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector (for 'click', 'type_text', 'get_text', 'get_html', 'find_element')"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type (for 'type_text' action)"
                },
                "js": {
                    "type": "string",
                    "description": "JavaScript code to execute (for 'execute_js' action)"
                },
                "path": {
                    "type": "string",
                    "description": "File name for the screenshot (optional; only the file-name part is used, and it lands in the run's output dir)"
                },
                "full_page": {
                    "type": "boolean",
                    "description": "'screenshot': capture the whole scrollable page instead of just the viewport"
                },
                "offset": {
                    "type": "integer",
                    "description": "'get_text' / 'get_html': start reading at this character offset (see next_offset)"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "'get_text' / 'get_html' / 'execute_js': cap the inline result (bounded by the context budget)"
                },
                "raw": {
                    "type": "boolean",
                    "description": "'get_html': return untouched HTML including script/style instead of the stripped version"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        let action = args["action"].as_str()
            .ok_or_else(|| "Missing 'action'".to_string())?;

        // Probe does not need (and must not trigger) a browser launch
        if action == "probe" {
            return Ok(self.session.status().await);
        }

        // Close does not need browser init
        if action == "close" {
            self.session.close().await.map_err(|e| -> crate::error::AgentError { e.into() })?;
            return Ok(json!({
                "success": true,
                "action": "close",
                "message": "Browser session closed"
            }));
        }


        // Execute with auto-recovery: if the action fails due to a dead browser,
        // clear state and recover with a freshly launched browser.
        let output_dir = ctx.output_dir();
        let max_text_len = ctx.inline_limit(15_000);
        let result = self.execute_action(action, &args, &output_dir, max_text_len).await;

        // 两类失败分开处理，先轻后重：
        // - 只是我们那一页没了（站点关页、执行上下文被销毁）：确认一下，页还在就原样
        //   重试；页没了就新开一页，此时页面状态已丢，只有 `navigate` 能接着跑。
        // - 整个浏览器没了（连接断）：清状态 + 重启，同样只有 `navigate` 能直接重试。
        let result = match result {
            Err(ref e) if is_target_gone(&e.to_string()) => {
                let why = e.to_string();
                warn!("Browser CDP: page-level failure during '{}': {}", action, why);
                match self.session.recover_page().await {
                    Ok(false) => self.execute_action(action, &args, &output_dir, max_text_len).await,
                    Ok(true) => {
                        self.retry_after_state_loss(action, &args, &output_dir, max_text_len, &why)
                            .await
                    }
                    Err(r) => Err(format!(
                        "The browser page died during '{}' and opening a replacement failed: {}",
                        action, r
                    )
                    .into()),
                }
            }
            other => other,
        };
        match result {
            Err(ref e) if Self::is_connection_lost(&e.to_string()) => {
                warn!("Browser CDP: connection lost during '{}', attempting auto-recovery", action);
                let why = e.to_string();
                self.session.clear_state().await;
                self.retry_after_state_loss(action, &args, &output_dir, max_text_len, &why).await
            }
            other => other,
        }
    }
}

impl BrowserCdpTool {
    /// 页面状态已经没了：只有 `navigate` 能重试，其余动作明确告诉调用方先导航。
    async fn retry_after_state_loss(
        &self,
        action: &str,
        args: &Value,
        output_dir: &str,
        max_text_len: usize,
        why: &str,
    ) -> AgentResult<Value> {
        if action == "navigate" {
            self.execute_action(action, args, output_dir, max_text_len).await
        } else {
            Err(format!(
                "The browser page was lost ({why}) and a fresh blank page was opened. The \'{action}\' \
                 action cannot be retried without page state: call 'navigate' with the URL first, \
                 then retry \'{action}\'."
            )
            .into())
        }
    }


    /// Check if an error message indicates the browser connection was lost.
    fn is_connection_lost(err: &str) -> bool {
        err.contains("receiver is gone")
            || err.contains("send failed")
            || err.contains("connection closed")
            || err.contains("broken pipe")
            || err.contains("Not connected")
    }

    /// Execute a single browser action (called by execute, may be retried).
    async fn execute_action(&self, action: &str, args: &Value, output_dir: &str, max_text_len: usize) -> AgentResult<Value> {
        // All page-level actions run on the session's current active tab.
        let page = self.session.get_or_init().await
            .map_err(|e| -> crate::error::AgentError { e.into() })?;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let max_chars = (args["max_chars"].as_u64().unwrap_or(0) as usize)
            .min(max_text_len)
            .max(1)
            .min(max_text_len);

        match action {
            "navigate" => {
                let url = args["url"].as_str()
                    .ok_or_else(|| "Missing 'url' for navigate".to_string())?;
                page.goto(chromiumoxide::cdp::browser_protocol::page::NavigateParams {
                    url: url.to_string(),
                    referrer: None,
                    transition_type: None,
                    frame_id: None,
                    referrer_policy: None,
                }).await
                    .map_err(|e| format!("Navigate failed: {}", e))?;
                // Wait for the page load event with a 30s cap — stalled pages must not
                // hang the tool until the global tool timeout.
                let mut loaded = true;
                match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    page.wait_for_navigation(),
                ).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => return Err(format!("Navigation wait failed: {}", e).into()),
                    Err(_) => {
                        loaded = false;
                        warn!("Browser CDP: navigation wait timed out after 30s, continuing");
                    }
                }
                let title = page.get_title().await
                    .map_err(|e| format!("Get title failed: {}", e))?
                    .unwrap_or_default();
                let tabs = self.session.tab_count().await;
                let mut out = json!({
                    "success": true,
                    "action": "navigate",
                    "url": url,
                    "title": title,
                    // 没等到底就返回时必须是 false：只 warn 进日志的话，结果里看起来
                    // 和"页面加载完成"一模一样，模型会当成已就绪去取内容。
                    "loaded": loaded
                });
                if tabs > 1 { out["tab_count"] = json!(tabs); }
                Ok(out)
            }

            "get_text" => {
                let text = if let Some(selector) = args["selector"].as_str() {
                    let elem = find_element_wait(&page, selector).await
                        .map_err(|e| format!("Element not found '{}': {}", selector, e))?;
                    elem.inner_text().await
                        .map_err(|e| format!("Get text failed: {}", e))?
                        .unwrap_or_default()
                } else {
                    let result = page.evaluate_expression("document.body.innerText")
                        .await
                        .map_err(|e| format!("Evaluate failed: {}", e))?;
                    result.value().and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_default()
                };
                let s = slice_text(&text, offset, max_chars, output_dir, &format!("page_text_{}.txt", stamp()));
                Ok(json!({
                    "success": true,
                    "action": "get_text",
                    "text": s.window,
                    "offset": s.offset,
                    "total_chars": s.total,
                    "truncated": s.truncated,
                    "next_offset": s.next_offset,
                    "full_text_path": s.path
                }))
            }

            "click" => {
                let selector = args["selector"].as_str()
                    .ok_or_else(|| "Missing 'selector' for click".to_string())?;
                let elem = find_element_wait(&page, selector).await
                    .map_err(|e| format!("Element not found '{}': {}", selector, e))?;
                let before = page.url().await.ok().flatten().unwrap_or_default();
                elem.click().await
                    .map_err(|e| format!("Click failed: {}", e))?;
                // 点击可能触发导航。`wait_for_navigation` 在页面已经加载完时会立刻返回
                // （handler 先查 frame.is_loaded()），所以这里只在真有导航在飞时等；
                // 等不等得到都不报错——下一动的 selector 轮询会接住慢页面。
                let _ = tokio::time::timeout(CLICK_NAV_WAIT, page.wait_for_navigation()).await;
                let after = page.url().await.ok().flatten().unwrap_or_default();
                let tabs = self.session.tab_count().await;
                let mut out = json!({
                    "success": true,
                    "action": "click",
                    "selector": selector,
                    "navigated": before != after,
                    "url": after
                });
                if tabs > 1 { out["tab_count"] = json!(tabs); }
                Ok(out)
            }

            "type_text" => {
                let selector = args["selector"].as_str()
                    .ok_or_else(|| "Missing 'selector' for type_text".to_string())?;
                let text = args["text"].as_str()
                    .ok_or_else(|| "Missing 'text' for type_text".to_string())?;
                let elem = find_element_wait(&page, selector).await
                    .map_err(|e| format!("Element not found '{}': {}", selector, e))?;
                elem.click().await
                    .map_err(|e| format!("Click (focus) failed: {}", e))?;
                elem.type_str(text).await
                    .map_err(|e| format!("Type failed: {}", e))?;
                Ok(json!({
                    "success": true,
                    "action": "type_text",
                    "selector": selector,
                    "typed": text
                }))
            }

            "screenshot" => {
                let filename = format!("screenshot_{}.png", stamp());
                // Always save into the run's output dir — if the caller provides 'path',
                // only use its file_name component (discard any directory portion).
                let file_name = if let Some(p) = args["path"].as_str() {
                    let pb = PathBuf::from(p);
                    pb.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or(filename)
                } else {
                    filename
                };
                let out = PathBuf::from(output_dir);
                let _ = std::fs::create_dir_all(&out);
                let path = out.join(&file_name);
                let params = CaptureScreenshotParams {
                    format: Some(CaptureScreenshotFormat::Png),
                    capture_beyond_viewport: args["full_page"].as_bool().filter(|b| *b).map(|_| true),
                    ..Default::default()
                };
                page.save_screenshot(params, &path)
                    .await
                    .map_err(|e| format!("Screenshot failed: {}", e))?;
                Ok(json!({
                    "success": true,
                    "action": "screenshot",
                    "url": display_url(&path, &self.session.workspace_dir),
                    "path": path.to_string_lossy()
                }))
            }

            "get_url" => {
                let info = self.session.active_tab_info().await;
                let url = page.url().await
                    .map_err(|e| format!("Get URL failed: {}", e))?
                    .filter(|u| !u.is_empty())
                    .or_else(|| info.as_ref().map(|(u, _)| u.clone()).filter(|u| !u.is_empty()))
                    .unwrap_or_default();
                let title = page.get_title().await
                    .ok()
                    .flatten()
                    .filter(|t| !t.is_empty())
                    .or_else(|| info.as_ref().map(|(_, t)| t.clone()).filter(|t| !t.is_empty()))
                    .unwrap_or_default();
                Ok(json!({
                    "success": true,
                    "action": "get_url",
                    "url": url,
                    "title": title
                }))
            }

            "get_html" => {
                let html = if let Some(selector) = args["selector"].as_str() {
                    let elem = find_element_wait(&page, selector).await
                        .map_err(|e| format!("Element not found '{}': {}", selector, e))?;
                    elem.inner_html().await
                        .map_err(|e| format!("Get HTML failed: {}", e))?
                        .unwrap_or_default()
                } else {
                    page.content().await
                        .map_err(|e| format!("Get content failed: {}", e))?
                };
                let stripped = !args["raw"].as_bool().unwrap_or(false);
                let html = if stripped { strip_html_noise(&html) } else { html };
                let s = slice_text(&html, offset, max_chars, output_dir, &format!("page_html_{}.html", stamp()));
                Ok(json!({
                    "success": true,
                    "action": "get_html",
                    "html": s.window,
                    "offset": s.offset,
                    "total_chars": s.total,
                    "truncated": s.truncated,
                    "next_offset": s.next_offset,
                    "full_text_path": s.path,
                    "stripped": stripped
                }))
            }

            "execute_js" => {
                let js = args["js"].as_str()
                    .ok_or_else(|| "Missing 'js' for execute_js".to_string())?;
                let result = page.evaluate_expression(js)
                    .await
                    .map_err(|e| format!("JS execution failed: {}", e))?;
                let value = result.value().cloned();
                let mut out = json!({ "success": true, "action": "execute_js" });
                match value {
                    Some(v) => {
                        // get_text / get_html 有上限而 execute_js 没有的话，等于给上下文
                        // 开了个后门：一句 `document.body.innerHTML` 就能把整页灌进来。
                        let text = v.to_string();
                        if text.chars().count() > max_chars {
                            let s = slice_text(&text, 0, max_chars, output_dir,
                                &format!("js_result_{}.json", stamp()));
                            out["result"] = json!(s.window);
                            out["truncated"] = json!(s.truncated);
                            out["total_chars"] = json!(s.total);
                            out["full_text_path"] = json!(s.path);
                        } else {
                            out["result"] = v;
                        }
                    }
                    None => {
                        out["result"] = json!(Value::Null);
                        // 取不到值 ≠ 结果就是 null：DOM 节点、函数在 returnByValue 下没有
                        // 可序列化值。只报 null 会被读成"页面上没有东西"。
                        let ty = format!("{:?}", result.object().r#type);
                        out["note"] = json!(format!(
                            "the JS returned a non-serializable {ty} — \
                             wrap the expression in JSON.stringify(...) to get data back"
                        ));
                    }
                }
                Ok(out)
            }

            "find_element" => {
                let selector = args["selector"].as_str()
                    .ok_or_else(|| "Missing 'selector' for find_element".to_string())?;
                let elem = find_element_wait(&page, selector).await
                    .map_err(|e| format!("Element not found '{}': {}", selector, e))?;
                let attrs = elem.attributes().await
                    .map_err(|e| format!("Get attributes failed: {}", e))?;
                let text = elem.inner_text().await
                    .map_err(|e| format!("Get text failed: {}", e))?
                    .unwrap_or_default();
                // attributes() returns flat vec: [name1, val1, name2, val2, ...]
                let mut attr_map = serde_json::Map::new();
                let mut iter = attrs.into_iter();
                while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
                    attr_map.insert(k, Value::String(v));
                }
                // 按字符截，不按字节：整页中文时字节切会正好落在半个 UTF-8 字符上直接 panic。
                let text_brief: String = text.chars().take(500).collect();
                Ok(json!({
                    "success": true,
                    "action": "find_element",
                    "selector": selector,
                    "attributes": attr_map,
                    "text": text_brief,
                    "text_total_chars": text.chars().count()
                }))
            }

            _ => Err(format!(
                "Unknown action '{}'. Valid: navigate, get_text, click, type_text, \
                 screenshot, get_url, get_html, execute_js, find_element, list_tabs, probe, close",
                action
            ).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用：profile 显式指到 workspace 里面的临时目录，绝不往
    /// `%LOCALAPPDATA%` 里种真实 profile（那是 `default_profile_dir` 的去处）。
    fn session_in(dir: &str, headless: bool) -> Arc<BrowserSession> {
        BrowserSession::with_profile_dir(
            dir.to_string(),
            Arc::new(AtomicBool::new(headless)),
            Arc::new(RwLock::new(String::new())),
            PathBuf::from(dir).join(".test_profile"),
        )
    }

    /// 登录态就住在 profile 里：close() 只负责让进程退出，绝不动目录。
    #[tokio::test]
    async fn closing_a_session_keeps_the_profile_dir() {
        let tmp = std::env::temp_dir().join(format!("foxir_prof_{}", std::process::id()));
        let s = session_in(tmp.to_str().unwrap(), true);
        std::fs::create_dir_all(&s.profile_dir).unwrap();
        let cookie = s.profile_dir.join("Cookies");
        std::fs::write(&cookie, b"keep me").unwrap();

        s.close().await.unwrap();

        assert!(cookie.exists(), "close() must never delete the persistent profile");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// profile 不能再住在 workspace 里：`/workspace/{*path}` 是免鉴权的静态文件路由，
    /// 而 profile 里装的是登录后的 cookie；case 目录本身还要打包、移交。
    /// 同时每个 workspace 必须各拿一份目录——Chromium 的单实例互斥按 user-data-dir 划分。
    #[test]
    fn each_workspace_gets_its_own_profile_outside_itself() {
        let base = std::env::temp_dir().join(format!("foxir_ws_{}", std::process::id()));
        let case_a = base.join("case-a");
        let case_b = base.join("case-b");
        std::fs::create_dir_all(&case_a).unwrap();
        std::fs::create_dir_all(&case_b).unwrap();

        let a = default_profile_dir(&case_a.to_string_lossy());
        let b = default_profile_dir(&case_b.to_string_lossy());
        assert!(!a.starts_with(&case_a), "profile must live outside the workspace: {a:?}");
        assert_ne!(a, b, "two cases must not share one user-data-dir");
        assert!(a.to_string_lossy().contains("browser_profiles"), "{a:?}");
        assert!(
            a.file_name().unwrap().to_string_lossy().starts_with("case-a-"),
            "the directory name must still say which case it belongs to: {a:?}"
        );
        assert_eq!(
            a,
            default_profile_dir(&case_a.to_string_lossy()),
            "the same workspace must resolve to the same profile across restarts"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// workspace 还不存在（刚建案子的第一步就会这样）时不能崩，路径仍要有确定性。
    #[test]
    fn a_missing_workspace_still_resolves_to_a_stable_profile() {
        let absent = std::env::temp_dir().join(format!("foxir_absent_{}", std::process::id()));
        let first = default_profile_dir(&absent.to_string_lossy());
        let second = default_profile_dir(&absent.to_string_lossy());
        assert_eq!(first, second);
        assert!(
            first.file_name().unwrap().to_string_lossy().starts_with("foxir_absent_"),
            "{first:?}"
        );
    }


    #[test]
    fn stamp_carries_subsecond_precision() {
        let s = stamp();
        // YYYYMMDD_HHMMSS_ffffff：微秒后缀才是"同一秒两张截图不互相覆盖"的原因
        assert_eq!(s.len(), 22, "{s}");
        assert!(s[16..].chars().all(|c| c.is_ascii_digit()), "{s}");
    }

    /// 只有由真实落盘路径反推的链接才不会 404：Expert/managed run 的 output_dir 并不
    /// 等于 `workspace/output`。
    #[test]
    fn screenshot_url_is_derived_from_where_the_file_actually_went() {
        let ws = std::env::temp_dir().join(format!("foxir_url_{}", std::process::id()));
        let round = ws.join("managed").join("contract1").join("round_03");
        std::fs::create_dir_all(&round).unwrap();
        let shot = round.join("screenshot_x.png");
        std::fs::write(&shot, b"png").unwrap();

        let url = display_url(&shot, &ws.to_string_lossy());
        let slashes = url.replace('\\', "/");
        assert!(url.starts_with("/workspace/"), "{url}");
        assert!(slashes.ends_with("managed/contract1/round_03/screenshot_x.png"), "{url}");
        assert!(!slashes.contains("/output/"), "不能再假装落在 workspace/output: {url}");

        // workspace 之外给不出可服务的相对链接，就原样返回绝对路径
        let outside = std::env::temp_dir().join(format!("foxir_outside_{}.png", std::process::id()));
        std::fs::write(&outside, b"png").unwrap();
        let url2 = display_url(&outside, &ws.to_string_lossy());
        assert!(!url2.starts_with("/workspace/"), "{url2}");

        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn html_stripping_keeps_the_text_and_drops_the_noise() {
        let html = "<html><head><style>.a{color:red}</style><script>var x=1;</script></head>\
            <body><!-- secret comment --><h1>Report</h1><p>1 host compromised</p></body></html>";
        let out = strip_html_noise(html);
        assert!(out.contains("Report") && out.contains("host compromised"), "{out}");
        assert!(!out.contains("color:red") && !out.contains("var x"), "{out}");
        assert!(!out.contains("secret comment"), "{out}");
    }

    /// 截断必须按字符，且超阈值时把全文交出去——否则调用方只能整页重取。
    #[test]
    fn long_text_spills_the_full_page_and_supports_reading_on() {
        let dir = std::env::temp_dir().join(format!("foxir_slice_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = "汉".repeat(50); // 50 字符 = 150 字节：字节切会正好切在半个字符上
        let s = slice_text(&big, 0, 10, &dir.to_string_lossy(), "full.txt");
        assert_eq!(s.window.chars().count(), 10);
        assert_eq!(s.total, 50);
        assert!(s.truncated);
        assert_eq!(s.next_offset, 10);
        let spilled = s.path.expect("truncated output must be written out");
        assert_eq!(std::fs::read_to_string(&spilled).unwrap().chars().count(), 50);

        let tail = slice_text(&big, 40, 10, &dir.to_string_lossy(), "full2.txt");
        assert!(!tail.truncated);
        assert!(tail.path.is_none());
        assert_eq!(tail.window.chars().count(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无头开关是共享原子：会话在 launch 时现读，Settings 切换不需要重启。
    #[test]
    fn headless_toggle_is_observed_through_the_shared_atomic() {
        let flag = Arc::new(AtomicBool::new(true));
        let s = BrowserSession::new(
            "w".to_string(),
            flag.clone(),
            Arc::new(RwLock::new(String::new())),
        );
        assert!(s.headless.load(Ordering::Relaxed));
        flag.store(false, Ordering::Relaxed);
        assert!(!s.headless.load(Ordering::Relaxed), "session must see the flipped switch");
    }

    /// 空路径 = 自动探测；填了就原样交给 discover。
    #[test]
    fn explicit_path_is_surfaced_to_discovery() {
        let exe = Arc::new(RwLock::new(String::new()));
        let s = BrowserSession::new(
            "w".to_string(),
            Arc::new(AtomicBool::new(true)),
            exe.clone(),
        );
        assert_eq!(s.override_path(), "");
        *exe.write().unwrap() = r"C:\Edge\msedge.exe".to_string();
        assert_eq!(s.override_path(), r"C:\Edge\msedge.exe");
    }

    /// probe 绝不能把浏览器叫醒：它存在的意义正是在“起不来”的机器上回答为什么。
    #[tokio::test]
    async fn status_does_not_launch_a_browser() {
        let s = session_in("unused", true);
        let v = s.status().await;
        assert_eq!(v["running"], json!(false));
        assert!(!s.is_alive(), "status() must not change the session state");
        assert!(v.get("searched").is_some(), "must always report where it looked");
        assert_eq!(
            v["tab_count"],
            json!(0),
            "a session that never launched has no tabs to list: {v}"
        );
    }

    /// 真机验证（默认 `#[ignore]`，普通测试批次不跑）：起 → 关 → 立刻再起。
    ///
    /// 修的就是第二次：原来 close() 只发一条 CDP 关闭请求就返回，进程还在持有
    /// user-data-dir，单实例语义让新实例交出命令后直接退出，看起来就是“启动失败”。
    /// 跑法：`cargo test --lib browser_cdp -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "launches a real browser; run it on a machine with Edge/Chrome installed"]
    async fn real_browser_launch_close_and_relaunch() {
        let tmp = std::env::temp_dir().join(format!("foxir_cdp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let s = session_in(tmp.to_str().unwrap(), true);

        s.get_or_init().await.expect("first launch must succeed");
        let first = s.status().await;
        println!(
            "round 1: running={} browser={} [{}] v{}",
            first["running"], first["browser"], first["browser_source"], first["version_on_disk"]
        );
        assert_eq!(first["running"], json!(true));

        s.close().await.expect("close must not fail");
        assert!(!s.is_alive(), "session must be marked dead after close");

        let again = s.get_or_init().await.expect("relaunch right after close must succeed");
        println!("round 2 relaunched, url={:?}", again.url().await.ok().flatten());
        s.close().await.unwrap();

        assert!(
            s.profile_dir.exists(),
            "profile must survive so a login is not lost between sessions"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// P1 的有头路径：不只看能不能起，而是看“窗口真的在”——无头下没人能输密码/2FA/扫码，
    /// 所以登录一次必须走这个分支。跑起来会真弹一个 Edge/Chrome 窗口，几秒后自动关闭。
    #[tokio::test]
    #[ignore = "opens a real browser window on screen"]
    async fn real_browser_visible_window_round_trip() {
        let tmp = std::env::temp_dir().join(format!("foxir_cdp_vis_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let s = session_in(tmp.to_str().unwrap(), false);

        s.get_or_init().await.expect("headed launch must succeed");
        let st = s.status().await;
        println!("visible mode: {}", st["mode"]);
        assert_eq!(st["mode"], json!("visible"));
        assert_eq!(st["running"], json!(true));

        s.close().await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 真机验证（默认 `#[ignore]`）：站点自己开一页之后，我们那一页仍然可用、`list_tabs`
    /// 能看见那个新页；我们脚下这页被关掉时能另开一页继续干活。
    /// 跑法：`cargo test --lib browser_cdp -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "launches a real browser"]
    async fn real_browser_survives_a_page_opened_by_the_site() {
        let tmp = std::env::temp_dir().join(format!("foxir_cdp_site_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let s = session_in(tmp.to_str().unwrap(), true);

        let page = s.get_or_init().await.expect("first launch must succeed");
        page.goto("https://example.com/").await.expect("navigate to example.com");
        let ours = s.active_tab_info().await.expect("our page is listed");
        assert!(ours.0.contains("example.com"), "our page url: {ours:?}");

        // 站点自己开一页：只要求"看得见多出来的这一页"。刚弹出的页在 CDP 列表里 URL/标题
        // 常常还是空的（导航没提交），所以不能拿 URL 当识别依据。
        page.evaluate_expression("window.open('https://example.net/'); 'ok'")
            .await
            .expect("window.open should not throw");
        let listed = s.list_tabs().await.expect("list_tabs");
        let count = listed["tab_count"].as_u64().unwrap_or(0);
        assert!(count >= 2, "the page the site opened must show up: {listed}");
        assert_eq!(
            listed["tabs"]
                .as_array()
                .map(|a| a.iter().filter(|t| t["active"] == json!(true)).count())
                .unwrap_or(0),
            1,
            "exactly one page stays ours to drive: {listed}"
        );
        assert!(page.get_title().await.is_ok(), "our own page must still be drivable");

        // 我们脚下这页没了：recover_page 应当另开一页，而不是让后续动作全线报错
        println!("closing our own page: {:?}", page.close().await);
        s.recover_page().await.expect("recover_page must not fail");
        let again = s.get_or_init().await.expect("a page must be available again");
        assert!(again.get_title().await.is_ok(), "the replacement page must be drivable");
        assert!(s.list_tabs().await.is_ok(), "listing still works");

        s.close().await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
