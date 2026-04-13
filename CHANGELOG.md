# Changelog

## [2026-04-13] v0.1.3 - SiliconFlow Anthropic 端点适配

### 新增

- **SiliconFlow Anthropic SDK**: 新增 `sdk_siliconflow_anthropic.lua`，支持 SiliconFlow 的 Anthropic 兼容端点
- **baseurl 重写逻辑**: 当 baseurl 为 `https://api.siliconflow.cn/anthropic/v1` 时，自动重写为 `https://api.siliconflow.cn`，路径设为 `/v1/messages`
- **调试日志增强**: 新增 `code:debug_sdk` 和 `code:debug_rewrite` Redis 调试键

### 修复

- 修复 SiliconFlow Anthropic 端点路径计算 bug（原路径 `/anthropic/v1/v1/messages` 修正为 `/v1/messages`）

### 行为变化

```
之前: SiliconFlow Anthropic baseurl → 错误路径 /anthropic/v1/v1/messages
现在: SiliconFlow Anthropic baseurl → 正确路径 /v1/messages
     同时保持 Anthropic 格式透传（不转换）
```

---

## [2026-04-11] v0.1.2 - Anthropic 完整支持 + 端口分离

### 新增

- **端口 9089 独立路由**: 新增 router2.lua，支持独立的路由逻辑
- **Anthropic 格式完整支持**: SSE 流式响应 + 请求体转换 + 响应格式修正
- **真 SSE 流式转换**: 9089 端口逐 chunk 转换 + 连接注册表管理
- **请求保存功能**: router2.lua 支持保存最近 5 个请求到 Redis (`raw` key)
- **config2.lua 初始化**: router2.lua 支持从 config2.lua 加载独立配置
- **自定义 Redis 连接池**: 实现自定义连接池，复用 Redis 连接

### 变更

- 使用 flate2 解压 + 拆分 Anthropic 压缩响应
- Lua 主导字段改写 + Rust SSE 处理 + code:select 识别

### 修复

- router2.lua 5 个关键 bug 修复
- 使用 r2d2 连接池复用 Redis 连接，避免 TIME_WAIT 爆炸

### 架构变化

```
之前: 单端口统一路由
现在: 9090端口 -> router.lua (通用)
     9089端口 -> router2.lua (Anthropic专用)
```

---

## [2026-04-05] TLS 与请求体重写优化

### 新增

- **请求体自动重写**: Lua 返回 `new_body` 字段，自动替换请求体中的 `model` 字段
- **TLS 证书验证开关**: 环境变量 `LLM_TLS_VERIFY` 控制证书验证（默认跳过）

### 变更

- **上游连接改用 reqwest**: 替代 Pingora 内置上游连接，解决 rustls 兼容性问题
  - 使用 `rustls-tls` feature
  - 支持标准 HTTPS 连接
- **移除无用依赖**: 清理 `clap`, `http` 等未使用的依赖

### 修复

- TLS 连接失败问题（Pingora rustls 与某些服务端不兼容）
- Content-Length 不匹配问题（请求体替换后自动更新）

### 架构变化

```
之前: Pingora 接收 → Lua 决策 → Pingora 上游连接 (rustls 问题)
现在: Pingora 接收 → Lua 决策 → reqwest 上游连接 (rustls-tls)
```

---

## [2026-04-04] Provider 配置分离

### 新增

- **Provider 配置分离**: 提供商配置独立存储，LLM 配置引用提供商
- **Redis Key 重构**: 管道分隔格式简化存储

### 变更

- `provider:{name}` -> `baseurl|apikey`
- `llm:{num}` -> `provider|model|cd`

---

## [2026-04-03] 初始版本

### 功能

- 基于 Pingora + Lua 的反向代理
- Redis 配置存储
- 主备切换机制
- Embeddings / Rerank 路由
- 热更新支持
