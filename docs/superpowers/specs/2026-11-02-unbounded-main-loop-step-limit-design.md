# 爻爻主循环取消固定步数上限设计

> 日期：2026-11-02
> 状态：设计方向已确认，待用户评审规格
> 范围：配置解析、RunBudget、Scheduler、AgentLoop 上下文、停车记录、子代理结果、Web 可读状态与验收测试
> 外部参考：Claude Code 2.1.316 源码快照、DeepSeek Harness 源码快照
> 关联问题：爻爻在正常长任务中因 `max_steps` 用尽而停车，用户需要手动追问才能继续

## 1. 结论

爻爻不再把“步数”作为主循环的主要预算。

新的语义是：

- 配置省略 `budget.max_steps` 时，主循环没有固定的步数上限。
- 显式配置 `budget.max_steps = N` 时，仍然在第 N 步停车，保留现有精确上限能力。
- `budget.max_steps = 0` 继续作为非法配置拒绝。
- 不再在 `RunBudget::default()` 中注入 8 步。
- 不再在存在 Token 或墙钟预算时偷偷注入 300 步。
- 用户取消、停滞检测、显式工具调用上限、Token 预算、墙钟预算、模型完成、模型错误和内容过滤仍然是合法停止条件。
- 步数继续计数并进入日志、指标和上下文，但不再是默认的终点。

这借鉴两类外部设计：

| 设计 | Claude Code | DeepSeek Harness |
| --- | --- | --- |
| 主循环 | 主会话没有必须配置的步数硬上限；`maxTurns` 是 agent 局部能力 | `agent-loop` 用 `while (turn)` 持续运行，没有 `maxSteps` 字段 |
| 停止条件 | 正常完成、取消、错误、可选 USD 预算、后台任务控制 | 正常完成、`max-tokens`、blocked、错误、用户取消 `AbortSignal` |
| 到达局部上限 | 子代理标记为 partial，并提示继续，而不是伪装成完成 | 通过持久化会话、取消和恢复处理，不把步数当主预算 |
| 安全阀 | 后台并发、预算、权限、可恢复会话 | 上下文压缩、工具超时、取消、持久化 |

## 2. 当前实现与根因

当前代码事实：

- `crates/agent-config/src/layers.rs` 的 `BudgetSpec.max_steps` 已经是 `Option<u32>`，可以表达“没有写这个字段”。
- `crates/agent-protocol/src/profile.rs` 的 `RunBudget.max_steps` 却是 `u32`，`Default` 固定为 8。
- `crates/agent-config/src/resolve.rs` 在存在 `max_total_tokens` 或 `wall_clock_timeout` 且没有显式步数时，把 `max_steps` 改成 300。
- `crates/agent-kernel/src/scheduler.rs` 无条件执行 `if steps >= self.max_steps`。
- `crates/agent-kernel/src/parking.rs` 计算停车百分比时要求一个具体的 `max_steps`。
- `crates/agent-ports/src/agent_loop.rs` 的 `TurnStateFacts.max_steps` 是 `u32`，循环因此总把“最多 N 步”写进上下文。
- `crates/agent-loop/src/context_layers.rs` 与 `work_state.rs` 会把“N of M steps”告诉模型。
- `crates/agent-protocol/src/reason.rs` 的 `Situation.max_steps` 是 `u32`。
- `apps/agent/src/inprocess.rs` 把子代理的 `MaxSteps` 翻译为 `Failed`，即使子代理已经有部分输出。

因此当前问题不是配置值太小，而是“步数”被建模成必填的主预算。升高数值只能延后问题，不能消除错误语义。

## 3. 目标与非目标

### 3.1 目标

1. 主对话默认不再因固定步数上限停止。
2. 显式 `max_steps` 继续可用，并与旧日志和旧配置保持读取兼容。
3. 删除隐性默认 8 步和隐性回退 300 步。
4. 保留所有已有的安全停止条件，不取消取消、停滞、Token、墙钟、工具调用和错误处理。
5. 子代理显式到达步数上限时返回 `Partial`，不把已有输出说成完整答案。
6. 上下文、停车记录、Web 文案和指标都能表达“没有固定步数上限”。
7. 用测试钉住长任务超过 300 步仍能继续，以及显式上限仍然精确生效。

### 3.2 非目标

1. 不取消 `max_tool_calls`、`max_total_tokens`、`wall_clock_timeout`。
2. 不取消 `stall_steps` 停滞检测。
3. 不新增默认墙钟或默认 Token 预算。
4. 不改变模型错误、内容过滤、用户取消和完成判据的现有行为。
5. 不开放无限子代理并发或无限委派深度。
6. 不重做 Web UI 的整体视觉设计。
7. 不实现子代理 provider 的“同一子会话原位续跑”协议；本轮只保证 partial 结果携带子会话身份和可继续的判断依据。

## 4. 行为语义

### 4.1 配置

| 配置 | 语义 |
| --- | --- |
| 省略 `max_steps` | 无固定步数上限 |
| `max_steps = 1` | 只允许 1 步，第 2 步边界前停车 |
| `max_steps = 20` | 最多 20 步，保持现有 `max_steps` 停止原因 |
| `max_steps = 0` | 配置错误，拒绝启动 |
| 省略 `max_tool_calls` | 无累计工具调用上限 |
| 显式 `max_tool_calls = N` | 第 N+1 次调用前停止，现有行为不变 |

存在 `max_total_tokens` 或 `wall_clock_timeout` 时，不再自动生成 `max_steps = 300`。这两个预算自身负责停止；步数只作为诊断计数。

### 4.2 停止原因

正常运行时，AgentLoop 在以下情况停止：

- 模型给出最终答案：`final_answer`
- 用户取消：`user_cancelled`
- 连续无新证据达到停滞阈值：`stalled`
- 显式 `max_steps` 达到：`max_steps`
- 显式 `max_tool_calls` 达到：`max_tool_calls`
- 显式墙钟预算达到：`wall_clock_exceeded`
- 显式累计 Token 预算达到：`token_budget_exceeded`
- 单次输出上限：`max_output_tokens`
- 内容过滤：`content_filtered`
- 模型或工具致命错误：`fatal_error`
- 需要用户输入或审批：`needs_user_input`
- 完成判据通过：`completed`

“步数很多”本身不是停止原因。`stall_steps` 仍然会在反复做同一件事、没有新证据时停止，防止无限空转。

### 4.3 显式上限停车

显式 `max_steps` 到达时，现有停车语义保持不变：

- 写入 `turn/completed`，`stop_reason = "max_steps"`。
- 写入 `budget_parked`。
- 保留 plan、work state、剩余目标、下一步和证据引用。
- 不把本轮输入标记为已完整回答。
- 用户继续对话时开启新 run，使用新预算。

默认无步数上限的运行不会产生 `max_steps` 停车记录。

## 5. 架构与数据流

### 5.1 配置解析

`BudgetSpec` 保持现有形状。`apply_budget` 只在显式写有 `max_steps` 时设置 `RunBudget.max_steps = Some(steps)`。

`resolve::budget` 删除：

- `DEFAULT_BACKSTOP_STEPS`
- `named_steps`
- 由 `primary_limit` 触发步数回填的分支

`finalize_at_percent`、`settle_at_percent`、`stall_steps` 和 `cache_hit_weight_percent` 的默认逻辑保持不变。`finalize_at_percent` 仍要求存在 Token 或墙钟主预算。

### 5.2 协议与持久化

`RunBudget.max_steps` 改为 `Option<u32>`：

- `None`：无固定步数上限。
- `Some(n)`：显式上限。
- 新写入的 `profile/resolved` 始终保留 `max_steps` 字段；`None` 写为 JSON `null`，不写成 0。
- 旧快照中的数字 `max_steps` 继续反序列化为 `Some(n)`。
- 旧快照缺失 `max_steps` 时使用兼容默认 `Some(8)`，因为字段缺失只能来自“默认 8 步”的旧版本；不能把它追溯解释成无限。
- 新快照中的 JSON `null` 明确反序列化为 `None`。

实现上，`RunBudget.max_steps` 使用字段级旧版默认函数（缺字段返回 `Some(8)`），而不是普通 `Option::default()`。这样旧历史不漂移，新无限语义也不需要借用 0。

`TurnStateFacts.max_steps` 改为 `Option<u32>`。`Situation.max_steps` 同步改为 `Option<u32>`，并始终显式序列化 `null`，避免读取者把缺失字段误当旧默认 8。

### 5.3 Scheduler

`Scheduler::new` 接收 `Option<u32>`。

步数边界改为：

- `Some(limit)`：`steps >= limit` 时以 `MaxSteps` 结束。
- `None`：该判断永不触发，循环继续到其他停止条件。

`steps` 继续累加并写入步骤事件、终止统计和指标。计数器格式不变，避免引入无必要的持久化迁移。

### 5.4 停车百分比

`parking::percent_of` 接收 `Option<u32>`：

- 有显式步数上限时，计算步数百分比。
- 没有显式步数上限时，步数贡献为 0。
- 有 Token 预算时，继续按 Token 使用率计算。
- 两者都没有时，percent 为 0；该值只用于描述停车，不参与停止判断。

`stalled`、`max_tool_calls`、`wall_clock_exceeded` 等停止原因仍按现状记录，不把“无步数上限”伪装成 100%。

### 5.5 AgentLoop 上下文

当 `max_steps` 为 `Some(n)`：

- 任务层继续写“This run allows at most N model steps.”
- work state 继续写“step X of N”。

当 `max_steps` 为 `None`：

- 任务层写“This run has no fixed model-step ceiling; finish when the task is done or another stated limit or stop condition applies.”
- work state 写“step X”，不写“of N”。
- 不向模型暗示存在一个隐藏的默认值。

停滞提示和预算提示保持现有优先级。

### 5.6 Web 与可观测性

- `max_steps: null` 或字段缺失显示为“不限”，不得显示为 0，也不得触发 `max_steps` 文案。
- 显式 `max_steps` 停车继续显示现有“达到步数上限/已暂停”文案。
- 长任务指标继续展示实际步数，供成本与异常分析。
- `profile/resolved`、终止事件和指标中的步数字段继续可读。
- 新增日志/上下文测试，确保无上限运行不会在 8、20 或 300 步处产生伪停车。

### 5.7 子代理结果

子代理显式配置 `max_steps` 并到达上限时：

- `StopReason::MaxSteps` 映射为 `SubagentOutcome::Partial`，不再映射为 `Failed`。
- summary 必须包含：子代理达到显式步数上限、输出可能不完整、实际达到的步数、`child_session_id`，以及父代理可以继续或改派的事实。
- 如果子代理没有最终文本，summary 仍明确说明“没有可交付文本，但子代理曾执行工作”，不得声称完成。
- 其他预算停止（工具调用、墙钟、Token、输出上限）本轮保持 `Failed`，避免扩大语义范围。
- `SubagentOutcome::Partial` 的文档改为：既可以由子代理自报，也可以由父侧在显式步数上限造成可验证的部分结果时合成。
- 现有 `subagent/spawned`、settlement payload 和 `child_session_id` 足以让父代理定位子会话；本轮不新增 provider 原位 resume 方法。

## 6. 兼容性与迁移

### 6.1 旧配置

- 旧配置中显式写有 `max_steps` 的运行保持原上限。
- 旧配置省略 `max_steps` 的运行将从“默认 8 步”变为“无固定上限”；这是本次有意的破坏性行为变化，必须在 `CONTRACT.md` §7 新增裁决并更新旧默认说明。
- `0` 仍然拒绝，不能把 0 当成无限。
- 用户当前工作区配置若仍保留 `max_steps = 20`，需要删除该行才会获得无限步数；`max_tool_calls = 40` 仍是独立上限，也应删除或调高。

### 6.2 旧事件与快照

- 数字 `max_steps` 继续可读为 `Some(n)`。
- 缺失 `max_steps` 的旧快照读取为 `Some(8)`，保持旧运行语义。
- 新代码读取旧日志时不追溯改写。
- 新代码写出的无上限预算写为 `"max_steps": null`，不再依赖字段缺失表达无限。
- 旧 Web/外部读取者必须能处理 JSON `null`；这比把 0 当无限更明确，也不会与非法配置混淆。

### 6.3 安全边界

- 用户取消仍在下一步边界生效，现有正在执行的一次模型或工具调用不被强行中断。
- `stall_steps` 默认仍为 8，连续无新证据达到两倍阈值时停止。
- Token 与墙钟预算不新增默认值，只对显式配置生效。
- 工具调用上限与权限策略不变。
- 子代理并发、深度、工作区和权限限制不变。

## 7. 测试计划

### 7.1 配置与协议

1. 空配置解析得到 `max_steps = None`。
2. 显式 `max_steps = 4` 解析得到 `Some(4)`。
3. `max_total_tokens` 或 `wall_clock_timeout` 不再补出 300 步。
4. `max_steps = 0` 仍被拒绝。
5. `RunBudget` 的旧数字快照可读为 `Some(n)`；缺字段旧快照读为 `Some(8)`。
6. 新无上限预算序列化为 `"max_steps": null`，并能稳定读回 `None`。

### 7.2 Scheduler 与 AgentLoop

7. `max_steps = None` 的 fake provider 连续执行 320 步后给出最终答案，期间不产生 `max_steps` 停车。
8. `max_steps = Some(3)` 在第 4 步边界前停止，停止原因和停车记录不变。
9. `parking::percent_of` 在无步数上限但有 Token 预算时只按 Token 计算百分比。
10. 无任何数值预算时，`percent_of` 返回 0，不触发除零或虚假 100%。
11. 任务层和 work state 在不限步数时不再写“of N”。
12. 停滞检测仍按原阈值停止。
13. 用户取消、工具调用上限、墙钟预算、Token 预算和完成判据的原有测试保持通过。

### 7.3 子代理与 Web

14. 显式步数上限的子代理返回 `Partial`，summary 含 `child_session_id` 和不完整声明。
15. 无步数上限的子代理不会因默认 8 或 300 步返回 partial/failed。
16. Web 渲染无上限预算时显示“不限”，不显示 0。
17. 显式 `max_steps` 停车仍渲染为暂停/部分结果，而不是完整完成。

### 7.4 回归门

18. `cargo fmt --all -- --check`
19. `cargo clippy --workspace --all-targets -- -D warnings`
20. `cargo test --workspace`
21. 若本机执行门脚本可用，再运行 `scripts\test.ps1`。

## 8. 实施顺序

1. 修改 `RunBudget`、配置 resolve、Scheduler 和 parking，先让核心语义正确。
2. 修改 `TurnStateFacts`、任务层、work state 和 reason 快照。
3. 修改子代理 `MaxSteps` 的 partial 映射。
4. 修改 Web 限额展示和文案。
5. 更新 `CONTRACT.md` §7 的默认步数裁决，更新相关旧测试。
6. 跑完整门，并用一个超过 300 步的 fake 场景做反向实测。

## 9. 验收标准

- 不配置 `max_steps` 时，长任务不会因 8、20 或 300 步停止。
- 显式配置 `max_steps = N` 时，仍在 N 步停车，并保留可继续的 parked 状态。
- 用户取消、停滞、工具调用、Token、墙钟、模型错误和完成判据都能正常停止。
- 子代理达到显式步数上限时显示 partial，而不是 pretending complete。
- 无上限语义在配置解析、日志快照、上下文、Web 和测试中一致，不出现 0 被误解成“无限”的情况。