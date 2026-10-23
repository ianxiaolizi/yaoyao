$answer = (Get-Content -LiteralPath answer.txt -Raw).Trim()
if ($answer -ne '42') {
    Write-Output 'answer.txt must say 42'
    exit 1
}
$flag = (Get-Content -LiteralPath flag.txt -Raw).Trim()
if ($flag -ne 'on') {
    Write-Output 'flag.txt must say on'
    exit 1
}
exit 0
