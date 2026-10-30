param([string]$LogFile)
$tail = (Get-Content $LogFile -Tail 80 | Out-String)
$tail = $tail -replace '%','%25' -replace "`r",'%0D' -replace "`n",'%0A'
Write-Host "::error title=SelfTestFailure::$tail"
