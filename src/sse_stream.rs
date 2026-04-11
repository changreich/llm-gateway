//! SSE 流式响应转换模块
//!
//! 将 OpenAI SSE 流式响应实时转换为 Anthropic SSE 流式响应。
//! 完全在 Rust 层处理，不经过 Lua。
//!
//! 参考 SSE_RESPONSE.md 获取完整设计文档。

use log::{info, warn};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

// =====================================================================
// SSE 流状态机
// =====================================================================

/// SSE 流式转换状态
///
/// 跟踪从 OpenAI 到 Anthropic 的流式转换进度。
/// 每个请求创建一个新实例，逐 chunk 推进状态。
pub struct SseStreamState {
    pub started: bool,
    pub content_block_index: usize,
    pub in_content_block: bool,
    pub current_block_type: BlockType,
    pub tool_ids: Vec<String>,
    pub model: String,
    pub msg_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockType {
    None,
    Text,
    ToolUse,
}

/// 从 OpenAI chunk 提取的关键信息
struct OpenaiChunkInfo {
    id: String,
    model: String,
    delta_content: Option<String>,
    #[allow(dead_code)]
    delta_role: Option<String>,
    delta_tool_calls: Vec<OpenaiToolCallDelta>,
    finish_reason: Option<String>,
    usage_prompt_tokens: Option<u64>,
    usage_completion_tokens: Option<u64>,
}

struct OpenaiToolCallDelta {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

// =====================================================================
// 公开接口
// =====================================================================

/// 创建新的 SSE 流状态
///
/// `model` 是来自客户端原始请求的 model 名（会被回传给客户端）
pub fn new_sse_stream_state(model: String) -> SseStreamState {
    SseStreamState {
        started: false,
        content_block_index: 0,
        in_content_block: false,
        current_block_type: BlockType::None,
        tool_ids: Vec::new(),
        model,
        msg_id: String::new(),
        input_tokens: 0,
        output_tokens: 0,
    }
}

/// 将单个 OpenAI SSE chunk 转换为一个或多个 Anthropic SSE 事件
///
/// `chunk_json` 是 `data: ` 之后的 JSON 字符串（不含 `data: ` 前缀）。
/// 返回 Anthropic SSE 事件字符串列表，每个元素已是 `event: xxx\ndata: xxx\n\n` 格式。
pub fn transform_openai_sse_chunk_to_anthropic(
    chunk_json: &str,
    state: &mut SseStreamState,
) -> Vec<String> {
    let mut events = Vec::new();

    let chunk = match serde_json::from_str::<Value>(chunk_json) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "Failed to parse SSE chunk as JSON: {} - data: {}",
                e,
                &chunk_json[..chunk_json.len().min(200)]
            );
            return events;
        }
    };

    let info = match extract_openai_chunk_info(&chunk) {
        Some(i) => i,
        None => return events,
    };

    if state.msg_id.is_empty() {
        if !info.id.is_empty() {
            state.msg_id = format!(
                "msg_{}",
                info.id.strip_prefix("chatcmpl-").unwrap_or(&info.id)
            );
        } else {
            state.msg_id = "msg_unknown".to_string();
        }
    }

    if state.model.is_empty() && !info.model.is_empty() {
        state.model = info.model.clone();
    }

    if !state.started {
        state.started = true;

        let message_start = json!({
            "type": "message_start",
            "message": {
                "id": state.msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": state.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": state.input_tokens,
                    "output_tokens": 0
                }
            }
        });
        events.push(format_sse_event("message_start", &message_start));
    }

    // 处理 content
    if let Some(ref content) = info.delta_content {
        if !content.is_empty() || !state.in_content_block {
            if state.current_block_type == BlockType::ToolUse {
                events.push(format_sse_event(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": state.content_block_index}),
                ));
                state.in_content_block = false;
                state.content_block_index += 1;
            }

            if !state.in_content_block {
                state.content_block_index += 1;
                state.current_block_type = BlockType::Text;
                state.in_content_block = true;

                events.push(format_sse_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": state.content_block_index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                ));
            }

            events.push(format_sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": state.content_block_index,
                    "delta": {"type": "text_delta", "text": content}
                }),
            ));
        } else if !content.is_empty()
            && state.in_content_block
            && state.current_block_type == BlockType::Text
        {
            events.push(format_sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": state.content_block_index,
                    "delta": {"type": "text_delta", "text": content}
                }),
            ));
        }
    }

    // 处理 tool_calls
    for tc_delta in &info.delta_tool_calls {
        let tc_idx = tc_delta.index;

        // 新 tool call（含 id 和/或 name）
        if tc_delta.id.is_some() || tc_delta.name.is_some() {
            // 关闭之前的 content block
            if state.in_content_block {
                events.push(format_sse_event(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": state.content_block_index}),
                ));
                state.in_content_block = false;
                state.content_block_index += 1;
            } else if state.current_block_type == BlockType::None && state.started {
                // 首个 content block，不需要额外关闭
            }

            let tool_id = tc_delta.id.clone().unwrap_or_else(|| {
                tc_idx
                    .checked_sub(state.tool_ids.len())
                    .map(|i| format!("call_{}", state.tool_ids.len() + i))
                    .unwrap_or_else(|| format!("call_{}", tc_idx))
            });
            let tool_name = tc_delta.name.clone().unwrap_or_default();

            // 确保 tool_ids 向量足够大
            while state.tool_ids.len() <= tc_idx {
                if state.tool_ids.len() == tc_idx {
                    state.tool_ids.push(tool_id.clone());
                } else {
                    state
                        .tool_ids
                        .push(format!("call_{}", state.tool_ids.len()));
                }
            }
            if tc_idx < state.tool_ids.len() {
                state.tool_ids[tc_idx] = tool_id.clone();
            }

            state.content_block_index += 1;
            state.current_block_type = BlockType::ToolUse;
            state.in_content_block = true;

            events.push(format_sse_event(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": state.content_block_index,
                    "content_block": {
                        "type": "tool_use",
                        "id": tool_id,
                        "name": tool_name,
                        "input": {}
                    }
                }),
            ));
        }

        // tool call arguments 增量
        if let Some(ref args_fragment) = tc_delta.arguments {
            if !args_fragment.is_empty() {
                if !state.in_content_block {
                    state.content_block_index += 1;
                    state.current_block_type = BlockType::ToolUse;
                    state.in_content_block = true;

                    let tool_id = if tc_idx < state.tool_ids.len() {
                        state.tool_ids[tc_idx].clone()
                    } else {
                        format!("call_{}", tc_idx)
                    };

                    events.push(format_sse_event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": state.content_block_index,
                            "content_block": {
                                "type": "tool_use",
                                "id": tool_id,
                                "name": "",
                                "input": {}
                            }
                        }),
                    ));
                }

                events.push(format_sse_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": state.content_block_index,
                        "delta": {"type": "input_json_delta", "partial_json": args_fragment}
                    }),
                ));
            }
        }
    }

    // 处理 finish_reason
    if let Some(ref reason) = info.finish_reason {
        events.extend(generate_stream_end_events(
            state,
            reason,
            info.usage_completion_tokens.unwrap_or(0),
        ));
    }

    // 如果 chunk 包含 usage 信息，更新 input_tokens
    if let Some(pt) = info.usage_prompt_tokens {
        state.input_tokens = pt;
    }
    if let Some(ct) = info.usage_completion_tokens {
        state.output_tokens += ct;
    }

    events
}

/// 生成流结束事件（content_block_stop + message_delta + message_stop）
///
/// 在收到 finish_reason 后调用，或作为流结束的安全保障。
pub fn generate_stream_end_events(
    state: &mut SseStreamState,
    finish_reason: &str,
    output_tokens: u64,
) -> Vec<String> {
    let mut events = Vec::new();

    // 关闭当前 content block（如果有）
    if state.in_content_block {
        events.push(format_sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": state.content_block_index}),
        ));
        state.in_content_block = false;
    }

    // 映射 finish_reason → stop_reason
    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    };

    let total_output_tokens = if output_tokens > 0 {
        output_tokens
    } else {
        state.output_tokens
    };

    // message_delta
    events.push(format_sse_event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": null
            },
            "usage": {
                "output_tokens": total_output_tokens
            }
        }),
    ));

    // message_stop
    events.push(format_sse_event(
        "message_stop",
        &json!({"type": "message_stop"}),
    ));

    events
}

/// 生成 Anthropic 错误事件流
///
/// 当上游返回非 200 状态码时，包装为 Anthropic 错误事件流。
pub fn generate_error_sse_stream(error_msg: &str, model: &str) -> String {
    let mut sse = String::new();

    // message_start
    let msg_id = format!(
        "msg_error_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    sse.push_str(&format_sse_event(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        }),
    ));

    // content_block_start (error text)
    sse.push_str(&format_sse_event(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        }),
    ));

    // content_block_delta (error message)
    sse.push_str(&format_sse_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": error_msg}
        }),
    ));

    // content_block_stop
    sse.push_str(&format_sse_event(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": 0}),
    ));

    // message_delta
    sse.push_str(&format_sse_event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 0}
        }),
    ));

    // message_stop
    sse.push_str(&format_sse_event(
        "message_stop",
        &json!({"type": "message_stop"}),
    ));

    sse
}

// =====================================================================
// SSE 行解析缓冲区
// =====================================================================

/// SSE 行解析器
///
/// TCP 可能将 SSE 数据分片到达，需要缓冲并按行提取。
/// 从缓冲区中提取完整的 `data: ...` 行。
pub struct SseLineParser {
    buffer: String,
}

impl SseLineParser {
    pub fn new() -> Self {
        SseLineParser {
            buffer: String::with_capacity(4096),
        }
    }

    /// 追加新数据到缓冲区
    pub fn push_data(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        self.buffer.push_str(&text);
    }

    /// 从缓冲区提取所有完整的 SSE data 行
    ///
    /// 返回提取到的 data 内容列表（不含 `data: ` 前缀）。
    /// `[DONE]` 会作为特殊值返回。
    /// 只消费到最后一个 `\n` 为止，不完整的尾部保留在缓冲区中。
    pub fn extract_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();

        // 找到最后一个 \n 的位置，只处理到该位置为止
        // 不完整的尾部（最后一个 \n 之后的数据）保留在缓冲区中
        let last_newline_pos = match self.buffer.rfind('\n') {
            Some(pos) => pos + 1, // 包含 \n 本身
            None => return lines, // 没有完整行，等更多数据
        };

        let complete_part = self.buffer[..last_newline_pos].to_string();
        self.buffer = self.buffer[last_newline_pos..].to_string();

        for line in complete_part.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(data_content) = trimmed.strip_prefix("data: ") {
                if data_content.trim() == "[DONE]" {
                    lines.push("[DONE]".to_string());
                } else {
                    lines.push(data_content.to_string());
                }
            } else if trimmed.starts_with("data:") {
                let data_content = trimmed[5..].trim_start();
                if data_content == "[DONE]" {
                    lines.push("[DONE]".to_string());
                } else {
                    lines.push(data_content.to_string());
                }
            }
        }

        lines
    }

    /// 检查缓冲区是否已清空
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.buffer.trim().is_empty()
    }
}

// =====================================================================
// 向请求体注入 stream:true
// =====================================================================

/// 向 JSON 请求体注入 `"stream":true`
///
/// 如果请求体已是合法 JSON，在顶层添加 stream 字段。
/// 如果请求体已经包含 stream 字段，覆盖为 true。
pub fn inject_stream_true(request_body: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(request_body).ok()?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("stream".to_string(), Value::Bool(true));
    }
    serde_json::to_string(&v).ok()
}

// =====================================================================
// 内部辅助函数
// =====================================================================

/// 从 OpenAI chunk JSON 中提取关键信息
fn extract_openai_chunk_info(chunk: &Value) -> Option<OpenaiChunkInfo> {
    let id = chunk
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let model = chunk
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let choice = chunk.get("choices").and_then(|c| c.get(0))?;

    let delta = choice.get("delta")?;

    let delta_content = delta
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());

    let delta_role = delta
        .get("role")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(|s| s.to_string());

    // 提取 usage（如果存在，通常在最后一个 chunk）
    let usage_prompt_tokens = chunk
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|p| p.as_u64());

    let usage_completion_tokens = chunk
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|c| c.as_u64());

    let mut delta_tool_calls = Vec::new();
    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let id = tc.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_string());
            let arguments = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .map(|s| s.to_string());

            delta_tool_calls.push(OpenaiToolCallDelta {
                index: idx,
                id,
                name,
                arguments,
            });
        }
    }

    Some(OpenaiChunkInfo {
        id,
        model,
        delta_content,
        delta_role,
        delta_tool_calls,
        finish_reason,
        usage_prompt_tokens,
        usage_completion_tokens,
    })
}

/// 格式化一个 Anthropic SSE 事件
///
/// 输出格式: `event: <type>\ndata: <json>\n\n`
fn format_sse_event(event_type: &str, data: &Value) -> String {
    let json_str = serde_json::to_string(data).unwrap_or_default();
    // 不转义 Unicode 字符（保持中文可读）
    // serde_json 默认会转义非 ASCII，这里用 to_string_unescaped
    // 实际上 serde_json 默认就不转义中文，只有 to_string_pretty 会多空格
    format!("event: {}\ndata: {}\n\n", event_type, json_str)
}

// =====================================================================
// SSE 连接注册表
// =====================================================================

/// SSE 流连接信息
///
/// 每个流式请求注册一个连接，跟踪客户端请求 ID 和服务端响应 ID 的映射。
/// 流结束后销毁。
#[derive(Debug, Clone)]
pub struct SseConnection {
    /// 唯一连接 ID（自增序列号）
    pub id: u64,
    /// 客户端请求中的 ID（Anthropic 格式，可能为空）
    pub client_request_id: String,
    /// 服务端 OpenAI SSE 流中的 ID（如 chatcmpl-xxx，可能为空直到首个 chunk）
    pub openai_sse_id: String,
    /// 客户端请求的 model
    pub model: String,
    /// 连接创建时间（Unix 时间戳毫秒）
    pub created_at: u64,
    /// 连接结束时间（Unix 时间戳毫秒，流结束时设置）
    pub finished_at: Option<u64>,
}

/// SSE 连接注册表
///
/// 全局单例，用于追踪所有活跃的 SSE 流连接。
/// 每次 SSE 流请求开始时注册，流结束后销毁。
pub struct SseConnectionRegistry {
    connections: RwLock<HashMap<u64, SseConnection>>,
    next_id: AtomicU64,
}

static SSE_REGISTRY: Lazy<SseConnectionRegistry> = Lazy::new(|| SseConnectionRegistry {
    connections: RwLock::new(HashMap::new()),
    next_id: AtomicU64::new(1),
});

/// 注册一个新的 SSE 连接
///
/// 返回分配的连接 ID。
pub fn sse_register(client_request_id: String, openai_sse_id: String, model: String) -> u64 {
    let id = SSE_REGISTRY.next_id.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let conn = SseConnection {
        id,
        client_request_id,
        openai_sse_id,
        model,
        created_at: now,
        finished_at: None,
    };
    info!(
        "[SSE-REG] Registered connection #{}: client_req_id={}, openai_sse_id={}, model={}",
        id, conn.client_request_id, conn.openai_sse_id, conn.model
    );
    SSE_REGISTRY.connections.write().unwrap().insert(id, conn);
    id
}

/// 更新连接的 openai_sse_id（首个 chunk 到达时设置）
pub fn sse_update_openai_id(conn_id: u64, openai_sse_id: &str) {
    if let Ok(mut conns) = SSE_REGISTRY.connections.write() {
        if let Some(conn) = conns.get_mut(&conn_id) {
            conn.openai_sse_id = openai_sse_id.to_string();
            info!(
                "[SSE-REG] Updated connection #{} openai_sse_id={}",
                conn_id, openai_sse_id
            );
        }
    }
}

/// 销毁一个 SSE 连接（流结束后调用）
pub fn sse_unregister(conn_id: u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    if let Ok(mut conns) = SSE_REGISTRY.connections.write() {
        if let Some(conn) = conns.get_mut(&conn_id) {
            conn.finished_at = Some(now);
            let elapsed = now - conn.created_at;
            info!(
                "[SSE-REG] Unregistered connection #{}: openai_sse_id={}, model={}, elapsed={}ms",
                conn_id, conn.openai_sse_id, conn.model, elapsed
            );
        }
        conns.remove(&conn_id);
    }
}

/// 获取当前所有活跃连接的快照
pub fn sse_get_active_connections() -> Vec<SseConnection> {
    SSE_REGISTRY
        .connections
        .read()
        .unwrap()
        .values()
        .cloned()
        .collect()
}

/// 获取活跃连接数量
pub fn sse_active_count() -> usize {
    SSE_REGISTRY.connections.read().unwrap().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_line_parser() {
        let mut parser = SseLineParser::new();

        parser.push_data(b"data: {\"id\":\"chatcmpl-123\"}\n\n");
        let lines = parser.extract_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("chatcmpl-123"));

        parser.push_data(b"data: [DONE]\n\n");
        let lines = parser.extract_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "[DONE]");
    }

    #[test]
    fn test_sse_line_parser_fragmented() {
        let mut parser = SseLineParser::new();

        parser.push_data(b"da");
        let lines = parser.extract_lines();
        assert!(lines.is_empty());

        parser.push_data(b"ta: {\"id\":\"test\"}\n\n");
        let lines = parser.extract_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("test"));
    }

    #[test]
    fn test_inject_stream_true() {
        let body = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hello"}]}"#;
        let result = inject_stream_true(body).unwrap();
        assert!(result.contains("\"stream\":true"));
    }

    #[test]
    fn test_transform_first_chunk() {
        let mut state = new_sse_stream_state("claude-sonnet-4-20250514".to_string());
        let chunk = r#"{"id":"chatcmpl-abc123","object":"chat.completion.chunk","choices":[{"delta":{"role":"assistant"},"index":0}]}"#;

        let events = transform_openai_sse_chunk_to_anthropic(chunk, &mut state);
        assert!(events.len() >= 1);
        assert!(events[0].contains("message_start"));
        assert!(state.started);
    }

    #[test]
    fn test_transform_content_chunk() {
        let mut state = new_sse_stream_state("claude-sonnet-4-20250514".to_string());
        state.started = true;
        state.msg_id = "msg_abc123".to_string();

        let chunk = r#"{"id":"chatcmpl-abc123","object":"chat.completion.chunk","choices":[{"delta":{"content":"Hello"},"index":0}]}"#;

        let events = transform_openai_sse_chunk_to_anthropic(chunk, &mut state);
        // 应该产生 content_block_start + content_block_delta
        assert!(events.len() >= 2);
        assert!(events[0].contains("content_block_start"));
        assert!(events[1].contains("content_block_delta"));
    }

    #[test]
    fn test_transform_finish_chunk() {
        let mut state = new_sse_stream_state("claude-sonnet-4-20250514".to_string());
        state.started = true;
        state.msg_id = "msg_abc123".to_string();
        state.in_content_block = true;
        state.content_block_index = 0;
        state.current_block_type = BlockType::Text;

        let chunk = r#"{"id":"chatcmpl-abc123","object":"chat.completion.chunk","choices":[{"delta":{},"finish_reason":"stop","index":0}],"usage":{"prompt_tokens":10,"completion_tokens":20}}"#;

        let events = transform_openai_sse_chunk_to_anthropic(chunk, &mut state);
        // content_block_stop + message_delta + message_stop
        let combined = events.join("");
        assert!(combined.contains("content_block_stop"));
        assert!(combined.contains("message_delta"));
        assert!(combined.contains("message_stop"));
        assert!(combined.contains("end_turn"));
    }

    #[test]
    fn test_generate_error_sse_stream() {
        let result = generate_error_sse_stream("test error", "claude-test");
        assert!(result.contains("message_start"));
        assert!(result.contains("text_delta"));
        assert!(result.contains("message_stop"));
    }
}
