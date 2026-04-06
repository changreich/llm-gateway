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
