-- 智谱 AI SDK
--
-- 适用于：智谱 GLM 系列模型
-- 请求格式：OpenAI 兼容格式 (有部分差异)
-- 响应格式：OpenAI 兼容格式
--
-- 参考文档：https://open.bigmodel.cn/dev/api
--
-- 关键差异：
-- 1. 端点：/api/paas/v4/chat/completions (非标准 /v1/chat/completions)
-- 2. 模型名：GLM-4-Flash, GLM-4-Plus, GLM-4-0520 等
-- 3. 工具调用：tools 格式略有不同

local zhipu = {}

-- 端点路径 (相对于 baseurl)
-- 智谱 baseurl 通常是 https://open.bigmodel.cn/api/paas/v4
-- 所以 endpoint 只需要 /chat/completions
function zhipu.get_endpoint(baseurl)
    if baseurl and (baseurl:match("/v4$") or baseurl:match("/v4/")) then
        return "/chat/completions"
    end
    return "/api/paas/v4/chat/completions"
end

-- 额外请求头
function zhipu.get_extra_headers(api_key)
    return {
        ["Authorization"] = "Bearer " .. api_key,
        ["Content-Type"] = "application/json"
    }
end

-- 转换请求
-- body_str: OpenAI 格式的 JSON 字符串
-- model: 目标模型名称 (GLM-4-Flash, GLM-4-Plus 等)
-- config: 提供商配置
function zhipu.transform_request(body_str, model, config)
    if not body_str or body_str == "" then
        return '{"model":"' .. model .. '","messages":[]}'
    end

    -- 替换 model 字段
    local result = body_str:gsub('"model"%s*:%s*"[^"]*"', '"model": "' .. model .. '"')
    return result
end

-- 转换响应
-- response_str: JSON 字符串
function zhipu.transform_response(response_str)
    -- 智谱格式基本兼容 OpenAI，无需转换
    return response_str
end

-- 提取 token 用量
function zhipu.extract_tokens(response_str)
    local prompt = response_str:match('"prompt_tokens"%s*:%s*(%d+)')
    local completion = response_str:match('"completion_tokens"%s*:%s*(%d+)')

    if prompt then
        return tonumber(prompt) or 0, tonumber(completion) or 0
    end

    return 0, 0
end

return zhipu
