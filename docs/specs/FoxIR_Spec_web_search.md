# Web Search 多引擎可配置化 Spec v1.0（Rust 实现）

## 1. 背景与目标

当前 `web_search` 功能无法配置。目标是在 Rust 服务中实现：

- 支持选择：`baidu`、`bing`、`google`、`duckduckgo`、`brave`、`tavily`、`firecrawl`
- 支持自定义 HTTP 搜索服务，例如 `firecrack`
- 统一调用入口：`web_search(query, engine?, max_results?)`
- 统一返回结构，屏蔽不同引擎差异
- 配置化：启用/禁用、默认引擎、密钥、请求/响应映射
- 安全：密钥脱敏、SSRF 防护、超时、限流、日志脱敏
- 可测试、可观测、可逐步上线

非目标：

- 不实现完整搜索引擎爬虫
- 不保证传统引擎 HTML 解析长期稳定；生产建议优先官方 API 或 Tavily/Firecrawl
- 不负责搜索引擎账号申请与配额管理

---

## 2. 技术选型

| 能力 | 推荐 crate |
|---|---|
| 异步运行时 | `tokio` |
| HTTP 客户端 | `reqwest` + `rustls-tls` |
| 序列化 | `serde`、`serde_json`、`serde_yaml` |
| 错误 | `thiserror` |
| 异步 trait | `async-trait` |
| URL 处理 | `url` |
| HTML 解析 | `scraper` |
| JSONPath | `jsonpath-rust` |
| Web API | `axum` |
| 日志 | `tracing`、`tracing-subscriber` |
| 配置热加载 | `arc-swap`、`notify` |
| 测试 | `wiremock`、`tokio::test` |

---

## 3. 目录结构

```text
web-search/
  Cargo.toml
  config/
    web_search.yaml
  src/
    lib.rs
    main.rs
    config.rs
    error.rs
    models.rs
    registry.rs
    api.rs
    engine/
      mod.rs
      url_template.rs
      tavily.rs
      firecrawl.rs
      custom_http.rs
    parser/
      mod.rs
      html.rs
```

---

## 4. 配置格式

`config/web_search.yaml`：

```yaml
web_search:
  default_engine: bing
  timeout_ms: 10000
  max_results: 5
  engines:
    baidu:
      enabled: true
      type: url_template
      url_template: "https://www.baidu.com/s?wd={query}"
      selectors:
        item: "div.result"
        title: "h3 a"
        url: "h3 a"
        snippet: ".c-abstract"

    bing:
      enabled: true
      type: url_template
      url_template: "https://www.bing.com/search?q={query}"
      selectors:
        item: "li.b_algo"
        title: "h2 a"
        url: "h2 a"
        snippet: ".b_caption p"

    google:
      enabled: true
      type: url_template
      url_template: "https://www.google.com/search?q={query}"
      selectors:
        item: "div.g"
        title: "h3"
        url: "a"
        snippet: "div.VwiC3b"

    duckduckgo:
      enabled: true
      type: url_template
      url_template: "https://duckduckgo.com/?q={query}"
      selectors:
        item: "article[data-testid='result']"
        title: "a[data-testid='result-title-a']"
        url: "a[data-testid='result-title-a']"
        snippet: "div[data-testid='result-snippet']"

    brave:
      enabled: true
      type: url_template
      url_template: "https://search.brave.com/search?q={query}"
      selectors:
        item: "div.snippet"
        title: "a"
        url: "a"
        snippet: "div.snippet-description"

    tavily:
      enabled: true
      type: tavily
      endpoint: "https://api.tavily.com/search"
      api_key: "${TAVILY_API_KEY}"
      search_depth: basic
      max_results: 5

    firecrawl:
      enabled: true
      type: firecrawl
      endpoint: "https://api.firecrawl.dev/v2/search"
      api_key: "${FIRECRAWL_API_KEY}"
      limit: 5

    custom_firecrack:
      enabled: true
      type: custom_http
      method: POST
      url: "https://api.example.com/search"
      headers:
        Authorization: "Bearer ${CUSTOM_API_KEY}"
        Content-Type: "application/json"
      body:
        q: "{query}"
        limit: "{max_results}"
      response:
        results_path: "$.data"
        fields:
          title: "$.title"
          url: "$.url"
          snippet: "$.snippet"
```

传统引擎查询参数：

| 引擎 | 查询地址模板 | 参数 |
|---|---|---|
| Baidu | `https://www.baidu.com/s?wd={query}` | `wd` |
| Bing | `https://www.bing.com/search?q={query}` | `q` |
| Google | `https://www.google.com/search?q={query}` | `q` |
| DuckDuckGo | `https://duckduckgo.com/?q={query}` | `q` |
| Brave | `https://search.brave.com/search?q={query}` | `q` |

Tavily / Firecrawl 是 API 型，必须 `POST` + 密钥，不能只拼 URL。

---

## 5. 核心模型

```rust
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub max_results: usize,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub rank: usize,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub engine: String,
    pub query: String,
    pub results: Vec<SearchResult>,
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
}
```

---

## 6. 核心 Trait

```rust
use async_trait::async_trait;
use crate::error::SearchError;
use crate::models::{SearchOptions, SearchResult};

#[async_trait]
pub trait SearchEngine: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn kind(&self) -> &'static str;

    async fn search(
        &self,
        query: &str,
        opts: &SearchOptions,
    ) -> Result<Vec<SearchResult>, SearchError>;
}
```

---

## 7. 配置解析

```rust
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct WebSearchConfig {
    pub default_engine: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    pub engines: HashMap<String, EngineEntry>,
}

fn default_timeout_ms() -> u64 { 10_000 }
fn default_max_results() -> usize { 5 }

#[derive(Debug, Clone, Deserialize)]
pub struct EngineEntry {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub config: EngineConfig,
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineConfig {
    UrlTemplate(UrlTemplateConfig),
    Tavily(TavilyConfig),
    Firecrawl(FirecrawlConfig),
    CustomHttp(CustomHttpConfig),
}

#[derive(Debug, Clone, Deserialize)]
pub struct UrlTemplateConfig {
    pub url_template: String,
    #[serde(default)]
    pub selectors: Option<HtmlSelectors>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HtmlSelectors {
    pub item: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TavilyConfig {
    pub endpoint: String,
    pub api_key: String,
    #[serde(default)]
    pub search_depth: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FirecrawlConfig {
    pub endpoint: String,
    pub api_key: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CustomHttpConfig {
    #[serde(default = "default_method")]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    pub response: CustomResponseConfig,
}

fn default_method() -> String { "GET".into() }

#[derive(Debug, Clone, Deserialize)]
pub struct CustomResponseConfig {
    pub results_path: String,
    pub fields: CustomFields,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CustomFields {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub snippet: Option<String>,
}
```

---

## 8. 引擎实现

### 8.1 URL 模板引擎

适用：`baidu`、`bing`、`google`、`duckduckgo`、`brave`。

```rust
use async_trait::async_trait;
use scraper::{Html, Selector};
use crate::config::UrlTemplateConfig;
use crate::error::SearchError;
use crate::models::{SearchOptions, SearchResult};
use crate::engine::SearchEngine;

pub struct UrlTemplateEngine {
    id: String,
    name: String,
    cfg: UrlTemplateConfig,
    client: reqwest::Client,
}

impl UrlTemplateEngine {
    pub fn new(id: String, cfg: UrlTemplateConfig) -> Result<Self, SearchError> {
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; WebSearchBot/1.0)")
            .build()
            .map_err(|e| SearchError::ConfigInvalid(e.to_string()))?;

        Ok(Self {
            name: id.clone(),
            id,
            cfg,
            client,
        })
    }

    fn parse_html(&self, html: &str) -> Result<Vec<SearchResult>, SearchError> {
        let selectors = self.cfg.selectors.as_ref()
            .ok_or_else(|| SearchError::ParseError("missing selectors".into()))?;

        let doc = Html::parse_document(html);

        let item_sel = Selector::parse(&selectors.item)
            .map_err(|e| SearchError::ParseError(format!("item selector: {e:?}")))?;
        let title_sel = Selector::parse(&selectors.title)
            .map_err(|e| SearchError::ParseError(format!("title selector: {e:?}")))?;
        let url_sel = Selector::parse(&selectors.url)
            .map_err(|e| SearchError::ParseError(format!("url selector: {e:?}")))?;
        let snippet_sel = selectors.snippet.as_ref()
            .and_then(|s| Selector::parse(s).ok());

        let mut results = Vec::new();

        for (i, item) in doc.select(&item_sel).enumerate() {
            let title = item.select(&title_sel)
                .next()
                .map(|e| e.text().collect::<String>())
                .unwrap_or_default();

            let url = item.select(&url_sel)
                .next()
                .and_then(|e| e.value().attr("href"))
                .unwrap_or_default()
                .to_string();

            let snippet = snippet_sel.as_ref()
                .and_then(|sel| item.select(sel).next())
                .map(|e| e.text().collect::<String>())
                .unwrap_or_default();

            if !url.is_empty() {
                results.push(SearchResult {
                    title,
                    url,
                    snippet,
                    rank: i + 1,
                    source: self.id.clone(),
                    raw: None,
                });
            }
        }

        Ok(results)
    }
}

#[async_trait]
impl SearchEngine for UrlTemplateEngine {
    fn id(&self) -> &str { &self.id }
    fn name(&self) -> &str { &self.name }
    fn kind(&self) -> &'static str { "url_template" }

    async fn search(
        &self,
        query: &str,
        opts: &SearchOptions,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let encoded = url::form_urlencoded::byte_serialize(query.as_bytes())
            .collect::<String>();

        let url = self.cfg.url_template.replace("{query}", &encoded);

        let resp = self.client
            .get(&url)
            .timeout(opts.timeout)
            .send()
            .await?;

        let html = resp.text().await?;
        let mut results = self.parse_html(&html)?;
        results.truncate(opts.max_results);
        Ok(results)
    }
}
```

### 8.2 Tavily 引擎

```rust
use async_trait::async_trait;
use serde::Deserialize;
use crate::config::TavilyConfig;
use crate::error::SearchError;
use crate::models::{SearchOptions, SearchResult};
use crate::engine::SearchEngine;

#[derive(Deserialize)]
struct TavilyResponse {
    results: Vec<TavilyItem>,
}

#[derive(Deserialize)]
struct TavilyItem {
    title: String,
    url: String,
    content: Option<String>,
}

pub struct TavilyEngine {
    id: String,
    cfg: TavilyConfig,
    client: reqwest::Client,
}

impl TavilyEngine {
    pub fn new(id: String, cfg: TavilyConfig) -> Result<Self, SearchError> {
        Ok(Self {
            id,
            cfg,
            client: reqwest::Client::new(),
        })
    }
}

#[async_trait]
impl SearchEngine for TavilyEngine {
    fn id(&self) -> &str { &self.id }
    fn name(&self) -> &str { "Tavily" }
    fn kind(&self) -> &'static str { "tavily" }

    async fn search(
        &self,
        query: &str,
        opts: &SearchOptions,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let body = serde_json::json!({
            "api_key": self.cfg.api_key,
            "query": query,
            "search_depth": self.cfg.search_depth.as_deref().unwrap_or("basic"),
            "max_results": opts.max_results,
        });

        let resp = self.client
            .post(&self.cfg.endpoint)
            .json(&body)
            .timeout(opts.timeout)
            .send()
            .await?;

        let data: TavilyResponse = resp.json().await?;

        Ok(data.results
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.content.unwrap_or_default(),
                rank: i + 1,
                source: self.id.clone(),
                raw: None,
            })
            .collect())
    }
}
```

### 8.3 Firecrawl 引擎

```rust
use async_trait::async_trait;
use serde::Deserialize;
use crate::config::FirecrawlConfig;
use crate::error::SearchError;
use crate::models::{SearchOptions, SearchResult};
use crate::engine::SearchEngine;

#[derive(Deserialize)]
struct FirecrawlResponse {
    data: Vec<FirecrawlItem>,
}

#[derive(Deserialize)]
struct FirecrawlItem {
    title: String,
    url: String,
    description: Option<String>,
}

pub struct FirecrawlEngine {
    id: String,
    cfg: FirecrawlConfig,
    client: reqwest::Client,
}

impl FirecrawlEngine {
    pub fn new(id: String, cfg: FirecrawlConfig) -> Result<Self, SearchError> {
        Ok(Self {
            id,
            cfg,
            client: reqwest::Client::new(),
        })
    }
}

#[async_trait]
impl SearchEngine for FirecrawlEngine {
    fn id(&self) -> &str { &self.id }
    fn name(&self) -> &str { "Firecrawl" }
    fn kind(&self) -> &'static str { "firecrawl" }

    async fn search(
        &self,
        query: &str,
        opts: &SearchOptions,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let body = serde_json::json!({
            "query": query,
            "limit": opts.max_results,
        });

        let resp = self.client
            .post(&self.cfg.endpoint)
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .timeout(opts.timeout)
            .send()
            .await?;

        let data: FirecrawlResponse = resp.json().await?;

        Ok(data.data
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.description.unwrap_or_default(),
                rank: i + 1,
                source: self.id.clone(),
                raw: None,
            })
            .collect())
    }
}
```

### 8.4 自定义 HTTP 引擎

用于接入 `firecrack` 或任意内部搜索 API。

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use crate::config::CustomHttpConfig;
use crate::error::SearchError;
use crate::models::{SearchOptions, SearchResult};
use crate::engine::SearchEngine;

pub struct CustomHttpEngine {
    id: String,
    cfg: CustomHttpConfig,
    client: reqwest::Client,
}

impl CustomHttpEngine {
    pub fn new(id: String, cfg: CustomHttpConfig) -> Result<Self, SearchError> {
        validate_custom_url(&cfg.url)?;
        Ok(Self {
            id,
            cfg,
            client: reqwest::Client::new(),
        })
    }

    fn render_string(&self, input: &str, query: &str, max_results: usize) -> String {
        input
            .replace("{query}", query)
            .replace("{max_results}", &max_results.to_string())
    }

    fn render_headers(&self, query: &str, max_results: usize) -> HashMap<String, String> {
        self.cfg.headers
            .iter()
            .map(|(k, v)| {
                let rendered = self.render_string(v, query, max_results);
                let rendered = expand_env(&rendered);
                (k.clone(), rendered)
            })
            .collect()
    }
}

#[async_trait]
impl SearchEngine for CustomHttpEngine {
    fn id(&self) -> &str { &self.id }
    fn name(&self) -> &str { &self.id }
    fn kind(&self) -> &'static str { "custom_http" }

    async fn search(
        &self,
        query: &str,
        opts: &SearchOptions,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let url = self.render_string(&self.cfg.url, query, opts.max_results);
        let headers = self.render_headers(query, opts.max_results);

        let mut req = match self.cfg.method.to_uppercase().as_str() {
            "POST" => self.client.post(&url),
            _ => self.client.get(&url),
        };

        for (k, v) in headers {
            req = req.header(k, v);
        }

        if let Some(body) = &self.cfg.body {
            let body = render_json(body, query, opts.max_results);
            req = req.json(&body);
        }

        let resp = req.timeout(opts.timeout).send().await?;
        let json: serde_json::Value = resp.json().await?;

        let items = jsonpath(&json, &self.cfg.response.results_path)?;

        let mut results = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let title = get_string(item, &self.cfg.response.fields.title).unwrap_or_default();
            let url = get_string(item, &self.cfg.response.fields.url).unwrap_or_default();
            let snippet = self.cfg.response.fields.snippet.as_ref()
                .and_then(|p| get_string(item, p))
                .unwrap_or_default();

            if !url.is_empty() {
                results.push(SearchResult {
                    title,
                    url,
                    snippet,
                    rank: i + 1,
                    source: self.id.clone(),
                    raw: Some(item.clone()),
                });
            }
        }

        results.truncate(opts.max_results);
        Ok(results)
    }
}

fn expand_env(input: &str) -> String {
    // 简单实现：${ENV_NAME}
    let re = regex::Regex::new(r"\$\{([A-Z0-9_]+)\}").unwrap();
    re.replace_all(input, |caps: &regex::Captures| {
        std::env::var(&caps[1]).unwrap_or_default()
    }).to_string()
}

fn render_json(value: &serde_json::Value, query: &str, max_results: usize) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(
                s.replace("{query}", query)
                 .replace("{max_results}", &max_results.to_string())
            )
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|v| render_json(v, query, max_results)).collect())
        }
        serde_json::Value::Object(map) => {
            serde_json::Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), render_json(v, query, max_results)))
                    .collect()
            )
        }
        _ => value.clone(),
    }
}

fn jsonpath<'a>(json: &'a serde_json::Value, path: &str) -> Result<Vec<&'a serde_json::Value>, SearchError> {
    // 这里用 jsonpath-rust 或 serde_json_path 实现
    // 示例省略具体 crate 调用
    let finder = jsonpath_rust::JsonPathFinder::new_from_str(path)
        .map_err(|e| SearchError::ParseError(e.to_string()))?;
    let nodes = finder.find(json);
    Ok(nodes)
}

fn get_string(item: &serde_json::Value, path: &str) -> Option<String> {
    let finder = jsonpath_rust::JsonPathFinder::new_from_str(path).ok()?;
    let nodes = finder.find(item);
    nodes.first().and_then(|v| v.as_str()).map(|s| s.to_string())
}
```

---

## 9. 注册表与调用

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use crate::config::{EngineConfig, WebSearchConfig};
use crate::engine::{SearchEngine, UrlTemplateEngine, TavilyEngine, FirecrawlEngine, CustomHttpEngine};
use crate::error::SearchError;
use crate::models::{EngineInfo, SearchOptions, SearchResponse};

pub struct EngineRegistry {
    default_engine: String,
    timeout: Duration,
    default_max_results: usize,
    engines: HashMap<String, Arc<dyn SearchEngine>>,
}

impl EngineRegistry {
    pub fn from_config(cfg: WebSearchConfig) -> Result<Self, SearchError> {
        let timeout = Duration::from_millis(cfg.timeout_ms);
        let mut engines: HashMap<String, Arc<dyn SearchEngine>> = HashMap::new();

        for (id, entry) in cfg.engines {
            if !entry.enabled {
                continue;
            }

            let engine: Arc<dyn SearchEngine> = match entry.config {
                EngineConfig::UrlTemplate(c) => {
                    Arc::new(UrlTemplateEngine::new(id.clone(), c)?)
                }
                EngineConfig::Tavily(c) => {
                    Arc::new(TavilyEngine::new(id.clone(), c)?)
                }
                EngineConfig::Firecrawl(c) => {
                    Arc::new(FirecrawlEngine::new(id.clone(), c)?)
                }
                EngineConfig::CustomHttp(c) => {
                    Arc::new(CustomHttpEngine::new(id.clone(), c)?)
                }
            };

            engines.insert(id, engine);
        }

        if !engines.contains_key(&cfg.default_engine) {
            return Err(SearchError::ConfigInvalid(
                "default_engine 未启用或不存在".into(),
            ));
        }

        Ok(Self {
            default_engine: cfg.default_engine,
            timeout,
            default_max_results: cfg.max_results,
            engines,
        })
    }

    pub async fn search(
        &self,
        query: &str,
        engine: Option<&str>,
        max_results: Option<usize>,
    ) -> Result<SearchResponse, SearchError> {
        let engine_id = engine.unwrap_or(&self.default_engine);

        let eng = self.engines
            .get(engine_id)
            .ok_or_else(|| SearchError::EngineNotFound(engine_id.to_string()))?;

        let opts = SearchOptions {
            max_results: max_results.unwrap_or(self.default_max_results),
            timeout: self.timeout,
        };

        let results = eng.search(query, &opts).await?;

        Ok(SearchResponse {
            engine: engine_id.to_string(),
            query: query.to_string(),
            results,
            error: None,
        })
    }

    pub fn list_engines(&self) -> Vec<EngineInfo> {
        self.engines
            .values()
            .map(|e| EngineInfo {
                id: e.id().to_string(),
                name: e.name().to_string(),
                kind: e.kind().to_string(),
                enabled: true,
            })
            .collect()
    }
}
```

---

## 10. 错误定义

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SearchError {
    #[error("engine not found: {0}")]
    EngineNotFound(String),

    #[error("config invalid: {0}")]
    ConfigInvalid(String),

    #[error("api key missing")]
    ApiKeyMissing,

    #[error("upstream timeout")]
    UpstreamTimeout,

    #[error("upstream error: {0}")]
    UpstreamError(String),

    #[error("parse error: {0}")]
    ParseError(String),

    #[error("ssrf blocked: {0}")]
    SsrfBlocked(String),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

impl SearchError {
    pub fn code(&self) -> &'static str {
        match self {
            SearchError::EngineNotFound(_) => "ENGINE_NOT_FOUND",
            SearchError::ConfigInvalid(_) => "ENGINE_CONFIG_INVALID",
            SearchError::ApiKeyMissing => "API_KEY_MISSING",
            SearchError::UpstreamTimeout => "UPSTREAM_TIMEOUT",
            SearchError::UpstreamError(_) => "UPSTREAM_ERROR",
            SearchError::ParseError(_) => "PARSE_ERROR",
            SearchError::SsrfBlocked(_) => "SSRF_BLOCKED",
            SearchError::Http(_) => "HTTP_ERROR",
        }
    }
}
```

---

## 11. API 层（axum）

```rust
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;
use serde::Deserialize;

use crate::models::{EngineInfo, ErrorInfo, SearchResponse};
use crate::registry::EngineRegistry;

#[derive(Deserialize)]
struct SearchReq {
    query: String,
    engine: Option<String>,
    max_results: Option<usize>,
}

async fn list_engines(
    State(reg): State<Arc<EngineRegistry>>,
) -> Json<Vec<EngineInfo>> {
    Json(reg.list_engines())
}

async fn search(
    State(reg): State<Arc<EngineRegistry>>,
    Json(req): Json<SearchReq>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<ErrorInfo>)> {
    reg.search(&req.query, req.engine.as_deref(), req.max_results)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorInfo {
                    code: e.code().to_string(),
                    message: e.to_string(),
                }),
            )
        })
}

pub fn router(reg: Arc<EngineRegistry>) -> Router {
    Router::new()
        .route("/api/web_search/engines", get(list_engines))
        .route("/api/web_search", post(search))
        .with_state(reg)
}
```

---

## 12. 安全与稳定性

1. **密钥管理**：API Key 只存环境变量或密钥管理服务，前端/日志不返回明文。
2. **SSRF 防护**：自定义引擎 URL 禁止访问内网 IP、localhost、元数据地址。
3. **超时与重试**：默认 10s，最多重试 1 次；仅对 5xx/网络错误重试。
4. **限流**：按引擎维度限流，避免触发反爬或超额。
5. **日志脱敏**：`api_key`、`Authorization`、`token` 全部脱敏。
6. **结果校验**：统一过滤空 URL、非 http/https URL。
7. **降级**：默认引擎失败时可配置 fallback 引擎，如 `bing -> tavily`。

SSRF 校验示例：

```rust
use std::net::IpAddr;

fn validate_custom_url(raw: &str) -> Result<(), SearchError> {
    let url = url::Url::parse(raw)
        .map_err(|e| SearchError::ConfigInvalid(e.to_string()))?;

    let host = url.host_str()
        .ok_or_else(|| SearchError::ConfigInvalid("missing host".into()))?;

    if host == "localhost" || host.ends_with(".local") {
        return Err(SearchError::SsrfBlocked(host.to_string()));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if ip.is_loopback() || ip.is_private() || ip.is_link_local() {
            return Err(SearchError::SsrfBlocked(host.to_string()));
        }
    }

    Ok(())
}
```

---

## 13. 配置校验与热加载

启动时校验：

- `default_engine` 必须存在且启用
- `type` 必须是 `url_template | tavily | firecrawl | custom_http`
- `url_template` 必须包含 `{query}`
- API 型引擎必须有 `endpoint` 和 `api_key`
- 自定义引擎必须有 `response.results_path` 和字段映射

热加载：

- 文件监听或配置中心推送
- 校验通过后原子替换注册表
- 失败保留旧配置并告警

---

## 14. 测试方案

### 单元测试

- Baidu 模板生成：`wd=OpenAI`
- Bing/Google/DuckDuckGo/Brave 模板生成：`q=OpenAI`
- Tavily 请求体包含 `query` 和 `api_key`
- Firecrawl 请求头包含 `Authorization: Bearer`
- 自定义引擎 JSONPath 映射正确

### 集成测试

- 使用 `wiremock` Mock HTTP Server 返回各引擎响应
- 切换引擎后返回统一结构
- 引擎不存在、密钥缺失、超时、5xx 的错误码

### 验收测试

1. 配置中启用 `bing` 和 `tavily`
2. 调用 `web_search("OpenAI", engine="bing")` 返回 Bing 结果
3. 调用 `web_search("OpenAI", engine="tavily")` 返回 Tavily 结果
4. `GET /api/web_search/engines` 能看到所有启用引擎
5. 自定义 `custom_firecrack` 能通过配置接入并返回结果
6. 日志中不出现 API Key 明文

---

## 15. 错误码

| 错误码 | 说明 |
|---|---|
| `ENGINE_NOT_FOUND` | 引擎不存在或未启用 |
| `ENGINE_CONFIG_INVALID` | 引擎配置非法 |
| `API_KEY_MISSING` | 缺少密钥 |
| `UPSTREAM_TIMEOUT` | 上游超时 |
| `UPSTREAM_ERROR` | 上游返回错误 |
| `PARSE_ERROR` | 结果解析失败 |
| `SSRF_BLOCKED` | 自定义 URL 被安全策略拦截 |

---

## 16. 上线里程碑

| 阶段 | 内容 | 产出 |
|---|---|---|
| M1 | 配置模型 + 注册表 + URL 模板引擎 | 支持 baidu/bing/google/duckduckgo/brave |
| M2 | Tavily + Firecrawl 适配器 | 支持 API 型搜索 |
| M3 | 自定义 HTTP 引擎 | 支持 firecrack 等任意 API |
| M4 | API/UI 选择引擎 | 用户可选择、默认引擎生效 |
| M5 | 安全、限流、日志、测试 | 可生产上线 |
| M6 | 文档与示例 | 配置文档、接入指南 |

---

## 17. 最小可用配置

```yaml
web_search:
  default_engine: bing
  timeout_ms: 10000
  max_results: 5
  engines:
    baidu:
      enabled: true
      type: url_template
      url_template: "https://www.baidu.com/s?wd={query}"
    bing:
      enabled: true
      type: url_template
      url_template: "https://www.bing.com/search?q={query}"
    google:
      enabled: true
      type: url_template
      url_template: "https://www.google.com/search?q={query}"
    duckduckgo:
      enabled: true
      type: url_template
      url_template: "https://duckduckgo.com/?q={query}"
    brave:
      enabled: true
      type: url_template
      url_template: "https://search.brave.com/search?q={query}"
    tavily:
      enabled: true
      type: tavily
      endpoint: "https://api.tavily.com/search"
      api_key: "${TAVILY_API_KEY}"
    firecrawl:
      enabled: true
      type: firecrawl
      endpoint: "https://api.firecrawl.dev/v2/search"
      api_key: "${FIRECRAWL_API_KEY}"
```

---

以上方案可以直接交给 Rust 后端实现：先做配置模型和注册表，再按适配器逐个接入，最后补 API/UI 选择和安全策略。传统引擎用 `url_template` 快速覆盖，Tavily/Firecrawl/自定义引擎用专用适配器或 `custom_http` 接入。