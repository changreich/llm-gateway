//! LLM Gateway - Pingora + Lua 反向代理
//!
//! 支持 embeddings/rerank/chat 等多种请求类型
//! 通过 Lua 脚本实现模型映射和路由决策
//!
//! Pingora 职责：接收请求 → 传给 Lua (header) → 执行决策 → 上报响应
//! Lua 职责：Redis 配置读取、URL 重写、Token 统计

use async_trait::async_trait;
use bytes::Bytes;
use log::{error, info};
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

/// OpenAI Chat Completion 响应 → Anthropic Message 响应
fn transform_openai_to_anthropic(body: &str, original_model: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;

    if v.get("object").and_then(|o| o.as_str()) != Some("chat.completion") {
        return None;
    }

    let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
    let model = original_model.unwrap_or_else(|| v.get("model").and_then(|m| m.as_str()).unwrap_or(""));

    let msg = v.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("message"))?;

    let mut content: Vec<serde_json::Value> = Vec::new();

    // text content
    if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": text}));
        }
    }

    // tool_calls → tool_use content blocks
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
        for tc in tool_calls {
            let tool_id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let tool_name = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
            let tool_input = tc.get("function").and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .unwrap_or(serde_json::json!({}));
            content.push(serde_json::json!({
                "type": "tool_use",
                "id": tool_id,
                "name": tool_name,
                "input": tool_input
            }));
        }
    }

    // fallback: empty content
    if content.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": ""}));
    }

    let finish_reason = v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        .unwrap_or("stop");

    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    };

    let input_tokens = v.get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|p| p.as_u64())
        .unwrap_or(0);

    let output_tokens = v.get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|c| c.as_u64())
        .unwrap_or(0);

    let anthropic_response = serde_json::json!({
        "id": format!("msg_{}", id.strip_prefix("chatcmpl-").unwrap_or(id)),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    });

    Some(serde_json::to_string(&anthropic_response).unwrap_or_default())
}

/// 将 Anthropic JSON 响应包装为 SSE 流格式
/// 模拟 Anthropic 流式响应事件序列:
///   message_start → content_block_start → content_block_delta(s) → content_block_stop → message_delta → message_stop
fn wrap_as_sse(anthropic_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(anthropic_json) {
        Ok(v) => v,
        Err(_) => return anthropic_json.to_string(),
    };

    let msg_id = v.get("id").and_then(|i| i.as_str()).unwrap_or("msg_unknown");
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let input_tokens = v.get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let output_tokens = v.get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let stop_reason = v.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("end_turn");

    let mut sse = String::new();

    // message_start
    let message_start = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": msg_id,
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": model,
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": 0
            }
        }
    });
    sse.push_str(&format!("event: message_start\ndata: {}\n\n", message_start));

    // content blocks
    let content = v.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    for (idx, block) in content.iter().enumerate() {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("text");

        // content_block_start
        let start_block = if block_type == "text" {
            serde_json::json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "text", "text": ""}
            })
        } else if block_type == "tool_use" {
            serde_json::json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "tool_use",
                    "id": block.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                    "name": block.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                    "input": {}
                }
            })
        } else {
            serde_json::json!({"type": "content_block_start", "index": idx, "content_block": block})
        };
        sse.push_str(&format!("event: content_block_start\ndata: {}\n\n", start_block));

        // content_block_delta
        if block_type == "text" {
            let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let delta = serde_json::json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "text_delta", "text": text}
            });
            sse.push_str(&format!("event: content_block_delta\ndata: {}\n\n", delta));
        } else if block_type == "tool_use" {
            let input = block.get("input").cloned().unwrap_or(serde_json::json!({}));
            let input_str = serde_json::to_string(&input).unwrap_or_default();
            let delta = serde_json::json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "input_json_delta", "partial_json": input_str}
            });
            sse.push_str(&format!("event: content_block_delta\ndata: {}\n\n", delta));
        }

        // content_block_stop
        sse.push_str(&format!("event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{}}}\n\n", idx));
    }

    // message_delta
    let message_delta = serde_json::json!({
        "type": "message_delta",
        "delta": {
            "stop_reason": stop_reason,
            "stop_sequence": null
        },
        "usage": {
            "output_tokens": output_tokens
        }
    });
    sse.push_str(&format!("event: message_delta\ndata: {}\n\n", message_delta));

    // message_stop
    sse.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    sse
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
    let total_calls = STATS_TOTAL_CALLS.load(Ordering::Relaxed);
    let total_prompt = STATS_TOTAL_PROMPT.load(Ordering::Relaxed);
    let total_completion = STATS_TOTAL_COMPLETION.load(Ordering::Relaxed);
    let total_tokens = total_prompt + total_completion;

    let selected = STATS_SELECTED.read().map(|s| s.clone()).unwrap_or_else(|_| "01".to_string());
    let config = STATS_CONFIG.read().map(|c| c.clone()).unwrap_or_else(|_| HashMap::new());
    let models = STATS_MODELS.read().map(|m| m.clone()).unwrap_or_else(|_| HashMap::new());

    let mut model_rows = String::new();
    let mut nums: Vec<String> = config.keys().cloned().collect();
    nums.sort();

    for num in &nums {
        let (provider, model) = config.get(num).cloned().unwrap_or_else(|| ("?".to_string(), "?".to_string()));
        let stats = models.get(num).cloned().unwrap_or_default();
        let is_selected = num == &selected;
        let sel_class = if is_selected { " class=\"selected\"" } else { "" };
        let sel_mark = if is_selected { " *" } else { "" };
        let last = if stats.last_prompt > 0 {
            format!("<span class=\"prompt\">{}</span>+<span class=\"completion\">{}</span>", stats.last_prompt, stats.last_completion)
        } else {
            "-".to_string()
        };

        model_rows.push_str(&format!(
            "<tr{}><td><span class=\"num\">{}</span>{}</td><td><span class=\"provider\">{}</span></td><td style=\"font-size:0.8em;color:#666\">{}</td><td>{}</td><td class=\"prompt\">{}</td><td class=\"completion\">{}</td><td style=\"font-size:0.8em;color:#888\">{}</td></tr>",
            sel_class, num, sel_mark, provider, model, stats.calls, stats.prompt, stats.completion, last
        ));
    }

    format!(r#"<!DOCTYPE html>
<html><head><meta charset="UTF-8"><title>LLM Gateway</title>
<style>body{{font-family:sans-serif;max-width:900px;margin:40px auto;padding:20px;background:#f5f5f5}}h1{{color:#333;border-bottom:2px solid #4CAF50;padding-bottom:10px}}h2{{color:#555;margin-top:25px}}.card{{background:white;border-radius:8px;padding:20px;margin:15px 0;box-shadow:0 2px 4px rgba(0,0,0,0.1)}}table{{width:100%;border-collapse:collapse}}th,td{{text-align:left;padding:10px;border-bottom:1px solid #eee}}th{{background:#f9f9f9}}.stat-box{{display:inline-block;background:linear-gradient(135deg,#667eea,#764ba2);color:white;padding:12px 20px;border-radius:8px;margin:5px;text-align:center}}.stat-box .v{{font-size:1.4em;font-weight:bold}}.stat-box .l{{font-size:0.75em;opacity:0.9}}.stat-box.green{{background:linear-gradient(135deg,#11998e,#38ef7d)}}.stat-box.orange{{background:linear-gradient(135deg,#f093fb,#f5576c)}}.num{{font-family:monospace;background:#e3f2fd;padding:2px 6px;border-radius:4px}}.provider{{color:#1976d2;font-weight:500}}.selected{{background:#fff3e0}}.prompt{{color:#4CAF50}}.completion{{color:#2196F3}}</style>
</head><body><h1>LLM Gateway</h1>
<div class="stat-box"><div class="v">{}</div><div class="l">调用次数</div></div>
<div class="stat-box green"><div class="v">{}</div><div class="l">Prompt</div></div>
<div class="stat-box orange"><div class="v">{}</div><div class="l">Completion</div></div>
<div class="stat-box"><div class="v">{}</div><div class="l">总Token</div></div>
<div class="card"><h2>模型统计</h2><table>
<tr><th>编号</th><th>提供商</th><th>模型</th><th>调用</th><th class="prompt">Prompt</th><th class="completion">Completion</th><th>最近</th></tr>
{}</table></div>
<p style="color:#999;text-align:center;margin-top:20px">LLM Gateway | <a href="/debug">debug</a> | <a href="/config">config</a> | <a href="/raw">raw</a></p>
</body></html>"#, total_calls, total_prompt, total_completion, total_tokens, model_rows)
}

#[derive(Clone, Default, Debug)]
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
}

impl LuaGateway {
    fn new(lua: Arc<RwLock<LuaRuntime>>, port: u16) -> Self {
        Self { lua, port }
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
        let path = session.req_header().uri.path().to_string();
        info!("Processing: {} {}", method, path);

        // /running 端点：直接在 Rust 中处理，完全绕过 Lua（避免被其他请求阻塞）
        if path == "/running" {
            let html = generate_running_html();
            session.respond_error_with_body(200, Bytes::from(html)).await?;
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
                // 使用 reqwest 直接代理请求
                ctx.decision = decision.clone();

                // 构建上游 URL
                let upstream_url = if decision.tls {
                    format!("https://{}{}", decision.addr, decision.rewrite_path)
                } else {
                    format!("http://{}{}", decision.addr, decision.rewrite_path)
                };

                info!("Proxying to: {} (tls={})", upstream_url, decision.tls);

                // 使用 reqwest 发送请求
                let client = reqwest::Client::builder()
                    .danger_accept_invalid_certs(!*SKIP_TLS_VERIFY || !decision.tls)
                    .build()
                    .map_err(|e| Error::explain(ErrorType::InternalError, format!("create client: {}", e)))?;

                let request_body = if !decision.new_request_body.is_empty() {
                    decision.new_request_body.clone()
                } else {
                    String::from_utf8_lossy(&body).to_string()
                };

                let mut req = client.post(&upstream_url)
                    .header("Content-Type", "application/json")
                    .body(request_body);

                if !decision.api_key.is_empty() {
                    req = req.header("Authorization", format!("Bearer {}", decision.api_key));
                }

                // Extract hostname only for Host header (strip path from addr like "opencode.ai/zen/go")
                let host_only = decision.addr.split('/').next().unwrap_or(&decision.addr);
                req = req.header("Host", host_only);

                let response = req.send().await
                    .map_err(|e| Error::explain(ErrorType::InternalError, format!("upstream error: {}", e)))?;

                let status = response.status();
                let response_body = response.text().await
                    .map_err(|e| Error::explain(ErrorType::InternalError, format!("read response: {}", e)))?;

                info!("Upstream response: status={}", status);

                // 9089 端口始终将 OpenAI 响应转为 Anthropic 格式
                let stream_requested = serde_json::from_slice::<serde_json::Value>(&ctx.request_body)
                    .ok()
                    .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
                    .unwrap_or(false);

                let original_model = serde_json::from_slice::<serde_json::Value>(&ctx.request_body)
                    .ok()
                    .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()));

                let final_response_body = if self.port == 9089 {
                    match transform_openai_to_anthropic(&response_body, original_model.as_deref()) {
                        Some(converted) => {
                            if stream_requested {
                                info!("Wrapping Anthropic response as SSE stream");
                                wrap_as_sse(&converted)
                            } else {
                                info!("Transformed OpenAI response to Anthropic format");
                                converted
                            }
                        }
                        None => response_body.clone(),
                    }
                } else {
                    response_body.clone()
                };

                // 返回响应 (先克隆用于回调)
                let resp_body = Bytes::from(final_response_body.clone());
                if stream_requested && self.port == 9089 {
                    let mut resp = ResponseHeader::build(200, None)?;
                    resp.insert_header("Content-Type", "text/event-stream")?;
                    resp.insert_header("Cache-Control", "no-cache")?;
                    resp.insert_header("Connection", "keep-alive")?;
                    session.write_response_header(Box::new(resp), false).await?;
                    session.write_response_body(Some(resp_body), true).await?;
                } else {
                    session.respond_error_with_body(status.as_u16(), resp_body).await?;
                }

                ctx.response_status = status.as_u16();

                // 调用 Lua 回调
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

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // 使用环境变量或默认值配置
    let listen = std::env::var("LLM_LISTEN").unwrap_or_else(|_| "0.0.0.0:9090".to_string());
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

    let lua_runtime = match LuaRuntime::new(script_path.clone()) {
        Ok(rt) => Arc::new(RwLock::new(rt)),
        Err(e) => {
            error!("Lua init failed: {}", e);
            std::process::exit(1);
        }
    };

    let _watcher = spawn_file_watcher(lua_runtime.clone(), script_path.clone());

    // 第二个 Lua 运行时 (端口 9089)
    let listen2 = std::env::var("LLM_LISTEN_2").unwrap_or_else(|_| "0.0.0.0:9089".to_string());
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

    let opt = Opt::parse_args();
    let mut server = Server::new(Some(opt)).unwrap();
    server.bootstrap();

    // 主服务 (端口 9090)
    let gateway = LuaGateway::new(lua_runtime, 9090);
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, gateway);
    proxy_service.add_tcp(&listen);
    server.add_service(proxy_service);
    info!("Listening on {} (router.lua)", listen);

    // 第二服务 (端口 9089)
    let gateway2 = LuaGateway::new(lua_runtime2, 9089);
    let mut proxy_service2 = pingora_proxy::http_proxy_service(&server.configuration, gateway2);
    proxy_service2.add_tcp(&listen2);
    server.add_service(proxy_service2);
    info!("Listening on {} (router2.lua)", listen2);

    // 启动独立的 stats HTTP 服务器 (9091 端口，完全绕过 Pingora/Lua)
    let stats_listen = std::env::var("LLM_STATS_LISTEN").unwrap_or_else(|_| "0.0.0.0:9091".to_string());
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

    server.run_forever();
}
