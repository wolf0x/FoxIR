use std::sync::Arc;
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::ModelConfig;
use crate::error::{AgentError, AgentResult};
use crate::model::{
    ChatMessage, FunctionCallDelta, Llm, LlmRequest, LlmResponse,
    LlmResponseStream, ToolCallDelta, ToolDefinition,
};

/// OpenAI-compatible LLM provider.
/// Implements the Llm trait (modeled after ADK-RUST's OpenAIClient).
pub struct OpenAiProvider {
    client: Client,
    models: Arc<tokio::sync::RwLock<Vec<ModelConfig>>>,
}

// --- Internal streaming types ---

/// Returns the correct JSON key for the max output tokens parameter.
/// Newer OpenAI models (GPT-5, o1, o3, o4) require `max_completion_tokens`
/// instead of the legacy `max_tokens`. All other OpenAI-compatible models
/// (DeepSeek, Qwen, GPT-4, etc.) continue using `max_tokens`.
fn max_tokens_key(model_name: &str) -> &'static str {
    let lower = model_name.to_lowercase();
    if lower.starts_with("gpt-5")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Option<Vec<StreamChoice>>,
    usage: Option<RawUsage>,
}

#[derive(Debug, Deserialize)]
struct RawUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    /// OpenAI-style nested accounting: `usage.prompt_tokens_details.cached_tokens`.
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// DeepSeek / Moonshot-style flat accounting.
    prompt_cache_hit_tokens: Option<u64>,
    prompt_cache_miss_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u64>,
}

impl RawUsage {
    /// Input tokens served from the provider's prompt cache (a cache HIT), or
    /// `None` when this endpoint reports no cache accounting at all. The two are
    /// deliberately distinct: `Some(0)` is a measured miss, `None` must be kept
    /// out of the denominator so an unsupported model cannot look like a broken
    /// cache. Field names differ per vendor, hence the fallback ladder.
    fn cached_prompt_tokens(&self) -> Option<u64> {
        if let Some(v) = self.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens) {
            return Some(v);
        }
        if let Some(v) = self.prompt_cache_hit_tokens {
            return Some(v);
        }
        // A few gateways expose only the miss counter; derive the hit from it.
        match (self.prompt_tokens, self.prompt_cache_miss_tokens) {
            (Some(p), Some(miss)) => Some(p.saturating_sub(miss)),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: DeltaContent,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeltaContent {
    #[allow(dead_code)]
    role: Option<String>,
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ToolCallChunk>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallChunk {
    /// `index` is optional in practice: several OpenAI-compatible gateways omit it.
    /// A missing index must therefore never default to slot 0, or parallel tool
    /// calls in one turn collapse into a single call with concatenated names and
    /// arguments. See `ToolCallAccumulator::slot_for`.
    index: Option<usize>,
    id: Option<String>,
    function: Option<FunctionChunk>,
}

#[derive(Debug, Deserialize)]
struct FunctionChunk {
    name: Option<String>,
    arguments: Option<String>,
}

impl OpenAiProvider {
    pub fn new(models: Vec<ModelConfig>) -> Self {
        Self::new_with_shared_timeouts(
            Arc::new(tokio::sync::RwLock::new(models)),
            super::DEFAULT_LLM_READ_TIMEOUT_SECS,
            super::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
        )
    }

    pub fn new_with_shared(models: Arc<tokio::sync::RwLock<Vec<ModelConfig>>>) -> Self {
        Self::new_with_shared_timeouts(
            models,
            super::DEFAULT_LLM_READ_TIMEOUT_SECS,
            super::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
        )
    }

    /// 构建 provider，显式指定 LLM 请求的超时语义。
    ///
    /// 关键：**不使用 reqwest 的 `ClientBuilder::timeout`（total deadline）**。
    /// 该语义是「整轮请求从建连到 body 读完的最长时限」。实测确认（见
    /// output/timeout-verify/）：长响应即使持续稳定输出，只要总时长超过该值就会被硬性切断，
    /// 只留下开头若干字符，最终被误判为「模型给出了短回答」而静默收尾。
    ///
    /// 改用两段独立超时：
    /// - `connect_timeout`：连接阶段保护（未设 total deadline 时必需，否则建连可无限挂起）。
    /// - `read_timeout`：读间隔上限，每次成功读取即重置，只在中途真正静默时触发。
    pub fn new_with_shared_timeouts(
        models: Arc<tokio::sync::RwLock<Vec<ModelConfig>>>,
        read_timeout_secs: u64,
        connect_timeout_secs: u64,
    ) -> Self {
        let insecure = std::env::var("RUST_AGENT_INSECURE_TLS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let client = Client::builder()
            .danger_accept_invalid_certs(insecure)
            .read_timeout(std::time::Duration::from_secs(read_timeout_secs.max(1)))
            .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs.max(1)))
            .build()
            .expect("Failed to create HTTP client");
        if insecure {
            warn!("TLS certificate verification is DISABLED (RUST_AGENT_INSECURE_TLS=1)");
        }
        Self { client, models }
    }

    pub fn models_ref(&self) -> Arc<tokio::sync::RwLock<Vec<ModelConfig>>> {
        self.models.clone()
    }

    async fn find_model(&self, name: &str) -> Option<ModelConfig> {
        let models = self.models.read().await;
        models.iter().find(|m| m.name == name).cloned().or_else(|| models.first().cloned())
    }

    /// Quick connectivity test for a configured model/provider. Sends a tiny
    /// non-streaming chat request and reports latency + a short reply.
    pub async fn test_connection(&self, model_name: &str) -> Result<(u64, String), String> {
        let model = self.find_model(model_name).await
            .ok_or_else(|| format!("No model configured with name '{model_name}'"))?;
        self.test_connection_for(&model).await
    }
    /// Test connectivity for an already-resolved provider config (by ID).
    pub async fn test_connection_for(&self, model: &ModelConfig) -> Result<(u64, String), String> {
        let api_key = model.resolved_api_key();
        let url = format!("{}/chat/completions", model.api_base.trim_end_matches('/'));

        let mut body = serde_json::json!({
            "model": model.name,
            "messages": [{"role": "user", "content": "Reply with a single word: pong"}],
            "stream": false,
            "temperature": 0.0,
        });
        body[max_tokens_key(&model.name)] = serde_json::json!(16u32);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| format!("failed to build http client: {e}"))?;

        let mut req = client.post(&url).header("Content-Type", "application/json");
        if !api_key.is_empty() {
            req = req.bearer_auth(&api_key);
        }

        let start = std::time::Instant::now();
        let resp = req.json(&body).send().await
            .map_err(|e| format!("connection failed: {e}"))?;
        let status = resp.status();
        let latency_ms = start.elapsed().as_millis() as u64;
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            let preview: String = text.chars().take(300).collect();
            return Err(format!("HTTP {status}: {preview}"));
        }
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("bad response JSON: {e}"))?;
        let content = parsed["choices"][0]["message"]["content"]
            .as_str().unwrap_or("").trim().to_string();
        let reply = if content.is_empty() {
            "(empty reply)".to_string()
        } else {
            content.chars().take(120).collect()
        };
        Ok((latency_ms, reply))
    }

    /// Non-streaming LLM call for lightweight tasks (e.g. knowledge distillation).

    /// Returns the assistant's text content directly. No tool support, lower token limit.
    pub async fn chat_simple(
        &self,
        model_name: &str,
        messages: &[ChatMessage],
    ) -> Result<String, String> {
        let model = self.find_model(model_name).await.ok_or("No model configured")?;
        let api_key = model.resolved_api_key();
        let url = format!("{}/chat/completions", model.api_base.trim_end_matches('/'));

        let mut body = serde_json::json!({
            "model": model.name,
            "messages": messages,
            "stream": false,
            "temperature": 0.3,
        });
        body[max_tokens_key(&model.name)] = serde_json::json!(4096u32);

        let mut req = self.client.post(&url).header("Content-Type", "application/json");
        if !api_key.is_empty() {
            req = req.bearer_auth(&api_key);
        }

        let resp = req.json(&body).send().await
            .map_err(|e| format!("LLM request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("LLM error {}: {}", status, err_body));
        }

        let parsed: serde_json::Value = resp.json().await
            .map_err(|e| format!("Failed to parse LLM response: {}", e))?;

        let content = parsed["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_default();

        if content.is_empty() {
            return Err("LLM returned empty content".to_string());
        }

        Ok(content)
    }

    /// Legacy chat_stream method for backward compat (used by agent loop internally).
    ///
    /// Sends text deltas through an mpsc channel. 返回 6 元组：
    /// (content, reasoning, tool_calls, usage, finish_reason, stream_timed_out)。
    /// stream_timed_out 是传输层标志（与 consumer_gone 同级）：为 true 时表示流在
    /// 读完之前被传输错误（读超时或中途断连）切断，content 是残缺前缀。调用方据此决定补救方式；该标志
    /// 不改变 finish_reason 的含义。
    pub async fn chat_stream(
        &self,
        model_name: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tx: mpsc::Sender<AgentResult<crate::agent::AgentEvent>>,
        invocation_id: &str,
        author: &str,
    ) -> Result<(String, String, Vec<ToolCallDelta>, Option<crate::model::UsageMetadata>, Option<String>, bool), String> {
        let model = self.find_model(model_name).await.ok_or("No model configured")?;
        let api_key = model.resolved_api_key();
        let url = format!("{}/chat/completions", model.api_base.trim_end_matches('/'));

        let mut body = serde_json::json!({
            "model": model.name,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
            "temperature": model.temperature,
        });
        body[max_tokens_key(&model.name)] = serde_json::json!(model.max_tokens);
        if !tools.is_empty() {
            body["tools"] = serde_json::to_value(tools).unwrap();
            body["tool_choice"] = serde_json::json!("auto");
        }

        let mut req = self.client.post(&url).header("Content-Type", "application/json");
        if !api_key.is_empty() {
            req = req.bearer_auth(&api_key);
        }

        let resp = req.json(&body).send().await
            .map_err(|e| format!("LLM request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("LLM error {}: {}", status, err_body));
        }

        let mut s = resp.bytes_stream();
        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let mut tc_accum = ToolCallAccumulator::default();
        let mut byte_buf: Vec<u8> = Vec::new();
        let mut captured_usage: Option<crate::model::UsageMetadata> = None;
        let mut finish_reason: Option<String> = None;
        // 服务端已用终止哨兵明确结束本次应答；此后即使连接未立即关闭、读超时随后触发，也不属于截断。
        let mut saw_done = false;

        // If the consumer (agent stream / WebSocket) drops the receiver, there is
        // no point continuing to read the HTTP stream. We watch for that with a
        // flag and abort the loops as soon as a send fails, instead of spamming
        // one warning per remaining chunk.
        let mut consumer_gone = false;
        // 传输层截断标志。与 consumer_gone 同级：只标记"这是截断"而非"正常结束"，
        // 仅当流尚未送出终止信号时才置位，由主循环决定补救方式，不改动 finish_reason 的含义。
        let mut stream_timed_out = false;

        'outer: while let Some(chunk_result) = s.next().await {
            let chunk_bytes = match chunk_result {
                Ok(b) => b,
                Err(e) => {
                    // 分类传输层错误。是否截断的唯一判据是「流有没有送出终止信号」：
                    // 终止信号之前的任何读取错误都只留下残缺前缀，必须交给主循环补救；
                    // 终止信号之后的错误属于连接延迟关闭，应答已完整，按正常结束处理。
                    //
                    // 注意：reqwest 将 body 超时包装成 Kind::Decode，Display 为
                    // "error decoding response body"，同时 is_decode() 也为 true。
                    // 读超时因此必须用 is_timeout() 判定，否则会把真正的 serde 解析失败一并收纳。
                    if e.is_timeout() {
                        // 读超时本身不是截断的充分条件：应答可能已经完整送出，只是连接延迟关闭。
                        if saw_done || finish_reason.is_some() {
                            debug!("Stream read timeout after the stream was terminated (done={}, finish_reason={:?}); response is already complete", saw_done, finish_reason);
                        } else {
                            warn!("Stream read timeout before finish_reason: {}", e);
                            stream_timed_out = true;
                        }
                    }
                    // 非超时的中途断连（连接被重置、被代理掐断、服务端重启等）同样只会留下残缺前缀。
                    // 终止信号送出之前的任何传输错误都算截断，不能只认读超时。
                    else if !saw_done && finish_reason.is_none() {
                        warn!("Stream cut before finish_reason by a transport error: {}", e);
                        stream_timed_out = true;
                    } else {
                        warn!("Stream chunk error: {}", e);
                    }
                    break;
                }
            };
            byte_buf.extend_from_slice(&chunk_bytes);

            // Process complete lines (delimited by \n = 0x0A) from the byte buffer.
            // This avoids corrupting multi-byte UTF-8 characters split across chunks.
            while let Some(pos) = byte_buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = byte_buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();

                if line.is_empty() { continue; }
                if line == "data: [DONE]" { saw_done = true; continue; }

                if let Some(data) = line.strip_prefix("data: ") {
                    match serde_json::from_str::<StreamChunk>(data) {
                        Ok(chunk) => {
                            if let Some(choices) = chunk.choices {
                                for choice in choices {
                                    if let Some(fr) = &choice.finish_reason {
                                        finish_reason = Some(fr.clone());
                                    }
                                    // Handle reasoning_content (thinking phase for DeepSeek V4 etc.)
                                    if let Some(reasoning) = &choice.delta.reasoning_content {
                                        full_reasoning.push_str(reasoning);
                                        if tx.send(
                                            Ok(crate::agent::AgentEvent::thinking(reasoning, invocation_id, author))
                                        ).await.is_err() {
                                            consumer_gone = true;
                                            break 'outer;
                                        }
                                    }
                                    // Handle content (actual response)
                                    if let Some(content) = &choice.delta.content {
                                        if !content.is_empty() {
                                            full_content.push_str(content);
                                            if tx.send(
                                                Ok(crate::agent::AgentEvent::text(content, invocation_id, author))
                                            ).await.is_err() {
                                                consumer_gone = true;
                                                break 'outer;
                                            }
                                        }
                                    }
                                    if let Some(tcs) = &choice.delta.tool_calls {
                                        for tc in tcs {
                                            tc_accum.absorb(tc);
                                        }
                                    }
                                }
                            }
                            // Capture usage from the last chunk (stream_options.include_usage=true)
                            if let Some(ref raw) = chunk.usage {
                                captured_usage = Some(crate::model::UsageMetadata {
                                    prompt_tokens: raw.prompt_tokens,
                                    completion_tokens: raw.completion_tokens,
                                    total_tokens: raw.total_tokens,
                                    cached_prompt_tokens: raw.cached_prompt_tokens(),
                                });
                            }
                        }
                        Err(e) => { debug!("Failed to parse chunk: {} | data: {}", e, data); }
                    }
                }
            }
        }

        if consumer_gone {
            debug!("LLM stream aborted because the client disconnected or stopped the session");
        }

        let tool_calls = tc_accum.finish();

        Ok((full_content, full_reasoning, tool_calls, captured_usage, finish_reason, stream_timed_out))
    }
}

#[async_trait]
impl Llm for OpenAiProvider {
    fn name(&self) -> &str { "openai-compatible" }

    async fn generate_content(
        &self,
        request: LlmRequest,
        stream: bool,
    ) -> AgentResult<LlmResponseStream> {
        let model = self.find_model(&request.model).await
            .ok_or_else(|| AgentError::model(format!("Model '{}' not found", request.model)))?;
        let api_key = model.resolved_api_key();
        let url = format!("{}/chat/completions", model.api_base.trim_end_matches('/'));

        let mut body = serde_json::json!({
            "model": model.name,
            "messages": request.messages,
            "stream": stream,
        });
        if !request.tools.is_empty() {
            body["tools"] = serde_json::to_value(&request.tools).unwrap();
        }
        if let Some(temp) = request.config.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.config.max_tokens {
            body[max_tokens_key(&model.name)] = serde_json::json!(max);
        }

        let mut req = self.client.post(&url).header("Content-Type", "application/json");
        if !api_key.is_empty() {
            req = req.bearer_auth(&api_key);
        }

        let resp = req.json(&body).send().await
            .map_err(|e| AgentError::model(format!("Request failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(AgentError::model(format!("{}: {}", status, err_body)));
        }

        if !stream {
            // Non-streaming: read full response
            let text = resp.text().await.map_err(|e| AgentError::model(e.to_string()))?;
            let parsed: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| AgentError::model(format!("Parse: {}", e)))?;

            let content = parsed["choices"][0]["message"]["content"]
                .as_str().map(|s| s.to_string());

            // Parse tool_calls from non-streamed response
            let tool_calls: Vec<ToolCallDelta> = parsed["choices"][0]["message"]["tool_calls"]
                .as_array()
                .map(|arr| {
                    arr.iter().filter_map(|tc| {
                        let id = tc["id"].as_str().unwrap_or_default().to_string();
                        let name = tc["function"]["name"].as_str().unwrap_or_default().to_string();
                        let arguments = tc["function"]["arguments"].as_str().unwrap_or_default().to_string();
                        if name.is_empty() { return None; }
                        Some(ToolCallDelta {
                            id,
                            call_type: "function".to_string(),
                            function: FunctionCallDelta {
                                name: Some(name),
                                arguments: Some(arguments),
                            },
                        })
                    }).collect()
                })
                .unwrap_or_default();

            let response = LlmResponse {
                content,
                tool_calls,
                finish_reason: parsed["choices"][0]["finish_reason"].as_str().map(|s| s.to_string()),
                usage: None,
            };
            return Ok(Box::pin(stream::once(async move { Ok(response) })));
        }

        // Streaming: return a stream that parses SSE chunks
        let byte_stream = resp.bytes_stream();

        let parsed_stream = async_stream::stream! {
            let mut byte_buf: Vec<u8> = Vec::new();
            let mut tc_accum = ToolCallAccumulator::default();
            let mut accumulated_content = String::new();
            let mut accumulated_reasoning = String::new();
            let mut finish_reason: Option<String> = None;
            let mut captured_usage: Option<crate::model::UsageMetadata> = None;

            tokio::pin!(byte_stream);
            while let Some(chunk_result) = byte_stream.next().await {
                let chunk_bytes = match chunk_result {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(AgentError::model(format!("Stream error: {}", e)));
                        return;
                    }
                };
                byte_buf.extend_from_slice(&chunk_bytes);

                while let Some(pos) = byte_buf.iter().position(|&b| b == b'\n') {
                    let line_bytes: Vec<u8> = byte_buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line_bytes);
                    let line = line.trim();

                    if line.is_empty() || line == "data: [DONE]" { continue; }

                    if let Some(data) = line.strip_prefix("data: ") {
                        match serde_json::from_str::<StreamChunk>(data) {
                            Ok(chunk) => {
                                // Capture usage from final chunk (stream_options.include_usage=true)
                                if let Some(ref raw) = chunk.usage {
                                    captured_usage = Some(crate::model::UsageMetadata {
                                        prompt_tokens: raw.prompt_tokens,
                                        completion_tokens: raw.completion_tokens,
                                        total_tokens: raw.total_tokens,
                                        cached_prompt_tokens: raw.cached_prompt_tokens(),
                                    });
                                }
                                if let Some(choices) = chunk.choices {
                                    for choice in choices {
                                        if let Some(fr) = &choice.finish_reason {
                                            finish_reason = Some(fr.clone());
                                        }
                                        // Accumulate reasoning_content (thinking phase)
                                        if let Some(reasoning) = &choice.delta.reasoning_content {
                                            accumulated_reasoning.push_str(reasoning);
                                        }
                                        // Accumulate content (actual response)
                                        if let Some(content) = &choice.delta.content {
                                            accumulated_content.push_str(content);
                                        }
                                        if let Some(tcs) = &choice.delta.tool_calls {
                                            for tc in tcs {
                                                tc_accum.absorb(tc);
                                            }
                                        }
                                    }
                                }
                            }
                            Err(_) => { /* skip unparseable chunks */ }
                        }
                    }
                }
            }

            // Emit final response with accumulated data
            let tool_calls = tc_accum.finish();

            yield Ok(LlmResponse {
                content: if accumulated_content.is_empty() { None } else { Some(accumulated_content) },
                tool_calls,
                finish_reason,
                usage: captured_usage,
            });
        };

        Ok(Box::pin(parsed_stream))
    }

    fn available_models(&self) -> Vec<String> {
        self.models.try_read()
            .map(|m| m.iter().map(|mc| mc.name.clone()).collect())
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
    /// Set once this call's arguments hit `MAX_ARGS_BYTES`, so the warning fires
    /// once per call instead of once per dropped fragment.
    args_capped: bool,
}

/// Slots streaming `tool_calls` fragments into whole calls.
///
/// The wire gives no ordering guarantee for parallel calls, only the `index`
/// key, and that key is missing on some compatible gateways. Attribution
/// therefore falls through a ladder rather than trusting one field.
#[derive(Default)]
struct ToolCallAccumulator {
    calls: Vec<ToolCallAccum>,
}

impl ToolCallAccumulator {
    /// Calls beyond this are dropped. Bounds allocation against a malformed
    /// `index` (or an endpoint inventing call ids forever).
    const MAX_CALLS: usize = 128;
    /// Argument bytes retained per call. Bounds a stream that never stops
    /// emitting `arguments` for one call.
    const MAX_ARGS_BYTES: usize = 2 * 1024 * 1024;

    fn slot_for(&mut self, tc: &ToolCallChunk) -> Option<usize> {
        // 1. An explicit index wins.
        if let Some(idx) = tc.index {
            if idx >= Self::MAX_CALLS {
                tracing::warn!(
                    "Discarding tool-call fragment with out-of-range index {} (cap {})",
                    idx,
                    Self::MAX_CALLS
                );
                return None;
            }
            while self.calls.len() <= idx {
                self.calls.push(ToolCallAccum::default());
            }
            return Some(idx);
        }

        // 2. A call id: continue the call carrying it, else start a new one.
        if let Some(id) = tc.id.as_deref().filter(|s| !s.is_empty()) {
            if let Some(pos) = self.calls.iter().position(|c| c.id == id) {
                return Some(pos);
            }
            return self.open_new();
        }

        // 3. Neither index nor id. OpenAI delivers `name` whole in a call's first
        // fragment, so a second name while the last call already has one means a
        // second call; anything else continues the last one.
        let carries_name = tc
            .function
            .as_ref()
            .and_then(|f| f.name.as_deref())
            .is_some_and(|s| !s.is_empty());
        let last_has_name = self.calls.last().is_some_and(|c| !c.name.is_empty());
        if carries_name && last_has_name {
            return self.open_new();
        }
        match self.calls.len().checked_sub(1) {
            Some(last) => Some(last),
            None => self.open_new(),
        }
    }

    fn open_new(&mut self) -> Option<usize> {
        if self.calls.len() >= Self::MAX_CALLS {
            tracing::warn!(
                "Tool-call stream exceeded {} distinct calls; dropping further fragments",
                Self::MAX_CALLS
            );
            return None;
        }
        self.calls.push(ToolCallAccum::default());
        Some(self.calls.len() - 1)
    }

    fn absorb(&mut self, tc: &ToolCallChunk) {
        let Some(slot) = self.slot_for(tc) else { return };
        let call = &mut self.calls[slot];
        if let Some(id) = tc.id.as_deref().filter(|s| !s.is_empty()) {
            call.id = id.to_string();
        }
        let Some(func) = tc.function.as_ref() else { return };
        if let Some(name) = func.name.as_deref() {
            call.name.push_str(name);
        }
        if let Some(args) = func.arguments.as_deref() {
            if call.arguments.len() + args.len() > Self::MAX_ARGS_BYTES {
                if !call.args_capped {
                    tracing::warn!(
                        "Tool call '{}' arguments exceeded {} bytes; truncating the rest of the stream for this call",
                        if call.name.is_empty() { "<unnamed>" } else { &call.name },
                        Self::MAX_ARGS_BYTES
                    );
                    call.args_capped = true;
                }
                let room = Self::MAX_ARGS_BYTES.saturating_sub(call.arguments.len());
                // Cut on a char boundary so the kept prefix stays valid UTF-8.
                let mut take = room;
                while take > 0 && !args.is_char_boundary(take) {
                    take -= 1;
                }
                call.arguments.push_str(&args[..take]);
            } else {
                call.arguments.push_str(args);
            }
        }
    }

    fn finish(mut self) -> Vec<ToolCallDelta> {
        let mut synthetic_id_counter = 0u32;
        self.calls
            .iter_mut()
            .filter(|tc| !tc.name.is_empty())
            .map(|tc| {
                let id = if tc.id.is_empty() {
                    let sid = format!("tc_synthetic_{}", synthetic_id_counter);
                    synthetic_id_counter += 1;
                    debug!(
                        "Tool call '{}' missing ID from API, generated synthetic ID: {}",
                        tc.name, sid
                    );
                    sid
                } else {
                    std::mem::take(&mut tc.id)
                };
                ToolCallDelta {
                    id,
                    call_type: "function".to_string(),
                    function: FunctionCallDelta {
                        name: Some(std::mem::take(&mut tc.name)),
                        arguments: Some(std::mem::take(&mut tc.arguments)),
                    },
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod stream_timeout_guard {
    use super::*;
    use crate::config::ModelConfig;
    use std::sync::Arc;

    // Minimal SSE stub: writes one chunk of a streaming response, then stalls for a long
    // time. The response is chunked and has no Content-Length, matching a real streaming
    // LLM endpoint, so the cut happens inside the body rather than at connect time.
    async fn spawn_stalling_sse_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            let _ = sock.write_all(headers.as_bytes()).await;
            let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"The\"}}]}\n\n";
            let frame = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
            let _ = sock.write_all(frame.as_bytes()).await;
            let _ = sock.flush().await;
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        format!("http://{}", addr)
    }

    fn model_for(api_base: String) -> ModelConfig {
        ModelConfig {
            title: "stub".into(),
            name: "stub-model".into(),
            api_base,
            api_key: None,
            api_key_env: None,
            context_window: 4096,
            max_tokens: 64,
            temperature: 0.0,
            supports_vision: false,
        }
    }

    #[tokio::test]
    async fn read_timeout_marks_stream_timed_out() {
        let api_base = spawn_stalling_sse_server().await;
        let provider = OpenAiProvider::new_with_shared_timeouts(
            Arc::new(tokio::sync::RwLock::new(vec![model_for(api_base)])),
            1,
            5,
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentResult<crate::agent::AgentEvent>>(8);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let started = std::time::Instant::now();
        let res = provider
            .chat_stream("stub-model", &[ChatMessage::user("hi")], &[], tx, "inv", "test")
            .await
            .expect("a mid-stream timeout must surface as a result flag, not an Err");

        let (content, _reasoning, _tool_calls, _usage, _finish_reason, stream_timed_out) = res;
        assert!(stream_timed_out, "mid-stream silence must set stream_timed_out");
        assert_eq!(content, "The", "the prefix received before the cut must be preserved");
        assert!(started.elapsed() < std::time::Duration::from_secs(30), "must not wait out the 60s stall");
    }
}

#[cfg(test)]
mod stream_cut_after_termination {
    use super::*;
    use crate::config::ModelConfig;
    use std::sync::Arc;

    async fn spawn_complete_then_stall() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            let _ = sock.write_all(headers.as_bytes()).await;
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Final answer \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"is done. \"}, \"finish_reason\":\"stop\"}]}\n\n"
            );
            let frame = format!("{:x}\r\n{}\r\n", body.len(), body);
            let _ = sock.write_all(frame.as_bytes()).await;
            let _ = sock.flush().await;
            tokio::time::sleep(std::time::Duration::from_secs(99)).await;
        });
        format!("http://{}", addr)
    }

    fn model_for(api_base: String) -> ModelConfig {
        ModelConfig {
            title: "stub".into(),
            name: "stub-model".into(),
            api_base,
            api_key: None,
            api_key_env: None,
            context_window: 4096,
            max_tokens: 64,
            temperature: 0.0,
            supports_vision: false,
        }
    }

    #[tokio::test]
    async fn read_timeout_after_terminal_finish_reason_is_not_truncation() {
        let api_base = spawn_complete_then_stall().await;
        let provider = OpenAiProvider::new_with_shared_timeouts(
            Arc::new(tokio::sync::RwLock::new(vec![model_for(api_base)])),
            1,
            5,
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentResult<crate::agent::AgentEvent>>(8);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let res = provider
            .chat_stream("stub-model", &[ChatMessage::user("hi")], &[], tx, "inv", "test")
            .await
            .expect("call");
        let (content, _reasoning, _tool_calls, _usage, finish_reason, stream_timed_out) = res;
        // 应答已经带终止 finish_reason，读超时随后才触发；这属于服务端延迟关闭连接，
        // 不是截断。若误判为截断，主循环会把一个完整回答再补一轮“继续输出”。
        assert_eq!(content, "Final answer is done. ", "content before the stall must be preserved in full");
        assert_eq!(finish_reason.as_deref(), Some("stop"));
        assert!(!stream_timed_out, "a timeout after the terminal finish_reason is not a truncation");
    }
}

#[cfg(test)]
mod stream_cut_by_transport_error {
    use super::*;
    use crate::config::ModelConfig;
    use std::sync::Arc;

    // Server writes one SSE chunk, then closes the TCP connection WITHOUT the chunked
    // terminator. That surfaces as a body error that is not a timeout.
    async fn spawn_abrupt_close() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            let _ = sock.write_all(headers.as_bytes()).await;
            let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"The\"}}]}\n\n";
            let frame = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
            let _ = sock.write_all(frame.as_bytes()).await;
            let _ = sock.flush().await;
            // Drop without writing the 0-length terminator frame.
            drop(sock);
        });
        format!("http://{}", addr)
    }

    fn model_for(api_base: String) -> ModelConfig {
        ModelConfig { title: "stub".into(), name: "stub-model".into(), api_base,
            api_key: None, api_key_env: None, context_window: 4096, max_tokens: 64,
            temperature: 0.0, supports_vision: false }
    }

    #[tokio::test]
    async fn abrupt_close_mid_body_is_marked_truncated() {
        let api_base = spawn_abrupt_close().await;
        let provider = OpenAiProvider::new_with_shared_timeouts(
            Arc::new(tokio::sync::RwLock::new(vec![model_for(api_base)])), 3, 5);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentResult<crate::agent::AgentEvent>>(8);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let res = provider.chat_stream("stub-model", &[ChatMessage::user("hi")], &[], tx, "inv", "test").await;
        let (content, _reasoning, _tool_calls, _usage, finish_reason, stream_timed_out) = res
            .expect("a transport cut must surface as a result flag, not an Err");
        // 非超时的中途断连同样只留下残缺前缀，必须标记为截断；
        // 否则主循环会把 "The" 当成完整回答收尾。
        assert!(stream_timed_out, "a transport error before the finish_reason must set stream_timed_out");
        assert_eq!(content, "The", "the prefix received before the cut must be preserved");
        assert!(finish_reason.is_none(), "no finish_reason was sent in this scenario");
    }
}

#[cfg(test)]
mod usage_cache_parsing {
    use super::*;

    fn parse(s: &str) -> RawUsage {
        serde_json::from_str::<RawUsage>(s).expect("usage object must parse")
    }

    /// Vendor field names for the same quantity differ; all three known shapes
    /// must land in one field, and "not reported" must stay distinguishable from
    /// "reported a miss".
    #[test]
    fn cache_fields_from_every_vendor_shape() {
        // OpenAI-compatible nested details.
        let openai = parse(r#"{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050,
                                 "prompt_tokens_details":{"cached_tokens":800}}"#);
        assert_eq!(openai.cached_prompt_tokens(), Some(800));

        // DeepSeek / Moonshot flat counters.
        let flat = parse(r#"{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050,
                             "prompt_cache_hit_tokens":640,"prompt_cache_miss_tokens":360}"#);
        assert_eq!(flat.cached_prompt_tokens(), Some(640));

        // Only a miss counter: derive the hit from the input total.
        let miss_only = parse(r#"{"prompt_tokens":1000,"prompt_cache_miss_tokens":250}"#);
        assert_eq!(miss_only.cached_prompt_tokens(), Some(750));

        // Explicit zero is a measured miss, NOT a silence.
        let zero = parse(r#"{"prompt_tokens":1000,"prompt_tokens_details":{"cached_tokens":0}}"#);
        assert_eq!(zero.cached_prompt_tokens(), Some(0));

        // Silence: no cache fields at all -> None (must be excluded upstream).
        let blind = parse(r#"{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050}"#);
        assert_eq!(blind.cached_prompt_tokens(), None);

        // A miss counter larger than the input total cannot invent a negative hit.
        let weird = parse(r#"{"prompt_tokens":100,"prompt_cache_miss_tokens":180}"#);
        assert_eq!(weird.cached_prompt_tokens(), Some(0));
    }

    /// Unknown extra fields from a gateway must not break usage capture.
    #[test]
    fn unknown_usage_fields_are_ignored() {
        let u = parse(r#"{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,
                          "reasoning_tokens":2,"serving_cost":0.001}"#);
        assert_eq!(u.prompt_tokens, Some(10));
        assert_eq!(u.cached_prompt_tokens(), None);
    }
}

#[cfg(test)]
mod tool_call_slot_attribution {
    use super::*;
    use crate::config::ModelConfig;
    use std::sync::Arc;

    fn frag(index: Option<usize>, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> ToolCallChunk {
        let function = (name.is_some() || args.is_some()).then(|| FunctionChunk {
            name: name.map(str::to_string),
            arguments: args.map(str::to_string),
        });
        ToolCallChunk { index, id: id.map(str::to_string), function }
    }

    fn collect(frags: impl IntoIterator<Item = ToolCallChunk>) -> Vec<ToolCallDelta> {
        let mut acc = ToolCallAccumulator::default();
        for f in frags {
            acc.absorb(&f);
        }
        acc.finish()
    }

    fn name_of(tc: &ToolCallDelta) -> &str {
        tc.function.name.as_deref().unwrap_or("")
    }

    fn args_of(tc: &ToolCallDelta) -> &str {
        tc.function.arguments.as_deref().unwrap_or("")
    }

    /// The regression this file used to fail: a gateway omitting `index` landed every
    /// fragment in slot 0, so a turn requesting two tools came back as ONE call whose
    /// name was both names concatenated and whose arguments were both payloads glued
    /// together. The loop then reported a nonexistent tool and the model retried blind.
    #[test]
    fn indexless_fragments_with_distinct_ids_stay_two_calls() {
        let calls = collect([
            frag(None, Some("call_A"), Some("ir_evtx_parse"), Some("{\"path\":\"")),
            frag(None, None, None, Some(r#"a.evtx"}"#)),
            frag(None, Some("call_B"), Some("ir_usn"), Some(r#"{"drive":"C"}"#)),
        ]);
        assert_eq!(calls.len(), 2, "two distinct call ids must never merge");
        assert_eq!(calls[0].id, "call_A");
        assert_eq!(name_of(&calls[0]), "ir_evtx_parse");
        assert_eq!(args_of(&calls[0]), r#"{"path":"a.evtx"}"#);
        assert_eq!(name_of(&calls[1]), "ir_usn");
        assert_eq!(args_of(&calls[1]), r#"{"drive":"C"}"#);
    }

    #[test]
    fn explicit_index_still_wins_over_arrival_order() {
        let calls = collect([
            frag(Some(1), Some("c2"), Some("second"), Some("{}")),
            frag(Some(0), Some("c1"), Some("first"), Some("{}")),
        ]);
        assert_eq!(calls.len(), 2);
        assert_eq!(name_of(&calls[0]), "first", "slot 0 is the first call");
        assert_eq!(name_of(&calls[1]), "second");
    }

    /// With neither index nor id, a second `name` is the only remaining signal that a
    /// new call began. This holds because OpenAI delivers `name` whole in a call's
    /// first fragment; a gateway that splits names AND omits both keys would be
    /// mis-split here, and is the reason the explicit-index path is checked first.
    #[test]
    fn indexless_and_idless_fragments_split_on_a_second_name() {
        let calls = collect([
            frag(None, None, Some("file_read"), Some("{}")),
            frag(None, None, Some("file_list"), Some("{}")),
        ]);
        assert_eq!(calls.len(), 2);
        assert_eq!(name_of(&calls[1]), "file_list");
    }

    #[test]
    fn a_split_name_stays_one_call_when_indexed() {
        let calls = collect([
            frag(Some(0), Some("c1"), Some("ir_evtx"), None),
            frag(Some(0), None, Some("_parse"), Some("{}")),
        ]);
        assert_eq!(calls.len(), 1);
        assert_eq!(name_of(&calls[0]), "ir_evtx_parse");
    }

    #[test]
    fn an_out_of_range_index_is_dropped_not_allocated() {
        let calls = collect([
            frag(Some(0), Some("c1"), Some("kept"), Some("{}")),
            frag(Some(9_999_999), Some("c2"), Some("dropped"), Some("{}")),
        ]);
        assert_eq!(calls.len(), 1, "a huge index must not grow the map to 10M entries");
        assert_eq!(name_of(&calls[0]), "kept");
    }

    #[test]
    fn distinct_ids_beyond_the_cap_are_dropped() {
        let frags: Vec<ToolCallChunk> = (0..300)
            .map(|i| frag(None, Some(&format!("id_{i}")), Some("tool"), Some("{}")))
            .collect();
        let calls = collect(frags);
        assert_eq!(calls.len(), ToolCallAccumulator::MAX_CALLS);
    }

    #[test]
    fn an_endless_argument_stream_is_bounded() {
        let big = "x".repeat(500_000);
        let frags: Vec<ToolCallChunk> = (0..5)
            .map(|_| frag(Some(0), Some("c1"), Some("tool"), Some(&big)))
            .collect();
        let calls = collect(frags);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            args_of(&calls[0]).len(),
            ToolCallAccumulator::MAX_ARGS_BYTES,
            "2.5MB of streamed arguments must be cut at the cap, not kept whole"
        );
    }

    async fn spawn_indexless_tool_call_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;

            // Deliberately no "index" key anywhere, as some compatible gateways emit.
            let a_head = "{\"path\":\"";
            let a_tail = r#"a.evtx"}"#;
            let b_args = r#"{"drive":"C"}"#;
            let events = [
                serde_json::json!({"choices": [{"delta": {"tool_calls": [
                    {"id": "call_A", "function": {"name": "ir_evtx_parse", "arguments": a_head}}
                ]}}]}),
                serde_json::json!({"choices": [{"delta": {"tool_calls": [
                    {"function": {"arguments": a_tail}}
                ]}}]}),
                serde_json::json!({"choices": [{"delta": {"tool_calls": [
                    {"id": "call_B", "function": {"name": "ir_usn", "arguments": b_args}}
                ]}, "finish_reason": "tool_calls"}]}),
            ];
            let mut body = String::new();
            for e in events {
                body.push_str(&format!("data: {e}\n\n"));
            }
            body.push_str("data: [DONE]\n\n");

            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            let _ = sock.write_all(headers.as_bytes()).await;
            let frame = format!("{:x}\r\n{}\r\n", body.len(), body);
            let _ = sock.write_all(frame.as_bytes()).await;
            let _ = sock.write_all(b"0\r\n\r\n").await;
            let _ = sock.flush().await;
        });
        format!("http://{}", addr)
    }

    fn model_for(api_base: String) -> ModelConfig {
        ModelConfig { title: "stub".into(), name: "stub-model".into(), api_base,
            api_key: None, api_key_env: None, context_window: 4096, max_tokens: 64,
            temperature: 0.0, supports_vision: false }
    }

    /// The same wire bytes through the real `chat_stream` path, so the fix is proven
    /// where the agent loop actually consumes it rather than only in the helper.
    #[tokio::test]
    async fn chat_stream_separates_indexless_parallel_tool_calls() {
        let api_base = spawn_indexless_tool_call_server().await;
        let provider = OpenAiProvider::new_with_shared_timeouts(
            Arc::new(tokio::sync::RwLock::new(vec![model_for(api_base)])), 10, 5);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentResult<crate::agent::AgentEvent>>(16);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let (_content, _reasoning, tool_calls, _usage, finish_reason, stream_timed_out) = provider
            .chat_stream("stub-model", &[ChatMessage::user("go")], &[], tx, "inv", "test")
            .await
            .expect("a complete stream must return Ok");
        assert!(!stream_timed_out, "the stub terminated cleanly");
        assert_eq!(tool_calls.len(), 2, "one turn, two tools, no index on the wire");
        assert_eq!(name_of(&tool_calls[0]), "ir_evtx_parse");
        assert_eq!(args_of(&tool_calls[0]), r#"{"path":"a.evtx"}"#);
        assert_eq!(name_of(&tool_calls[1]), "ir_usn");
        assert_eq!(args_of(&tool_calls[1]), r#"{"drive":"C"}"#);
        assert_eq!(finish_reason.as_deref(), Some("tool_calls"));
    }
}
