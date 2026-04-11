# Anthropic Messages API ↔ OpenAI Chat Completions API 协议对照表

## 1. 请求字段对照 (Anthropic → OpenAI)

| 类别 | Anthropic 字段 | OpenAI 字段 | 类型差异 | 转换规则 |
|---|---|---|---|---|
| 顶层 | `model` | `model` | string → string | 替换为上游模型名 |
| 顶层 | `max_tokens` | `max_tokens` | number → number | 直接透传 |
| 顶层 | `temperature` | `temperature` | number → number | 直接透传 |
| 顶层 | `top_p` | `top_p` | number → number | 直接透传 |
| 顶层 | `top_k` | — | number | OpenAI 无此字段，丢弃 |
| 顶层 | `stop_sequences` | `stop` | string[] → string/string[] | 直接映射 |
| 顶层 | `stream` | `stream` | boolean → boolean | **必须透传**，缺失=false |
| 顶层 | `system` | `messages[0]` (role=system) | string/string[] → message | string 作 content；数组取每个 `.text` 拼接 |
| 顶层 | `messages` | `messages` | 格式不同 | 见下方 messages 对照 |
| 顶层 | `tools` | `tools` | 格式不同 | 见下方 tools 对照 |
| 顶层 | `tool_choice` | `tool_choice` | 格式不同 | 见下方 tool_choice 对照 |
| 顶层 | `metadata` | — | object | OpenAI 无此字段，丢弃 |
| 顶层 | `anthropic_version` | — | string | OpenAI 无此字段，丢弃 |
| 顶层 | — | `presence_penalty` | number | Anthropic 无此字段 |
| 顶层 | — | `frequency_penalty` | number | Anthropic 无此字段 |
| 顶层 | — | `seed` | integer | Anthropic 无此字段 |
| 顶层 | — | `logprobs` | object | Anthropic 无此字段 |

### 1.1 Messages 对照

| Anthropic message | OpenAI message | 说明 |
|---|---|---|
| `{"role":"user", "content":"text"}` | `{"role":"user", "content":"text"}` | 纯字符串，直接透传 |
| `{"role":"user", "content":[{"type":"text","text":"..."}]}` | `{"role":"user", "content":"..."}` | content 数组只有 text block → 拼接为字符串 |
| `{"role":"user", "content":[{"type":"image","source":{...}}]}` | `{"role":"user", "content":[{"type":"image_url","image_url":{...}}]}` | 图片格式需转换 |
| `{"role":"assistant", "content":"text"}` | `{"role":"assistant", "content":"text"}` | 纯字符串 |
| `{"role":"assistant", "content":[{"type":"text","text":"..."}]}` | `{"role":"assistant", "content":"..."}` | 拼接为字符串 |
| `{"role":"assistant", "content":[{"type":"tool_use","id":"...","name":"...","input":{...}}]}` | `{"role":"assistant", "tool_calls":[{"id":"...","type":"function","function":{"name":"...","arguments":"..."}}]}` | tool_use → tool_calls |
| `{"role":"user", "content":[{"type":"tool_result","tool_use_id":"...","content":"..."}]}` | `{"role":"tool", "tool_call_id":"...", "content":"..."}` | tool_result → tool role message |

### 1.2 Content Block 类型对照

| Anthropic content block | OpenAI 对应 | 说明 |
|---|---|---|
| `{"type":"text", "text":"..."}` | `content: "..."` (string) | 文本块 |
| `{"type":"image", "source":{"type":"base64",...}}` | `{"type":"image_url", "image_url":{"url":"data:..."}}` | 图片 |
| `{"type":"tool_use", "id":"toolu_xxx", "name":"fn", "input":{...}}` | `{"type":"function","id":"toolu_xxx","function":{"name":"fn","arguments":"{...}"}}` | 工具调用 |
| `{"type":"tool_result", "tool_use_id":"toolu_xxx", "content":"..."}` | `{"role":"tool","tool_call_id":"toolu_xxx","content":"..."}` | 工具结果（独立为一条 message） |
| `{"type":"thinking", "thinking":"...","signature":"..."}` | — | 扩展思考，OpenAI 无对应 |

### 1.3 Tools 对照

| Anthropic | OpenAI | 说明 |
|---|---|---|
| `{"name":"fn", "description":"...", "input_schema":{...}}` | `{"type":"function","function":{"name":"fn","description":"...","parameters":{...}}}` | `input_schema` → `parameters` |

### 1.4 tool_choice 对照

| Anthropic | OpenAI | 说明 |
|---|---|---|
| `"auto"` | `"auto"` | 直接映射 |
| `"any"` | `"required"` | any → required |
| `{"type":"tool","name":"fn"}` | `{"type":"function","function":{"name":"fn"}}` | 指定工具 |
| `{"type":"auto"}` | `"auto"` | 对象形式 → 字符串 |

---

## 2. 响应字段对照 (OpenAI → Anthropic)

| 类别 | OpenAI 字段 | Anthropic 字段 | 转换规则 |
|---|---|---|---|
| 顶层 | `id: "chatcmpl-xxx"` | `id: "msg_xxx"` | 去掉 `chatcmpl-` 前缀，加 `msg_` |
| 顶层 | `object: "chat.completion"` | `type: "message"` | 固定值替换 |
| 顶层 | — | `role: "assistant"` | 固定值 |
| 顶层 | `model: "gpt-4o"` | `model: "claude-..."` | **必须用客户端原始请求的 model** |
| 顶层 | — | `stop_sequence: null` | 固定 null |
| choices[0] | `message.content` | `content: [{type:"text", text:"..."}]` | string → text block 数组 |
| choices[0] | `message.tool_calls` | `content: [{type:"tool_use", id, name, input}]` | tool_calls → tool_use content block |
| choices[0] | `finish_reason` | `stop_reason` | 见下方映射表 |
| usage | `prompt_tokens` | `usage.input_tokens` | 直接数值映射 |
| usage | `completion_tokens` | `usage.output_tokens` | 直接数值映射 |
| usage | `total_tokens` | — | Anthropic 不需要 |

### 2.1 finish_reason ↔ stop_reason

| OpenAI `finish_reason` | Anthropic `stop_reason` | 说明 |
|---|---|---|
| `"stop"` | `"end_turn"` | 正常结束 |
| `"length"` | `"max_tokens"` | 达到 token 上限 |
| `"tool_calls"` | `"tool_use"` | 工具调用 |
| `"content_filter"` | `"refusal"` | 内容过滤（近似） |

### 2.2 tool_calls ↔ tool_use content block

| OpenAI tool_calls | Anthropic tool_use | 说明 |
|---|---|---|
| `id: "call_xxx"` | `id: "call_xxx"` | ID 直接映射 |
| `type: "function"` | — | OpenAI 固定值，Anthropic 无此字段 |
| `function.name: "fn"` | `name: "fn"` | 直接映射 |
| `function.arguments: "{...}"` (string) | `input: {...}` (object) | **字符串 → JSON 对象** |

---

## 3. SSE 流式事件对照

### 3.1 Anthropic SSE 事件序列

```
event: message_start       → 包含完整 message 对象 (content=[], stop_reason=null, output_tokens=0)
event: content_block_start  → {index, content_block: {type, ...}}
event: content_block_delta  → {index, delta: {type, ...}}  (可多次)
event: content_block_stop   → {index}
event: message_delta        → {delta: {stop_reason}, usage: {output_tokens}}
event: message_stop         → {}
```

### 3.2 OpenAI SSE 事件序列

```
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"role":"assistant"},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"content":"Hello"},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"delta":{"tool_calls":[...]},"index":0}]}
data: {"id":"chatcmpl-xxx","object":"chat.completion.chunk","choices":[{"finish_reason":"stop","index":0}]}
data: [DONE]
```

### 3.3 SSE 事件类型映射

| Anthropic 事件 | 对应 OpenAI chunk | 说明 |
|---|---|---|
| `message_start` | `delta: {role: "assistant"}` | 首个 chunk 含 role |
| `content_block_start` (text) | `delta: {content: ""}` | 文本块开始 |
| `content_block_delta` (text_delta) | `delta: {content: "..."}` | 文本增量 |
| `content_block_start` (tool_use) | `delta: {tool_calls: [{function:{name:"..."}}]}` | 工具调用开始 |
| `content_block_delta` (input_json_delta) | `delta: {tool_calls: [{function:{arguments:"..."}}]}` | 工具参数增量 |
| `content_block_stop` | — | OpenAI 无对应事件 |
| `message_delta` | `delta: {}, finish_reason: "stop"` | 流结束信号 |
| `message_stop` | `data: [DONE]` | 流终止标记 |

### 3.4 SSE delta 类型对照

| Anthropic delta 类型 | OpenAI 对应 | 说明 |
|---|---|---|
| `{"type":"text_delta","text":"..."}` | `{"content":"..."}` | 文本增量 |
| `{"type":"thinking_delta","thinking":"..."}` | — | 扩展思考，OpenAI 无对应 |
| `{"type":"input_json_delta","partial_json":"..."}` | `{"tool_calls":[{"function":{"arguments":"..."}}]}` | 工具参数增量 |
| `{"type":"signature_delta","signature":"..."}` | — | 扩展思考签名 |

---

## 关键差异总结

1. **content 结构不同** — Anthropic 用 `content` 数组（type-discriminated blocks），OpenAI 用 `content` 字符串 + 独立 `tool_calls` 数组
2. **system 位置不同** — Anthropic 的 `system` 是顶层字段，OpenAI 的 system 是 `messages[0]`
3. **SSE 事件模型不同** — Anthropic 有 6 种命名事件类型，OpenAI 只有 `chat.completion.chunk` 一种 + `[DONE]` 终止标记
4. **tool 调用归属不同** — Anthropic 的 `tool_use` 是 content block 的一种，OpenAI 的 `tool_calls` 是 message 的独立字段
5. **model 必须回传** — 响应中的 `model` 必须返回客户端原始请求的 model 名，而非上游实际模型名
6. **arguments vs input** — OpenAI 的 `function.arguments` 是 JSON 字符串，Anthropic 的 `input` 是 JSON 对象
7. **tool_result 是独立 message** — Anthropic 的 `tool_result` 在 user message 的 content 数组中，OpenAI 对应独立的 `role: "tool"` message