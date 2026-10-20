# Install a published CarryFS package, or with -Local the checkout holding this
# script. -Uninstall removes CarryFS. All system writes happen after confirmation.
[CmdletBinding()]
param([string]$Version, [switch]$Upgrade, [switch]$Local, [switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$CarryFSRepository = 'carry-fs/carryfs'
$CarryFSApi = "https://api.github.com/repos/$CarryFSRepository"

# Parse X.Y.Z with an optional SemVer prerelease such as -rc.1. [version] cannot
# hold a prerelease and Windows PowerShell 5.1 has no [semver].
function ConvertTo-CarryFSVersion([string]$Value) {
    $identifier = '(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
    $pattern = "^v?((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))(?:-($identifier(?:\.$identifier)*))?$"
    if (-not ($Value -cmatch $pattern)) {
        throw "Invalid version '$Value'; use X.Y.Z or vX.Y.Z, optionally with a prerelease such as -rc.1."
    }
    $pre = $Matches[2]
    $text = $Matches[1]
    if ($pre) { $text += "-$pre" }
    return [pscustomobject]@{ Text = $text; Core = [version]$Matches[1]; Pre = @(if ($pre) { $pre -split '\.' }) }
}

# SemVer precedence as -1, 0 or 1. A prerelease sorts below its release, and
# rc.2 above rc.1.
function Compare-CarryFSVersion($A, $B) {
    $order = $A.Core.CompareTo($B.Core)
    if ($order -ne 0) { return [Math]::Sign($order) }
    if (-not $A.Pre.Count -and -not $B.Pre.Count) { return 0 }
    if (-not $A.Pre.Count) { return 1 }
    if (-not $B.Pre.Count) { return -1 }
    $shared = [Math]::Min($A.Pre.Count, $B.Pre.Count)
    for ($i = 0; $i -lt $shared; $i++) {
        $x = $A.Pre[$i]
        $y = $B.Pre[$i]
        $xNumeric = $x -match '^[0-9]+$'
        $yNumeric = $y -match '^[0-9]+$'
        if ($xNumeric -and $yNumeric) {
            # No leading zeros, so the longer number is the larger one.
            if ($x.Length -ne $y.Length) { return [Math]::Sign($x.Length - $y.Length) }
        } elseif ($xNumeric) { return -1 } elseif ($yNumeric) { return 1 }
        $order = [string]::CompareOrdinal($x, $y)
        if ($order -ne 0) { return [Math]::Sign($order) }
    }
    return [Math]::Sign($A.Pre.Count - $B.Pre.Count)
}

function Get-CarryFSReleases {
    $page = 1
    do {
        $uri = "$CarryFSApi/releases?per_page=100&page=$page"
        try {
            $batch = Invoke-RestMethod -Uri $uri -Headers @{
                Accept = 'application/vnd.github+json'; 'User-Agent' = 'carryfs-installer'
            } -UseBasicParsing -TimeoutSec 300
            $batch = @($batch)
        } catch {
            throw "Cannot list CarryFS releases. Check your connection and GitHub API rate limit. $($_.Exception.Message)"
        }
        foreach ($release in $batch) { $release }
        $page++
    } while ($batch.Count -eq 100)
}

function Select-CarryFSRelease($Releases, $Installed, [switch]$Upgrade) {
    $best = $null
    $bestVersion = $null
    foreach ($release in $Releases) {
        if ($release.draft -ne $false -or $release.prerelease -ne $false -or
            $release.tag_name -cnotmatch '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') { continue }
        $candidate = ConvertTo-CarryFSVersion $release.tag_name
        if ($Installed -and -not $Upgrade -and $candidate.Core.Major -ne $Installed.Core.Major) { continue }
        if (-not $best -or (Compare-CarryFSVersion $candidate $bestVersion) -gt 0) {
            $best = $release
            $bestVersion = $candidate
        }
    }
    if (-not $best) { throw 'No published stable release matches this installation. Use -Version for an explicit published tag.' }
    return $best
}

function Save-CarryFSDownload([string]$Uri, [string]$Path) {
    try {
        Invoke-WebRequest -Uri $Uri -OutFile $Path -UseBasicParsing -TimeoutSec 300 | Out-Null
    } catch {
        throw "Download failed: $Uri. $($_.Exception.Message)"
    }
}

function Test-CarryFSPrerequisites([switch]$Local) {
    if ($Local -and -not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'Missing cargo: https://rustup.rs/' }
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Run install.ps1 in Windows PowerShell; Linux and macOS use install.sh.' }
    if ($PSVersionTable.PSVersion -lt [version]'5.1') { throw 'PowerShell 5.1 or newer is required: https://learn.microsoft.com/powershell/scripting/install/installing-powershell-on-windows' }
    if ([Environment]::OSVersion.Version.Major -lt 10) { throw 'CarryFS requires Windows 10 or later.' }
    $architecture = $env:PROCESSOR_ARCHITEW6432
    if (-not $architecture) { $architecture = $env:PROCESSOR_ARCHITECTURE }
    if ($architecture -ne 'AMD64' -or -not [Environment]::Is64BitProcess) { throw 'CarryFS packages require x86-64 Windows and a 64-bit PowerShell process.' }
    $key = 'HKLM:\SOFTWARE\WOW6432Node\WinFsp'
    if (-not (Test-Path $key)) { throw 'Install WinFsp 2.1 or later: https://winfsp.dev/rel/' }
    $directory = (Get-ItemProperty $key).InstallDir
    $dll = Join-Path $directory 'bin\winfsp-x64.dll'
    if (-not (Test-Path $dll -PathType Leaf)) { throw 'WinFsp is incomplete. Install WinFsp 2.1 or later: https://winfsp.dev/rel/' }
    $fileVersion = (Get-Item $dll).VersionInfo
    if ($fileVersion.FileMajorPart -lt 2 -or ($fileVersion.FileMajorPart -eq 2 -and $fileVersion.FileMinorPart -lt 1)) {
        throw 'Update WinFsp to 2.1 or later: https://winfsp.dev/rel/'
    }
    if (-not (Test-Path (Join-Path $directory 'License.txt') -PathType Leaf)) { throw 'The WinFsp license notice is missing; reinstall WinFsp: https://winfsp.dev/rel/' }
}

function Expand-CarryFSPackage([string]$Archive, [string]$Destination) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        foreach ($entry in $zip.Entries) {
            if ($entry.FullName -match '^/|(^|/)\.\.(/|$)|[^a-zA-Z0-9_./-]') { throw "Unsafe archive path: $($entry.FullName)" }
            $kind = ($entry.ExternalAttributes -shr 16) -band 0xF000
            if ($kind -notin @(0, 0x8000, 0x4000)) { throw "Archive entry is not a regular file or directory: $($entry.FullName)" }
        }
    } finally { $zip.Dispose() }
    [IO.Compression.ZipFile]::ExtractToDirectory($Archive, $Destination)
}

function Get-CarryFSCheckoutVersion([string]$Checkout) {
    $manifest = Join-Path $Checkout 'Cargo.toml'
    if (-not (Test-Path $manifest -PathType Leaf) -or
        -not (Test-Path (Join-Path $Checkout 'windows\packaging\install.ps1') -PathType Leaf)) {
        throw '-Local needs a CarryFS checkout: run .\install.ps1 -Local in the cloned repository.'
    }
    $line = Get-Content $manifest | Where-Object { $_ -cmatch '^version = ".+"$' } | Select-Object -First 1
    if (-not $line) { throw 'Cannot read the version from Cargo.toml.' }
    return ConvertTo-CarryFSVersion ($line -replace '^version = "|"$', '')
}

# Build the checkout and stage it in the layout of a release package.
function Build-CarryFSCheckout([string]$Checkout, $Selected, [string]$Payload) {
    New-Item -ItemType Directory $Payload | Out-Null
    Push-Location $Checkout
    try {
        & cargo build --release --locked -p carryfs-core -p carryfs-windows
        # A failed build must not stage binaries left by an older one.
        if ($LASTEXITCODE -ne 0) { throw 'Local build failed; nothing was installed.' }
        $target = 'target'
        if ($env:CARGO_TARGET_DIR) { $target = $env:CARGO_TARGET_DIR }
        foreach ($file in @('carryfs.exe', 'carryfs-windows.exe')) {
            Copy-Item -LiteralPath (Join-Path $target "release\$file") -Destination $Payload
        }
        Copy-Item -LiteralPath 'windows\packaging\install.ps1' -Destination (Join-Path $Payload 'install-system.ps1')
    } finally { Pop-Location }
    Set-Content -LiteralPath (Join-Path $Payload 'VERSION') -Value $Selected.Text -Encoding Ascii
}

# Run PowerShell with these arguments as Administrator and return its exit code.
function Invoke-CarryFSElevated([string]$Arguments) {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    # Use a child process so streamed invocation also works with Restricted policy.
    $shell = (Get-Process -Id $PID).Path
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        $process = Start-Process -FilePath $shell -ArgumentList $Arguments -NoNewWindow -Wait -PassThru
    } else {
        # The same arguments cross the UAC boundary.
        $process = Start-Process -FilePath $shell -Verb RunAs -ArgumentList $Arguments -Wait -PassThru
    }
    return $process.ExitCode
}

function Invoke-CarryFSUninstall {
    $target = Join-Path $env:ProgramFiles 'CarryFS'
    $service = Get-CimInstance Win32_Service -Filter "Name='CarryFS'"
    if (-not $service -and -not (Test-Path $target)) { Write-Host 'CarryFS is not installed.'; return }
    $servicePid = 0
    if ($service) { $servicePid = $service.ProcessId }
    $foreground = @(Get-Process -Name carryfs-windows -ErrorAction SilentlyContinue | Where-Object { $_.Id -ne $servicePid })
    if ($foreground.Count) { throw 'A foreground CarryFS adapter is running. Stop it and retry.' }
    Write-Host "Remove CarryFS from $target with its automount service. Mounted CarryFS volumes will be ejected."
    Write-Host 'WinFsp and disk contents will be left alone. Removal requires administrator permission.'
    if ((Read-Host 'Uninstall? [y/N]') -notmatch '^(y|yes)$') { Write-Host 'Uninstall cancelled.'; return }
    # Remove what windows\packaging\install.ps1 installs; that script is not kept on the system.
    $removal = {
        $ErrorActionPreference = 'Stop'
        $service = Get-Service CarryFS -ErrorAction SilentlyContinue
        if ($service) {
            # Stopping the service publishes and ejects every mounted volume.
            Stop-Service CarryFS
            $service.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(60))
            & sc.exe delete CarryFS
            if ($LASTEXITCODE -ne 0) { throw "Service command failed with exit code $LASTEXITCODE." }
        }
        $target = Join-Path $env:ProgramFiles 'CarryFS'
        if (Test-Path $target) { Remove-Item -LiteralPath $target -Recurse -Force }
    }
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($removal.ToString()))
    $code = Invoke-CarryFSElevated "-NoProfile -EncodedCommand $encoded"
    if ($code -ne 0) { throw "Uninstall failed with exit code $code." }
    Write-Host 'Uninstalled CarryFS. WinFsp and disk contents were left alone.'
}

function Invoke-CarryFSInstall([string]$Version, [switch]$Upgrade, [switch]$Local, [switch]$Uninstall) {
    if ($Version -and $Upgrade) { throw '-Version and -Upgrade cannot be combined.' }
    if ($Local -and ($Version -or $Upgrade -or $Uninstall)) { throw '-Local cannot be combined with -Version, -Upgrade, or -Uninstall.' }
    if ($Uninstall) {
        if ($Version -or $Upgrade) { throw '-Uninstall cannot be combined with -Version or -Upgrade.' }
        Invoke-CarryFSUninstall
        return
    }
    $requested = $null
    if ($Version) { $requested = ConvertTo-CarryFSVersion $Version }
    Test-CarryFSPrerequisites -Local:$Local
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $target = Join-Path $env:ProgramFiles 'CarryFS'
    $marker = Join-Path $target 'VERSION'
    $installed = $null
    $expected = 'unrecorded'
    if (Test-Path $marker -PathType Leaf) {
        $expected = (Get-Content $marker -Raw).Trim()
        $installed = ConvertTo-CarryFSVersion $expected
    } elseif (Test-Path (Join-Path $target 'carryfs-windows.exe')) {
        if (-not $requested -and -not $Upgrade -and -not $Local) { throw 'This installation has no version record. Select -Version or -Upgrade explicitly.' }
    }
    if ($Local) {
        # $PSScriptRoot is empty when the script is streamed instead of run from a file.
        if (-not $PSScriptRoot) { throw '-Local needs a CarryFS checkout: run .\install.ps1 -Local in the cloned repository.' }
        $selected = Get-CarryFSCheckoutVersion $PSScriptRoot
    } else {
        if ($requested) {
            $uri = "$CarryFSApi/releases/tags/v$($requested.Text)"
            $release = Invoke-RestMethod -Uri $uri -Headers @{
                Accept = 'application/vnd.github+json'; 'User-Agent' = 'carryfs-installer'
            } -UseBasicParsing -TimeoutSec 300
            if ($release.tag_name -cne "v$($requested.Text)" -or $release.draft -ne $false) { throw 'Invalid release metadata from GitHub.' }
        } else {
            $release = Select-CarryFSRelease @(Get-CarryFSReleases) $installed -Upgrade:$Upgrade
        }
        $selected = ConvertTo-CarryFSVersion $release.tag_name
        if ($installed -and ((Compare-CarryFSVersion $selected $installed) -eq 0 -or (-not $requested -and (Compare-CarryFSVersion $selected $installed) -lt 0))) {
            Write-Host "CarryFS $($installed.Text) is already current for this selection."
            return
        }
    }
    $work = Join-Path ([IO.Path]::GetTempPath()) ('carryfs-install-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $work | Out-Null
    try {
        $payload = Join-Path $work 'payload'
        if ($Local) {
            Build-CarryFSCheckout $PSScriptRoot $selected $payload
            $origin = "Built from $PSScriptRoot."
        } else {
            $asset = "carryfs-$($selected.Text)-windows-x86_64.zip"
            $base = "https://github.com/$CarryFSRepository/releases/download/$($release.tag_name)"
            $archive = Join-Path $work $asset
            $checksums = Join-Path $work 'SHA256SUMS'
            Save-CarryFSDownload "$base/$asset" $archive
            Save-CarryFSDownload "$base/SHA256SUMS" $checksums
            $pattern = '^([0-9a-fA-F]{64}) [ *]' + [regex]::Escape($asset) + '$'
            $lines = @(Get-Content $checksums | Where-Object { $_ -cmatch $pattern })
            if ($lines.Count -ne 1) { throw "Missing or duplicate SHA-256 checksum for $asset." }
            $checksum = [regex]::Match($lines[0], $pattern).Groups[1].Value
            if ((Get-FileHash $archive -Algorithm SHA256).Hash -ne $checksum) { throw "Checksum mismatch for $asset; nothing was installed." }
            Expand-CarryFSPackage $archive $payload
            foreach ($file in @('carryfs.exe', 'carryfs-windows.exe', 'install-system.ps1', 'VERSION')) {
                if (-not (Test-Path (Join-Path $payload $file) -PathType Leaf)) { throw "The release package is missing $file." }
            }
            if ((Get-Content (Join-Path $payload 'VERSION') -Raw).Trim() -cne $selected.Text) { throw 'The package version does not match its release tag.' }
            $origin = 'The package is verified.'
        }
        Write-Host "Install CarryFS $($selected.Text) to $target."
        if ($installed) { Write-Host "Current version: $($installed.Text)." }
        if ($installed -and (Compare-CarryFSVersion $selected $installed) -lt 0) { Write-Host 'This will downgrade CarryFS.' }
        Write-Host "Eject all CarryFS volumes before continuing. $origin System installation requires administrator permission."
        if ((Read-Host 'Install? [y/N]') -notmatch '^(y|yes)$') { Write-Host 'Installation cancelled.'; return }
        $installer = Join-Path $payload 'install-system.ps1'
        $code = Invoke-CarryFSElevated ('-NoProfile -ExecutionPolicy Bypass -File "{0}" -ExpectedVersion "{1}"' -f $installer, $expected)
        if ($code -ne 0) { throw "System installation failed with exit code $code." }
        Write-Host "Installed CarryFS $($selected.Text)."
    } finally { Remove-Item -LiteralPath $work -Recurse -Force }
}

Invoke-CarryFSInstall @PSBoundParameters
