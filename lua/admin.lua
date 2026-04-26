-- LLM Gateway Admin Console (端口 9093)
--
-- 功能：Redis 配置管理界面

handler = {}

-------------------------------------------------------------------------------
-- 工具函数
-------------------------------------------------------------------------------

local function split_pipe(str)
    local parts = {}
    for part in string.gmatch(str, "([^|]+)") do
        table.insert(parts, part)
    end
    return parts
end

local function safe_redis_get(key)
    local ok, val = pcall(redis_get, key)
    if ok and val and val ~= "" then
        return val
    end
    return nil
end

local function safe_redis_keys(pattern)
    local ok, keys = pcall(redis_keys, pattern)
    if ok and keys then
        return keys
    end
    return {}
end

local function escape_html(str)
    return str:gsub("&", "&amp;"):gsub("<", "&lt;"):gsub(">", "&gt;"):gsub('"', "&quot;")
end

-- 转义 gsub 替换字符串中的 % 字符
local function escape_gsub(str)
    if not str then return "" end
    return (str:gsub("%%", "%%%%"))  -- 用括号只返回第一个值
end

-- URL 解码
local function url_decode(str)
    if not str then return "" end
    return str:gsub("%%(%x%x)", function(hex)
        return string.char(tonumber(hex, 16))
    end):gsub("+", " ")
end

-------------------------------------------------------------------------------
-- 配置读取
-------------------------------------------------------------------------------

local function get_all_llm_configs()
    local configs = {}
    for i = 1, 20 do
        local num = string.format("%02d", i)
        local val = safe_redis_get("llm:" .. num)
        if val then
            local parts = split_pipe(val)
            if #parts >= 2 then
                configs[num] = { provider = parts[1], model = parts[2], cd = tonumber(parts[3]) or 60 }
            end
        end
    end
    return configs
end

local function get_all_code_configs()
    local configs = {}
    for i = 1, 20 do
        local num = string.format("%02d", i)
        local val = safe_redis_get("code:" .. num)
        if val then
            local parts = split_pipe(val)
            if #parts >= 2 then
                configs[num] = {
                    provider = parts[1],
                    model = parts[2],
                    sdk = parts[3] or "openai",      -- SDK 类型
                    params = parts[4] or ""           -- 参数覆盖配置引用
                }
            end
        end
    end
    return configs
end

local function get_all_opt_configs()
    local configs = {}
    local keys = safe_redis_keys("opt:*")
    for _, key in ipairs(keys) do
        local opt_id, field = key:match("opt:(%d+):(.+)")
        if opt_id and field then
            local val = safe_redis_get(key)
            if val then
                if not configs[opt_id] then configs[opt_id] = {} end
                configs[opt_id][field] = val
            end
        end
    end
    return configs
end

local function get_all_providers()
    local providers = {}
    local keys = safe_redis_keys("provider:*")
    for _, key in ipairs(keys) do
        local name = key:match("provider:([^:]+)$")
        if name then
            local val = safe_redis_get(key)
            if val then
                local parts = split_pipe(val)
                if #parts >= 2 then
                    providers[name] = {
                        baseurl = parts[1],
                        apikey = parts[2],
                        proxy = safe_redis_get("provider:" .. name .. ":proxy") or ""
                    }
                end
            end
        end
    end
    return providers
end

local function get_all_modelmaps()
    local modelmaps = {}
    local keys = safe_redis_keys("modelmap:*")
    for _, key in ipairs(keys) do
        local name = key:match("modelmap:([^:]+)$")
        if name then
            local val = safe_redis_get(key)
            if val then modelmaps[name] = val end
        end
    end
    return modelmaps
end

local function get_llm_cd_status()
    local status = {}
    local keys = safe_redis_keys("llm:cool-down:*")
    for _, key in ipairs(keys) do
        local num = key:match("llm:cool%-down:(.+)")
        if num then
            local val = safe_redis_get(key)
            if val then
                local remaining = math.max(0, (tonumber(val) or 0) - os.time())
                if remaining > 0 then status[num] = remaining end
            end
        end
    end
    return status
end

local function get_code_cd_status()
    local status = {}
    local keys = safe_redis_keys("code:cd:*")
    for _, key in ipairs(keys) do
        if not key:find("config") and not key:find("default") and not key:find("log") then
            local num = key:match("code:cd:(.+)")
            if num then
                local val = safe_redis_get(key)
                if val then
                    local remaining = math.max(0, (tonumber(val) or 0) - os.time())
                    if remaining > 0 then status[num] = remaining end
                end
            end
        end
    end
    return status
end

-------------------------------------------------------------------------------
-- 配置导出
-------------------------------------------------------------------------------

-- 生成 Lua 配置文件内容
local function generate_config_lua()
    local lines = {}
    table.insert(lines, "-- LLM Gateway 配置文件")
    table.insert(lines, "-- 由 Admin Console 自动生成: " .. os.date("%Y-%m-%d %H:%M:%S"))
    table.insert(lines, "")
    table.insert(lines, "return {")
    table.insert(lines, '    redis_host = "127.0.0.1",')
    table.insert(lines, '    redis_port = 7379,')
    table.insert(lines, '    redis_db = 0,')
    table.insert(lines, "")

    -- 全局配置
    local cool_down = tonumber(safe_redis_get("llm:config:cool_down")) or 60
    local llm_select = safe_redis_get("llm:select") or "01"
    local code_select = safe_redis_get("code:select") or "01"
    local global_proxy = safe_redis_get("global:proxy") or ""

    table.insert(lines, string.format('    cool_down = %d,', cool_down))
    table.insert(lines, string.format('    llm_selected = "%s",', llm_select))
    table.insert(lines, string.format('    code_selected = "%s",', code_select))
    table.insert(lines, "")

    -- 全局代理 (可选)
    if global_proxy ~= "" then
        table.insert(lines, string.format('    proxy = "%s",', global_proxy))
        table.insert(lines, "")
    end

    -- providers
    local providers = get_all_providers()
    local provider_names = {}
    for n in pairs(providers) do table.insert(provider_names, n) end
    table.sort(provider_names)

    table.insert(lines, "    providers = {")
    for _, name in ipairs(provider_names) do
        local cfg = providers[name]
        local line = string.format('        %s = {baseurl = "%s", apikey = "%s"',
            name, cfg.baseurl, cfg.apikey)
        if cfg.proxy and cfg.proxy ~= "" then
            line = line .. string.format(', proxy = "%s"', cfg.proxy)
        end
        line = line .. "},"
        table.insert(lines, line)
    end
    table.insert(lines, "    },")
    table.insert(lines, "")

    -- llm
    local llm_configs = get_all_llm_configs()
    local llm_nums = {}
    for n in pairs(llm_configs) do table.insert(llm_nums, n) end
    table.sort(llm_nums)

    table.insert(lines, "    llm = {")
    for _, num in ipairs(llm_nums) do
        local cfg = llm_configs[num]
        table.insert(lines, string.format('        ["%s"] = {provider = "%s", model = "%s", cd = %d},',
            num, cfg.provider, cfg.model, cfg.cd))
    end
    table.insert(lines, "    },")
    table.insert(lines, "")

    -- code
    local code_configs = get_all_code_configs()
    local code_nums = {}
    for n in pairs(code_configs) do table.insert(code_nums, n) end
    table.sort(code_nums)

    if #code_nums > 0 then
        table.insert(lines, "    code = {")
        for _, num in ipairs(code_nums) do
            local cfg = code_configs[num]
            local line = string.format('        ["%s"] = {provider = "%s", model = "%s", sdk = "%s"',
                num, cfg.provider, cfg.model, cfg.sdk)
            if cfg.params and cfg.params ~= "" then
                line = line .. string.format(', params = "%s"', cfg.params)
            end
            -- 添加 proxy 字段
            local code_proxy = safe_redis_get("code:" .. num .. ":proxy") or ""
            line = line .. string.format(', proxy = "%s"', code_proxy)
            line = line .. "},"
            table.insert(lines, line)
        end
        table.insert(lines, "    },")
        table.insert(lines, "")
    end

    -- opt
    local opt_configs = get_all_opt_configs()
    local opt_ids = {}
    for n in pairs(opt_configs) do table.insert(opt_ids, n) end
    table.sort(opt_ids)

    if #opt_ids > 0 then
        table.insert(lines, "    opt = {")
        for _, opt_id in ipairs(opt_ids) do
            local fields = opt_configs[opt_id]
            local parts = {}
            local field_keys = {}
            for k in pairs(fields) do table.insert(field_keys, k) end
            table.sort(field_keys)
            for _, k in ipairs(field_keys) do
                table.insert(parts, string.format('%s = "%s"', k, fields[k]))
            end
            table.insert(lines, string.format('        ["%s"] = {%s},', opt_id, table.concat(parts, ", ")))
        end
        table.insert(lines, "    },")
        table.insert(lines, "")
    end

    -- modelmap
    local modelmaps = get_all_modelmaps()
    local modelmap_names = {}
    for n in pairs(modelmaps) do table.insert(modelmap_names, n) end
    table.sort(modelmap_names)

    if #modelmap_names > 0 then
        table.insert(lines, "    modelmap = {")
        for _, name in ipairs(modelmap_names) do
            table.insert(lines, string.format('        ["%s"] = "%s",', name, modelmaps[name]))
        end
        table.insert(lines, "    },")
        table.insert(lines, "")
    end

    -- embed/rank 保持默认
    table.insert(lines, '    embed = {provider = "Local1", model = "bge-large-zh-v1.5-q8_0"},')
    table.insert(lines, '    rank = {provider = "Local2", model = "qwen3-reranker-0.6b-q8_0"}')

    table.insert(lines, "}")
    table.insert(lines, "")

    return table.concat(lines, "\n")
end

-------------------------------------------------------------------------------
-- HTML 生成
-------------------------------------------------------------------------------

local function generate_admin_html()
    -- 读取模板
    local template_file = io.open(script_dir .. "/admin.html", "r")
    if not template_file then
        return "Error: admin.html not found"
    end
    local html = template_file:read("*a")
    template_file:close()

    -- 获取数据
    local llm_configs = get_all_llm_configs()
    local code_configs = get_all_code_configs()
    local opt_configs = get_all_opt_configs()
    local providers = get_all_providers()
    local modelmaps = get_all_modelmaps()
    local llm_cd_status = get_llm_cd_status()
    local code_cd_status = get_code_cd_status()

    local llm_selected = safe_redis_get("llm:select") or "01"
    local code_selected = safe_redis_get("code:select") or "01"

    -- Provider 下拉框
    local provider_names = {}
    for name in pairs(providers) do table.insert(provider_names, name) end
    table.sort(provider_names)
    local provider_options = '<option value="">选择 Provider</option>'
    for _, name in ipairs(provider_names) do
        provider_options = provider_options .. string.format('<option value="%s">%s</option>', escape_html(name), escape_html(name))
    end

    -- LLM 下拉框和表格
    local llm_options, llm_rows = "", ""
    local llm_nums = {}
    for n in pairs(llm_configs) do table.insert(llm_nums, n) end
    table.sort(llm_nums)

    for _, num in ipairs(llm_nums) do
        local cfg = llm_configs[num]
        local sel = num == llm_selected and " selected" or ""
        llm_options = llm_options .. string.format('<option value="%s"%s>%s - %s (%s)</option>', num, sel, num, escape_html(cfg.provider), escape_html(cfg.model))

        local cd_remaining = llm_cd_status[num] or 0
        local status_icon = cd_remaining > 0 and '<span class="status-dot cd"></span>冷却中' or '<span class="status-dot ok"></span>正常'
        llm_rows = llm_rows .. string.format(
            '<tr data-id="%s" data-provider="%s" data-model="%s" data-cd="%d"><td><span class="num%s">%s</span></td><td><span class="provider">%s</span></td>' ..
            '<td><span class="model">%s</span></td><td>%d 秒</td>' ..
            '<td class="status">%s</td></tr>',
            num, escape_html(cfg.provider), escape_html(cfg.model), cfg.cd,
            sel == " selected" and " active" or "", num, escape_html(cfg.provider),
            escape_html(cfg.model), cfg.cd, status_icon
        )
    end

    -- Code 下拉框和表格
    local code_options, code_rows = "", ""
    local code_nums = {}
    for n in pairs(code_configs) do table.insert(code_nums, n) end
    table.sort(code_nums)

    for _, num in ipairs(code_nums) do
        local cfg = code_configs[num]
        local sel = num == code_selected and " selected" or ""
        code_options = code_options .. string.format('<option value="%s"%s>%s - %s (%s)</option>', num, sel, num, escape_html(cfg.provider), escape_html(cfg.model))

        local cd_remaining = code_cd_status[num] or 0
        local status_icon = cd_remaining > 0 and '<span class="status-dot cd"></span>冷却中' or '<span class="status-dot ok"></span>正常'
        code_rows = code_rows .. string.format(
            '<tr data-id="%s" data-provider="%s" data-model="%s" data-sdk="%s" data-params="%s"><td><span class="num%s">%s</span></td><td><span class="provider">%s</span></td>' ..
            '<td><span class="model">%s</span></td><td><span class="sdk">%s</span></td><td>%s</td>' ..
            '<td class="status">%s</td></tr>',
            num, escape_html(cfg.provider), escape_html(cfg.model), escape_html(cfg.sdk), escape_html(cfg.params),
            sel == " selected" and " active" or "", num, escape_html(cfg.provider),
            escape_html(cfg.model), escape_html(cfg.sdk), cfg.params ~= "" and escape_html(cfg.params) or "-", status_icon
        )
    end

    -- Opt 表格
    local opt_rows = ""
    local opt_ids = {}
    for id in pairs(opt_configs) do table.insert(opt_ids, id) end
    table.sort(opt_ids)
    for _, opt_id in ipairs(opt_ids) do
        local fields = opt_configs[opt_id]
        local field_str = ""
        for k, v in pairs(fields) do
            if field_str ~= "" then field_str = field_str .. ", " end
            field_str = field_str .. k .. "=" .. v
        end
        opt_rows = opt_rows .. string.format(
            '<tr data-id="%s" data-fields="%s"><td><span class="num">%s</span></td>' ..
            '<td><span class="model">%s</span></td></tr>',
            opt_id, escape_html(field_str), opt_id, escape_html(field_str)
        )
    end

    -- Provider 表格
    local provider_rows = ""
    for _, name in ipairs(provider_names) do
        local cfg = providers[name]
        provider_rows = provider_rows .. string.format(
            '<tr data-name="%s" data-baseurl="%s" data-apikey="%s" data-proxy="%s"><td><span class="provider">%s</span></td>' ..
            '<td><span class="model">%s</span></td>' ..
            '<td>%s</td></tr>',
            escape_html(name), escape_html(cfg.baseurl), escape_html(cfg.apikey), escape_html(cfg.proxy),
            escape_html(name), escape_html(cfg.baseurl),
            cfg.proxy ~= "" and escape_html(cfg.proxy) or "-"
        )
    end

    -- ModelMap 表格
    local modelmap_rows = ""
    local modelmap_names = {}
    for n in pairs(modelmaps) do table.insert(modelmap_names, n) end
    table.sort(modelmap_names)
    for _, name in ipairs(modelmap_names) do
        modelmap_rows = modelmap_rows .. string.format(
            '<tr data-name="%s" data-value="%s"><td><span class="model">%s</span></td>' ..
            '<td><span class="num">%s</span></td></tr>',
            escape_html(name), escape_html(modelmaps[name]),
            escape_html(name), escape_html(modelmaps[name])
        )
    end

    -- 替换计数占位符
    html = html:gsub("{{LLM_COUNT}}", tostring(#llm_nums))
    html = html:gsub("{{CODE_COUNT}}", tostring(#code_nums))
    html = html:gsub("{{PROVIDER_COUNT}}", tostring(#provider_names))
    html = html:gsub("{{OPT_COUNT}}", tostring(#opt_ids))
    html = html:gsub("{{MODELMAP_COUNT}}", tostring(#modelmap_names))
    html = html:gsub("{{TOTAL_CALLS}}", safe_redis_get("llm:count:total") or "0")

    -- 替换表格行占位符 (使用 escape_gsub 防止 % 字符导致捕获组错误)
    html = html:gsub("{{LLM_ROWS}}", escape_gsub(llm_rows))
    html = html:gsub("{{CODE_ROWS}}", escape_gsub(code_rows))
    html = html:gsub("{{PROVIDER_ROWS}}", escape_gsub(provider_rows))
    html = html:gsub("{{OPT_ROWS}}", escape_gsub(opt_rows))
    html = html:gsub("{{MODELMAP_ROWS}}", escape_gsub(modelmap_rows))

    -- 替换选项占位符
    html = html:gsub("{{LLM_OPTIONS}}", escape_gsub(llm_options))
    html = html:gsub("{{CODE_OPTIONS}}", escape_gsub(code_options))
    html = html:gsub("{{PROVIDER_OPTIONS}}", escape_gsub(provider_options))

    return html
end

-------------------------------------------------------------------------------
-- API 端点
-------------------------------------------------------------------------------

function handler.on_request(method, path, headers, body)
    -- 主页
    if path == "/" or path:match("^/%?") or path == "" then
        local ok, result = pcall(generate_admin_html)
        if not ok then
            return { action = "reject", status = 500, body = "Error: " .. tostring(result) }
        end
        return { action = "reject", status = 200, body = result }
    end

    -- API: 设置配置
    if path:match("^/api/set") then
        local key = url_decode(path:match("key=([^&]+)") or "")
        local value = url_decode(path:match("value=([^&]+)") or "")
        if key and key ~= "" then
            local ok = pcall(redis_set, key, value)
            return { action = "reject", status = 200, body = ok and '{"ok":true}' or '{"ok":false,"error":"redis error"}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing key"}' }
    end

    -- API: 获取 LLM 配置
    if path:match("^/api/get%-llm") then
        local num = path:match("num=([^&]+)") or ""
        if num and num ~= "" then
            local val = safe_redis_get("llm:" .. num)
            if val then
                local parts = split_pipe(val)
                if #parts >= 2 then
                    return { action = "reject", status = 200, body = '{"ok":true,"provider":"' .. parts[1] .. '","model":"' .. parts[2] .. '"}' }
                end
            end
        end
        return { action = "reject", status = 200, body = '{"ok":false}' }
    end

    -- API: 获取 Code 配置
    if path:match("^/api/get%-code") then
        local num = path:match("num=([^&]+)") or ""
        if num and num ~= "" then
            local val = safe_redis_get("code:" .. num)
            if val then
                local parts = split_pipe(val)
                if #parts >= 2 then
                    return { action = "reject", status = 200, body = '{"ok":true,"provider":"' .. parts[1] .. '","model":"' .. parts[2] .. '","sdk":"' .. (parts[3] or "openai") .. '","params":"' .. (parts[4] or "") .. '"}' }
                end
            end
        end
        return { action = "reject", status = 200, body = '{"ok":false}' }
    end

    -- API: 添加/更新 Provider
    if path:match("^/api/add%-provider") then
        local name = url_decode(path:match("name=([^&]+)") or "")
        local baseurl = url_decode(path:match("baseurl=([^&]+)") or "")
        local apikey = url_decode(path:match("apikey=([^&]+)") or "")
        local proxy = url_decode(path:match("proxy=([^&]+)") or "")
        if name and name ~= "" and baseurl and baseurl ~= "" and apikey and apikey ~= "" then
            pcall(redis_set, "provider:" .. name, baseurl .. "|" .. apikey)
            if proxy and proxy ~= "" then
                pcall(redis_set, "provider:" .. name .. ":proxy", proxy)
            else
                pcall(redis_del, "provider:" .. name .. ":proxy")
            end
            return { action = "reject", status = 200, body = '{"ok":true}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing params"}' }
    end

    -- API: 删除 Provider
    if path:match("^/api/del%-provider") then
        local name = path:match("name=([^&]+)") or ""
        if name and name ~= "" then
            pcall(redis_del, "provider:" .. name)
            pcall(redis_del, "provider:" .. name .. ":proxy")
            return { action = "reject", status = 200, body = '{"ok":true}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing name"}' }
    end

    -- API: 设置 LLM 选中模型
    if path:match("^/api/set%-llm%-select") then
        local num = path:match("num=([^&]+)") or ""
        if num and num ~= "" then
            local ok = pcall(redis_set, "llm:select", num)
            return { action = "reject", status = 200, body = ok and '{"ok":true}' or '{"ok":false,"error":"redis error"}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing num"}' }
    end

    -- API: 添加/更新 LLM 配置
    if path:match("^/api/add%-llm") then
        local num = url_decode(path:match("num=([^&]+)") or "")
        local provider = url_decode(path:match("provider=([^&]+)") or "")
        local model = url_decode(path:match("model=([^&]+)") or "")
        local cd = url_decode(path:match("cd=([^&]+)") or "60")
        if num and num ~= "" and provider and provider ~= "" and model and model ~= "" then
            local ok = pcall(redis_set, "llm:" .. num, provider .. "|" .. model .. "|" .. cd)
            return { action = "reject", status = 200, body = ok and '{"ok":true}' or '{"ok":false,"error":"redis error"}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing params"}' }
    end

    -- API: 删除 LLM 配置
    if path:match("^/api/del%-llm") then
        local num = path:match("num=([^&]+)") or ""
        if num and num ~= "" then
            pcall(redis_del, "llm:" .. num)
            return { action = "reject", status = 200, body = '{"ok":true}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing num"}' }
    end

    -- API: 清除单个 LLM CD
    if path:match("^/api/clear%-llm%-cd") then
        local num = path:match("num=([^&]+)") or ""
        if num and num ~= "" then
            pcall(redis_del, "llm:cool-down:" .. num)
            return { action = "reject", status = 200, body = '{"ok":true}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing num"}' }
    end

    -- API: 清除所有 LLM CD
    if path:match("^/api/clear%-all%-llm%-cd") then
        local keys = safe_redis_keys("llm:cool-down:*")
        for _, key in ipairs(keys) do pcall(redis_del, key) end
        return { action = "reject", status = 200, body = '{"ok":true}' }
    end

    -- API: 设置 Code 选中配置
    if path:match("^/api/set%-code%-select") then
        local num = path:match("num=([^&]+)") or ""
        if num and num ~= "" then
            local ok = pcall(redis_set, "code:select", num)
            return { action = "reject", status = 200, body = ok and '{"ok":true}' or '{"ok":false,"error":"redis error"}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing num"}' }
    end

    -- API: 添加/更新 Code 配置
    if path:match("^/api/add%-code") then
        local num = url_decode(path:match("num=([^&]+)") or "")
        local provider = url_decode(path:match("provider=([^&]+)") or "")
        local model = url_decode(path:match("model=([^&]+)") or "")
        local sdk = url_decode(path:match("sdk=([^&]+)") or "openai")
        local params = url_decode(path:match("params=([^&]+)") or "")
        if num and num ~= "" and provider and provider ~= "" and model and model ~= "" then
            local ok = pcall(redis_set, "code:" .. num, provider .. "|" .. model .. "|" .. sdk .. "|" .. params)
            return { action = "reject", status = 200, body = ok and '{"ok":true}' or '{"ok":false,"error":"redis error"}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing params"}' }
    end

    -- API: 删除 Code 配置
    if path:match("^/api/del%-code") then
        local num = path:match("num=([^&]+)") or ""
        if num and num ~= "" then
            pcall(redis_del, "code:" .. num)
            return { action = "reject", status = 200, body = '{"ok":true}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing num"}' }
    end

    -- API: 清除单个 Code CD
    if path:match("^/api/clear%-code%-cd") then
        local num = path:match("num=([^&]+)") or ""
        if num and num ~= "" then
            pcall(redis_del, "code:cd:" .. num)
            return { action = "reject", status = 200, body = '{"ok":true}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing num"}' }
    end

    -- API: 清除所有 Code CD
    if path:match("^/api/clear%-all%-code%-cd") then
        local keys = safe_redis_keys("code:cd:*")
        for _, key in ipairs(keys) do
            if not key:find("config") and not key:find("default") and not key:find("log") then
                pcall(redis_del, key)
            end
        end
        return { action = "reject", status = 200, body = '{"ok":true}' }
    end

    -- API: 保存 Opt 配置
    if path:match("^/api/save%-opt") then
        local opt_id = url_decode(path:match("id=([^&]+)") or "")
        if opt_id and opt_id ~= "" then
            local saved = 0
            for key, value in path:gmatch("([%w_]+)=([^&]+)") do
                if key ~= "id" then
                    pcall(redis_set, "opt:" .. opt_id .. ":" .. key, url_decode(value))
                    saved = saved + 1
                end
            end
            if saved > 0 then
                return { action = "reject", status = 200, body = '{"ok":true,"saved":' .. saved .. '}' }
            end
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing params"}' }
    end

    -- API: 删除 Opt 配置
    if path:match("^/api/del%-opt") then
        local opt_id = path:match("id=([^&]+)") or ""
        if opt_id and opt_id ~= "" then
            local keys = safe_redis_keys("opt:" .. opt_id .. ":*")
            for _, key in ipairs(keys) do pcall(redis_del, key) end
            return { action = "reject", status = 200, body = '{"ok":true,"deleted":' .. #keys .. '}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing id"}' }
    end

    -- API: 添加/更新 ModelMap
    if path:match("^/api/add%-modelmap") then
        local name = url_decode(path:match("name=([^&]+)") or "")
        local value = url_decode(path:match("value=([^&]+)") or "")
        if name and name ~= "" and value and value ~= "" then
            local ok = pcall(redis_set, "modelmap:" .. name, value)
            return { action = "reject", status = 200, body = ok and '{"ok":true}' or '{"ok":false,"error":"redis error"}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing params"}' }
    end

    -- API: 删除 ModelMap
    if path:match("^/api/del%-modelmap") then
        local name = path:match("name=([^&]+)") or ""
        if name and name ~= "" then
            pcall(redis_del, "modelmap:" .. name)
            pcall(redis_del, "modelmap:" .. name .. ":idx")
            return { action = "reject", status = 200, body = '{"ok":true}' }
        end
        return { action = "reject", status = 400, body = '{"ok":false,"error":"missing name"}' }
    end

    -- API: 重置统计
    if path:match("^/api/reset%-stats") then
        local keys1 = safe_redis_keys("llm:count:*")
        for _, key in ipairs(keys1) do pcall(redis_del, key) end
        local keys2 = safe_redis_keys("llm:model:*")
        for _, key in ipairs(keys2) do pcall(redis_del, key) end
        return { action = "reject", status = 200, body = '{"ok":true}' }
    end

    -- API: 导出配置到文件
    if path:match("^/api/export%-config") then
        -- 备份原文件
        local backup_file = io.open(script_dir .. "/config.lua.bak", "w")
        if backup_file then
            local original = io.open(script_dir .. "/config.lua", "r")
            if original then
                backup_file:write(original:read("*a"))
                original:close()
            end
            backup_file:close()
        end

        -- 生成新配置
        local config_content = generate_config_lua()

        -- 写入文件
        local file, err = io.open(script_dir .. "/config.lua", "w")
        if not file then
            return { action = "reject", status = 500, body = '{"ok":false,"error":"file write error: ' .. (err or "unknown") .. '"}' }
        end
        file:write(config_content)
        file:close()

        return { action = "reject", status = 200, body = '{"ok":true,"file":"config.lua"}' }
    end

    -- API: 预览配置文件内容
    if path:match("^/api/preview%-config") then
        local config_content = generate_config_lua()
        return { action = "reject", status = 200, body = config_content, headers = { ["Content-Type"] = "text/plain; charset=utf-8" } }
    end

    -- 404
    return { action = "reject", status = 404, body = '{"error":"not found"}' }
end

function handler.on_response(upstream, status, body)
end
