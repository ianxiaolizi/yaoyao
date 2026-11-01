# 爻爻通用高权限 Agent 整改设计（定稿草案）

> 日期：2026-11-01  
> 状态：五条产品决策已定稿，待用户评审后进入实施计划  
> 范围：Agent 内核、任务模型、意图调度、验证、权限、可观测性与发布治理  
> 首期边界：代码/文件任务与只读网络；不开放发送、发布、远端写入等外部副作用  
> 关联证据：`G:\yaoyao-work\.agent\audit-minigit-20261101-run\events.jsonl`

## 1. 结论先行

这次观察到的问题不能归结为“模型某次粗心”。它们来自同一条系统性缺口：

> 爻爻目前擅长生成动作和最终文本，但还没有把“用户最新意图、任务合约、执行证据、权限边界、完成状态”做成内核级事实。

因此整改不应继续堆叠提示词，也不应只针对迷你 Git 任务补测试。需要把爻爻从“会调用工具的对话模型”升级为“有任务状态机、证据门和权限门的高权限执行体”。

整改顺序必须是：

1. **先建立任务真相与证据闭环**，否则权限越高，错误完成越危险。
2. **再建立最小权限与独立验收边界**，避免自证和越权。
3. **最后建立跨任务事故学习机制**，让同类问题进入回归库和发布门，而不是停留在一次复盘。

新增的关键设计决策是：**新用户指令不无条件取代旧任务，也不继续沿用 FIFO。采用“规则预判 + 独立意图仲裁 + 内核策略校验 + 不确定时确认”的软取代机制。**

权限模型同步定为：**主模型自主决定行动，独立权限分类器判断放行、询问或拒绝，内核保留硬否决；用户只处理高风险、越界、冲突或不确定情形。**

首期只开放代码/文件任务和只读网络。发送、发布、远端写入、凭据操作和不可逆系统变更在信任底座与高权限边界通过前不进入产品通道。

## 2. 现象到根因的映射

| 观察到的现象 | 直接原因 | 系统根因 |
| --- | --- | --- |
| 新的安全修复指令没有执行，反而返回上一轮旧报告 | 新指令与旧 `NextTurn` 同时在队列，调度领取了旧项 | 只有 FIFO 收件箱，没有“用户意图版本”和 supersede 语义 |
| 多次用 `node --test ... \| tail`，失败仍显示 shell 退出码 0 | 管道末段覆盖退出码 | 验收依赖模型自由拼接 shell，而不是结构化验证器 |
| 修改测试后继续自测，缺少测试完整性约束 | 测试文件被视为普通工作区文件 | 没有区分“交付物”和“验收装置” |
| 达到 35/35 后继续扩测，最后缺少 README / 收尾 | 计划与预算没有强制子目标闭环 | `plan.update` / `state.update` 是建议性记录，不是状态机 |
| 多轮 `max_steps` 后没有最终答复 | 预算耗尽只终止循环，没有明确的 parked 状态 | 预算治理没有接入任务生命周期 |
| Junction 逃逸由外部审计才发现 | 生成实现后主要靠模型自己写测试自证 | 没有独立的、任务外的验收器与对抗性检查 |
| 临时探针、重复读取、无效 `fs.grep` 消耗步骤 | 没有重复调用检测和任务相关性约束 | 缺少执行效率与预算异常治理 |
| 完成报告把审计目录当作“外部产物” | 操作来源没有被任务状态完整建模 | 缺少任务/动作来源与工作区变更归属 |

关键代码事实：

- `crates/agent-kernel/src/inbox.rs:342-354`：待办按 priority + FIFO 排序，`next_turn` 直接取第一个 `NextTurn`。
- `crates/agent-kernel/src/scheduler.rs:276-301`：`run_pending` 没有“新指令是否已 supersede 旧指令”的判断。
- `crates/agent-protocol/src/profile.rs:306-342`：已有 `CompletionSpec`，但默认未启用，且只读取命令退出码或 HTTP 状态，不足以表达“某个交付物必须存在/测试必须 X/Y 通过/独立验证器必须通过”。
- `plan.update`、`state.update`、`why` 已存在，但目前主要服务于模型自述和解释，不构成强制完成门。

## 3. 设计原则

1. **最新用户意图优先于旧队列。** 会话内新的直接指令必须显式取代、合并或阻断旧任务，不能静默排队。
2. **取代是软取代，不是推倒重做。** 保留已完成进度、证据和产物，只让被新要求推翻的目标与验收项失效。
3. **完成是验证后的事实，不是模型的措辞。** 执行轮次结束、回答交付、结果验证是三种不同事实，不得混用一个 `completed` 表达。
4. **验收装置不可由被测者单方面改动。** 测试、基准、验收脚本和评分规则需要完整性保护或外部复核。
5. **模型拥有行动自主权，不拥有自我授权权。** 主模型可以自由规划和申请能力，但不能批准自己的高风险动作。
6. **权限判断交给独立分类器，最终否决留给内核。** 普通动作由 LLM 自主执行；边界动作由独立权限分类器给出 `allow / ask / deny`；硬边界由内核执行。
7. **高风险动作先确认、可回滚、全审计。** 删除、外发、凭据、持久化、系统设置、安装和权限提升不能被宽泛 session 授权覆盖。
8. **验收独立性按风险分层。** 低风险使用同模型新上下文；高风险使用独立模型、独立进程和独立快照；确定性验证优先于 LLM 验收。
9. **预算耗尽必须进入显式 parked 状态。** 不能静默结束，也不能假装完成。
10. **事故要变成回归测试和策略变更。** 记忆文本只能辅助，不能替代可执行门禁。
11. **模型可以规划，但内核持有真相。** 任务、意图、证据、权限和完成状态由内核事件记录，模型只能提出变更。
12. **意图仲裁器没有执行权。** 它只能判断新消息与当前任务的关系，不能批准权限、删除数据、扩大范围或启动副作用。
13. **首期先覆盖可验证面。** 代码/文件任务和只读网络先进入产品通道；外部副作用在幂等、回执、补偿和独立验收成熟后再开放。

## 4. 目标架构

```text
用户输入
  -> 意图账本（Intent Ledger）
  -> 独立意图仲裁器（Intent Adjudicator）
  -> 任务合约（Task Contract）
  -> Agent Loop
  -> 工具调用
  -> 权限检查（tool.check）
  -> 独立权限分类器（Permission Classifier）
  -> 内核硬边界（Hard Policy Gate）
  -> 沙箱 / egress proxy / 工具网关
  -> 证据账本（Evidence Ledger）
  -> 确定性验收器（Deterministic Verifier）
  -> 独立语义验收器（Independent Semantic Verifier，按风险启用）
  -> 完成门（Completion Gate）
  -> 三轴状态 / UI / 审计导出
```

架构中的三个事实必须分开记录：

- **执行状态**：本轮是否仍在运行、正常结束、被中断或失败。
- **交付状态**：用户是否已经收到结果。
- **验证状态**：结果是否有新鲜、可审计、与当前任务版本绑定的证据。

### 4.1 意图账本

新增任务级意图记录，至少包含：

- `intent_id`
- `session_id`
- `task_id`
- `kind`：`new_task | continuation | correction | supersede | schedule | interrupt`
- `supersedes`：被取代的 intent/item 列表
- `target_turn_id`
- `created_at`
- `source`：用户、调度器、子代理、系统
- `text` 与规范化摘要

约束：

- 一个会话最多一个“用户当前活动任务”。
- 新的直接用户指令与该会话未执行完的旧 `NextTurn` 冲突时，必须经过意图仲裁，显式 supersede、合并、另建任务或询问用户。
- `scheduled` 后台任务拥有独立任务身份，不因普通对话而自动消失。
- `status` 必须返回 `active_task`、`pending_user_inputs`、`latest_intent_id`、`stale_items` 和 `intent_adjudication`。
- 最终答复必须绑定它实际回答的 `intent_id`；若最新 intent 尚未执行，不得把旧回答显示为当前答复。

### 4.2 意图仲裁器

#### 4.2.1 为什么不无条件取代

“新消息一律取代旧任务”语义简单，但成本与误伤都高：

- 用户可能只是追加一个约束，不应让已完成部分全部失效。
- 用户可能只是补充背景，而不是修改目标。
- 用户可能希望旧任务继续在后台排队。
- 多条短消息可能属于同一意图。
- 定时任务和子任务不能被普通对话覆盖。

因此采用条件式仲裁，而不是条件反射式 supersede。

#### 4.2.2 触发条件

只有同时满足以下条件才考虑调用独立意图仲裁器：

- 当前会话存在活动任务、未执行指令或正在运行的轮次；
- 新消息来自用户，而不是工具输出、网页内容或第三方文本；
- 新消息可能改变目标、范围、验收标准或执行优先级；
- 确定性规则无法高置信度路由。

以下情况不调用 LLM：

- 当前没有活动任务：直接建立新任务。
- 消息只是“继续/可以/停止/取消”等明确控制词。
- 用户明确说“另做一件事”“先暂停这个”“取消前一个”。
- 消息属于定时任务、系统恢复、子代理回报。
- 只是确认、表情、无任务含义的寒暄。

#### 4.2.3 输入边界

仲裁调用只获取完成任务关系判断所需的最小上下文：

- 当前任务合约摘要和版本；
- 当前风险评估；
- 最近 1–2 轮用户/助手消息；
- 尚未执行的用户指令摘要；
- 新消息原文；
- 明确标注为不可信的、经过裁剪的必要工具结果摘要（默认不提供完整日志）。

不提供：

- 完整思维链；
- 全部工具输出；
- 密钥、凭据或敏感数据；
- 可被第三方内容操纵的未隔离文本；
- 当前运行的全部工作区文件。

#### 4.2.4 输出契约

意图仲裁器只能输出严格 JSON：

```json
{
  "relation": "continue | revise_task | new_task | stop | schedule | unclear",
  "target_task_id": "task-id-or-null",
  "confidence": 0.0,
  "preserve": ["progress", "evidence", "artifacts"],
  "invalidate": ["acceptance_check_id"],
  "scope_expansion": false,
  "requires_confirmation": false,
  "reason": "short rationale"
}
```

限制：

- `confidence` 必须由模型给出，但内核不能只按模型自报值放行；还要结合规则命中、任务风险、是否存在待执行旧项做联合判断。
- `scope_expansion=true` 不能被模型自动批准。
- `reason` 只用于审计和 UI，不作为执行授权。
- 输出解析失败、超时、置信度不足或关系未知时，进入 `unclear` 路径。

#### 4.2.5 判定与执行矩阵

| 仲裁结果 | 风险 | 内核行为 |
| --- | --- | --- |
| `continue` | 低 | 在当前任务上继续，不新建任务 |
| `revise_task` | 低且无扩权 | 创建任务新版本，保留兼容的进度与证据 |
| `revise_task` | 中高或扩权 | 暂停并重新审批，不自动执行 |
| `new_task` | 低 | 旧任务保留为 paused/queued，新任务新建 |
| `new_task` | 高 | 新建任务但先等待用户确认风险与范围 |
| `stop` | 任意 | 取消当前待执行动作，保留已有证据 |
| `schedule` | 任意 | 建立独立定时任务，不取代当前任务 |
| `unclear` | 任意 | 不执行副作用，向用户提出一个最小澄清问题 |

#### 4.2.6 软取代语义

`revise_task` 和 `supersede` 都不是删除旧任务：

- 旧的未执行指令标记为 `superseded`。
- 已完成的子目标、工具结果、证据和产物保留，并标注“来自旧任务版本”。
- 新建任务版本，记录 `based_on_task_revision`。
- 只重新计算被新目标或新验收标准影响的步骤。
- 旧证据没有失效时继续可用，但必须重新检查它是否满足新版本验收标准。
- 用户可以在历史中看到“哪些结论被保留、哪些被推翻”。

#### 4.2.7 模型独立性与成本控制

设计目标：**一次新消息最多一次仲裁调用，而不是每个工具步骤调用一次。**

- 确定性规则优先，多数明显情况不调用模型。
- 对连续短消息做 500–1000ms 合并，只对合并后的消息批次仲裁一次。
- 普通语义冲突使用低延迟独立路由；可以与主执行模型同模型，但必须独立调用、独立上下文，不得继承执行会话历史。
- 高风险范围变化、取消高风险动作、扩权或验收标准变化使用不同模型或独立进程。
- 仅在存在活动任务/待执行项时触发，空闲会话不触发。
- 分类结果和提示词版本可缓存、可审计，但任务状态变化后不得盲目复用。
- 若当前任务正在执行，仲裁可以与当前步骤并行；结果只在下一个安全步骤边界生效。
- 调用失败、超时、解析失败或低置信度默认进入 `unclear`，暂停副作用并请求确认，而不是猜测。

这里的成本应理解为“避免一次错误旧任务执行和返工”的投资，不应追求把所有语义问题都交给分类器。

#### 4.2.8 安全与审计

- 仲裁器没有工具执行权、文件写权限和审批权限。
- 仲裁器始终使用独立调用和独立上下文，不得进入主执行会话继续推理。
- 输入端明确区分用户文本与工具/网页等不可信内容。
- 记录模型版本、路由身份、提示词哈希、输入摘要哈希、输出 JSON、触发规则、耗时和最终内核决策。
- 将“模型建议”和“内核决定”分别记录，便于事后判断是分类错误还是策略错误。
- 高风险任务的仲裁模型或进程与执行模型分离；同模型只允许在普通、低风险场景使用。
- 所有自动软取代都必须可回滚到上一任务版本。

### 4.3 任务合约

复杂任务在首次执行前生成结构化合约：

- 目标与非目标
- 风险等级
- 允许的路径、命令、网络和外部通信范围
- 验收标准列表
- 验收独立性等级：确定性 / 同模型新上下文 / 独立模型与进程 / 人工复核
- 必须运行的验证命令或独立验收器
- 预算（步骤、token、时间、工具调用）
- 交付物与证据要求
- 允许修改的“验收装置”清单
- 动作能力请求摘要：工具、程序、参数、路径、网络、数据范围

合约必须有版本号。任何扩大范围、修改验收标准、新增高风险能力或改变验收独立性，都产生新版本，并重新经过意图仲裁、权限分类器和必要时的用户确认。

### 4.4 证据账本与完成状态

每条证据至少记录：

- `check_id`
- `task_id`
- `task_revision`
- `kind`：`deterministic | model_review | human_review | receipt`
- `status`：`passed | failed | skipped | error | human_review`
- `program` / `args` / `cwd`
- `exit_code`
- `stdout_sha256` / `stderr_sha256`
- `artifact_refs`
- `started_at` / `finished_at`
- `environment_hash`
- `verifier_identity`
- `verifier_model` / `verifier_process`
- `verifier_context_hash`
- `input_manifest_hash`
- `authorization_source`

状态分为三个正交轴：

- `run_status`：`in_progress | completed | interrupted | failed`
- `delivery_status`：`delivered | unanswered`
- `verification_status`：`verified | unverified | stale | not_applicable | blocked`

`completion_status` 只作为派生结果：

- `verified_complete`：完成门要求的所有证据通过，且证据产生于当前任务版本的最后一次有效变更之后。
- `blocked_or_parked`：失败、预算耗尽、权限不足、验证器异常或等待输入。
- 不再提供用户可见的 `unverified_complete`。

纯信息、低风险、无持久化副作用的回答可以显示为“结果已交付”，同时 `verification_status=not_applicable`。这不是任务完成，不计入完成率，也不能作为后续副作用授权的依据。

任何代码、文件或工具产生持久化变更后，旧证据立即变为 `stale`，必须重新验证。

### 4.5 独立验收器

验收按风险分层，确定性证据始终优先：

1. **确定性验收器**
   - 测试、构建、类型检查、静态检查、结构校验、产物存在性、退出码和测试计数。
   - 直接执行结构化的 `program + args + cwd`，不依赖模型自由拼接 shell。
2. **低风险语义验收**
   - 同模型，但必须使用全新上下文和独立调用。
   - 只读权限、独立工作目录或快照。
   - 只接收用户需求、任务合约、验收标准、diff、产物和证据账本。
3. **中高风险语义验收**
   - 使用不同模型或不同供应商路由。
   - 独立进程、独立工作目录、只读工具、无写入、无网络。
   - 记录 `reviewer_identity`、模型版本、提示词哈希和上下文清单哈希。
4. **安全或不可逆任务**
   - 增加独立对抗验证器。
   - 严重发现默认按“未被驳倒即阻塞”处理。
   - 验证器超时、解析失败、模型不可用或返回空结论时 fail closed。

验收器不得把执行模型的最终自述、完整思维链或“我已经完成”的文本当作证据。它只能基于任务合约、用户需求、产物、diff、命令结果和证据账本判断。

测试、基准、验收脚本和评分规则被修改时，必须标记 `verification_integrity_changed`，并从独立副本重跑。

### 4.6 权限与风险

风险等级 L0–L3 保留为内核内部风险信号，不作为要求用户理解的权限套餐：

- **L0 只读**：读取工作区、运行只读检查、只读网络查询。
- **L1 可恢复写入**：任务工作区内编辑、生成临时文件。
- **L2 外部副作用**：网络写入、安装依赖、修改系统状态、调用外部服务。
- **L3 高风险不可逆**：删除、凭据、权限提升、外发敏感数据、持久化系统变更。

权限决策链：

```text
主模型提出动作
  -> tool.check（allow / ask / deny）
  -> 独立权限分类器（判断是否符合用户意图与任务范围）
  -> 内核硬边界（不可绕过的 deny floor）
  -> 必要时询问用户
  -> 沙箱 / 工具网关执行
```

约束：

- 主执行模型拥有行动自主权，但不能批准自己的 L2/L3 动作。
- 权限分类器只输出 `allow / ask / deny`，没有工具执行权，不能修改内核规则。
- 普通权限判断使用独立调用、独立路由和新上下文；高风险使用不同模型或独立进程。
- 以下情况直接要求用户确认：不可逆删除或大范围破坏、凭据读取、权限提升、敏感数据外发、超出任务合约的能力扩张、用户明确设置过的禁止边界、分类器无法解释动作摘要。
- L2 默认任务级；用户确认后可形成精确的能力会话租约，租约绑定工具、程序、参数模式、路径、网络目标、数据范围、任务版本和有效期。任何范围变化都必须重新判断。
- L3 永远逐动作确认，不提供 session 级复用；只有动作摘要完全一致的重试可以复用原审批。
- 自动放行必须记录 `authorization_source=policy | permission_classifier`、动作摘要、理由、路由身份和模型版本。
- 分类器失败、超时或低置信度时进入 `ask` 或 `deny`，不得猜测放行。
- 复用现有 `agent-sandbox-win`，把 local backend 定位为“可信工作区模式”，不作为通用高权限默认。
- 只读网络走 allowlist egress proxy，网页内容标记为不可信输入；敏感凭据由 secrets broker 注入，模型不接触明文。
- 首期不注册发送、发布、远端写入和凭据型外部动作工具。

### 4.7 预算与折返

- 按任务复杂度自动给出预算，而不是全局固定 8 步。
- 预算消耗到 50% / 75% / 90% 时强制写结构化状态并评估剩余目标。
- 检测重复工具调用、重复读取、连续失败探针、测试修改后反复试错。
- 预算耗尽统一进入 `parked`，保留任务合约、已完成项、失败证据和下一步，不生成“完成”答复。
- 用户继续时从 parked 状态恢复，不重新领取旧队列项。

### 4.8 可观测性与复盘

把以下行为作为一等事件：

- `intent_superseded`
- `intent_adjudication_requested`
- `intent_adjudication_completed`
- `intent_adjudication_unclear`
- `permission_decision`
- `permission_classifier_requested`
- `permission_classifier_completed`
- `permission_classifier_unclear`
- `capability_lease_granted`
- `capability_lease_revoked`
- `verification_exit_masked`
- `verification_missing`
- `verification_integrity_changed`
- `verification_reviewer_started`
- `verification_reviewer_completed`
- `completion_claim_rejected`
- `unverified_result_delivered`
- `budget_parked`
- `scope_expanded`
- `scope_denied`
- `approval_escalated`
- `duplicate_tool_cycle`
- `workspace_provenance_conflict`

每个任务结束后生成“自动运行复盘”：

- 哪些动作没有证据
- 哪些指令被延迟或覆盖
- 哪些失败重复出现
- 哪些预算浪费在低价值动作
- 哪些权限请求超出任务范围
- 权限分类器的放行、询问、拒绝分布是否正确
- 意图仲裁是否误判、是否过频调用
- 验收器是否与被测执行器保持了要求的独立性
- 应新增哪些回归用例和策略

复盘结果进入行为回归库；只有通过回归库和评分阈值，版本才允许扩大权限或接入外部副作用。

## 5. 分阶段整改

首期产品边界：**代码/文件任务 + 只读网络**。外部副作用不随首期开放。

### 阶段 A：信任底座（必须先做）

目标：先让“任务是否正确完成”可被系统判断。

1. 意图账本与软取代语义。
2. 规则预判 + 独立意图仲裁 + 内核校验。
3. 最终答复绑定最新 intent。
4. 三轴状态模型：执行、交付、验证分离。
5. 结构化证据账本与完成门。
6. 确定性验收器与测试/验收装置完整性保护。
7. 预算耗尽统一 parked。
8. 结构化验收命令：直接执行 `program + args + cwd`，不依赖模型自由拼接 shell。
9. 状态 UI 显示 active task、pending intent、adjudication 结果、delivery state 和 verification state。

阶段 A 验收门槛：

- 新 direct intent 存在时，旧 pending `NextTurn` 不得被优先执行。
- 旧 intent 的回答不得被显示为最新 intent 的回答。
- 明显新任务/继续/停止不触发 LLM 仲裁；只有冲突场景才触发。
- 一次消息批次最多一次仲裁调用；重复短消息被合并。
- 普通仲裁与主执行使用独立调用和独立上下文。
- 高风险仲裁使用独立模型或独立进程。
- 仲裁超时、异常或低置信度时不得产生副作用，必须暂停或确认。
- 任一 `verified_complete` 必须有当前任务版本的新鲜通过证据。
- 验证缺失、过期、验证器异常时进入 `blocked_or_parked`。
- 纯信息任务只能显示“结果已交付”，不得显示“完成”。
- `max_steps`/预算耗尽 100% 进入 parked。
- 验证命令的退出码、stdout/stderr 摘要和测试计数全部可审计。
- 测试文件被修改时，验证结果必须标记完整性变化。
- 首期不注册外部副作用工具。

### 阶段 B：自主权限与高权限边界

目标：让模型自主行动，同时把权限判断和高风险边界做成可审计链路。

1. 统一 `tool.check`：`allow / ask / deny`。
2. 独立权限分类器，普通场景可使用独立路由，高风险使用独立模型或进程。
3. 内核硬边界与不可绕过的 deny floor。
4. L0–L3 内部风险分级与动作摘要。
5. L2 精确能力租约；L3 逐动作确认且禁止 session 级复用。
6. AppContainer/隔离环境作为高风险默认。
7. 只读网络 egress allowlist、secrets broker、来源不可信标记。
8. 工作区快照、变更清单和回滚点。
9. 高风险任务的独立安全验收器。

阶段 B 验收门槛：

- 未授权路径和未授权网络目标全部拒绝。
- 首期只读网络只能访问 allowlist 目标，外部副作用工具未被注册。
- 主执行模型不能自行批准 L2/L3 动作。
- 权限分类器失败、超时或低置信度时不得放行。
- L2 租约只能在完整能力指纹范围内复用；任何目标、参数、路径或数据范围变化都会重新判断。
- L3 不存在 session 级授权，只能逐动作确认。
- 高风险动作可以在确认前显示动作摘要、目标、数据范围和可撤销性。
- 任务外副作用必须关联 `task_id`、`approval_id`、`action_digest` 和 `authorization_source`。
- 回滚演练可恢复被修改文件。
- 红队用例覆盖 Junction/symlink、路径穿越、对象损坏、敏感数据外发和提示注入。

### 阶段 C：外部副作用扩展（默认关闭）

目标：在信任底座和权限边界通过后，逐步接入发送、发布和远端写入。

1. 外部连接器与能力白名单。
2. 幂等键、第三方回执和重复发送防护。
3. 接收方、数据范围、内容和撤销性确认。
4. 补偿或回滚策略。
5. 敏感数据分类与外发策略。
6. 频率、数量和成本限额。
7. 高风险外部动作的独立验收器与人工复核。

阶段 C 验收门槛：

- 每次外部动作都有唯一幂等键和可验证回执。
- 无法获得回执时不得显示完成。
- 发送对象、内容、附件和数据范围必须与审批摘要一致。
- 不可逆外发必须有人确认，且不能继承 session 授权。
- 补偿演练通过，失败后进入 blocked/parked。
- 外部动作不能绕过首期形成的意图、权限和证据门。

### 阶段 D：持续学习与发布治理

目标：让系统从事故中学习，而不是靠人记。

1. 行为回归库。
2. 自动运行复盘与事故分类。
3. 任务/版本评分卡。
4. 发布门禁。
5. 线上异常自动生成回归提案。
6. 定期审计权限使用和审批范围。

建议指标：

- false-complete rate
- stale-intent execution rate
- intent adjudication accuracy
- intent adjudication call rate
- intent adjudication p95 latency
- verified completion coverage
- verification-integrity violations
- unauthorized side-effect rate
- permission classifier ask/allow/deny distribution
- human escalation rate
- approval scope ratio
- capability lease reuse violation count
- budget-parking correctness
- repeated incident rate

发布门禁建议：

- false-complete rate：0
- stale-intent execution rate：0
- 高权限任务 verified completion coverage：100%
- 未授权副作用：0
- 预算耗尽后错误显示完成：0
- 验收装置未标记修改：0
- 仲裁失败后仍执行副作用：0
- 权限分类器失败后仍自动放行：0
- L3 动作发生 session 级复用：0

## 6. 优先级与依赖

### P0：不修就不能扩大权限

- 意图账本
- 独立意图仲裁与软取代
- 最终答复绑定
- 三轴状态模型
- 证据门
- 确定性验收器
- 分层独立验收器
- 测试完整性
- 预算 parked
- `tool.check` 与独立权限分类器
- 只读网络 egress allowlist

### P1：高风险开放前必须完成

- AppContainer 默认隔离
- secrets broker
- 工作区快照与回滚
- 审计导出
- 精确能力租约与撤销 UI
- 高风险独立安全验收器
- 外部副作用连接器保持关闭

### P2：外部副作用与规模化治理

- 发送、发布、远端写入连接器
- 幂等、回执与补偿机制
- 自动复盘
- 行为回归库
- 评分卡
- 发布门禁
- 趋势分析与策略迭代

## 7. 明确不做的事

- 不把“多写一段系统提示”当作整改。
- 不把“记住这次教训”当作行为修复。
- 不用模型自述证明模型自己完成。
- 不让用户可见的 `unverified_complete` 继续存在。
- 不把纯信息任务的“结果已交付”计入完成率。
- 不让主执行模型在同一上下文中自行批准自己的动作。
- 不让意图分类器直接扩权、删除、外发或执行工具。
- 不对每条消息无条件调用一次 LLM。
- 不用宽泛 session grant 覆盖后续新范围。
- 不允许 L3 动作使用 session 级授权。
- 不在没有完成门的情况下扩大 shell、网络和外部通信权限。
- 首期不开放发送、发布、远端写入和凭据型外部动作。
- 不把测试修改视为普通文件修改。
- 不接受“已删除的一次性脚本”作为唯一验收证据。
- 不以单一用例通过作为安全边界成立。

## 8. 已确认的产品决策（定稿）

1. **完成状态采用三层模型。** 取消用户可见的 `unverified_complete`，拆成 `run_status`、`delivery_status`、`verification_status`；纯信息任务只显示“结果已交付，无机器验收标准”，不计入完成率。
2. **权限判断交给 LLM，但不交给执行者自证。** 主模型自主决定行动；独立权限分类器输出 `allow / ask / deny`；内核保留硬否决；只有高风险、越界、冲突或不确定时询问用户。
3. **验收独立性按风险分层。** 确定性验证器优先；低风险使用同模型新上下文；中高风险使用独立模型、独立进程和独立快照；严重发现使用对抗验证器并 fail closed。
4. **首期范围为代码/文件任务与只读网络。** 发送、发布、远端写入、凭据和不可逆外部动作默认关闭，待幂等、回执、补偿和独立验收完成后开放。
5. **意图仲裁使用独立调用和独立上下文。** 普通场景可以使用同模型但必须使用不同路由；高风险场景使用独立模型或独立进程并要求重新确认。

补充约束：

- 软取代保留已完成进度、证据和产物，只失效被新要求推翻的部分。
- 意图分类器没有执行权和授权权。
- 新指令不无脑取代旧任务，采用规则优先、模型仲裁、内核决策、不确定时确认。
- 用户接受结果可以记录为 `human_review` 证据，但不能自动替代任务合约要求的机器验证。

## 9. 实施后的目标状态

- 用户看到的“完成”只来自当前任务版本的新鲜验证证据。
- 回答已交付、任务执行结束和任务结果已验证不再混为一个状态。
- 最新指令不会被旧队列静默抢占，也不会因为无脑取代而丢弃有效进度。
- 语义明确的新指令可以快速路由；语义模糊时才使用一次受控的独立仲裁。
- 模型可以自主申请和执行普通动作，但不能自行批准高风险动作。
- 用户只处理高风险、越界、冲突和不确定情形，不承担日常权限调度。
- 高风险动作有明确的任务边界、动作摘要、审批、审计和回滚。
- 首期外部副作用默认关闭；后续接入时必须有幂等、回执和补偿。
- 预算耗尽是可见的暂停，不是伪完成。
- 失败会进入回归库和发布门，而不是只留在一次对话里。
- 模型可以自由规划，但不能自行定义事实、证据和权限边界。

## 10. 外部参考（仅作设计参考）

- Codex：`TurnStatus` 区分执行生命周期；审批按具体 key 缓存；review model 优先独立并支持独立 review history。参考 `G:\爻爻工作区\04_RESEARCH\external\调研资料\01_平台型\codex-main\codex-rs\app-server-protocol\src\protocol\v2\turn.rs`、`...\core\src\tools\sandboxing.rs`、`...\ext\guardian-reviewer\src\model.rs`。
- Pi Durable：`SubmissionSettlement`、`TaskOutcome` 和 reviewer 分层；reviewer 使用独立模型、只读工具和独立工作目录。参考 `G:\爻爻工作区\04_RESEARCH\external\调研资料\01_平台型\pi-main\packages\durable\src\types.ts`、`...\test\examples\28-reviewer.ts`。
- Hermes：验证证据账本记录 `unverified / stale / passed / failed`，但账本本身不阻止完成；这证明证据必须接入完成门。参考 `G:\爻爻工作区\04_RESEARCH\external\调研资料\01_平台型\hermes-agent-main\agent\verification_evidence.py`。
- DeepSeek Harness：Goal 有 `active / paused / blocked / complete`，但明确缺少独立 evaluator；这证明目标状态机不能替代验收。参考 `G:\爻爻工作区\04_RESEARCH\external\调研资料\01_平台型\deepseek-harness-master\packages\goal\goal\src\types.ts`。
- Claude Code：权限模式包含独立 auto classifier；工具检查输出 `allow / ask / deny`；security-guidance 使用规则、LLM diff review 和只读 agentic review 三层。参考 `G:\爻爻工作区\04_RESEARCH\external\调研资料\02_插件生态型\claude-code-main\mods\types\claude-code.d.ts`、`...\plugins\security-guidance\README.md`。
- ECC：按维度并行 review，严重发现交给独立对抗验证器，验证器失败时 fail closed。参考 `G:\爻爻工作区\04_RESEARCH\external\调研资料\02_插件生态型\ECC-main\workflows\README.md`。
- Superpowers：完成声明必须有新鲜验证证据；reviewer 使用专门裁剪的上下文，不复用执行会话历史。参考 `G:\爻爻工作区\04_RESEARCH\external\调研资料\02_插件生态型\superpowers-main\skills\verification-before-completion\SKILL.md`、`...\skills\requesting-code-review\SKILL.md`。
