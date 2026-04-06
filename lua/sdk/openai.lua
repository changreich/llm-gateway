-- OpenAI 兼容格式 SDK
--
-- 适用于：OpenAI, SiliconFlow, DeepSeek, Moonshot, 百川 等
-- 请求格式：标准 OpenAI Chat Completions API
-- 响应格式：标准 OpenAI Chat Completions API
--
-- 参考文档：https://platform.openai.com/docs/api-reference/chat

local openai = {}

-- 端点路径 (相对于 baseurl)
function openai.get_endpoint(baseurl)
    -- 检测 baseurl 是否已包含 /v1
    if baseurl and (baseurl:match("/v1$") or baseurl:match("/v1/")) then
        return "/chat/completions"
    end
    -- 标准 OpenAI 格式
    return "/v1/chat/completions"
end

-- 额外请求头
function openai.get_extra_headers(api_key)
    return {
        ["Authorization"] = "Bearer " .. api_key,
        ["Content-Type"] = "application/json"
    }
end

-- 转换请求
-- body_str: JSON 字符串 (OpenAI 格式)
-- model: 目标模型名称
-- config: 提供商配置
function openai.transform_request(body_str, model, config)
    if not body_str or body_str == "" then
        return '{"model":"' .. model .. '","messages":[]}'
    end

    -- 替换 model 字段
    local result = body_str:gsub('"model"%s*:%s*"[^"]*"', '"model": "' .. model .. '"')
    return result
end

-- 转换响应
-- response_str: JSON 字符串 (OpenAI 格式)
function openai.transform_response(response_str)
    -- OpenAI 格式无需转换
    return response_str
end

-- 提取 token 用量
function openai.extract_tokens(response_str)
    local prompt = response_str:match('"prompt_tokens"%s*:%s*(%d+)')
    local completion = response_str:match('"completion_tokens"%s*:%s*(%d+)')

    if prompt then
        return tonumber(prompt) or 0, tonumber(completion) or 0
    end

    -- 某些提供商使用 input/output
    local input = response_str:match('"input_tokens"%s*:%s*(%d+)')
    local output = response_str:match('"output_tokens"%s*:%s*(%d+)')

    if input then
        return tonumber(input) or 0, tonumber(output) or 0
    end

    return 0, 0
end

return openai
