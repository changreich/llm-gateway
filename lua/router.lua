-- LLM Gateway Router (SDK 版本)
--
-- 架构：
--   请求 -> SDK.transform_request -> 提供商 API -> SDK.transform_response -> 响应
--
-- 统一格式：OpenAI Chat Completions API
-- SDK 职责：请求/响应转换、端点映射、请求头处理

handler = {}

-- 从 config.lua 加载默认配置
local config_file = loadfile("lua/config.lua")
local default_config = config_file and config_file() or {}

-- 加载 SDK 模块
-- script_dir 由 Rust 传入
local sdk = dofile(script_dir .. "/sdk/init.lua")

-- Redis 配置
local redis_host = default_config.redis_host or "127.0.0.1"
local redis_port = default_config.redis_port or 6379
local redis_db = default_config.redis_db or 0

-- 网关配置
local cool_down = default_config.cool_down or 60
local cache_ttl = default_config.cache_ttl or 60

-- 提供商默认配置
local default_providers = default_config.providers or {}

-- LLM 默认配置
local default_llm = default_config.llm or {}

-- Embed/Rank 默认配置
local default_embed = default_config.embed or {}
local default_rank = default_config.rank or {}

-- 配置缓存
local config_cache = {}
local cache_time = 0
local config_refresh_interval = 10

-- 初始化随机数种子（用于负载均衡随机选择）
math.randomseed(os.time() * 1000 + (tonumber(tostring({}):sub(8)) or 0))

-- 统计缓存
local stats_cache = {
    total_calls = 0,
    total_prompt = 0,
    total_completion = 0,
    models = {},
    last_update = 0
}

-------------------------------------------------------------------------------
-- 初始化：将 config.lua 配置写入 Redis
-------------------------------------------------------------------------------

local function init_config_to_redis()
    local ok, initialized = pcall(redis_get, "llm:initialized")
    if ok and initialized == "1" then
        return
    end

    for name, cfg in pairs(default_providers) do
        local key = "provider:" .. name
        local value = (cfg.baseurl or "") .. "|" .. (cfg.apikey or "")
        pcall(redis_set, key, value)
    end

    for num, cfg in pairs(default_llm) do
        local key = "llm:" .. num
        local value = (cfg.provider or "") .. "|" .. (cfg.model or "") .. "|" .. tostring(cfg.cd or cool_down)
        pcall(redis_set, key, value)
    end

    if default_embed.provider then
        pcall(redis_set, "embed:provider", default_embed.provider)
    end
    if default_embed.model then
        pcall(redis_set, "embed:model", default_embed.model)
    end

    if default_rank.provider then
        pcall(redis_set, "rank:provider", default_rank.provider)
    end
    if default_rank.model then
        pcall(redis_set, "rank:model", default_rank.model)
    end

    pcall(redis_set, "llm:select", default_config.llm_selected or "01")
    pcall(redis_set, "llm:config:cool_down", tostring(cool_down))
    pcall(redis_set, "llm:initialized", "1")
end

-------------------------------------------------------------------------------
-- 配置加载
-------------------------------------------------------------------------------

local function load_settings()
    local ok, v
    ok, v = pcall(redis_get, "llm:config:cool_down")
    if ok and v and v ~= "" then cool_down = tonumber(v) or cool_down end
end

local function split_pipe(str)
    local parts = {}
    for part in string.gmatch(str, "([^|]+)") do
        table.insert(parts, part)
    end
    return parts
end

local function load_provider_config(provider_name)
    if not provider_name or provider_name == "" then
        return nil
    end

    local key = "provider:" .. provider_name
    local ok, result = pcall(redis_get, key)
    if ok and result and result ~= "" then
        local parts = split_pipe(result)
        if #parts >= 2 then
            return {
                baseurl = parts[1] or "",
                apikey = parts[2] or ""
            }
        end
    end

    if default_providers[provider_name] then
        return {
            baseurl = default_providers[provider_name].baseurl or "",
            apikey = default_providers[provider_name].apikey or ""
        }
    end

    return nil
end

local function load_llm_config(number)
    local key = "llm:" .. number
    local ok, result = pcall(redis_get, key)

    if ok and result and result ~= "" then
        local parts = split_pipe(result)
        if #parts >= 2 then
            local provider_name = parts[1]
            local provider_cfg = load_provider_config(provider_name)
            if provider_cfg then
                return {
                    number = number,
                    provider = provider_name,
                    baseurl = provider_cfg.baseurl,
                    apikey = provider_cfg.apikey,
                    model = parts[2] or "",
                    cd = tonumber(parts[3]) or cool_down
                }
            end
        end
    end

    if default_llm[number] then
        local cfg = default_llm[number]
        local provider_cfg = load_provider_config(cfg.provider)
        if provider_cfg then
            return {
                number = number,
                provider = cfg.provider,
                baseurl = provider_cfg.baseurl,
                apikey = provider_cfg.apikey,
                model = cfg.model or "",
                cd = cfg.cd or cool_down
            }
        end
    end

    return nil
end

local function load_config()
    load_settings()

    local now = os.time()
    if config_cache.llm and (now - cache_time) < config_refresh_interval then
        return config_cache
    end

    config_cache = {
        llm = {},
        embed = {},
        rank = {},
        selected = default_config.llm_selected or "01"
    }

    local ok, selected = pcall(redis_get, "llm:select")
    if ok and selected and selected ~= "" then
        config_cache.selected = selected
    end

    for i = 1, 20 do
        local num = string.format("%02d", i)
        local cfg = load_llm_config(num)
        if cfg then
            config_cache.llm[num] = cfg
        end
    end

    local ok2, provider_name = pcall(redis_get, "embed:provider")
    if not ok2 or not provider_name or provider_name == "" then
        provider_name = default_embed.provider
    end
    if provider_name then
        local provider_cfg = load_provider_config(provider_name)
        if provider_cfg then
            local ok3, model = pcall(redis_get, "embed:model")
            table.insert(config_cache.embed, {
                provider = provider_name,
                baseurl = provider_cfg.baseurl,
                apikey = provider_cfg.apikey,
                model = (ok3 and model and model ~= "") and model or default_embed.model or ""
            })
        end
    end

    local ok4, provider_name = pcall(redis_get, "rank:provider")
    if not ok4 or not provider_name or provider_name == "" then
        provider_name = default_rank.provider
    end
    if provider_name then
        local provider_cfg = load_provider_config(provider_name)
        if provider_cfg then
            local ok5, model = pcall(redis_get, "rank:model")
            table.insert(config_cache.rank, {
                provider = provider_name,
                baseurl = provider_cfg.baseurl,
                apikey = provider_cfg.apikey,
                model = (ok5 and model and model ~= "") and model or default_rank.model or ""
            })
        end
    end

    cache_time = now

    -- 同步配置到 Rust（用于 /running 页面，无阻塞）
    if stats_set_selected then
        pcall(stats_set_selected, config_cache.selected)
    end
    if stats_set_config then
        for num, cfg in pairs(config_cache.llm) do
            pcall(stats_set_config, num, cfg.provider or "?", cfg.model or "?")
        end
    end

    return config_cache
end

-------------------------------------------------------------------------------
-- LLM 模型选择 (轮询 + CD 跳过)
-------------------------------------------------------------------------------

local function get_model_cool_down(num)
    local cfg = config_cache.llm[num]
    if cfg and cfg.cd then
        return cfg.cd
    end
    return cool_down
end

-- 检查模型是否在冷却中
local function is_in_cool_down(num)
    local cool_down_key = "llm:cool-down:" .. num
    local ok, cool_until_str = pcall(redis_get, cool_down_key)
    if ok and cool_until_str and cool_until_str ~= "" then
        local cool_until = tonumber(cool_until_str) or 0
        if os.time() < cool_until then
            return true, cool_until
        end
    end
    return false, nil
end

-- 设置模型进入冷却状态
local function set_model_cool_down(num)
    if not num then return end
    local cd = get_model_cool_down(num)
    if cd <= 0 then return end
    local cool_down_key = "llm:cool-down:" .. num
    local cool_until = os.time() + cd
    pcall(redis_set, cool_down_key, tostring(cool_until))
    pcall(redis_expire, cool_down_key, cd + 10)
    pcall(redis_set, "llm:cd:log:" .. num,
        string.format("set at %s for %ds until %d", os.date("%H:%M:%S"), cd, cool_until))
end

-- 安全 INCR Redis 值
local function safe_redis_incr(key)
    local ok, val = pcall(redis_incr, key)
    if ok and val then
        return tonumber(val) or 0
    end
    return 0
end

-- 轮询选择模型：跳过 cd 中的模型
-- 返回: cfg, num, all_in_cd
local function select_llm(config)
    -- 获取所有模型编号并排序
    local nums = {}
    for n in pairs(config.llm) do
        table.insert(nums, n)
    end
    if #nums == 0 then
        return nil, nil, true
    end
    table.sort(nums)

    -- 轮询索引
    local idx_key = "llm:poll:idx"
    local start_idx = safe_redis_incr(idx_key) - 1

    -- 遍历所有模型，找到第一个不在 cd 中的
    for i = 0, #nums - 1 do
        local pos = ((start_idx + i) % #nums) + 1
        local num = nums[pos]
        local in_cd, cool_until = is_in_cool_down(num)
        if not in_cd then
            local cfg = config.llm[num]
            pcall(redis_set, "llm:debug_poll",
                string.format("idx=%d pos=%d selected=%s", start_idx, pos, num))
            return cfg, num, false
        end
    end

    -- 所有模型都在 cd 中
    pcall(redis_set, "llm:debug_poll", "all_in_cd")
    return nil, nil, true
end

-------------------------------------------------------------------------------
-- SDK 调用核心逻辑
-------------------------------------------------------------------------------

-- 保存原始请求/响应用到 Redis (保留最近5个)
local function save_raw_request(req_type, request, response)
    local timestamp = os.date("%Y-%m-%d %H:%M:%S")
    local entry = '{"time":"' .. timestamp .. '","type":"' .. req_type .. '","request":' .. request .. ',"response":' .. response .. '}'
    pcall(redis_lpush, "raw", entry)
    pcall(redis_ltrim, "raw", 0, 4)  -- 只保留最近5个
end

-- 通过 Rust 注册的 openai_call 函数发送请求
local function call_llm_sdk(cfg, body_str)
    if not cfg then
        return '{"error":"No LLM config"}'
    end

    -- 加载提供商 SDK
    local provider_sdk = sdk.load(cfg.provider)

    -- 1. 转换请求 (统一格式 -> 提供商格式)
    local transformed_body = body_str
    if provider_sdk and provider_sdk.transform_request then
        transformed_body = provider_sdk.transform_request(body_str, cfg.model, cfg)
    end

    -- 2. 获取端点路径
    local baseurl = cfg.baseurl or ""
    local endpoint = "/v1/chat/completions"
    if provider_sdk and provider_sdk.get_endpoint then
        endpoint = provider_sdk.get_endpoint(baseurl)
    end

    -- 3. 清理 baseurl 尾部斜杠
    if baseurl:sub(-1) == "/" then
        baseurl = baseurl:sub(1, -2)
    end

    -- 4. 构建请求 JSON (包含 baseurl, apikey, model)
    local request_json = transformed_body
    if request_json:sub(1, 1) == "{" then
        -- 添加 baseurl 和 api_key 字段 (Rust 端会识别并使用)
        request_json = request_json:gsub("^{", '{"baseurl":"' .. baseurl .. '","api_key":"' .. (cfg.apikey or "") .. '","endpoint":"' .. endpoint .. '",', 1)
    end

    -- 5. 调用 Rust 注册的 openai_call 函数
    local ok, response = pcall(openai_call, request_json)
    if not ok then
        response = '{"error":"openai_call failed: ' .. (response or "unknown") .. '"}'
    end

    -- 6. 保存原始请求/响应 (用于调试 claude-mem 失败问题)
    save_raw_request("llm", request_json, response)

    -- 7. 转换响应 (提供商格式 -> 统一格式)
    if ok and provider_sdk and provider_sdk.transform_response then
        response = provider_sdk.transform_response(response)
    end

    return response
end

-------------------------------------------------------------------------------
-- 主路由逻辑
-------------------------------------------------------------------------------

function handler.on_request(method, path, headers, body)
    init_config_to_redis()
    local config = load_config()

    -- /debug 端点
    if path == "/debug" then
        local llm_cfg, target_num = select_llm(config)
        local selected = config.selected or "01"

        -- 获取选中模型的调用次数
        local ok, main_count = pcall(redis_get, "llm:count:" .. selected)
        local count = 0
        if ok and main_count and main_count ~= "" then
            count = tonumber(main_count) or 0
        end

        -- 获取目标模型的调用次数
        local target_calls = get_model_calls(target_num)

        local provider_sdk = llm_cfg and sdk.load(llm_cfg.provider) or nil
        local endpoint = "/v1/chat/completions"
        if provider_sdk and provider_sdk.get_endpoint then
            endpoint = provider_sdk.get_endpoint(llm_cfg.baseurl)
        end

        return {
            action = "reject",
            status = 200,
            body = '{"status":"ok","selected":"' .. selected ..
                   '","target":"' .. target_num ..
                   '","target_calls":' .. target_calls ..
                   ',"cool_down":' .. cool_down ..
                   ',"provider":"' .. (llm_cfg and llm_cfg.provider or "?") ..
                   '","model":"' .. (llm_cfg and llm_cfg.model or "?") ..
                   '","endpoint":"' .. endpoint .. '"}'
        }
    end

    -- /settings 端点
    if path == "/settings" then
        return {
            action = "reject",
            status = 200,
            body = '{"cool_down":' .. cool_down ..
                   ',"cache_ttl":' .. cache_ttl .. '}'
        }
    end

    -- /config 端点
    if path == "/config" then
        local info = '{"selected":"' .. (config.selected or "?") .. '"'
        if config.llm[config.selected] then
            local cfg = config.llm[config.selected]
            info = info .. ',"provider":"' .. (cfg.provider or "?") ..
                        '","baseurl":"' .. (cfg.baseurl or "?") ..
                        '","model":"' .. (cfg.model or "?") .. '"'
        end
        info = info .. '}'
        return { action = "reject", status = 200, body = info }
    end

    -- /raw 端点：查看最近5个请求的原始数据
    if path == "/raw" then
        local ok, items = pcall(redis_lrange, "raw", 0, 4)
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

    -- /cd 端点：查看所有模型的 cd 状态
    if path == "/cd" then
        local status_list = {}
        for num, cfg in pairs(config.llm) do
            local in_cd, cool_until = is_in_cool_down(num)
            local cd_seconds = get_model_cool_down(num)
            table.insert(status_list, string.format(
                '{"num":"%s","provider":"%s","model":"%s","in_cd":%s,"cool_until":%s,"cd_seconds":%d}',
                num, cfg.provider or "", cfg.model or "",
                in_cd and "true" or "false",
                cool_until or "null",
                cd_seconds
            ))
        end
        table.sort(status_list)
        return { action = "reject", status = 200, body = "[" .. table.concat(status_list, ",") .. "]" }
    end

    -- /running 端点：展示运行统计（调用次数、token消耗）
    if path == "/running" then
        -- 从 Rust 获取总数（无阻塞原子读取）
        local rust_stats = stats_get and stats_get() or { calls = 0, prompt = 0, completion = 0 }

        local html = '<!DOCTYPE html><html><head><meta charset="UTF-8"><title>LLM Gateway</title>'
        html = html .. '<style>body{font-family:sans-serif;max-width:900px;margin:40px auto;padding:20px;background:#f5f5f5}h1{color:#333;border-bottom:2px solid #4CAF50;padding-bottom:10px}h2{color:#555;margin-top:25px}.card{background:white;border-radius:8px;padding:20px;margin:15px 0;box-shadow:0 2px 4px rgba(0,0,0,0.1)}table{width:100%;border-collapse:collapse}th,td{text-align:left;padding:10px;border-bottom:1px solid #eee}th{background:#f9f9f9}.stat-box{display:inline-block;background:linear-gradient(135deg,#667eea,#764ba2);color:white;padding:12px 20px;border-radius:8px;margin:5px;text-align:center}.stat-box .v{font-size:1.4em;font-weight:bold}.stat-box .l{font-size:0.75em;opacity:0.9}.stat-box.green{background:linear-gradient(135deg,#11998e,#38ef7d)}.stat-box.orange{background:linear-gradient(135deg,#f093fb,#f5576c)}.num{font-family:monospace;background:#e3f2fd;padding:2px 6px;border-radius:4px}.provider{color:#1976d2;font-weight:500}.selected{background:#fff3e0}.prompt{color:#4CAF50}.completion{color:#2196F3}</style>'
        html = html .. '</head><body><h1>LLM Gateway</h1>'

        -- 总计（从 Rust 原子读取，无阻塞）
        local total_calls = rust_stats.calls or 0
        local total_prompt = rust_stats.prompt or 0
        local total_completion = rust_stats.completion or 0
        html = html .. '<div class="stat-box"><div class="v">' .. total_calls .. '</div><div class="l">调用次数</div></div>'
        html = html .. '<div class="stat-box green"><div class="v">' .. total_prompt .. '</div><div class="l">Prompt</div></div>'
        html = html .. '<div class="stat-box orange"><div class="v">' .. total_completion .. '</div><div class="l">Completion</div></div>'
        html = html .. '<div class="stat-box"><div class="v">' .. (total_prompt + total_completion) .. '</div><div class="l">总Token</div></div>'

        -- 模型表
        html = html .. '<div class="card"><h2>模型统计</h2><table>'
        html = html .. '<tr><th>编号</th><th>提供商</th><th>模型</th><th>调用</th><th class="prompt">Prompt</th><th class="completion">Completion</th><th>最近</th></tr>'

        local nums = {}
        for n in pairs(config.llm) do table.insert(nums, n) end
        table.sort(nums)

        for _, num in ipairs(nums) do
            local cfg = config.llm[num]
            local cached = stats_cache.models[num] or { calls = 0, prompt = 0, completion = 0, last_prompt = 0, last_completion = 0 }
            local sel = num == config.selected and ' class="selected"' or ""
            html = html .. '<tr' .. sel .. '>'
            html = html .. '<td><span class="num">' .. num .. '</span>' .. (num == config.selected and ' *' or '') .. '</td>'
            html = html .. '<td><span class="provider">' .. (cfg.provider or "?") .. '</span></td>'
            html = html .. '<td style="font-size:0.8em;color:#666">' .. (cfg.model or "?") .. '</td>'
            html = html .. '<td>' .. cached.calls .. '</td>'
            html = html .. '<td class="prompt">' .. cached.prompt .. '</td>'
            html = html .. '<td class="completion">' .. cached.completion .. '</td>'
            html = html .. '<td style="font-size:0.8em;color:#888">'
            if cached.last_prompt > 0 then
                html = html .. '<span class="prompt">' .. cached.last_prompt .. '</span>+<span class="completion">' .. cached.last_completion .. '</span>'
            else
                html = html .. '-'
            end
            html = html .. '</td></tr>'
        end
        html = html .. '</table></div>'

        html = html .. '<p style="color:#999;text-align:center;margin-top:20px">LLM Gateway | <a href="/debug">debug</a> | <a href="/config">config</a> | <a href="/raw">raw</a></p>'
        html = html .. '</body></html>'

        return { action = "reject", status = 200, body = html }
    end

    -- /test 端点：测试 SDK 调用
    if path == "/test" then
        local llm_cfg, _, _ = select_llm(config)
        if not llm_cfg then
            return { action = "reject", status = 503, body = '{"error":"No LLM available"}' }
        end

        -- 测试请求
        local test_body = '{"model":"test","messages":[{"role":"user","content":"Say hello in one word"}],"max_tokens":100}'

        -- 使用 SDK 调用
        local response = call_llm_sdk(llm_cfg, test_body)

        return {
            action = "reject",
            status = 200,
            body = response
        }
    end

    -- Embeddings 路由
    if path:match("/embed") or path:match("/v1/embeddings") then
        if #config.embed > 0 then
            local cfg = config.embed[1]
            local provider_sdk = sdk.load(cfg.provider)

            -- 转换请求
            local new_body = body
            if provider_sdk and provider_sdk.transform_request then
                new_body = provider_sdk.transform_request(body, cfg.model, cfg)
            end

            -- 获取端点
            local baseurl = cfg.baseurl or ""
            local endpoint = "/v1/embeddings"
            if provider_sdk and provider_sdk.get_endpoint then
                endpoint = provider_sdk.get_endpoint(baseurl)
            end

            -- 清理 baseurl 尾部斜杠
            if baseurl:sub(-1) == "/" then baseurl = baseurl:sub(1, -2) end

            local request_json = new_body
            if request_json:sub(1, 1) == "{" then
                request_json = request_json:gsub("^{", '{"baseurl":"' .. baseurl .. '","api_key":"' .. (cfg.apikey or "") .. '","endpoint":"' .. endpoint .. '",', 1)
            end

            local ok, response = pcall(openai_call, request_json)
            if not ok then
                return { action = "reject", status = 500, body = '{"error":"embed call failed"}' }
            end

            -- 转换响应
            if provider_sdk and provider_sdk.transform_response then
                response = provider_sdk.transform_response(response)
            end

            return { action = "reject", status = 200, body = response }
        end
    end

    -- Rerank 路由
    if path:match("/rerank") then
        if #config.rank > 0 then
            local cfg = config.rank[1]
            local provider_sdk = sdk.load(cfg.provider)

            local new_body = body
            if provider_sdk and provider_sdk.transform_request then
                new_body = provider_sdk.transform_request(body, cfg.model, cfg)
            end

            local baseurl = cfg.baseurl or ""
            if baseurl:sub(-1) == "/" then baseurl = baseurl:sub(1, -2) end

            local request_json = new_body
            if request_json:sub(1, 1) == "{" then
                request_json = request_json:gsub("^{", '{"baseurl":"' .. baseurl .. '","api_key":"' .. (cfg.apikey or "") .. '","endpoint":"/v1/rerank",', 1)
            end

            local ok, response = pcall(openai_call, request_json)
            if not ok then
                return { action = "reject", status = 500, body = '{"error":"rerank call failed"}' }
            end

            return { action = "reject", status = 200, body = response }
        end
    end

    -- LLM 路由：只处理 /openai 前缀的请求
    -- /openai/v1/chat/completions 或 /openai/chat/completions
    if path:match("^/openai/") then
        local llm_cfg, target_num, all_in_cd = select_llm(config)

        -- 所有模型都在 cd 中，返回 429
        if all_in_cd or not llm_cfg then
            return {
                action = "reject",
                status = 429,
                body = '{"error":"all models in cooldown","retry_after":' .. cool_down .. '}'
            }
        end

        local model_num = target_num

        -- 使用 SDK 调用 LLM (model 字段会被替换为配置的模型)
        local response = call_llm_sdk(llm_cfg, body)

        -- 检查响应是否失败（包含 error 字段表示失败）
        local has_error = response:match('"error"')
        local has_valid_response = response:match('"choices"') or response:match('"data"')

        -- 如果失败，设置该模型进入 cd
        if has_error and not has_valid_response then
            set_model_cool_down(model_num)
            pcall(redis_incr, "llm:errors:" .. model_num)
            pcall(redis_set, "llm:last_error:" .. model_num,
                string.format("time=%s", os.date("%H:%M:%S")))
        end

        -- 计数：按模型编号统计
        pcall(redis_incr, "llm:count:" .. model_num)

        -- 提取 token 用量并按模型统计
        local provider_sdk = sdk.load(llm_cfg.provider)
        if provider_sdk and provider_sdk.extract_tokens then
            local prompt_tokens, completion_tokens = provider_sdk.extract_tokens(response)
            local total = prompt_tokens + completion_tokens
            if total > 0 then
                -- 按 provider 统计（保留旧逻辑）
                local provider_key = "llm:tokens:" .. llm_cfg.provider
                pcall(redis_set, provider_key .. ":last", tostring(total))
                for i = 1, total do
                    pcall(redis_incr, provider_key)
                end

                -- 按模型编号统计 - Redis
                local model_key = "llm:model:" .. model_num
                pcall(redis_incr, model_key .. ":calls")
                pcall(redis_incrby, model_key .. ":prompt", prompt_tokens)
                pcall(redis_incrby, model_key .. ":completion", completion_tokens)
                pcall(redis_incrby, model_key .. ":total", total)
                pcall(redis_set, model_key .. ":last_prompt", tostring(prompt_tokens))
                pcall(redis_set, model_key .. ":last_completion", tostring(completion_tokens))
                pcall(redis_set, model_key .. ":last_total", tostring(total))

                -- 更新缓存（用于 /running 页面）
                if not stats_cache.models[model_num] then
                    stats_cache.models[model_num] = { calls = 0, prompt = 0, completion = 0 }
                end
                local m = stats_cache.models[model_num]
                m.calls = m.calls + 1
                m.prompt = m.prompt + prompt_tokens
                m.completion = m.completion + completion_tokens
                m.last_prompt = prompt_tokens
                m.last_completion = completion_tokens
                stats_cache.total_calls = stats_cache.total_calls + 1
                stats_cache.total_prompt = stats_cache.total_prompt + prompt_tokens
                stats_cache.total_completion = stats_cache.total_completion + completion_tokens

                -- 同步到 Rust 全局缓存（无阻塞原子操作）
                if stats_update_model then
                    pcall(stats_update_model, model_num, 1, prompt_tokens, completion_tokens, prompt_tokens, completion_tokens)
                end
            end
        end

        return {
            action = "reject",
            status = 200,
            body = response
        }
    end

    -- 其他路径返回 404
    return {
        action = "reject",
        status = 404,
        body = '{"error":"Not found. Use /openai/v1/chat/completions for LLM requests."}'
    }
end

function handler.on_response(upstream, status, body)
    -- SDK 版本中，响应已在 on_request 中处理
    -- 此回调保留用于日志记录
end

function handler.on_error(upstream, err)
    pcall(redis_incr, "llm:errors:" .. upstream)
end
