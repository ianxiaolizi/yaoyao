# 爻爻 · 项目目标与方向

> 本文件回答「爻爻要成为什么、架构怎么分层、怎么跑起来」。
>
> 这里只放稳定事实：目标、定位、架构、crate 地图、技术栈和运行方式。阶段计划、实施记录、实验数据、版本演进和修复流水放项目工作区，不在本文件累积。改代码之前仍要先读 `CONTRACT.md`（不变量与判据）。

## 一句话

事件驱动、协议优先、可插拔的通用 Agent 框架。Rust 单二进制 CLI。首个验证闭环（本地编码 Agent）已经完成；当前以打磨 Agent 内核为主线，最终目标是成熟的**个人**通用助手，之后扩展到研究、写作、系统操作、个人任务管理。

## 定位：为个人服务

**爻爻为个人服务。** 凡以「多用户 / 产品化 / 可发布 / 给别人跑」为前提的能力，默认不作为目标；确有需要时再逐项评估。

## 首个验证闭环（已达成）

用户在项目目录启动 CLI → Agent 读仓库文件 → 改文件 → 执行命令（测试 / 格式化 / `git status`）→ 据结果继续迭代；
用户在 **step 边界** 可 steering；危险命令需审批；崩溃后可恢复；所有模型请求、工具调用、审批决策、文件副作用**可审计**。

## 当前方向

当前阶段打磨 **Agent 内核本身**：让它在真实任务中发现问题、根据证据调整，并知道何时停手、如何退出，而不是继续增加工具数量。

方向原则：

- **结果导向**：任务是否完成由世界状态判定，包括命令、磁盘文件和 `git diff`，不以模型自述为准。
- **失败可行动**：返回给模型的失败信息应简洁说明原因和下一步；完整原文留在事件日志与 artifact 中供复盘。
- **上下文有结构**：用状态、任务、能力、计划和步骤账本组织上下文，按工作集管理预算，避免把历史原样堆进请求。
- **能力尽量沉淀到内核**：重复出现的判断应逐步变成可验证的确定性过程；模型负责理解和处理前沿问题。
- **个人优先**：先在一个有明确裁判的领域做深，再扩展到更广的个人任务。

阶段计划、设计细节、真实使用证据和版本演进记录在项目工作区；稳定的不变量与裁决记录在 `CONTRACT.md` §7。

## 架构与依赖方向（硬规则）

```text
CLI / TUI / IDE / SDK / Web
  → Gateway Commands → Kernel
      Inbox · Scheduler · AgentLoop · PolicyEngine · ApprovalService
      ContextEngine · ModelRouter/ModelProvider · ToolRegistry/ToolProvider
      ExecutionEnvironment · SubagentProvider · EventStore · Projection · ArtifactStore
  ← Durable Events / Live Events / Snapshot
```

```text
apps/agent → cli/gateway/kernel/adapters/context → ports → protocol
protocol → 尽量零依赖    ports → protocol    kernel → ports/protocol/config
```

三条硬规则：

1. **`agent-protocol` 不含业务逻辑；`agent-ports` 不含实现；`agent-kernel` 不依赖具体 OpenAI / SQLite / MCP / shell。**
2. **`loop`（与 `kernel`）不依赖 `projection`**——loop 只认 `agent-ports` 的 `ContextEngine` 端口，实现由 apps 层注入。否则投影引擎会反转依赖方向。
3. **依赖方向由测试把守**，不靠自觉：`crates/agent-testkit/tests/architecture_boundaries.rs` 用集合相等双向钉住**两张**矩阵——运行期 `[dependencies]` 与测试期 `[dev-dependencies]`。清单用 `toml` **解析**而不是逐行扫，所以点表写法、重命名与不含斜杠的成员都看得见；另有一条守卫要求每个 `agent-*` 依赖必须是 workspace 内的 `path`。

workspace 现有 **23 个成员**（22 个 lib crate + `apps/agent` 一个 bin）。测试期允许跨层（适配器要对着真调用方测，而不是对着它的假替身），但每一处都必须在矩阵里注明理由，不得悄悄发生。

## crate 地图

| crate | 职责 |
|---|---|
| `agent-protocol` | IDs, Event, DTO, schema 版本（零业务依赖）；请求面是 `InboxItem`；`reason` 是推理链节点与具名规则表 |
| `agent-ports` | EventStore / ModelProvider / ToolProvider / AgentLoop / ToolGateway / ContextEngine 等 trait 缝 |
| `agent-config` | 配置分层与解析 |
| `agent-kernel` | Inbox、Scheduler、Recovery、Policy、Approval、ResourceGuard、Redactor、plugin、fork、resume、subagent、guarded、reason 链折叠 |
| `agent-loop` | `DefaultAgentLoop`（model → tool → model → answer） |
| `agent-projection` | 投影引擎 + SessionProjection + ModelContextProjection（纯 fold + checkpoint） |
| `agent-context` | `ProjectionContextEngine`（唯一同时看见 projection 引擎与 loop 面向的端口 `agent-ports` 的 crate；运行期不依赖 `agent-loop`，那是 dev 依赖） |
| `agent-decision` | TableModelRouter / RuleDecisionProvider / DecisionBroker / DeadlineAdvisor（shadow；只实现端口） |
| `agent-decision-laya` | Laya 侧车决策适配器（仅 `model_route`，影子模式，失败降级到规则基线） |
| `agent-store-jsonl` | `JsonlEventStore`（每 session 一个 `events.jsonl`） |
| `agent-store-artifact` | `FsArtifactStore`（content-addressed blob） |
| `agent-store-secret` | `EnvSecretProvider`（allowlist 环境变量后端） |
| `agent-tools-fs` | `FsToolProvider`（`fs.list`/`fs.glob`/`fs.grep`/`fs.read`/`fs.write`/`fs.edit`/`shell.run`）+ `LocalExecutionEnvironment`（含 `list_dir`，符号链接只报告不跟随） |
| `agent-tools-git` | git 工具（编码证据链）：`git.status`/`git.diff` 由进程内 libgit2 读取仓库，不 spawn `git`（AppContainer 下 git 拿不到工作目录，`CONTRACT.md` §7 #59） |
| `agent-tools-mcp` | MCP 工具适配器（能力闸门下的注册） |
| `agent-subagent-stdio` | 外部 CLI subagent 适配器（stdio 后端） |
| `agent-sandbox-win` | Windows AppContainer 执行后端：profile/SID、ACL 授予、`STARTUPINFOEX` + Job Object 启动；唯一允许 `unsafe` 的 crate（`CONTRACT.md` §7 #38） |
| `agent-model-openai` | OpenAI 兼容适配器（SSE 流式 + ContentBlock 规范化） |
| `agent-index-sqlite` | `SqliteMemoryIndex`（可丢弃的 SQLite 读模型：从日志重建、词面子串检索；版本不符拒绝，只由 `reindex` 重建）+ `memory.search` 工具（装配时绑定当前 session，能力 `memory.read`） |
| `agent-testkit` | FakeEventStore / FakeModel·Tool·Gateway / FakeSecret / FakeDoctorCheck + 架构守卫 |
| `agent-cli` | CLI 命令 runner：`doctor` `run\|ask` `resume` `fork` `steer` `interrupt` `export` `why` `status` `approve` `deny` `tool` `plugin enable\|disable` `schedule at\|list\|cancel` `tick` `serve` `web` `memory search\|reindex` `context inspect` `artifact get` `metrics [--out]` |
| `agent-web` | 本地回环 HTTP 入口：内嵌 GUI、一次性令牌、会话/transcript/thinking/why/diff/context/plan/subagent/schedule 读模型、SSE（历史重放 + 实时预览）、导出与 artifact、起轮/审批/过期续跑/分叉/插话/中断；写操作经 `Backend` 缝交给 `apps/agent` 的 `WebBackend`，再由它调用同一套 `agent-cli` 具名命令，不复制内核装配 |
| `apps/agent` | 唯一具体组装入口（二进制）；`instructions` 模块把工作区根部的 `AGENTS.md` + 内置前言组成每次运行的系统指令 |

`agent-kernel` 一个 crate 占 src 的约四分之一；`agent-loop`、`agent-cli` 是单文件大模块。

## 关键设计决策

| 决策 | 选择 |
|---|---|
| 内核架构 | 事件驱动 + Rust Trait 插拔；崩溃可恢复、可审计、单二进制、可替换 |
| 协议分离 | `agent-protocol` 零业务依赖，CLI、daemon、SDK 与测试共享协议 |
| 权限模型 | fail closed：缺策略或能力时拒绝，不静默降级 |
| Inbox | 内核一等能力，统一承载用户输入、审批、定时唤醒与 steering |
| 模型接入 | provider-neutral 内部协议；第一版提供 OpenAI-compatible 适配器 |
| 工具扩展 | 工具名全局唯一；MCP 是工具来源之一，不成为内核协议 |
| 上下文 | 稳定系统前缀 + 状态/任务层 + 计划与步骤账本；按工作集预算压缩 |
| 审计 | 持久化事件是事实源；请求、工具定义、审批和 artifact 均可回读 |
| 执行边界 | 默认 `local`；Windows 可选 `app-container`，选中后失败关闭 |
| 决策中枢 | shadow 模式起步，只记录 trace，不改变主链 |
| 可倒推 | 内核在决策点写 `reason` 节点（situation / reading / stance / action / observation / gate / verdict），每个节点引用具名规则与依据；真实失败或用户纠正会在回合内切位并写 `switch` 节点；`why` 反向展开到事实与用户原话；链不进入模型请求 |
| 项目范围 | 优先服务个人，不按多租户或通用发布平台设计 |

具体裁决、理由和当前约束见 `CONTRACT.md` §7。

## 技术栈

Rust / Cargo，**edition 2024，`rust-version = 1.99`**；tokio 1.53、serde、async-trait、thiserror、chrono、uuid、reqwest 0.12（rustls-tls/stream/json）、futures-util。Windows 专用 FFI 经 `windows-sys`（`[target.'cfg(windows)'.dependencies]`），只在 `agent-sandbox-win` 内使用。

## 怎么跑

```powershell
cargo build --workspace
powershell -ExecutionPolicy Bypass -File scripts\test.ps1   # fmt + clippy + 全量测试
```

`scripts\test.ps1` 是跑门的唯一入口。仓库根可能带 Low 完整性标签，构建出的测试二进制会继承它，导致 CLI 集成测试打不开 `NUL` 而假失败；脚本先 `cargo test --workspace --no-run`，再把 `target\debug` 下的二进制提到 Medium integrity，并把 `TEMP/TMP` 指到 `%TEMP%\yaoyao-test-tmp`。`scripts\preflight.ps1` 在跑门前报告 TEMP / NUL / 完整性 / 工具链。该修复由 `crates/agent-testkit/tests/environment_gate.rs` 把守。

默认 `AGENT_MODEL_PROVIDER=echo`，**离线可跑，不需要网络与凭据**。接真实模型（第一版 OpenAI-compatible）：

| 变量 | 说明 |
|---|---|
| `AGENT_MODEL_PROVIDER` | `echo`（默认）或 `openai` |
| `AGENT_MODEL_BASE_URL` | 端点基址，**不含** `/chat/completions`，如 `https://api.deepseek.com` |
| `AGENT_MODEL_NAME` | 模型名，如 `deepseek-flash` |
| `AGENT_MODEL_API_KEY` | 凭据。只以 `SecretRef` 形式进配置，运行时经 `EnvSecretProvider` 解析，**不落盘、不进事件、不进日志** |

> `crates/agent-model-openai/src/config.rs` 里的 `ENV_*` 常量装的是**变量名**，不是值。把 base URL / 模型名 / key 的字面值填进去，等于让 `from_env()` 去找一个以该字面值为名的环境变量：配置直接失败，且 key 明文留在受版本控制的源码里。值只经环境变量传递。

每次运行开始前，爻爻会读 **workspace root 下的 `AGENTS.md`**（只看根部这一份，不读嵌套文件，也不读 `CLAUDE.md`），与内置前言一起作为本次运行的系统指令。上限 64 KiB；不可读、超限、非 UTF-8 或解析到工作区外，本次运行**不会开始**（退 `2`，inbox 项保持待办）。指令按运行快照记进 `run/started`，所以「模型被告知了什么」与日志是同一条事实；没有这份文件是合法状态，运行照常。

`AGENT_WORKSPACE_DIR` 是 workspace root，**相对路径一律相对它解析，与 cwd 无关**（`ResourceGuard` 与工具层同一基准）。配置里写下的路径也按这条规则：MCP server 与 subagent provider 的 `cwd` 同样相对 workspace root 解析、同样受边界约束，**落在工作区外的不启动该插件/后端**（会在 stderr 记一条 note 并跳过，不会让整次 run 失败）——不写 `cwd` 则维持原样，子进程继承进程的 cwd。`AGENT_DATA_DIR` 放事件日志与 artifact，**按同一条规则解析**：相对值相对 workspace root，绝对值原样，未设（或设成空串）则用 workspace 里的 `.agent-data`。注意这意味着「只设了 `AGENT_WORKSPACE_DIR`」时数据落在 workspace 内，而不是 cwd 下的 `./.agent-data`——旧目录不会被搬移或删除。

`AGENT_SANDBOX` 选择执行后端：`read-only`（只读文件 + 记忆检索，拒绝写与命令）、`local`（默认；命令在宿主机上跑，文件与命令 cwd 受 workspace 边界约束）、`app-container`（Windows AppContainer；工作区外写入由内核拒绝）、`docker`（当前 build 尚无后端）。

`app-container` 需要先以管理员权限运行 `sandbox setup`。它的授权、网络策略、启动和保活细节由 `CONTRACT.md` §7 约束；选中但环境未准备好时退 `2`，不会悄悄回落到 `local`。`AGENT_SANDBOX_NETWORK=allow|deny` 控制沙箱网络，默认 `deny`。

模型档位写在 profile 的 `[model]` 里：`main` 是本次运行的模型，`fast` / `thinking` 留给摘要与审查；`--model` 只覆盖 `main`，不会动另两档。今天**只有上下文压缩**按 `summarize` 真的走到 `fast`，`review` 还没有生产者，所以配了 `thinking` 也只会被记录、不会被用到。档位只换模型名，不换 provider——一个进程仍然只说一种 provider kind。没配任何档位时行为与以前完全一致，`profile/resolved` 也不多一个字段（`CONTRACT.md` §7 #118）。

```bash
# 定时唤醒：安排、查看、取消，然后驱动一次
cargo run -p agent -- schedule at +10m "check the repo"
cargo run -p agent -- schedule list
cargo run -p agent -- schedule cancel <item_id>
cargo run -p agent -- tick          # 0 = 跑了一个 turn；2 = 没有可跑的
cargo run -p agent -- serve         # 守护循环：到期就跑；Ctrl-C 退 0；一个 session 一个 serve

# 记忆检索：索引是派生数据，每次搜索前先与日志同步
cargo run -p agent -- memory search xylophone   # 0 = 有结果或没有结果；2 = 搜索没能运行
cargo run -p agent -- memory reindex            # 丢弃并重建 <data_dir>/<session>/memory.sqlite

# 上下文检查：下一条请求会带什么（大小 / 分类 / 预算 / 最大消息 / 上次请求的实录）
cargo run -p agent -- context inspect            # 空会话也是答案（0）；配置/日志读不了 2
cargo run -p agent -- context inspect --json --top 10   # 一条 JSON；--top 控制最大消息条数

# 指标：turn/工具/审批/压缩/token/耗时的纯日志派生（默认全部会话；--session 收窄）
cargo run -p agent -- metrics                      # 文本；空数据目录也是答案（0）
cargo run -p agent -- metrics --json --out metrics.json   # 派生文档，同一日志重算字节相同

# 倒推一轮的每一步为什么：从答复沿依据走到用户原话（无链时退 2）
cargo run -p agent -- why
cargo run -p agent -- why --turn <turn_id> --json

# 取回日志里出现的 artifact（大工具输出 / MCP 二进制）；locator 或裸 hash 都收
cargo run -p agent -- artifact get artifact://sha256/<hash> --out blob.bin  # 0 = 取到；1 = 没有或已损坏
cargo run -p agent -- artifact get <hash>       # UTF-8 文本直接打到 stdout；二进制必须给 --out

# 工作区与数据目录（PowerShell）
$env:AGENT_WORKSPACE_DIR = 'C:\path\to\workspace'
$env:AGENT_DATA_DIR = Join-Path $env:AGENT_WORKSPACE_DIR 'data'

# 联网的适配器测试（默认 #[ignore]，需显式打开）
cargo test -p agent-model-openai --test live_endpoint -- --ignored --nocapture

# AppContainer：宿主准备（需提权；每次开机后重放）与只读状态；真跑用例同样默认 #[ignore]
cargo run -p agent -- sandbox setup --readonly 'C:\path\to\readonly'
cargo run -p agent -- sandbox status
# 登录时自动重放授权（需提权；安装/查看/卸载）
powershell -File scripts/sandbox-boot-task.ps1 -Install
powershell -File scripts/sandbox-boot-task.ps1 -Status
powershell -File scripts/sandbox-boot-task.ps1 -Remove
# AppContainer 真跑用例（release 与 debug）
powershell -File scripts/sandbox-live.ps1
powershell -File scripts/sandbox-live.ps1 -DebugBuild

# 覆盖率门（含 AppContainer 真跑用例）
$env:AGENT_APPCONTAINER_TEST = '1'
powershell -File scripts/coverage.ps1

# M4–M6 离线验收
cargo test -p agent --test m4_acceptance
cargo test -p agent --test m5_acceptance
cargo test -p agent --test m6_acceptance

# 内核评测：真端点、真二进制，任务按结果判定，并记录失败之后的动作
cargo test -p agent --test kernel_eval -- --ignored --nocapture
# 重复次数、只跑指定 fixture、报告路径：KERNEL_EVAL_RUNS=3、KERNEL_EVAL_TASKS=recall,anchor、KERNEL_EVAL_REPORT=<path>
# 思考流水线档位：gates（默认，只保留确定性闸门）/ on（完整 judgment 账本）/ off（评测基线）
$env:AGENT_JUDGMENT = 'on'
```

Windows 注意：Git Bash 的 `/usr/bin/link` 会遮蔽 MSVC 的 `link.exe` 导致链接失败，确保 VS C++ 工具路径在 PATH 中优先；VS Build Tools 2022 需装「使用 C++ 的桌面开发」工作负载。

## 参考来源与采纳

| 参考 | 采纳内容 |
|---|---|
| DeepSeek Harness | Capability Seam, Turn/Step, Session Log, Inbox |
| Codex | ExecPolicy, Sandbox, Protocol crate |
| Hermes | Message UID, Observer/Middleware 分离, FastDecision |
| Claude Code | Capability 声明与同意, Skills 渐进披露 |
| OpenCode | Tool Manifest 快照, Plugin Hook 覆盖点 |
| Dify | Runtime Lease, Deferred Tool, Workflow（暂缓） |

调研材料、选型比较和未采纳方案在项目工作区；本表只记录已采纳的架构参考。

## 文档面

| 文档 | 回答什么 | 谁该读 |
|---|---|---|
| `PROJECT.md`（本文件） | 要成为什么 / 架构与依赖 / 怎么跑 | 所有人，第一次进项目 |
| `CONTRACT.md` | **不变量与判据**：事实源、fail closed、退出码、租约、命名、版本化；术语与分类口径 | **改任何一行代码之前** |
| `STYLE.md` | 代码风格与工程规则：`deny(unsafe_code)`、unwrap 禁令、错误类型、测试与提交约定 | 写代码时 |

阶段计划、实施记录、真实使用证据和版本演进不进入本仓文档，统一放在项目工作区。
