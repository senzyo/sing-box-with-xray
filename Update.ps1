# 停止运行 sing-box 和 xray
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

# ---- 公共函数 ----
# 校验 Hash
function VerifyHash {
    $Digest = $Response.assets | Where-Object { $_.name -eq "$FileName.zip" } | Select-Object -ExpandProperty digest
    $RemoteHash = $Digest.Split(':')[-1]
    $LocalHash = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLower()
    Write-Host "Verifying SHA256 checksum... " -NoNewline
    if ($RemoteHash -eq $LocalHash) {
        Write-Host "Correct!" -ForegroundColor Green
        return $true
    } else {
        Write-Host "Wrong!" -ForegroundColor Red
        return $false
    }
}

# 升级
function Upgrade {
    $Url = $Response.assets | Where-Object { $_.name -eq "$FileName.zip" } | Select-Object -ExpandProperty browser_download_url
    do {
        if (Test-Path -Path $ZipPath) {
            Remove-Item $ZipPath -Force
        }
        Write-Host "Downloading..."
        Invoke-WebRequest -OutFile $ZipPath -Uri "https://gh-proxy.org/$Url"
        $Correct = VerifyHash
        if ($Correct) {
            $script:Cover = $true
        } else {
            Write-Host "Retry Downloading..."
            Start-Sleep -Seconds 1
        }
    } until ($Correct)
}

# 检查更新
function CheckUpdate ($ExeName) {
    $LocalVersionStr = "0.0.0"
    if (Test-Path -Path $ExePath) {
        $VersionOutput = (& $ExePath version) 2>&1
        if ($VersionOutput[0] -match "([\d.]+)") {
            $LocalVersionStr = $Matches[1]
        }
    }
    $LocalVersionObj = [System.Version]$LocalVersionStr
    $RemoteVersionObj = [System.Version]$RemoteVersionStr
    if ($RemoteVersionObj -gt $LocalVersionObj) {
        Write-Host "version: $LocalVersionStr -> " -NoNewline
        Write-Host "$RemoteVersionStr" -ForegroundColor Yellow
        Upgrade
    }
    else {
        Write-Host "Up to date: " -NoNewline
        Write-Host "$ExeName $LocalVersionStr" -ForegroundColor Green
    }
}
# ---- 公共函数 结束 ----

$OutDir = "$env:USERPROFILE\Apps\sing-box-with-xray"

# ---- 更新 sing-box ----
$ExePath = "$OutDir\sing-box.exe"
$Response = Invoke-RestMethod -Uri "https://api.github.com/repos/SagerNet/sing-box/releases/latest" -Method Get
$TagName = $Response.tag_name
$RemoteVersionStr = $TagName.TrimStart('v')
$FileName = "sing-box-$RemoteVersionStr-windows-amd64"
$ZipPath = "$OutDir\$FileName.zip"

CheckUpdate sing-box

# 解压缩并覆盖 sing-box
if ($script:Cover) {
    Expand-Archive -Path $ZipPath -DestinationPath $OutDir -Force
    Move-Item -Path "$OutDir\$FileName\sing-box.exe" -Destination "$OutDir\sing-box.exe" -Force
    Remove-Item -Recurse "$OutDir\$FileName*" -Force
}
# ---- 更新 sing-box 结束 ----

# ---- 更新 xray ----
$ExePath = "$OutDir\xray.exe"
$Response = Invoke-RestMethod -Uri "https://api.github.com/repos/XTLS/Xray-core/releases/latest" -Method Get
$TagName = $Response.tag_name
$RemoteVersionStr = $TagName.TrimStart('v')
$FileName = "Xray-windows-64"
$ZipPath = "$OutDir\$FileName.zip"
$script:Cover = $false

CheckUpdate xray

# 解压缩并覆盖 xray
if ($script:Cover) {
    Expand-Archive -Path $ZipPath -DestinationPath "$OutDir\$FileName" -Force
    Move-Item -Path "$OutDir\$FileName\xray.exe" -Destination "$OutDir\xray.exe" -Force
    Remove-Item -Recurse "$OutDir\$FileName*" -Force
}
# ---- 更新 xray 结束 ----

pause