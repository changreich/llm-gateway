# LLM Gateway 设计文档

## 概述

LLM Gateway 是一个基于 Pingora + Lua + reqwest 的反向代理，支持多端口路由、主备切换、Anthropic 格式支持。

## 架构

```
客户端 → Pingora (端口 9090/9089) → Lua (router.lua/router2.lua) → reqwest (上游连接) → LLM API
                                         ↓
                                   Redis (配置存储)
```

**双端口设计：**

| 端口 | 路由脚本 | 用途 |
|------|---------|------|
| 9090 | `router.lua` | 通用路由（OpenAI 兼容 API） |
| 9089 | `router2.lua` | Anthropic 专用路由 |

**组件职责：**

| 组件 | 职责 |
|------|------|
| Pingora | HTTP 服务器（双端口），接收请求、返回响应 |
| Lua | 业务逻辑：配置加载、路由决策、请求体重写 |
| reqwest | 上游 HTTPS 连接（rustls-tls） |
| Redis | 配置存储、状态存储、请求缓存 |

## 请求处理流程

```
1. Pingora 接收 HTTP 请求 (9090 或 9089)
2. 读取请求体 (POST/PUT/PATCH)
3. 根据端口加载对应的 Lua 脚本
4. 调用 Lua handler.on_request(method, path, headers, body)
5. Lua 返回决策：
   - action: "proxy" | "reject"
   - addr: 上游地址 (host:port)
   - tls: 是否使用 HTTPS
   - rewrite_path: 重写后的路径
   - api_key: Authorization 头
   - model: 目标模型 (用于替换请求体)
   - new_body: 替换后的请求体
6. reqwest 发送请求到上游
7. 如果是 9089 端口，进行 Anthropic 格式转换
8. 返回响应给客户端
```

## Anthropic 格式转换 (9089 端口)

9089 端口专门用于 Anthropic API 调用，支持：

1. **请求体转换**: 将 OpenAI 格式转换为 Anthropic 格式
   - `model` → `model`
   - `messages` → `messages`
   - `max_tokens` → `max_tokens`
   - `stream` → `stream` (自动转换为 Anthropic Beta 头)

2. **响应转换**: ��� Anthropic 压缩响应转换为 OpenAI 格式
   - 使用 flate2 解压
   - 拆分多 chunk 组装
   - SSE 流式逐 chunk 转换

3. **连接管理**: 使用连接注册表管理流式连接的生命周期

## Redis Key 设计

| Key | 格式 | 说明 |
|-----|------|------|
| `provider:{name}` | `baseurl\|apikey` | 提供商配置 |
| `llm:{num}` | `provider\|model\|cd` | LLM 配置 |
| `embed:provider` | `provider_name` | Embeddings 提供商 |
| `embed:model` | `model_name` | Embeddings 模型 |
| `rank:provider` | `provider_name` | Rerank 提供商 |
| `rank:model` | `model_name` | Rerank 模型 |
| `llm:select` | `01` | 当前选中的主 LLM 编号 |
| `llm:config:switch_threshold` | `10` | 主备切换阈值 |
| `llm:config:cool_down` | `60` | 默认冷却期（秒） |
| `llm:count:{num}` | `15` | LLM 调用次数 |
| `llm:cool-down:{num}` | `timestamp` | 备用冷却期截止时间 |
| `raw` | `[{...}, {...}, ...]` | 最近 5 个请求原始数据 (router2.lua) |

## 主备切换机制

### 切换逻辑

1. 主 LLM (01) 调用次数达到 `switch_threshold` 时，尝试切换到备用
2. 按顺序遍历 02, 03, 04... 查找不在冷却期的备用
3. 找到可用备用后，设置其冷却期并使用
4. 如果所有备用都在冷却期，回退使用主 LLM

### 流程图

```
请求到达
    ↓
主 LLM 调用次数 < switch_threshold?
    ├─ 是 → 使用主 LLM
    └─ 否 → 遍历备用 02, 03...
              ↓
         备用在冷却期?
              ├─ 是 → 继续下一个
              └─ 否 → 设置冷却期，使用该备用
                     ↓
              所有备用都在冷却期?
                   ├─ 是 → 使用主 LLM
                   └─ 否 → 使用选中的备用
```

## 请求体重写

网关自动替换请求体中的 `model` 字段：

```lua
-- Lua 返回 new_body
return {
    action = "proxy",
    model = "Qwen/Qwen3.5-4B",  -- 目标模型
    new_body = '{"model": "Qwen/Qwen3.5-4B", ...}'  -- 替换后的请求体
}
```

**替换逻辑：**
- 客户端发送 `{"model": "any-model", ...}`
- 网关替换为 `{"model": "配置中的模型", ...}`
- 上游收到正确的模型名称

## API 端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `/debug` | GET | 查看当前路由状态 |
| `/settings` | GET | 查看当前配置项 |
| `/config` | GET | 查看 LLM 配置 |
| `/raw` | GET | 查看最近 5 个请求原始数据 (router2.lua) |
| `/running` | GET | 运行统计 HTML 页面 |
| `/v1/chat/completions` | POST | Chat Completions API (9090) |
| `/openai/v1/chat/completions` | POST | Chat Completions API (9089) |
| `/v1/embeddings` | POST | Embeddings API |
| `/rerank` | POST | Rerank API |

**端口 9090 vs 9089：**

- 9090: 标准 OpenAI 兼容 API (`/v1/chat/completions`)
- 9089: Anthropic 兼容 API (`/openai/v1/chat/completions`)，自动格式转换

## /debug 输出示例

```json
{
  "status": "ok",
  "main": "01",
  "target": "02",
  "fallback": true,
  "main_count": 15,
  "threshold": 10,
  "provider": "siliconflow",
  "rewrite_path": "/v1/chat/completions"
}
```

## 配置文件 (config2.lua - Code 配置)

router2.lua 使用独立的 config2.lua，支持 Code 选择：

```lua
return {
    redis_host = "127.0.0.1",
    redis_port = 7379,
    redis_db = 0,

    selected = "01",

    -- Code 配置
    code = {
        ["01"] = {
            provider = "opengo",
            model = "glm-5.1",
            opt = ""  -- 无选项
        },
        ["02"] = {
            provider = "openzen",
            model = "mimo-v2-pro-free",
            opt = ""  -- 无选项
        }
    }
}
```

**Redis Key 设计 (router2.lua):**

| Key | 格式 | 说明 |
|-----|------|------|
| `code:{num}` | `provider\|model\|opt` | Code 配置 |
| `opt:{id}:{field}` | `value` | 选项字段 |
| `code:select` | `num` | 当前选中的编号 |

```lua
return {
    -- Redis 连接
    redis_host = "127.0.0.1",
    redis_port = 7379,
    redis_db = 0,

    -- 网关配置
    switch_threshold = 2,
    cool_down = 60,

    -- 当前选中的 LLM
    selected = "01",

    -- 提供商配置 (复用)
    providers = {
        siliconflow = {
            baseurl = "https://api.siliconflow.cn/v1",
            apikey = "sk-xxx"
        },
        zhipu = {
            baseurl = "https://open.bigmodel.cn/api/paas/v4",
            apikey = "xxx"
        },
        Local1 = {
            baseurl = "http://127.0.0.1:3333",
            apikey = ""
        }
    },

    -- LLM 配置 (引用 provider)
    llm = {
        ["01"] = { provider = "zhipu", model = "GLM-4-Flash", cd = 0 },
        ["02"] = { provider = "siliconflow", model = "Qwen/Qwen3.5-4B", cd = 15 },
        ["03"] = { provider = "siliconflow", model = "deepseek-ai/DeepSeek-R1-Distill-Qwen-7B", cd = 15 }
    },

    -- Embeddings 配置
    embed = {
        provider = "Local1",
        model = "bge-large-zh-v1.5-q8_0"
    },

    -- Rerank 配置
    rank = {
        provider = "Local2",
        model = "qwen3-reranker-0.6b-q8_0"
    }
}
```

## 环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `REDIS_URL` | Redis 连接 URL | `redis://127.0.0.1:7379` |
| `LLM_LISTEN` | 监听地址 (9090) | `0.0.0.0:9090` |
| `LLM_LISTEN2` | 监听地址 (9089) | `0.0.0.0:9089` |
| `LLM_SCRIPT` | Lua 脚本路径 (9090) | `lua/router.lua` |
| `LLM_SCRIPT2` | Lua 脚本路径 (9089) | `lua/router2.lua` |
| `LLM_TLS_VERIFY` | TLS 证书验证 (1=启用) | `0` (跳过) |
| `LLM_BASEURL` | 默认 API 地址 | `https://api.anthropic.com` |
| `LLM_API_KEY` | 默认 API Key | (空) |
| `LLM_MODEL` | 默认模型 | `claude-sonnet-4-20250514` |

## Lua 函数接口

由 Rust 提供：

| 函数 | 说明 |
|------|------|
| `redis_get(key)` | 获取值 |
| `redis_set(key, value)` | 设置值 |
| `redis_keys(pattern)` | 查找键 |
| `redis_incr(key)` | 递增计数 |
| `redis_expire(key, seconds)` | 设置 TTL |
| `json_decode(str)` | JSON 解码 |
| `openai_call(request_json)` | 调用 OpenAI 兼容 API |
| `openai_chat(messages, model, api_key)` | 简化版聊天接口 |
| `get_default_config()` | 获取默认配置 |

## 构建与运行

```bash
# 构建
cd llm-gateway
cargo build --release

# 运行
./target/release/llm-gateway.exe

# 或使用环境变量
REDIS_URL=redis://127.0.0.1:6379 LLM_LISTEN=0.0.0.0:9090 ./target/release/llm-gateway.exe
```

## 热更新

修改 `lua/router.lua` 后自动重新加载（500ms 冷却），无需重启服务。

## TLS 说明

- 使用 reqwest 的 `rustls-tls` feature
- 默认跳过证书验证（适用于透明代理场景）
- 设置 `LLM_TLS_VERIFY=1` 启用证书验证
