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
    #[serde(default)]
    index: usize,
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
        let mut tool_calls_map: Vec<ToolCallAccum> = Vec::new();
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
                                            let idx = tc.index;
                                            while tool_calls_map.len() <= idx {
                                                tool_calls_map.push(ToolCallAccum::default());
                                            }
                                            if let Some(ref id) = tc.id {
                                                tool_calls_map[idx].id = id.clone();
                                            }
                                            if let Some(ref func) = tc.function {
                                                if let Some(ref name) = func.name {
                                                    tool_calls_map[idx].name.push_str(name);
                                                }
                                                if let Some(ref args) = func.arguments {
                                                    tool_calls_map[idx].arguments.push_str(args);
                                                }
                                            }
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

        let mut synthetic_id_counter = 0u32;
        let tool_calls: Vec<ToolCallDelta> = tool_calls_map
            .into_iter()
            .filter(|tc| !tc.name.is_empty())
            .map(|tc| {
                let id = if tc.id.is_empty() {
                    let sid = format!("tc_synthetic_{}", synthetic_id_counter);
                    synthetic_id_counter += 1;
                    debug!("Tool call '{}' missing ID from API, generated synthetic ID: {}", tc.name, sid);
                    sid
                } else {
                    tc.id
                };
                ToolCallDelta {
                    id,
                    call_type: "function".to_string(),
                    function: FunctionCallDelta {
                        name: Some(tc.name),
                        arguments: Some(tc.arguments),
                    },
                }
            })
            .collect();

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
            let mut tc_map: Vec<ToolCallAccum> = Vec::new();
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
                                                let idx = tc.index;
                                                while tc_map.len() <= idx {
                                                    tc_map.push(ToolCallAccum::default());
                                                }
                                                if let Some(ref id) = tc.id {
                                                    tc_map[idx].id = id.clone();
                                                }
                                                if let Some(ref func) = tc.function {
                                                    if let Some(ref name) = func.name {
                                                        tc_map[idx].name.push_str(name);
                                                    }
                                                    if let Some(ref args) = func.arguments {
                                                        tc_map[idx].arguments.push_str(args);
                                                    }
                                                }
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
            let mut synthetic_id_counter = 0u32;
            let tool_calls: Vec<ToolCallDelta> = tc_map
                .into_iter()
                .filter(|tc| !tc.name.is_empty())
                .map(|tc| {
                    let id = if tc.id.is_empty() {
                        let sid = format!("tc_synthetic_{}", synthetic_id_counter);
                        synthetic_id_counter += 1;
                        sid
                    } else {
                        tc.id
                    };
                    ToolCallDelta {
                        id,
                        call_type: "function".to_string(),
                        function: FunctionCallDelta {
                            name: Some(tc.name),
                            arguments: Some(tc.arguments),
                        },
                    }
                })
                .collect();

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
