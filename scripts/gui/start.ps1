# 启动爻爻 Web GUI：构建（可选）、起服务、自动打开带令牌的页面。
#
# 双击仓库根目录的 启动爻爻.cmd 即可。这个脚本不写事件、不碰会话，
# 只负责选路径、起 `agent web` 和打开它打印的那一条 URL。
[CmdletBinding()]
param(
    [int]$Port = 7441,
    [switch]$SkipBuild,
    [switch]$NoBrowser,
    [switch]$DryRun,
    [switch]$NoPause
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$exe = Join-Path $repo 'target\debug\agent.exe'
$log = Join-Path $repo 'target\yaoyao-web.log'

function Stop-WithMessage([string]$message) {
    Write-Host ''
    Write-Host $message -ForegroundColor Red
    if (-not $NoPause) {
        Read-Host '按 Enter 关闭'
    }
    exit 1
}

# 默认用本机已有的工作区；没有就退回仓库本身。
# 这两个变量如果已经设置，尊重调用者，不覆盖。
if (-not $env:AGENT_WORKSPACE_DIR) {
    if (Test-Path 'G:\yaoyao-work') {
        $env:AGENT_WORKSPACE_DIR = 'G:\yaoyao-work'
    } else {
        $env:AGENT_WORKSPACE_DIR = $repo
    }
}
if (-not $env:AGENT_DATA_DIR) {
    $legacy = Join-Path $env:AGENT_WORKSPACE_DIR '.agent'
    if (Test-Path $legacy) {
        $env:AGENT_DATA_DIR = $legacy
    } else {
        $env:AGENT_DATA_DIR = Join-Path $env:AGENT_WORKSPACE_DIR '.agent-data'
    }
}

# 可选的机器本地设置：不要把它提交进仓库。
$local = Join-Path $PSScriptRoot 'start.local.ps1'
if (Test-Path $local) {
    . $local
}

Write-Host ''
Write-Host '  爻爻 · 本地 Web' -ForegroundColor Cyan
Write-Host "  工作区  $env:AGENT_WORKSPACE_DIR"
Write-Host "  数据    $env:AGENT_DATA_DIR"
Write-Host "  端口    $Port"
Write-Host ''

# 端口被占用时不要把第二个服务叠上去，否则同一个会话会有两个驱动者。
$listener = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($listener) {
    $owner = (Get-Process -Id $listener.OwningProcess -ErrorAction SilentlyContinue).ProcessName
    Stop-WithMessage "端口 $Port 已被占用（$owner，PID $($listener.OwningProcess)）。`n关掉旧窗口，或用 -Port 7442 换一个端口。"
}

if (-not $SkipBuild) {
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargo) {
        Stop-WithMessage '找不到 cargo。请先安装 Rust，或用已经构建好的 target\debug\agent.exe。'
    }
    $tmp = Join-Path $repo 'target\tmp'
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $env:TEMP = $tmp
    $env:TMP = $tmp
    Push-Location $repo
    try {
        Write-Host '  正在构建 agent…' -ForegroundColor DarkGray
        & $cargo.Source build -p agent
        if ($LASTEXITCODE -ne 0) {
            Stop-WithMessage '构建失败。上面的 cargo 输出就是原因。'
        }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path $exe)) {
    Stop-WithMessage "找不到 $exe。先运行一次 cargo build -p agent。"
}

# 之前机器上的 target 目录可能继承了 Low integrity，agent 写 .agent 会拒绝访问。
# 这个调用失败不致命：普通终端本来就有权限。
try {
    & icacls.exe $exe /setintegritylevel Medium 2>$null | Out-Null
} catch {
    # 不是错误：不是每台机器都需要这一步。
}

if ($DryRun) {
    Write-Host "  exe     $exe" -ForegroundColor DarkGray
    Write-Host "  log     $log" -ForegroundColor DarkGray
    Write-Host '  dry-run：不启动服务。' -ForegroundColor DarkGray
    exit 0
}

if (Test-Path $log) {
    Remove-Item $log -Force -ErrorAction SilentlyContinue
}
$urlSeen = $false
$host.UI.RawUI.WindowTitle = "爻爻 Web · $Port"

Write-Host '  启动中；看到 URL 会自动打开浏览器。Ctrl+C 停止。' -ForegroundColor DarkGray
Write-Host ''

try {
    & $exe web --port $Port 2>&1 | ForEach-Object {
        $line = $_.ToString()
        Write-Host $line
        # 日志会被留下来分享；令牌只出现在控制台和浏览器的地址栏里。
        $logLine = $line -replace '(#t=)[0-9a-f]+', '$1<redacted>'
        Add-Content -Path $log -Value $logLine -Encoding UTF8

        if (-not $urlSeen -and $line -match 'http://127\.0\.0\.1:\d+/\#t=[0-9a-f]+') {
            $url = $Matches[0]
            $urlSeen = $true
            Write-Host ''
            Write-Host "  页面  $url" -ForegroundColor Green
            Write-Host '  （令牌只在这个进程的生命周期内有效；重启后看新 URL）' -ForegroundColor DarkGray
            Write-Host ''
            if (-not $NoBrowser) {
                Start-Sleep -Milliseconds 250
                Start-Process -FilePath $url
            }
        }
    }
} catch {
    Stop-WithMessage "启动失败：$($_.Exception.Message)"
}

if (-not $urlSeen) {
    Stop-WithMessage "agent web 退出了，但没有打印带令牌的 URL。`n看 $log。"
}

if (-not $NoPause) {
    Read-Host '服务已停止。按 Enter 关闭'
}
