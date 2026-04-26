-- Alibaba Cloud (DashScope/Qwen) SDK
--
-- 适用于：阿里云百炼平台 Qwen 系列模型
-- 请求格式：OpenAI 兼容格式 (DashScope OpenAI-兼容模式)
-- 响应格式：OpenAI 兼容格式 (含扩展字段)
--
-- 参考文档：
--   国内: https://help.aliyun.com/zh/model-studio/developer-reference/use-qwen-by-calling-api
--   国际: https://www.alibabacloud.com/help/en/model-studio/qwen-api-via-openai-chat-completions
--
-- 模型列表：
--   商业版: qwen3-max, qwen-plus, qwen-flash, qwen-turbo
--   开源版: qwen3-235b-a22b, qwen3-32b, qwen3-14b
--   推理版: qwq-plus, qwq-32b, qwen3-next-80b-a3b-thinking
--   代码版: qwen-coder, qwen3-coder-plus, qwen3-coder-flash
--
-- 关键特性：
--   1. 端点：OpenAI 兼容，/compatible-mode/v1/chat/completions
--   2. 思考模式：请求可加 enable_thinking=true, reasoning_content 字段
--   3. 缓存 Token：响应中 prompt_tokens_details.cached_tokens
--   4. 推理 Token：响应中 completion_tokens_details.reasoning_tokens

local alibaba = {}

-- 端点路径 (相对于 baseurl)
-- baseurl 格式通常是 https://dashscope.aliyuncs.com/compatible-mode/v1
-- 或 https://dashscope-intl.aliyuncs.com/compatible-mode/v1
function alibaba.get_endpoint(baseurl)
    if baseurl then
        -- 如果 baseurl 已包含 /v1，使用相对路径
        if baseurl:match("/v1$") or baseurl:match("/v1/") then
            return "/chat/completions"
        end
        -- 如果包含 /compatible-mode，补充 /v1
        if baseurl:match("/compatible%-mode") then
            return "/chat/completions"
        end
    end
    return "/v1/chat/completions"
end

-- 额外请求头
function alibaba.get_extra_headers(api_key)
    return {
        ["Authorization"] = "Bearer " .. api_key,
        ["Content-Type"] = "application/json"
    }
end

-- 转换请求
-- body_str: OpenAI 格式的 JSON 字符串
-- model: 目标模型名称 (qwen3-max, qwen-plus 等)
-- config: 提供商配置
function alibaba.transform_request(body_str, model, config)
    if not body_str or body_str == "" then
        return '{"model":"' .. model .. '","messages":[]}'
    end

    -- 解析请求体
    local ok, body = pcall(json_decode, body_str)
    if not ok or type(body) ~= "table" then
        -- JSON 解析失败，尝试简单替换 model
        return body_str:gsub('"model"%s*:%s*"[^"]*"', '"model": "' .. model .. '"')
    end

    -- 替换 model 字段
    body.model = model

    -- Alibaba 扩展参数处理
    -- 如果 config 中有 alibaba 特定选项，注入到请求中
    if config then
        -- enable_thinking: 启用思考模式
        -- 从 config.opt 表读取
        if config.enable_thinking ~= nil then
            body.enable_thinking = config.enable_thinking
        end
        -- thinking_budget: 思考 Token 预算
        if config.thinking_budget then
            body.thinking_budget = config.thinking_budget
        end
    end

    return json_encode(body)
end

-- 转换响应
-- 阿里云返回 OpenAI 兼容格式，但可能包含扩展字段：
--   - reasoning_content: 思考内容
--   - prompt_tokens_details.cached_tokens: 缓存命中
--   - completion_tokens_details.reasoning_tokens: 推理 Token
-- 直接透传即可，不做转换
function alibaba.transform_response(response_str)
    return response_str
end

-- 提取 token 用量
-- 阿里云响应包含 OpenAI 标准字段 + 扩展字段：
-- {
--   "usage": {
--     "prompt_tokens": 100,
--     "completion_tokens": 50,
--     "total_tokens": 150,
--     "prompt_tokens_details": {
--       "cached_tokens": 80    -- 缓存命中的 token
--     },
--     "completion_tokens_details": {
--       "reasoning_tokens": 30  -- 推理用 token
--     }
--   }
-- }
function alibaba.extract_tokens(response_str)
    -- 优先使用 prompt_tokens / completion_tokens (OpenAI 标准格式)
    local prompt = response_str:match('"prompt_tokens"%s*:%s*(%d+)')
    local completion = response_str:match('"completion_tokens"%s*:%s*(%d+)')

    if prompt then
        return tonumber(prompt) or 0, tonumber(completion) or 0
    end

    -- 备选：某些版本使用 input_tokens / output_tokens
    local input = response_str:match('"input_tokens"%s*:%s*(%d+)')
    local output = response_str:match('"output_tokens"%s*:%s*(%d+)')

    if input then
        return tonumber(input) or 0, tonumber(output) or 0
    end

    return 0, 0
end

return alibaba