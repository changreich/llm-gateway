# LLM Gateway SDK 设计文档

## 概述

LLM Gateway 采用 SDK 模块化设计，实现请求/响应的完全转换。

### 架构流程

```
客户端请求 (OpenAI 格式)
        ↓
    SDK.transform_request()  → 转换为提供商格式
        ↓
    提供商 API (Anthropic/智谱/SiliconFlow 等)
        ↓
    SDK.transform_response() → 转换回 OpenAI 格式
        ↓
返回给客户端
```

## 目录结构

```
lua/
├── router.lua          # 主路由逻辑
├── config.lua          # 配置文件
└── sdk/
    ├── init.lua        # SDK 加载器
    ├── base.lua        # 基础 SDK 类
    ├── openai.lua      # OpenAI 兼容格式
    ├── anthropic.lua   # Anthropic (Claude) 格式
    ├── zhipu.lua       # 智谱 AI 格式
    └── siliconflow.lua # SiliconFlow 格式
```

## SDK 接口

每个 SDK 模块实现以下接口：

```lua
-- 获取端点路径
function sdk.get_endpoint() -> string

-- 获取额外请求头
function sdk.get_extra_headers(api_key) -> table

-- 转换请求：OpenAI 格式 → 提供商格式
function sdk.transform_request(body_str, model, config) -> string

-- 转换响应：提供商格式 → OpenAI 格式
function sdk.transform_response(response_str) -> string

-- 提取 token 用量
function sdk.extract_tokens(response_str) -> (prompt_tokens, completion_tokens)
```

## 统一请求格式 (OpenAI)

所有请求使用 OpenAI Chat Completions API 格式：

```json
{
  "model": "gpt-4",
  "messages": [
    {"role": "system", "content": "You are a helpful assistant."},
    {"role": "user", "content": "Hello!"}
  ],
  "max_tokens": 4096,
  "temperature": 0.7
}
```

## 提供商差异

### Anthropic

- 端点: `/v1/messages`
- 认证: `x-api-key` header
- 版本: `anthropic-version: 2023-06-01`
- system 消息在顶级字段，非 messages 数组

### 智谱 AI

- 端点: `/api/paas/v4/chat/completions`
- 认证: `Authorization: Bearer`
- 基本兼容 OpenAI 格式

### SiliconFlow

- 端点: `/v1/chat/completions`
- 完全兼容 OpenAI 格式
- 模型名格式: `owner/model` (如 `Qwen/Qwen3.5-4B`)

## 使用示例

### 添加新提供商

1. 创建 `lua/sdk/newprovider.lua`:

```lua
local newprovider = {}

function newprovider.get_endpoint()
    return "/v1/chat/completions"
end

function newprovider.get_extra_headers(api_key)
    return {
        ["Authorization"] = "Bearer " .. api_key
    }
end

function newprovider.transform_request(body_str, model, config)
    -- 转换请求
    return body_str:gsub('"model"%s*:%s*"[^"]*"', '"model": "' .. model .. '"')
end

function newprovider.transform_response(response_str)
    -- 转换响应
    return response_str
end

function newprovider.extract_tokens(response_str)
    local p = response_str:match('"prompt_tokens"%s*:%s*(%d+)') or "0"
    local c = response_str:match('"completion_tokens"%s*:%s*(%d+)') or "0"
    return tonumber(p), tonumber(c)
end

return newprovider
```

2. 在 `config.lua` 中添加提供商配置：

```lua
providers = {
    newprovider = {
        baseurl = "https://api.newprovider.com",
        apikey = "your-api-key"
    }
}
```

## 测试

```bash
# 测试当前配置
curl http://localhost:9090/test

# 查看 debug 信息
curl http://localhost:9090/debug

# 查看配置
curl http://localhost:9090/config
```
