$out = & "$PSScriptRoot\package.ps1" -DryRun 2>&1
if ($LASTEXITCODE -ne 0) { throw "package dry-run failed: $($out -join "`n")" }
$text = $out -join "`n"
foreach ($name in @('agent.exe', '启动爻爻.cmd', 'config.example.toml', 'README.txt')) {
    if ($text -notmatch [regex]::Escape($name)) { throw "package omitted $name" }
}
Write-Host 'PASS'