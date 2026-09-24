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
//! - `probe`: Report which browser would be used + current session state
//! - `close`: Close the browser session

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams,
};
use chromiumoxide::page::Page;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard};
use tracing::{info, warn};

use super::{TimeoutStage, Tool};
use super::browser_launch;
use crate::context::ToolContext;
use crate::error::AgentResult;

/// Maximum text length returned to the LLM (to avoid flooding context).


/// 关闭会话时等待浏览器进程真正退出的宽限，超时才强杀。
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// Inner state holding the browser connection.
struct BrowserInner {
    browser: Browser,
    page: Page,
}

/// Shared browser session with lazy initialization and auto-recovery.
pub struct BrowserSession {
    inner: Mutex<Option<BrowserInner>>,
    workspace_dir: String,
    /// 持久浏览器 profile：跨启动保留，登录态就住在这里，绝不删除。
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

impl BrowserSession {
    /// `headless` / `executable_override` 是 Settings 的热更开关：会话只持有原子和锁的
    /// 引用，切换不需要重启进程，下一次启动浏览器时即生效。
    pub fn new(
        workspace_dir: String,
        headless: Arc<AtomicBool>,
        executable_override: Arc<RwLock<String>>,
    ) -> Arc<Self> {
        // 目录名与旧的 .chrome_cdp 不同是有意为之：出问题的机器上旧目录可能还带着
        // 起不来的残留状态，换名字等于给它一次干净的重启。
        let profile_dir = PathBuf::from(&workspace_dir).join(".browser_profile");
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

    /// Clear stale browser state, invalidate the generation, and mark as dead.
    async fn clear_state(&self) {
        let mut guard = self.inner.lock().await;
        guard.take(); // drop BrowserInner (kill_on_drop kills Chrome if still running)
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

    /// Get a live page, auto-recovering if the browser died.
    /// The lock is held across the whole check+launch sequence so that
    /// concurrent callers cannot spawn two Chrome processes.
    async fn get_or_init(&self) -> Result<Page, String> {
        let mut guard = self.inner.lock().await;

        // Fast path: browser is alive and we have a page
        if self.is_alive() {
            if let Some(inner) = guard.as_ref() {
                return Ok(inner.page.clone());
            }
        }

        // Slow path: clear stale state and (re-)launch while holding the lock
        guard.take();
        let page = self.launch_locked(&mut guard).await?;
        Ok(page)
    }

    /// Launch a fresh browser instance. Caller MUST hold the inner lock.
    ///
    /// 可执行文件由 `browser_launch::discover` 自己探测（Settings 显式路径 → 环境变量
    /// → PATH → 注册表 → 常见安装目录），不依赖 chromiumoxide 的内置检测；任何一步
    /// 失败都返回带路径/来源/版本/模式/profile 状态的诊断文本。
    async fn launch_locked(
        &self,
        guard: &mut MutexGuard<'_, Option<BrowserInner>>,
    ) -> Result<Page, String> {
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
        let mut builder = BrowserConfig::builder()
            .no_sandbox()
            .chrome_executable(&chosen.path)
            .user_data_dir(&self.profile_dir)
            .launch_timeout(Duration::from_secs(browser_launch::LAUNCH_WAIT_SECS))
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
                return Err(self.launch_failed(
                    diag_ctx,
                    &format!("Browser started but the initial tab failed: {}", e),
                    started.elapsed().as_secs(),
                ));
            }
        };

        info!("Browser CDP: browser launched successfully (gen {})", gen);

        **guard = Some(BrowserInner {
            browser,
            page: page.clone(),
        });

        Ok(page)
    }

    /// Close the browser session, and make sure the process is really gone.
    ///
    /// `Browser::close()` 只是往远端发一条 CDP `Browser.close`，发完就返回：进程可能
    /// 还在刷盘、还在写 profile。原实现发完就 drop，于是下一轮 launch 撞上“上一个实例
    /// 还持有 user-data-dir”的单实例互斥（Chromium 的单实例语义按用户数据目录划分，
    /// 不是按机器划分）——这正是 Win10 测试机上启动失败的直接原因。这里 close 之后等
    /// 进程真的退出，超时才强杀，两条路走完才返回。
    pub async fn close(&self) -> Result<(), String> {
        let mut guard = self.inner.lock().await;
        if let Some(mut inner) = guard.take() {
            info!("Browser CDP: closing browser");
            let _ = inner.browser.close().await;
            match tokio::time::timeout(CLOSE_GRACE, inner.browser.wait()).await {
                Ok(Ok(status)) => {
                    info!("Browser CDP: browser process exited ({:?})", status);
                }
                Ok(Err(e)) => {
                    warn!("Browser CDP: waiting for browser exit failed: {}", e);
                    let _ = inner.browser.kill().await;
                }
                Err(_) => {
                    warn!(
                        "Browser CDP: browser still alive after {}s, killing it",
                        CLOSE_GRACE.as_secs()
                    );
                    let _ = inner.browser.kill().await;
                }
            }
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.browser_alive.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// 在已运行的共享实例上开一个临时 tab（报告导出用），调用方用完自己关 tab。
    ///
    /// 不要为了导出另起一个浏览器：同一台机器上两个实例抢同一个 user-data-dir 会直接
    /// 失败。也没必要去碰 `Browser::close()`——那会关掉整个会话。
    pub async fn scratch_page(&self) -> Result<Page, String> {
        self.get_or_init().await?;
        let guard = self.inner.lock().await;
        let inner = guard
            .as_ref()
            .ok_or_else(|| "Browser session is not running".to_string())?;
        inner
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| format!("Failed to open tab: {}", e))
    }

    /// 自检报告：会用哪个浏览器、现在在不在跑、什么模式、profile 在哪。
    ///
    /// 不启动浏览器——它存在的意义正是“起不来的时候能问出为什么”。
    pub async fn status(&self) -> Value {
        let running = {
            let guard = self.inner.lock().await;
            guard.is_some()
        } && self.is_alive();
        let headless = self.headless.load(Ordering::Relaxed);
        let discovery = browser_launch::discover(&self.override_path());

        let current_url = if running {
            let guard = self.inner.lock().await;
            match guard.as_ref() {
                Some(inner) => inner
                    .page
                    .url()
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                None => String::new(),
            }
        } else {
            String::new()
        };

        let (browser, browser_source, version) = match &discovery.chosen {
            Some(c) => (
                c.path.to_string_lossy().to_string(),
                c.source.label().to_string(),
                browser_launch::version_from_layout(&c.path),
            ),
            None => (String::new(), "not found".to_string(), String::new()),
        };

        json!({
            "success": true,
            "action": "probe",
            "running": running,
            "current_url": current_url,
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

    fn output_dir(&self) -> PathBuf {
        let dir = PathBuf::from(&self.session.workspace_dir).join("output");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}

#[async_trait]
impl Tool for BrowserCdpTool {
    fn name(&self) -> &str { "browser_cdp" }

    fn description(&self) -> &str {
        "Browser automation via CDP (Chrome DevTools Protocol). \
         Runs hidden by default (no visible window); a visible window can be enabled in Settings. \
         Use this for: screenshots, web scraping, checking URLs, extracting page content. \
         It drives its own browser profile stored under the workspace, so it does not start \
         with the cookies of an everyday browser; a site signed into once in this profile \
         stays signed in for later sessions.\n\
         A login that needs a password, 2FA or a QR scan must be done once in the \
         visible-window mode available in Settings; headless runs then inherit that state.\n\
         Actions:\n\
         - 'navigate': Go to a URL. Provide 'url'.\n\
         - 'get_text': Get page text or element text. Optional 'selector' (CSS).\n\
         - 'click': Click an element. Provide 'selector' (CSS).\n\
         - 'type_text': Type into an element. Provide 'selector' and 'text'.\n\
         - 'screenshot': Take a screenshot. Optional 'path' (defaults to workspace/output/).\n\
         - 'get_url': Get current page URL.\n\
         - 'get_html': Get page or element HTML. Optional 'selector' (CSS).\n\
         - 'execute_js': Run JavaScript. Provide 'js'.\n\
         - 'find_element': Find element by CSS selector. Provide 'selector'.\n\
         - 'probe': Report the detected browser executable, mode and session state without \
           launching anything. Use it first when a launch fails.\n\
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
                             "get_url", "get_html", "execute_js", "find_element", "probe", "close"],
                    "description": "Which browser action to perform"
                },
                "url": {
                    "type": "string",
                    "description": "URL to navigate to (for 'navigate' action)"
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
                    "description": "File path for screenshot (optional, defaults to workspace/output/)"
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

        match result {
            Err(ref e) if Self::is_connection_lost(&e.to_string()) => {
                warn!("Browser CDP: connection lost during '{}', attempting auto-recovery", action);
                self.session.clear_state().await;
                // Only 'navigate' is meaningful to retry — other actions operate on page
                // state that is lost when the browser restarts (fresh page = about:blank).
                if action == "navigate" {
                    self.execute_action(action, &args, &output_dir, max_text_len).await
                } else {
                    Err(format!(
                        "Browser session was lost and has been restarted with a blank page. \
                         The '{}' action cannot be retried without page state. \
                         Call 'navigate' with the URL first, then retry '{}'.",
                        action, action
                    ).into())
                }
            }
            other => other,
        }
    }
}

impl BrowserCdpTool {
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
        // All actions need the browser initialized
        let page = self.session.get_or_init().await
            .map_err(|e| -> crate::error::AgentError { e.into() })?;

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
                match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    page.wait_for_navigation(),
                ).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => return Err(format!("Navigation wait failed: {}", e).into()),
                    Err(_) => warn!("Browser CDP: navigation wait timed out after 30s, continuing"),
                }
                let title = page.get_title().await
                    .map_err(|e| format!("Get title failed: {}", e))?
                    .unwrap_or_default();
                Ok(json!({
                    "success": true,
                    "action": "navigate",
                    "url": url,
                    "title": title
                }))
            }

            "get_text" => {
                let text = if let Some(selector) = args["selector"].as_str() {
                    let elem = page.find_element(selector)
                        .await
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
                let truncated = text.chars().count() > max_text_len;
                let brief: String = if truncated {
                    text.chars().take(max_text_len).collect::<String>()
                } else {
                    text
                };
                Ok(json!({
                    "success": true,
                    "action": "get_text",
                    "text": brief,
                    "truncated": truncated
                }))
            }

            "click" => {
                let selector = args["selector"].as_str()
                    .ok_or_else(|| "Missing 'selector' for click".to_string())?;
                let elem = page.find_element(selector)
                    .await
                    .map_err(|e| format!("Element not found '{}': {}", selector, e))?;
                elem.click().await
                    .map_err(|e| format!("Click failed: {}", e))?;
                let _ = tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                Ok(json!({
                    "success": true,
                    "action": "click",
                    "selector": selector
                }))
            }

            "type_text" => {
                let selector = args["selector"].as_str()
                    .ok_or_else(|| "Missing 'selector' for type_text".to_string())?;
                let text = args["text"].as_str()
                    .ok_or_else(|| "Missing 'text' for type_text".to_string())?;
                let elem = page.find_element(selector)
                    .await
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
                let filename = format!("screenshot_{}.png",
                    chrono::Local::now().format("%Y%m%d_%H%M%S"));
                // Always save into workspace/output/ — if user provides 'path',
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
                    ..Default::default()
                };
                page.save_screenshot(params, &path)
                    .await
                    .map_err(|e| format!("Screenshot failed: {}", e))?;
                let url = format!("/workspace/output/{}", file_name);
                Ok(json!({
                    "success": true,
                    "action": "screenshot",
                    "url": url
                }))
            }

            "get_url" => {
                let url = page.url().await
                    .map_err(|e| format!("Get URL failed: {}", e))?
                    .unwrap_or_default();
                let title = page.get_title().await
                    .map_err(|e| format!("Get title failed: {}", e))?
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
                    let elem = page.find_element(selector)
                        .await
                        .map_err(|e| format!("Element not found '{}': {}", selector, e))?;
                    elem.inner_html().await
                        .map_err(|e| format!("Get HTML failed: {}", e))?
                        .unwrap_or_default()
                } else {
                    page.content().await
                        .map_err(|e| format!("Get content failed: {}", e))?
                };
                let truncated = html.chars().count() > max_text_len;
                let brief: String = if truncated {
                    html.chars().take(max_text_len).collect::<String>()
                } else {
                    html
                };
                Ok(json!({
                    "success": true,
                    "action": "get_html",
                    "html": brief,
                    "truncated": truncated
                }))
            }

            "execute_js" => {
                let js = args["js"].as_str()
                    .ok_or_else(|| "Missing 'js' for execute_js".to_string())?;
                let result = page.evaluate_expression(js)
                    .await
                    .map_err(|e| format!("JS execution failed: {}", e))?;
                let value = result.value().cloned().unwrap_or(Value::Null);
                Ok(json!({
                    "success": true,
                    "action": "execute_js",
                    "result": value
                }))
            }

            "find_element" => {
                let selector = args["selector"].as_str()
                    .ok_or_else(|| "Missing 'selector' for find_element".to_string())?;
                let elem = page.find_element(selector)
                    .await
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
                let text_brief = if text.len() > 500 { &text[..500] } else { &text };
                Ok(json!({
                    "success": true,
                    "action": "find_element",
                    "selector": selector,
                    "attributes": attr_map,
                    "text": text_brief
                }))
            }

            _ => Err(format!(
                "Unknown action '{}'. Valid: navigate, get_text, click, type_text, \
                 screenshot, get_url, get_html, execute_js, find_element, probe, close",
                action
            ).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_in(dir: &str, headless: bool) -> Arc<BrowserSession> {
        BrowserSession::new(
            dir.to_string(),
            Arc::new(AtomicBool::new(headless)),
            Arc::new(RwLock::new(String::new())),
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

    /// profile 必须落在 workspace 内，不能拿用户日常浏览器的目录去跑自动化。
    #[test]
    fn profile_lives_inside_the_workspace() {
        let s = session_in(r"C:\IR\case1", true);
        assert!(s.profile_dir.starts_with(r"C:\IR\case1"), "{:?}", s.profile_dir);
        assert_eq!(s.profile_dir.file_name().unwrap(), ".browser_profile");
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
}
