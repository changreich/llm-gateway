-- LLM Gateway Router 2 (端口 9089)
--
-- 特性：
--   - URL 精确匹配包含 "code" 字样
--   - 通过 code:select 获取当前配置序号
--   - 支持多 opt 组合 (01+02)
--   - 重建请求体，覆盖原请求参数
--   - 无负载均衡、无 fallback

handler = {}

-- 从 config2.lua 加载默认配置
local config_file = loadfile("lua/config2.lua")
local default_config = config_file and config_file() or {}

-- 加载 SDK 模块
local sdk = dofile(script_dir .. "/sdk/init.lua")

-- 默认配置
local default_code = default_config.code or {}
local default_opt = default_config.opt or {}

-------------------------------------------------------------------------------
-- 初始化：将 config2.lua 配置写入 Redis
-------------------------------------------------------------------------------

local function init_config_to_redis()
    local ok, initialized = pcall(redis_get, "code:initialized")
    if ok and initialized == "1" then
        return
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

    -- 设置默认选中
    if default_config.selected then
        pcall(redis_set, "code:select", default_config.selected)
    end

    -- 标记已初始化
    pcall(redis_set, "code:initialized", "1")
end

-------------------------------------------------------------------------------
-- 工具函数
-------------------------------------------------------------------------------

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
local function rebuild_request_body(original_body, model, opt_config)
    -- 解析原请求体
    local ok, orig = pcall(json_decode, original_body)
    if not ok or type(orig) ~= "table" then
        return nil
    end

    -- 创建新请求体
    local new_body = {}

    -- 保留 messages
    if orig.messages then
        new_body.messages = orig.messages
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

    -- 返回 JSON
    return json_encode(new_body)
end

-- 简单的 JSON 编码 (只支持基本类型)
function json_encode(t)
    if type(t) == "table" then
        local is_array = #t > 0
        local parts = {}

        if is_array then
            for i, v in ipairs(t) do
                table.insert(parts, json_encode(v))
            end
            return "[" .. table.concat(parts, ",") .. "]"
        else
            for k, v in pairs(t) do
                table.insert(parts, '"' .. k .. '":' .. json_encode(v))
            end
            return "{" .. table.concat(parts, ",") .. "}"
        end
    elseif type(t) == "string" then
        -- 简单转义
        local escaped = t:gsub('\\', '\\\\'):gsub('"', '\\"'):gsub('\n', '\\n'):gsub('\r', '\\r'):gsub('\t', '\\t')
        return '"' .. escaped .. '"'
    elseif type(t) == "number" then
        return tostring(t)
    elseif type(t) == "boolean" then
        return t and "true" or "false"
    else
        return "null"
    end
end

-------------------------------------------------------------------------------
-- 主请求处理
-------------------------------------------------------------------------------

function handler.on_request(method, path, headers, body)
    -- 初始化配置到 Redis
    init_config_to_redis()

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

    -- 重建请求体
    local new_body = rebuild_request_body(body, code_cfg.model, opt_config)
    if not new_body then
        return {
            action = "reject",
            status = 400,
            body = '{"error":"failed to parse request body"}'
        }
    end

    -- 获取 SDK 端点
    local provider_sdk = sdk.load(code_cfg.provider)
    local endpoint = "/v1/chat/completions"
    if provider_sdk and provider_sdk.get_endpoint then
        endpoint = provider_sdk.get_endpoint(provider_cfg.baseurl)
    end

    -- 重写路径
    local rewrite_path = endpoint

    -- 统计：调用次数
    local count_key = "code:" .. selected .. ":calls"
    pcall(redis_incr, count_key)

    -- 返回代理决策
    return {
        action = "proxy",
        upstream = code_cfg.provider,
        addr = provider_cfg.baseurl,
        tls = string.sub(provider_cfg.baseurl, 1, 5) == "https",
        sni = "",
        api_key = provider_cfg.apikey,
        model = code_cfg.model,
        rewrite_path = rewrite_path,
        new_request_body = new_body
    }
end

function handler.on_response(upstream, status, body)
    -- 响应统计 (可选)
end

function handler.on_error(upstream, err)
    -- 错误处理
end
