-- LLM Gateway Router 2 (端口 9089)
--
-- 特性：
--   - URL 精确匹配包含 "code" 字样
--   - 通过 code:select 获取当前配置序号
--   - 支持多 opt 组合 (01+02)
--   - 重建请求体，覆盖原请求参数
--   - 支持 Anthropic provider 直通（不转换格式）
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
    -- ============================================================
    -- 始终更新 provider 和 global proxy 配置 (不受 initialized 影响)
    -- ============================================================
    local config_lua_path = script_dir .. "/config.lua"
    local config_lua_file = loadfile(config_lua_path)
    if config_lua_file then
        local config_lua = config_lua_file()
        if config_lua and config_lua.providers then
            for name, cfg in pairs(config_lua.providers) do
                local key = "provider:" .. name
                local value = (cfg.baseurl or "") .. "|" .. (cfg.apikey or "")
                pcall(redis_set, key, value)
                -- 写入 provider 级代理配置 (始终更新)
                if cfg.proxy then
                    pcall(redis_set, "provider:" .. name .. ":proxy", cfg.proxy)
                else
                    -- 配置文件中无 proxy，删除 Redis 中的旧配置
                    pcall(redis_del, "provider:" .. name .. ":proxy")
                end
            end
        end
        -- 写入全局代理配置 (始终更新)
        if config_lua.proxy then
            pcall(redis_set, "global:proxy", config_lua.proxy)
        else
            pcall(redis_del, "global:proxy")
        end
    end

    -- ============================================================
    -- 以下配置只在首次初始化时写入
    -- ============================================================
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

    -- 写入全局代理配置 (来自 config2.lua)
    if default_config.proxy then
        pcall(redis_set, "global:proxy", default_config.proxy)
    end

    -- 写入 code 配置 (含 proxy)
    for num, cfg in pairs(default_code) do
        local key = "code:" .. num
        local value = (cfg.provider or "") .. "|" .. (cfg.model or "") .. "|" .. (cfg.opt or "")
        pcall(redis_set, key, value)
        -- 写入 code 级代理配置
        if cfg.proxy then
            pcall(redis_set, "code:" .. num .. ":proxy", cfg.proxy)
        end
    end

    -- 写入 opt 配置
    for opt_id, fields in pairs(default_opt) do
        for field, value in pairs(fields) do
            local key = "opt:" .. opt_id .. ":" .. field
            pcall(redis_set, key, value)
        end
    end

    -- 写入 modelmap 配置
    if default_config.modelmap then
        for model_name, num in pairs(default_config.modelmap) do
            local key = "modelmap:" .. model_name
            pcall(redis_set, key, num)
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

-- 根据 model 名称查询映射的配置编号
-- 返回: num | nil
local function get_modelmap_num(model_name)
    if not model_name or model_name == "" then
        return nil
    end

    -- 精确匹配
    local num = safe_redis_get("modelmap:" .. model_name)
    if num then
        return num
    end

    -- 前缀匹配 (model-name-xxx -> model-name)
    local parts = {}
    for part in string.gmatch(model_name, "[^-]+") do
        table.insert(parts, part)
    end

    if #parts > 1 then
        -- 尝试去掉最后一部分
        local prefix = table.concat(parts, "-", 1, #parts - 1)
        num = safe_redis_get("modelmap:" .. prefix)
        if num then
            return num
        end
    end

    return nil
end

-- 解析请求体中的 model 字段
local function extract_model_from_body(body)
    if not body or body == "" then
        return nil
    end

    local ok, parsed = pcall(json_decode, body)
    if not ok or type(parsed) ~= "table" then
        return nil
    end

    return parsed.model
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

--- 获取代理配置
-- 优先级: code:{num}:proxy > provider:{name}:proxy > global:proxy
-- @param code_num string 配置编号
-- @param provider_name string 提供商名称
-- @return string|nil 代理URL，nil表示直连
local function get_proxy_config(code_num, provider_name)
    -- 1. 模型级代理
    local code_proxy = safe_redis_get("code:" .. code_num .. ":proxy")
    if code_proxy then
        if code_proxy == "" then
            return nil  -- 空字符串强制直连
        end
        return code_proxy
    end

    -- 2. 提供商级代理
    local provider_proxy = safe_redis_get("provider:" .. provider_name .. ":proxy")
    if provider_proxy then
        if provider_proxy == "" then
            return nil
        end
        return provider_proxy
    end

    -- 3. 全局代理
    local global_proxy = safe_redis_get("global:proxy")
    if global_proxy and global_proxy ~= "" then
        return global_proxy
    end

    -- 4. 无代理，直连
    return nil
end

local function extract_tool_result_content(content)
    if type(content) == "string" then
        return content
    end
    if type(content) ~= "table" then
        return ""
    end

    local parts = {}
    for _, item in ipairs(content) do
        if type(item) == "string" then
            table.insert(parts, item)
        elseif type(item) == "table" and item.text then
            table.insert(parts, item.text)
        end
    end
    return table.concat(parts, "\n")
end

local function anthropic_content_to_openai(role, content, out_messages)
    if type(content) == "string" then
        return content, nil
    end
    if type(content) ~= "table" then
        return "", nil
    end

    local text_parts = {}
    local multipart = {}
    local tool_calls = {}
    local has_non_text_part = false

    for _, block in ipairs(content) do
        if type(block) == "string" then
            table.insert(text_parts, block)
            table.insert(multipart, { ["type"] = "text", text = block })
        elseif type(block) == "table" then
            local block_type = block.type
            if block_type == "text" then
                local t = block.text or ""
                table.insert(text_parts, t)
                table.insert(multipart, { ["type"] = "text", text = t })
            elseif block_type == "image" and role == "user" then
                has_non_text_part = true
                local source = block.source or {}
                local image_url = nil
                if source.type == "base64" and source.data then
                    local media_type = source.media_type or "image/jpeg"
                    image_url = "data:" .. media_type .. ";base64," .. source.data
                elseif source.type == "url" and source.url then
                    image_url = source.url
                end
                if image_url then
                    table.insert(multipart, {
                        ["type"] = "image_url",
                        image_url = { url = image_url }
                    })
                end
            elseif block_type == "tool_use" and role == "assistant" then
                local args = block.input or {}
                local args_json = json_encode(args)
                table.insert(tool_calls, {
                    id = block.id or "",
                    ["type"] = "function",
                    ["function"] = {
                        name = block.name or "",
                        arguments = args_json
                    }
                })
            elseif block_type == "tool_result" and role == "user" then
                table.insert(out_messages, {
                    role = "tool",
                    tool_call_id = block.tool_use_id or "",
                    content = extract_tool_result_content(block.content)
                })
            end
        end
    end

    if has_non_text_part then
        return multipart, tool_calls
    end
    return table.concat(text_parts, "\n"), tool_calls
end

--- 判断 provider 是否需要 Anthropic→OpenAI 格式转换
-- provider 名称或 baseurl 包含 "anthropic" → 不需要转换，直接透传
-- @param provider_key string provider 名称 (如 "opengo", "anthropic")
-- @param baseurl string provider 的 baseurl
-- @return boolean true=需要转换, false=不需要转换
local function needs_format_conversion(provider_key, baseurl)
    -- 检查 provider 名称
    if provider_key then
        local lower_key = string.lower(provider_key)
        if string.find(lower_key, "anthropic") then
            return false  -- Anthropic provider 不需要转换
        end
    end
    -- 检查 baseurl
    if baseurl then
        local lower_url = string.lower(baseurl)
        if string.find(lower_url, "anthropic") then
            return false  -- baseurl 含 anthropic，不需要转换
        end
    end
    return true  -- 其他 provider 需要转换为 OpenAI 格式
end

--- 检测请求体格式
-- @param body string 请求体 JSON 字符串
-- @return string "anthropic" | "openai" | "unknown"
local function detect_request_format(body)
    local ok, orig = pcall(json_decode, body)
    if not ok or type(orig) ~= "table" then
        return "unknown"
    end
    -- Anthropic 特征: prompt 字段 (且没有 messages)
    if orig.prompt and not orig.messages then
        return "anthropic"
    end
    -- Anthropic 特征: messages 中 content 是 table 数组 (含 type 字段)
    if orig.messages then
        for _, msg in ipairs(orig.messages) do
            if type(msg.content) == "table" then
                for _, block in ipairs(msg.content) do
                    if type(block) == "table" and block.type then
                        return "anthropic"
                    end
                end
            end
        end
        return "openai"  -- 有 messages 但无 type → OpenAI
    end
    return "unknown"
end

-- 重建请求体
local function rebuild_request_body(original_body, model, opt_config, provider_sdk, provider_cfg, provider_key)
    -- 解析原请求体
    local ok, orig = pcall(json_decode, original_body)
    if not ok or type(orig) ~= "table" then
        return nil
    end

    -- ★ Anthropic provider 直通：不做格式转换，只替换模型名和应用 opt
    if not needs_format_conversion(provider_key, provider_cfg.baseurl) then
        local request_format = detect_request_format(original_body)
        pcall(redis_set, "code:debug_format",
            string.format("skip_conversion:provider=%s req_format=%s", provider_key, request_format))
        local passthrough = {}
        passthrough.model = model
        for k, v in pairs(orig) do
            if k ~= "model" then
                passthrough[k] = v
            end
        end
        -- 应用 opt 配置
        for field, value in pairs(opt_config) do
            local num = tonumber(value)
            if field == "stream" then
                passthrough.stream = (value == "true")
            elseif num then
                passthrough[field] = num
            elseif value == "true" then
                passthrough[field] = true
            elseif value == "false" then
                passthrough[field] = false
            else
                passthrough[field] = value
            end
        end
        local passthrough_str = json_encode(passthrough)
        if provider_sdk and provider_sdk.transform_request then
            passthrough_str = provider_sdk.transform_request(passthrough_str, model, provider_cfg)
        end
        return passthrough_str
    end

    -- 需要转换：执行原有的 Anthropic → OpenAI 转换逻辑
    local request_format = detect_request_format(original_body)
    pcall(redis_set, "code:debug_format",
        string.format("convert:provider=%s req_format=%s", provider_key, request_format))

    -- 创建新请求体
    local new_body = {}
    local messages = {}

    -- 兼容 Anthropic 格式: messages 或 prompt
    if orig.messages then
        for _, msg in ipairs(orig.messages) do
            local role = msg.role or "user"
            local content = msg.content
            local openai_content, tool_calls = anthropic_content_to_openai(role, content, messages)
            local only_tool_result = false
            if role == "user" and type(content) == "table" then
                only_tool_result = true
                for _, block in ipairs(content) do
                    if type(block) ~= "table" or block.type ~= "tool_result" then
                        only_tool_result = false
                        break
                    end
                end
            end

            if role ~= "tool" and not only_tool_result then
                local new_msg = {
                    role = role,
                    content = openai_content
                }
                if role == "assistant" and tool_calls and #tool_calls > 0 then
                    new_msg.tool_calls = tool_calls
                    if type(openai_content) == "string" and openai_content == "" then
                        new_msg.content = nil
                    end
                end
                if msg.name then
                    new_msg.name = msg.name
                end
                table.insert(messages, new_msg)
            end
        end
    elseif orig.prompt then
        local prompt = orig.prompt
        if type(prompt) == "string" then
            table.insert(messages, { role = "user", content = prompt })
        elseif type(prompt) == "table" then
            for _, p in ipairs(prompt) do
                table.insert(messages, { role = "user", content = p })
            end
        end
    end

    if #messages == 0 and orig.content then
        table.insert(messages, { role = "user", content = orig.content })
    end

    new_body.messages = messages

    -- 设置模型
    new_body.model = model

    -- 透传通用参数
    for _, field in ipairs({"max_tokens", "temperature", "top_p", "presence_penalty", "frequency_penalty", "seed", "logprobs"}) do
        if orig[field] ~= nil then
            new_body[field] = orig[field]
        end
    end

    -- Anthropic stop_sequences -> OpenAI stop
    if orig.stop_sequences ~= nil then
        new_body.stop = orig.stop_sequences
    elseif orig.stop ~= nil then
        new_body.stop = orig.stop
    end

    -- 透传 system 消息 (Anthropic: system 字符串/数组 → OpenAI: system role message)
    if orig.system then
        if type(orig.system) == "string" then
            table.insert(new_body.messages, 1, {role = "system", content = orig.system})
        elseif type(orig.system) == "table" then
            local parts = {}
            for _, block in ipairs(orig.system) do
                if type(block) == "string" then
                    table.insert(parts, block)
                elseif type(block) == "table" and block.text then
                    table.insert(parts, block.text)
                end
            end
            if #parts > 0 then
                table.insert(new_body.messages, 1, {role = "system", content = table.concat(parts, "\n")})
            end
        end
    end

    -- 转换 Anthropic tools → OpenAI tools
    if orig.tools then
        local openai_tools = {}
        for _, tool in ipairs(orig.tools) do
            local openai_tool = {
                ["type"] = "function",
                ["function"] = {
                    name = tool.name,
                    description = tool.description or "",
                    parameters = tool.input_schema or {}
                }
            }
            table.insert(openai_tools, openai_tool)
        end
        new_body.tools = openai_tools
    end

    -- 透传 tool_choice
    if orig.tool_choice then
        if type(orig.tool_choice) == "string" then
            if orig.tool_choice == "auto" then
                new_body.tool_choice = "auto"
            elseif orig.tool_choice == "any" then
                new_body.tool_choice = "required"
            end
        elseif type(orig.tool_choice) == "table" then
            if orig.tool_choice.type == "auto" then
                new_body.tool_choice = "auto"
            elseif orig.tool_choice.type == "tool" and orig.tool_choice.name then
                new_body.tool_choice = {["type"] = "function", ["function"] = {name = orig.tool_choice.name}}
            elseif orig.tool_choice.name then
                new_body.tool_choice = {["type"] = "function", ["function"] = {name = orig.tool_choice.name}}
            end
        end
    end

    -- 注意: 不转发 stream 参数，让上游返回非流式响应
    -- Rust 端会检测原始请求的 stream 字段，包装为 SSE 返回

    -- 应用 opt 配置 (覆盖上面透传的参数)
    for field, value in pairs(opt_config) do
        local num = tonumber(value)
        if field == "stream" then
            -- stream 由 Rust 统一注入，不由 Lua 决定
        elseif num then
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

    -- 尝试从请求体提取 model 名称
    local request_model = extract_model_from_body(body)

    -- 根据 model 名称或 code:select 获取配置序号
    -- 优先级: modelmap:{model} > code:select
    local selected
    if request_model then
        selected = get_modelmap_num(request_model)
        if selected then
            pcall(redis_set, "code:debug_modelmap", string.format("model=%s -> num=%s", request_model, selected))
        end
    end

    -- 如果 modelmap 没有映射，fallback 到 code:select
    if not selected then
        selected = safe_redis_get("code:select") or "01"
    end

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
    local new_body = rebuild_request_body(body, code_cfg.model, opt_config, provider_sdk, provider_cfg, code_cfg.provider)
    if not new_body then
        return {
            action = "reject",
            status = 400,
            body = '{"error":"failed to parse request body"}'
        }
    end

    -- 获取端点路径
    -- Anthropic provider: /v1/messages
    -- OpenAI 兼容 provider: /v1/chat/completions (或 SDK 定义)
    local endpoint = "/v1/chat/completions"
    if string.find(string.lower(provider_cfg.baseurl), "anthropic") then
        endpoint = "/v1/messages"
    elseif provider_sdk and provider_sdk.get_endpoint then
        endpoint = provider_sdk.get_endpoint(provider_cfg.baseurl)
    end

    -- 提取 host 和 rewrite_path
    -- 统一逻辑：从 baseurl 分离 host 和 path，再拼接 endpoint
    local baseurl = provider_cfg.baseurl
    local use_tls = string.sub(baseurl, 1, 5) == "https"

    -- 去掉协议前缀
    local url_body = baseurl:gsub("^https?://", "")

    -- 分离 host 和 path
    -- url_body: "qianfan.baidubce.com/anthropic/coding" 或 "api.openai.com"
    local host, path_prefix = url_body:match("^([^/]+)(.*)")
    if not host then
        host = url_body  -- 无路径的情况
        path_prefix = ""
    end

    -- ★ 对于 Anthropic provider，endpoint 已经是完整路径
    -- rewrite_path = path_prefix + endpoint
    -- 例: /anthropic/coding + /v1/messages = /anthropic/coding/v1/messages
    local rewrite_path = path_prefix .. endpoint

    -- DEBUG: 保存路由信息
    pcall(redis_set, "code:debug_route", string.format(
        "host=%s|endpoint=%s|path_prefix=%s|rewrite_path=%s",
        host, endpoint, path_prefix, rewrite_path
    ))

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

    -- ★ 是否需要格式转换 (Anthropic provider 不需要)
    local need_transform = needs_format_conversion(code_cfg.provider, provider_cfg.baseurl)

    -- ★ 获取代理配置
    local proxy_url = get_proxy_config(selected, code_cfg.provider)
    pcall(redis_set, "code:debug_proxy", proxy_url or "direct")

    -- ★ 更新统计配置 (Rust 全局变量)
    pcall(stats_code_set_config, selected, code_cfg.provider, code_cfg.model)
    pcall(stats_code_set_selected, selected)

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
        new_request_body = new_body,
        need_transform = need_transform,
        proxy = proxy_url  -- 新增：代理URL
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

        -- ★ 提取 token 并更新统计
        if status == 200 and current_request.selected then
            local ok_parse, parsed = pcall(json_decode, body)
            if ok_parse and type(parsed) == "table" then
                local input_tokens = 0
                local output_tokens = 0

                -- OpenAI 格式: usage.prompt_tokens / usage.completion_tokens
                -- Anthropic 格式: usage.input_tokens / usage.output_tokens
                if parsed.usage then
                    input_tokens = parsed.usage.prompt_tokens or parsed.usage.input_tokens or 0
                    output_tokens = parsed.usage.completion_tokens or parsed.usage.output_tokens or 0
                end

                if input_tokens > 0 then
                    pcall(stats_code_add, current_request.selected, 1, input_tokens, output_tokens)
                end
            end
        end
    else
        pcall(redis_set, "RESPONSE_" .. ts, "no_body")
    end
end

function handler.on_error(upstream, err)
    -- 错误处理
end

-- Rust 层调用：OpenAI 响应简单字段映射
-- 参数: id, model, finish_reason, input_tokens, output_tokens, compressed_content, compressed_tool_calls
-- compressed_content / compressed_tool_calls 是 gzip+base64 编码的大字段，Lua 原样回传
function handler.on_transform_response(id, model, finish_reason, input_tokens, output_tokens, compressed_content, compressed_tool_calls)
    -- id 前缀映射: chatcmpl-xxx → msg_xxx
    local msg_id = id
    if id:find("chatcmpl-") == 1 then
        msg_id = "msg_" .. id:sub(9)
    end

    -- finish_reason → stop_reason
    local stop_reason = "end_turn"
    if finish_reason == "length" then
        stop_reason = "max_tokens"
    elseif finish_reason == "tool_calls" then
        stop_reason = "tool_use"
    end

    -- model 保持原始请求的 model (Rust 传入的 model 就是原始请求的 model)

    return {
        id = msg_id,
        model = model,
        stop_reason = stop_reason,
        input_tokens = input_tokens,
        output_tokens = output_tokens,
        compressed_content = compressed_content,
        compressed_tool_calls = compressed_tool_calls
    }
end
