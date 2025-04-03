$ESC = [char]27
$Black = "$ESC[90m"
$Red = "$ESC[91m"       # [Error]
$Green = "$ESC[92m"     # [Success]
$Yellow = "$ESC[93m"    # [Warning]
$Blue = "$ESC[94m"
$Magenta = "$ESC[95m"
$Cyan = "$ESC[96m"      # [Notice]
$White = "$ESC[97m"
$NC = "$ESC[0m"         # No Color

$currentPrincipal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
$isAdmin = $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

if (-not $isAdmin) {
    Write-Host "${Red}[Error]${NC} Please run this script as Administrator."
    exit 1
}

$Process = @("sing-box", "xray")
foreach ($P in $Process) {
    if (Get-Process $P -ErrorAction SilentlyContinue) {
        Stop-Process -Name $P -Force
        Write-Host "${Green}[Success]${NC} $P has stopped."
    } else {
        Write-Host "${Yellow}[Warning]${NC} $P is not running."
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
        Write-Host "${Green}[Success]${NC} TUN interface name randomized."
        Write-Host "${Cyan}[Notice]${NC} Starting sing-box..."
        Start-Process -FilePath "$WorkDir\sing-box.exe" -ArgumentList "run -D $WorkDir -c $ConfigPath" -WindowStyle Hidden
    } else {
        Write-Host "${Red}[Error]${NC} TUN interface name randomization failed."
        pause
        exit
    }
} else {
    Write-Host "${Red}[Error]${NC} File not found: $ConfigPath"
    pause
    exit
}

Write-Host "${Cyan}[Notice]${NC} Starting xray..."
$ConfigPath = "$WorkDir\xray.json"
if (Test-Path $ConfigPath) {
    Start-Process -FilePath "$WorkDir\xray.exe" -ArgumentList "run -c $ConfigPath" -WindowStyle Hidden
} else {
    Write-Host "${Red}[Error]${NC} File not found: $ConfigPath"
    pause
    exit
}

Start-Sleep -Seconds 2
foreach ($P in $Process) {
    if (Get-Process $P -ErrorAction SilentlyContinue) {
        Write-Host "${Green}[Success]${NC} $P is running."
    } else {
        Write-Host "${Red}[Error]${NC} $P is not running."
    }
}
Start-Sleep -Seconds 1
