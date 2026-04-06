-- Anthropic SDK
--
-- 适用于：Claude API
-- 请求格式：Anthropic Messages API
-- 响应格式：Anthropic Messages API
--
-- 参考文档：https://docs.anthropic.com/en/api/messages
--
-- 关键差异：
-- 1. 端点：/v1/messages (非 /v1/chat/completions)
-- 2. 认证：x-api-key header (非 Authorization: Bearer)
-- 3. 版本：anthropic-version header 必需
-- 4. 消息格式：messages 中的 content 可以是字符串或数组

local anthropic = {}

-- 端点路径
function anthropic.get_endpoint()
    return "/v1/messages"
end

-- 额外请求头
function anthropic.get_extra_headers(api_key)
    return {
        ["x-api-key"] = api_key,
        ["anthropic-version"] = "2023-06-01",
        ["Content-Type"] = "application/json"
    }
end

-- 转换请求：OpenAI 格式 -> Anthropic 格式
-- body_str: OpenAI 格式的 JSON 字符串
-- model: 目标模型名称 (claude-sonnet-4-20250514 等)
-- config: 提供商配置
function anthropic.transform_request(body_str, model, config)
    -- 使用 json_decode 函数（由 Rust 注册）
    local ok, body_table = pcall(json_decode, body_str)
    if not ok or not body_table then
        return '{"model":"' .. model .. '","messages":[],"max_tokens":4096}'
    end

    -- 构建 Anthropic 格式请求
    local anthropic_request = {
        model = model,
        max_tokens = body_table.max_tokens or 4096,
        messages = {}
    }

    -- 转换 messages
    -- OpenAI: {"role": "user", "content": "hello"}
    -- Anthropic: {"role": "user", "content": "hello"} (相同，但 content 可以是数组)
    local messages = body_table.messages
    if messages then
        for i, msg in ipairs(messages) do
            local role = msg.role
            local content = msg.content

            -- 跳过 system 消息（Anthropic 使用顶级 system 字段）
            if role == "system" then
                anthropic_request.system = content
            else
                table.insert(anthropic_request.messages, {
                    role = role,
                    content = content
                })
            end
        end
    end

    -- 处理流式请求
    if body_table.stream then
        anthropic_request.stream = body_table.stream
    end

    -- 处理其他参数
    if body_table.temperature then
        anthropic_request.temperature = body_table.temperature
    end
    if body_table.top_p then
        anthropic_request.top_p = body_table.top_p
    end
    if body_table.stop then
        anthropic_request.stop_sequences = body_table.stop
    end

    -- 序列化回 JSON（使用简单的字符串构建，因为 Lua 没有内置 JSON 库）
    return serialize_json(anthropic_request)
end

-- 转换响应：Anthropic 格式 -> OpenAI 格式
-- response_str: Anthropic 格式的 JSON 字符串
function anthropic.transform_response(response_str)
    local ok, body = pcall(json_decode, response_str)
    if not ok or not body then
        return response_str
    end

    -- Anthropic 响应格式：
    -- {
    --   "id": "msg_xxx",
    --   "type": "message",
    --   "role": "assistant",
    --   "content": [{"type": "text", "text": "..."}],
    --   "model": "claude-sonnet-4-20250514",
    --   "usage": {"input_tokens": 10, "output_tokens": 20}
    -- }
    --
    -- OpenAI 响应格式：
    -- {
    --   "id": "chatcmpl-xxx",
    --   "object": "chat.completion",
    --   "model": "gpt-4",
    --   "choices": [{"message": {"role": "assistant", "content": "..."}}],
    --   "usage": {"prompt_tokens": 10, "completion_tokens": 20}
    -- }

    -- 提取 content
    local content = ""
    if body.content and type(body.content) == "table" then
        for _, item in ipairs(body.content) do
            if item.type == "text" and item.text then
                content = content .. item.text
            end
        end
    end

    -- 构建 OpenAI 格式响应
    local openai_response = {
        id = body.id or "chatcmpl-anthropic",
        object = "chat.completion",
        created = os.time(),
        model = body.model or "claude",
        choices = {
            {
                index = 0,
                message = {
                    role = "assistant",
                    content = content
                },
                finish_reason = body.stop_reason or "stop"
            }
        },
        usage = {
            prompt_tokens = body.usage and body.usage.input_tokens or 0,
            completion_tokens = body.usage and body.usage.output_tokens or 0,
            total_tokens = (body.usage and body.usage.input_tokens or 0) + (body.usage and body.usage.output_tokens or 0)
        }
    }

    return serialize_json(openai_response)
end

-- 提取 token 用量
function anthropic.extract_tokens(response_str)
    local ok, body = pcall(json_decode, response_str)
    if not ok or not body or not body.usage then
        return 0, 0
    end
    return body.usage.input_tokens or 0, body.usage.output_tokens or 0
end

-------------------------------------------------------------------------------
-- 内部辅助函数
-------------------------------------------------------------------------------

-- 简单 JSON 序列化（Lua table -> JSON string）
function serialize_json(t)
    if type(t) == "string" then
        return '"' .. t:gsub('"', '\\"'):gsub('\n', '\\n'):gsub('\r', '\\r') .. '"'
    elseif type(t) == "number" then
        return tostring(t)
    elseif type(t) == "boolean" then
        return t and "true" or "false"
    elseif type(t) == "table" then
        -- 判断是数组还是对象
        local is_array = true
        for k, _ in pairs(t) do
            if type(k) ~= "number" then
                is_array = false
                break
            end
        end

        if is_array then
            local parts = {}
            for _, v in ipairs(t) do
                table.insert(parts, serialize_json(v))
            end
            return "[" .. table.concat(parts, ",") .. "]"
        else
            local parts = {}
            for k, v in pairs(t) do
                table.insert(parts, '"' .. k .. '":' .. serialize_json(v))
            end
            return "{" .. table.concat(parts, ",") .. "}"
        end
    else
        return "null"
    end
end

return anthropic
