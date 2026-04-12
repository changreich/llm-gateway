# Code 模型监控设计

## 需求

1. 移除 `/running` 页面的 SSE 监控
2. 新增 code 模型（9089 端口）使用量监控

## 当前架构

### LLM 监控 (9090 端口)

```
STATS_TOTAL_CALLS     - 全局调用次数
STATS_TOTAL_PROMPT    - 全局 prompt tokens
STATS_TOTAL_COMPLETION - 全局 completion tokens
STATS_MODELS          - 每个模型的统计 HashMap<String, ModelStats>
STATS_SELECTED        - 当前选中的模型
STATS_CONFIG          - 模型配置 num -> (provider, model)
```

### Code 监控 (9089 端口) - 新增

需要类似的数据结构：

```rust
// Code 模型统计
static CODE_STATS_TOTAL_CALLS: AtomicU64
static CODE_STATS_TOTAL_PROMPT: AtomicU64
static CODE_STATS_TOTAL_COMPLETION: AtomicU64
static CODE_STATS_MODELS: RwLock<HashMap<String, ModelStats>>
static CODE_STATS_SELECTED: RwLock<String>
static CODE_STATS_CONFIG: RwLock<HashMap<String, (String, String)>>
```

## Redis Key 设计

| Key | 格式 | 说明 |
|-----|------|------|
| `code:{num}:calls` | `15` | 模型调用次数 |
| `code:{num}:prompt` | `1234` | 累计 prompt tokens |
| `code:{num}:completion` | `567` | 累计 completion tokens |
| `code:select` | `03` | 当前选中的模型编号 |

## 数据来源

### 调用次数
- Lua 层已经在 `router2.lua` 中统计：`code:{num}:calls`

### Token 统计
- 需要从响应中提取 `input_tokens` 和 `output_tokens`
- OpenAI 格式响应：`usage.prompt_tokens` + `usage.completion_tokens`
- Anthropic 格式响应：`usage.input_tokens` + `usage.output_tokens`

## 实现方案

### 1. Rust 层新增统计数据结构

```rust
// main.rs

/// Code 模型统计数据 (9089 端口)
static CODE_STATS_TOTAL_CALLS: AtomicU64 = AtomicU64::new(0);
static CODE_STATS_TOTAL_PROMPT: AtomicU64 = AtomicU64::new(0);
static CODE_STATS_TOTAL_COMPLETION: AtomicU64 = AtomicU64::new(0);
static CODE_STATS_MODELS: Lazy<RwLock<HashMap<String, ModelStats>>> = Lazy::new(|| RwLock::new(HashMap::new()));
static CODE_STATS_SELECTED: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("01".to_string()));
static CODE_STATS_CONFIG: Lazy<RwLock<HashMap<String, (String, String)>>> = Lazy::new(|| RwLock::new(HashMap::new()));
```

### 2. Lua 函数注册

```rust
// stats_code_add(num, calls, prompt, completion) -> 累加统计
// stats_code_set_config(num, provider, model) -> 设置模型配置
// stats_code_set_selected(num) -> 设置当前选中
```

### 3. router2.lua 改动

在 `on_response` 中提取 token 并调用统计函数：

```lua
function handler.on_response(upstream, status, body)
    -- ... 原有逻辑

    -- 提取 token 统计
    local ok, parsed = pcall(json_decode, body)
    if ok and parsed then
        local input_tokens = 0
        local output_tokens = 0

        -- OpenAI 格式
        if parsed.usage then
            input_tokens = parsed.usage.prompt_tokens or parsed.usage.input_tokens or 0
            output_tokens = parsed.usage.completion_tokens or parsed.usage.output_tokens or 0
        end

        if input_tokens > 0 and current_request then
            stats_code_add(current_request.selected, 1, input_tokens, output_tokens)
        end
    end
end
```

### 4. /running 页面改动

移除 SSE 监控部分，改为显示两个端口的数据：

```html
<div class="card">
  <h2>LLM 模型统计 (9090)</h2>
  <!-- 现有的 LLM 表格 -->
</div>

<div class="card">
  <h2>Code 模型统计 (9089)</h2>
  <!-- 新增的 Code 表格 -->
</div>
```

## 文件改动

| 文件 | 改动 |
|------|------|
| `src/main.rs` | 新增 `CODE_STATS_*` 全局变量和 Lua 函数，修改 `generate_running_html()` |
| `lua/router2.lua` | 在 `on_response` 中调用统计函数 |

## UI 设计

```
┌─────────────────────────────────────────────────────────────┐
│  LLM Gateway                                                 │
├─────────────────────────────────────────────────────────────┤
│  [调用次数: 100] [Prompt: 5000] [Completion: 3000] [总: 8000] │
├─────────────────────────────────────────────────────────────┤
│  LLM 模型统计 (9090)                                         │
│  ┌─────┬──────────┬───────────────────┬──────┬────────┬────┐ │
│  │ 编号│ 提供商   │ 模型              │ 调用 │ Prompt │... │ │
│  ├─────┼──────────┼───────────────────┼──────┼────────┼────┤ │
│  │ 01* │ zhipu    │ GLM-4-Flash       │ 50   │ 2500   │... │ │
│  │ 02  │ silicon  │ Qwen3.5-4B        │ 30   │ 1500   │... │ │
│  └─────┴──────────┴───────────────────┴──────┴────────┴────┘ │
├─────────────────────────────────────────────────────────────┤
│  Code 模型统计 (9089)                                        │
│  ┌─────┬──────────┬───────────────────┬──────┬────────┬────┐ │
│  │ 编号│ 提供商   │ 模型              │ 调用 │ Prompt │... │ │
│  ├─────┼──────────┼───────────────────┼──────┼────────┼────┤ │
│  │ 03* │ qfcode   │ qianfan-code      │ 20   │ 1000   │... │ │
│  │ 04  │ qfacode  │ ernie-4.5-turbo   │ 10   │ 500    │... │ │
│  └─────┴──────────┴───────────────────┴──────┴────────┴────┘ │
└─────────────────────────────────────────────────────────────┘
```

## 实现步骤

1. **main.rs** - 新增 `CODE_STATS_*` 全局变量
2. **main.rs** - 注册 Lua 函数 `stats_code_add`, `stats_code_set_config`, `stats_code_set_selected`
3. **main.rs** - 修改 `generate_running_html()` 移除 SSE，新增 Code 统计
4. **router2.lua** - 在 `on_request` 中调用 `stats_code_set_config` 和 `stats_code_set_selected`
5. **router2.lua** - 在 `on_response` 中提取 token 并调用 `stats_code_add`
