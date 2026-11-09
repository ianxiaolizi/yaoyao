# 爻爻交付真实性与运行能力硬化设计

**日期：** 2026-11-09  
**事故会话：** `web-20261103090946-2ntt`  
**事故任务：** “做一个 3 秒的火柴人战斗视频”  
**相关约束：** `CONTRACT.md` §7 #166、#170、#183；`PROJECT.md` 的 sandbox / evidence / status 章节

## 1. 结论

这次事故不是启动崩溃，也不是模型请求失败。它暴露的是一条完整的错误链路：

```text
任务需要运行和渲染
  -> local 后端拒绝 process.spawn
  -> 模型选择“交付 HTML 导出页”作为降级
  -> 没有浏览器渲染或视频导出验证
  -> 78 步、117 次工具调用后才结束
  -> 完成闸判定 verdict=missing
  -> 页面仍把 answered 显示成“完成”
```

最需要修的不是“让模型更会写动画”，而是三件事：

1. **结果状态必须真实**：已答复、已交付、未验证、已验证完成是不同事实。
2. **能力缺口必须尽早暴露**：运行环境不能启动进程时，不应让模型长时间用静态编辑绕路。
3. **验证必须是任务闭环的一部分**：生成视频、网页、文档等 artifact 时，必须有对应的运行或渲染证据。

## 2. 事故问题清单

| 问题 | 证据 | 影响 | 优先级 |
| --- | --- | --- | --- |
| 交付物与原始要求不一致 | 只留下 HTML/README，没有视频文件 | 用户拿不到任务要求的视频 | P0 |
| 未验证结果被最终文案称为“交付已完成” | `completion/unverified_result_delivered`, `verdict=missing` | 用户误以为结果已经完成 | P0 |
| Web 顶部状态把 `answered` 显示成“完成” | `web/src/format.ts::turnStatus` | 完成状态与内核判定不一致 | P0 |
| Web 没调用已有 `/status` 接口 | `/api/sessions/{id}/status` 已存在，但前端未使用 | 三轴状态没有到达用户界面 | P0 |
| local 后端不能启动进程 | 两次 `shell.run` 均因 risk l3 被拒绝 | 无法调用 ffmpeg / 浏览器 / 本机检查 | P1 |
| 模型在能力不足后继续静态试错 | 78 步、117 次工具调用、36 分 51 秒 | 成本高、收敛慢 | P1 |
| 没有浏览器实渲和导出验证 | 只有 fs.read/edit/grep/state.update | 动画接触、音效、导出都可能坏 | P1 |
| 多次原地编辑造成临时代码损坏 | 模型记录 `B_KEYS` 重复、语法损坏、IK 0/0 等 | 增加修复轮次 | P1 |
| 工作区不是 Git 仓库 | `git.status` 失败：`not a git repository: G:\yaoyao-work` | 无 git diff 证据 | P2 |
| 工具参数误用 | `fs.grep` 把文件当目录 | 一次无效调用 | P3 |

## 3. 同类 Agent 的做法

### 3.1 OpenAI Codex（本机 CLI 行为）

- 提供 `read-only`、`workspace-write`、`danger-full-access` 三种 sandbox，并配合审批策略。
- `codex exec` 支持 `--skip-git-repo-check`，说明执行能力与 Git 仓库不是同一个概念。
- 有独立 `review` 与 `apply` 路径，把“生成改动”和“检查/应用改动”分开。
- 对本项目的启示：**沙箱模式、审批、Git 证据应解耦**。不要因为工作区不是 Git 仓库就失去变更证据，也不要因为 local 不能起进程就把“不能做”误报成“已经做完”。

### 3.2 Claude Code

- 权限架构由 permission mode、allow/ask/deny 规则和 sandboxed bash 组成；权限由运行时强制，不依赖模型自觉。
- 支持 hooks：`PostToolUse` 可做格式化/检查，`Stop` 类生命周期钩子可阻止未通过检查就结束。
- 提供 checkpointing / rewind，降低反复修改的恢复成本。
- 可用 Chrome 或 computer use 做页面/应用的真实交互验证。
- 对本项目的启示：**把验证做成运行时门，而不是提示词建议**；同时把视觉/浏览器验证纳入可用能力。

来源：

- https://code.claude.com/docs/en/security
- https://code.claude.com/docs/en/permissions
- https://code.claude.com/docs/en/hooks-guide
- https://code.claude.com/docs/en/checkpointing
- https://code.claude.com/docs/en/chrome

### 3.3 OpenHands

- 提供 Docker/agent-server 隔离运行环境。
- 提供 built-in stuck detector：重复 action-observation、重复错误、交替循环等模式会被识别并中止。
- 支持 lifecycle hooks，Stop hook 可以强制 lint/test 通过后才结束。
- 提供 browser tools 和浏览器会话录制，能对 Web 产出做真实观察。
- 对本项目的启示：**长循环要靠进度检测，不靠固定步数上限**；视觉任务应有浏览器 observation，而不是只读代码。

来源：

- https://docs.openhands.dev/sdk/guides/agent-stuck-detector
- https://docs.openhands.dev/openhands/usage/customization/hooks
- https://docs.openhands.dev/sdk/guides/agent-server/docker-sandbox
- https://docs.openhands.dev/sdk/guides/browser-session-recording

### 3.4 Aider

- 强烈依赖 Git：自动提交、`/undo`、`/diff`、提交历史构成修改证据。
- 每次编辑后可自动 lint / test，失败后继续修复。
- 使用 repository map 和 architect/editor 分工降低上下文与编辑错误。
- 对本项目的启示：**编辑后立即检查**很有价值；但不要照搬“必须有 Git 仓库”，爻爻面向个人自动化任务，需要 snapshot 作为通用变更账本。

来源：

- https://aider.chat/docs/git.html
- https://aider.chat/docs/usage/lint-test.html
- https://aider.chat/docs/repomap.html
- https://aider.chat/docs/usage/modes.html

### 3.5 横向对照

| 能力 | Codex | Claude Code | OpenHands | Aider | 爻爻当前 | 建议 |
| --- | --- | --- | --- | --- | --- | --- |
| 沙箱与审批分离 | 有 | 有 | 有 | 弱 | 有，但 UI 未充分表达 | 保持并前置说明 |
| 拒绝工具是否隐藏 | 由策略/审批控制 | 可移除或拦截 | 运行时拦截 | 不适用 | 仍出现在 manifest | 至少在前端和提示中明确“不可用” |
| 完成验证 | 命令/评审闭环 | hooks + 人工 | Stop hooks + runtime | lint/test | 证据账本 + completion gate，机制最强 | 把状态真正送到 UI |
| 视觉/浏览器验证 | 本地可执行，需环境支持 | Chrome/computer use | browser tools | 无 | 缺 | Phase 2 增加 browser renderer |
| 长循环检测 | 预算/审批/人工 | hooks/权限 | stuck detector | lint/test 反馈 | 有 `stall_steps`，但只覆盖“无新证据” | 增加“无进展/反复写同一文件”检测 |
| 变更证据 | Git/diff/review | checkpoint + git | workspace/runtime | Git | snapshot 已有，diff 面板只读 git.diff | 用 snapshot 作为非 Git 工作区兜底 |

## 4. 设计决策

### D1. “答复”不等于“完成”

`turn/ending=answered` 只说明模型给出了最终文本。页面不能直接显示“完成”。

用户可见状态必须来自内核三轴：

- `run_status`
- `delivery_status`
- `verification_status`
- 派生的 `completion_status`

建议文案：

| 状态 | 文案 |
| --- | --- |
| `verified_complete` | 已验证完成 |
| `delivered + unverified` | 已交付 · 未验证 |
| `delivered + not_applicable` | 已回答 |
| `delivered + stale` | 验证已过期 |
| `blocked_or_parked` 且未交付 | 未完成 / 已阻塞 |
| 运行中 | 运行中 |

### D2. 不把“静态产出”默认当成降级完成

当用户的原始目标需要运行、渲染或导出，而当前后端不能提供对应能力时：

- 可以先提出替代方案，但必须让用户知道这不是原目标；
- 不能在没有用户同意的情况下把替代方案标为“已完成”；
- 一次关键能力拒绝后，应阻止长时间的盲目绕路。

### D3. 保留 unbounded 主循环，不用固定步数治所有问题

当前方向允许无固定步数上限。不要退回到“默认 8 步/固定步数”的旧方案。

应增加的是：

- 无新增可观察进展的检测；
- 同一文件反复编辑且没有验证的检测；
- 任务类型对应的 wall-clock / token / tool-call 预算；
- 到预算阈值时暂停并交付当前状态，而不是静默成功。

### D4. 变更证据不依赖 Git

snapshot 已经是爻爻的事实来源。下一阶段应让“改动”面板在没有 `git.diff` 时回退到最近一次 workspace snapshot / changes 记录。

### D5. 验证能力按 artifact 类型落地

- 代码：构建、测试、lint、语法检查。
- Web：打开页面、截图、控制台错误、关键交互。
- 视频/音频：导出文件存在、可解码、时长/帧率/音轨符合要求。
- 文档/表格：结构读取、必要字段、可预览。

## 5. 分阶段调整路线

### Phase 0：止血（本计划的实施范围）

1. Web 接入 `/api/sessions/{id}/status`。
2. 顶部状态区分“已回答 / 已交付·未验证 / 已验证完成”。
3. 对未验证交付显示明确横幅，不再显示普通“完成”。
4. 当 `sandbox=local` 时，在输入区提示“不启动命令/渲染进程，需要 app-container”。
5. 增加事故回归 smoke fixture，锁住这套文案和状态行为。

### Phase 1：能力前置与停止规则

1. 同一 run 内出现两次 `sandbox_unavailable` / risk-l3 拒绝时，转入 `NeedsUserInput`，而不是继续绕路。
2. 任务开始前输出可执行能力摘要：文件、命令、浏览器、网络、导出。
3. 对需要进程/渲染的任务，提供明确的“切换 app-container / 接受降级 / 取消”三分支。
4. 不在 local 后端静默生成“可导出视频页”并称为视频完成。

### Phase 2：验证闭环与收敛

1. 增加 Web artifact 的 browser render / screenshot / console check。
2. 增加任务级 verifier recipe，例如 `check: web`, `check: video`。
3. 对同一文件连续多次编辑但无成功验证的情况注入收敛提示。
4. 记录“编辑次数、验证次数、最后修改 hash”到 task report。

### Phase 3：证据与预算

1. 非 Git 工作区的 diff 面板回退到 snapshot / changes ledger。
2. 按任务类型选择 wall-clock/token/tool-call 预算。
3. 把“验证失败后继续编辑”的循环纳入 metrics，用于评估模型与 prompt。
4. 增加失败成本报表：每个任务的步骤、工具数、编辑数、验证数、最终状态。

## 6. 验收标准

Phase 0 完成后：

- 一次 `answered` 但没有 completion evidence 的会话，顶部不能显示“完成”。
- 页面在 `verification_status=unverified` 时显示“已交付 · 未验证”。
- 页面在 `verification_status=verified` 且 `completion_status=verified_complete` 时显示“已验证完成”。
- local 后端页面明确提示命令/渲染能力不足。
- 事故 fixture 在 GUI smoke 中稳定复现并断言上述行为。
- 不修改内核的 completion gate 语义，不破坏现有 CLI 的 `agent status` 输出。

## 7. 明确的非目标

- 本阶段不实现完整视频渲染器。
- 本阶段不引入新的默认固定步数上限。
- 本阶段不要求所有工作区必须是 Git 仓库。
- 本阶段不自动批准高风险命令。
- 本阶段不改变“审批是逐次、精确动作”的安全边界。