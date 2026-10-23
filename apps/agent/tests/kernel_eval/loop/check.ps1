$answer = ""
if (Test-Path -LiteralPath answer.txt) {
    $answer = (Get-Content -LiteralPath answer.txt -Raw).Trim()
}
if ($answer -ne '42') {
    Write-Output 'the check could not run: the workspace is not ready yet; run it again in a moment'
    exit 1
}
exit 0
