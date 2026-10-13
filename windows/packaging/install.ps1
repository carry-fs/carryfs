# Local system installer; also packaged as install-system.ps1 in Windows releases.
# Run as Administrator next to carryfs-windows.exe. -Uninstall removes CarryFS.
[CmdletBinding()]
param([switch]$Uninstall, [string]$ExpectedVersion)
$ErrorActionPreference = 'Stop'

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this system installer as Administrator, or use the download installer to request elevation.'
}

function Invoke-CarryFSServiceCommand([string[]]$Arguments) {
    & sc.exe @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Service command failed with exit code $LASTEXITCODE." }
}

$target = Join-Path $env:ProgramFiles 'CarryFS'
$serviceName = 'CarryFS'
$mutex = New-Object Threading.Mutex($false, 'Global\CarryFSInstaller')
$locked = $false
$work = $null
$keepBackup = $false
try {
    try { $locked = $mutex.WaitOne(0) }
    catch [Threading.AbandonedMutexException] { $locked = $true }
    if (-not $locked) { throw 'Another CarryFS installation is running.' }
    $service = Get-Service $serviceName -ErrorAction SilentlyContinue
    if ($Uninstall) {
        if ($service) {
            # Stopping the service publishes and ejects every mounted volume.
            Stop-Service $serviceName
            $service.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(60))
            Invoke-CarryFSServiceCommand -Arguments @('delete', $serviceName)
        }
        if (Test-Path $target) { Remove-Item -LiteralPath $target -Recurse -Force }
        return
    }

    $marker = Join-Path $target 'VERSION'
    $current = 'unrecorded'
    if (Test-Path $marker -PathType Leaf) { $current = (Get-Content $marker -Raw).Trim() }
    if ($PSBoundParameters.ContainsKey('ExpectedVersion') -and $current -cne $ExpectedVersion) {
        throw 'The installed version changed during download; run the installer again.'
    }
    $key = 'HKLM:\SOFTWARE\WOW6432Node\WinFsp'
    if (-not (Test-Path $key)) { throw 'Install WinFsp 2.1 or later: https://winfsp.dev/rel/' }
    $winfsp = (Get-ItemProperty $key).InstallDir
    $dll = Join-Path $winfsp 'bin\winfsp-x64.dll'
    if (-not (Test-Path $dll -PathType Leaf)) { throw 'WinFsp is incomplete; reinstall it from https://winfsp.dev/rel/.' }
    $versionInfo = (Get-Item $dll).VersionInfo
    if ($versionInfo.FileMajorPart -lt 2 -or ($versionInfo.FileMajorPart -eq 2 -and $versionInfo.FileMinorPart -lt 1)) {
        throw 'Update WinFsp to 2.1 or later: https://winfsp.dev/rel/'
    }
    $mounted = @(Get-CimInstance Win32_LogicalDisk -Filter "FileSystem='CarryFS'")
    if ($mounted.Count) { throw 'Eject all CarryFS volumes before installing or upgrading.' }
    $serviceInfo = Get-CimInstance Win32_Service -Filter "Name='CarryFS'"
    $servicePid = 0
    if ($serviceInfo) { $servicePid = $serviceInfo.ProcessId }
    $foreground = @(Get-Process -Name carryfs-windows -ErrorAction SilentlyContinue | Where-Object { $_.Id -ne $servicePid })
    if ($foreground.Count) { throw 'A foreground CarryFS adapter is running. Stop it before installing.' }

    $work = Join-Path $env:ProgramFiles ('.CarryFS-install-' + [guid]::NewGuid().ToString('N'))
    $ready = Join-Path $work 'ready'
    $backup = Join-Path $work 'previous'
    New-Item -ItemType Directory -Path $ready | Out-Null
    if (Test-Path $target) {
        Get-ChildItem -LiteralPath $target -Force | Copy-Item -Destination $ready -Recurse -Force
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'carryfs-windows.exe') -Destination $ready -Force
    Copy-Item -LiteralPath (Join-Path $winfsp 'License.txt') -Destination (Join-Path $ready 'WinFsp-License.txt') -Force
    $packageVersion = Join-Path $PSScriptRoot 'VERSION'
    if (Test-Path $packageVersion -PathType Leaf) {
        $value = (Get-Content $packageVersion -Raw).Trim()
        if ($value -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') { throw 'Invalid package version.' }
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'carryfs.exe') -Destination $ready -Force
        Copy-Item -LiteralPath $packageVersion -Destination $ready -Force
    } else {
        # A locally built adapter has no release version; never retain an old stamp.
        $oldMarker = Join-Path $ready 'VERSION'
        if (Test-Path $oldMarker) { Remove-Item -LiteralPath $oldMarker }
    }
    $wasRunning = $service -and $service.Status -ne [ServiceProcess.ServiceControllerStatus]::Stopped
    $replaced = $false
    $createdService = $false
    try {
        if ($service -and $wasRunning) {
            Stop-Service $serviceName
            $service.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(60))
        }
        if (Test-Path $target) { [IO.Directory]::Move($target, $backup) }
        $replaced = $true
        [IO.Directory]::Move($ready, $target)
        if (-not $service) {
            $binary = '"{0}" service' -f (Join-Path $target 'carryfs-windows.exe')
            New-Service -Name $serviceName -BinaryPathName $binary -DisplayName 'CarryFS automount' `
                -Description 'Mounts inserted CarryFS disks and ejects them on safe removal.' `
                -StartupType Automatic | Out-Null
            $createdService = $true
        }
        Start-Service $serviceName
        (Get-Service $serviceName).WaitForStatus([ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(60))
    } catch {
        $failure = $_
        try {
            if ($createdService -or $service) {
                $active = Get-Service $serviceName
                if ($active.Status -ne [ServiceProcess.ServiceControllerStatus]::Stopped) {
                    Stop-Service $serviceName
                    $active.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(60))
                }
            }
            if ($createdService) { Invoke-CarryFSServiceCommand -Arguments @('delete', $serviceName) }
            if ($replaced) {
                if (Test-Path $target) { Remove-Item -LiteralPath $target -Recurse -Force }
                if (Test-Path $backup) { [IO.Directory]::Move($backup, $target) }
            }
            if ($wasRunning) { Start-Service $serviceName }
        } catch {
            $keepBackup = $true
            throw "Installation failed: $($failure.Exception.Message). Restoration also failed: $($_.Exception.Message). Keep the backup at $work and restore it before mounting CarryFS."
        }
        throw $failure
    }
    Write-Host "Installed CarryFS system files in $target; the automount service is running."
} finally {
    try {
        if ($work -and -not $keepBackup) { Remove-Item -LiteralPath $work -Recurse -Force }
    } finally {
        if ($locked) { $mutex.ReleaseMutex() }
        $mutex.Dispose()
    }
}
