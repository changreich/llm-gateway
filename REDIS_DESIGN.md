# Redis 配置管理

## Key 设计原则

采用**管道分隔格式**简化存储，避免使用 Redis Hash：

```
provider:{name} -> "baseurl|apikey"
llm:{num}       -> "provider|model|cd"
```

## 完整 Key 列表

### 提供商配置

| Key | 格式 | 示例 |
|-----|------|------|
| `provider:siliconflow` | `baseurl\|apikey` | `https://api.siliconflow.cn/v1\|sk-xxx` |
| `provider:zhipu` | `baseurl\|apikey` | `https://open.bigmodel.cn/api/paas/v4\|xxx` |
| `provider:openai` | `baseurl\|apikey` | `https://api.openai.com/v1\|sk-xxx` |

### LLM 配置

| Key | 格式 | 示例 |
|-----|------|------|
| `llm:01` | `provider\|model\|cd` | `zhipu\|GLM-4-Flash\|0` |
| `llm:02` | `provider\|model\|cd` | `siliconflow\|Qwen/Qwen3.5-4B\|15` |
| `llm:03` | `provider\|model\|cd` | `siliconflow\|deepseek-ai/DeepSeek-R1-Distill-Qwen-7B\|15` |

### Embed/Rank 配置

| Key | 说明 | 示例 |
|-----|------|------|
| `embed:provider` | Embeddings 提供商 | `Local1` |
| `embed:model` | Embeddings 模型 | `bge-large-zh-v1.5-q8_0` |
| `rank:provider` | Rerank 提供商 | `Local2` |
| `rank:model` | Rerank 模型 | `qwen3-reranker-0.6b-q8_0` |

### 全局配置

| Key | 说明 | 默认值 |
|-----|------|--------|
| `llm:select` | 当前主 LLM 编号 | `01` |
| `llm:config:switch_threshold` | 切换阈值 | `10` |
| `llm:config:cool_down` | 默认冷却期(秒) | `60` |
| `llm:initialized` | 初始化标记 | `1` |

### 运行时状态

| Key | 说明 | 示例 |
|-----|------|------|
| `llm:count:01` | 主 LLM 调用次数 | `15` |
| `llm:cool-down:02` | 备用冷却截止时间戳 | `1712246400` |
| `llm:tokens:{upstream}` | Token 统计 | `12345` |
| `llm:errors:{upstream}` | 错误计数 | `3` |

## 管理命令

### 查看配置

```bash
# 查看所有提供商
redis-cli -p 7379 KEYS "provider:*"

# 查看所有 LLM
redis-cli -p 7379 KEYS "llm:*"

# 查看具体配置
redis-cli -p 7379 GET "provider:siliconflow"
redis-cli -p 7379 GET "llm:01"
```

### 修改配置

```bash
# 添加提供商
redis-cli -p 7379 SET "provider:newprovider" "https://api.example.com/v1|sk-xxx"

# 添加 LLM
redis-cli -p 7379 SET "llm:04" "newprovider|model-name|30"

# 切换主 LLM
redis-cli -p 7379 SET "llm:select" "02"

# 修改切换阈值
redis-cli -p 7379 SET "llm:config:switch_threshold" "20"

# 重置调用计数
redis-cli -p 7379 SET "llm:count:01" "0"
```

### 调试

```bash
# 查看 debug 信息
redis-cli -p 7379 GET "debug:rewrite_path"
redis-cli -p 7379 GET "debug:baseurl"
```

## 配置初始化流程

1. 启动时检查 `llm:initialized` 键
2. 如果未初始化，从 `config.lua` 读取配置写入 Redis
3. 后续启动跳过初始化（保护 Redis 中的运行时修改）

### 重新初始化

```bash
# 清除初始化标记，下次启动时重新加载 config.lua
redis-cli -p 7379 DEL "llm:initialized"
```

## 配置热更新

修改 `lua/router.lua` 后自动重新加载，但 `config.lua` 中的配置不会自动同步到 Redis。

手动同步方式：

```bash
# 方式1: 删除初始化标记，重启服务
redis-cli -p 7379 DEL "llm:initialized"
# 重启 llm-gateway

# 方式2: 直接修改 Redis
redis-cli -p 7379 SET "llm:01" "zhipu|GLM-4-Flash|0"
```

## Code 路由配置 (端口 9089)

### 设计说明

端口 9089 使用 `code:` 前缀，URL 精确匹配包含 "code" 字样的路径。

**URL 匹配规则**: 路径中包含 "code" 字样

示例：
- `/v1/code/chat/completions` → 匹配
- `/api/code/embeddings` → 匹配
- `/code/test` → 匹配

**选择机制**: 通过 `code:select` 指定当前使用的序号（与 `llm:select` 逻辑相同）

### Code 配置

| Key | 格式 | 示例 |
|-----|------|------|
| `code:select` | 序号 | `01` |
| `code:01` | `provider\|model\|opt` | `zhipu\|GLM-4-Flash\|01` |
| `code:02` | `provider\|model\|opt` | `openai\|gpt-4o\|02` |

**字段说明**:
- `provider`: 提供商名称（复用 `provider:*` 配置）
- `model`: 模型名称
- `opt`: 选项编号，引用 `opt:*` 配置（空表示无选项）

### Opt 选项配置

| Key | 值 | 说明 |
|-----|------|------|
| `opt:01:max_tokens` | `999` | 设置 max_tokens |
| `opt:01:temperature` | `0.7` | 设置 temperature |
| `opt:01:stream` | `true` | 设置 stream |
| `opt:02:max_tokens` | `800` | 另一组配置 |
| `opt:02:top_p` | `0.9` | 设置 top_p |

**格式**: `opt:{编号}:{JSON字段名}` → `{值}`

**工作原理**:

当请求 `/v1/code/chat/completions` 时：
1. URL 包含 "code" → 走 code 路由
2. 查询 `code:select` → `01`
3. 查询 `code:01` → `zhipu|GLM-4-Flash|01+02`
4. 解析 opt = `01+02`（多个选项用 `+` 分隔）
5. 查询 `opt:01:*` 获取所有配置项
6. 查询 `opt:02:*` 获取所有配置项
7. **重建请求体**：用配置参数替换原请求中的对应字段
8. 转发到 zhipu

**请求体重建示例**:

原请求体：
```json
{
  "model": "some-model",
  "messages": [...],
  "max_tokens": 100,
  "temperature": 1.0
}
```

配置：
```
opt:01:max_tokens → 999
opt:01:temperature → 0.7
opt:02:stream → true
```

重建后请求体：
```json
{
  "model": "GLM-4-Flash",
  "messages": [...],
  "max_tokens": 999,
  "temperature": 0.7,
  "stream": true
}
```

**关键点**: 用配置参数**重建**请求体，覆盖原请求中的对应字段。

### Code 统计

| Key | 说明 |
|-----|------|
| `code:{code}:calls` | 调用次数 |
| `code:{code}:prompt` | Prompt Token 累计 |
| `code:{code}:completion` | Completion Token 累计 |
| `code:{code}:total` | 总 Token 累计 |

### Code 全局配置

| Key | 说明 | 默认值 |
|-----|------|--------|
| `code:initialized` | 初始化标记 | `1` |

### 管理命令

```bash
# 添加 code 配置
redis-cli -p 7379 SET "code:test123" "zhipu|GLM-4-Flash|"

# 查看所有 code
redis-cli -p 7379 KEYS "code:*"

# 查看 code 统计
redis-cli -p 7379 GET "code:test123:calls"
```

---

## 配置示例

### 多提供商负载均衡

```bash
# 提供商
redis-cli -p 7379 SET "provider:openai" "https://api.openai.com/v1|sk-xxx"
redis-cli -p 7379 SET "provider:anthropic" "https://api.anthropic.com|sk-ant-xxx"
redis-cli -p 7379 SET "provider:local" "http://127.0.0.1:8080|"

# LLM 配置
redis-cli -p 7379 SET "llm:01" "openai|gpt-4o|0"
redis-cli -p 7379 SET "llm:02" "anthropic|claude-3-5-sonnet-20241022|60"
redis-cli -p 7379 SET "llm:03" "local|qwen2.5-7b|30"

# 切换阈值
redis-cli -p 7379 SET "llm:config:switch_threshold" "50"
```

### 纯本地模型

```bash
redis-cli -p 7379 SET "provider:local" "http://127.0.0.1:3333|"
redis-cli -p 7379 SET "llm:01" "local|qwen2.5-7b-instruct|0"
redis-cli -p 7379 SET "llm:select" "01"
```
