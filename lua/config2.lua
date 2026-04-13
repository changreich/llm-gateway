-- LLM Gateway Router 2 配置文件
--
-- Redis Key 设计:
--   Code: code:{num} -> provider|model|opt
--   Opt:  opt:{id}:{field} -> value
--   Select: code:select -> num
--   ModelMap: modelmap:{model_name} -> num | "num1,num2,..."
--   ModelMapIdx: modelmap:{model_name}:idx -> counter (轮询索引)
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

    -- Model 名称映射 (model_name -> num | "num1,num2,...")
    -- 优先级高于 code:select
    -- 单值 "05" -> 直接使用; 多值 "05,08,09" -> 轮询
    modelmap = {
        ["default"] = "05",
        ["haru"] = "06",
        ["claude-sonnet-4-20250514"] = "04",
        ["claude-sonnet"] = "04",
        ["qianfan-code-latest"] = "05",
    },

    -- Code 配置
    code = {
        ["01"] = {
            provider = "opengo",
            model = "glm-5.1",
            opt = "",  -- 无选项
            proxy = ""  -- 空=使用上级配置
        },
		["10"] = {
            provider = "opengo",
            model = "minimax-m2.7",
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
            opt = "01",
            proxy = ""  -- 空=使用上级配置
        },
		["05"] = {
            provider = "qfacode",     -- Anthropic API (baseurl含anthropic)
            model = "glm-5",  -- 百度千帆的coding模型
            opt = "",
            proxy = ""  -- 空=使用上级配置
        },
		["09"] = {
            provider = "qfacode",     -- Anthropic API (baseurl含anthropic)
            model = "deepseek-v3.2",  -- 百度千帆的coding模型
            opt = "",
            proxy = ""  -- 空=使用上级配置
        },
		["06"] = {
            provider = "zpap",     -- Anthropic API (baseurl含anthropic)
            model = "GLM-4-Flash",  -- 百度千帆的coding模型
            opt = "",
            proxy = ""  -- 空=使用上级配置
        },
		["07"] = {
            provider = "openzen",
            model = "gpt-5.3-codex",
            opt = "",  -- 无选项
            proxy = ""  -- 空=使用上级配置
        },
		["08"] = {
            provider = "siliflowa",
            model = "Pro/zai-org/GLM-5.1",
            opt = "",  -- 无选项
            proxy = ""  -- 空=使用上级配置
        },
    },

    -- Opt 配置
    opt = {
        ["01"] = {
            max_tokens = "12280"
        },
        ["02"] = {
            stream = "true"
        },
        ["03"] = {
            max_tokens = "999",
			temperature = "0.7",
            top_p = "0.9"
        }
    }
}
