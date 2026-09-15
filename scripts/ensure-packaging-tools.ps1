# Makes sure makensis and wix are available, installing them only if missing.
#
# Written to be idempotent and to work on both runners this repo builds on,
# which differ in ways that broke a naive `choco install`:
#
#   - GitHub's windows-latest is a fresh VM with chocolatey preinstalled and
#     neither NSIS nor WiX present.
#   - The self-hosted Windows runner is a developer's machine. It already has
#     NSIS and WiX, and has winget rather than chocolatey - so `choco install`
#     fails outright there even though nothing needed installing.
#
# Hence: check first, and fall back through whatever package manager exists.

$ErrorActionPreference = 'Stop'

function Find-MakeNsis {
    @(
        "${env:ProgramFiles(x86)}\NSIS\makensis.exe",
        "$env:ProgramFiles\NSIS\makensis.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
}

function Find-Wix {
    @(
        "$env:USERPROFILE\.dotnet\tools\wix.exe",
        (Get-Command wix -ErrorAction SilentlyContinue).Source
    ) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
}

# --- NSIS ---------------------------------------------------------------------
$nsis = Find-MakeNsis
if ($nsis) {
    Write-Output "NSIS already present: $nsis"
} elseif (Get-Command choco -ErrorAction SilentlyContinue) {
    Write-Output "installing NSIS via chocolatey"
    choco install nsis --no-progress -y
} elseif (Get-Command winget -ErrorAction SilentlyContinue) {
    Write-Output "installing NSIS via winget"
    winget install --id NSIS.NSIS --accept-source-agreements --accept-package-agreements --silent
} else {
    throw "no NSIS and no package manager to install it with"
}

# --- WiX ----------------------------------------------------------------------
$wix = Find-Wix
if ($wix) {
    Write-Output "WiX already present: $wix"
} else {
    Write-Output "installing WiX as a dotnet global tool"
    # Pinned: the .wxs uses the v4/v5 schema and the Util extension version has
    # to match the toolset, so letting this float would break the build the
    # next time WiX ships a major.
    dotnet tool install --global wix --version 5.0.2
}

$wix = Find-Wix
if (-not $wix) { throw "wix still not found after install" }

# The Util extension supplies CloseApplication, which stops a running tray app
# so the installer can replace a locked exe. Adding it twice is harmless.
& $wix extension add -g WixToolset.Util.wixext/5.0.2 2>&1 | Out-Null

Write-Output "makensis: $(Find-MakeNsis)"
Write-Output "wix     : $wix"
