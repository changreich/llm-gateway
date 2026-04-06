-- Simple test router

handler = {}  -- 全局变量，不用 local

function handler.on_request(method, path, headers)
    -- Simple test: always return ok
    return {
        action = "reject",
        status = 200,
        body = '{"status":"ok","path":"' .. path .. '"}'
    }
end

function handler.on_response(upstream, status, body)
end

function handler.on_error(upstream, err)
end
