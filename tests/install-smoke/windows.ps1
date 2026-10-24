$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_OS -ne 'Windows') {
    throw 'Run only on a disposable GitHub-hosted Windows runner.'
}

New-Item -ItemType Directory -Force smoke-logs | Out-Null
Start-Transcript -Path smoke-logs/windows.log
try {
    $msi = Join-Path $env:RUNNER_TEMP 'winfsp-2.1.25156.msi'
    Invoke-WebRequest 'https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-2.1.25156.msi' -OutFile $msi -UseBasicParsing
    # SHA-256 published at https://github.com/winfsp/winfsp/releases/tag/v2.1.
    if ((Get-FileHash $msi -Algorithm SHA256).Hash -ne '073A70E00F77423E34BED98B86E600DEF93393BA5822204FAC57A29324DB9F7A') {
        throw 'WinFsp MSI checksum mismatch.'
    }
    $msiLog = Join-Path $PWD 'smoke-logs/winfsp-msi.log'
    $process = Start-Process msiexec.exe -ArgumentList "/i `"$msi`" /qn /norestart /L*v `"$msiLog`"" -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "WinFsp installation failed with exit code $($process.ExitCode)." }

    $command = "& ([scriptblock]::Create((irm https://carryfs.com/install.ps1))) -Version $env:CARRYFS_VERSION"
    Write-Host "Testing: $command"
    # A child console host reads the answer from stdin, including on PowerShell 5.1.
    'y' | powershell.exe -NoProfile -ExecutionPolicy Bypass -Command $command | Tee-Object smoke-logs/install.log
    if ($LASTEXITCODE -ne 0) { throw "CarryFS installation failed with exit code $LASTEXITCODE." }
    if (-not (Select-String -Path smoke-logs/install.log -SimpleMatch "Installed CarryFS $env:CARRYFS_VERSION.")) {
        throw 'The public installer did not report a successful installation.'
    }

    $target = Join-Path $env:ProgramFiles 'CarryFS'
    foreach ($file in @('carryfs.exe', 'carryfs-windows.exe', 'WinFsp-License.txt', 'VERSION')) {
        if (-not (Test-Path (Join-Path $target $file) -PathType Leaf)) { throw "Missing installed file: $file" }
    }
    if ((Get-Content (Join-Path $target 'VERSION') -Raw).Trim() -cne $env:CARRYFS_VERSION) {
        throw 'Installed version does not match the requested release.'
    }
    $service = Get-CimInstance Win32_Service -Filter "Name='CarryFS'"
    if ($service.State -ne 'Running' -or $service.StartMode -ne 'Auto' -or $service.ProcessId -eq 0) {
        throw 'CarryFS automount service is not running with automatic startup.'
    }
    $expectedBinary = '"{0}" service' -f (Join-Path $target 'carryfs-windows.exe')
    if ($service.PathName -cne $expectedBinary) { throw "Unexpected adapter service command: $($service.PathName)" }
    $adapter = Get-Process -Id $service.ProcessId
    if ($adapter.Path -ne (Join-Path $target 'carryfs-windows.exe')) { throw 'The installed adapter process is not running.' }
    $service | Format-List Name, State, StartMode, PathName, ProcessId

    $image = Join-Path $env:RUNNER_TEMP 'carryfs-install-smoke.img'
    & "$target\carryfs.exe" mkfs $image 268435456
    if ($LASTEXITCODE -ne 0) { throw 'Installed CLI could not create a disposable image.' }
    & "$target\carryfs.exe" check $image
    if ($LASTEXITCODE -ne 0) { throw 'Installed CLI could not check the disposable image.' }
    Remove-Item -LiteralPath $image

    'y' | powershell.exe -NoProfile -ExecutionPolicy Bypass -Command $command | Tee-Object smoke-logs/reinstall.log
    if ($LASTEXITCODE -ne 0) { throw "Reinstall failed with exit code $LASTEXITCODE." }
    if (-not (Select-String -Path smoke-logs/reinstall.log -SimpleMatch "CarryFS $env:CARRYFS_VERSION is already current")) {
        throw 'Reinstall did not detect the installed release.'
    }
    if ((Get-Service CarryFS).Status -ne 'Running') { throw 'Reinstall stopped the adapter service.' }

    'y' | powershell.exe -NoProfile -ExecutionPolicy Bypass -Command '& ([scriptblock]::Create((irm https://carryfs.com/install.ps1))) -Uninstall'
    if ($LASTEXITCODE -ne 0) { throw "Uninstall failed with exit code $LASTEXITCODE." }
    if (Test-Path $target) { throw 'CarryFS installation directory remains after uninstall.' }
    if (Get-CimInstance Win32_Service -Filter "Name='CarryFS'") { throw 'CarryFS service remains after uninstall.' }
    if (-not (Test-Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp')) { throw 'Uninstall removed WinFsp.' }
    "Windows PowerShell $($PSVersionTable.PSVersion), CarryFS ${env:CARRYFS_VERSION}: public install, adapter service, reinstall, and uninstall passed." >> $env:GITHUB_STEP_SUMMARY
} finally {
    Stop-Transcript
}
