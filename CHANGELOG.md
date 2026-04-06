# Changelog

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
