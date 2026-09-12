#Requires -Version 7
<#
Enforce the ~1:1 test-to-production line ratio on src/core/ ONLY.

EXEMPT ON PURPOSE — do not "fix" this later by adding these directories:

  src/platform/  WinRT / COM / Win32 interop shims. These are deliberately
                 too dumb to be wrong in an interesting way. Mocking the FFI
                 seam would only ever test the mock. They are verified against
                 real hardware via the soak matrix in docs/soak.md.
  src/ui/        egui view code. Asserting on widget trees is fake coverage.
  src/bin/       Throwaway spikes.

Padding tests onto unmockable FFI is how coverage targets manufacture false
confidence. The ratio is meaningful for core/ precisely because core/ is pure.

WHY NOT tokei (which CLAUDE.md suggests): tokei counts per FILE, but our test
code lives in `#[cfg(test)]` blocks INSIDE production files. Splitting those
apart needs brace matching, which tokei cannot do. This script does the brace
matching and otherwise reports the same "code lines" notion tokei would
(blank lines and line comments excluded).
#>

$ErrorActionPreference = 'Stop'
$MinRatio = 0.9
$CoreDir = Join-Path $PSScriptRoot '..' 'src' 'core'
$TestsDir = Join-Path $PSScriptRoot '..' 'tests'

function Measure-RustFile {
    param([string]$Path)

    $prod = 0
    $test = 0
    $depth = 0          # brace depth inside a #[cfg(test)] block
    $armed = $false     # saw #[cfg(test)], waiting for the opening brace
    $inBlockComment = $false

    foreach ($raw in [System.IO.File]::ReadAllLines($Path)) {
        $line = $raw.Trim()

        # Crude block-comment tracking: good enough for counting, and a
        # miscount here cannot mask a real ratio failure by more than a
        # handful of lines.
        if ($inBlockComment) {
            if ($line -match '\*/') { $inBlockComment = $false }
            continue
        }
        if ($line -match '^/\*' -and $line -notmatch '\*/') {
            $inBlockComment = $true
            continue
        }

        $isCode = $line -ne '' -and -not $line.StartsWith('//')

        if ($line -match '#\[cfg\(test\)\]') {
            $armed = $true
            if ($isCode) { $test++ }
            continue
        }

        $opens = ([regex]::Matches($line, '\{')).Count
        $closes = ([regex]::Matches($line, '\}')).Count

        if ($armed -and $opens -gt 0) {
            $armed = $false
            $depth = $opens - $closes
            if ($isCode) { $test++ }
            continue
        }

        if ($depth -gt 0) {
            if ($isCode) { $test++ }
            $depth += $opens - $closes
            continue
        }

        if ($isCode) { $prod++ }
    }

    [pscustomobject]@{ Production = $prod; Test = $test }
}

if (-not (Test-Path $CoreDir)) {
    Write-Output "RATIO GATE: src/core/ does not exist yet -- nothing to enforce."
    Write-Output "This gate becomes load-bearing at milestone 5 (core traits +"
    Write-Output "health state machine). It is wired into CI now so that it"
    Write-Output "cannot be forgotten then."
    exit 0
}

$prod = 0
$test = 0

foreach ($f in Get-ChildItem -Path $CoreDir -Filter *.rs -Recurse -File) {
    $m = Measure-RustFile -Path $f.FullName
    $prod += $m.Production
    $test += $m.Test
    Write-Output ("  {0,-40} prod {1,5}  test {2,5}" -f $f.Name, $m.Production, $m.Test)
}

if (Test-Path $TestsDir) {
    foreach ($f in Get-ChildItem -Path $TestsDir -Filter *.rs -Recurse -File) {
        $m = Measure-RustFile -Path $f.FullName
        # Everything in tests/ is test code by definition.
        $n = $m.Production + $m.Test
        $test += $n
        Write-Output ("  {0,-40} prod {1,5}  test {2,5}" -f "tests/$($f.Name)", 0, $n)
    }
}

Write-Output ''
Write-Output "core/ production lines : $prod"
Write-Output "test lines             : $test"

if ($prod -eq 0) {
    Write-Output "RATIO GATE: core/ exists but has no production code yet -- skipping."
    exit 0
}

$ratio = [math]::Round($test / $prod, 3)
Write-Output "ratio                  : $ratio (floor $MinRatio)"

if ($ratio -lt $MinRatio) {
    Write-Output ''
    Write-Output "FAIL: core/ is under-tested. The health state machine is the whole"
    Write-Output "point of this app; it is the one thing that must be exhaustively"
    Write-Output "covered. Add tests rather than lowering this floor."
    exit 1
}

Write-Output 'PASS'
exit 0
