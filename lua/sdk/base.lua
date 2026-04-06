-- SDK 基础类
--
-- 所有 SDK 模块继承此基础类
-- 默认实现 OpenAI 兼容格式

local base = {}

-- 端点路径
function base.get_endpoint()
    return "/v1/chat/completions"
end

-- 额外请求头
function base.get_extra_headers(api_key)
    return {
        ["Authorization"] = "Bearer " .. api_key
    }
end

-- 转换请求
-- body_str: JSON 字符串 (OpenAI 格式)
-- model: 目标模型名称
-- config: 提供商配置 {baseurl, apikey, ...}
function base.transform_request(body_str, model, config)
    -- 默认不转换，只替换 model
    if not body_str or body_str == "" then
        return body_str
    end

    -- 简单的 model 字段替换
    local result = body_str:gsub('"model"%s*:%s*"[^"]*"', '"model": "' .. model .. '"')
    return result
end

-- 转换响应
-- response_str: JSON 字符串
function base.transform_response(response_str)
    -- 默认不转换，直接返回
    return response_str
end

-- 是否流式响应
function base.is_streaming()
    return false
end

return base
