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

## 快速开始

```powershell
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

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
