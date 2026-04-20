//! LLM Gateway - Pingora + Lua 反向代理
//!
//! 支持 embeddings/rerank/chat 等多种请求类型
//! 通过 Lua 脚本实现模型映射和路由决策
//!
//! Pingora 职责：接收请求 → 传给 Lua (header) → 执行决策 → 上报响应
//! Lua 职责：Redis 配置读取、URL 重写、Token 统计

mod anthropic_convert;
mod sse_stream;
mod tls_server;
mod ssl;
use anthropic_convert::{
    extract_error_message, extract_openai_fields, transform_anthropic_request_to_openai,
    transform_openai_to_anthropic, convert_openai_content_to_anthropic,
    convert_openai_tool_calls_to_anthropic, decompress_field, assemble_anthropic_response,
    anthropic_error_response,
};
use sse_stream::{
    new_sse_stream_state, transform_openai_sse_chunk_to_anthropic,
    generate_stream_end_events, generate_error_sse_stream, SseLineParser, inject_stream_true,
    sse_register, sse_update_openai_id, sse_unregister, sse_active_count,
};

use async_trait::async_trait;
use bytes::Bytes;
use log::{error, info, warn};
use mlua::{Lua, ObjectLike, Table};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use once_cell::sync::Lazy;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_error::{Error, ErrorType, Result};
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session};
use redis::Client;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// 从 listen 字符串解析端口号
fn parse_port(listen: &str) -> u16 {
    listen.rsplit(':').next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9090)
}

/// 模型统计数据
#[derive(Clone, Debug, Default)]
struct ModelStats {
    calls: u64,
    prompt: u64,
    completion: u64,
    last_prompt: u64,
    last_completion: u64,
}

/// 全局统计数据 (原子操作 + RwLock)
static STATS_TOTAL_CALLS: AtomicU64 = AtomicU64::new(0);
static STATS_TOTAL_PROMPT: AtomicU64 = AtomicU64::new(0);
static STATS_TOTAL_COMPLETION: AtomicU64 = AtomicU64::new(0);
static STATS_MODELS: Lazy<RwLock<HashMap<String, ModelStats>>> = Lazy::new(|| RwLock::new(HashMap::new()));
static STATS_SELECTED: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("01".to_string()));
static STATS_CONFIG: Lazy<RwLock<HashMap<String, (String, String)>>> = Lazy::new(|| RwLock::new(HashMap::new())); // num -> (provider, model);

/// Code 模型统计数据 (9089 端口)
static CODE_STATS_TOTAL_CALLS: AtomicU64 = AtomicU64::new(0);
static CODE_STATS_TOTAL_PROMPT: AtomicU64 = AtomicU64::new(0);
static CODE_STATS_TOTAL_COMPLETION: AtomicU64 = AtomicU64::new(0);
static CODE_STATS_MODELS: Lazy<RwLock<HashMap<String, ModelStats>>> = Lazy::new(|| RwLock::new(HashMap::new()));
static CODE_STATS_SELECTED: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("01".to_string()));
static CODE_STATS_CONFIG: Lazy<RwLock<HashMap<String, (String, String)>>> = Lazy::new(|| RwLock::new(HashMap::new()));

/// 端口配置 (用于统计页面显示)
static LLM_PORT: AtomicU64 = AtomicU64::new(9090);
static CODE_PORT: AtomicU64 = AtomicU64::new(9089);

/// 端口配置字符串 (从 config.lua 读取)
static LLM_LISTEN: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("0.0.0.0:9090".to_string()));
static CODE_LISTEN: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("0.0.0.0:9089".to_string()));
static ADMIN_LISTEN: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("0.0.0.0:9093".to_string()));
static STATS_LISTEN: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("0.0.0.0:9091".to_string()));

/// TLS 配置 (从 config.lua 读取)
static TLS_ENABLED: Lazy<RwLock<bool>> = Lazy::new(|| RwLock::new(false));
static TLS_CERT: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new(String::new()));
static TLS_KEY: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new(String::new()));
static TLS_LISTEN: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("0.0.0.0:443".to_string()));

/// Code 端口配置 (router2.lua 处理)
/// code_tls: TLS 端口 (443)
/// code_http: HTTP 端口 (9443)
static CODE_TLS_LISTEN: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("0.0.0.0:443".to_string()));
static CODE_HTTP_LISTEN: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("0.0.0.0:9443".to_string()));

/// 简单的 Redis 连接池
struct RedisConnPool {
    client: Client,
    pool: Mutex<Vec<redis::Connection>>,
    max_size: usize,
}

impl RedisConnPool {
    fn new(url: &str, max_size: usize) -> Self {
        let client = Client::open(url).expect("Redis client");
        Self {
            client,
            pool: Mutex::new(Vec::with_capacity(max_size)),
            max_size,
        }
    }

    fn get(&self) -> Option<RedisConnGuard> {
        let mut pool = self.pool.lock().unwrap();
        if let Some(conn) = pool.pop() {
            Some(RedisConnGuard { conn: Some(conn), pool: &self.pool })
        } else {
            self.client.get_connection().ok().map(|conn| {
                RedisConnGuard { conn: Some(conn), pool: &self.pool }
            })
        }
    }
}

/// 连接守卫，归还连接到池
struct RedisConnGuard<'a> {
    conn: Option<redis::Connection>,
    pool: &'a Mutex<Vec<redis::Connection>>,
}

impl std::ops::Deref for RedisConnGuard<'_> {
    type Target = redis::Connection;
    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for RedisConnGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn.as_mut().unwrap()
    }
}

impl Drop for RedisConnGuard<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut pool = self.pool.lock().unwrap();
            if pool.len() < 10 {  // max_size
                pool.push(conn);
            }
            // 如果池满了，连接直接 drop
        }
    }
}

/// 全局 Redis 连接池
static REDIS_POOL: Lazy<RedisConnPool> = Lazy::new(|| {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:7379".to_string());
    RedisConnPool::new(&url, 10)
});

/// 获取 Redis 连接
fn get_redis_conn() -> Option<RedisConnGuard<'static>> {
    REDIS_POOL.get()
}

/// TLS 证书验证开关 (默认跳过，因为有透明代理)
static SKIP_TLS_VERIFY: Lazy<bool> = Lazy::new(|| {
    std::env::var("LLM_TLS_VERIFY").map(|v| v != "0" && v != "false").unwrap_or(false)
});

/// 全局默认 API 配置 (预置默认值，实现开箱即用)
static DEFAULT_CONFIG: Lazy<Arc<RwLock<DefaultConfig>>> = Lazy::new(|| {
    Arc::new(RwLock::new(DefaultConfig {
        baseurl: std::env::var("LLM_BASEURL").unwrap_or_else(|_| "https://api.anthropic.com".to_string()),
        api_key: std::env::var("LLM_API_KEY").unwrap_or_else(|_| "".to_string()),
        model: std::env::var("LLM_MODEL").unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string()),
    }))
});

#[derive(Clone)]
struct DefaultConfig {
    baseurl: String,
    api_key: String,
    model: String,
}

/// Lua 运行时封装
struct LuaRuntime {
    lua: Lua,
    script_path: PathBuf,
}

impl LuaRuntime {
    fn new(script_path: PathBuf) -> Result<Self> {
        let lua = Lua::new();
        Self::register_redis_functions(&lua)?;
        Self::register_openai_functions(&lua)?;
        let rt = Self { lua, script_path };
        rt.reload()?;
        Ok(rt)
    }

    fn register_redis_functions(lua: &Lua) -> Result<()> {
        let globals = lua.globals();

        // redis_get(key) -> value | nil
        let redis_get_fn = lua.create_function(|lua, key: String| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => return Ok(None),
            };
            let value = redis::cmd("GET")
                .arg(&key)
                .query::<Option<String>>(&mut conn)
                .ok()
                .flatten();

            match value {
                Some(v) => Ok(Some(lua.create_string(&v)?)),
                None => Ok(None),
            }
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_get: {}", e)))?;
        globals.set("redis_get", redis_get_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_get: {}", e)))?;

        // redis_set(key, value) -> bool
        let redis_set_fn = lua.create_function(|_lua, (key, value): (String, String)| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => return Ok(false),
            };
            let result: bool = redis::cmd("SET")
                .arg(&key)
                .arg(&value)
                .query::<()>(&mut conn)
                .is_ok();
            Ok(result)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_set: {}", e)))?;
        globals.set("redis_set", redis_set_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_set: {}", e)))?;

        // redis_keys(pattern) -> table
        let redis_keys_fn = lua.create_function(|lua, pattern: String| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => {
                    let table = lua.create_table()?;
                    return Ok(table);
                }
            };
            let keys: Vec<String> = redis::cmd("KEYS")
                .arg(&pattern)
                .query::<Vec<String>>(&mut conn)
                .unwrap_or_default();

            let table = lua.create_table()?;
            for (i, k) in keys.into_iter().enumerate() {
                table.set(i + 1, k)?;
            }
            Ok(table)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_keys: {}", e)))?;
        globals.set("redis_keys", redis_keys_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_keys: {}", e)))?;

        // redis_incr(key) -> number
        let redis_incr_fn = lua.create_function(|_lua, key: String| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => return Ok(0i64),
            };
            let n: i64 = redis::cmd("INCR")
                .arg(&key)
                .query::<i64>(&mut conn)
                .unwrap_or(0);
            Ok(n)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_incr: {}", e)))?;
        globals.set("redis_incr", redis_incr_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_incr: {}", e)))?;

        // redis_incrby(key, amount) -> number
        let redis_incrby_fn = lua.create_function(|_lua, (key, amount): (String, i64)| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => return Ok(0i64),
            };
            let n: i64 = redis::cmd("INCRBY")
                .arg(&key)
                .arg(amount)
                .query::<i64>(&mut conn)
                .unwrap_or(0);
            Ok(n)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_incrby: {}", e)))?;
        globals.set("redis_incrby", redis_incrby_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_incrby: {}", e)))?;

        // json_decode(str) -> table
        let json_decode_fn = lua.create_function(|lua, s: String| {
            let v: serde_json::Value = serde_json::from_str(&s)
                .map_err(|e| mlua::Error::external(format!("json decode: {}", e)))?;
            json_to_lua_deep(&lua, &v)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create json_decode: {}", e)))?;
        globals.set("json_decode", json_decode_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set json_decode: {}", e)))?;

        // json_encode(table) -> string
        let json_encode_fn = lua.create_function(|_lua, val: mlua::Value| {
            let json_val = lua_to_json(&val);
            let s = serde_json::to_string(&json_val)
                .map_err(|e| mlua::Error::external(format!("json encode: {}", e)))?;
            Ok(s)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create json_encode: {}", e)))?;
        globals.set("json_encode", json_encode_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set json_encode: {}", e)))?;

        // redis_expire(key, seconds) -> bool
        let redis_expire_fn = lua.create_function(|_lua, (key, seconds): (String, i64)| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => return Ok(false),
            };
            let result: i64 = redis::cmd("EXPIRE")
                .arg(&key)
                .arg(seconds)
                .query::<i64>(&mut conn)
                .unwrap_or(0);
            Ok(result == 1)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_expire: {}", e)))?;
        globals.set("redis_expire", redis_expire_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_expire: {}", e)))?;

        // redis_del(key) -> bool
        let redis_del_fn = lua.create_function(|_lua, key: String| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => return Ok(false),
            };
            let result: i64 = redis::cmd("DEL")
                .arg(&key)
                .query::<i64>(&mut conn)
                .unwrap_or(0);
            Ok(result > 0)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_del: {}", e)))?;
        globals.set("redis_del", redis_del_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_del: {}", e)))?;

        // redis_lpush(key, value) -> bool
        let redis_lpush_fn = lua.create_function(|_lua, (key, value): (String, String)| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => return Ok(false),
            };
            let result: i64 = redis::cmd("LPUSH")
                .arg(&key)
                .arg(&value)
                .query::<i64>(&mut conn)
                .unwrap_or(0);
            Ok(result > 0)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_lpush: {}", e)))?;
        globals.set("redis_lpush", redis_lpush_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_lpush: {}", e)))?;

        // redis_ltrim(key, start, stop) -> bool
        let redis_ltrim_fn = lua.create_function(|_lua, (key, start, stop): (String, i64, i64)| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => return Ok(false),
            };
            let result: bool = redis::cmd("LTRIM")
                .arg(&key)
                .arg(start)
                .arg(stop)
                .query::<()>(&mut conn)
                .is_ok();
            Ok(result)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_ltrim: {}", e)))?;
        globals.set("redis_ltrim", redis_ltrim_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_ltrim: {}", e)))?;

        // redis_lrange(key, start, stop) -> table
        let redis_lrange_fn = lua.create_function(|lua, (key, start, stop): (String, i64, i64)| {
            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => {
                    let table = lua.create_table()?;
                    return Ok(table);
                }
            };
            let items: Vec<String> = redis::cmd("LRANGE")
                .arg(&key)
                .arg(start)
                .arg(stop)
                .query::<Vec<String>>(&mut conn)
                .unwrap_or_default();

            let table = lua.create_table()?;
            for (i, item) in items.into_iter().enumerate() {
                table.set(i + 1, item)?;
            }
            Ok(table)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create redis_lrange: {}", e)))?;
        globals.set("redis_lrange", redis_lrange_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set redis_lrange: {}", e)))?;

        // modelmap_get(model_name) -> num | nil
        // 根据 model 名称查询映射的配置编号
        let modelmap_get_fn = lua.create_function(|_lua, model_name: String| {
            if model_name.is_empty() {
                return Ok(None);
            }

            let mut conn = match get_redis_conn() {
                Some(c) => c,
                None => return Ok(None),
            };

            // 精确匹配
            let modelmap_key = format!("modelmap:{}", model_name);
            if let Ok(Some(num)) = redis::cmd("GET")
                .arg(&modelmap_key)
                .query::<Option<String>>(&mut conn)
            {
                if !num.is_empty() {
                    return Ok(Some(num));
                }
            }

            // 前缀匹配 (model-name-xxx -> model-name)
            let parts: Vec<&str> = model_name.split('-').collect();
            if parts.len() > 1 {
                // 尝试去掉最后一部分
                let prefix = parts[..parts.len()-1].join("-");
                let prefix_key = format!("modelmap:{}", prefix);
                if let Ok(Some(num)) = redis::cmd("GET")
                    .arg(&prefix_key)
                    .query::<Option<String>>(&mut conn)
                {
                    if !num.is_empty() {
                        return Ok(Some(num));
                    }
                }
            }

            Ok(None)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create modelmap_get: {}", e)))?;
        globals.set("modelmap_get", modelmap_get_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set modelmap_get: {}", e)))?;

        info!("Redis functions registered to Lua");
        Ok(())
    }

    fn register_openai_functions(lua: &Lua) -> Result<()> {
        let globals = lua.globals();

        // openai_call(request_json) -> response_json
        // 通用 LLM API 调用，支持自定义端点和请求体透传
        // 请求 JSON 可包含特殊字段：baseurl, api_key, endpoint (会被移除后发送)
        // 在独立线程中执行，避免与 tokio 冲突
        let openai_call_fn = lua.create_function(|_lua, request_json: String| {
            let config = DEFAULT_CONFIG.read().unwrap().clone();

            // 解析请求 JSON
            let mut req: serde_json::Value = match serde_json::from_str(&request_json) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(format!(r#"{{"error":"invalid json: {}"}}"#, e));
                }
            };

            // 提取特殊字段
            let api_key = req.get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or(&config.api_key)
                .to_string();
            let baseurl = req.get("baseurl")
                .and_then(|v| v.as_str())
                .unwrap_or(&config.baseurl)
                .to_string();
            let endpoint = req.get("endpoint")
                .and_then(|v| v.as_str())
                .unwrap_or("/v1/chat/completions")
                .to_string();

            // 移除特殊字段（不发送给提供商）
            if let Some(obj) = req.as_object_mut() {
                obj.remove("api_key");
                obj.remove("baseurl");
                obj.remove("endpoint");
            }

            // 构建完整 URL
            let url_parsed = format!("{}{}", baseurl.trim_end_matches('/'), endpoint);

            // 构建请求
            let mut req_builder = reqwest::blocking::Client::new()
                .post(&url_parsed)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", api_key));

            // 根据提供商添加特殊 header
            if baseurl.contains("anthropic") {
                req_builder = req_builder.header("x-api-key", &api_key);
                req_builder = req_builder.header("anthropic-version", "2023-06-01");
            }

            // 在独立线程中执行 HTTP 请求
            let result: Result<reqwest::blocking::Response, String> = std::thread::spawn(move || {
                let body_str = serde_json::to_string(&req).unwrap_or_default();
                let http_req = req_builder
                    .body(body_str)
                    .header("Content-Type", "application/json")
                    .send();
                match http_req {
                    Ok(r) => Ok(r),
                    Err(e) => Err(format!("send error: {}", e)),
                }
            }).join().map_err(|e| mlua::Error::external(format!("thread join error")))?;

            let result = match result {
                Ok(r) => r,
                Err(e) => return Ok(format!(r#"{{"error":"{}"}}"#, e)),
            };

            let status = result.status();
            let text = result.text().unwrap_or_default();

            if status.is_success() {
                Ok(text)
            } else {
                Ok(format!(r#"{{"error":{{"message":"{}","type":"api_error","code":{}}}}}"#, text, status.as_u16()))
            }
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create openai_call: {}", e)))?;
        globals.set("openai_call", openai_call_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set openai_call: {}", e)))?;

        // openai_chat(messages, model, api_key) -> response_json
        // 简化版调用接口
        let openai_chat_fn = lua.create_function(|_lua, (messages, model, api_key): (String, String, String)| {
            let config = DEFAULT_CONFIG.read().unwrap().clone();

            // messages 是 JSON 数组字符串
            let parsed_messages: Vec<serde_json::Value> = match serde_json::from_str(&messages) {
                Ok(v) => v,
                Err(_) => vec![],
            };

            let model = if model.is_empty() { &config.model } else { &model };
            let api_key = if api_key.is_empty() { &config.api_key } else { &api_key };
            let baseurl = &config.baseurl;

            let body = serde_json::json!({
                "model": model,
                "messages": parsed_messages,
                "max_tokens": 4096,
                "stream": false
            });

            let url_parsed = format!("{}/v1/chat/completions", baseurl.trim_end_matches('/'));
            let mut req_builder = reqwest::blocking::Client::new()
                .post(&url_parsed)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", api_key));

            if baseurl.contains("anthropic") {
                req_builder = req_builder.header("x-api-key", api_key);
                req_builder = req_builder.header("anthropic-version", "2023-06-01");
            }

            // 在独立线程中执行
            let result = std::thread::spawn(move || {
                // 直接构建 JSON body
                let body_str = serde_json::to_string(&body).unwrap_or_default();
                let req = req_builder
                    .body(body_str)
                    .header("Content-Type", "application/json")
                    .send();
                match req {
                    Ok(r) => Ok(r),
                    Err(e) => Err(format!("send error: {}", e)),
                }
            }).join().map_err(|e| mlua::Error::external(format!("thread join error")))?;

            let result: reqwest::blocking::Response = match result {
                Ok(r) => r,
                Err(e) => return Ok(format!(r#"{{"error":"{}"}}"#, e)),
            };

            let text = result.text().unwrap_or_default();
            Ok(text)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create openai_chat: {}", e)))?;
        globals.set("openai_chat", openai_chat_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set openai_chat: {}", e)))?;

        // get_default_config() -> {baseurl, api_key, model}
        let get_default_config_fn = lua.create_function(|_lua, _: ()| {
            let config = DEFAULT_CONFIG.read().unwrap().clone();
            let table = _lua.create_table()?;
            table.set("baseurl", config.baseurl)?;
            table.set("api_key", config.api_key)?;
            table.set("model", config.model)?;
            Ok(table)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create get_default_config: {}", e)))?;
        globals.set("get_default_config", get_default_config_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set get_default_config: {}", e)))?;

        // stats_update(calls, prompt, completion) -> 更新全局统计
        let stats_update_fn = lua.create_function(|_lua, (calls, prompt, completion): (u64, u64, u64)| {
            STATS_TOTAL_CALLS.fetch_add(calls, Ordering::Relaxed);
            STATS_TOTAL_PROMPT.fetch_add(prompt, Ordering::Relaxed);
            STATS_TOTAL_COMPLETION.fetch_add(completion, Ordering::Relaxed);
            Ok(())
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create stats_update: {}", e)))?;
        globals.set("stats_update", stats_update_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set stats_update: {}", e)))?;

        // stats_get() -> {calls, prompt, completion} 读取全局统计（无阻塞）
        let stats_get_fn = lua.create_function(|lua, _: ()| {
            let table = lua.create_table()?;
            table.set("calls", STATS_TOTAL_CALLS.load(Ordering::Relaxed))?;
            table.set("prompt", STATS_TOTAL_PROMPT.load(Ordering::Relaxed))?;
            table.set("completion", STATS_TOTAL_COMPLETION.load(Ordering::Relaxed))?;
            Ok(table)
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create stats_get: {}", e)))?;
        globals.set("stats_get", stats_get_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set stats_get: {}", e)))?;

        // stats_update_model(num, calls, prompt, completion, last_prompt, last_completion) -> 更新模型统计
        let stats_update_model_fn = lua.create_function(|_lua, (num, calls, prompt, completion, last_prompt, last_completion): (String, u64, u64, u64, u64, u64)| {
            STATS_TOTAL_CALLS.fetch_add(calls, Ordering::Relaxed);
            STATS_TOTAL_PROMPT.fetch_add(prompt, Ordering::Relaxed);
            STATS_TOTAL_COMPLETION.fetch_add(completion, Ordering::Relaxed);
            if let Ok(mut models) = STATS_MODELS.write() {
                let entry = models.entry(num).or_insert_with(ModelStats::default);
                entry.calls += calls;
                entry.prompt += prompt;
                entry.completion += completion;
                entry.last_prompt = last_prompt;
                entry.last_completion = last_completion;
            }
            Ok(())
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create stats_update_model: {}", e)))?;
        globals.set("stats_update_model", stats_update_model_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set stats_update_model: {}", e)))?;

        // stats_set_config(num, provider, model) -> 设置模型配置
        let stats_set_config_fn = lua.create_function(|_lua, (num, provider, model): (String, String, String)| {
            if let Ok(mut config) = STATS_CONFIG.write() {
                config.insert(num, (provider, model));
            }
            Ok(())
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create stats_set_config: {}", e)))?;
        globals.set("stats_set_config", stats_set_config_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set stats_set_config: {}", e)))?;

        // stats_set_selected(num) -> 设置当前选中的模型
        let stats_set_selected_fn = lua.create_function(|_lua, num: String| {
            if let Ok(mut selected) = STATS_SELECTED.write() {
                *selected = num;
            }
            Ok(())
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create stats_set_selected: {}", e)))?;
        globals.set("stats_set_selected", stats_set_selected_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set stats_set_selected: {}", e)))?;

        // ============================================================
        // Code 统计函数 (9089 端口)
        // ============================================================

        // stats_code_add(num, calls, prompt, completion) -> 累加统计
        let stats_code_add_fn = lua.create_function(|_lua, (num, calls, prompt, completion): (String, u64, u64, u64)| {
            CODE_STATS_TOTAL_CALLS.fetch_add(calls, Ordering::Relaxed);
            CODE_STATS_TOTAL_PROMPT.fetch_add(prompt, Ordering::Relaxed);
            CODE_STATS_TOTAL_COMPLETION.fetch_add(completion, Ordering::Relaxed);
            if let Ok(mut models) = CODE_STATS_MODELS.write() {
                let entry = models.entry(num).or_insert_with(ModelStats::default);
                entry.calls += calls;
                entry.prompt += prompt;
                entry.completion += completion;
                entry.last_prompt = prompt;
                entry.last_completion = completion;
            }
            Ok(())
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create stats_code_add: {}", e)))?;
        globals.set("stats_code_add", stats_code_add_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set stats_code_add: {}", e)))?;

        // stats_code_set_config(num, provider, model) -> 设置 Code 模型配置
        let stats_code_set_config_fn = lua.create_function(|_lua, (num, provider, model): (String, String, String)| {
            if let Ok(mut config) = CODE_STATS_CONFIG.write() {
                config.insert(num, (provider, model));
            }
            Ok(())
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create stats_code_set_config: {}", e)))?;
        globals.set("stats_code_set_config", stats_code_set_config_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set stats_code_set_config: {}", e)))?;

        // stats_code_set_selected(num) -> 设置当前选中的 Code 模型
        let stats_code_set_selected_fn = lua.create_function(|_lua, num: String| {
            if let Ok(mut selected) = CODE_STATS_SELECTED.write() {
                *selected = num;
            }
            Ok(())
        }).map_err(|e| Error::explain(ErrorType::InternalError, format!("create stats_code_set_selected: {}", e)))?;
        globals.set("stats_code_set_selected", stats_code_set_selected_fn).map_err(|e| Error::explain(ErrorType::InternalError, format!("set stats_code_set_selected: {}", e)))?;

        info!("OpenAI functions registered to Lua");
        Ok(())
    }

    fn reload(&self) -> Result<()> {
        let script = std::fs::read_to_string(&self.script_path)
            .map_err(|e| Error::because(ErrorType::InternalError, "read script", e))?;

        // 设置 script_dir 全局变量 (用于 SDK 加载)
        let script_dir = self.script_path.parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        self.lua.globals().set("script_dir", script_dir.clone())
            .map_err(|e| Error::explain(ErrorType::InternalError, format!("set script_dir: {}", e)))?;

        self.lua.load(&script).exec().map_err(|e| {
            Error::because(ErrorType::InternalError, "Lua script error", e.to_string())
        })?;
        info!("Lua script loaded: {:?}", self.script_path);
        Ok(())
    }

    fn call_on_request(
        &self,
        method: &str,
        path: &str,
        headers: HashMap<String, String>,
        body: &[u8],
    ) -> Result<RequestDecision> {
        // 获取 handler 表
        let handler: Table = self.lua.globals().get("handler").map_err(|e| {
            Error::explain(ErrorType::InternalError, format!("handler table not found: {}", e))
        })?;

        // 获取 on_request 函数
        let on_request: mlua::Function = handler.get("on_request").map_err(|e| {
            Error::explain(ErrorType::InternalError, format!("on_request function not found: {}", e))
        })?;

        let headers_table = self.lua.create_table().map_err(|e| {
            Error::explain(ErrorType::InternalError, format!("create table: {}", e))
        })?;
        for (k, v) in headers {
            headers_table.set(k, v).map_err(|e| {
                Error::explain(ErrorType::InternalError, format!("set header: {}", e))
            })?;
        }

        let body_str = String::from_utf8_lossy(body).to_string();

        // 调用函数: on_request(method, path, headers, body)
        let result: Table = on_request
            .call((method, path, headers_table, body_str))
            .map_err(|e| {
                Error::explain(ErrorType::InternalError, format!("on_request call failed: {}", e))
            })?;

        Ok(RequestDecision {
            action: result.get("action").unwrap_or_else(|_| "proxy".to_string()),
            upstream: result.get("upstream").unwrap_or_default(),
            status: result.get("status").unwrap_or(200),
            addr: result.get("addr").unwrap_or_default(),
            tls: result.get("tls").unwrap_or(false),
            sni: result.get("sni").unwrap_or_default(),
            api_key: result.get("api_key").unwrap_or_default(),
            model: result.get("model").unwrap_or_default(),
            rewrite_path: result.get("rewrite_path").unwrap_or_default(),
            response_body: result.get("body").unwrap_or_default(),
            new_request_body: result.get("new_request_body").unwrap_or_default(),
            need_transform: result.get("need_transform").unwrap_or(true),  // 默认需要转换
            proxy: result.get("proxy").ok().flatten(),  // 代理URL，None=直连
        })
    }

    fn call_on_response(&self, upstream: &str, status: u16, body: &[u8]) {
        info!("call_on_response called: upstream={}, status={}", upstream, status);
        if let Ok(handler) = self.lua.globals().get::<Table>("handler") {
            match handler.get::<mlua::Function>("on_response") {
                Ok(on_response) => {
                    let body_str = String::from_utf8_lossy(body).to_string();
                    match on_response.call::<()>((upstream, status, body_str)) {
                        Ok(_) => info!("on_response completed successfully"),
                        Err(e) => error!("on_response call failed: {}", e),
                    }
                }
                Err(e) => error!("on_response function not found: {}", e),
            }
        } else {
            error!("handler table not found");
        }
    }

    fn call_on_error(&self, upstream: &str, error_msg: &str) {
        if let Ok(handler) = self.lua.globals().get::<Table>("handler") {
            let _ = handler.call_method::<()>("on_error", (upstream, error_msg));
        }
    }

    /// 调用 Lua 的 on_transform_response，完成简单字段映射
    /// 返回 (id, model, stop_reason, input_tokens, output_tokens, compressed_content, compressed_tool_calls)
    fn call_on_transform_response(
        &self,
        id: &str,
        model: &str,
        finish_reason: &str,
        input_tokens: u64,
        output_tokens: u64,
        compressed_content: Option<&str>,
        compressed_tool_calls: Option<&str>,
    ) -> Option<TransformResult> {
        if let Ok(handler) = self.lua.globals().get::<Table>("handler") {
            match handler.get::<mlua::Function>("on_transform_response") {
                Ok(func) => {
                    let comp_content = compressed_content.unwrap_or("");
                    let comp_tc = compressed_tool_calls.unwrap_or("");
                    match func.call::<Table>((id, model, finish_reason, input_tokens, output_tokens, comp_content, comp_tc)) {
                        Ok(table) => {
                            let result_id: String = table.get("id").unwrap_or_else(|_| id.to_string());
                            let result_model: String = table.get("model").unwrap_or_else(|_| model.to_string());
                            let result_stop_reason: String = table.get("stop_reason").unwrap_or_else(|_| "end_turn".to_string());
                            let result_input_tokens: u64 = table.get("input_tokens").unwrap_or(input_tokens);
                            let result_output_tokens: u64 = table.get("output_tokens").unwrap_or(output_tokens);
                            let result_compressed_content: String = table.get("compressed_content").unwrap_or_default();
                            let result_compressed_tool_calls: String = table.get("compressed_tool_calls").unwrap_or_default();

                            Some(TransformResult {
                                id: result_id,
                                model: result_model,
                                stop_reason: result_stop_reason,
                                input_tokens: result_input_tokens,
                                output_tokens: result_output_tokens,
                                compressed_content: if result_compressed_content.is_empty() { None } else { Some(result_compressed_content) },
                                compressed_tool_calls: if result_compressed_tool_calls.is_empty() { None } else { Some(result_compressed_tool_calls) },
                            })
                        }
                        Err(e) => {
                            error!("on_transform_response call failed: {}", e);
                            None
                        }
                    }
                }
                Err(e) => {
                    error!("on_transform_response function not found: {}", e);
                    None
                }
            }
        } else {
            error!("handler table not found");
            None
        }
    }
}

/// JSON Value 转 Lua Table (深度递归版，支持嵌套对象和数组)
fn json_to_lua_deep(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Table> {
    match v {
        serde_json::Value::Object(map) => {
            let table = lua.create_table()?;
            for (k, val) in map {
                match val {
                    serde_json::Value::String(s) => table.set(k.clone(), s.clone())?,
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            table.set(k.clone(), i)?;
                        } else if let Some(f) = n.as_f64() {
                            table.set(k.clone(), f)?;
                        }
                    }
                    serde_json::Value::Bool(b) => table.set(k.clone(), *b)?,
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        let nested = json_to_lua_deep(lua, val)?;
                        table.set(k.clone(), nested)?;
                    }
                    serde_json::Value::Null => { /* skip null */ }
                }
            }
            Ok(table)
        }
        serde_json::Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, val) in arr.iter().enumerate() {
                match val {
                    serde_json::Value::String(s) => table.set(i + 1, s.clone())?,
                    serde_json::Value::Number(n) => {
                        if let Some(i_val) = n.as_i64() {
                            table.set(i + 1, i_val)?;
                        } else if let Some(f) = n.as_f64() {
                            table.set(i + 1, f)?;
                        }
                    }
                    serde_json::Value::Bool(b) => table.set(i + 1, *b)?,
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        let nested = json_to_lua_deep(lua, val)?;
                        table.set(i + 1, nested)?;
                    }
                    serde_json::Value::Null => { /* skip null */ }
                }
            }
            Ok(table)
        }
        _ => {
            let table = lua.create_table()?;
            Ok(table)
        }
    }
}

/// Lua Value 转 JSON Value
fn lua_to_json(val: &mlua::Value) -> serde_json::Value {
    match val {
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::Value::Number((*i).into()),
        mlua::Value::Number(f) => {
            if let Some(n) = serde_json::Number::from_f64(*f) {
                serde_json::Value::Number(n)
            } else {
                serde_json::Value::Null
            }
        }
        mlua::Value::String(s) => serde_json::Value::String(s.to_str().map(|v| v.to_string()).unwrap_or_default()),
        mlua::Value::Table(t) => {
            // 判断是数组还是对象：如果从1开始有连续整数key，优先当数组
            let mut max_idx = 0i64;
            let mut has_string_key = false;
            for pair in t.pairs::<mlua::Value, mlua::Value>() {
                if let Ok((k, _)) = pair {
                    if let mlua::Value::Integer(i) = k {
                        if i > max_idx { max_idx = i; }
                    } else {
                        has_string_key = true;
                    }
                }
            }

            if max_idx > 0 && !has_string_key {
                // 数组
                let mut arr = Vec::new();
                for i in 1..=max_idx {
                    if let Ok(v) = t.get::<mlua::Value>(i) {
                        arr.push(lua_to_json(&v));
                    }
                }
                serde_json::Value::Array(arr)
            } else {
                // 对象（也可能混合了数字和字符串key）
                let mut map = serde_json::Map::new();
                for pair in t.pairs::<mlua::Value, mlua::Value>() {
                    if let Ok((k, v)) = pair {
                        let key_str = match k {
                            mlua::Value::String(s) => s.to_str().map(|v| v.to_string()).unwrap_or_default(),
                            mlua::Value::Integer(i) => i.to_string(),
                            _ => continue,
                        };
                        map.insert(key_str, lua_to_json(&v));
                    }
                }
                serde_json::Value::Object(map)
            }
        }
        _ => serde_json::Value::Null,
    }
}

/// 生成 /running 页面 HTML（从 Rust 全局缓存读取，无阻塞）
fn generate_running_html() -> String {
    // 读取端口配置
    let llm_port = LLM_PORT.load(Ordering::Relaxed) as u16;
    let code_port = CODE_PORT.load(Ordering::Relaxed) as u16;

    // LLM 统计
    let llm_calls = STATS_TOTAL_CALLS.load(Ordering::Relaxed);
    let llm_prompt = STATS_TOTAL_PROMPT.load(Ordering::Relaxed);
    let llm_completion = STATS_TOTAL_COMPLETION.load(Ordering::Relaxed);

    let llm_selected = STATS_SELECTED.read().map(|s| s.clone()).unwrap_or_else(|_| "01".to_string());
    let llm_config = STATS_CONFIG.read().map(|c| c.clone()).unwrap_or_else(|_| HashMap::new());
    let llm_models = STATS_MODELS.read().map(|m| m.clone()).unwrap_or_else(|_| HashMap::new());

    let mut llm_rows = String::new();
    let mut nums: Vec<String> = llm_config.keys().cloned().collect();
    nums.sort();

    for num in &nums {
        let (provider, model) = llm_config.get(num).cloned().unwrap_or_else(|| ("?".to_string(), "?".to_string()));
        let stats = llm_models.get(num).cloned().unwrap_or_default();
        let is_selected = num == &llm_selected;
        let sel_class = if is_selected { " class=\"selected\"" } else { "" };
        let sel_mark = if is_selected { " *" } else { "" };
        let last = if stats.last_prompt > 0 {
            format!("<span class=\"prompt\">{}</span>+<span class=\"completion\">{}</span>", stats.last_prompt, stats.last_completion)
        } else {
            "-".to_string()
        };

        llm_rows.push_str(&format!(
            "<tr{}><td><span class=\"num\">{}</span>{}</td><td><span class=\"provider\">{}</span></td><td style=\"font-size:0.8em;color:#666\">{}</td><td>{}</td><td class=\"prompt\">{}</td><td class=\"completion\">{}</td><td style=\"font-size:0.8em;color:#888\">{}</td></tr>",
            sel_class, num, sel_mark, provider, model, stats.calls, stats.prompt, stats.completion, last
        ));
    }
    if llm_rows.is_empty() {
        llm_rows = "<tr><td colspan=\"7\" style=\"color:#999;text-align:center\">暂无数据</td></tr>".to_string();
    }

    // Code 统计
    let code_calls = CODE_STATS_TOTAL_CALLS.load(Ordering::Relaxed);
    let code_prompt = CODE_STATS_TOTAL_PROMPT.load(Ordering::Relaxed);
    let code_completion = CODE_STATS_TOTAL_COMPLETION.load(Ordering::Relaxed);

    let code_selected = CODE_STATS_SELECTED.read().map(|s| s.clone()).unwrap_or_else(|_| "01".to_string());
    let code_config = CODE_STATS_CONFIG.read().map(|c| c.clone()).unwrap_or_else(|_| HashMap::new());
    let code_models = CODE_STATS_MODELS.read().map(|m| m.clone()).unwrap_or_else(|_| HashMap::new());

    let mut code_rows = String::new();
    let mut code_nums: Vec<String> = code_config.keys().cloned().collect();
    code_nums.sort();

    for num in &code_nums {
        let (provider, model) = code_config.get(num).cloned().unwrap_or_else(|| ("?".to_string(), "?".to_string()));
        let stats = code_models.get(num).cloned().unwrap_or_default();
        let is_selected = num == &code_selected;
        let sel_class = if is_selected { " class=\"selected\"" } else { "" };
        let sel_mark = if is_selected { " *" } else { "" };
        let last = if stats.last_prompt > 0 {
            format!("<span class=\"prompt\">{}</span>+<span class=\"completion\">{}</span>", stats.last_prompt, stats.last_completion)
        } else {
            "-".to_string()
        };

        code_rows.push_str(&format!(
            "<tr{}><td><span class=\"num\">{}</span>{}</td><td><span class=\"provider\">{}</span></td><td style=\"font-size:0.8em;color:#666\">{}</td><td>{}</td><td class=\"prompt\">{}</td><td class=\"completion\">{}</td><td style=\"font-size:0.8em;color:#888\">{}</td></tr>",
            sel_class, num, sel_mark, provider, model, stats.calls, stats.prompt, stats.completion, last
        ));
    }
    if code_rows.is_empty() {
        code_rows = "<tr><td colspan=\"7\" style=\"color:#999;text-align:center\">暂无数据</td></tr>".to_string();
    }

    format!(r#"<!DOCTYPE html>
<html><head><meta charset="UTF-8"><title>LLM Gateway</title>
<style>
:root{{--bg:#f5f5f5;--card:#fff;--text:#333;--text2:#666;--border:#eee}}
.dark{{--bg:#1a1a2e;--card:#16213e;--text:#e8e8e8;--text2:#a0a0a0;--border:#2a2a4a}}
body{{font-family:sans-serif;max-width:1000px;margin:40px auto;padding:20px;background:var(--bg);color:var(--text);transition:background .3s,color .3s}}
h1{{color:var(--text);border-bottom:2px solid #4CAF50;padding-bottom:10px}}
h2{{color:var(--text);margin-top:25px;font-size:1.1em}}
.card{{background:var(--card);border-radius:8px;padding:20px;margin:15px 0;box-shadow:0 2px 4px rgba(0,0,0,0.1);transition:background .3s}}
.stat-box{{display:inline-block;background:linear-gradient(135deg,#667eea,#764ba2);color:white;padding:10px 16px;border-radius:8px;margin:4px;text-align:center;font-size:0.9em}}
.stat-box .v{{font-size:1.3em;font-weight:bold}}
.stat-box .l{{font-size:0.7em;opacity:0.9}}
.stat-box.green{{background:linear-gradient(135deg,#11998e,#38ef7d)}}
.stat-box.orange{{background:linear-gradient(135deg,#f093fb,#f5576c)}}
.stat-box.blue{{background:linear-gradient(135deg,#4facfe,#00f2fe)}}
.port-label{{font-size:0.75em;color:var(--text2);background:rgba(0,0,0,0.05);padding:2px 6px;border-radius:4px;margin-left:8px}}
.dark .port-label{{background:rgba(255,255,255,0.1)}}
table{{width:100%;border-collapse:collapse;font-size:0.9em}}
th,td{{text-align:left;padding:8px;border-bottom:1px solid var(--border)}}
th{{background:rgba(0,0,0,0.02);font-weight:600}}
.dark th{{background:rgba(255,255,255,0.03)}}
.num{{font-family:monospace;background:#e3f2fd;padding:2px 6px;border-radius:4px;color:#1976d2}}
.dark .num{{background:#1e3a5f}}
.provider{{color:#1976d2;font-weight:500}}
.selected{{background:rgba(255,152,0,0.15)}}
.prompt{{color:#4CAF50}}
.completion{{color:#2196F3}}
.section-header{{display:flex;align-items:center;margin-bottom:10px}}
.theme-toggle{{position:fixed;top:15px;right:20px;display:flex;align-items:center;gap:8px;font-size:0.85em;z-index:100}}
.theme-toggle input{{width:40px;height:20px;appearance:none;background:#ccc;border-radius:10px;cursor:pointer;position:relative}}
.theme-toggle input::before{{content:'';position:absolute;left:2px;top:2px;width:16px;height:16px;background:#fff;border-radius:50%;transition:transform .3s}}
.theme-toggle input:checked{{background:#1890ff}}
.theme-toggle input:checked::before{{transform:translateX(20px)}}
a{{color:#1890ff}}
</style>
<script>
function toggleTheme(){{document.documentElement.classList.toggle('dark');localStorage.setItem('theme',document.documentElement.classList.contains('dark')?'dark':'light')}}
(function(){{if(localStorage.getItem('theme')==='dark')document.documentElement.classList.add('dark')}})();
</script>
</head><body>
<div class="theme-toggle"><span>☀️</span><input type="checkbox" onchange="toggleTheme()"><span>🌙</span></div>
<h1>LLM Gateway</h1>
<div class="card">
<div class="section-header"><h2>LLM 模型统计</h2><span class="port-label">端口 {}</span></div>
<div class="stat-box"><div class="v">{}</div><div class="l">调用</div></div>
<div class="stat-box green"><div class="v">{}</div><div class="l">Prompt</div></div>
<div class="stat-box orange"><div class="v">{}</div><div class="l">Completion</div></div>
<table>
<tr><th>编号</th><th>提供商</th><th>模型</th><th>调用</th><th class="prompt">Prompt</th><th class="completion">Completion</th><th>最近</th></tr>
{}</table>
</div>
<div class="card">
<div class="section-header"><h2>Code 模型统计</h2><span class="port-label">端口 {}</span></div>
<div class="stat-box blue"><div class="v">{}</div><div class="l">调用</div></div>
<div class="stat-box green"><div class="v">{}</div><div class="l">Prompt</div></div>
<div class="stat-box orange"><div class="v">{}</div><div class="l">Completion</div></div>
<table>
<tr><th>编号</th><th>提供商</th><th>模型</th><th>调用</th><th class="prompt">Prompt</th><th class="completion">Completion</th><th>最近</th></tr>
{}</table>
</div>
<p style="color:#999;text-align:center;margin-top:20px;font-size:0.85em">LLM Gateway | <a href="/debug">debug</a> | <a href="/config">config</a> | <a href="/raw">raw</a></p>
</body></html>"#, llm_port, llm_calls, llm_prompt, llm_completion, llm_rows, code_port, code_calls, code_prompt, code_completion, code_rows)
}

#[derive(Clone, Default, Debug)]
struct TransformResult {
    id: String,
    model: String,
    stop_reason: String,
    input_tokens: u64,
    output_tokens: u64,
    compressed_content: Option<String>,
    compressed_tool_calls: Option<String>,
}

#[derive(Clone, Debug)]
struct RequestDecision {
    action: String,
    upstream: String,
    status: u16,
    addr: String,
    tls: bool,
    sni: String,
    api_key: String,
    model: String,
    rewrite_path: String,
    response_body: String,
    new_request_body: String,  // Lua 返回的新请求体
    need_transform: bool,      // 是否需要格式转换 (Anthropic provider 直通时为 false)
    proxy: Option<String>,     // 代理URL (None=直连)
}

impl Default for RequestDecision {
    fn default() -> Self {
        Self {
            action: "proxy".to_string(),
            upstream: String::new(),
            status: 200,
            addr: String::new(),
            tls: false,
            sni: String::new(),
            api_key: String::new(),
            model: String::new(),
            rewrite_path: String::new(),
            response_body: String::new(),
            new_request_body: String::new(),
            need_transform: true,  // 默认需要转换
            proxy: None,  // 默认无代理，直连
        }
    }
}

struct GatewayCtx {
    decision: RequestDecision,
    request_body: Vec<u8>,
    response_status: u16,
}

impl Default for GatewayCtx {
    fn default() -> Self {
        Self {
            decision: RequestDecision::default(),
            request_body: Vec::new(),
            response_status: 0,
        }
    }
}

struct LuaGateway {
    lua: Arc<RwLock<LuaRuntime>>,
    port: u16,
    transform: bool,  // 是否执行 A→OpenAI 转换
}

impl LuaGateway {
    fn new(lua: Arc<RwLock<LuaRuntime>>, port: u16, transform: bool) -> Self {
        Self { lua, port, transform }
    }
}

#[async_trait]
impl ProxyHttp for LuaGateway {
    type CTX = GatewayCtx;

    fn new_ctx(&self) -> Self::CTX {
        GatewayCtx::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let method = session.req_header().method.as_str().to_string();
        // 获取完整路径（包含查询参数）
        let path = session.req_header().uri.path_and_query()
            .map(|pq| pq.to_string())
            .unwrap_or_else(|| session.req_header().uri.path().to_string());
        info!("Processing: {} {}", method, path);

        // /running 端点：直接在 Rust 中处理，完全绕过 Lua（避免被其他请求阻塞）
        if path == "/running" {
            let html = generate_running_html();
            session.respond_error_with_body(200, Bytes::from(html)).await?;
            return Ok(true);
        }

        // /sse 端点：查看 SSE 连接状态
        if path == "/sse" {
            let connections = sse_stream::sse_get_active_connections();
            let conn_json: Vec<serde_json::Value> = connections.iter().map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "client_request_id": c.client_request_id,
                    "openai_sse_id": c.openai_sse_id,
                    "model": c.model,
                    "created_at": c.created_at,
                    "finished_at": c.finished_at,
                })
            }).collect();
            let json = serde_json::json!({
                "active_count": sse_stream::sse_active_count(),
                "connections": conn_json,
            });
            let body = serde_json::to_string_pretty(&json).unwrap_or_default();
            session.respond_error_with_body(200, Bytes::from(body)).await?;
            return Ok(true);
        }

        // 读取请求体 (POST/PUT 等方法) - 循环读取完整 body
        let body = if method == "POST" || method == "PUT" || method == "PATCH" {
            let mut full_body = Vec::new();
            loop {
                match session.read_request_body().await {
                    Ok(Some(chunk)) => {
                        full_body.extend_from_slice(&chunk);
                    }
                    Ok(None) => break, // 读取完成
                    Err(_) => break,
                }
            }
            full_body
        } else {
            Vec::new()
        };
        info!("Request body length: {} bytes", body.len());
        ctx.request_body = body.clone();

        let decision = {
            let lua = self.lua.read().map_err(|_| {
                Error::explain(ErrorType::InternalError, "Lua runtime lock poisoned")
            })?;
            let headers = extract_headers(session);
            lua.call_on_request(&method, &path, headers, &body)?
        };

        match decision.action.as_str() {
            "reject" => {
                info!("[port:{}] Action: reject", self.port);
                let body = Bytes::from(decision.response_body.clone());
                session.respond_error_with_body(decision.status, body).await?;
                return Ok(true);
            }
"proxy" => {
                info!("[port:{}] Action: proxy, upstream: {}, addr: {}", self.port, decision.upstream, decision.addr);
                ctx.decision = decision.clone();

                let upstream_url = if decision.tls {
                    format!("https://{}{}", decision.addr, decision.rewrite_path)
                } else {
                    format!("http://{}{}", decision.addr, decision.rewrite_path)
                };

                info!("Proxying to: {} (tls={})", upstream_url, decision.tls);

                // 解析原始请求参数
                let stream_requested = serde_json::from_slice::<serde_json::Value>(&ctx.request_body)
                    .ok()
                    .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
                    .unwrap_or(false);

                let original_model = serde_json::from_slice::<serde_json::Value>(&ctx.request_body)
                    .ok()
                    .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()));

                // 目标模型优先级：Lua decision.model > 原始请求 model
                let target_model_for_upstream = if !decision.model.is_empty() {
                    decision.model.clone()
                } else {
                    original_model.clone().unwrap_or_default()
                };

                // 构建请求体：9089 端口根据 need_transform 决定是否转换
                info!("[port:{}] need_transform={}, upstream={}", self.port, decision.need_transform, decision.upstream);
                let request_body = if !decision.new_request_body.is_empty() {
                    if self.transform && stream_requested {
                        if decision.need_transform {
                            // A→OpenAI 转换：注入 stream:true
                            match inject_stream_true(&decision.new_request_body) {
                                Some(body) => body,
                                None => decision.new_request_body.clone(),
                            }
                        } else {
                            // A→A 直通：Anthropic 格式已含 stream 字段
                            decision.new_request_body.clone()
                        }
                    } else {
                        decision.new_request_body.clone()
                    }
                } else if self.transform {
                    if decision.need_transform {
                        // 无 Lua 返回体 + 需要转换：Rust 层兜底转换
                        match transform_anthropic_request_to_openai(
                            &String::from_utf8_lossy(&body),
                            &target_model_for_upstream,
                        ) {
                            Some(converted) => {
                                if stream_requested {
                                    inject_stream_true(&converted).unwrap_or(converted)
                                } else {
                                    converted
                                }
                            }
                            None => {
                                warn!("Rust-level Anthropic→OpenAI conversion failed, using raw body");
                                String::from_utf8_lossy(&body).to_string()
                            }
                        }
                    } else {
                        // 无 Lua 返回体 + 不需要转换：直接透传
                        String::from_utf8_lossy(&body).to_string()
                    }
                } else {
                    String::from_utf8_lossy(&body).to_string()
                };

                // 创建 reqwest 客户端，配置代理
                let mut client_builder = reqwest::Client::builder()
                    .danger_accept_invalid_certs(!*SKIP_TLS_VERIFY || !decision.tls);

                // 配置代理
                if let Some(proxy_url) = &decision.proxy {
                    match reqwest::Proxy::all(proxy_url) {
                        Ok(proxy) => {
                            client_builder = client_builder.proxy(proxy);
                            info!("[port:{}] Using proxy: {}", self.port, proxy_url);
                        }
                        Err(e) => {
                            warn!("[port:{}] Invalid proxy URL {}: {}", self.port, proxy_url, e);
                        }
                    }
                } else {
                    info!("[port:{}] No proxy configured, direct connection", self.port);
                }

                let client = client_builder.build()
                    .map_err(|e| Error::explain(ErrorType::InternalError, format!("create client: {}", e)))?;

                let mut req = client.post(&upstream_url)
                    .header("Content-Type", "application/json")
                    .body(request_body);

                if !decision.api_key.is_empty() {
                    req = req.header("Authorization", format!("Bearer {}", decision.api_key));
                }

                let host_only = decision.addr.split('/').next().unwrap_or(&decision.addr);
                req = req.header("Host", host_only);

                // ============================================================
                // 9089 端口 + 流式请求：根据 need_transform 决定是否转换
                // ============================================================
                if self.transform && stream_requested && decision.need_transform {
                    // A→OpenAI 流式转换路径
                    // 注册 SSE 连接
                    let client_req_id = serde_json::from_slice::<serde_json::Value>(&ctx.request_body)
                        .ok()
                        .and_then(|v| {
                            // Anthropic 客户端可能发送 id，用于追踪
                            v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())
                        })
                        .unwrap_or_default();
                    let conn_id = sse_register(
                        client_req_id,
                        String::new(), // openai_sse_id 在首个 chunk 时更新
                        target_model_for_upstream.clone(),
                    );

                    let response = req.send().await
                        .map_err(|e| {
                            sse_unregister(conn_id);
                            Error::explain(ErrorType::InternalError, format!("upstream error: {}", e))
                        })?;

                    let status = response.status();

                    if !status.is_success() {
                        // 上游错误：包装为 Anthropic 错误 SSE 流
                        let error_body = response.text().await.unwrap_or_default();
                        let model_str = original_model.as_deref().unwrap_or("unknown");
                        let error_msg = extract_error_message(&error_body);
                        info!("[SSE-REG#{}] Upstream error for streaming request: status={}, msg={}", conn_id, status, &error_msg[..error_msg.len().min(200)]);
                        let error_sse = generate_error_sse_stream(&error_msg, model_str);

                        let mut resp = ResponseHeader::build(200, None)?;
                        resp.insert_header("Content-Type", "text/event-stream")?;
                        resp.insert_header("Cache-Control", "no-cache")?;
                        resp.insert_header("Connection", "keep-alive")?;
                        session.write_response_header(Box::new(resp), false).await?;
                        session.write_response_body(Some(Bytes::from(error_sse)), true).await?;

                        ctx.response_status = status.as_u16();
                        if let Ok(lua) = self.lua.read() {
                            lua.call_on_response(&decision.upstream, status.as_u16(), error_body.as_bytes());
                        }
                        sse_unregister(conn_id);
                        return Ok(true);
                    }

                    // 上游成功，开始真流式转换
                    info!(
                        "[SSE-REG#{}] Starting SSE stream transformation (client_model={}, upstream_model={}, active={})",
                        conn_id,
                        original_model.as_deref().unwrap_or("unknown"),
                        target_model_for_upstream,
                        sse_active_count()
                    );

                    let mut resp = ResponseHeader::build(200, None)?;
                    resp.insert_header("Content-Type", "text/event-stream")?;
                    resp.insert_header("Cache-Control", "no-cache")?;
                    resp.insert_header("Connection", "keep-alive")?;
                    session.write_response_header(Box::new(resp), false).await?;

                    let mut state = new_sse_stream_state(
                        original_model
                            .clone()
                            .unwrap_or_else(|| target_model_for_upstream.clone()),
                    );
                    let mut sse_parser = SseLineParser::new();
                    let mut stream_done = false;

                    let mut stream_response = response;
                    while let Some(chunk) = stream_response.chunk().await.map_err(|e| {
                        sse_unregister(conn_id);
                        Error::explain(ErrorType::InternalError, format!("chunk read error: {}", e))
                    })? {
                        sse_parser.push_data(&chunk);

                        let lines = sse_parser.extract_lines();
                        for line in lines {
                            if line == "[DONE]" {
                                // 流结束，确保发送结束事件
                                if !stream_done {
                                    stream_done = true;
                                    if state.started && !state.msg_id.is_empty() {
                                        // 如果已经开始了但没收到 finish_reason，补发结束事件
                                        if !state.in_content_block && state.content_block_index > 0 {
                                            let end_events = generate_stream_end_events(&mut state, "end_turn", 0);
                                            for event in end_events {
                                                session.write_response_body(Some(Bytes::from(event)), false).await?;
                                            }
                                        }
                                    }
                                }
                                continue;
                            }

                            let events = transform_openai_sse_chunk_to_anthropic(&line, &mut state);
                            // 首个 chunk 时更新连接的 openai_sse_id
                            if !state.msg_id.is_empty() {
                                sse_update_openai_id(conn_id, &state.msg_id);
                            }
                            for event in events {
                                if let Err(e) = session.write_response_body(Some(Bytes::from(event)), false).await {
                                    warn!("[SSE-REG#{}] Failed to write SSE event to client: {}", conn_id, e);
                                }
                            }
                        }
                    }

                    // 流结束安全保障：如果消息已开始但未正常结束，补发结束事件
                    if state.started && !stream_done {
                        if state.in_content_block || state.content_block_index > 0 {
                            let end_events = generate_stream_end_events(&mut state, "end_turn", 0);
                            for event in end_events {
                                let _ = session.write_response_body(Some(Bytes::from(event)), false).await;
                            }
                        }
                    }

                    // 关闭响应流
                    session.write_response_body(None, true).await?;

                    info!("[SSE-REG#{}] SSE stream transformation completed (openai_id={}, tokens: {}+{})",
                        conn_id, state.msg_id, state.input_tokens, state.output_tokens);

                    ctx.response_status = 200;

                    // ★ SSE 流结束时上报 token 统计
                    if state.input_tokens > 0 || state.output_tokens > 0 {
                        let selected = CODE_STATS_SELECTED.read().map(|s| s.clone()).unwrap_or_else(|_| "01".to_string());
                        CODE_STATS_TOTAL_CALLS.fetch_add(1, Ordering::Relaxed);
                        CODE_STATS_TOTAL_PROMPT.fetch_add(state.input_tokens, Ordering::Relaxed);
                        CODE_STATS_TOTAL_COMPLETION.fetch_add(state.output_tokens, Ordering::Relaxed);
                        if let Ok(mut models) = CODE_STATS_MODELS.write() {
                            let entry = models.entry(selected.clone()).or_insert_with(ModelStats::default);
                            entry.calls += 1;
                            entry.prompt += state.input_tokens;
                            entry.completion += state.output_tokens;
                            entry.last_prompt = state.input_tokens;
                            entry.last_completion = state.output_tokens;
                        }
                        info!("[SSE-REG#{}] Stats updated: num={}, prompt={}, completion={}",
                            conn_id, selected, state.input_tokens, state.output_tokens);
                    }

                    sse_unregister(conn_id);
                    return Ok(true);
                }

                // ============================================================
                // 9089 端口 + 流式请求 + 不需要转换：直接透传 SSE 流
                // ============================================================
                if self.transform && stream_requested && !decision.need_transform {
                    info!("[port:{}] A→A passthrough SSE stream", self.port);

                    let response = req.send().await
                        .map_err(|e| Error::explain(ErrorType::InternalError, format!("upstream error: {}", e)))?;
                    let status = response.status();

                    if !status.is_success() {
                        let error_body = response.text().await.unwrap_or_default();
                        let mut resp = ResponseHeader::build(status.as_u16(), None)?;
                        resp.insert_header("Content-Type", "application/json")?;
                        session.write_response_header(Box::new(resp), false).await?;
                        session.write_response_body(Some(Bytes::from(error_body)), true).await?;
                        ctx.response_status = status.as_u16();
                        return Ok(true);
                    }

                    // 直接透传 SSE 流，同时解析 token 统计
                    let mut resp = ResponseHeader::build(200, None)?;
                    resp.insert_header("Content-Type", "text/event-stream")?;
                    resp.insert_header("Cache-Control", "no-cache")?;
                    resp.insert_header("Connection", "keep-alive")?;
                    session.write_response_header(Box::new(resp), false).await?;

                    // 用于解析 SSE 并提取 token
                    let mut sse_parser = SseLineParser::new();
                    let mut total_input_tokens: u64 = 0;
                    let mut total_output_tokens: u64 = 0;

                    let mut stream_response = response;
                    while let Some(chunk) = stream_response.chunk().await.map_err(|e| {
                        Error::explain(ErrorType::InternalError, format!("passthrough chunk read error: {}", e))
                    })? {
                        // 解析 SSE 提取 token
                        sse_parser.push_data(&chunk);
                        let lines = sse_parser.extract_lines();
                        for line in lines {
                            if line == "[DONE]" {
                                continue;
                            }
                            // 尝试解析 JSON 提取 usage
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                                if let Some(usage) = v.get("usage") {
                                    if let Some(pt) = usage.get("input_tokens").and_then(|t| t.as_u64()) {
                                        total_input_tokens = pt;
                                    }
                                    if let Some(ct) = usage.get("output_tokens").and_then(|t| t.as_u64()) {
                                        total_output_tokens += ct;
                                    }
                                    // OpenAI 格式兼容
                                    if let Some(pt) = usage.get("prompt_tokens").and_then(|t| t.as_u64()) {
                                        total_input_tokens = pt;
                                    }
                                    if let Some(ct) = usage.get("completion_tokens").and_then(|t| t.as_u64()) {
                                        total_output_tokens += ct;
                                    }
                                }
                            }
                        }
                        session.write_response_body(Some(chunk), false).await?;
                    }
                    session.write_response_body(None, true).await?;

                    // ★ A→A passthrough 流结束时上报 token 统计
                    if total_input_tokens > 0 || total_output_tokens > 0 {
                        let selected = CODE_STATS_SELECTED.read().map(|s| s.clone()).unwrap_or_else(|_| "01".to_string());
                        CODE_STATS_TOTAL_CALLS.fetch_add(1, Ordering::Relaxed);
                        CODE_STATS_TOTAL_PROMPT.fetch_add(total_input_tokens, Ordering::Relaxed);
                        CODE_STATS_TOTAL_COMPLETION.fetch_add(total_output_tokens, Ordering::Relaxed);
                        if let Ok(mut models) = CODE_STATS_MODELS.write() {
                            let entry = models.entry(selected.clone()).or_insert_with(ModelStats::default);
                            entry.calls += 1;
                            entry.prompt += total_input_tokens;
                            entry.completion += total_output_tokens;
                            entry.last_prompt = total_input_tokens;
                            entry.last_completion = total_output_tokens;
                        }
                        info!("[port:{}] A→A passthrough stats: num={}, prompt={}, completion={}",
                            self.port, selected, total_input_tokens, total_output_tokens);
                    }

                    return Ok(true);
                }

                // ============================================================
                // 非流式路径：完整读取响应后转换
                // ============================================================
                let response = req.send().await
                    .map_err(|e| Error::explain(ErrorType::InternalError, format!("upstream error: {}", e)))?;

                let status = response.status();
                let response_body = response.text().await
                    .map_err(|e| Error::explain(ErrorType::InternalError, format!("read response: {}", e)))?;

                info!("Upstream response: status={}", status);

                let final_response_body = if self.transform && decision.need_transform {
                    let lua = self.lua.read().unwrap();

                    match extract_openai_fields(&response_body) {
                        Some(fields) => {
                            let lua_result = lua.call_on_transform_response(
                                &fields.id,
                                &fields.model,
                                &fields.finish_reason,
                                fields.input_tokens,
                                fields.output_tokens,
                                fields.compressed_content.as_deref(),
                                fields.compressed_tool_calls.as_deref(),
                            );

                            match lua_result {
                                Some(lr) => {
                                    let mut content_blocks = Vec::new();

                                    if let Some(ref comp_content) = lr.compressed_content {
                                        if let Some(content_val) = decompress_field(comp_content) {
                                            let text = content_val.as_str().unwrap_or("");
                                            if !text.is_empty() {
                                                content_blocks.extend(convert_openai_content_to_anthropic(text));
                                            }
                                        }
                                    }

                                    if let Some(ref comp_tc) = lr.compressed_tool_calls {
                                        if let Some(tc_val) = decompress_field(comp_tc) {
                                            if let Some(tc_arr) = tc_val.as_array() {
                                                let tool_use_blocks = convert_openai_tool_calls_to_anthropic(tc_arr);
                                                content_blocks.extend(tool_use_blocks);
                                            }
                                        }
                                    }

                                    if content_blocks.is_empty() {
                                        content_blocks.push(serde_json::json!({"type": "text", "text": ""}));
                                    }

                                    let anthropic_json = assemble_anthropic_response(
                                        &lr.id,
                                        &lr.model,
                                        &lr.stop_reason,
                                        lr.input_tokens,
                                        lr.output_tokens,
                                        content_blocks,
                                    );

                                    info!("Transformed OpenAI response to Anthropic format (via Lua+Rust pipeline)");
                                    anthropic_json
                                }
                                None => {
                                    info!("Lua on_transform_response failed, falling back to direct transform");
                                    match transform_openai_to_anthropic(&response_body, original_model.as_deref()) {
                                        Some(converted) => converted,
                                        None => {
                                            let model_str = original_model.as_deref().unwrap_or("");
                                            let error_msg = extract_error_message(&response_body);
                                            info!("Upstream non-OpenAI response, wrapping as Anthropic error: {}", error_msg);
                                            anthropic_error_response(&error_msg, model_str)
                                        }
                                    }
                                }
                            }
                        }
                        None => {
                            let model_str = original_model.as_deref().unwrap_or("");
                            let error_msg = extract_error_message(&response_body);
                            info!("Upstream non-OpenAI response, wrapping as Anthropic error: {}", error_msg);
                            anthropic_error_response(&error_msg, model_str)
                        }
                    }
                } else {
                    response_body.clone()
                };

                let resp_body = Bytes::from(final_response_body.clone());
                session.respond_error_with_body(status.as_u16(), resp_body).await?;

                ctx.response_status = status.as_u16();

                info!("[port:{}] Calling on_response callback with upstream: {}", 
                    self.port, decision.upstream);
                if let Ok(lua) = self.lua.read() {
                    lua.call_on_response(&decision.upstream, status.as_u16(), response_body.as_bytes());
                    info!("on_response callback completed");
                } else {
                    error!("Failed to get lua runtime");
                }

                return Ok(true);
            }
            _ => {
                ctx.decision = decision;
            }
        }
        Ok(false)
    }

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<Box<pingora_core::upstreams::peer::HttpPeer>> {
        // 使用 reqwest 直接代理，此方法不会被调用
        Err(Error::explain(ErrorType::InternalError, "upstream_peer not used"))
    }

    async fn logging(&self, session: &mut Session, _e: Option<&pingora_error::Error>, ctx: &mut Self::CTX) {
        let status = session.response_written().map_or(0, |resp| resp.status.as_u16());
        info!("{} {} -> {} (status={})", session.req_header().method, session.req_header().uri.path(), ctx.decision.upstream, status);
    }
}

fn extract_headers(session: &Session) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    for (name, value) in session.req_header().headers.iter() {
        if let Ok(v) = value.to_str() {
            headers.insert(name.to_string(), v.to_string());
        }
    }
    headers
}

/// 预加载 config.lua 到 Redis
///
/// Redis Key 设计:
///   provider:<name> -> baseurl|apikey
///   llm:<num> -> provider|model|cd
///   embed:provider, embed:model
///   rank:provider, rank:model
///   llm:select -> 当前选中的模型编号
///   llm:config:cool_down -> 冷却时间
fn preload_config(config_path: &PathBuf) {
    let config_content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(e) => {
            info!("Config file not found or readable: {}, skipping preload", e);
            return;
        }
    };

    let lua = Lua::new();

    let config_table: Table = match lua.load(&config_content).eval() {
        Ok(t) => t,
        Err(e) => {
            info!("Failed to load config.lua: {}, skipping preload", e);
            return;
        }
    };

    // 解析端口配置 (优先级: 环境变量 > config.lua > 默认值)
    if let Ok(listen) = config_table.get::<Table>("listen") {
        // LLM 端口
        if std::env::var("LLM_LISTEN").is_err() {
            if let Ok(llm) = listen.get::<String>("llm") {
                if let Ok(mut guard) = LLM_LISTEN.write() {
                    *guard = llm.clone();
                    info!("Set LLM_LISTEN from config: {}", llm);
                }
            }
        }
        // Code 端口
        if std::env::var("LLM_LISTEN_2").is_err() {
            if let Ok(code) = listen.get::<String>("code") {
                if let Ok(mut guard) = CODE_LISTEN.write() {
                    *guard = code.clone();
                    info!("Set CODE_LISTEN from config: {}", code);
                }
            }
        }
        // Admin 端口
        if std::env::var("LLM_LISTEN_3").is_err() {
            if let Ok(admin) = listen.get::<String>("admin") {
                if let Ok(mut guard) = ADMIN_LISTEN.write() {
                    *guard = admin.clone();
                    info!("Set ADMIN_LISTEN from config: {}", admin);
                }
            }
        }
        // Stats 端口
        if std::env::var("LLM_STATS_LISTEN").is_err() {
            if let Ok(stats) = listen.get::<String>("stats") {
                if let Ok(mut guard) = STATS_LISTEN.write() {
                    *guard = stats.clone();
                    info!("Set STATS_LISTEN from config: {}", stats);
                }
            }
        }
    }

    // 解析 TLS 配置 (优先级: 环境变量 > config.lua)
    if let Ok(tls) = config_table.get::<Table>("tls") {
        // TLS 启用状态
        if std::env::var("LLM_TLS_ENABLED").is_err() {
            if let Ok(enabled) = tls.get::<bool>("enabled") {
                if let Ok(mut guard) = TLS_ENABLED.write() {
                    *guard = enabled;
                    info!("Set TLS_ENABLED from config: {}", enabled);
                }
            }
        }
        // 证书路径
        if std::env::var("LLM_TLS_CERT").is_err() {
            if let Ok(cert) = tls.get::<String>("cert") {
                if let Ok(mut guard) = TLS_CERT.write() {
                    *guard = cert.clone();
                    info!("Set TLS_CERT from config: {}", cert);
                }
            }
        }
        // 私钥路径
        if std::env::var("LLM_TLS_KEY").is_err() {
            if let Ok(key) = tls.get::<String>("key") {
                if let Ok(mut guard) = TLS_KEY.write() {
                    *guard = key.clone();
                    info!("Set TLS_KEY from config: {}", key);
                }
            }
        }
        // TLS 监听地址 (兼容旧配置)
        if std::env::var("LLM_TLS_LISTEN").is_err() {
            if let Ok(tls_listen) = tls.get::<String>("listen") {
                if let Ok(mut guard) = TLS_LISTEN.write() {
                    *guard = tls_listen.clone();
                    info!("Set TLS_LISTEN from config: {}", tls_listen);
                }
            }
        }
    }

    // 解析 Code 端口配置 (router2.lua 处理)
    // code_tls: TLS 端口 (443)
    // code_http: HTTP 端口 (9443)
    if std::env::var("LLM_CODE_TLS_LISTEN").is_err() {
        if let Ok(code_tls) = config_table.get::<String>("code_tls") {
            if let Ok(mut guard) = CODE_TLS_LISTEN.write() {
                *guard = code_tls.clone();
                info!("Set CODE_TLS_LISTEN from config: {}", code_tls);
            }
            // 同时更新 TLS_LISTEN (兼容)
            if let Ok(mut guard) = TLS_LISTEN.write() {
                *guard = code_tls.clone();
            }
        }
    }
    if std::env::var("LLM_CODE_HTTP_LISTEN").is_err() {
        if let Ok(code_http) = config_table.get::<String>("code_http") {
            if let Ok(mut guard) = CODE_HTTP_LISTEN.write() {
                *guard = code_http.clone();
                info!("Set CODE_HTTP_LISTEN from config: {}", code_http);
            }
        }
    }

    // 环境变量覆盖 TLS 配置
    if let Ok(enabled) = std::env::var("LLM_TLS_ENABLED") {
        if let Ok(mut guard) = TLS_ENABLED.write() {
            *guard = enabled == "true" || enabled == "1";
            info!("Set TLS_ENABLED from env: {}", enabled);
        }
    }
    if let Ok(cert) = std::env::var("LLM_TLS_CERT") {
        if let Ok(mut guard) = TLS_CERT.write() {
            *guard = cert.clone();
            info!("Set TLS_CERT from env");
        }
    }
    if let Ok(key) = std::env::var("LLM_TLS_KEY") {
        if let Ok(mut guard) = TLS_KEY.write() {
            *guard = key.clone();
            info!("Set TLS_KEY from env");
        }
    }
    if let Ok(tls_listen) = std::env::var("LLM_TLS_LISTEN") {
        if let Ok(mut guard) = TLS_LISTEN.write() {
            *guard = tls_listen.clone();
            info!("Set TLS_LISTEN from env: {}", tls_listen);
        }
    }

    let mut conn = match get_redis_conn() {
        Some(c) => c,
        None => {
            info!("Failed to get Redis connection, skipping preload");
            return;
        }
    };

    if let Ok(existing) = redis::cmd("GET").arg("llm:initialized").query::<Option<String>>(&mut conn) {
        if existing.is_some() {
            info!("Redis already initialized (llm:initialized exists), skipping preload");
            return;
        }
    }

    info!("Preloading config.lua to Redis...");

    let cool_down: i64 = config_table.get("cool_down").unwrap_or(60);

    if let Ok(selected) = config_table.get::<String>("selected") {
        let _ = redis::cmd("SET").arg("llm:select").arg(&selected).query::<()>(&mut conn);
        info!("Set llm:select = {}", selected);
    }

    let _ = redis::cmd("SET").arg("llm:config:cool_down").arg(cool_down).query::<()>(&mut conn);
    info!("Set llm:config:cool_down = {}", cool_down);

    if let Ok(providers) = config_table.get::<Table>("providers") {
        for pair in providers.pairs::<String, Table>() {
            if let Ok((name, cfg)) = pair {
                let baseurl: String = cfg.get("baseurl").unwrap_or_default();
                let apikey: String = cfg.get("apikey").unwrap_or_default();
                let key = format!("provider:{}", name);
                let value = format!("{}|{}", baseurl, apikey);
                let _ = redis::cmd("SET").arg(&key).arg(&value).query::<()>(&mut conn);
                info!("Set provider:{} = {}|***", name, baseurl);
            }
        }
    }

    if let Ok(llm) = config_table.get::<Table>("llm") {
        for pair in llm.pairs::<String, Table>() {
            if let Ok((num, cfg)) = pair {
                let provider: String = cfg.get("provider").unwrap_or_default();
                let model: String = cfg.get("model").unwrap_or_default();
                let cd: i64 = cfg.get("cd").unwrap_or(cool_down);
                let key = format!("llm:{}", num);
                let value = format!("{}|{}|{}", provider, model, cd);
                let _ = redis::cmd("SET").arg(&key).arg(&value).query::<()>(&mut conn);
                info!("Set llm:{} = {}|{}|{}", num, provider, model, cd);
            }
        }
    }

    if let Ok(embed) = config_table.get::<Table>("embed") {
        if let Ok(provider) = embed.get::<String>("provider") {
            let _ = redis::cmd("SET").arg("embed:provider").arg(&provider).query::<()>(&mut conn);
            info!("Set embed:provider = {}", provider);
        }
        if let Ok(model) = embed.get::<String>("model") {
            let _ = redis::cmd("SET").arg("embed:model").arg(&model).query::<()>(&mut conn);
            info!("Set embed:model = {}", model);
        }
    }

    if let Ok(rank) = config_table.get::<Table>("rank") {
        if let Ok(provider) = rank.get::<String>("provider") {
            let _ = redis::cmd("SET").arg("rank:provider").arg(&provider).query::<()>(&mut conn);
            info!("Set rank:provider = {}", provider);
        }
        if let Ok(model) = rank.get::<String>("model") {
            let _ = redis::cmd("SET").arg("rank:model").arg(&model).query::<()>(&mut conn);
            info!("Set rank:model = {}", model);
        }
    }

    let _ = redis::cmd("SET").arg("llm:initialized").arg("1").query::<()>(&mut conn);
    info!("Config preload completed, llm:initialized = 1");
}

fn spawn_file_watcher(lua: Arc<RwLock<LuaRuntime>>, script_path: PathBuf) -> RecommendedWatcher {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |res: std::result::Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        },
        notify::Config::default(),
    )
    .expect("create watcher");

    watcher.watch(&script_path, RecursiveMode::NonRecursive).expect("watch script");

    std::thread::spawn(move || {
        let mut last_reload = std::time::Instant::now();
        while let Ok(event) = rx.recv() {
            if matches!(event.kind, EventKind::Modify(_)) && last_reload.elapsed() > Duration::from_millis(500) {
                last_reload = std::time::Instant::now();
                info!("Reloading script...");
                if let Ok(lua) = lua.write() {
                    match lua.reload() {
                        Ok(_) => info!("Reloaded"),
                        Err(e) => error!("Reload failed: {}", e),
                    }
                }
            }
        }
    });
    watcher
}

/// TLS 错误记录到 Redis 的 key
static TLS_ERROR_KEY: &str = "llm:tls_errors";

/// 自定义日志写入器，检测 TLS 握手错误并写入 Redis
struct TlsAwareLogWriter {
    inner: std::fs::File,
}

impl TlsAwareLogWriter {
    fn new(file: std::fs::File) -> Self {
        Self { inner: file }
    }

    /// 检查日志内容是否是 TLS 握手错误，如果是则写入 Redis
    fn check_and_log_tls_error(&self, content: &str) {
        // 检测 TLS 握手失败的错误
        if content.contains("Downstream handshake error") ||
           content.contains("TLSHandshakeFailure") ||
           content.contains("TLS accept() failed") {
            // 提取客户端地址 (如果有)
            let peer_info = if let Some(start) = content.find("from ") {
                if let Some(end) = content[start..].find(':') {
                    &content[start + 5..start + end]
                } else {
                    "unknown"
                }
            } else {
                "unknown"
            };

            // 异步写入 Redis (使用 try_spawn 避免阻塞)
            let error_msg = content.to_string();
            let timestamp = chrono::Utc::now().to_rfc3339();

            // 使用非阻塞方式尝试写入 Redis
            if let Some(mut conn) = get_redis_conn() {
                let full_entry = format!("{}|{}|{}", timestamp, peer_info, error_msg);
                // 使用 RPUSH 添加到列表尾部，保留最近 100 条
                let _: Result<(), _> = redis::cmd("RPUSH")
                    .arg(TLS_ERROR_KEY)
                    .arg(&full_entry)
                    .query(&mut *conn);
                // 限制列表长度为最近 100 条
                let _: Result<(), _> = redis::cmd("LTRIM")
                    .arg(TLS_ERROR_KEY)
                    .arg(-100i64)
                    .arg(-1i64)
                    .query(&mut *conn);
                info!("TLS error logged to Redis: {}", peer_info);
            }
        }
    }
}

impl std::io::Write for TlsAwareLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // 先写入文件
        let result = self.inner.write(buf);

        // 检查是否是 TLS 错误
        if let Ok(s) = std::str::from_utf8(buf) {
            self.check_and_log_tls_error(s);
        }

        result
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// 检查端口是否可用
fn check_port_available(addr: &str) -> Result<(), String> {
    use std::net::TcpListener;

    // 解析地址
    let socket_addr: std::net::SocketAddr = addr.parse()
        .map_err(|e| format!("Invalid address '{}': {}", addr, e))?;

    // 尝试绑定端口
    match TcpListener::bind(socket_addr) {
        Ok(listener) => {
            // 成功绑定后立即释放
            drop(listener);
            Ok(())
        }
        Err(e) => {
            Err(format!("Port {} is not available: {}", addr, e))
        }
    }
}

/// 检查所有服务端口是否可用
fn check_all_ports_available() -> Result<(), String> {
    let mut errors = Vec::new();

    // 主服务端口 (9090)
    let listen = std::env::var("LLM_LISTEN").unwrap_or_else(|_| {
        LLM_LISTEN.read().unwrap().clone()
    });
    if let Err(e) = check_port_available(&listen) {
        errors.push(format!("Main service ({}): {}", listen, e));
    }

    // Code HTTP 端口 (9443)
    let code_http_listen = std::env::var("LLM_CODE_HTTP_LISTEN").unwrap_or_else(|_| {
        CODE_HTTP_LISTEN.read().unwrap().clone()
    });
    if let Err(e) = check_port_available(&code_http_listen) {
        errors.push(format!("Code HTTP ({}): {}", code_http_listen, e));
    }

    // Code TLS 端口 (443) - 仅在 TLS 启用时检查
    let tls_enabled = std::env::var("LLM_TLS_ENABLED")
        .map(|v| v == "true" || v == "1")
        .unwrap_or_else(|_| {
            TLS_ENABLED.read().map(|g| *g).unwrap_or(false)
        });
    if tls_enabled {
        let code_tls_listen = std::env::var("LLM_CODE_TLS_LISTEN").unwrap_or_else(|_| {
            CODE_TLS_LISTEN.read().unwrap().clone()
        });
        if let Err(e) = check_port_available(&code_tls_listen) {
            errors.push(format!("Code TLS ({}): {}", code_tls_listen, e));
        }
    }

    // Admin Console 端口 (9093)
    let listen3 = std::env::var("LLM_ADMIN_LISTEN").unwrap_or_else(|_| {
        ADMIN_LISTEN.read().unwrap().clone()
    });
    if let Err(e) = check_port_available(&listen3) {
        errors.push(format!("Admin Console ({}): {}", listen3, e));
    }

    // Stats 端口 (9091)
    let stats_listen = std::env::var("LLM_STATS_LISTEN").unwrap_or_else(|_| {
        STATS_LISTEN.read().unwrap().clone()
    });
    if let Err(e) = check_port_available(&stats_listen) {
        errors.push(format!("Stats server ({}): {}", stats_listen, e));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("\n  - {}", errors.join("\n  - ")))
    }
}

fn main() {
    // 初始化 rustls 加密后端 (ring)
    // rustls 0.23+ 需要显式安装 CryptoProvider
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // 日志输出到文件 (gateway.log)
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("gateway.log")
        .expect("Failed to open gateway.log");
    let tls_aware_writer = TlsAwareLogWriter::new(log_file);
    let log_file_box: Box<dyn std::io::Write + Send> = Box::new(tls_aware_writer);

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(log_file_box))
        .format(|buf, record| {
            use std::io::Write;
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            writeln!(buf, "[{} {}:{}] {}", record.level(), record.target(), record.line().unwrap_or(0), record.args())
        })
        .init();

    // 使用环境变量或默认值配置
    // 注意：preload_config 会先设置全局端口变量，环境变量优先级最高
    let listen = std::env::var("LLM_LISTEN").unwrap_or_else(|_| {
        LLM_LISTEN.read().unwrap().clone()
    });
    let script = std::env::var("LLM_SCRIPT").unwrap_or_else(|_| "lua/router.lua".to_string());

    // 解析脚本路径：
    // 1. 绝对路径直接使用
    // 2. 相对于项目根目录（通过查找 Cargo.toml 确定）
    // 3. 相对于可执行文件所在目录
    let script_path = if PathBuf::from(&script).is_absolute() {
        PathBuf::from(&script)
    } else {
        // 尝试查找项目根目录（向上查找 Cargo.toml）
        let mut search_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        loop {
            if search_dir.join("Cargo.toml").exists() {
                // 找到项目根目录
                break;
            }
            if !search_dir.pop() {
                // 已经到达根目录，使用当前目录
                search_dir = PathBuf::from(".");
                break;
            }
        }
        search_dir.join(&script)
    };

    // 解析 config.lua 路径
    let config_path = script_path.parent()
        .map(|p| p.join("config.lua"))
        .unwrap_or_else(|| PathBuf::from("lua/config.lua"));

    info!("Starting LLM Gateway on {}", listen);
    info!("Script: {:?}", script_path);
    info!("Config: {:?}", config_path);

    preload_config(&config_path);

    // 检查所有端口是否可用
    if let Err(e) = check_all_ports_available() {
        eprintln!("FATAL: Port check failed:{}", e);
        eprintln!("Please ensure all required ports are available and not in use by other processes.");
        std::process::exit(1);
    }
    info!("All ports are available");

    let lua_runtime = match LuaRuntime::new(script_path.clone()) {
        Ok(rt) => Arc::new(RwLock::new(rt)),
        Err(e) => {
            error!("Lua init failed: {}", e);
            std::process::exit(1);
        }
    };

    let _watcher = spawn_file_watcher(lua_runtime.clone(), script_path.clone());

    // 第二个 Lua 运行时 (router2.lua - 用于 Code 端口)
    let script2 = std::env::var("LLM_SCRIPT_2").unwrap_or_else(|_| "router2.lua".to_string());
    let script_path2 = if PathBuf::from(&script2).is_absolute() {
        PathBuf::from(&script2)
    } else {
        script_path.parent().map(|p| p.join(&script2)).unwrap_or_else(|| PathBuf::from(&script2))
    };
    info!("Script2: {:?}", script_path2);

    let lua_runtime2 = match LuaRuntime::new(script_path2.clone()) {
        Ok(rt) => Arc::new(RwLock::new(rt)),
        Err(e) => {
            error!("Lua2 init failed: {}", e);
            std::process::exit(1);
        }
    };
    let _watcher2 = spawn_file_watcher(lua_runtime2.clone(), script_path2);

    // 第三个 Lua 运行时 (端口 9093 - Admin Console)
    let listen3 = std::env::var("LLM_LISTEN_3").unwrap_or_else(|_| {
        ADMIN_LISTEN.read().unwrap().clone()
    });
    let script3 = std::env::var("LLM_SCRIPT_3").unwrap_or_else(|_| "admin.lua".to_string());
    let script_path3 = if PathBuf::from(&script3).is_absolute() {
        PathBuf::from(&script3)
    } else {
        script_path.parent().map(|p| p.join(&script3)).unwrap_or_else(|| PathBuf::from(&script3))
    };
    info!("Script3: {:?}", script_path3);

    let lua_runtime3 = match LuaRuntime::new(script_path3.clone()) {
        Ok(rt) => Arc::new(RwLock::new(rt)),
        Err(e) => {
            error!("Lua3 init failed: {}", e);
            std::process::exit(1);
        }
    };
    let _watcher3 = spawn_file_watcher(lua_runtime3.clone(), script_path3);

    let opt = Opt::parse_args();
    let mut server = Server::new(Some(opt)).unwrap();
    server.bootstrap();

    // 解析端口号
    let port1 = parse_port(&listen);
    let port3 = parse_port(&listen3);

    // 解析转换模式标志 (默认启用，向后兼容)
    let code_transform = std::env::var("LLM_CODE_TRANSFORM")
        .map(|v| v == "true")
        .unwrap_or(true);

    // 设置端口全局变量 (用于统计页面显示)
    LLM_PORT.store(port1 as u64, Ordering::Relaxed);
    // CODE_PORT 使用 HTTP 端口 (9443)
    let code_http_port = parse_port(&std::env::var("LLM_CODE_HTTP_LISTEN").unwrap_or_else(|_| {
        CODE_HTTP_LISTEN.read().unwrap().clone()
    }));
    CODE_PORT.store(code_http_port as u64, Ordering::Relaxed);

    // 初始化 TLS 管理器 (优先级: 环境变量 > config.lua)
    let tls_enabled = TLS_ENABLED.read().map(|g| *g).unwrap_or(false);
    let tls_cert = TLS_CERT.read().map(|g| g.clone()).unwrap_or_default();
    let tls_key = TLS_KEY.read().map(|g| g.clone()).unwrap_or_default();

    // Code 端口配置 (router2.lua 处理)
    let code_tls_listen = std::env::var("LLM_CODE_TLS_LISTEN").unwrap_or_else(|_| {
        CODE_TLS_LISTEN.read().unwrap().clone()
    });
    let code_http_listen = std::env::var("LLM_CODE_HTTP_LISTEN").unwrap_or_else(|_| {
        CODE_HTTP_LISTEN.read().unwrap().clone()
    });

    let ssl_manager: Option<ssl::SslManager> = if tls_enabled && !tls_cert.is_empty() && !tls_key.is_empty() {
        match ssl::SslManager::new(ssl::TlsConfig::new(&tls_cert, &tls_key)) {
            Ok(manager) => {
                info!("TLS configured: cert={}, key={}", tls_cert, tls_key);
                Some(manager)
            }
            Err(e) => {
                error!("TLS config invalid: {}", e);
                None
            }
        }
    } else {
        info!("TLS not configured (enabled={}, cert={})", tls_enabled, tls_cert);
        None
    };

    // 主服务 (端口 9090)
    let gateway = LuaGateway::new(lua_runtime, port1, false);
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, gateway);
    proxy_service.add_tcp(&listen);
    server.add_service(proxy_service);
    info!("Listening on {} (router.lua, port={})", listen, port1);

    // Code HTTP 服务 (端口 9443，无 TLS)
    // 注意：如果启用 TLS，HTTP 服务只绑定到 127.0.0.1，由独立 TLS 代理转发
    let gateway_http = LuaGateway::new(lua_runtime2.clone(), code_http_port as u16, code_transform);
    let mut proxy_service_http = pingora_proxy::http_proxy_service(&server.configuration, gateway_http);

    // 保存 TLS 配置用于后续启动独立代理
    let tls_proxy_config: Option<(String, String, String, String)> = if let Some(ref manager) = ssl_manager {
        // TLS 模式：HTTP 服务只监听本地
        let local_http_listen = format!("127.0.0.1:{}", code_http_port);
        proxy_service_http.add_tcp(&local_http_listen);
        info!("HTTP backend listening on {} (router2.lua, TLS mode)", local_http_listen);

        // 返回 TLS 代理配置
        Some((
            manager.cert_path().to_string_lossy().to_string(),
            manager.key_path().to_string_lossy().to_string(),
            code_tls_listen.clone(),
            local_http_listen,
        ))
    } else {
        // 非 TLS 模式：HTTP 服务监听所有接口
        proxy_service_http.add_tcp(&code_http_listen);
        info!("Listening on {} (router2.lua HTTP, port={})", code_http_listen, code_http_port);
        None
    };
    server.add_service(proxy_service_http);

    // 第三服务 (端口 9093 - Admin Console)
    let gateway3 = LuaGateway::new(lua_runtime3, port3, false);
    let mut proxy_service3 = pingora_proxy::http_proxy_service(&server.configuration, gateway3);
    proxy_service3.add_tcp(&listen3);
    server.add_service(proxy_service3);
    info!("Listening on {} (admin.lua, port={})", listen3, port3);

    // 启动独立的 stats HTTP 服务器 (9091 端口，完全绕过 Pingora/Lua)
    let stats_listen = std::env::var("LLM_STATS_LISTEN").unwrap_or_else(|_| {
        STATS_LISTEN.read().unwrap().clone()
    });
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = match TcpListener::bind(&stats_listen) {
            Ok(l) => {
                info!("Stats server listening on {}", stats_listen);
                l
            }
            Err(e) => {
                error!("Failed to bind stats server: {}", e);
                return;
            }
        };

        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let mut buf = [0u8; 1024];
                    if let Ok(_) = stream.read(&mut buf) {
                        let request = String::from_utf8_lossy(&buf);
                        if request.starts_with("GET /running") || request.starts_with("GET / ") {
                            let html = generate_running_html();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                                html.len(), html
                            );
                            let _ = stream.write_all(response.as_bytes());
                        } else {
                            let body = r#"{"error":"Use /running for stats"}"#;
                            let response = format!(
                                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(), body
                            );
                            let _ = stream.write_all(response.as_bytes());
                        }
                    }
                }
                Err(_) => continue,
            }
        }
    });

    // 启动独立 TLS 代理 (Plan B: 避免 Pingora TLS 问题)
    if let Some((cert_path, key_path, tls_listen, http_backend)) = tls_proxy_config {
        info!("Starting standalone TLS proxy: {} -> {}", tls_listen, http_backend);

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime for TLS proxy");
            rt.block_on(async {
                if let Err(e) = tls_server::start_tls_proxy(&tls_listen, &cert_path, &key_path, &http_backend).await {
                    error!("TLS proxy error: {}", e);
                }
            });
        });
    }

    server.run_forever();
}

