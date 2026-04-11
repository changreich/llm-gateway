-- LLM Gateway Router 2 (端口 9089)
--
-- 特性：
--   - URL 精确匹配包含 "code" 字样
--   - 通过 code:select 获取当前配置序号
--   - 支持多 opt 组合 (01+02)
--   - 重建请求体，覆盖原请求参数
--   - 无负载均衡、无 fallback

handler = {}

-- 全局状态（用于 on_response 回调中保存请求数据）
request_id = 0
current_request = nil

-- 脚本初始化完成标记 (v6)
pcall(redis_set, "ROUTER2_V6_LOADED", "YES_" .. os.date("%H:%M:%S"))

-- 从 config2.lua 加载默认配置
local config_path = script_dir .. "/config2.lua"
local config_file = loadfile(config_path)
local default_config = config_file and config_file() or {}

-- 加载 SDK 模块
local sdk = dofile(script_dir .. "/sdk/init.lua")

-- 默认配置
local default_code = default_config.code or {}
local default_opt = default_config.opt or {}

-- 标记脚本已加载 (v4)
pcall(redis_set, "router2_init_check", os.date("%H:%M:%S") .. "_test")
pcall(redis_set, "router2_init_v4", os.date("%Y-%m-%d %H:%M:%S"))

-------------------------------------------------------------------------------
-- 初始化：将 config2.lua 配置写入 Redis
-------------------------------------------------------------------------------

local function init_config_to_redis()
    local ok, initialized = pcall(redis_get, "code:initialized")
    
    -- 如果 initialized 存在且为 "1"，检查 code:select 是否存在
    if ok and initialized == "1" then
        local select_exists = pcall(redis_get, "code:select")
        if not select_exists or select_exists == "" then
            -- code:select 不存在，强制重新初始化
            pcall(redis_set, "code:initialized", "")
        else
            return
        end
    end

    -- 先设置默认选中，确保 code:select 最早被创建
    if default_config.selected then
        pcall(redis_set, "code:select", default_config.selected)
    end

    -- 写入 code 配置
    for num, cfg in pairs(default_code) do
        local key = "code:" .. num
        local value = (cfg.provider or "") .. "|" .. (cfg.model or "") .. "|" .. (cfg.opt or "")
        pcall(redis_set, key, value)
    end

    -- 写入 opt 配置
    for opt_id, fields in pairs(default_opt) do
        for field, value in pairs(fields) do
            local key = "opt:" .. opt_id .. ":" .. field
            pcall(redis_set, key, value)
        end
    end

    -- 加载 config.lua 中的 providers 并写入 Redis
    local config_lua_path = script_dir .. "/config.lua"
    local config_lua_file = loadfile(config_lua_path)
    if config_lua_file then
        local config_lua = config_lua_file()
        if config_lua and config_lua.providers then
            for name, cfg in pairs(config_lua.providers) do
                local key = "provider:" .. name
                local value = (cfg.baseurl or "") .. "|" .. (cfg.apikey or "")
                pcall(redis_set, key, value)
            end
        end
    end

    -- 标记已初始化
    pcall(redis_set, "code:initialized", "1")
end

-------------------------------------------------------------------------------
-- 工具函数
-------------------------------------------------------------------------------

-- 保存原始请求/响应到 Redis (保留最近5个)
local function save_raw_request(request, status, response)
    pcall(redis_set, "code:debug_save_raw", "start")
    
    local timestamp = os.date("%Y-%m-%d %H:%M:%S")
    local entry = '{"time":"' .. timestamp .. '","status":' .. status ..
                  ',"request":' .. request .. ',"response":' .. response .. '}'
    
    pcall(redis_set, "code:debug_save_raw", "entry_len:" .. tostring(#entry))
    
    local lpush_result = pcall(redis_lpush, "code:raw", entry)
    pcall(redis_set, "code:debug_save_raw", "lpush_result:" .. tostring(lpush_result))
    
    pcall(redis_ltrim, "code:raw", 0, 4)  -- 只保留最近5个
    
    pcall(redis_set, "code:debug_save_raw", "done")
end

-- 保存转换后的请求到 Redis (保留最近5个)
local function save_translated_request(request, selected, provider, model)
    pcall(redis_set, "code:debug_save2", "start:" .. tostring(#request))
    
    local timestamp = os.date("%Y-%m-%d %H:%M:%S")
    -- 记录请求长度和前100个字符用于调试
    local debug_info = "len:" .. tostring(#request) .. " preview:" .. string.sub(request, 1, 100)
    pcall(redis_set, "code:debug_save2_detail", debug_info)
    
    local entry = '{"time":"' .. timestamp .. '","select":"' .. selected ..
                  '","provider":"' .. provider .. '","model":"' .. model ..
                  '","request":' .. request .. '}'
    pcall(redis_lpush, "code:raw2", entry)
    pcall(redis_ltrim, "code:raw2", 0, 4)  -- 只保留最近5个
    
    pcall(redis_set, "code:debug_save2", "done")
end

-- 分割字符串
local function split(str, sep)
    local result = {}
    for part in string.gmatch(str, "[^" .. sep .. "]+") do
        table.insert(result, part)
    end
    return result
end

-- 安全获取 Redis 值
local function safe_redis_get(key)
    local ok, val = pcall(redis_get, key)
    if ok and val and val ~= "" then
        return val
    end
    return nil
end

-- 获取 code 配置
local function get_code_config(num)
    local config = safe_redis_get("code:" .. num)
    if not config then
        return nil
    end

    local parts = split(config, "|")
    if #parts < 2 then
        return nil
    end

    return {
        provider = parts[1],
        model = parts[2],
        opt = parts[3] or ""
    }
end

-- 获取 provider 配置
local function get_provider_config(name)
    local config = safe_redis_get("provider:" .. name)
    if not config then
        return nil
    end

    local parts = split(config, "|")
    if #parts < 2 then
        return nil
    end

    return {
        baseurl = parts[1],
        apikey = parts[2]
    }
end

-- 获取 opt 配置项
local function get_opt_config(opt_str)
    -- opt_str: "01+02" 或 "01"
    if not opt_str or opt_str == "" then
        return {}
    end

    local opt_ids = split(opt_str, "+")
    local result = {}

    for _, opt_id in ipairs(opt_ids) do
        -- 查询 opt:{id}:* 的所有配置
        local ok, keys = pcall(redis_keys, "opt:" .. opt_id .. ":*")
        if ok and keys then
            for _, key in ipairs(keys) do
                -- 提取字段名: opt:01:max_tokens -> max_tokens
                local field = string.match(key, "opt:%d+:(.+)")
                if field then
                    local value = safe_redis_get(key)
                    if value then
                        result[field] = value
                    end
                end
            end
        end
    end

    return result
end

-- 重建请求体
local function rebuild_request_body(original_body, model, opt_config, provider_sdk, provider_cfg)
    -- 解析原请求体
    local ok, orig = pcall(json_decode, original_body)
    if not ok or type(orig) ~= "table" then
        return nil
    end

    -- 创建新请求体
    local new_body = {}

    -- 兼容 Anthropic 格式: messages 或 prompt
    if orig.messages then
        new_body.messages = orig.messages
    elseif orig.prompt then
        -- Anthropic 旧格式: prompt 是字符串或数组，转为 messages
        local prompt = orig.prompt
        if type(prompt) == "string" then
            new_body.messages = {{role = "user", content = prompt}}
        elseif type(prompt) == "table" then
            local msgs = {}
            for i, p in ipairs(prompt) do
                table.insert(msgs, {role = "user", content = p})
            end
            new_body.messages = msgs
        end
    end

    -- 如果没有 messages，尝试从其他字段构建
    if not new_body.messages and orig.content then
        new_body.messages = {{role = "user", content = orig.content}}
    end

    -- 设置模型
    new_body.model = model

    -- 应用 opt 配置
    for field, value in pairs(opt_config) do
        -- 尝试转换数值
        local num = tonumber(value)
        if num then
            new_body[field] = num
        elseif value == "true" then
            new_body[field] = true
        elseif value == "false" then
            new_body[field] = false
        else
            new_body[field] = value
        end
    end

    -- 转换为 JSON
    local new_body_str = json_encode(new_body)

    -- 调用 SDK 的 transform_request 进行额外转换
    if provider_sdk and provider_sdk.transform_request then
        new_body_str = provider_sdk.transform_request(new_body_str, model, provider_cfg)
    end

    return new_body_str
end

-- json_encode 由 Rust 注册，不需要 Lua 端定义

-------------------------------------------------------------------------------
-- 主请求处理
-------------------------------------------------------------------------------

function handler.on_request(method, path, headers, body)
    -- 初始化配置到 Redis
    init_config_to_redis()

    -- /raw 端点：查看最近5个请求的原始数据
    if path == "/raw" then
        local ok, items = pcall(redis_lrange, "code:raw", 0, 4)
        if ok and items then
            local result = "["
            for i, item in ipairs(items) do
                if i > 1 then result = result .. "," end
                result = result .. item
            end
            result = result .. "]"
            return { action = "reject", status = 200, body = result }
        end
        return { action = "reject", status = 200, body = "[]" }
    end

    -- URL 匹配：路径中包含 "code" 字样
    if not string.find(path, "code") then
        return {
            action = "reject",
            status = 404,
            body = '{"error":"not found - use /xxx/code/xxx pattern"}'
        }
    end

    -- 获取当前选中的配置序号
    local selected = safe_redis_get("code:select") or "01"

    -- 获取 code 配置
    local code_cfg = get_code_config(selected)
    if not code_cfg then
        return {
            action = "reject",
            status = 503,
            body = '{"error":"code config not found: ' .. selected .. '"}'
        }
    end

    -- 获取 provider 配置
    local provider_cfg = get_provider_config(code_cfg.provider)
    if not provider_cfg then
        return {
            action = "reject",
            status = 503,
            body = '{"error":"provider not found: ' .. code_cfg.provider .. '"}'
        }
    end

    -- 获取 opt 配置
    local opt_config = get_opt_config(code_cfg.opt)

    -- 加载 SDK
    local provider_sdk = sdk.load(code_cfg.provider)

    pcall(redis_set, "code:debug", "loaded sdk:" .. tostring(provider_sdk))

    -- 重建请求体
    local new_body = rebuild_request_body(body, code_cfg.model, opt_config, provider_sdk, provider_cfg)
    if not new_body then
        return {
            action = "reject",
            status = 400,
            body = '{"error":"failed to parse request body"}'
        }
    end

    -- 获取 SDK 端点 (只是路径前缀，如 /zen/go)
    local endpoint = "/v1/chat/completions"
    if provider_sdk and provider_sdk.get_endpoint then
        endpoint = provider_sdk.get_endpoint(provider_cfg.baseurl)
    end

    -- 提取 host (移除协议，保留路径前缀)
    -- baseurl = https://opencode.ai/zen/go/v1/chat/completions
    -- host = opencode.ai/zen/go
    local host = provider_cfg.baseurl:gsub("^https?://", "")
    host = host:gsub("/v1/chat/completions.*", "")
    if host == "" then host = provider_cfg.baseurl:gsub("^https?://", "") end
    local use_tls = string.sub(provider_cfg.baseurl, 1, 5) == "https"

    -- 完整 URL = host + /v1/chat/completions
    local rewrite_path = "/v1/chat/completions"

    -- 统计：调用次数
    local count_key = "code:" .. selected .. ":calls"
    pcall(redis_incr, count_key)

    -- 保存请求信息 (用于 on_response 中保存 raw)
    request_id = request_id + 1
    current_request = {
        id = request_id,
        body = new_body,
        selected = selected,
        provider = code_cfg.provider,
        model = code_cfg.model
    }
    pcall(redis_set, "code:debug_id", "saved_id:" .. tostring(request_id))

    -- 保存转换后的请求到 Redis (code:raw2)
    save_translated_request(new_body, selected, code_cfg.provider, code_cfg.model)

    -- 返回代理决策
    return {
        action = "proxy",
        upstream = code_cfg.provider,
        addr = host,
        tls = use_tls,
        sni = "",
        api_key = provider_cfg.apikey,
        model = code_cfg.model,
        rewrite_path = rewrite_path,
        new_request_body = new_body
    }
end

function handler.on_response(upstream, status, body)
    -- 强制写入 Redis 以确认回调被调用
    local ok = pcall(redis_set, "ON_RESPONSE_CALLED_V5", "YES_at_" .. os.date("%H%M%S"))
    
    local ts = os.date("%H%M%S")
    pcall(redis_set, "RESPONSE_" .. ts, "called")
    
    if current_request and current_request.body then
        pcall(redis_set, "RESPONSE_" .. ts, "has_body")
        
        local response_json = body
        if string.sub(body, 1, 1) == "{" or string.sub(body, 1, 1) == "[" then
            response_json = body
        else
            response_json = '"' .. body:gsub('"', '\\"'):gsub('\n', '\\n') .. '"'
        end
        
        pcall(redis_set, "RESPONSE_" .. ts, "calling_save")
        save_raw_request(current_request.body, status, response_json)
        pcall(redis_set, "RESPONSE_" .. ts .. "_done", "yes")
    else
        pcall(redis_set, "RESPONSE_" .. ts, "no_body")
    end
end

function handler.on_error(upstream, err)
    -- 错误处理
end
