# Download a published CarryFS package. The packaged installer runs after confirmation.
[CmdletBinding()]
param([string]$Version, [switch]$Upgrade)

$ErrorActionPreference = 'Stop'
$CarryFSRepository = 'carry-fs/carryfs'
$CarryFSApi = "https://api.github.com/repos/$CarryFSRepository"

function ConvertTo-CarryFSVersion([string]$Value) {
    if ($Value -cnotmatch '^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') {
        throw "Invalid version '$Value'; use X.Y.Z or vX.Y.Z."
    }
    return [version]($Value -replace '^v', '')
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

function Select-CarryFSRelease($Releases, [version]$Installed, [switch]$Upgrade) {
    $best = $null
    $bestVersion = $null
    foreach ($release in $Releases) {
        if ($release.draft -ne $false -or $release.prerelease -ne $false -or
            $release.tag_name -cnotmatch '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') { continue }
        $candidate = ConvertTo-CarryFSVersion $release.tag_name
        if ($Installed -and -not $Upgrade -and $candidate.Major -ne $Installed.Major) { continue }
        if (-not $best -or $candidate -gt $bestVersion) {
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

function Test-CarryFSPrerequisites {
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

function Invoke-CarryFSInstall([string]$Version, [switch]$Upgrade) {
    if ($Version -and $Upgrade) { throw '-Version and -Upgrade cannot be combined.' }
    $requested = $null
    if ($Version) { $requested = ConvertTo-CarryFSVersion $Version }
    Test-CarryFSPrerequisites
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $target = Join-Path $env:ProgramFiles 'CarryFS'
    $marker = Join-Path $target 'VERSION'
    $installed = $null
    $expected = 'unrecorded'
    if (Test-Path $marker -PathType Leaf) {
        $expected = (Get-Content $marker -Raw).Trim()
        $installed = ConvertTo-CarryFSVersion $expected
    } elseif (Test-Path (Join-Path $target 'carryfs-windows.exe')) {
        if (-not $requested -and -not $Upgrade) { throw 'This installation has no version record. Select -Version or -Upgrade explicitly.' }
    }
    if ($requested) {
        $uri = "$CarryFSApi/releases/tags/v$($requested.ToString(3))"
        $release = Invoke-RestMethod -Uri $uri -Headers @{
            Accept = 'application/vnd.github+json'; 'User-Agent' = 'carryfs-installer'
        } -UseBasicParsing -TimeoutSec 300
        if ($release.tag_name -cne "v$($requested.ToString(3))" -or $release.draft -ne $false) { throw 'Invalid release metadata from GitHub.' }
    } else {
        $release = Select-CarryFSRelease @(Get-CarryFSReleases) $installed -Upgrade:$Upgrade
    }
    $selected = ConvertTo-CarryFSVersion $release.tag_name
    if ($installed -and ($selected -eq $installed -or (-not $requested -and $selected -lt $installed))) {
        Write-Host "CarryFS $($installed.ToString(3)) is already current for this selection."
        return
    }
    $work = Join-Path ([IO.Path]::GetTempPath()) ('carryfs-install-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $work | Out-Null
    try {
        $asset = "carryfs-$($selected.ToString(3))-windows-x86_64.zip"
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
        $payload = Join-Path $work 'payload'
        Expand-CarryFSPackage $archive $payload
        foreach ($file in @('carryfs.exe', 'carryfs-windows.exe', 'install-system.ps1', 'VERSION')) {
            if (-not (Test-Path (Join-Path $payload $file) -PathType Leaf)) { throw "The release package is missing $file." }
        }
        if ((Get-Content (Join-Path $payload 'VERSION') -Raw).Trim() -cne $selected.ToString(3)) { throw 'The package version does not match its release tag.' }
        Write-Host "Install CarryFS $($selected.ToString(3)) to $target."
        if ($installed) { Write-Host "Current version: $($installed.ToString(3))." }
        if ($installed -and $selected -lt $installed) { Write-Host 'This will downgrade CarryFS.' }
        Write-Host 'Eject all CarryFS volumes before continuing. The package is verified; system installation requires administrator permission.'
        if ((Read-Host 'Install? [y/N]') -notmatch '^(y|yes)$') { Write-Host 'Installation cancelled.'; return }
        $installer = Join-Path $payload 'install-system.ps1'
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
        $principal = New-Object Security.Principal.WindowsPrincipal($identity)
        # Use a child process so streamed invocation also works with Restricted policy.
        $arguments = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -ExpectedVersion "{1}"' -f $installer, $expected
        $shell = (Get-Process -Id $PID).Path
        if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
            $process = Start-Process -FilePath $shell -ArgumentList $arguments -NoNewWindow -Wait -PassThru
        } else {
            # The same package and expected version cross the UAC boundary.
            $process = Start-Process -FilePath $shell -Verb RunAs -ArgumentList $arguments -Wait -PassThru
        }
        if ($process.ExitCode -ne 0) { throw "System installation failed with exit code $($process.ExitCode)." }
        Write-Host "Installed CarryFS $($selected.ToString(3))."
    } finally { Remove-Item -LiteralPath $work -Recurse -Force }
}

Invoke-CarryFSInstall @PSBoundParameters
