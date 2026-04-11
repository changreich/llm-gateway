//! Anthropic ↔ OpenAI 协议转换模块
//!
//! 处理请求/响应在两个 API 格式之间的转换。
//! 参考 PROTOCOL_COMPARISON.md 获取完整的字段对照表。

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use log::info;
use serde_json::{json, Value};
use std::io::{Read, Write};

// =====================================================================
// 压缩 / 解压辅助函数
// =====================================================================

/// 将 JSON Value 序列化 → gzip 压缩 → base64 编码
pub fn compress_field(value: &Value) -> Option<String> {
    let json_str = serde_json::to_string(value).ok()?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(json_str.as_bytes()).ok()?;
    let compressed = encoder.finish().ok()?;
    Some(BASE64.encode(compressed))
}

/// base64 解码 → gzip 解压 → JSON 反序列化
pub fn decompress_field(compressed: &str) -> Option<Value> {
    let bytes = BASE64.decode(compressed).ok()?;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut json_str = String::new();
    decoder.read_to_string(&mut json_str).ok()?;
    serde_json::from_str(&json_str).ok()
}

// =====================================================================
// OpenAI → Anthropic 响应转换
// =====================================================================

/// 从 OpenAI 响应中提取简单字段和大字段的压缩版本
///
/// 返回 (id, model, finish_reason, input_tokens, output_tokens,
///         compressed_content, compressed_tool_calls)
///
/// - content 和 tool_calls 通过 gzip+base64 压缩后传给 Lua
/// - Lua 原样回传压缩字段，Rust 解压后做结构转换
pub fn extract_openai_fields(body: &str) -> Option<OpenaiFields> {
    let v: Value = serde_json::from_str(body).ok()?;

    if v.get("object").and_then(|o| o.as_str()) != Some("chat.completion") {
        return None;
    }

    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();
    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let finish_reason = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        .unwrap_or("stop")
        .to_string();

    let input_tokens = v
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|p| p.as_u64())
        .unwrap_or(0);
    let output_tokens = v
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|c| c.as_u64())
        .unwrap_or(0);

    let msg = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))?;

    // 提取并压缩 content
    let content_val = msg
        .get("content")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let compressed_content = compress_field(&content_val);

    // 提取并压缩 tool_calls
    let tool_calls_val = msg
        .get("tool_calls")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let compressed_tool_calls =
        if tool_calls_val.is_array() && !tool_calls_val.as_array().unwrap().is_empty() {
            compress_field(&tool_calls_val)
        } else {
            None
        };

    Some(OpenaiFields {
        id,
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        compressed_content,
        compressed_tool_calls,
    })
}

/// OpenAI 响应提取的字段
pub struct OpenaiFields {
    pub id: String,
    pub model: String,
    pub finish_reason: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub compressed_content: Option<String>,
    pub compressed_tool_calls: Option<String>,
}

/// 将 OpenAI 的 content (string) 转为 Anthropic 的 content blocks (array)
///
/// 参考 PROTOCOL_COMPARISON.md §2:
/// - string → [{type:"text", text:"..."}]
/// - 空字符串 → [{type:"text", text:""}]
pub fn convert_openai_content_to_anthropic(content: &str) -> Vec<Value> {
    if content.is_empty() {
        vec![json!({"type": "text", "text": ""})]
    } else {
        vec![json!({"type": "text", "text": content})]
    }
}

/// 将 OpenAI 的 tool_calls (array) 转为 Anthropic 的 tool_use content blocks (array)
///
/// 参考 PROTOCOL_COMPARISON.md §2.2:
/// - function.arguments (string) → input (object)
/// - id → id, function.name → name
pub fn convert_openai_tool_calls_to_anthropic(tool_calls: &[Value]) -> Vec<Value> {
    tool_calls
        .iter()
        .filter_map(|tc| {
            let tool_id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let tool_name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let tool_input = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(json!({}));

            Some(json!({
                "type": "tool_use",
                "id": tool_id,
                "name": tool_name,
                "input": tool_input
            }))
        })
        .collect()
}

/// 组装最终的 Anthropic 响应
///
/// 由 Rust 调用：接收 Lua 映射后的简单字段 + 解压后的大字段，
/// 组装成完整的 Anthropic Messages API 响应。
pub fn assemble_anthropic_response(
    id: &str,
    model: &str,
    stop_reason: &str,
    input_tokens: u64,
    output_tokens: u64,
    content_blocks: Vec<Value>,
) -> String {
    let response = json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "content": content_blocks,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    });
    serde_json::to_string(&response).unwrap_or_default()
}

/// 完整的 OpenAI → Anthropic 响应转换（直接模式，不经过 Lua）
///
/// 用于非 9089 端口或 Lua 回调失败的降级路径。
pub fn transform_openai_to_anthropic(body: &str, original_model: Option<&str>) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;

    if v.get("object").and_then(|o| o.as_str()) != Some("chat.completion") {
        return None;
    }

    let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
    let model =
        original_model.unwrap_or_else(|| v.get("model").and_then(|m| m.as_str()).unwrap_or(""));

    let msg = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))?;

    let mut content: Vec<Value> = Vec::new();

    if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }

    if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
        content.extend(convert_openai_tool_calls_to_anthropic(tool_calls));
    }

    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }

    let stop_reason = match v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        .unwrap_or("stop")
    {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    };

    let input_tokens = v
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|p| p.as_u64())
        .unwrap_or(0);
    let output_tokens = v
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|c| c.as_u64())
        .unwrap_or(0);

    let id_transformed = format!("msg_{}", id.strip_prefix("chatcmpl-").unwrap_or(id));

    Some(assemble_anthropic_response(
        &id_transformed,
        model,
        stop_reason,
        input_tokens,
        output_tokens,
        content,
    ))
}

// =====================================================================
// SSE 流式包装
// =====================================================================

/// 将 Anthropic JSON 响应包装为 SSE 流格式
///
/// 模拟 Anthropic 流式响应事件序列:
///   message_start → content_block_start → content_block_delta(s) → content_block_stop → message_delta → message_stop
pub fn wrap_as_sse(anthropic_json: &str) -> String {
    let v: Value = match serde_json::from_str(anthropic_json) {
        Ok(v) => v,
        Err(_) => return anthropic_json.to_string(),
    };

    let msg_id = v
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("msg_unknown");
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let input_tokens = v
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let output_tokens = v
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let stop_reason = v
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .unwrap_or("end_turn");

    let mut sse = String::new();

    // message_start
    let message_start = json!({
        "type": "message_start",
        "message": {
            "id": msg_id, "type": "message", "role": "assistant", "content": [],
            "model": model, "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": input_tokens, "output_tokens": 0}
        }
    });
    sse.push_str(&format!(
        "event: message_start\ndata: {}\n\n",
        message_start
    ));

    // content blocks
    let content = v
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    for (idx, block) in content.iter().enumerate() {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("text");

        // content_block_start
        let start_block = if block_type == "text" {
            json!({"type": "content_block_start", "index": idx, "content_block": {"type": "text", "text": ""}})
        } else if block_type == "tool_use" {
            json!({
                "type": "content_block_start", "index": idx,
                "content_block": {"type": "tool_use", "id": block.get("id").and_then(|i| i.as_str()).unwrap_or(""), "name": block.get("name").and_then(|n| n.as_str()).unwrap_or(""), "input": {}}
            })
        } else {
            json!({"type": "content_block_start", "index": idx, "content_block": block})
        };
        sse.push_str(&format!(
            "event: content_block_start\ndata: {}\n\n",
            start_block
        ));

        // content_block_delta
        if block_type == "text" {
            let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
            sse.push_str(&format!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({
                    "type": "content_block_delta", "index": idx, "delta": {"type": "text_delta", "text": text}
                })
            ));
        } else if block_type == "tool_use" {
            let input = block.get("input").cloned().unwrap_or(json!({}));
            let input_str = serde_json::to_string(&input).unwrap_or_default();
            sse.push_str(&format!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({
                    "type": "content_block_delta", "index": idx, "delta": {"type": "input_json_delta", "partial_json": input_str}
                })
            ));
        }

        // content_block_stop
        sse.push_str(&format!(
            "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{}}}\n\n",
            idx
        ));
    }

    // message_delta
    sse.push_str(&format!(
        "event: message_delta\ndata: {}\n\n",
        json!({
            "type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": null}, "usage": {"output_tokens": output_tokens}
        })
    ));

    // message_stop
    sse.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    sse
}

// =====================================================================
// Anthropic 请求 → OpenAI 请求转换
// =====================================================================

/// Anthropic content blocks → OpenAI content string
fn anthropic_content_to_openai(blocks: &[Value]) -> Value {
    if blocks.len() == 1 {
        if let Some(text) = blocks[0].get("text").and_then(|t| t.as_str()) {
            if blocks[0].get("type").and_then(|t| t.as_str()) == Some("text") {
                return Value::String(text.to_string());
            }
        }
    }

    let parts: Vec<String> = blocks
        .iter()
        .filter_map(|b| {
            b.get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    if !parts.is_empty() {
        return Value::String(parts.join("\n"));
    }

    let results: Vec<String> = blocks
        .iter()
        .filter_map(|b| {
            b.get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    if !results.is_empty() {
        return Value::String(results.join("\n"));
    }

    Value::String(String::new())
}

/// 将 Anthropic 格式请求体转为 OpenAI 格式
pub fn transform_anthropic_request_to_openai(body: &str, model: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;

    let mut openai = serde_json::Map::new();
    openai.insert("model".into(), Value::String(model.into()));

    let mut messages = Vec::new();

    // system 消息 → system role message
    if let Some(system) = v.get("system") {
        let system_text = match system {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|b| match b {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(m) => m
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !system_text.is_empty() {
            messages.push(json!({"role": "system", "content": system_text}));
        }
    }

    // messages
    if let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content");

            let openai_content = match content {
                Some(Value::String(s)) => Value::String(s.clone()),
                Some(Value::Array(arr)) => {
                    let has_tool_result = arr
                        .iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
                    if has_tool_result {
                        let tool_parts: Vec<Value> = arr
                            .iter()
                            .filter_map(|b| {
                                let block_type =
                                    b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                match block_type {
                                    "tool_result" => {
                                        let tool_use_id = b
                                            .get("tool_use_id")
                                            .and_then(|i| i.as_str())
                                            .unwrap_or("");
                                        let tool_content = b.get("content");
                                        let content_str = match tool_content {
                                            Some(Value::String(s)) => s.clone(),
                                            Some(Value::Array(blocks)) => blocks
                                                .iter()
                                                .filter_map(|bl| {
                                                    bl.get("text")
                                                        .and_then(|t| t.as_str())
                                                        .map(|s| s.to_string())
                                                })
                                                .collect::<Vec<_>>()
                                                .join("\n"),
                                            _ => String::new(),
                                        };
                                        Some(json!({
                                            "role": "tool",
                                            "tool_call_id": tool_use_id,
                                            "content": content_str
                                        }))
                                    }
                                    "text" => {
                                        let text =
                                            b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                        Some(json!({"role": role, "content": text}))
                                    }
                                    _ => None,
                                }
                            })
                            .collect();

                        for part in &tool_parts {
                            messages.push(part.clone());
                        }
                        continue;
                    }

                    anthropic_content_to_openai(arr)
                }
                Some(other) => other.clone(),
                None => Value::String(String::new()),
            };

            if !matches!(openai_content, Value::Null) || role != "tool" {
                if role != "tool" {
                    messages.push(json!({"role": role, "content": openai_content}));
                }
            }
        }
    }

    openai.insert("messages".into(), Value::Array(messages));

    // 透传通用参数
    for key in &[
        "max_tokens",
        "temperature",
        "top_p",
        "stop",
        "presence_penalty",
        "frequency_penalty",
        "seed",
        "logprobs",
    ] {
        if let Some(val) = v.get(*key) {
            openai.insert((*key).into(), val.clone());
        }
    }

    // tools: Anthropic → OpenAI function calling
    if let Some(tools) = v.get("tools").and_then(|t| t.as_array()) {
        let openai_tools: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": tool.get("name")?,
                        "description": tool.get("description").unwrap_or(&Value::String(String::new())),
                        "parameters": tool.get("input_schema").unwrap_or(&json!({}))
                    }
                }))
            })
            .collect();
        if !openai_tools.is_empty() {
            openai.insert("tools".into(), Value::Array(openai_tools));
        }
    }

    // tool_choice: Anthropic → OpenAI
    if let Some(tc) = v.get("tool_choice") {
        let openai_tc = match tc {
            Value::String(s) => match s.as_str() {
                "auto" => json!("auto"),
                "any" => json!("required"),
                _ => json!("auto"),
            },
            Value::Object(m) => {
                if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                    json!({"type": "function", "function": {"name": name}})
                } else {
                    json!("auto")
                }
            }
            _ => json!("auto"),
        };
        openai.insert("tool_choice".into(), openai_tc);
    }

    Some(serde_json::to_string(&Value::Object(openai)).unwrap_or_default())
}

// =====================================================================
// 错误响应
// =====================================================================

/// 从 JSON 错误响应中提取错误消息
pub fn extract_error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return msg.to_string();
        }
        if let Some(msg) = v.get("error").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
    }
    if body.len() > 200 {
        body[..200].to_string()
    } else {
        body.to_string()
    }
}

/// 生成 Anthropic 格式的错误响应
pub fn anthropic_error_response(error_msg: &str, model: &str) -> String {
    let err = json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": error_msg
        }
    });
    serde_json::to_string(&err).unwrap_or_else(|_| {
        format!(
            r#"{{"type":"error","error":{{"type":"api_error","message":"{}"}}}}"#,
            error_msg
        )
    })
}
