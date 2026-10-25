$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$source = Get-Content (Join-Path $root 'install.ps1') -Raw
$source = $source -replace '(?m)^Invoke-CarryFSInstall @PSBoundParameters\r?$', ''
. ([scriptblock]::Create($source))

function Assert($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Must-Fail([scriptblock]$Action, [string]$Message) {
    $failed = $false
    try { & $Action | Out-Null } catch {
        if ($_.Exception.Message -notlike "*$Message*") { throw }
        $failed = $true
    }
    Assert $failed "Expected failure: $Message"
}
function Invoke-RestMethod {
    param($Uri, [switch]$UseBasicParsing, $TimeoutSec)
    Assert ($Uri -eq 'https://carryfs.com/releases.txt') "Unexpected metadata request: $Uri"
    $script:index
}

$script:index = "0.1.9`r`n0.1.10`r`n`r`n1.0.0`r`n"
$versions = @(Get-CarryFSVersions)
Assert ((Select-CarryFSVersion $versions $null).Text -eq '1.0.0') 'Fresh install selected the wrong version'
$installed = ConvertTo-CarryFSVersion '0.1.0'
Assert ((Select-CarryFSVersion $versions $installed).Text -eq '0.1.10') 'Default selection crossed a major version'
Assert ((Select-CarryFSVersion $versions $installed -Upgrade).Text -eq '1.0.0') 'Upgrade did not select the newest major'
Must-Fail { Select-CarryFSVersion @('1.0.0') $installed } 'No published stable release'
$script:index = ''
Must-Fail { Select-CarryFSVersion @(Get-CarryFSVersions) $null } 'No published stable release'
foreach ($invalid in @('<html>unavailable</html>', '0.1.1-rc.1', 'v0.1.1', '01.1.1', ' 0.1.1')) {
    $script:index = "0.1.10`n$invalid`n"
    Must-Fail { Get-CarryFSVersions } 'Invalid release index'
}
'PowerShell release-index selection checks passed.'
