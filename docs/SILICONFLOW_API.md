# SiliconFlow API 调用规范

## 端点

| 格式 | 端点 | 说明 |
|------|------|------|
| OpenAI | `https://api.siliconflow.cn/v1/chat/completions` | OpenAI 兼容格式 |
| Anthropic | `https://api.siliconflow.cn/v1/messages` | Anthropic 兼容格式 |

**注意**：本项目使用 Anthropic 端点 `https://api.siliconflow.cn/v1/messages`

## 认证

```
Authorization: Bearer YOUR_API_KEY
```

## 支持的模型

常用模型列表（2026-04）：

| 模型 ID | 说明 |
|---------|------|
| `Pro/zai-org/GLM-5.1` | GLM-5.1 高速版 |
| `Pro/zai-org/GLM-4.7` | GLM-4.7 |
| `zai-org/GLM-4.6` | GLM-4.6 |
| `deepseek-ai/DeepSeek-R1` | DeepSeek R1 |
| `deepseek-ai/DeepSeek-V3` | DeepSeek V3 |
| `Qwen/QwQ-32B` | QwQ-32B 推理模型 |

## Anthropic 格式请求示例

```bash
curl --request POST \
  --url https://api.siliconflow.cn/v1/messages \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "Pro/zai-org/GLM-5.1",
    "max_tokens": 4096,
    "messages": [
      {"role": "user", "content": "Hello"}
    ]
  }'
```

## 流式响应

```json
{
  "model": "Pro/zai-org/GLM-5.1",
  "max_tokens": 4096,
  "stream": true,
  "messages": [...]
}
```

SSE 格式与 Anthropic API 兼容。

## 不支持的端点

SiliconFlow Anthropic 兼容端点 **不支持** 以下 Anthropic 原生端点：

| 端点 | 状态 |
|------|------|
| `/v1/messages/count_tokens` | ❌ 不支持 |
| `/v1/models` | ❌ 不支持 |

**解决方案**：如果客户端需要 `count_tokens`，需要在网关层实现本地估算或返回错误提示。

## 网关配置

### Redis 配置

```bash
# Provider 配置（注意：baseurl 使用 /anthropic/v1 后缀，SDK 会自动重写）
redis-cli -p 7379 SET provider:siliflowa "https://api.siliconflow.cn/anthropic/v1|YOUR_API_KEY"

# Code 配置
redis-cli -p 7379 SET code:08 "siliflowa|Pro/zai-org/GLM-5.1|"
```

### SDK 处理逻辑

`lua/sdk/sdk_siliconflow_anthropic.lua` 会自动处理：

1. **baseurl 重写**：`https://api.siliconflow.cn/anthropic/v1` → `https://api.siliconflow.cn`
2. **路径设置**：`/v1/messages`
3. **格式透传**：不转换 Anthropic 格式请求/响应

## 错误处理

### 400 Bad Request

常见原因：
- 模型名称错误
- 请求格式不正确
- 请求了不支持的端点（如 `count_tokens`）

### 401 Unauthorized

- API Key 无效或过期

### 429 Too Many Requests

- 触发速率限制，需要重试

## 参考文档

- [SiliconFlow API 文档](https://docs.siliconflow.cn/cn/api-reference/)
- [创建对话请求（Anthropic）](https://docs.siliconflow.cn/cn/api-reference/chat-completions/anthropic-chat-completions)
