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
//! One page per caller: each agent (main session or sub-agent) gets its own page inside
//! one shared browser process, so nobody can navigate somebody else's page. Pages opened
//! by a site are counted but not drivable (chromiumoxide 0.9 gives no reliable handle for
//! a target we did not create) — bring the URL back with `navigate` instead.

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams,
};
use chromiumoxide::cdp::browser_protocol::target::{GetTargetsParams, TargetId, TargetInfo};
use chromiumoxide::page::Page;
use chromiumoxide::{Element, Handler};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, MutexGuard};
use tracing::{info, warn};

use super::{TimeoutStage, Tool};
use super::browser_launch;
use crate::context::ToolContext;
use crate::error::AgentResult;

/// 关闭会话时等待浏览器进程真正退出的宽限，超时才强杀。
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// 收养交接浏览器时，一次 CDP 探活的等待上限。
const ADOPT_WAIT: Duration = Duration::from_secs(4);

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

/// 截图落在哪：**永远**是这一轮 run 的 output 目录，调用方给的 `path` 只取文件名。
///
/// 证据去哪儿由 `ToolContext::output_dir()` 说了算，不由模型传参说了算：传
/// `C:\workspace\output\x.png` 这种绝对路径也只留 `x.png`。否则一次跑完，截图会散在
/// 案件目录之外——既进不了证据包，`/workspace/*` 也服务不到它。
fn screenshot_target(output_dir: &str, requested: Option<&str>) -> PathBuf {
    let fallback = format!("screenshot_{}.png", stamp());
    let name = requested
        .map(PathBuf::from)
        .and_then(|pb| pb.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or(fallback);
    let dir = PathBuf::from(output_dir);
    let _ = std::fs::create_dir_all(&dir);
    dir.join(name)
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

/// 一个 agent 自己创建、并且握有可用句柄的那一页。
///
/// 为什么是"自己创建的这一页"而不是"任意一页"：完整的多 tab（`new_tab`/`switch_tab`/
/// `close_tab` + 按 target id 现取句柄）实现过，被两条真机事实否掉——`Target.getTargets`
/// 的返回顺序不稳定（最后开的会排在第一个），以及 `Browser::get_page(target_id)` 现取的
/// 句柄发命令会 `channel disconnected`。所以每个调用者只驱动自己创建的那一页；站点自己
/// 开的页只呈现"有这回事"，要把内容拿回来用 `navigate`——那条路每一环都验证过。
struct OwnedPage {
    id: TargetId,
    page: Page,
}

/// Inner state holding the browser connection.
struct BrowserInner {
    browser: Browser,
    /// 按调用者（`invocation_id`）分片：一个 agent 一页。浏览器进程和 profile 只有一个，
    /// 所以登录态天然共享；页与页互不干扰，也就不需要一把全局锁去防并发抢同一页。
    pages: HashMap<String, OwnedPage>,
    /// 最后一个 agent 交回页面的时刻，用于把空闲的浏览器整个收掉。
    idle_since: Option<Instant>,
    /// 这一次启动用的是无头还是可见窗口。Settings 里那个开关只有在这个值和当前设置
    /// 不一致、浏览器被重启之后才会生效（见 `lock_ready`）。
    launched_headless: bool,
}

impl BrowserInner {
    fn page_for(&self, key: &str) -> Option<Page> {
        self.pages.get(key).map(|p| p.page.clone())
    }

    fn page_id(&self, key: &str) -> Option<TargetId> {
        self.pages.get(key).map(|p| p.id.clone())
    }

    /// 把一页交给 `key`；同一个 key 再要就是同一页（复用，不重开）。
    fn claim(&mut self, key: &str, page: Page) -> Page {
        self.pages.insert(
            key.to_string(),
            OwnedPage { id: page.target_id().clone(), page: page.clone() },
        );
        self.idle_since = None;
        page
    }

    /// 一个 agent 交回自己那一页；空了才开始计空闲。
    fn take_page(&mut self, key: &str) -> Option<OwnedPage> {
        let page = self.pages.remove(key);
        if self.pages.is_empty() {
            self.idle_since = Some(Instant::now());
        }
        page
    }
}

/// 空到多久就把浏览器整个收掉。太短会让连续几轮取证反复付启动成本（还要等进程退出握手），
/// 太长就是白占一个带登录态的进程。
const IDLE_REAP: Duration = Duration::from_secs(10 * 60);

/// 纯函数：这一轮该不该把浏览器收掉。
fn should_reap(pages_empty: bool, idle_since: Option<Instant>, now: Instant, ttl: Duration) -> bool {
    pages_empty
        && match idle_since {
            Some(since) => now.saturating_duration_since(since) >= ttl,
            None => false,
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

/// `<profile>/DevToolsActivePort` 的首行 = 这台浏览器实际监听的调试端口。
/// 我们传的是 `--remote-debugging-port=0`（chromiumoxide 的缺省），端口由浏览器自己挑，
/// 这个文件是唯一能把端口找回来的线索。
fn parse_devtools_active_port(txt: &str) -> Option<u16> {
    txt.lines().next().and_then(|l| l.trim().parse().ok())
}

/// 收养"交接之后真正活下来的那台浏览器"。
///
/// 真机证据（Edge 153 + `--no-startup-window`，运行时日志 10:03:42 / 10:03:55）：
/// `msedge.exe` 的启动进程把请求移交给真正干活的浏览器进程后**自己以 0 退出**，而
/// chromiumoxide 的 pipe 传输只看见 "Browser process exited with status 0 before
/// websocket URL could be resolved"。接手的那台浏览器是活的、也在监听 CDP，并且会长期
/// 占住 user-data-dir —— 于是从这一次之后，**每一次**启动都以同样的方式失败，看起来就
/// 是"内置浏览器起不来"。
///
/// 收养必须验明正身：这个文件在浏览器退出后不会被删，端口号也可能被别的浏览器拿去用。
/// 接上用户正在浏览的窗口、又在收尾时把它关掉，对取证工具是不可接受的代价。所以要求
/// 我们这次是无头启动、且端点自报是同一家族的无头浏览器（UA 里有 Headless）。
/// 可见窗口模式下不收养——那种情况下用户看得见、也关得掉。
pub(crate) async fn adopt_handoff_browser(
    profile_dir: &Path,
    exe: &Path,
    headless: bool,
) -> Option<(Browser, Handler)> {
    if !headless {
        return None;
    }
    let port = std::fs::read_to_string(profile_dir.join("DevToolsActivePort"))
        .ok()
        .and_then(|t| parse_devtools_active_port(&t))?;
    let url = format!("http://127.0.0.1:{}/json/version", port);
    let client = match reqwest::Client::builder().timeout(ADOPT_WAIT).build() {
        Ok(c) => c,
        Err(_) => return None,
    };
    let info: Value = client.get(&url).send().await.ok()?.json().await.ok()?;

    let product = info.get("Browser").and_then(|v| v.as_str()).unwrap_or("");
    let ua = info.get("User-Agent").and_then(|v| v.as_str()).unwrap_or("");
    let exe_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let family_ok = if exe_name.contains("edge") {
        product.starts_with("Edg/")
    } else if exe_name.contains("chrome") {
        product.starts_with("Chrome/")
    } else {
        false
    };
    if !family_ok || !ua.contains("Headless") {
        if !product.is_empty() {
            warn!(
                "Browser CDP: port {} answered but is not our headless browser ({}), not adopting",
                port, product
            );
        }
        return None;
    }

    let ws = info.get("webSocketDebuggerUrl").and_then(|v| v.as_str())?;
    match tokio::time::timeout(ADOPT_WAIT, Browser::connect(ws)).await {
        Ok(Ok((browser, handler))) => {
            info!(
                "Browser CDP: launcher handed off (exit 0); adopted the live browser on port {} ({})",
                port, product
            );
            Some((browser, handler))
        }
        Ok(Err(e)) => {
            warn!("Browser CDP: adopting {} failed: {}", ws, e);
            None
        }
        Err(_) => {
            warn!("Browser CDP: adopting {} timed out after {}s", ws, ADOPT_WAIT.as_secs());
            None
        }
    }
}

/// Shared browser session with lazy initialization and auto-recovery.
pub struct BrowserSession {
    inner: Mutex<Option<BrowserInner>>,
    workspace_dir: String,
    /// 持久浏览器 profile：跨启动保留，登录态就住在这里，绝不删除。
    /// 位置由 `default_profile_dir` 决定（就在 case 目录里），测试可用
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

/// 浏览器 profile 的位置：**就在 case 目录里** —— `<workspace>/.browser_profile`。
///
/// 这是有意的取舍：FoxIR 是单机本机工具，不把它当网络暴露面来设计，于是换来三个实在好处：
/// 案件目录自包含（拷走案子就带着登录态）、删案即清、出问题时运维直接在 case 目录里能看到它。
/// 每个 workspace 一份也必须成立——Chromium 的单实例互斥按 user-data-dir 划分，
/// 两个案子共用一份 profile 会直接起不来。
pub fn default_profile_dir(workspace_dir: &str) -> PathBuf {
    PathBuf::from(workspace_dir).join(".browser_profile")
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

    /// 指定 profile 目录的构造器：测试要能把 profile 圈在自己的临时目录里。
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

    /// 保证有一个活着的浏览器（且模式与 Settings 一致），并返回持有它的锁守卫。
    ///
    /// 整段（检查 + 重启 + 启动）都持锁，所以并发调用不会起出两个实例。
    async fn lock_ready(&self, key: &str) -> Result<MutexGuard<'_, Option<BrowserInner>>, String> {
        let mut guard = self.inner.lock().await;
        let want_headless = self.headless.load(Ordering::Relaxed);
        // 开关对不上就必须重启：可见窗口模式是"登录一次、之后长期复用"这条路的唯一入口，
        // 而浏览器现在会在 run 之间存活（还可能是从上一台收养来的）。只在启动时读一次开关
        // 的话，用户勾掉无头之后看到的仍然只有截图，登录这件事就永远做不成。
        let stale_mode = guard
            .as_ref()
            .map(|i| i.launched_headless != want_headless)
            .unwrap_or(false);
        if stale_mode || !self.is_alive() || guard.is_none() {
            if stale_mode {
                let was = guard.as_ref().map(|i| i.launched_headless).unwrap_or(true);
                info!(
                    "Browser CDP: mode setting changed ({} -> {}), restarting the browser",
                    if was { "headless" } else { "visible window" },
                    if want_headless { "headless" } else { "visible window" }
                );
            }
            // Slow path. `is_alive()==false` only means the handler stream ended — the
            // child can still be draining and holding the profile directory, so this is
            // the one place a plain `guard.take()` (drop without waiting) reintroduces
            // the failure this whole handshake exists to kill: the re-launch sees the
            // dir still owned, hands the request to the dying instance and exits 0.
            // Lock stays held across the handshake, so nobody launches in between.
            if let Some(inner) = guard.take() {
                shutdown_browser(inner.browser).await;
            }
            self.launch_locked(&mut guard, key).await?;
        }
        Ok(guard)
    }

    /// `key`（一个 agent）那一页的句柄。快路径不碰 CDP；这个 key 第一次来要页时才新开一页。
    async fn get_or_init(&self, key: &str) -> Result<Page, String> {
        let mut guard = self.lock_ready(key).await?;
        let inner = guard
            .as_mut()
            .ok_or_else(|| "Browser session is not running".to_string())?;
        if let Some(page) = inner.page_for(key) {
            return Ok(page);
        }
        let page = inner
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| format!("Browser is running but no page could be opened: {}", e))?;
        Ok(inner.claim(key, page))
    }

    /// 一个 agent 结束（或显式 `close`）：只交回自己那一页，绝不动别人的，也不关浏览器。
    pub async fn release(&self, key: &str) {
        let mut guard = self.inner.lock().await;
        let Some(inner) = guard.as_mut() else { return };
        if let Some(owned) = inner.take_page(key) {
            // 这一页是我们自己创建的，句柄也是自己拿的那个，关它是安全的路径；
            // 不可靠的是"按 target id 现取句柄再驱动"（见 `OwnedPage`）。
            let _ = owned.page.close().await;
        }
    }

    /// 这一页被站点关掉时，给同一个 key 另开一页。只新开、不接手别人的页。
    /// Caller MUST hold the inner lock.
    async fn replace_page(&self, inner: &mut BrowserInner, key: &str) -> Result<Page, String> {
        let page = inner
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| format!("Browser is running but no page could be opened: {}", e))?;
        Ok(inner.claim(key, page))
    }

    /// `key` 那一页在 CDP 侧的 (url, title)。
    ///
    /// 为什么要这份兜底：`Page::url()` / `get_title()` 读的是 handler 里的 frame 状态，
    /// 可能比 CDP 的说法慢、甚至为空（真机实测过两次）。以 CDP 的说法为准，句柄自己的
    /// 说法为辅。
    async fn page_info(&self, key: &str) -> Option<(String, String)> {
        let mut guard = self.inner.lock().await;
        let inner = guard.as_mut()?;
        let id = inner.page_id(key)?;
        let live = self.live_pages_locked(inner).await.ok()?;
        live.iter()
            .find(|t| t.target_id == id)
            .map(|t| (t.url.clone(), t.title.clone()))
    }

    /// 失败恢复用：确认这个 key 那一页还在不在，不在就另开一页。返回是否换过一页
    /// （换过就意味着页面状态没了，只有 `navigate` 能直接重试）。
    async fn recover_page(&self, key: &str) -> Result<bool, String> {
        let mut guard = self.inner.lock().await;
        let inner = match guard.as_mut() {
            Some(inner) => inner,
            None => return Ok(false),
        };
        if let Some(id) = inner.page_id(key) {
            let live = self.live_pages_locked(inner).await?;
            if live.iter().any(|t| t.target_id == id) {
                return Ok(false);
            }
        }
        warn!("Browser CDP: our page is gone, opening a fresh one");
        self.replace_page(inner, key).await?;
        Ok(true)
    }

    /// 浏览器里现在有几个页 + 我们自己握有几页（只读，不外泄别人的页 URL）。
    async fn page_counts(&self) -> (usize, usize) {
        let mut guard = self.inner.lock().await;
        match guard.as_mut() {
            Some(inner) => {
                let owned = inner.pages.len();
                (
                    self.live_pages_locked(inner).await.map(|l| l.len()).unwrap_or(0),
                    owned,
                )
            }
            None => (0, 0),
        }
    }

    /// 没人用够久就把浏览器整个收掉。返回是否真的收了。
    async fn reap_if_idle(&self) -> bool {
        let idle = {
            let guard = self.inner.lock().await;
            match guard.as_ref() {
                Some(inner) => should_reap(
                    inner.pages.is_empty(),
                    inner.idle_since,
                    Instant::now(),
                    IDLE_REAP,
                ),
                None => false,
            }
        };
        if !idle {
            return false;
        }
        info!(
            "Browser CDP: no page held for {}s, shutting the browser down",
            IDLE_REAP.as_secs()
        );
        let _ = self.close().await;
        true
    }

    /// 后台巡检空闲浏览器。由 `main` 在启动时拉起一次；agent 结束只交回自己那一页，
    /// 浏览器本体留给这里统一收，省掉"下一轮又从头启动 + 等进程退出握手"的反复开销。
    pub fn spawn_idle_reaper(self: &Arc<Self>) {
        let session = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(IDLE_REAP / 4).await;
                session.reap_if_idle().await;
            }
        });
    }

    /// Launch a fresh browser instance. Caller MUST hold the inner lock.
    ///
    /// 可执行文件由 `browser_launch::discover` 自己探测（Settings 显式路径 → 环境变量
    /// → PATH → 注册表 → 常见安装目录），不依赖 chromiumoxide 的内置检测；任何一步
    /// 失败都返回带路径/来源/版本/模式/profile 状态的诊断文本。
    async fn launch_locked(
        &self,
        guard: &mut MutexGuard<'_, Option<BrowserInner>>,
        key: &str,
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
                // Edge/Chrome 的启动进程可能在把活儿交给真正的浏览器进程后就以 0 退出了
                // （pipe 传输因此拿不到端点），而那台浏览器活得好好的、还占着 profile。
                // 先把它接回来，接不回来才算真的启动失败。
                let raw = format!("{}", e);
                match adopt_handoff_browser(&self.profile_dir, &chosen.path, headless).await {
                    Some(v) => v,
                    None => {
                        return Err(self.launch_failed(
                            diag_ctx,
                            &raw,
                            started.elapsed().as_secs(),
                        ));
                    }
                }
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
        info!("Browser CDP: browser launched successfully (gen {}, 1 page)", gen);

        // 启动时探出的那一页直接归触发这次启动的 key —— 不留"公共的孤儿页"，
        // 因为共享句柄正是这轮重构要消灭的东西。
        let mut inner = BrowserInner {
            browser,
            pages: HashMap::new(),
            idle_since: None,
            launched_headless: headless,
        };
        let _ = id;
        inner.claim(key, page);
        **guard = Some(inner);

        Ok(())
    }

    /// 关掉整个会话。等进程真退出后才返回（握手过程见 `shutdown_browser`）。
    ///
    /// 这是"整个浏览器"级别的关闭，只给 Tools 页开关、空闲回收和进程退出用；
    /// agent 结束或模型调 `close` 走的是 `release`（只交回自己那一页）。
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

    /// 报告导出用：在**同一个**浏览器实例上取调用者那一页。
    ///
    /// 不要为了导出另起一个浏览器：同一台机器上两个实例抢同一个 user-data-dir 会直接
    /// 失败。用完请调 `release_scratch`，也不要关整个会话。
    pub async fn scratch_page(&self, key: &str) -> Result<Page, String> {
        self.get_or_init(key).await
    }

    /// 导出结束，交回那一页。
    pub async fn release_scratch(&self, key: &str) {
        self.release(key).await;
    }

    /// 列出"我这一页 + 浏览器里还有几页"（`list_tabs` 动作，只读）。
    ///
    /// 不列别人的页 URL：并发取证时 A 能从列表里读到 B 正在看哪个页面，属于我们不该
    /// 提供的旁路。要驱动的一定是自己创建的那一页，所以只报自己的 (url, title)。
    pub async fn list_tabs(&self, key: &str) -> Result<Value, String> {
        let mut guard = self.lock_ready(key).await?;
        let inner = guard
            .as_mut()
            .ok_or_else(|| "Browser session is not running".to_string())?;
        let live = self.live_pages_locked(inner).await?;
        let mine = inner.page_id(key);
        let my_page = live
            .iter()
            .find(|t| Some(&t.target_id) == mine.as_ref())
            .map(|t| json!({ "url": t.url, "title": t.title }));
        Ok(json!({
            "success": true,
            "action": "list_tabs",
            "page": my_page,
            "pages_in_browser": live.len(),
            "pages_we_own": inner.pages.len(),
            "note": "the other pages belong to other agents or to the site; drive only your own, \
                      use navigate to bring a url to your page",
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
    pub async fn status(&self, key: Option<&str>) -> Value {
        let headless = self.headless.load(Ordering::Relaxed);
        let discovery = browser_launch::discover(&self.override_path());

        // 一次普通命令就能拿到全部页：不建句柄、也不列别人的 URL。
        let mut guard = self.inner.lock().await;
        let live_mode = guard.as_ref().filter(|_| self.is_alive()).map(|i| i.launched_headless);
        let (running, pages_in_browser, pages_we_own, my_page) = match guard.as_mut() {
            Some(inner) if self.is_alive() => {
                let owned = inner.pages.len();
                let mine = key.and_then(|k| inner.page_id(k));
                match list_pages(&mut inner.browser).await {
                    Ok(targets) => {
                        let live = page_targets(targets);
                        let my_page = live
                            .iter()
                            .find(|t| Some(&t.target_id) == mine.as_ref())
                            .map(|t| json!({ "url": t.url, "title": t.title }));
                        (true, live.len(), owned, my_page)
                    }
                    Err(e) => {
                        // 活着却问不到页——这正是"卡死但还没被判死"的窗口，要说出来。
                        warn!("Browser CDP: probe could not list pages: {}", e);
                        (true, 0, owned, None)
                    }
                }
            }
            _ => (false, 0, 0, None),
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
            "pages_in_browser": pages_in_browser,
            "pages_we_own": pages_we_own,
            "page": my_page,
            "mode": if headless { "headless" } else { "visible" },
            // 开关刚改过、活着的还是上一台模式时说清楚：下一次真正干活会自动重启。
            // 不说的话，用户勾掉无头却仍然只拿到截图，会以为这个开关根本没生效。
            "restart_pending": live_mode.map(|was| was != headless).unwrap_or(false),
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
         It drives its own browser profile inside the case directory (one per workspace), so it \
         does not start with the cookies of an everyday browser; \
         a site signed into once in this profile stays signed in for later sessions.\n\
         A login that needs a password, 2FA or a QR scan must be done once in the \
         visible-window mode available in Settings; headless runs then inherit that state.\n\
         One page per caller: this tool keeps its own page for each agent (main session or \
         sub-agent) inside one shared browser, so concurrent agents never navigate each other's \
         page while the signed-in profile stays shared. 'list_tabs' shows your page plus how many \
         pages exist - other agents' urls are deliberately not listed. A page the site opens is \
         counted but cannot be driven; take its url and 'navigate' instead. 'close' releases only \
         your page; a browser nobody holds gets shut down on its own.\n\
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
         - 'list_tabs': Your page (url, title) plus how many pages exist and how many we own.\n\
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
        // 状态按调用者分片：一个 agent（父会话或某个子代理）拥有自己那一页，
        // 谁结束都不影响别人。
        let key = ctx.base.base.invocation_id.clone();

        // Probe does not need (and must not trigger) a browser launch
        if action == "probe" {
            return Ok(self.session.status(Some(&key)).await);
        }

        if action == "list_tabs" {
            return self
                .session
                .list_tabs(&key)
                .await
                .map_err(|e| -> crate::error::AgentError { e.into() });
        }

        // `close` 交回的是**自己那一页**，不是整个浏览器：并发下别的 agent、
        // 以及模型下一轮还要用同一个浏览器。浏览器本体由 Tools 开关、空闲回收、
        // 进程退出来收。
        if action == "close" {
            self.session.release(&key).await;
            return Ok(json!({
                "success": true,
                "action": "close",
                "message": "Released this agent's page; the browser itself stays for other                             sessions and is shut down when it has been idle"
            }));
        }

        // Execute with auto-recovery: if the action fails due to a dead browser,
        // clear state and recover with a freshly launched browser.
        let output_dir = ctx.output_dir();
        let max_text_len = ctx.inline_limit(15_000);
        let result = self.execute_action(action, &args, &output_dir, max_text_len, &key).await;

        // 两类失败分开处理，先轻后重：
        // - 只是我们那一页没了（站点关页、执行上下文被销毁）：确认一下，页还在就原样
        //   重试；页没了就新开一页，此时页面状态已丢，只有 `navigate` 能接着跑。
        // - 整个浏览器没了（连接断）：清状态 + 重启，同样只有 `navigate` 能直接重试。
        let result = match result {
            Err(ref e) if is_target_gone(&e.to_string()) => {
                let why = e.to_string();
                warn!("Browser CDP: page-level failure during '{}': {}", action, why);
                match self.session.recover_page(&key).await {
                    Ok(false) => self.execute_action(action, &args, &output_dir, max_text_len, &key).await,
                    Ok(true) => {
                        self.retry_after_state_loss(action, &args, &output_dir, max_text_len, &why, &key)
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
                self.retry_after_state_loss(action, &args, &output_dir, max_text_len, &why, &key).await
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
        key: &str,
    ) -> AgentResult<Value> {
        if action == "navigate" {
            self.execute_action(action, args, output_dir, max_text_len, key).await
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
    async fn execute_action(
        &self,
        action: &str,
        args: &Value,
        output_dir: &str,
        max_text_len: usize,
        key: &str,
    ) -> AgentResult<Value> {
        // All page-level actions run on the session's current active tab.
        let page = self.session.get_or_init(key).await
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
                let (tabs, _) = self.session.page_counts().await;
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
                let (tabs, _) = self.session.page_counts().await;
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
                let path = screenshot_target(output_dir, args["path"].as_str());
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
                let info = self.session.page_info(key).await;
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

    /// 测试专用：profile 指到被测 workspace 下的临时目录，不碰真实 profile。
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

    /// `parse_devtools_active_port` 的形状：首行端口，第二行 ws 路径，可能带 \r\n。
    #[test]
    fn devtools_active_port_is_parsed_from_the_first_line() {
        assert_eq!(parse_devtools_active_port("12129\n/devtools/browser/fdc592aa"), Some(12129));
        assert_eq!(parse_devtools_active_port("9333\r\n"), Some(9333));
        assert_eq!(parse_devtools_active_port(""), None);
        assert_eq!(parse_devtools_active_port("not-a-port\n/x"), None);
        // 端口必须能塞进 u16：>65535 的垃圾不该被当成端点
        assert_eq!(parse_devtools_active_port("70000\n/x"), None);
    }

    /// 真机回归：profile 被**另一个还活着的浏览器**占着时（现场就是 Edge 的启动进程把请求
    /// 交给真正干活的浏览器进程后自己以 0 退出，chromiumoxide 的 pipe 只看到 exit 0），
    /// 我们必须把那一台接回来继续干活，而不是每次都失败。
    ///
    /// 这条测试自己造那个状态：先手工起一台无头浏览器占住一个临时 profile，再让
    /// BrowserSession 用同一个 profile 起，走 launch 失败 → 收养这条路径。
    #[tokio::test]
    #[ignore = "launches two real browsers to reproduce the launcher handoff"]
    async fn a_profile_owned_by_a_live_browser_is_adopted_not_refused() {
        let tag = format!("foxir_adopt_{}_{}", std::process::id(), stamp());
        let tmp = std::env::temp_dir().join(&tag);
        let profile = tmp.join(".browser_profile");
        std::fs::create_dir_all(&profile).unwrap();

        let exe = browser_launch::discover("")
            .chosen
            .map(|c| c.path)
            .expect("this test needs a browser installed");
        // 故意用 std::process::Command 起第一台：它不是 chromiumoxide 的孩子，
        // 也不会被我们的 Drop 回收 —— 就是现场那个"活着的占位者"。
        let _first = std::process::Command::new(&exe)
            .arg("--headless")
            .arg("--no-sandbox")
            .arg("--disable-gpu")
            .arg("--no-first-run")
            .arg("--remote-debugging-port=0")
            // Chromium 只认 `--switch=value` 这种带等号的形式；分成两个参数时它会
            // 把路径当成位置参数，profile 就落到别处去了（真机踩过一次，测试因此空跑）。
            .arg(format!("--user-data-dir={}", profile.display()))
            .spawn()
            .expect("spawn the first browser");

        let port = wait_for_devtools_port(&profile).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let seen: Value = client
            .get(format!("http://127.0.0.1:{}/json/version", port))
            .send()
            .await
            .expect("first browser must serve /json/version")
            .json()
            .await
            .unwrap();
        println!(
            "leftover browser: {} on port {}",
            seen["Browser"], port
        );

        // 第二台（我们的工具）：profile 已被占，launch 必然以交接的方式失败，
        // 失败路径应当收养上面那一台，而不是把错误抛给调用方。
        let s = BrowserSession::with_profile_dir(
            tmp.to_string_lossy().to_string(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(String::new())),
            profile.clone(),
        );
        let page = s
            .get_or_init("a")
            .await
            .expect("adopting the live browser must beat 'launch failed'");
        page.goto(chromiumoxide::cdp::browser_protocol::page::NavigateParams {
            url: "data:text/html,<title>adopted</title>".to_string(),
            referrer: None,
            transition_type: None,
            frame_id: None,
            referrer_policy: None,
        })
        .await
        .expect("a command over the adopted connection must work");
        let st = s.status(Some("a")).await;
        println!(
            "after adoption: running={} pages={} our_page={}",
            st["running"], st["pages_in_browser"], st["url"].as_str().unwrap_or("?")
        );
        assert_eq!(st["running"], json!(true));

        // 收尾：收养来的那台没有子进程句柄，close() 走 CDP Browser.close 把它带走。
        s.close().await.unwrap();
        let gone = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let ok = client
                    .get(format!("http://127.0.0.1:{}/json/version", port))
                    .send()
                    .await
                    .is_ok();
                if !ok {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(gone, "the adopted browser was still listening 15s after close");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 等第一台浏览器把端口写进 profile（`--remote-debugging-port=0` 下由它自己挑）。
    async fn wait_for_devtools_port(profile: &Path) -> u16 {
        for _ in 0..60 {
            if let Ok(txt) = std::fs::read_to_string(profile.join("DevToolsActivePort")) {
                if let Some(p) = parse_devtools_active_port(&txt) {
                    return p;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("the first browser never wrote DevToolsActivePort");
    }

    /// 真机回归：Settings 里那个无头开关必须在**下一次调用**就生效。
    ///
    /// 为什么值得单独一条：浏览器现在会在 run 之间存活（还可能是从上一台收养来的），
    /// 如果只在启动时读一次开关，用户勾掉无头之后看到的仍然只有截图 —— 而可见窗口是
    /// "登录一次、之后长期复用"这条路的唯一入口（账号密码/扫码/2FA 都没法在隐藏窗口里做）。
    #[tokio::test]
    #[ignore = "flips the headless switch and briefly opens a real window"]
    async fn the_headless_switch_takes_effect_on_the_next_call() {
        let tmp = std::env::temp_dir().join(format!("foxir_mode_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let headless = Arc::new(AtomicBool::new(true));
        let s = BrowserSession::with_profile_dir(
            tmp.to_string_lossy().to_string(),
            headless.clone(),
            Arc::new(RwLock::new(String::new())),
            tmp.join(".browser_profile"),
        );

        let page = s.get_or_init("a").await.expect("headless launch must work");
        let before = page.target_id().clone();
        assert_eq!(s.status(Some("a")).await["mode"], json!("headless"));

        headless.store(false, Ordering::Relaxed);
        let probe = s.status(Some("a")).await;
        assert_eq!(probe["running"], json!(true));
        assert_eq!(
            probe["restart_pending"],
            json!(true),
            "开关还没落到这台活浏览器上，探针必须说出来: {probe}"
        );

        let after = s.get_or_init("a").await.expect("relaunch in visible mode must work");
        assert_ne!(*after.target_id(), before, "换模式必须是一台新浏览器");
        let st = s.status(Some("a")).await;
        assert_eq!(st["mode"], json!("visible"));
        assert_eq!(st["restart_pending"], json!(false));
        assert_eq!(st["running"], json!(true));

        s.close().await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// profile 就在 case 目录里（本机工具，不做网络暴露面设计）：案件目录自包含、
    /// 拷走案子带着登录态、删案即清。但每个 workspace 必须各拿一份目录——Chromium 的
    /// 单实例互斥按 user-data-dir 划分，两个案子共用一份会直接起不来。
    #[test]
    fn profile_lives_in_the_case_dir_and_is_per_case() {
        let base = std::env::temp_dir().join(format!("foxir_ws_{}", std::process::id()));
        let case_a = base.join("case-a");
        let case_b = base.join("case-b");
        std::fs::create_dir_all(&case_a).unwrap();
        std::fs::create_dir_all(&case_b).unwrap();

        let a = default_profile_dir(&case_a.to_string_lossy());
        let b = default_profile_dir(&case_b.to_string_lossy());
        assert_eq!(a, case_a.join(".browser_profile"), "profile 要跟着案子走: {a:?}");
        assert_ne!(a, b, "两个案子不能共用一份 user-data-dir");
        assert_eq!(
            a,
            default_profile_dir(&case_a.to_string_lossy()),
            "同一个 workspace 每次都要解析到同一份 profile（登录态才留得住）"
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
        assert_eq!(first.file_name().unwrap(), ".browser_profile", "{first:?}");
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

    /// 模型传进来的是绝对路径也不能把截图带出案件目录。
    ///
    /// 现场那次是 `shell_exec` 绕开本工具写到 `C:\workspace\output`，但本工具自己这条
    /// 路径必须守得住：证据散在案件目录之外就进不了证据包，也服务不出去。
    #[test]
    fn a_screenshot_never_leaves_the_runs_output_dir() {
        let ws = std::env::temp_dir().join(format!("foxir_shot_{}", std::process::id()));
        let out = ws.join("output");
        std::fs::create_dir_all(&ws).unwrap();

        for asked in [
            r"C:\workspace\output\baidu_home.png",
            "../../Windows/temp/evil.png",
            "/etc/passwd.png",
        ] {
            let p = screenshot_target(&out.to_string_lossy(), Some(asked));
            assert_eq!(p.parent().unwrap(), out, "{asked} -> {p:?}");
            assert!(p.to_string_lossy().ends_with(".png"), "{asked} -> {p:?}");
        }

        // 纯目录参数没有文件名 → 回落到自动命名；不传 path 也一样
        for asked in ["..", "output/", ""] {
            let p = screenshot_target(&out.to_string_lossy(), Some(asked));
            assert_eq!(p.parent().unwrap(), out, "{asked} -> {p:?}");
        }
        let auto = screenshot_target(&out.to_string_lossy(), None);
        assert_eq!(auto.parent().unwrap(), out, "{auto:?}");
        std::fs::write(&auto, b"png").unwrap();
        let url = display_url(&auto, &ws.to_string_lossy()).replace('\\', "/");
        assert!(url.starts_with("/workspace/output/screenshot_"), "{url}");

        let _ = std::fs::remove_dir_all(&ws);
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

    /// 空闲回收的判定：只有"没人握页"且"确实空够久"才收浏览器。
    #[test]
    fn only_a_long_idle_empty_session_gets_reaped() {
        let now = Instant::now();
        let long_ago = now - IDLE_REAP - Duration::from_secs(1);
        assert!(should_reap(true, Some(long_ago), now, IDLE_REAP));
        assert!(!should_reap(true, Some(now), now, IDLE_REAP), "刚空下来不收，下一轮再看");
        assert!(!should_reap(false, Some(long_ago), now, IDLE_REAP), "还有人握页就不能收");
        assert!(!should_reap(true, None, now, IDLE_REAP), "从来没空过就不该收");
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
        let v = s.status(None).await;
        assert_eq!(v["running"], json!(false));
        assert!(!s.is_alive(), "status() must not change the session state");
        assert!(v.get("searched").is_some(), "must always report where it looked");
        assert_eq!(
            v["pages_in_browser"],
            json!(0),
            "a session that never launched has no pages to report: {v}"
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

        s.get_or_init("a").await.expect("first launch must succeed");
        let first = s.status(Some("a")).await;
        println!(
            "round 1: running={} browser={} [{}] v{}",
            first["running"], first["browser"], first["browser_source"], first["version_on_disk"]
        );
        assert_eq!(first["running"], json!(true));

        s.close().await.expect("close must not fail");
        assert!(!s.is_alive(), "session must be marked dead after close");

        let again = s.get_or_init("a").await.expect("relaunch right after close must succeed");
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

        s.get_or_init("a").await.expect("headed launch must succeed");
        let st = s.status(Some("a")).await;
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

        let page = s.get_or_init("a").await.expect("first launch must succeed");
        page.goto("https://example.com/").await.expect("navigate to example.com");
        let ours = s.page_info("a").await.expect("our page is listed");
        assert!(ours.0.contains("example.com"), "our page url: {ours:?}");

        // 站点自己开一页：只要求"看得见多出来的这一页"。刚弹出的页在 CDP 列表里 URL/标题
        // 常常还是空的（导航没提交），所以不能拿 URL 当识别依据。
        page.evaluate_expression("window.open('https://example.net/'); 'ok'")
            .await
            .expect("window.open should not throw");
        let listed = s.list_tabs("a").await.expect("list_tabs");
        assert!(
            listed["pages_in_browser"].as_u64().unwrap_or(0) >= 2,
            "the page the site opened must be counted: {listed}"
        );
        // 只报自己那一页，不泄露别人/站点那页的 URL
        assert!(listed["page"]["url"].as_str().unwrap_or_default().contains("example.com"),
                "{listed}");
        assert!(listed.get("tabs").is_none(), "must not list other agents' pages: {listed}");
        assert!(page.get_title().await.is_ok(), "our own page must still be drivable");

        // 我们脚下这页没了：recover_page 应当另开一页，而不是让后续动作全线报错
        println!("closing our own page: {:?}", page.close().await);
        s.recover_page("a").await.expect("recover_page must not fail");
        let again = s.get_or_init("a").await.expect("a page must be available again");
        assert!(again.get_title().await.is_ok(), "the replacement page must be drivable");
        assert!(s.list_tabs("a").await.is_ok(), "listing still works");

        s.close().await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 真机验证（默认 `#[ignore]`）：方案 B 的正面证据——同一个 profile、同一个浏览器
    /// 进程里，两个 agent 各自握一页，互不干扰；一个交回自己的页不影响另一个继续驱动，
    /// 交回后同一 key 再要页拿到的是新的一页。
    #[tokio::test]
    #[ignore = "launches a real browser"]
    async fn two_agents_each_own_their_own_page() {
        let tmp = std::env::temp_dir().join(format!("foxir_cdp_two_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let s = session_in(tmp.to_str().unwrap(), true);

        let a_page = s.get_or_init("agent-a").await.expect("agent A gets a page");
        let b_page = s.get_or_init("agent-b").await.expect("agent B gets a page");
        assert_ne!(
            a_page.target_id(),
            b_page.target_id(),
            "两个 agent 必须是两页，否则会互相导航"
        );
        // 同一个 key 再要是同一页（复用，不重开）
        let a_again = s.get_or_init("agent-a").await.expect("agent A re-asks");
        assert_eq!(a_again.target_id(), a_page.target_id());

        a_page.goto("https://example.com/").await.expect("A navigates");
        b_page.goto("https://example.net/").await.expect("B navigates");
        let a_info = s.page_info("agent-a").await.expect("A listed");
        let b_info = s.page_info("agent-b").await.expect("B listed");
        assert!(a_info.0.contains("example.com") && b_info.0.contains("example.net"),
                "各自的页互不串：{a_info:?} {b_info:?}");

        // A 交回自己那一页，B 必须照常能用
        s.release("agent-a").await;
        let after = s.list_tabs("agent-b").await.expect("B still lists");
        assert!(after["page"]["url"].as_str().unwrap_or_default().contains("example.net"),
                "A 的退出不该动 B: {after}");
        assert!(b_page.get_title().await.is_ok(), "B 的句柄必须还能发命令");
        assert_eq!(after["pages_we_own"].as_u64(), Some(1), "{after}");

        // A 回来时是另一页（旧的已经关了）
        let a_new = s.get_or_init("agent-a").await.expect("A gets a fresh page");
        assert_ne!(a_new.target_id(), a_page.target_id());

        s.close().await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}