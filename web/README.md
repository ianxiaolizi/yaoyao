# web · 爻爻前端（M7）

> 技术壳 **B**：Vite + TypeScript，无框架。选型依据见
> [`G:\yaoyao-gui-design\plan\08-tech-shell-spike.md`](../../yaoyao-gui-design/plan/08-tech-shell-spike.md)。
> 设计来源：`G:\yaoyao-gui-design\design\`（token / 组件 / 状态）。
> 自 M7 起，**构建产物直接编进 `agent web` 的二进制**（`crates/agent-web/build.rs`），
> 旧的手写 `crates/agent-web/static/` 壳已下线。

## 目录

```
web/
  index.html            入口；构建后换成带哈希的 assets
  src/
    api.ts              fetch + token + SSE 解析（Bearer，不落存储）
    state.ts            State / Action / Store（纯 reducer）
    view.ts             壳、转录、门、证据侧板、成果台、命令面板
    format.ts           时间、token、Markdown-lite、状态
    main.ts             启动、会话切换、SSE 生命周期、动作
    artifacts.ts        从 tool 输出里发现 artifact://sha256/<64>
    office.ts           docx / xlsx 的页面侧解包与抽取
    styles/
      tokens.css        来自 design/tokens.css（冻结）
      system.css        来自 design/system.css
      shell.css         新壳布局；类名统一 y- 前缀
  shots/                验收截图（m4–m9）
  dist/                 构建产物，随仓库提交
```

## 现在有什么

- 会话栏：真实 `/api/sessions`，待批标记，新任务；已归档的进底部分区；右缘可拖宽
- 顶栏：标题 / 第 N 轮 / 状态 / 详情 / ⋯ 菜单（导出、分叉、归档、删除）
- 对话：真实 `/api/sessions/{id}/transcript`；旧轮折成一行 + 「更早的 N 轮」
- 当前轮：目标、思考折叠、行动行（参数 / 耗时 / 输出）、回答、轮末用量、推理链入口
- 审批门：`pending` / `expired`，四决定走 `POST /approvals/{id}`，过期走 `/resume`，
  TTL 逐秒倒计时
- 输入框：新轮 `POST /turns`；运行中改走 `/steer`；停止 `POST /interrupt`；
  模型 chip 切换 `POST /model`（下一轮生效）
- 证据侧板：计划 / 改动 / 推理链（`/why`）/ 上下文 / 子代理（递归两层）/ 定时；
  左缘可拖宽
- 成果台：artifact 发现、图片 / HTML（sandbox iframe）/ Markdown / 文本 / PDF /
  docx / xlsx（页面侧解包，见 `src/office.ts`）、12% 条 / 展开 / 全屏，Esc 逐层退出
- 命令面板（⌘K / Ctrl K）、导出 Markdown / JSON、分叉这一轮
- 流：fetch-streaming SSE，Bearer 头，断线指数退避，`from=<seq>` 续传
- 1440 / 1280 / 1080 / 720 / 390；`prefers-reduced-motion` 下无过渡与闪烁

## 开发

```powershell
# 后端（另开窗口）；数据目录要在工作区之外
$env:AGENT_DATA_DIR='G:\yaoyao-data'
G:\yaoyao-public\target\debug\agent.exe web --port 7431 --provider echo

# 前端
cd G:\yaoyao-public\web
pnpm install
pnpm dev
```

把 agent 打印的 URL 里的 `#t=<token>` 拼到 `http://localhost:5173/#t=<token>`；
dev server 把 `/api` 代理到 `127.0.0.1:7431`。

## 生产与验收

```powershell
cd G:\yaoyao-public\web
pnpm check                 # tsc
pnpm build                 # 写 dist/；改完 src 必须重跑，dist 要一起提交

cd G:\yaoyao-public
cargo build -p agent       # build.rs 把 web/dist 编进二进制
node scripts/gui/smoke.mjs # 直接打 target\debug\agent.exe，不看 Vite
node scripts/gui/perf.mjs  # 本机性能对照 perf-baseline.json
```

`cargo build` 不会自己跑 `pnpm build`：`dist/` 是提交进仓库的产物，
`启动爻爻.cmd` 的启动脚本会在 `src/` 比 `dist/` 新时自动重建。
`web` CI job 会重跑 `pnpm build` 并检查 `dist/` 没有漂移。

## 还没做

- Office 保真：只保证内容看得见，不还原样式、图片、合并单元格与日期格式
- 真实 vendor 模型：本机没有 `AGENT_MODEL_API_KEY`；有 key 时
  `node scripts/gui/capture-real-chain.mjs --live` 可以重采真实执行链
- 折叠的跨进程缓存：`Projection` 的检查点还没接进 web 读模型
