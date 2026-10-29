# 爻爻 · 代码风格与工程规则

> 这份文档回答「代码该怎么写」。**不变量与判据在 `CONTRACT.md`**（改代码前先读那份），项目目标与架构在 `PROJECT.md`。

## 0. 门（唯一可量化的部分）

门只有一个入口：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\test.ps1
```

本机执行策略会拦未签名脚本（`UnauthorizedAccess`），所以文档里的调用一律带 `-ExecutionPolicy Bypass`；只想看一次也不改策略时可以照抄这一行。脚本依次跑 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`（本地默认 `-j 1`：并行偶发 `0xc0000409`）、`cargo test --workspace`。三门全绿才谈得上「改完了」。

普通 `cargo test --workspace` 在这台机器上会假失败，原因是环境而不是代码：仓库根带 Low 完整性标签，构建出的测试二进制继承 Low，而 CLI 集成测试要把 `NUL` 作为子进程 stdin 打开。`scripts\test.ps1` 在 `--no-run` 之后把 `target\debug` 下的二进制提到 Medium integrity，并把 `TEMP/TMP` 指到 `%TEMP%\yaoyao-test-tmp`；`scripts\preflight.ps1` 在跑门前报告 TEMP / NUL / 完整性 / 工具链四项事实。该修复本身由 `crates/agent-testkit/tests/environment_gate.rs` 把守。

**CI 跑的是同一脚本**（`.github/workflows/ci.yml`，push / PR 到 `main`，可 `workflow_dispatch` 手动重跑）：`check` job 分步跑 fmt、clippy，Test 步调 `scripts/test.ps1 -SkipFormat -SkipClippy`。另外两个 job：`coverage`（`scripts/coverage.ps1`，阈值在 `scripts/coverage-baseline.json`；设 `AGENT_APPCONTAINER_TEST=1` 时合并 AppContainer 真跑测量，否则本机会低于阈值）与 `perf-smoke`（release 跑性能示例的正确性哨兵，不与单机基线比对）。CI 的 Rust 版本钉在 workspace 的 `rust-version`（`1.99.0`）。

**当前基线（2026-10-29）：1588 passed / 0 failed / 15 ignored**（经 `scripts\test.ps1` 全量跑出，108 个 test result 行）；新增用例应让 passed 变大，failed 保持 0。覆盖率门槛读的是行覆盖率（`cargo llvm-cov report` 的 TOTAL 行第 9 列，与 `coverage-baseline.json` 的 `measured.lines` 同一列）。

## 1. Rust 工程规则（硬）

- **每个成员的每个 crate root 顶部 `#![deny(unsafe_code)]`**（lib 是 `lib.rs`、bin 是 `main.rs`；一个包同时有两种 target 时两个都要；放在模块 `//!` 文档之下）。22 个成员的 23 个 crate root 全部已声明，由 `crates/agent-testkit/tests/crate_lint_boundaries.rs` 把守。
  - **唯一例外是 `crates/agent-sandbox-win`**（`#![allow(unsafe_code)]`）：Windows AppContainer/ACL 没有纯 Rust 路径。它只依赖 `agent-ports`，每处 `unsafe` 带 `// SAFETY:`，crate 内 `#![deny(unsafe_op_in_unsafe_fn)]` + `#![deny(clippy::undocumented_unsafe_blocks)]`。闸门的白名单**精确到一个名字**，并有「例外只能有一个」的守卫；加第二个例外必须同时改 `CONTRACT.md` §7 #38。裁决理由见 §7 #38。
- **运行路径禁 `unwrap` / `expect`**：kernel / store / policy / approval / resume / plugin 等可被外部输入触达的路径。**锁中毒必须返回 `Err`，不得 panic**。测试与 `#[cfg(test)]` 里随便用。
- **错误类型按 crate 定义**（`thiserror`），**不用 `anyhow` 穿过公共协议边界**。
- **外部 I/O 只出现在 adapter**：`agent-protocol` 与 `agent-ports` 不含 I/O、不含业务逻辑；`agent-kernel` 不依赖具体 OpenAI / SQLite / MCP / shell。依赖方向见 `PROJECT.md`，由 `crates/agent-testkit/tests/architecture_boundaries.rs` 把守。
- **时间是输入，不是环境**：**参与判断**的时刻必须由调用方传入——`ApprovalService::fold_at(session, now)` 与 `LeaseSpecExt::is_expired(now)` 是范式，判定函数里不读时钟。直读 `Utc::now()` 只允许出现在**盖章位置**（`created_at:` / `recorded_at:` 字段初始化，以及 `DriverRecord` 的 `started_at:` / `heartbeat_at:`）和那**一处**把时刻递给 `fold_at` 的边界。**单调钟**（`Instant::now()`）用于超时/deadline 属正确实践，不在本条射程。
- **ID 从一处铸造**：`agent-protocol::ids::fresh_id()` 是全仓唯一取随机的地方——要类型走 `id_newtype!` 的 `new()`，要**跨重建稳定**走 `from_string` 从 durable id 派生。随机 ID 必须**每次唯一**：store 拿 `EventId` 做幂等去重键，一个能重复的生成器会静默丢事件。
- 上面两条由 `crates/agent-testkit/tests/clock_and_id_boundaries.rs` 把守；它是**文本**闸门，盲区（函数指针、跨行拆开的调用）在该测试里有探针钉住。
- **schema 版本从一处铸造**：事件信封与 `doctor` 报的版本只有一个家——`agent_protocol::ids::SchemaVersion::current()`（`SchemaVersion::CURRENT`，见 `CONTRACT.md` C6.1）。生产路径不得再写 `SchemaVersion::from_string("…")` 字面量。它**故意不与码库版本联动**——不改日志形状的代码改动不该动它。export bundle 的 `EXPORT_SCHEMA_VERSION` 与 MCP 客户端自报版本是**别的** artifact 的版本，各自独立。由 `crates/agent-testkit/tests/schema_version_boundaries.rs` 把守。
- **事件词汇表只说真话**：`EventType::ALL` 不是文档，它是 `is_core()` 的判据（决定折叠遇到一个事件时**跳过**还是 fail closed），所以表里每一条都必须真的有人写或有人读。由 `crates/agent-testkit/tests/event_vocabulary_boundaries.rs` 把守：`ALL` 与 `pub const` 声明一一对应，且每条至少有一处 `events.rs` 之外的引用。**这是阈值较弱的那一半**——它要「一处引用」，不区分「写」与「读」；盲区（注释里提一句也算、运行时拼出的名字看不见）都有探针钉住。
- **公共类型带 `Serialize`/`Deserialize` 与 schema 版本**（见 `CONTRACT.md` C6.1）。读侧校验统一走 `agent_protocol::versioning`：`check` 拒绝未来与无迁移的旧版本，错误文本必须说出「读到的是什么、本 build 支持什么」。`#[serde(default)]` 只用于字段引入前的旧记录（缺省=当前），不是省掉校验的理由——有默认值之后仍要在入口校验。

## 2. 模块与公开面

- **一个 crate 一个职责**，职责写在 `lib.rs` 的 `//!` 里，并**说明它不做什么**。
- **公开面优先收口**：`pub use` 白名单优于整棵模块树直出。`store-*` / `loop` / `context` 只出一个类型；`kernel` / `protocol` / `ports` 整树公开。**整树公开是判定不改的口径**：这三棵树的公开面本来就被适配器与测试整树引用，改成白名单只是多一份每个新公共类型都要同步的清单。`protocol` 另有逐模块的引用闸门；`kernel` / `ports` 的公开面没有文本闸门，边界靠各自的 `//!` 文档与依赖方向矩阵维持——这是已知的、写下来的限度。
- **协议面不摆没人用的模块**：`agent-protocol` 的每个 `pub mod` 必须在**包外**至少被引用一处。它是**别人读的契约**，所以一个没人接的模块不是「闲置」，是把读者指到错的地方。由 `crates/agent-testkit/tests/protocol_module_surface.rs` 把守：文本闸门，只认 `agent_protocol::<mod>` 形态的路径，故**注释里写一句也算**、宏或重导出拼出来的看不见，两个盲区都有探针钉住。**只守这一个 crate**。
- **命令面与它的说明是一件事**：`apps/agent` 派发表的每个命令名必须在 `usage()` 里印出来。派发而说不出用法的命令，用户只能靠读源码发现。由 `crates/agent-testkit/tests/cli_command_surface.rs` 把守：文本闸门，只认「用法串某行的**第一个词**」，所以命令名出现在散文里（`run` 在 `runtime` 里）不算数，只在参数位出现的名字（`deny <id>` 里的 `id`）也不算数。盲区：它只认顶层名字，`plugin list` / `plugin disable <id>` 三行都只把 `plugin` 记成已说明——探针钉住。
- **单文件大模块可接受**（`agent-loop`、`agent-cli`），前提是模块文档说清了结构。

## 3. 错误类型

- 每个 crate 一个（或一组）`enum ...Error` + `thiserror`，变体名描述**对调用方意味着什么**，不是内部步骤名。
- **不得把分类拍平成 `String`**：如果调用方需要区分两类失败，就要有两个变体。反例在 `crates/agent-ports/src/tool_gateway.rs`（见 `CONTRACT.md` §7 #11）。
- **变了分类要同时改 `CONTRACT.md` §7**。

## 4. 命名

- **事件类型**：`domain/thing.happened` 形态的常量（`EventType::TOOL_STARTED` = `tool/started`），不用裸字符串。
- **ID 类型**：每种 ID 一个 newtype（`SessionId` / `TurnId` / `AttemptId` / `EventSeq` …），不互相混用。
- **文件 / 目录**：`snake_case`；会话数据目录名必须能安全用作目录名的单个段（`CONTRACT.md` C5.3）。

## 5. 文档注释

- crate 级 `//!` 写职责 + **明确的不做**。
- **声称的保证必须与实现同改。** 注释里写着「崩溃不会产生重复副作用」而实现留着真实窗口，比不写更糟。**写「保证」之前先问：它由谁执行？**
- **不在代码里引用已出树的 spec §号**（`//! ... (§12.6)` 这类）：要引就引本仓库的 `CONTRACT.md` 条号。`crates/agent-testkit/tests/doc_reference_boundaries.rs` 是这条规则的闸门：带小节的 `§N.N`、`§18` 附录号，以及「M6 progress」这类把工作推给已完成里程碑的措辞都会让它失败。
- 注释解释**为什么**，不复述**是什么**（代码已经说了是什么）。

## 6. 测试

- **行为驱动**：断言对外行为，不抄实现步骤。测试是「照着声称的保证写」，不是「照着实现写」——后者会精确地绕开真 bug。
- **落盘一律 `tempfile` / `TempDir`。禁共享固定临时路径**，**保持它**。
- **绝不在仓库目录里写测试产物**。仓库根出现的 `.agent-smoke-*` 残留来自手工冒烟，不是测试套件——别把测试改成那样。
- **真端点 / 真凭据的测试用 `#[ignore]`**，不用 `feature` 开关（打开方式：`-- --ignored`）。且**缺环境时必须失败或跳过得显眼，不得静默通过**。
- **测试名必须与它真正检查的东西一致**。
- **替身（Fake）放 `agent-testkit`，且必须与真实现行为一致、差异要写下来。** 「写下来」不是注释里提一句：**凡 port 的契约可机器判定，替身就得给出与真实现相同的答案**。差异**消除不掉**的，写进替身自己的注释、并用一条断言那个差异本身的测试钉住。忠实度测试在 `tests/fakes.rs`、`fakes_artifacts.rs`、`fakes_ports.rs`；由 `crates/agent-testkit/tests/fake_coverage.rs` 把守：文本闸门，`src/lib.rs` 里每个**被实现了某个 trait 的 `pub` 类型**都必须在 `tests/` 下某个文件里被提到。盲区：只查「有测试提到这个名字」，不查测试断言了什么；只管本 crate 的替身，别处手搓的够不着；名字嵌在更长的词里不算。
- **跨进程的替身（假服务器）是 `examples/` 目标，不住 `src/bin/`**：Cargo 为 `cargo test` 构建 examples、**不**为 `cargo build --workspace` 构建，所以 `src/bin/` 下的文件会被自动发现成**产品二进制**。测试端自己定位：`CARGO_BIN_EXE_*` 对 example **不生效**，按 `current_exe()` 推 `<target>/<profile>/examples/`，缺了就 `cargo build -p <pkg> --example <name>`（范式见 `agent-tools-mcp/tests` 与 `apps/agent/tests` 的定位函数）。由 `crates/agent-testkit/tests/fixture_target_boundaries.rs` 把守。
- **「进程死了」用重执行自己的测试二进制来造**（范式：`apps/agent/tests/crash_matrix.rs`）：父进程 `current_exe()` + `--exact <子测试名> --ignored` 起子进程，子进程按环境变量装配产品栈并跑一轮，父进程等子进程写出信号（或日志里读得到 `tool/started`）后 `Child::kill` ——Windows 上是 `TerminateProcess`，无析构、无收尾、无最后一次写。子进程跑完没死就以固定非零码退出，父进程据此报「崩溃点没打中」：一条什么都没崩的崩溃测试不许静默通过。`Stdio::null()` 在这台机器上不可用（NUL 设备被 Low 完整性拦），子进程的 stdio 用管道。
- **验收套件驱动产品自己的装配，不另造一份**：`m4` / `m5` / `m6` 三套验收必须调用 `agent::stack::assemble`（bin 也调它），套件只把环境供不出的两件事注进去——文件在哪、模型是谁（`stack::Inputs`）。`apps/agent` 因此有 lib target 与 bin target 同名：bin 的模块默认私有，装配要能被测试调就不能住在 bin 里。由 `crates/agent-testkit/tests/acceptance_assembly_boundaries.rs` 把守：文本闸门，只认 `Scheduler::new(` / `DefaultAgentLoop::new(` / `GuardedToolProvider::new(` 三个构造器（三个凑齐才是「造了一份装配」）。
- **闸门之外要反向实测**：文本闸门只能拦写法，拦不住「装配改对了、套件没跟上」这类真问题——抽查方式是**改产品、看套件红不红**（例如 `stack.rs` 里不挂 advisor、composite 里摘掉 git 家族、摘掉 registry 面）。这类抽查不进 CI，做没做记在工作记录里。
- **架构守卫解析 `[dependencies]` 与 `[dev-dependencies]`**：运行期方向与测试期方向各有一张逐 crate 封闭的矩阵，新加一条 `agent-*` 边必须同时改矩阵，否则测试红。

## 7. 提交信息

- conventional 前缀（`feat:` / `fix:` / `docs:` / `refactor:` …）+ **散文体正文**：说**为什么这么改**、取舍是什么、遗留了什么。
- **破坏性改动在正文里点名**（改了哪条契约、哪些命令的退出码会变）。
- 这个风格是这个仓库的既有习惯（`git log` 里一路可见）。**裁决类改动**（判据 C）额外要求：把裁决填进 `CONTRACT.md` §7，**不能只留在提交信息里**。

## 8. 文档面规则

| 文档 | 什么进这里 | 什么**不**进 |
|---|---|---|
| `PROJECT.md` | 目标、方向、稳定架构与依赖方向、crate 地图、技术栈、怎么跑 | 缺陷清单、里程碑叙事、阶段计划、实施过程、实验数据、版本演进、修复流水 |
| `CONTRACT.md` | 不变量与判据、术语与分类口径（裁决记录）、**已知偏离** | 待办清单、进度 |
| `STYLE.md`（本文件） | 风格、工程规则、门与基线 | 具体缺陷 |

三条硬规则：

1. **里程碑叙事、质量基线数字、缺口台账、阶段计划、实施过程、实验数据、版本演进与修复流水一律不进仓库；裁决记录只进 `CONTRACT.md` §7。**
2. **改契约必须同时改代码与 `CONTRACT.md`**——判据不在文档里，第一个「顺手改好」的人就会把它改回去。
3. **`CONTRACT.md` §8 必须在收口时清空**：每行要么修掉，要么判定不改并附理由，要么移出范围；不得留待定。
