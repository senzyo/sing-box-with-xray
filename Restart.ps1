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

$workDir = "$env:USERPROFILE\Apps\sing-box-with-xray"
$configPath = "$workDir\sing-box.json"
$tempPath = "$workDir\sing-box.json.temp"
$randomHex = -join (1..3 | ForEach-Object { "{0:x2}" -f (Get-Random -Min 0 -Max 256) })
if (Test-Path $configPath) {
    $jsonResult = & jq --arg new_name "$randomHex" '(.inbounds[] | select(.type == \"tun\") | .interface_name) = $new_name' $configPath 2>$null
    if ($LASTEXITCODE -eq 0 -and $jsonResult) {
        [System.IO.File]::WriteAllLines($tempPath, $jsonResult)
        Move-Item -Path $tempPath -Destination $configPath -Force
        Write-Host "Success: TUN interface name randomized." -ForegroundColor Green
    } else {
        Write-Host "Error: TUN interface name randomization failed." -ForegroundColor Red
        pause
        exit
    }
} else {
    Write-Host "Error: File not found at $configPath" -ForegroundColor Red
    pause
    exit
}

Write-Host "Start sing-box and xray..." -ForegroundColor Cyan
Start-Process -FilePath "$workDir\sing-box.exe" -ArgumentList "run -D $workDir -c $workDir\sing-box.json" -WindowStyle Hidden
Start-Sleep -Seconds 1
Start-Process -FilePath "$workDir\xray.exe" -ArgumentList "run -c $workDir\xray.json" -WindowStyle Hidden
Start-Sleep -Seconds 1
Get-Process -Name sing-box,xray
Start-Sleep -Seconds 1