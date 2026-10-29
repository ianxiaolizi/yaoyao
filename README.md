# 爻爻 (yaoyao)

事件驱动、协议优先、可插拔的通用 Agent 框架。Rust 单二进制 CLI，面向本地编码与个人自动化任务。

> [!WARNING]
> 爻爻会执行 shell 命令、读写工作区文件，并读取工作区根目录的 `AGENTS.md` 作为运行指令。
> 只应在可信工作区中运行；不要对不可信仓库或不可信用户开放。
> 默认的 `local` 后端不是操作系统级沙箱。处理不可信任务时请使用 `app-container`，并保留审批流程。

项目当前处于 **developer preview**。首个验证闭环是本地编码 Agent：读取仓库文件、修改代码、执行测试，并依据世界状态继续修正。

## 能做什么

- 事件驱动、可恢复、可审计的 Agent 内核
- 可倒推的推理链：每个调用与答复都能用 `why` 沿依据走回触发它的事实与用户原话
- 事件日志、artifact、会话恢复、分叉和 steering
- 计划账本、结构化上下文、历史压缩
- 文件、Git、MCP、subagent 工具
- Windows AppContainer 执行后端
- OpenAI-compatible 模型适配器
- 本地离线 `echo` 模式

## 环境要求

- Windows 10/11
- Rust 1.99
- Visual Studio Build Tools 2022，安装“使用 C++ 的桌面开发”工作负载

当前版本只以 Windows 为主要运行目标。其他平台尚未纳入 CI。

## 双击启动

双击仓库根目录的 `启动爻爻.cmd`（它只负责调 `scripts\gui\start.ps1`，真正的逻辑在那一个脚本里）：

- 自动构建 `target\debug\agent.exe`；
- 使用 `G:\yaoyao-work` 作为工作区（存在时），数据目录用 `G:\yaoyao-work\.agent`；
- 在 `7441` 起服务；
- 读到带一次性令牌的 URL 后自动打开默认浏览器；
- 控制台保留服务日志，Ctrl+C 停止。

它不做的事：不创建新会话、不写事件、不把令牌写进日志或 localStorage。

想改成本机配置时：

```powershell
Copy-Item scripts\gui\start.local.ps1.example scripts\gui\start.local.ps1
notepad scripts\gui\start.local.ps1
```

`start.local.ps1` 已在 `.gitignore` 里，可以放模型 key，不要提交。示例里给了
DeepSeek / OpenAI-compatible 的四个环境变量。

启动器参数（直接跑 PowerShell 脚本时）：

```powershell
.\scripts\gui\start.ps1 -Port 7442
.\scripts\gui\start.ps1 -NoBrowser
.\scripts\gui\start.ps1 -SkipBuild
.\scripts\gui\start.ps1 -DryRun -NoPause
```

端口被占用时启动器会直接报出占用进程，不会把第二个服务叠上去。

## 手动启动 / 开发

```powershell
cargo build --workspace
powershell -ExecutionPolicy Bypass -File scripts\test.ps1   # fmt + clippy + 全量测试
```

`-ExecutionPolicy Bypass` 是本机必需的：默认策略拦未签名脚本。`scripts\test.ps1` 是跑门的唯一入口：仓库根可能带 Low 完整性标签，测试二进制继承后打不开 `NUL`，普通 `cargo test` 会假失败；脚本在构建后把测试二进制提到 Medium integrity，并把 `TEMP/TMP` 指到 `%TEMP%\yaoyao-test-tmp`。环境报错时先跑 `powershell -ExecutionPolicy Bypass -File scripts\preflight.ps1`，它报告 TEMP / NUL / 完整性 / 工具链四项事实。

默认使用离线回显模型，不需要网络或凭据：

```powershell
cargo run -p agent -- ask "hello"
```

连接 OpenAI-compatible 端点：

```powershell
$env:AGENT_MODEL_PROVIDER = 'openai'
$env:AGENT_MODEL_BASE_URL = 'https://api.deepseek.com'
$env:AGENT_MODEL_NAME = 'deepseek-flash'
$env:AGENT_MODEL_API_KEY = '<your-key>'
```

凭据只通过环境变量传入，不写入仓库、事件日志或模型上下文。

## 本地 Web GUI

`agent web` 在同一个进程、同一个二进制里起一个只绑 `127.0.0.1` 的页面：

```powershell
cargo run -p agent -- web
# 爻爻 · web  http://127.0.0.1:7431/#t=<one-time-token>
```

- 终端打印的 URL 里带一次性令牌；页面从 `#t=` 读它，之后每个 API 请求走
  `Authorization: Bearer`。令牌不进日志，也不进 `localStorage`。
- 左侧列出 `<data_dir>/*/events.jsonl` 折出的会话；中间是同一条时间线上的提问、
  工具调用、审批与回答；右侧是这一轮的推理链和真实用量。
- 页面通过 SSE 接收 **durable item**（按 id upsert）与 **live 文本**（只是预览）；
  刷新后从日志整块重建，不依赖前端缓存。
- 写操作只有起轮、回答审批、插话、中断。它们和 CLI 走同一套命令：
  `agent-web` 只负责 HTTP，装配在 `apps/agent` 的 `WebBackend` 里。
- 审批会用红色标出；允许一次 / 本回合 / 本会话 / 拒绝之后，被挂起的回合通过
  `Resume` 继续。审批卡有 24h TTL 倒计时；过期后出现「继续这一轮」，由
  `POST /resume` 让内核把过期记为「没有派发」并续跑。刷新页面不会丢状态。
- 右栏「改动」读的是日志里最近一次 `git.diff` 的**观察结果**，不现场跑 git；
  「推理链」来自 `/why`；「上下文」来自最近一次 `model/request.prepared` 的历史 /
  工具定义 / 本轮层与输入分项。
- 顶部「导出」可下载 Markdown 或 JSON；工具结果里的 `artifact://sha256/<hash>` 可以
  直接在页面打开，服务端读取时会重新校验摘要。图片 artifact 会按 blob 在页内预览，
  其他类型仍按原样打开。
- 侧栏、右侧面板、正文宽度与输入框高度都可拖动，双击复原；深浅色切换在左下角。
  这些只存在 `localStorage`，不属于会话事实。
- 1080px 以下侧栏收成抽屉，用 ☰ 打开，点遮罩或按 Escape 关闭。
- 顶部「分叉」从最近的闭合回合边界建子会话；侧栏会标出子会话的
  `↳ 分叉自 <parent>`。
- 右栏「子代理」显示父会话通过 `subagent.run` 委派出的 child、provider、目标和状态；
  点 child id 打开对应会话。
- 右栏「定时」列出、创建、取消 Inbox 里的定时条目；实际运行仍由 `tick` / `serve`
  驱动，Web 不会自己起轮。

`agent web` 也遵守 `AGENT_WORKSPACE_DIR`、`AGENT_DATA_DIR`、`AGENT_SANDBOX` 与
`AGENT_MODEL_PROVIDER`；没有配置时默认使用离线 `echo` 模型和当前目录。
一个会话同时只应有一个驱动进程：另一个驱动者会被内核租约挡在 append 之外；
Web 端点不会因此写第二份日志，而是把这次拒绝作为运行提示送回页面。

浏览器冒烟脚本会自己起一个临时服务、种出带 `thinking` / `git.diff` / 审批的会话：

```powershell
npm ci --prefix scripts/gui
node scripts/gui/smoke.mjs
```

## 文档

| 文件 | 内容 |
|---|---|
| `PROJECT.md` | 项目目标、稳定架构、crate 地图、技术栈与运行方式 |
| `CONTRACT.md` | 不变量、判据、退出码、并发、命名与裁决记录 |
| `STYLE.md` | Rust 风格、测试、文档与提交规则 |
| `SECURITY.md` | 安全边界与漏洞报告方式 |

阶段计划、真实使用证据和版本演进记录在独立项目工作区，不进入本仓库。

## 安全边界

- 不要将 `shell.run` 暴露给不可信用户。
- 不要把 `AGENTS.md` 当作可审计的安全策略；它是模型指令来源，可能包含提示注入。
- `local` 后端受工作区路径约束，但不是 OS 隔离。
- `app-container` 更适合不可信代码，但仍需审批、最小权限和独立工作区。
- 本项目不面向多租户或公网服务部署。

安全问题请按 `SECURITY.md` 私下报告，不要先开公开 issue。

## 许可证

MIT。维护者：道权。
