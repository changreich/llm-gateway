-- LLM Gateway Router 2 配置文件
--
-- Redis Key 设计:
--   Code: code:{num} -> provider|model|opt
--   Opt:  opt:{id}:{field} -> value
--   Select: code:select -> num

return {
    -- Redis 连接 (复用)
    redis_host = "127.0.0.1",
    redis_port = 7379,
    redis_db = 0,

    -- 当前选中的配置
    selected = "01",

    -- Code 配置
    code = {
        ["01"] = {
            provider = "opengo",
            model = "glm-5.1",
            opt = ""  -- 无选项
        },
        ["02"] = {
            provider = "openzen",
            model = "mimo-v2-pro-free",
            opt = ""  -- 无选项
        },
        ["03"] = {
            provider = "qfcode",
            model = "qianfan-code-latest",
            opt = ""  -- 使用 opt:01 和 opt:02
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
