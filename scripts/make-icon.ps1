# Converts the source artwork into the raw RGBA blob the app embeds.
#
# The crate deliberately has no image decoder: one icon does not justify pulling
# a JPEG/PNG stack into a Bluetooth audio daemon. So the decode happens here,
# once, and `src/ui/icon.rs` includes the result with `include_bytes!`.
#
# Re-run this if the artwork changes:
#     pwsh -File scripts/make-icon.ps1
#
# What it does, in order:
#   1. Clamps near-black pixels to TRUE black. The source is a JPEG, so its
#      "black" background is actually 0-2 with compression noise, plus a purple
#      glow under the drip. On a dark title bar a not-quite-black square reads
#      as a visible tile around the icon.
#   2. Autocrops to the artwork's bounding box and re-squares it, so the fang
#      fills the frame instead of floating in the middle of a mostly-empty
#      canvas. At 16px that is the difference between a recognisable shape and
#      a smudge.
#   3. Downsamples to ICON_SIZE and writes tightly packed RGBA.

param(
    # 256 rather than 128: Windows downsamples this to 16px for the title bar
    # and 32-48 for the taskbar, and giving it more to work with keeps the
    # small sizes from going muddy.
    [int]$IconSize = 256,
    # Max channel value still considered background. High enough to swallow
    # JPEG noise and the ambient glow, low enough to leave the artwork's own
    # dark purples alone.
    [int]$BlackThreshold = 30
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$src = Join-Path $root 'assets\icon-source.jpg'
$outRgba = Join-Path $root 'assets\icon.rgba'
$outPreview = Join-Path $root 'assets\icon-preview.png'

if (-not (Test-Path $src)) { throw "missing source artwork: $src" }

$img = [System.Drawing.Image]::FromFile($src)
$bmp = New-Object System.Drawing.Bitmap $img
$img.Dispose()
$w = $bmp.Width; $h = $bmp.Height

# --- 1. true black, and find the artwork's extent -----------------------------
$minX = $w; $minY = $h; $maxX = -1; $maxY = -1
$clean = New-Object System.Drawing.Bitmap $w, $h, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)

for ($y = 0; $y -lt $h; $y++) {
    for ($x = 0; $x -lt $w; $x++) {
        $c = $bmp.GetPixel($x, $y)
        $peak = [Math]::Max($c.R, [Math]::Max($c.G, $c.B))
        if ($peak -le $BlackThreshold) {
            $clean.SetPixel($x, $y, [System.Drawing.Color]::FromArgb(255, 0, 0, 0))
        } else {
            $clean.SetPixel($x, $y, [System.Drawing.Color]::FromArgb(255, $c.R, $c.G, $c.B))
            if ($x -lt $minX) { $minX = $x }
            if ($y -lt $minY) { $minY = $y }
            if ($x -gt $maxX) { $maxX = $x }
            if ($y -gt $maxY) { $maxY = $y }
        }
    }
}
$bmp.Dispose()
if ($maxX -lt 0) { throw "the whole image thresholded to black - lower BlackThreshold" }
Write-Output "artwork bounds: ($minX,$minY)-($maxX,$maxY)"

# --- 2. square the crop around the artwork ------------------------------------
$cw = $maxX - $minX + 1
$ch = $maxY - $minY + 1
$side = [Math]::Max($cw, $ch)
# The artwork is tall and narrow - roughly 385x890 in a 1024 canvas - so in a
# square icon it can only ever fill about 43% of the width. Every pixel of
# padding is therefore expensive at 16px. Just 2% of air: enough that the shape
# does not touch the edge, not enough to shrink it further.
$side = [int]($side * 1.02)
$cx = $minX + [int]($cw / 2)
$cy = $minY + [int]($ch / 2)
$left = [Math]::Max(0, $cx - [int]($side / 2))
$top = [Math]::Max(0, $cy - [int]($side / 2))
if ($left + $side -gt $w) { $side = $w - $left }
if ($top + $side -gt $h) { $side = $h - $top }
Write-Output "square crop: ${side}x${side} at ($left,$top)"

$square = New-Object System.Drawing.Bitmap $side, $side, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$gs = [System.Drawing.Graphics]::FromImage($square)
$gs.Clear([System.Drawing.Color]::Black)
$gs.DrawImage($clean, (New-Object System.Drawing.Rectangle 0, 0, $side, $side), `
    (New-Object System.Drawing.Rectangle $left, $top, $side, $side), `
    [System.Drawing.GraphicsUnit]::Pixel)
$gs.Dispose(); $clean.Dispose()

# --- 3. downsample and emit ---------------------------------------------------
$icon = New-Object System.Drawing.Bitmap $IconSize, $IconSize, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$gi = [System.Drawing.Graphics]::FromImage($icon)
$gi.InterpolationMode = 'HighQualityBicubic'
$gi.PixelOffsetMode = 'HighQuality'
$gi.Clear([System.Drawing.Color]::Black)
$gi.DrawImage($square, 0, 0, $IconSize, $IconSize)
$gi.Dispose(); $square.Dispose()

$bytes = New-Object byte[] ($IconSize * $IconSize * 4)
$i = 0
for ($y = 0; $y -lt $IconSize; $y++) {
    for ($x = 0; $x -lt $IconSize; $x++) {
        $c = $icon.GetPixel($x, $y)
        # Resampling reintroduces near-black halos around the glow; clamp again
        # so the background is exactly #000000 in the shipped blob.
        $peak = [Math]::Max($c.R, [Math]::Max($c.G, $c.B))
        if ($peak -le 8) {
            $bytes[$i] = 0; $bytes[$i + 1] = 0; $bytes[$i + 2] = 0
        } else {
            $bytes[$i] = $c.R; $bytes[$i + 1] = $c.G; $bytes[$i + 2] = $c.B
        }
        $bytes[$i + 3] = 255
        $i += 4
    }
}

New-Item -ItemType Directory -Force -Path (Split-Path $outRgba) | Out-Null
[System.IO.File]::WriteAllBytes($outRgba, $bytes)
$icon.Save($outPreview, [System.Drawing.Imaging.ImageFormat]::Png)
$icon.Dispose()

Write-Output "wrote $outRgba ($($bytes.Length) bytes, ${IconSize}x${IconSize} RGBA)"
Write-Output "wrote $outPreview"

# --- 4. a real .ico for the executable and the installers ---------------------
#
# The embedded RGBA above is what eframe draws in the title bar. Windows itself
# - Explorer, the Start menu, Add/Remove Programs, the installer's own UI - all
# read an .ico resource compiled into the exe, and none of them can see the
# eframe icon. Without this the app is a generic blank page everywhere outside
# its own window.
#
# Written by hand because .NET cannot save a multi-size ICO. The format is a
# small header plus, since Vista, ordinary PNGs - so each size is a PNG and the
# directory just points at them.
$icoPath = Join-Path $root 'assets\purptoof.ico'
$sizes = @(16, 24, 32, 48, 64, 128, 256)

$pngs = @()
foreach ($sz in $sizes) {
    $b = New-Object System.Drawing.Bitmap $sz, $sz, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $gg = [System.Drawing.Graphics]::FromImage($b)
    $gg.InterpolationMode = 'HighQualityBicubic'
    $gg.PixelOffsetMode = 'HighQuality'
    $gg.Clear([System.Drawing.Color]::Black)
    $srcIcon = [System.Drawing.Image]::FromFile($outPreview)
    $gg.DrawImage($srcIcon, 0, 0, $sz, $sz)
    $srcIcon.Dispose(); $gg.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $b.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $b.Dispose()
    $pngs += , @{ Size = $sz; Bytes = $ms.ToArray() }
    $ms.Dispose()
}

$out = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($out)
$bw.Write([UInt16]0)                 # reserved
$bw.Write([UInt16]1)                 # type: icon
$bw.Write([UInt16]$pngs.Count)
# Entries are fixed-width, so every image offset is known before any is written.
$offset = 6 + (16 * $pngs.Count)
foreach ($png in $pngs) {
    # 256 is encoded as 0 in a single byte, which is the one quirk of the format.
    $dim = if ($png.Size -ge 256) { 0 } else { $png.Size }
    $bw.Write([Byte]$dim); $bw.Write([Byte]$dim)
    $bw.Write([Byte]0)               # palette entries
    $bw.Write([Byte]0)               # reserved
    $bw.Write([UInt16]1)             # colour planes
    $bw.Write([UInt16]32)            # bits per pixel
    $bw.Write([UInt32]$png.Bytes.Length)
    $bw.Write([UInt32]$offset)
    $offset += $png.Bytes.Length
}
foreach ($png in $pngs) { $bw.Write($png.Bytes) }
$bw.Flush()
[System.IO.File]::WriteAllBytes($icoPath, $out.ToArray())
$bw.Dispose(); $out.Dispose()
Write-Output "wrote $icoPath ($((Get-Item $icoPath).Length) bytes, $($sizes -join '/')px)"
