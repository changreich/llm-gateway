# 9089 端口请求转换优化设计

## ✅ 实现状态：已完成 (2026-04-13)

### 已完成的工作

| 项目 | 状态 | 文件 |
|------|------|------|
| SiliconFlow Anthropic SDK | ✅ 完成 | `lua/sdk/sdk_siliconflow_anthropic.lua` |
| router2.lua SDK 集成 | ✅ 完成 | `lua/router2.lua` 第 761-824 行 |
| Rust SSE 透传 | ✅ 已存在 | `src/main.rs` 第 1413-1495 行 |
| 非流式响应透传 | ✅ 已存在 | `src/main.rs` 第 1583 行 |

### 验证方法

```bash
# 配置 Redis
redis-cli -p 7379 SET provider:siliflowa "https://api.siliconflow.cn/anthropic/v1|YOUR_API_KEY"
redis-cli -p 7379 SET code:08 "siliflowa|Pro/zai-org/GLM-5.1|"

# 发送测试请求
curl -X POST http://localhost:9089/xxx/code/xxx \
  -H "Content-Type: application/json" \
  -d '{"model": "haru", "messages": [{"role": "user", "content": "hello"}], "max_tokens": 100}'

# 检查调试键
redis-cli -p 7379 GET code:debug_route
# 期望: host=api.siliconflow.cn|endpoint=/v1/messages|path_prefix=|rewrite_path=/v1/messages|sf_match=true
```

---

## 需求背景

当前 `router2.lua` 的 `rebuild_request_body()` 函数会**无条件**将 Anthropic 格式转换为 OpenAI 格式。但存在以下场景不需要转换：

1. **请求本身是 OpenAI 格式** → 直接透传，无需转换
2. **目标是 Anthropic API** → Anthropic 格式无需转换，直接转发

核心问题：当前 9089 端口的所有请求都做 A→OpenAI 转换，即使目标就是 Anthropic。

## 设计目标

- provider 包含 "anthropic" → 不转换请求体，不转换响应
- 其他 provider → 保持原有转换逻辑

## 判断逻辑

| Provider | 请求方向 | Lua 转换请求? | Rust 转换请求? | Rust 转换响应? |
|---------|---------|:-----------:|:------------:|:------------:|
| anthropic | A→A | ✗ | ✗ | ✗ |
| opengo | A→OpenAI | ✓ | ✓ | ✓ |
| openzen | A→OpenAI | ✓ | ✓ | ✓ |

---

## 伪代码

### Lua 层 (router2.lua)

```lua
--- 判断 provider 是否需要格式转换
-- Anthropic provider 不需要转换，直接透传
local function needs_format_conversion(provider_key)
    if not provider_key then return true end
    local lower_key = string.lower(provider_key)
    -- 配置中 provider 名称包含 "anthropic" → 不需要转换
    if string.find(lower_key, "anthropic") then
        return false
    end
    return true  -- 默认需要转换
end

--- 检测请求体格式
local function detect_request_format(body)
    local ok, orig = pcall(json_decode, body)
    if not ok or type(orig) ~= "table" then return "unknown" end
    -- Anthropic 特征: prompt 字段, content 含 type 字段
    if orig.prompt and not orig.messages then return "anthropic" end
    if orig.messages then
        for _, msg in ipairs(orig.messages) do
            if type(msg.content) == "table" then
                for _, block in ipairs(msg.content) do
                    if type(block) == "table" and block.type then return "anthropic" end
                end
            end
        end
        return "openai"  -- 有 messages 但无 type → OpenAI
    end
    return "unknown"
end

--- 修改 rebuild_request_body: 根据 provider 决定是否转换
local function rebuild_request_body(original_body, model, opt_config, provider_sdk, provider_cfg, provider_key)
    local ok, orig = pcall(json_decode, original_body)
    if not ok or type(orig) ~= "table" then return nil end

    -- ★ 核心：如果 provider 是 Anthropic，不转换，直接透传
    if not needs_format_conversion(provider_key) then
        pcall(redis_set, "code:debug_format",
            "skip_conversion:provider=" .. provider_key)
        local new_body = {}
        new_body.model = model
        for k, v in pairs(orig) do
            if k ~= "model" then new_body[k] = v end
        end
        -- 应用 opt 配置
        for field, value in pairs(opt_config) do
            -- ... (原有 opt 逻辑)
        end
        if provider_sdk and provider_sdk.transform_request then
            return provider_sdk.transform_request(json_encode(new_body), model, provider_cfg)
        end
        return json_encode(new_body)
    end

    -- ★ 非 Anthropic provider：执行原有的 A→OpenAI 转换
    -- ... (原有 rebuild_request_body 逻辑不变)
end

--- 修改 on_request 返回值：增加 need_transform 字段
function handler.on_request(method, path, headers, body)
    -- ... (原有逻辑)
    local need_transform = needs_format_conversion(code_cfg.provider)

    return {
        action = "proxy",
        upstream = code_cfg.provider,
        addr = host,
        tls = use_tls,
        api_key = provider_cfg.apikey,
        model = code_cfg.model,
        rewrite_path = rewrite_path,
        new_request_body = new_body,
        need_transform = need_transform,  -- ★ 新增：传递给 Rust
    }
end
```

### Rust 层 (main.rs)

```rust
// ★ 1. RequestDecision 增加字段
struct RequestDecision {
    // ... 原有字段
    need_transform: bool,  // ★ 新增：Lua 传来的转换标志
}

impl Default for RequestDecision {
    fn default() -> Self {
        Self {
            // ... 原有默认值
            need_transform: true,  // ★ 默认需要转换
        }
    }
}

// ★ 2. GatewayCtx 增加字段
struct GatewayCtx {
    decision: RequestDecision,
    request_body: Vec<u8>,
    response_status: u16,
    need_transform: bool,  // ★ 新增：是否需要转换
}

// ★ 3. request_filter 中的请求体构建逻辑 (原 1100-1129 行)
let request_body = if !decision.new_request_body.is_empty() {
    if self.port == 9089 && stream_requested {
        // 无论 need_transform 与否，Lua 已经处理好了 new_request_body
        // 只需要决定是否注入 stream:true
        if decision.need_transform {
            // A→OpenAI 转换：注入 stream:true
            match inject_stream_true(&decision.new_request_body) {
                Some(body) => body,
                None => decision.new_request_body.clone(),
            }
        } else {
            // ★ A→A 直通：Anthropic 格式已含 stream 字段，直接使用
            decision.new_request_body.clone()
        }
    } else {
        decision.new_request_body.clone()
    }
} else if self.port == 9089 {
    if decision.need_transform {
        // ★ 无 Lua 返回体 + 需要转换：Rust 层兜底转换
        match transform_anthropic_request_to_openai(
            &String::from_utf8_lossy(&body), &target_model_for_upstream,
        ) {
            Some(converted) => {
                if stream_requested {
                    inject_stream_true(&converted).unwrap_or(converted)
                } else {
                    converted
                }
            }
            None => {
                warn!("Rust-level Anthropic→OpenAI conversion failed, using raw body");
                String::from_utf8_lossy(&body).to_string()
            }
        }
    } else {
        // ★ 无 Lua 返回体 + 不需要转换：直接透传原始请求体
        String::from_utf8_lossy(&body).to_string()
    }
} else {
    String::from_utf8_lossy(&body).to_string()
};

// ★ 4. 流式响应路径 (原 1150 行)
if self.port == 9089 && stream_requested {
    if decision.need_transform {
        // ★ A→OpenAI：需要 SSE 流式转换
        // ... 原有的 SSE 流式转换代码不变
    } else {
        // ★ A→A：直接透传 SSE 流，不做格式转换
        // 上游返回的就是 Anthropic 格式 SSE，直接转发
        // ... 透传逻辑
    }
}

// ★ 5. 非流式响应 (原 1293 行)
let final_response_body = if self.port == 9089 {
    if decision.need_transform {
        // ★ 需要转换：原有的 OpenAI→Anthropic 响应转换
        let lua = self.lua.read().unwrap();
        match extract_openai_fields(&response_body) {
            // ... 原有转换逻辑不变
        }
    } else {
        // ★ 不需要转换：直接返回 Anthropic 格式响应
        response_body.clone()
    }
} else {
    response_body.clone()
};
```

---

## 关键改动总结

### Lua 层改动点

| 文件 | 函数 | 改动 |
|------|------|------|
| router2.lua | `needs_format_conversion()` | **新增** - 判断 provider 是否需要转换 |
| router2.lua | `detect_request_format()` | **新增** - 检测请求格式 |
| router2.lua | `rebuild_request_body()` | **修改** - 增加 provider_key 参数，Anthropic 时透传 |
| router2.lua | `handler.on_request()` | **修改** - 传入 provider_key，返回 need_transform |

### Rust 层改动点

| 文件 | 位置 | 改动 |
|------|------|------|
| main.rs | `RequestDecision` | **新增** `need_transform: bool` 字段 |
| main.rs | `GatewayCtx` | **新增** `need_transform: bool` 字段 |
| main.rs | `request_filter` 1100-1129 行 | **修改** 请求体构建：need_transform=false 时跳过转换 |
| main.rs | `request_filter` 1150 行 | **修改** SSE 流式：need_transform=false 时直接透传 |
| main.rs | `request_filter` 1293 行 | **修改** 非流式响应：need_transform=false 时直接返回 |

---

## Redis 调试 Key

| Key | 示例值 | 说明 |
|-----|--------|------|
| `code:debug_format` | `skip_conversion:provider=anthropic` | Lua 层转换决策日志 |

## 配置示例

```lua
-- config2.lua 中 provider 命名规范
code = {
    ["01"] = {
        provider = "opengo",      -- 不含 "anthropic" → 需要转换 A→OpenAI
        model = "glm-5.1"
    },
    ["02"] = {
        provider = "anthropic",    -- 含 "anthropic" → 不需要转换，直接透传
        model = "claude-sonnet-4-20250514"
    },
    ["03"] = {
        provider = "qfcode",       -- 不含 "anthropic" → 需要转换
        model = "qianfan-code-latest"
    }
}
```

## 兼容性

- **默认行为不变**：原有 A→OpenAI 转换逻辑保持不变
- **新增 `need_transform` 字段默认 true**：Rust 结构体默认值为 true，确保向后兼容
- **只有 provider 含 "anthropic" 时才跳过转换**
- **Rust 层双保险**：即使 Lua 层遗漏，Rust 层也会检查 `need_transform` 标志