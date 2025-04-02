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

$WorkDir = "$env:USERPROFILE\Apps\sing-box-with-xray"
$ConfigPath = "$WorkDir\sing-box.json"
$TempPath = "$WorkDir\sing-box.json.temp"
$RandomHex = -join (1..3 | ForEach-Object { "{0:x2}" -f (Get-Random -Min 0 -Max 256) })
if (Test-Path $ConfigPath) {
    $JsonResult = & $WorkDir\jq.exe --arg new_name "$RandomHex" '(.inbounds[] | select(.type == \"tun\") | .interface_name) = $new_name' $ConfigPath 2>$null
    if ($LASTEXITCODE -eq 0 -and $JsonResult) {
        [System.IO.File]::WriteAllLines($TempPath, $JsonResult)
        Move-Item -Path $TempPath -Destination $ConfigPath -Force
        Write-Host "Success: TUN interface name randomized." -ForegroundColor Green
    } else {
        Write-Host "Error: TUN interface name randomization failed." -ForegroundColor Red
        pause
        exit
    }
} else {
    Write-Host "Error: File not found at $ConfigPath" -ForegroundColor Red
    pause
    exit
}

Write-Host "Start sing-box and xray..." -ForegroundColor Cyan
Start-Process -FilePath "$WorkDir\sing-box.exe" -ArgumentList "run -D $WorkDir -c $WorkDir\sing-box.json" -WindowStyle Hidden
Start-Sleep -Seconds 1
Start-Process -FilePath "$WorkDir\xray.exe" -ArgumentList "run -c $WorkDir\xray.json" -WindowStyle Hidden
Start-Sleep -Seconds 1
Get-Process -Name sing-box,xray
Start-Sleep -Seconds 1