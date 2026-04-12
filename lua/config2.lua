-- LLM Gateway Router 2 配置文件
--
-- Redis Key 设计:
--   Code: code:{num} -> provider|model|opt
--   Opt:  opt:{id}:{field} -> value
--   Select: code:select -> num
--   ModelMap: modelmap:{model_name} -> num
--   Proxy: global:proxy, provider:{name}:proxy, code:{num}:proxy

return {
    -- Redis 连接 (复用)
    redis_host = "127.0.0.1",
    redis_port = 7379,
    redis_db = 0,

    -- 全局代理 (可选，所有请求默认走此代理)
    -- 格式: http://host:port 或 socks5://host:port
    -- proxy = "http://127.0.0.1:7890",

    -- 当前选中的配置 (默认)
    selected = "05",

    -- Model 名称映射 (model_name -> num)
    -- 优先级高于 code:select
    modelmap = {
        ["default"] = "05",
        ["haru"] = "02",
        ["claude-sonnet-4-20250514"] = "04",
        ["claude-sonnet"] = "04",
    },

    -- Code 配置
    code = {
        ["01"] = {
            provider = "opengo",
            model = "glm-5.1",
            opt = "",  -- 无选项
            proxy = ""  -- 空=使用上级配置
        },
        ["02"] = {
            provider = "openzen",
            model = "minimax-m2.5-free",
            opt = "",  -- 无选项
            proxy = ""  -- 空=使用上级配置
        },
        ["03"] = {
            provider = "qfcode",
            model = "qianfan-code-latest",
            opt = "",  -- 使用 opt:01 和 opt:02
            proxy = ""  -- 空=使用上级配置
        },
        ["04"] = {
            provider = "qfacode",     -- Anthropic API (baseurl含anthropic)
            model = "ernie-4.5-turbo-20260402",  -- 百度千帆的coding模型
            opt = "",
            proxy = ""  -- 空=使用上级配置
        },
		["05"] = {
            provider = "qfacode",     -- Anthropic API (baseurl含anthropic)
            model = "glm-5",  -- 百度千帆的coding模型
            opt = "",
            proxy = ""  -- 空=使用上级配置
        }
    },

    -- Opt 配置
    opt = {
        ["01"] = {
            max_tokens = "4096",
            temperature = "0.7"
        },
        ["02"] = {
            stream = "true"
        },
        ["03"] = {
            max_tokens = "999",
            top_p = "0.9"
        }
    }
}
