//! Anthropic SDK 客户端模块
//!
//! 参考 @ai-sdk/anthropic SDK 实现，用于 443 端口的 SDK 模式处理。
//!
//! 主要功能：
//! - 发送 Anthropic 格式请求到后端 API
//! - 处理 SSE 流式响应
//! - 提取 token 统计 (input_tokens, output_tokens, cache_*_tokens)

use log::{info, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// =====================================================================
// Anthropic 请求/响应类型定义
// 参考 @ai-sdk/anthropic/src/anthropic-messages-api.ts
// =====================================================================

/// Anthropic 请求体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub max_tokens: u64,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
}

/// Anthropic 消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: MessageContent,
}

/// 消息内容 (字符串或内容块数组)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// 内容块
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ImageSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// 图像源
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// System 内容
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemContent {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

/// System 块
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// 缓存控制
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

/// Anthropic 工具定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Anthropic 响应体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicResponse {
    #[serde(rename = "type")]
    pub response_type: String,
    pub id: String,
    pub model: String,
    pub content: Vec<ResponseContent>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

/// 响应内容
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseContent {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Token 使用统计
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

// =====================================================================
// SSE 事件类型定义
// 参考 @ai-sdk/anthropic/src/anthropic-messages-api.ts (第 892-1325 行)
// =====================================================================

/// SSE 事件类型
#[derive(Debug, Clone)]
pub enum SseEvent {
    MessageStart {
        id: Option<String>,
        model: Option<String>,
        usage: Usage,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockStartData,
    },
    ContentBlockDelta {
        index: usize,
        delta: DeltaData,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaData,
        usage: UsageDelta,
    },
    MessageStop,
    Ping,
    Error {
        error: ErrorData,
    },
}

/// content_block_start 中的 content_block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlockStartData {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

/// content_block_delta 中的 delta
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub delta_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_json: Option<String>,
}

/// message_delta 中的 delta
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDeltaData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

/// message_delta 中的 usage
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

/// 错误数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorData {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// =====================================================================
// SSE 解析器
// =====================================================================

/// SSE 事件解析器
///
/// 解析 Anthropic API 返回的 SSE 流式事件
pub struct AnthropicSseParser {
    buffer: String,
}

impl AnthropicSseParser {
    pub fn new() -> Self {
        Self {
            buffer: String::with_capacity(8192),
        }
    }

    /// 追加数据到缓冲区
    pub fn push_data(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        self.buffer.push_str(&text);
    }

    /// 提取所有完整的 SSE 事件
    ///
    /// 返回解析后的事件列表
    pub fn extract_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 找到最后一个 \n\n 的位置（SSE 事件分隔符）
        let last_event_end = match self.buffer.rfind("\n\n") {
            Some(pos) => pos + 2,
            None => return events,
        };

        let complete_part = self.buffer[..last_event_end].to_string();
        self.buffer = self.buffer[last_event_end..].to_string();

        // 解析每个事件
        let mut current_event_type: Option<String> = None;
        let mut current_data: Option<String> = None;

        for line in complete_part.lines() {
            if let Some(event_type) = line.strip_prefix("event: ") {
                // 如果有之前的事件，先处理
                if let (Some(et), Some(data)) = (&current_event_type, &current_data) {
                    if let Some(event) = parse_sse_event(et, data) {
                        events.push(event);
                    }
                }
                current_event_type = Some(event_type.to_string());
                current_data = None;
            } else if let Some(data) = line.strip_prefix("data: ") {
                current_data = Some(data.to_string());
            } else if line.is_empty() {
                // 空行表示事件结束
                if let (Some(et), Some(data)) = (current_event_type.take(), current_data.take()) {
                    if let Some(event) = parse_sse_event(&et, &data) {
                        events.push(event);
                    }
                }
            }
        }

        // 处理最后一个事件（如果没有空行结尾）
        if let (Some(et), Some(data)) = (current_event_type, current_data) {
            if let Some(event) = parse_sse_event(&et, &data) {
                events.push(event);
            }
        }

        events
    }
}

/// 解析单个 SSE 事件
fn parse_sse_event(event_type: &str, data: &str) -> Option<SseEvent> {
    match event_type {
        "message_start" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let message = v.get("message")?;
            Some(SseEvent::MessageStart {
                id: message.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()),
                model: message.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()),
                usage: parse_usage(message.get("usage")?),
            })
        }
        "content_block_start" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let index = v.get("index")?.as_u64()? as usize;
            let content_block = v.get("content_block")?;
            let block: ContentBlockStartData = serde_json::from_value(content_block.clone()).ok()?;
            Some(SseEvent::ContentBlockStart { index, content_block: block })
        }
        "content_block_delta" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let index = v.get("index")?.as_u64()? as usize;
            let delta: DeltaData = serde_json::from_value(v.get("delta")?.clone()).ok()?;
            Some(SseEvent::ContentBlockDelta { index, delta })
        }
        "content_block_stop" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let index = v.get("index")?.as_u64()? as usize;
            Some(SseEvent::ContentBlockStop { index })
        }
        "message_delta" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let delta: MessageDeltaData = serde_json::from_value(v.get("delta")?.clone()).ok()?;
            let usage = v.get("usage")
                .map(|u| serde_json::from_value(u.clone()).unwrap_or_default())
                .unwrap_or_default();
            Some(SseEvent::MessageDelta { delta, usage })
        }
        "message_stop" => Some(SseEvent::MessageStop),
        "ping" => Some(SseEvent::Ping),
        "error" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let error: ErrorData = serde_json::from_value(v.get("error")?.clone()).ok()?;
            Some(SseEvent::Error { error })
        }
        _ => None,
    }
}

/// 解析 usage 对象
fn parse_usage(v: &Value) -> Usage {
    Usage {
        input_tokens: v.get("input_tokens").and_then(|i| i.as_u64()).unwrap_or(0),
        output_tokens: v.get("output_tokens").and_then(|o| o.as_u64()).unwrap_or(0),
        cache_creation_input_tokens: v.get("cache_creation_input_tokens").and_then(|c| c.as_u64()),
        cache_read_input_tokens: v.get("cache_read_input_tokens").and_then(|c| c.as_u64()),
    }
}

// =====================================================================
// SDK 客户端
// =====================================================================

/// Anthropic SDK 客户端配置
#[derive(Debug, Clone)]
pub struct SdkConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// 流式响应状态
#[derive(Debug, Clone, Default)]
pub struct StreamState {
    pub message_id: String,
    pub model: String,
    pub content_blocks: Vec<ContentBlockState>,
    pub usage: Usage,
    pub stop_reason: Option<String>,
    pub finished: bool,
}

/// 内容块状态
#[derive(Debug, Clone)]
pub struct ContentBlockState {
    pub block_type: String,
    pub text: String,
    pub tool_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: String,
}

impl Default for AnthropicSseParser {
    fn default() -> Self {
        Self::new()
    }
}

/// 处理 SSE 事件并更新流状态
///
/// 返回要发送给客户端的 SSE 事件字符串
pub fn process_sse_event(event: SseEvent, state: &mut StreamState) -> Option<String> {
    match event {
        SseEvent::MessageStart { id, model, usage } => {
            state.message_id = id.unwrap_or_default();
            state.model = model.unwrap_or_default();
            state.usage.input_tokens = usage.input_tokens;
            state.usage.cache_creation_input_tokens = usage.cache_creation_input_tokens;
            state.usage.cache_read_input_tokens = usage.cache_read_input_tokens;

            // 转发 message_start
            Some(format_sse_event("message_start", &json!({
                "type": "message_start",
                "message": {
                    "id": state.message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": state.model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": state.usage.input_tokens,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": state.usage.cache_creation_input_tokens,
                        "cache_read_input_tokens": state.usage.cache_read_input_tokens,
                    }
                }
            })))
        }
        SseEvent::ContentBlockStart { index, content_block } => {
            // 确保向量足够大
            while state.content_blocks.len() <= index {
                state.content_blocks.push(ContentBlockState {
                    block_type: String::new(),
                    text: String::new(),
                    tool_id: None,
                    tool_name: None,
                    tool_input: String::new(),
                });
            }

            state.content_blocks[index] = ContentBlockState {
                block_type: content_block.block_type.clone(),
                text: content_block.text.clone().unwrap_or_default(),
                tool_id: content_block.id.clone(),
                tool_name: content_block.name.clone(),
                tool_input: String::new(),
            };

            Some(format_sse_event("content_block_start", &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": content_block
            })))
        }
        SseEvent::ContentBlockDelta { index, delta } => {
            // 累积内容
            if index < state.content_blocks.len() {
                if let Some(ref text) = delta.text {
                    state.content_blocks[index].text.push_str(text);
                }
                if let Some(ref partial_json) = delta.partial_json {
                    state.content_blocks[index].tool_input.push_str(partial_json);
                }
            }

            Some(format_sse_event("content_block_delta", &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": delta
            })))
        }
        SseEvent::ContentBlockStop { index } => {
            Some(format_sse_event("content_block_stop", &json!({
                "type": "content_block_stop",
                "index": index
            })))
        }
        SseEvent::MessageDelta { delta, usage } => {
            state.stop_reason = delta.stop_reason.clone();
            if let Some(ot) = usage.output_tokens {
                state.usage.output_tokens = ot;
            }
            if let Some(it) = usage.input_tokens {
                state.usage.input_tokens = it;
            }
            if let Some(cc) = usage.cache_creation_input_tokens {
                state.usage.cache_creation_input_tokens = Some(cc);
            }
            if let Some(cr) = usage.cache_read_input_tokens {
                state.usage.cache_read_input_tokens = Some(cr);
            }

            Some(format_sse_event("message_delta", &json!({
                "type": "message_delta",
                "delta": delta,
                "usage": {
                    "output_tokens": state.usage.output_tokens
                }
            })))
        }
        SseEvent::MessageStop => {
            state.finished = true;
            Some(format_sse_event("message_stop", &json!({
                "type": "message_stop"
            })))
        }
        SseEvent::Ping => {
            // 心跳事件，不需要转发
            None
        }
        SseEvent::Error { error } => {
            Some(format_sse_event("error", &json!({
                "type": "error",
                "error": error
            })))
        }
    }
}

/// 格式化 SSE 事件
fn format_sse_event(event_type: &str, data: &Value) -> String {
    let json_str = serde_json::to_string(data).unwrap_or_default();
    format!("event: {}\ndata: {}\n\n", event_type, json_str)
}

/// 从请求体提取 model 字段
pub fn extract_model(request_body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(request_body).ok()?;
    v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string())
}

/// 从请求体提取 stream 字段
pub fn extract_stream(request_body: &[u8]) -> bool {
    match serde_json::from_slice::<Value>(request_body) {
        Ok(v) => v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
        Err(_) => false,
    }
}

/// 检查是否是 Anthropic 格式请求
///
/// Anthropic 请求通常包含 messages 字段，且 messages 中的 content 可能是数组
pub fn is_anthropic_request(request_body: &[u8]) -> bool {
    if let Ok(v) = serde_json::from_slice::<Value>(request_body) {
        // 检查是否有 messages 字段
        if v.get("messages").is_some() {
            // Anthropic 格式：messages 中的 content 可能是数组
            if let Some(messages) = v.get("messages").and_then(|m| m.as_array()) {
                if let Some(first_msg) = messages.first() {
                    if let Some(content) = first_msg.get("content") {
                        return content.is_array() || content.is_string();
                    }
                }
            }
            // 有 messages 但无法判断格式，假设是 Anthropic
            return true;
        }
    }
    false
}

/// 提取完整的 token 统计（用于非流式响应）
pub fn extract_usage_from_response(response_body: &[u8]) -> Option<Usage> {
    let v: Value = serde_json::from_slice(response_body).ok()?;
    let usage = v.get("usage")?;
    Some(parse_usage(usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sse_message_start() {
        let data = r#"{"type":"message_start","message":{"id":"msg_123","model":"claude-sonnet-4","usage":{"input_tokens":100,"output_tokens":0}}}"#;
        let event = parse_sse_event("message_start", data);
        assert!(event.is_some());
        if let Some(SseEvent::MessageStart { id, model, usage }) = event {
            assert_eq!(id, Some("msg_123".to_string()));
            assert_eq!(model, Some("claude-sonnet-4".to_string()));
            assert_eq!(usage.input_tokens, 100);
        } else {
            panic!("Wrong event type");
        }
    }

    #[test]
    fn test_extract_model() {
        let body = br#"{"model":"claude-sonnet-4-20250514","messages":[{"role":"user","content":"hello"}]}"#;
        let model = extract_model(body);
        assert_eq!(model, Some("claude-sonnet-4-20250514".to_string()));
    }

    #[test]
    fn test_is_anthropic_request() {
        let anthropic_body = br#"{"model":"claude-sonnet-4","messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
        assert!(is_anthropic_request(anthropic_body));

        let openai_body = br#"{"model":"gpt-4","messages":[{"role":"user","content":"hello"}]}"#;
        assert!(is_anthropic_request(openai_body)); // 也返回 true，因为有 messages
    }
}
