-- OpenGo SDK (opencode.ai/zen/go 端点)
--
-- 模型 ID: 支持两种格式
--   1. "opencode-go/<model-id>" — 直接使用（如 opencode-go/kimi-k2.5）
--   2. "<model-id>" — 保持原样（如 glm-5.1），opengo 端点自动映射

local opengo = {}

-- 规范化 model ID
-- 如果包含 "/" 视为已含 provider 前缀，直接使用
-- 否则保持原样
local function normalize_model(model)
    if not model or model == "" then
        return "glm-5.1"
    end
    if model:find("/", 1, true) then
        return model
    end
    return model
end

-- 端点路径 (baseurl 已包含完整路径)
function opengo.get_endpoint(baseurl)
    if baseurl and (baseurl:match("/v1$") or baseurl:match("/v1/")) then
        return "/chat/completions"
    end
    return "/v1/chat/completions"
end

-- 额外请求头
function opengo.get_extra_headers(api_key)
    return {
        ["Authorization"] = "Bearer " .. api_key,
        ["Content-Type"] = "application/json"
    }
end

-- 转换请求
-- 1. 规范化 model ID（已有前缀则保留，否则保持原样）
-- 2. 将 Anthropic 格式的 content 数组转为 OpenAI 字符串
function opengo.transform_request(body_str, model, config)
    if not body_str or body_str == "" then
        return '{"model":"' .. normalize_model(model) .. '","messages":[]}'
    end

    local ok, body = pcall(json_decode, body_str)
    if not ok or type(body) ~= "table" then
        return body_str:gsub('"model"%s*:%s*"[^"]*"', '"model": "' .. normalize_model(model) .. '"')
    end

    -- 规范化 model
    body.model = normalize_model(model)

    -- 将 Anthropic 格式 content 数组转为字符串
    if body.messages then
        for i, msg in ipairs(body.messages) do
            if type(msg.content) == "table" then
                local parts = {}
                for j, item in ipairs(msg.content) do
                    if type(item) == "string" then
                        table.insert(parts, item)
                    elseif type(item) == "table" and item.text then
                        table.insert(parts, item.text)
                    end
                end
                if #parts > 0 then
                    msg.content = table.concat(parts, "\n")
                end
            end
        end
    end

    return json_encode(body)
end

-- 转换响应
function opengo.transform_response(response_str)
    return response_str
end

-- 提取 token 用量
function opengo.extract_tokens(response_str)
    local prompt = response_str:match('"prompt_tokens"%s*:%s*(%d+)')
    local completion = response_str:match('"completion_tokens"%s*:%s*(%d+)')

    if prompt then
        return tonumber(prompt) or 0, tonumber(completion) or 0
    end

    local input = response_str:match('"input_tokens"%s*:%s*(%d+)')
    local output = response_str:match('"output_tokens"%s*:%s*(%d+)')

    if input then
        return tonumber(input) or 0, tonumber(output) or 0
    end

    return 0, 0
end

return opengo