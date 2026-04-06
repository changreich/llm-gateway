-- SiliconFlow SDK
--
-- 适用于：SiliconFlow 平台
-- 请求格式：OpenAI 兼容格式
-- 响应格式：OpenAI 兼容格式
--
-- 参考文档：https://docs.siliconflow.cn/
-- 参考示例：llm-gateway/silicon.md
--
-- 特点：
-- 1. 完全兼容 OpenAI API 格式
-- 2. 支持多种开源模型：Qwen, DeepSeek, GLM 等
-- 3. 模型名格式：owner/model (如 Qwen/Qwen3.5-4B)

local siliconflow = {}

-- 端点路径 (相对于 baseurl)
-- SiliconFlow baseurl 通常是 https://api.siliconflow.cn/v1
-- 所以 endpoint 只需要 /chat/completions
function siliconflow.get_endpoint(baseurl)
    if baseurl and (baseurl:match("/v1$") or baseurl:match("/v1/")) then
        return "/chat/completions"
    end
    return "/v1/chat/completions"
end

-- 额外请求头
function siliconflow.get_extra_headers(api_key)
    return {
        ["Authorization"] = "Bearer " .. api_key,
        ["Content-Type"] = "application/json"
    }
end

-- 转换请求
-- body_str: OpenAI 格式的 JSON 字符串
-- model: 目标模型名称 (如 Qwen/Qwen3.5-4B)
-- config: 提供商配置
function siliconflow.transform_request(body_str, model, config)
    if not body_str or body_str == "" then
        return '{"model":"' .. model .. '","messages":[]}'
    end

    -- 替换 model 字段
    local result = body_str:gsub('"model"%s*:%s*"[^"]*"', '"model": "' .. model .. '"')
    return result
end

-- 转换响应
function siliconflow.transform_response(response_str)
    -- 完全兼容 OpenAI，无需转换
    return response_str
end

-- 提取 token 用量
function siliconflow.extract_tokens(response_str)
    local prompt = response_str:match('"prompt_tokens"%s*:%s*(%d+)')
    local completion = response_str:match('"completion_tokens"%s*:%s*(%d+)')

    if prompt then
        return tonumber(prompt) or 0, tonumber(completion) or 0
    end

    -- SiliconFlow 可能使用 input/output
    local input = response_str:match('"input_tokens"%s*:%s*(%d+)')
    local output = response_str:match('"output_tokens"%s*:%s*(%d+)')

    if input then
        return tonumber(input) or 0, tonumber(output) or 0
    end

    return 0, 0
end

return siliconflow
