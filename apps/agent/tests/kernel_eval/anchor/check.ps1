$lines = Get-Content -LiteralPath report.txt
if ($lines[1] -ne 'value = 41') {
    Write-Output 'line 2 of report.txt must still read: value = 41'
    exit 1
}
if ($lines[3] -ne 'value = 42') {
    Write-Output 'line 4 of report.txt must read: value = 42'
    exit 1
}
exit 0
