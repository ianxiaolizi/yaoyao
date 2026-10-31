---
title: 模型自适应上下文预算设计
status: approved
owner: 李道权
created: 2026-10-31
---

# 模型自适应上下文预算设计

## 1. 问题

爻爻当前把不同层级的预算混成了同一组常量：

- 默认工作集是 `8,000` input tokens；
- 默认 completion cap 是 `8,000`，且 reasoning tokens 计入其中；
- OpenAI-compatible 请求固定使用 `120,000 ms` 总超时；
- 压缩阈值是 `(max_input_tokens - reserved_output_tokens) × 75%`。

2026-10-31 的真实日志已经证明这个组合会截断思考：`G:\yaoyao-work\.agent\web-20261031152504-ddax\events.jsonl` 第 96 行一次请求使用 `5,010` input、`8,000` output、`8,000` reasoning，正文为空，`finish_reason=length`；第 101 行以 `max_output_tokens` 结束。

这不是模型窗口不足，而是 harness 把 completion cap 当成了思考上限。

## 2. 目标

1. 从模型能力自动派生工作集、输出上限、压缩触发点、保留量和 reasoning effort。
2. 显式配置可以覆盖自动值，但不能超过模型物理能力。
3. reasoning 不再因固定的 8k output cap 被截断。
4. 累积 token 和 run wall clock 只在 step 边界生效，不中断正在生成的请求。
5. provider 流式请求使用首事件超时和空闲超时，而不是固定总超时。
6. 每个解析结果都可审计：模型能力、预算值和来源写入 profile snapshot。

## 3. 非目标

- 本计划不实现基于历史语料的自动调参学习。
- 本计划不改变现有 `state/1`、stall、completion criterion 的语义。
- 本计划不把上下文窗口自动填满到模型上限；工作集仍然是成本/延迟策略。
- 本计划不引入多 provider 混跑；一个进程仍使用一个 provider kind。

## 4. 模型能力

新增 provider-neutral 类型：

```rust
pub struct ModelCapabilities {
    pub context_window_tokens: u32,
    pub max_output_tokens: u32,
    pub reasoning_efforts: Vec<ReasoningEffort>,
    pub default_reasoning_effort: Option<ReasoningEffort>,
}

pub struct ResolvedModelCapabilities {
    pub capabilities: ModelCapabilities,
    pub source: ModelCapabilitySource,
}

pub enum ModelCapabilitySource {
    Provider,
    Bundled,
    Fallback,
}

pub enum ReasoningEffort {
    Off,
    Low,
    High,
    Max,
}
```

能力来源优先级：

```text
显式 profile 覆盖
  > provider /models
  > 内置模型目录
  > 保守回退
```

DeepSeek V4 Flash 的已知能力为：

```text
context_window_tokens = 1,048,576
max_output_tokens    = 393,216
reasoning_efforts    = low, high, max
default_reasoning    = high
```

未知模型回退为：

```text
context_window_tokens = 131,072
max_output_tokens    = 32,768
reasoning_efforts    = low, high
default_reasoning    = high
```

回退必须在审计来源中标明 `fallback`，不能伪装成 provider 事实。

## 5. 预算派生

设：

```text
W = model.context_window_tokens
O = completion cap，包含 reasoning
B = compaction headroom
I = input working set
C = compaction trigger
R = retained recent tokens
P = preset ratio
```

派生公式：

```text
O = min(model.max_output_tokens, floor(0.25 × W))
B = max(32_768, floor(0.0625 × W))
I = min(floor(P × W), floor(0.95 × W) - O)
C = floor(min(0.8 × W, 0.8 × I, W - O - B))
R = min(floor(0.16 × (W - O)), floor(0.5 × I))
```

Preset：

| preset | P |
|---|---:|
| `frugal` | 0.125 |
| `balanced` | 0.25 |
| `deep` | 0.5 |
| `max` | 0.75 |

对 DeepSeek V4 Flash 和 `balanced`：

```text
W = 1,048,576
O = 262,144
B = 65,536
I = 262,144
C = 209,715
R = 125,829
```

显式覆盖：

- `input_working_set_tokens` 覆盖 `I`；
- `completion_cap_tokens` 覆盖 `O`；
- `compaction_headroom_tokens` 覆盖 `B`；
- `retain_recent_tokens` 覆盖 `R`；
- `compact_at_percent` 覆盖 `0.8` 这个触发比例。

任何显式值必须满足：

```text
I > 0
O > 0
I + O <= floor(0.95 × W)
O <= model.max_output_tokens
0 < C <= I
0 <= R < C
```

自动值违反约束时收缩到合法范围；显式值违反约束时拒绝启动并说明字段。

## 6. reasoning effort

- profile 显式值优先；
- 否则使用模型目录的 `default_reasoning_effort`；
- 再否则使用 `high`；
- provider 不支持显式值时拒绝启动；
- provider 不声明 reasoning 支持时省略该字段，不发送伪造值。

`max_output_tokens` 和 `reasoning effort` 是两个控制量：

- effort 控制思考深度；
- completion cap 只防止失控输出，不能用于限制正常思考。

发送到 OpenAI-compatible `/chat/completions` 的字段名为 `reasoning_effort`。

## 7. 流式超时

替换固定 120 秒总超时：

```text
first_event_timeout_ms = 60_000
stream_idle_timeout_ms = 300_000
```

- 连接后等待响应头或首个事件，超过首事件超时则失败；
- 每个后续流 chunk/event 重置 idle watchdog；
- 只要事件持续到达，请求可以运行超过 300 秒；
- run wall clock 只在 step 边界检查；
- `max_total_tokens` 只在 step 边界检查。

## 8. 输出截断恢复

当满足以下全部条件时：

```text
finish_reason = length
content_blocks 中没有 text 或 tool_use
reasoning_tokens == output_tokens
该 step 尚未执行 output recovery
```

执行一次恢复：

1. 优先把 completion cap 翻倍，最高到 `model.max_output_tokens`；
2. 若 cap 已经等于模型上限，则把 reasoning effort 降低一档（`max -> high -> low`）；
3. 使用 `NullSink` 重试，避免重复 live reasoning；
4. 只持久化第二次成功响应的 settled content；
5. 第二次仍被截断时，以 `MaxOutputTokens` 结束，并把 verdict 描述为 reasoning budget exhausted，而不是普通答案。

恢复次数固定为一次，不递归。

## 9. 上下文压缩

压缩决策必须使用完整请求的估计：

- stable system prompt；
- state/task/identity/budget notices；
- settled history；
- 当前输入；
- 本轮工具流量；
- tool definitions；
- 可选图片/attachment 估计。

优先顺序：

1. `state/1` settlement，无模型调用；
2. 重复工具结果去重、超长工具结果 head/tail 裁剪；
3. 完整消息组摘要；
4. provider context overflow 后做一次最大安全压缩并重试一次。

压缩不能拆开 tool call 和 tool result，也不能移除当前用户输入。

## 10. 审计与兼容

`profile/resolved` 增加：

```json
{
  model_capabilities: {
    openai-compat/deepseek-flash: {
      context_window_tokens: 1048576,
      max_output_tokens: 393216,
      default_reasoning_effort: high
    }
  },
  budget_sources: {
    context_window_tokens: provider:/models,
    input_working_set_tokens: preset:balanced,
    completion_cap_tokens: derived:0.25W,
    reasoning_effort: model_default
  }
}
```

旧字段 `max_input_tokens` 和 `max_output_tokens` 在配置读入时作为新字段的别名接受；`reserved_output_tokens` 仅保留反序列化兼容，不再参与预算计算并在解析时产生 deprecation note。

每个 run 在开始时冻结预算。运行中不因新的 `/models` 响应或统计变化重算当前 run。

## 11. 验收

1. 1M 窗口、393,216 max output 的模型自动得到 `262,144 / 262,144 / 209,715`，除非显式覆盖。
2. 无显式预算的 1M 模型不会再出现 8k completion cap。
3. 8,000 reasoning 全部消耗后，回合会重试更大 cap，而不是立刻以空正文结束。
4. 持续产生事件的流可以超过 120 秒；300 秒无事件的流仍会超时。
5. 累积 token/run wall clock 不会中断 in-flight 请求。
6. 所有 profile snapshot 都能读出预算来源。
7. 旧配置和旧 snapshot 仍可读入。
8. 全量门通过：`powershell -ExecutionPolicy Bypass -File scripts\test.ps1`。
