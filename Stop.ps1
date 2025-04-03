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
