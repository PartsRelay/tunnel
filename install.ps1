#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Installs the latest Parts Relay tunnel on Windows and starts it at boot.
.PARAMETER BinaryPath
  Install this executable instead of downloading the latest release.
.PARAMETER NoService
  Do not register the scheduled task.
#>
param(
    [string]$BinaryPath,
    [switch]$NoService
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$releases = 'https://github.com/PartsRelay/tunnel/releases'
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$target = "$arch-pc-windows-msvc"
$archive = "partsrelay-tunnel-$target.zip"
$installDir = Join-Path $env:ProgramFiles 'PartsRelay'
$configDir = Join-Path $env:ProgramData 'PartsRelay'
$exe = Join-Path $installDir 'partsrelay-tunnel.exe'
$config = Join-Path $configDir 'tunnel.toml'
$taskName = 'PartsRelay Tunnel'
$account = 'NT AUTHORITY\LOCAL SERVICE'
# Well known SIDs: their names change with the language
$administrators = '*S-1-5-32-544'
$system = '*S-1-5-18'
$localService = '*S-1-5-19'

function Set-Access([string]$Path, [string]$ServiceRights) {
    icacls $Path /inheritance:r /grant:r "${administrators}:(OI)(CI)F" "${system}:(OI)(CI)F" `
        "${localService}:(OI)(CI)$ServiceRights" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Cannot set the permissions of $Path (icacls ended with $LASTEXITCODE)" }
}

New-Item -ItemType Directory -Force -Path $installDir, $configDir | Out-Null

if ($BinaryPath) {
    Copy-Item -Force $BinaryPath $exe
}
else {
    $work = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Path $work | Out-Null

    try {
        Write-Host 'Reading the latest release'
        $manifest = Invoke-RestMethod "$releases/latest/download/manifest.json"
        $asset = $manifest.assets.$target
        if (-not $asset) { throw "Release $($manifest.tag) has no build for $target" }

        Write-Host "Downloading $archive of $($manifest.tag)"
        $zip = Join-Path $work $archive
        Invoke-WebRequest "$releases/download/$($manifest.tag)/$archive" -OutFile $zip -UseBasicParsing

        $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($hash -ne $asset.sha256) { throw "$archive does not match its SHA-256" }

        Expand-Archive -Path $zip -DestinationPath $work -Force
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
        Copy-Item -Force (Join-Path $work 'partsrelay-tunnel.exe') $exe
    }
    finally {
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    }
}

# The tunnel replaces its own executable when it updates
Set-Access $installDir 'M'
# Only the tunnel and the administrators read the token
Set-Access $configDir 'R'

if (-not (Test-Path $config)) {
    @'
# Parts Relay tunnel. Every setting is described in the README.

# The token Parts Relay gave you for this tunnel
token = ""

# The only targets the tunnel may reach, as host:port
allow = []
'@ | Set-Content -Encoding utf8 $config
    Write-Host "Created $config: set the token and the allow list"
}

Write-Host "Installed $(& $exe --version) in $installDir"

if ($NoService) { return }

$action = New-ScheduledTaskAction -Execute $exe -Argument 'run'
$atStartup = New-ScheduledTaskTrigger -AtStartup
# Starts it again a minute after an update or crash
$everyMinute = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 1)
$settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
$principal = New-ScheduledTaskPrincipal -UserId $account -LogonType ServiceAccount

Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $atStartup, $everyMinute `
    -Settings $settings -Principal $principal -Force | Out-Null

Write-Host "Task '$taskName' registered. Edit $config, then run: Start-ScheduledTask -TaskName '$taskName'"
