-- LLM Gateway SDK 模块加载器
--
-- 统一格式：OpenAI Chat Completions API 格式
-- SDK 职责：请求/响应转换

local sdk = {}

-- 已注册的 SDK 模块
local registered_sdks = {}

-- 加载 SDK 模块
-- script_dir 由 Rust 设置为脚本目录
function sdk.load(provider_name)
    -- 已缓存
    if registered_sdks[provider_name] then
        return registered_sdks[provider_name]
    end

    -- 尝试加载模块文件
    local module_file = script_dir .. "/sdk/" .. provider_name .. ".lua"
    local ok, module = pcall(dofile, module_file)
    if ok and module then
        registered_sdks[provider_name] = module
        return module
    end

    -- 默认使用 OpenAI 兼容格式
    local default_file = script_dir .. "/sdk/openai.lua"
    ok, module = pcall(dofile, default_file)
    if ok and module then
        registered_sdks[provider_name] = module
        return module
    end

    -- 如果都失败，返回 nil
    return nil
end

-- 转换请求：统一格式 -> 提供商格式
function sdk.transform_request(provider_name, body_str, model, config)
    local s = sdk.load(provider_name)
    if s and s.transform_request then
        return s.transform_request(body_str, model, config)
    end
    return body_str
end

-- 转换响应：提供商格式 -> 统一格式
function sdk.transform_response(provider_name, response_str)
    local s = sdk.load(provider_name)
    if s and s.transform_response then
        return s.transform_response(response_str)
    end
    return response_str
end

-- 获取端点路径
function sdk.get_endpoint(provider_name)
    local s = sdk.load(provider_name)
    if s and s.get_endpoint then
        return s.get_endpoint()
    end
    return "/v1/chat/completions"
end

-- 获取额外请求头
function sdk.get_extra_headers(provider_name, api_key)
    local s = sdk.load(provider_name)
    if s and s.get_extra_headers then
        return s.get_extra_headers(api_key)
    end
    return {}
end

-- 判断是否需要流式响应处理
function sdk.is_streaming(provider_name)
    local s = sdk.load(provider_name)
    if s and s.is_streaming then
        return s.is_streaming()
    end
    return false
end

return sdk
