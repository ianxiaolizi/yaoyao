# The check's first line is the failure a later turn asks about; the list under
# it is what makes the failed call itself a large enough message that a
# compaction across turns has to summarize it rather than carry it whole. The
# failures record keeps the first line, which is where the expected value is.
$expected = '42'
$answerPath = Join-Path $PSScriptRoot 'answer.txt'
$missing = -not (Test-Path -LiteralPath $answerPath)
$answer = ''
if (-not $missing) {
    $answer = (Get-Content -LiteralPath $answerPath -Raw).Trim()
}
if ($missing -or $answer -ne $expected) {
    $got = if ($missing) { 'missing' } else { $answer }
    Write-Output "assertion failed: expected $expected, got $got"
    Write-Output 'records not verified:'
    $records = Join-Path $PSScriptRoot 'data\records.csv'
    Get-Content -LiteralPath $records | ForEach-Object {
        Write-Output ('unverified: ' + $_.Trim())
    }
    exit 1
}
exit 0
