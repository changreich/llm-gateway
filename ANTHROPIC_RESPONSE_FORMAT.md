# Anthropic Messages API 响应格式

基于 `anthropic-sdk-python-0.94.0` 源码分析，Claude Code 期望的响应格式。

---

## 非流式响应 (Non-Streaming)

```json
{
  "id": "msg_01XFDUDYJgAACzvnptvVo4EL",
  "type": "message",
  "role": "assistant",
  "content": [
    {
      "type": "text",
      "text": "Hello! How can I help you today?"
    }
  ],
  "model": "claude-sonnet-4-20250514",
  "stop_reason": "end_turn",
  "stop_sequence": null,
  "usage": {
    "input_tokens": 25,
    "output_tokens": 50
  }
}
```

### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `id` | string | 是 | 消息ID，格式 `msg_` 前缀 |
| `type` | string | 是 | **必须为 `"message"`** |
| `role` | string | 是 | **必须为 `"assistant"`** |
| `content` | array | 是 | 内容块数组，见下方 |
| `model` | string | 是 | **必须与客户端请求中的model一致** |
| `stop_reason` | string\|null | 是 | 停止原因，见下方 |
| `stop_sequence` | string\|null | 是 | 匹配的停止序列，通常为null |
| `usage` | object | 是 | token用量，见下方 |

### Content Block 类型

#### TextBlock（最常用）
```json
{ "type": "text", "text": "..." }
```

#### ThinkingBlock（扩展思考）
```json
{ "type": "thinking", "thinking": "...", "signature": "..." }
```

#### ToolUseBlock（工具调用）
```json
{ "type": "tool_use", "id": "toolu_...", "name": "get_weather", "input": {...} }
```

### StopReason 取值

| 值 | 含义 |
|----|------|
| `"end_turn"` | 正常结束 |
| `"max_tokens"` | 达到token上限 |
| `"stop_sequence"` | 匹配停止序列 |
| `"tool_use"` | 工具调用 |
| `"pause_turn"` | 暂停 |
| `"refusal"` | 拒绝 |

### Usage 格式

```json
{
  "input_tokens": 100,
  "output_tokens": 50
}
```

> `cache_creation_input_tokens`, `cache_read_input_tokens` 等为可选字段。

---

## 流式响应 (Streaming)

采用 SSE (Server-Sent Events) 格式，事件序列如下：

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":10}}

event: message_stop
data: {"type":"message_stop"}
```

### 流式事件类型

| 事件 | 说明 |
|------|------|
| `message_start` | 包含完整 Message 对象（content为空, stop_reason为null, output_tokens为0） |
| `content_block_start` | 内容块开始，含 `index` 和 `content_block` |
| `content_block_delta` | 增量内容，delta 类型：`text_delta`(text)、`thinking_delta`(thinking)、`input_json_delta`(partial_json)、`signature_delta`(signature) |
| `content_block_stop` | 内容块结束，含 `index` |
| `message_delta` | 最终 stop_reason 和 output_tokens（**output_tokens 是累计值**） |
| `message_stop` | 流结束，事件体为空 `{}` |

---

## 关键注意事项

1. **`type` 字段不可省略** — SDK 使用 `type` 作为判别联合(discriminated union)，缺失会导致反序列化失败
2. **`model` 必须与请求一致** — 客户端发 `claude-sonnet-4-20250514`，响应必须返回相同值（不能返回 OpenAI 端点的 model 名）
3. **`stop_reason` 在非流式中必须为有效值或null** — 不能是 OpenAI 的 `"stop"`，必须映射为 `"end_turn"`
4. **`usage.input_tokens` 是必填字段** — Claude Code 会读取 `usage.input_tokens`，缺失会报 `Cannot read properties of undefined`
5. **若有 tool_calls** — OpenAI 的 `tool_calls` 需转为 Anthropic 的 `tool_use` content block 格式，包含 `id`, `name`, `input`

---

## OpenAI → Anthropic 映射表

| OpenAI 字段 | Anthropic 字段 | 转换规则 |
|-------------|---------------|---------|
| `id` | `id` | 去掉 `chatcmpl-` 前缀，加 `msg_` 前缀 |
| `object: "chat.completion"` | `type: "message"` | 固定值 |
| `choices[0].message.role` | `role` | 固定 `"assistant"` |
| `choices[0].message.content` | `content` | 字符串 → `[{type:"text", text:"..."}]` |
| `choices[0].message.tool_calls` | `content` | 转为 `{type:"tool_use", id, name, input}` |
| `model` | `model` | **使用客户端请求的原始model** |
| `choices[0].finish_reason` | `stop_reason` | `stop`→`end_turn`, `length`→`max_tokens`, `tool_calls`→`tool_use` |
| `null` | `stop_sequence` | 固定 `null` |
| `usage.prompt_tokens` | `usage.input_tokens` | 直接映射 |
| `usage.completion_tokens` | `usage.output_tokens` | 直接映射 |