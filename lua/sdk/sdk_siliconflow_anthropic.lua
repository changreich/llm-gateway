-- SiliconFlow Anthropic 兼容 SDK
--
-- 适用于：SiliconFlow 平台的 Anthropic 兼容端点
-- baseurl: https://api.siliconflow.cn/anthropic/v1
-- 端点: /v1/messages (改写后的路径)
--
-- 请求格式：OpenAI Chat Completions messages 格式
-- 响应格式：Anthropic Messages API 格式
--
-- 参考 curl 示例：
--   curl --url https://api.siliconflow.cn/v1/messages
--   -H "Authorization: Bearer YOUR_API_KEY"
--   -d '{"model": "Pro/zai-org/GLM-4.7", "messages": [...]}'

local silianthropic = {}

-- 检测 baseurl 是否匹配此 SDK
function silianthropic.match(baseurl)
    if not baseurl then
        return false
    end
    local lower = string.lower(baseurl)
    return string.find(lower, "siliconflow") and string.find(lower, "/anthropic/v1")
end

-- 获取端点路径
-- 关键：baseurl 是 https://api.siliconflow.cn/anthropic/v1
-- 但实际请求路径应该是 /v1/messages (不是 /anthropic/v1/v1/messages)
function silianthropic.get_endpoint(baseurl)
    -- 无论 baseurl 是什么，固定返回 /v1/messages
    return "/v1/messages"
end

-- 重写 baseurl
-- 将 https://api.siliconflow.cn/anthropic/v1 改写为 https://api.siliconflow.cn
function silianthropic.rewrite_baseurl(baseurl)
    if not baseurl then
        return "https://api.siliconflow.cn"
    end
    -- 去掉 /anthropic/v1 路径部分
    local rewritten = baseurl:gsub("/anthropic/v1$", "")
    rewritten = rewritten:gsub("/anthropic/v1/", "/")
    return rewritten
end

-- 额外请求头
function silianthropic.get_extra_headers(api_key)
    return {
        ["Authorization"] = "Bearer " .. api_key,
        ["Content-Type"] = "application/json"
    }
end

-- 转换请求
-- 输入：OpenAI messages 格式 JSON
-- 输出：原样传递（已经是目标格式）
--
-- 关键：请求体格式是 OpenAI messages 格式
-- {
--   "model": "Pro/zai-org/GLM-4.7",
--   "messages": [...]
-- }
--
-- 但需要确保字段正确传递给 SiliconFlow
function silianthropic.transform_request(body_str, model, config)
    if not body_str or body_str == "" then
        return '{"model":"' .. model .. '","messages":[]}'
    end

    -- 解析请求体
    local ok, body = pcall(json_decode, body_str)
    if not ok or not body then
        -- 如果解析失败，尝试直接替换 model
        return body_str:gsub('"model"%s*:%s*"[^"]*"', '"model": "' .. model .. '"')
    end

    -- 确保 model 字段正确
    body.model = model

    -- 如果有 stream 字段，SiliconFlow Anthropic 端点支持
    -- 不需要移除，保留原样

    -- 序列化回 JSON
    return json_encode(body)
end

-- 转换响应
-- SiliconFlow Anthropic 端点返回 Anthropic 格式的响应
-- 需要转换为 OpenAI 格式以保持统一
function silianthropic.transform_response(response_str)
    local ok, body = pcall(json_decode, response_str)
    if not ok or not body then
        return response_str
    end

    -- 检查是否是 Anthropic 格式响应
    -- Anthropic 格式: {id, type: "message", content: [...], usage: {input_tokens, output_tokens}}
    if body.type ~= "message" then
        -- 不是 Anthropic 格式，原样返回
        return response_str
    end

    -- 提取 content
    local content = ""
    if body.content and type(body.content) == "table" then
        for _, item in ipairs(body.content) do
            if item.type == "text" and item.text then
                content = content .. item.text
            end
        end
    end

    -- 转换为 OpenAI 格式
    local openai_response = {
        id = body.id or "chatcmpl-silianthropic",
        object = "chat.completion",
        created = os.time(),
        model = body.model or "unknown",
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
            prompt_tokens = body.usage and (body.usage.input_tokens or body.usage.prompt_tokens) or 0,
            completion_tokens = body.usage and (body.usage.output_tokens or body.usage.completion_tokens) or 0,
            total_tokens = 0
        }
    }

    -- 计算 total_tokens
    if body.usage then
        local input_t = body.usage.input_tokens or body.usage.prompt_tokens or 0
        local output_t = body.usage.output_tokens or body.usage.completion_tokens or 0
        openai_response.usage.total_tokens = input_t + output_t
    end

    return json_encode(openai_response)
end

-- 提取 token 用量
function silianthropic.extract_tokens(response_str)
    local ok, body = pcall(json_decode, response_str)
    if not ok or not body or not body.usage then
        return 0, 0
    end

    local input_tokens = body.usage.input_tokens or body.usage.prompt_tokens or 0
    local output_tokens = body.usage.output_tokens or body.usage.completion_tokens or 0

    return input_tokens, output_tokens
end

-- 是否支持流式响应
-- SiliconFlow Anthropic 端点支持 SSE 流式响应
function silianthropic.is_streaming()
    return true
end

-- 是否需要 Rust 层 SSE 流转换
-- 返回 true 表示响应是 OpenAI SSE 格式，需要转换为 Anthropic SSE
-- SiliconFlow Anthropic 端点返回的是 Anthropic SSE 格式
-- 所以不需要 Rust 转换（客户端期望的就是 Anthropic 格式）
function silianthropic.need_sse_transform()
    return false
end

return silianthropic
