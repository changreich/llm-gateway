# SSE 流式响应处理设计文档

## 概述

9089 端口接收 Anthropic 格式请求，需要将 OpenAI SSE 流式响应实时转换为 Anthropic SSE 流式响应返回客户端。

此模块完全在 Rust 层实现，不经过 Lua。

## 数据流

```
[Client] POST /xxx/code/xxx (stream:true, Anthropic format)
    │
    ▼
[Lua] on_request → 路由/模型决策 (不含stream参数)
    │
    ▼
[Rust] 检测 stream_requested → 注入 "stream":true 到请求体
    │
    ▼
[Upstream] OpenAI SSE 流式响应
    │  data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"role":"assistant"},"index":0}]}
    │  data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"content":"Hello"},"index":0}]}
    │  data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"content":" world"},"index":0}]}
    │  data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{},"finish_reason":"stop"}]}
    │  data: [DONE]
    │
    ▼ (逐 chunk 读取并转换)
[Rust] SseStreamState + transform_openai_sse_chunk_to_anthropic()
    │  每个 OpenAI chunk → 1~3 个 Anthropic SSE 事件
    │
    ▼
[Client] 实时收到 Anthropic SSE 流:
    event: message_start
    data: {"type":"message_start","message":{...}}

    event: content_block_start
    data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

    event: content_block_delta
    data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

    event: content_block_delta
    data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}

    event: content_block_stop
    data: {"type":"content_block_stop","index":0}

    event: message_delta
    data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":10}}

    event: message_stop
    data: {"type":"message_stop"}
```

## SSE 事件映射详解

### OpenAI SSE 事件序列

```
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"role":"assistant"},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"content":"Hello"},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"content":"!"},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_xxx","type":"function","function":{"name":"get_weather","arguments":""}}]},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"lo"}}]},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"cation"}}]},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\": \"NYC\"}"}}]},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{},"finish_reason":"tool_calls"}]}
data: [DONE]
```

### Anthropic SSE 事件序列

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_xxx","type":"message","role":"assistant","content":[],"model":"claude-...","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_xxx","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"lo"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"cation"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\": \"NYC\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":50}}

event: message_stop
data: {"type":"message_stop"}
```

### 转换规则映射表

| OpenAI chunk 特征 | Anthropic 事件 | 状态变化 |
|---|---|---|
| `delta.role:"assistant"` (首个chunk) | `message_start` | `started=true` |
| `delta.content:"..."` (首次出现) | `content_block_start`(text) + `content_block_delta`(text_delta) | `in_content_block=true, content_block_index++` |
| `delta.content:"..."` (后续) | `content_block_delta`(text_delta) | 无变化 |
| `delta.tool_calls[N]` 含 `id+name` (新工具) | 若前有content_block则先发 `content_block_stop`，再发 `content_block_start`(tool_use) | `in_content_block=true, content_block_index++, 记录tool_id` |
| `delta.tool_calls[N]` 含 `function.arguments` (增量) | `content_block_delta`(input_json_delta) | 无变化 |
| `finish_reason:"stop"` | `content_block_stop` + `message_delta`(stop_reason:end_turn) + `message_stop` | 流结束 |
| `finish_reason:"length"` | `content_block_stop` + `message_delta`(stop_reason:max_tokens) + `message_stop` | 流结束 |
| `finish_reason:"tool_calls"` | `content_block_stop` + `message_delta`(stop_reason:tool_use) + `message_stop` | 流结束 |
| `data: [DONE]` | 确保 `message_stop` 已发送 | 流结束 |

### finish_reason → stop_reason 映射

| OpenAI | Anthropic |
|---|---|
| `"stop"` | `"end_turn"` |
| `"length"` | `"max_tokens"` |
| `"tool_calls"` | `"tool_use"` |
| `"content_filter"` | `"refusal"` |

## 状态机

```rust
pub struct SseStreamState {
    pub started: bool,               // 是否已发送 message_start
    pub content_block_index: usize,  // 当前 content block 索引
    pub in_content_block: bool,      // 是否在某个 content block 中
    pub current_block_type: String,  // "text" | "tool_use"
    pub tool_ids: Vec<String>,       // 已出现的 tool call IDs
    pub model: String,               // 来自原始请求的 model 名
    pub msg_id: String,              // 从 OpenAI 响应提取的 id（转换前缀）
    pub input_tokens: u64,           // 从 OpenAI usage 提取
    pub output_tokens: u64,          // 累积的 output tokens
}
```

### 状态转移

```
INIT → (首个chunk) → STARTED (发送 message_start)
STARTED → (content出现) → IN_TEXT_BLOCK (发送 content_block_start + delta)
IN_TEXT_BLOCK → (更多content) → IN_TEXT_BLOCK (发送 delta)
IN_TEXT_BLOCK → (tool_calls出现) → IN_TOOL_BLOCK (发送 content_block_stop + start)
IN_TOOL_BLOCK → (更多arguments) → IN_TOOL_BLOCK (发送 delta)
IN_TOOL_BLOCK → (新tool_call) → IN_TOOL_BLOCK (发送 stop + start, index++)
IN_*_BLOCK → (finish_reason) → ENDED (发送 stop + message_delta + message_stop)
```

## SSE 行解析

TCP 可能将 SSE 数据分片到达，需要缓冲：

```
收到: "da"
收到: "ta: {\"id\":\"chatcmpl-"
收到: "xxx\",...}\n\nda"
收到: "ta: {...}\n\n"
```

解析逻辑：
1. 将收到的字节追加到缓冲区
2. 按 `\n` 分割，提取完整行
3. 忽略空行（SSE 事件间的分隔符）
4. 提取 `data: ` 开头的行，取其后的 JSON
5. 遇到 `data: [DONE]` 表示流结束

## 实现文件

- `src/sse_stream.rs` — SSE 流式状态机和转换逻辑
- `src/main.rs` — 9089 端口流式响应分支（注入 stream:true + 逐 chunk 转发）
- `src/anthropic_convert.rs` — 保持不变（非流式路径继续使用）

## 与非流式路径的关系

- **非流式路径**（`stream:false` 或缺失）：保持 Rust(提取) → Lua(映射) → Rust(组装) 分层处理
- **流式路径**（`stream:true`）：完全在 Rust 层处理，Lua 仅参与请求路由
- `wrap_as_sse()` 函数保留但不再被流式场景调用

## SSE 连接注册表

每个 SSE 流式请求注册一个应用层"连接"，追踪客户端请求 ID 和服务端 OpenAI SSE ID 的映射关系。

### 数据结构

```rust
pub struct SseConnection {
    pub id: u64,                      // 自增连接 ID
    pub client_request_id: String,   // 客户端 Anthropic 请求中的 id（可能为空）
    pub openai_sse_id: String,       // OpenAI SSE 流中的 chatcmpl-xxx（首个 chunk 时更新）
    pub model: String,               // 客户端请求的 model
    pub created_at: u64,             // 创建时间（Unix ms）
    pub finished_at: Option<u64>,    // 结束时间（Unix ms，流结束时设置）
}
```

### API

| 函数 | 说明 |
|---|---|
| `sse_register(client_req_id, openai_sse_id, model)` | 注册新连接，返回 `conn_id` |
| `sse_update_openai_id(conn_id, openai_sse_id)` | 首个 chunk 到达时更新 OpenAI SSE ID |
| `sse_unregister(conn_id)` | 流结束后销毁连接 |
| `sse_get_active_connections()` | 获取所有活跃连接快照 |
| `sse_active_count()` | 获取活跃连接数量 |

### 监控端点

- `/sse` — 返回 JSON 格式的活跃连接列表
- `/running` — HTML 页面新增 SSE 流连接表

### 生命周期

```
[SSE 流开始]
    ↓
sse_register(client_req_id="", openai_sse_id="", model="claude-...")
    ↓ 返回 conn_id
[首个 OpenAI chunk 到达]
    ↓
sse_update_openai_id(conn_id, "chatcmpl-abc123")
    ↓
[逐 chunk 转换...]
    ↓
[流结束]
    ↓
sse_unregister(conn_id)
```

## 错误处理

1. 上游返回非 200 状态码 → 包装为 Anthropic 错误事件流
2. SSE 解析失败 → 跳过该 chunk，记录日志
3. 流中途断开 → 发送 `message_stop` 确保客户端能正常结束