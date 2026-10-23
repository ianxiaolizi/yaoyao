$bad = Get-ChildItem -LiteralPath notes -Filter *.md | Select-String -Pattern 'value: 41' -SimpleMatch
if ($bad) {
    Write-Output 'a note disagrees with sum.txt'
    exit 1
}
exit 0
