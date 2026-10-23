# Security Policy

## Supported versions

爻爻目前处于 developer preview。只有最新 `main` 分支接受安全修复。

## Reporting a vulnerability

请通过 GitHub 的私密安全公告报告：

https://github.com/ianxiaolizi/yaoyao/security/advisories/new

不要在公开 issue 中披露未修复的漏洞、凭据或可利用细节。

报告中请尽量包含：

- 受影响版本或提交
- 复现步骤
- 影响范围
- 是否涉及命令执行、路径逃逸、凭据泄漏、沙箱绕过或提示注入

## Security model

爻爻是一个会执行命令和修改文件的 Agent。审批和沙箱用于降低风险，但不构成对恶意多租户环境的完整隔离。

安全使用要求：

- 只在可信工作区运行
- 不要对不可信用户开放 `shell.run`
- 不要把 `AGENTS.md` 视为安全策略
- 不可信代码优先使用 `app-container`
- 为每个任务使用独立工作区和最低必要权限
- 不要把本项目当前版本部署为公网多租户服务
