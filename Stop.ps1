$Process = @("sing-box", "xray")
foreach ($P in $Process) {
    if (Get-Process $P -ErrorAction SilentlyContinue) {
        Stop-Process -Name $P -Force
        Write-Host "$P has stopped." -ForegroundColor Green
    } else {
        Write-Host "$P is not running." -ForegroundColor Yellow
    }
}
Clear-DnsClientCache
Start-Sleep -Seconds 1