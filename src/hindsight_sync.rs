//! Hindsight 远端记忆体同步层（Task #19 基础设施）。
//!
//! 通过既有 MCP 客户端（[`McpClientManager`]）编程式调用 Hindsight 的
//! retain / recall 工具，叠加超时保护与熔断器（Circuit Breaker），实现
//! “写入尽力而为、读取严格超时优雅降级”的远端记忆融合。
//!
//! 本模块只做基础设施：不直接接线到 agent 主循环，也不注册工具。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex as TokioMutex;
use tracing::{debug, warn};

use crate::config::HindsightConfig;
use crate::error::AgentResult;
use crate::tool::mcp_client::McpClientManager;

// ── Circuit Breaker ──────────────────────────────────────

/// 简易熔断器：连续失败达到阈值后在冷却窗口内直接跳过请求（OPEN），
/// 冷却结束后进入半开状态放行一次探测；任何一次成功都会复位计数。
pub struct CircuitBreaker {
    failures: AtomicU32,
    last_failure: Mutex<Option<Instant>>,
    threshold: u32,
    cooldown: Duration,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, cooldown_secs: u64) -> Self {
        Self {
            failures: AtomicU32::new(0),
            last_failure: Mutex::new(None),
            threshold,
            cooldown: Duration::from_secs(cooldown_secs),
        }
    }

    /// Returns true if circuit is OPEN (should skip requests)
    pub fn is_open(&self) -> bool {
        let count = self.failures.load(Ordering::Relaxed);
        if count < self.threshold {
            return false;
        }
        // Check if cooldown has elapsed (half-open state)
        if let Ok(guard) = self.last_failure.lock() {
            if let Some(last) = *guard {
                return last.elapsed() < self.cooldown;
            }
        }
        false // No timestamp means allow retry
    }

    pub fn record_success(&self) {
        self.failures.store(0, Ordering::Relaxed);
    }

    pub fn record_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.last_failure.lock() {
            *guard = Some(Instant::now());
        }
    }
}

// ── Recall Item ──────────────────────────────────────────

/// 一条召回结果，从 Hindsight recall 响应解析而来。
#[derive(Debug, Clone)]
pub struct RecallItem {
    pub text: String,
    pub fact_type: String,
    pub entities: Vec<String>,
    pub score: f64,
}

// ── HindsightSync ────────────────────────────────────────

/// Hindsight 同步器：持有共享的 MCP 管理器与可热更新的配置，
/// 对外提供 retain / recall / invalidate 三个操作。
pub struct HindsightSync {
    mcp: Arc<TokioMutex<McpClientManager>>,
    config: Arc<tokio::sync::RwLock<HindsightConfig>>,
    breaker: CircuitBreaker,
}

impl HindsightSync {
    pub fn new(mcp: Arc<TokioMutex<McpClientManager>>, config: HindsightConfig) -> Self {
        let breaker = CircuitBreaker::new(
            config.circuit_breaker_threshold,
            config.circuit_breaker_cooldown_s,
        );
        Self {
            mcp,
            config: Arc::new(tokio::sync::RwLock::new(config)),
            breaker,
        }
    }

    /// Update config (hot-reload from Settings)
    pub async fn update_config(&self, new_config: HindsightConfig) {
        let mut guard = self.config.write().await;
        *guard = new_config;
    }

    /// Check if sync is available (enabled + circuit closed + MCP connected)
    pub async fn is_available(&self) -> bool {
        let config = self.config.read().await;
        if !config.enabled {
            return false;
        }
        if self.breaker.is_open() {
            return false;
        }
        let mgr = self.mcp.lock().await;
        mgr.is_server_connected("hindsight")
    }

    /// Retain a memory to Hindsight (async, with timeout + circuit breaker)
    pub async fn retain(
        &self,
        content: &str,
        document_id: Option<&str>,
        tags: &[String],
    ) -> AgentResult<Value> {
        let config = self.config.read().await;
        if !config.enabled || !config.auto_sync_write {
            return Ok(Value::Null);
        }
        if self.breaker.is_open() {
            debug!("Hindsight circuit breaker open, skipping retain");
            return Ok(Value::Null);
        }

        let args = json!({
            "content": content,
            "document_id": document_id.unwrap_or(""),
            "tags": tags,
        });

        let timeout_dur = Duration::from_millis(config.write_timeout_ms);
        let mcp = self.mcp.clone();

        let result = tokio::time::timeout(timeout_dur, async {
            let mgr = mcp.lock().await;
            mgr.call_tool("hindsight", "retain", args).await
        })
        .await;

        match result {
            Ok(Ok(val)) => {
                self.breaker.record_success();
                Ok(val)
            }
            Ok(Err(e)) => {
                warn!("Hindsight retain failed: {}", e);
                self.breaker.record_failure();
                Err(e)
            }
            Err(_) => {
                warn!(
                    "Hindsight retain timed out after {}ms",
                    config.write_timeout_ms
                );
                self.breaker.record_failure();
                Err("Hindsight retain timed out".into())
            }
        }
    }

    /// Recall memories from Hindsight (strict timeout, graceful degradation).
    /// Only returns items whose relevance score >= `min_score` (default 0.4).
    pub async fn recall(&self, query: &str, max_tokens: u32) -> Vec<RecallItem> {
        let config = self.config.read().await;
        if !config.enabled || !config.auto_sync_read {
            return Vec::new();
        }
        if self.breaker.is_open() {
            debug!("Hindsight circuit breaker open, skipping recall");
            return Vec::new();
        }

        let args = json!({
            "query": query,
            "max_tokens": max_tokens,
        });

        let timeout_dur = Duration::from_millis(config.read_timeout_ms);
        let mcp = self.mcp.clone();

        let result = tokio::time::timeout(timeout_dur, async {
            let mgr = mcp.lock().await;
            mgr.call_tool("hindsight", "recall", args).await
        })
        .await;

        match result {
            Ok(Ok(val)) => {
                self.breaker.record_success();
                debug!("Hindsight recall raw response: {}", serde_json::to_string(&val).unwrap_or_default().chars().take(500).collect::<String>());
                let items = parse_recall_results(val);
                // 过滤低相关度结果（score < 0.4 视为噪声）
                let min_score = 0.4;
                let filtered: Vec<RecallItem> = items.into_iter()
                    .filter(|i| i.score >= min_score || i.score == 0.0 && i.fact_type != "unknown")
                    .take(5)
                    .collect();
                debug!("Hindsight recall: {} items after score filter (>= {})", filtered.len(), min_score);
                filtered
            }
            Ok(Err(e)) => {
                warn!("Hindsight recall failed: {}", e);
                self.breaker.record_failure();
                Vec::new()
            }
            Err(_) => {
                warn!(
                    "Hindsight recall timed out after {}ms",
                    config.read_timeout_ms
                );
                self.breaker.record_failure();
                Vec::new()
            }
        }
    }

    /// Invalidate/delete a remote memory
    pub async fn invalidate(&self, remote_id: &str) -> AgentResult<()> {
        let config = self.config.read().await;
        if !config.enabled || self.breaker.is_open() {
            return Ok(());
        }

        let args = json!({
            "memory_id": remote_id,
            "state": "invalidated",
        });

        let timeout_dur = Duration::from_millis(config.write_timeout_ms);
        let mcp = self.mcp.clone();

        let result = tokio::time::timeout(timeout_dur, async {
            let mgr = mcp.lock().await;
            // Hindsight MCP 实际工具名为 "invalidate_memory"
            if let Ok(val) = mgr.call_tool("hindsight", "invalidate_memory", args.clone()).await {
                return Ok(val);
            }
            // 回退：尝试 update_memory 设置状态
            mgr.call_tool("hindsight", "update_memory", args).await
        })
        .await;

        match result {
            Ok(Ok(_)) => {
                self.breaker.record_success();
                Ok(())
            }
            Ok(Err(e)) => {
                self.breaker.record_failure();
                Err(e)
            }
            Err(_) => {
                self.breaker.record_failure();
                Ok(())
            } // Silent on timeout
        }
    }

    /// 读取当前配置的快照（供 Settings GET 使用）。
    pub async fn get_config(&self) -> HindsightConfig {
        self.config.read().await.clone()
    }

    /// 写同步便捷方法：插入本地映射 → retain → 回填远端 id 与状态。
    ///
    /// 仅在 `enabled && auto_sync_write` 时生效；否则直接返回，不污染映射表。
    /// 设计为“尽力而为”：任何一步失败都不向上抛错，只落 `failed` 状态。
    pub async fn sync_fact(
        &self,
        mem: &Arc<crate::memory::MemoryStore>,
        local_id: &str,
        content: &str,
        document_id: Option<&str>,
        tags: &[String],
    ) {
        {
            let c = self.config.read().await;
            if !c.enabled || !c.auto_sync_write {
                return;
            }
        }
        let bank_id = {
            let c = self.config.read().await;
            c.bank_id.clone()
        };
        let _ = mem.hindsight_map_insert(local_id, &bank_id, document_id);
        match self.retain(content, document_id, tags).await {
            Ok(val) => {
                let remote_id = extract_remote_id(&val);
                let _ = mem.hindsight_map_update_remote(local_id, &remote_id, "synced");
            }
            Err(_) => {
                let _ = mem.hindsight_map_update_remote(local_id, "", "failed");
            }
        }
    }

    /// 遗忘同步：本地记忆被删除时，作废对应远端记忆并清理映射。
    pub async fn forget_fact(&self, mem: &Arc<crate::memory::MemoryStore>, local_id: &str) {
        if let Some(entry) = mem.hindsight_map_get(local_id) {
            if let Some(remote_id) = entry.remote_id.as_deref() {
                if !remote_id.is_empty() {
                    let _ = self.invalidate(remote_id).await;
                }
            }
        }
        let _ = mem.hindsight_map_remove(local_id);
    }
}

/// 从 retain 返回值中尽力提取远端记忆 id。
///
/// Hindsight 的 MCP 返回可能是 content 数组（`[{type:text,text:"<json>"}]`）、
/// 直接对象，或嵌套在 data/result/memory/fact 下。递归查找常见 id 字段。
pub fn extract_remote_id(val: &Value) -> String {
    const KEYS: [&str; 4] = ["id", "memory_id", "fact_id", "uuid"];
    if let Some(arr) = val.as_array() {
        let text = arr
            .iter()
            .filter_map(|i| {
                if i.get("type").and_then(|t| t.as_str()) == Some("text") {
                    i.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
            return extract_remote_id(&parsed);
        }
        return String::new();
    }
    if let Some(obj) = val.as_object() {
        for k in KEYS {
            if let Some(s) = obj.get(k).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        for k in ["data", "result", "memory", "fact"] {
            if let Some(inner) = obj.get(k) {
                let r = extract_remote_id(inner);
                if !r.is_empty() {
                    return r;
                }
            }
        }
    }
    if let Some(s) = val.as_str() {
        return s.to_string();
    }
    String::new()
}

/// Parse recall results from MCP tool response into RecallItems
fn parse_recall_results(val: Value) -> Vec<RecallItem> {
    // MCP tool returns content array; extract text content and parse as JSON
    let text = if let Some(arr) = val.as_array() {
        arr.iter()
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        val.to_string()
    };

    // Try to parse as JSON array of recall results
    if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
        if let Some(results) = parsed.as_array() {
            return results
                .iter()
                .filter_map(|item| {
                    Some(RecallItem {
                        text: item.get("text")?.as_str()?.to_string(),
                        fact_type: item
                            .get("type")
                            .or_else(|| item.get("fact_type"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        entities: item
                            .get("entities")
                            .and_then(|e| e.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        score: item
                            .get("scores")
                            .and_then(|s| s.get("final"))
                            .and_then(|f| f.as_f64())
                            .or_else(|| item.get("score").and_then(|s| s.as_f64()))
                            .unwrap_or(0.0),
                    })
                })
                .collect();
        }
        // Single object
        if let Some(item) = parsed.as_object() {
            if let Some(results) = item
                .get("results")
                .or_else(|| item.get("memories"))
                .and_then(|r| r.as_array())
            {
                return results
                    .iter()
                    .filter_map(|r| {
                        Some(RecallItem {
                            text: r.get("text")?.as_str()?.to_string(),
                            fact_type: r
                                .get("type")
                                .and_then(|t| t.as_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            entities: Vec::new(),
                            score: r.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0),
                        })
                    })
                    .collect();
            }
        }
    }

    // Fallback: treat entire text as one item
    if !text.is_empty() {
        vec![RecallItem {
            text,
            fact_type: "unknown".to_string(),
            entities: Vec::new(),
            score: 0.0,
        }]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_breaker_opens_after_threshold_and_resets_on_success() {
        let cb = CircuitBreaker::new(2, 60);
        assert!(!cb.is_open(), "fresh breaker must be closed");
        cb.record_failure();
        assert!(!cb.is_open(), "below threshold stays closed");
        cb.record_failure();
        assert!(cb.is_open(), "reaching threshold opens the circuit");
        cb.record_success();
        assert!(!cb.is_open(), "a success resets the breaker");
    }

    #[test]
    fn parse_recall_results_handles_content_array() {
        let payload = json!([
            {
                "type": "text",
                "text": "[{\"text\":\"fact one\",\"type\":\"preference\",\"score\":0.9}]"
            }
        ]);
        let items = parse_recall_results(payload);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "fact one");
        assert_eq!(items[0].fact_type, "preference");
        assert!((items[0].score - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_recall_results_falls_back_to_plain_text() {
        let payload = json!([{ "type": "text", "text": "just a memory" }]);
        let items = parse_recall_results(payload);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "just a memory");
        assert_eq!(items[0].fact_type, "unknown");
    }
}
