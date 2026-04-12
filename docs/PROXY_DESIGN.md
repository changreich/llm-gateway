# LLM Gateway 代理控制设计

## 需求背景

当前 LLM Gateway 使用 reqwest 直接连接上游 API，没有代理支持。需要增加：

1. **全局代理配置** - 所有请求默认使用的代理
2. **精确到模型的代理控制** - 每个 provider/code 配置独立代理

## 设计目标

- 支持多种代理协议：HTTP(S)、SOCKS5
- 代理可按 provider 或 code 配置覆盖
- 未配置代理时直连（向后兼容）
- 配置存储在 Redis，支持热更新

## 配置层级（优先级从高到低）

```
code:{num} (模型级代理) → provider:{name} (提供商级代理) → 全局代理 → 直连
```

| 层级 | Redis Key | 示例值 | 优先级 |
|------|-----------|--------|--------|
| 模型级 | `code:{num}:proxy` | `http://127.0.0.1:34010` | 最高 |
| 提供商级 | `provider:{name}:proxy` | `socks5://127.0.0.1:1080` | 中 |
| 全局 | `global:proxy` | `http://127.0.0.1:7890` | 低 |
| 无 | - | (直连) | 默认 |

## Redis Key 设计

| Key | 格式 | 说明 |
|-----|------|------|
| `global:proxy` | `proxy_url` | 全局代理配置 |
| `provider:{name}:proxy` | `proxy_url` | 提供商代理配置 |
| `code:{num}:proxy` | `proxy_url` | 模型代理配置 |

**proxy_url 格式：**
- HTTP 代理：`http://host:port` 或 `https://host:port`
- SOCKS5 代理：`socks5://host:port` 或 `socks5://user:pass@host:port`
- 空/不存在：不使用代理

## 配置文件格式 (config2.lua)

```lua
return {
    -- 全局代理 (可选)
    proxy = "http://127.0.0.1:7890",

    -- 提供商配置
    providers = {
        anthropic = {
            baseurl = "https://api.anthropic.com",
            apikey = "sk-xxx",
            proxy = "socks5://127.0.0.1:1080"  -- 提供商级代理
        },
        siliconflow = {
            baseurl = "https://api.siliconflow.cn/v1",
            apikey = "sk-xxx"
            -- 无 proxy，使用全局代理或直连
        }
    },

    -- Code 配置
    code = {
        ["01"] = {
            provider = "anthropic",
            model = "claude-sonnet-4-20250514",
            opt = "",
            proxy = ""  -- 空字符串表示强制直连，覆盖 provider 代理
        },
        ["02"] = {
            provider = "siliconflow",
            model = "Qwen/Qwen3.5-4B",
            opt = "",
            proxy = "http://127.0.0.1:34010"  -- 模型级代理
        }
    }
}
```

## Lua 层改动

### router2.lua

```lua
--- 获取代理配置
-- 优先级: code:{num}:proxy > provider:{name}:proxy > global:proxy
-- @param code_num string 配置编号
-- @param provider_name string 提供商名称
-- @return string|nil 代理URL，nil表示直连
local function get_proxy_config(code_num, provider_name)
    -- 1. 模型级代理
    local code_proxy = safe_redis_get("code:" .. code_num .. ":proxy")
    if code_proxy then
        if code_proxy == "" then
            return nil  -- 空字符串强制直连
        end
        return code_proxy
    end

    -- 2. 提供商级代理
    local provider_proxy = safe_redis_get("provider:" .. provider_name .. ":proxy")
    if provider_proxy then
        if provider_proxy == "" then
            return nil
        end
        return provider_proxy
    end

    -- 3. 全局代理
    local global_proxy = safe_redis_get("global:proxy")
    if global_proxy and global_proxy ~= "" then
        return global_proxy
    end

    -- 4. 无代理，直连
    return nil
end

--- on_request 返回值增加 proxy 字段
function handler.on_request(method, path, headers, body)
    -- ... 原有逻辑

    local proxy_url = get_proxy_config(selected, code_cfg.provider)

    return {
        action = "proxy",
        upstream = code_cfg.provider,
        addr = host,
        tls = use_tls,
        api_key = provider_cfg.apikey,
        model = code_cfg.model,
        rewrite_path = rewrite_path,
        new_request_body = new_body,
        need_transform = need_transform,
        proxy = proxy_url  -- 新增：代理URL
    }
end
```

## Rust 层改动

### main.rs

```rust
// RequestDecision 增加字段
struct RequestDecision {
    // ... 原有字段
    proxy: Option<String>,  // 新增：代理URL
}

// 创建 reqwest 客户端时配置代理
let mut client_builder = reqwest::Client::builder()
    .danger_accept_invalid_certs(!*SKIP_TLS_VERIFY || !decision.tls);

// 配置代理
if let Some(proxy_url) = &decision.proxy {
    match reqwest::Proxy::all(proxy_url) {
        Ok(proxy) => {
            client_builder = client_builder.proxy(proxy);
            info!("Using proxy: {}", proxy_url);
        }
        Err(e) => {
            warn!("Invalid proxy URL {}: {}", proxy_url, e);
        }
    }
}

let client = client_builder.build()
    .map_err(|e| Error::explain(ErrorType::InternalError, format!("create client: {}", e)))?;
```

## 环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `LLM_PROXY` | 全局代理 URL | (无) |

环境变量优先级最低，被 config2.lua 中的 `proxy` 配置覆盖。

## 使用场景

### 场景 1：全部走代理
```lua
proxy = "http://127.0.0.1:7890"  -- 全局代理
```

### 场景 2：特定提供商走代理
```lua
providers = {
    anthropic = {
        baseurl = "https://api.anthropic.com",
        apikey = "sk-xxx",
        proxy = "socks5://127.0.0.1:1080"  -- 只有 Anthropic 走代理
    },
    local = {
        baseurl = "http://127.0.0.1:8080",
        apikey = ""  -- 无 proxy，直连
    }
}
```

### 场景 3：特定模型走代理
```lua
code = {
    ["01"] = {
        provider = "anthropic",
        model = "claude-sonnet-4-20250514",
        proxy = "http://127.0.0.1:34010"  -- 这个模型走指定代理
    },
    ["02"] = {
        provider = "anthropic",
        model = "claude-opus-4-20250514",
        proxy = ""  -- 强制直连，即使 provider 有代理配置
    }
}
```

### 场景 4：通过 Redis 动态切换
```bash
# 设置全局代理
redis-cli SET global:proxy "http://127.0.0.1:7890"

# 设置提供商代理
redis-cli SET provider:anthropic:proxy "socks5://127.0.0.1:1080"

# 设置模型代理
redis-cli SET code:01:proxy "http://127.0.0.1:34010"

# 清除代理（直连）
redis-cli DEL code:01:proxy
redis-cli SET code:01:proxy ""  # 或设置空字符串强制直连
```

## 代理协议支持

| 协议 | URL 格式 | 说明 |
|------|----------|------|
| HTTP | `http://host:port` | HTTP CONNECT 代理 |
| HTTPS | `https://host:port` | HTTPS 代理 |
| SOCKS5 | `socks5://host:port` | SOCKS5 代理 |
| SOCKS5 认证 | `socks5://user:pass@host:port` | 带认证的 SOCKS5 |

## 实现步骤

1. **config2.lua 改动**
   - 增加 `proxy` 全局配置
   - provider 增加 `proxy` 字段
   - code 增加 `proxy` 字段

2. **router2.lua 改动**
   - 增加 `get_proxy_config()` 函数
   - `init_config_to_redis()` 写入代理配置到 Redis
   - `on_request()` 返回 `proxy` 字段

3. **main.rs 改动**
   - `RequestDecision` 增加 `proxy` 字段
   - 创建 reqwest 客户端时配置代理
   - 日志输出代理使用情况

4. **文档更新**
   - 更新 DESIGN.md
   - 添加代理配置说明

## 调试 Key

| Key | 说明 |
|-----|------|
| `code:debug_proxy` | 当前请求使用的代理 |

## 向后兼容

- 所有代理配置都是可选的
- 未配置代理时行为与现在一致（直连）
- 环境变量 `LLM_PROXY` 作为最低优先级配置
