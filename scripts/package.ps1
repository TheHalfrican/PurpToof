# Builds the release binary and both installers.
#
#     pwsh -File scripts/package.ps1
#
# Everything runs from the repository root, because WiX resolves source paths
# against the working directory rather than against the .wxs file.
#
# Two installers on purpose. NSIS is the friendlier download for a person:
# small, a normal wizard, no Windows Installer machinery. The MSI is what
# Intune, Group Policy and `msiexec /qn` want for unattended deployment, and it
# is the format anyone managing a fleet will ask for.
#
# Both install per-user into %LOCALAPPDATA%\Programs\PurpToof and need no
# elevation: the app advertises THIS PC as a speaker, routes to the logged-in
# user's default output, and keeps its settings in %APPDATA% - all per-user, so
# a machine-wide install would add a UAC prompt and buy nothing.

param(
    # Skip the cargo build when you already have a release binary.
    [switch]$SkipBuild,
    # Skip either installer if its tooling is missing and you only want the other.
    [switch]$SkipNsis,
    [switch]$SkipMsi
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Keep the version in one place. Cargo.toml is the source of truth; the
# installers read it from here rather than each carrying their own copy, which
# is how a release ends up shipping three different version numbers.
$version = (Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Write-Output "PurpToof $version"

New-Item -ItemType Directory -Force -Path dist | Out-Null

if (-not $SkipBuild) {
    Write-Output "`n=== icon ==="
    pwsh -NoProfile -File scripts/make-icon.ps1 | Select-Object -Last 2

    Write-Output "`n=== cargo build --release ==="
    # A running instance holds the exe open and the build fails on a file lock.
    Get-Process purptoof -ErrorAction SilentlyContinue | Stop-Process -Force
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

$exe = 'target\release\purptoof.exe'
if (-not (Test-Path $exe)) { throw "missing $exe - run without -SkipBuild" }
$size = [math]::Round((Get-Item $exe).Length / 1MB, 2)
Write-Output "binary: $exe ($size MB)"

if (-not $SkipNsis) {
    Write-Output "`n=== NSIS ==="
    $makensis = @(
        "${env:ProgramFiles(x86)}\NSIS\makensis.exe",
        "$env:ProgramFiles\NSIS\makensis.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $makensis) {
        Write-Warning "makensis not found - run scripts/ensure-packaging-tools.ps1"
    } else {
        & $makensis "/DVERSION=$version" packaging\purptoof.nsi | Select-Object -Last 3
        if ($LASTEXITCODE -ne 0) { throw "makensis failed" }
    }
}

if (-not $SkipMsi) {
    Write-Output "`n=== MSI ==="
    $wix = @(
        "$env:USERPROFILE\.dotnet\tools\wix.exe",
        (Get-Command wix -ErrorAction SilentlyContinue).Source
    ) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
    if (-not $wix) {
        Write-Warning "wix not found - run scripts/ensure-packaging-tools.ps1"
    } else {
        # Util supplies CloseApplication, which stops a running tray app so the
        # installer can replace a locked exe. UI supplies the wizard - without
        # it the MSI has no dialogs at all, and a double-click looks exactly
        # like the installer crashing.
        & $wix extension add -g WixToolset.Util.wixext/5.0.2 2>&1 | Out-Null
        & $wix extension add -g WixToolset.UI.wixext/5.0.2 2>&1 | Out-Null

        pwsh -NoProfile -File scripts/make-license-rtf.ps1 `
            -Source (Join-Path $root 'LICENSE') `
            -Destination (Join-Path $root 'dist\license.rtf') | Out-Null

        # -arch x64 matters. WiX defaults to x86, and a 32-bit package
        # registers itself under HKLM\WOW6432Node and installs into the 32-bit
        # view - wrong for a 64-bit binary.
        & $wix build packaging\purptoof.wxs `
            -ext WixToolset.Util.wixext -ext WixToolset.UI.wixext -arch x64 `
            -d Version=$version -o "dist\PurpToof-$version.msi"
        if ($LASTEXITCODE -ne 0) { throw "wix build failed" }
    }
}

Write-Output "`n=== dist ==="
Get-ChildItem dist -File | Where-Object { $_.Extension -in '.exe', '.msi' } |
    Select-Object Name, @{ n = 'MB'; e = { [math]::Round($_.Length / 1MB, 2) } } |
    Format-Table -AutoSize | Out-String -Width 80
