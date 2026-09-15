# Applies a rounded-corner alpha mask to a square Bitmap, in place.
#
# Dot-sourced by make-icon.ps1.
#
# The corners have to become TRANSPARENT, not black. The artwork sits on an
# opaque black field, so merely drawing black corners would leave the icon
# looking exactly as square as before - the rounding only reads if what is
# behind the icon shows through.
#
# Supersampled, because a hard in/out test turns the curve into a visible
# staircase at 16 and 24px, which is where most people will see it.

function Set-RoundedCorners {
    param(
        [System.Drawing.Bitmap]$Bitmap,
        # Corner radius as a fraction of the edge. Around 0.18 is the modern
        # Windows/macOS app-icon proportion: clearly rounded, not a lozenge.
        [double]$RadiusFraction = 0.18,
        [int]$Samples = 4
    )

    $size = $Bitmap.Width
    $r = $size * $RadiusFraction
    if ($r -lt 1) { return }

    # Only the four corner squares can differ from fully opaque, so the rest of
    # the image is left untouched - at 256px that is a few thousand pixels
    # instead of sixty-five thousand.
    $span = [int][Math]::Ceiling($r)
    $corners = @(
        @{ X0 = 0;             Y0 = 0;             CX = $r;            CY = $r },
        @{ X0 = $size - $span; Y0 = 0;             CX = $size - $r;    CY = $r },
        @{ X0 = 0;             Y0 = $size - $span; CX = $r;            CY = $size - $r },
        @{ X0 = $size - $span; Y0 = $size - $span; CX = $size - $r;    CY = $size - $r }
    )

    foreach ($c in $corners) {
        for ($y = $c.Y0; $y -lt $c.Y0 + $span; $y++) {
            for ($x = $c.X0; $x -lt $c.X0 + $span; $x++) {
                if ($x -lt 0 -or $y -lt 0 -or $x -ge $size -or $y -ge $size) { continue }

                $hits = 0
                for ($sy = 0; $sy -lt $Samples; $sy++) {
                    for ($sx = 0; $sx -lt $Samples; $sx++) {
                        $px = $x + ($sx + 0.5) / $Samples
                        $py = $y + ($sy + 0.5) / $Samples
                        # Outside the corner's quadrant the pixel is in the
                        # straight part of the edge and always inside.
                        $dx = $px - $c.CX
                        $dy = $py - $c.CY
                        $outX = ($c.CX -lt $size / 2) ? ($dx -lt 0) : ($dx -gt 0)
                        $outY = ($c.CY -lt $size / 2) ? ($dy -lt 0) : ($dy -gt 0)
                        if ($outX -and $outY) {
                            if ($dx * $dx + $dy * $dy -le $r * $r) { $hits++ }
                        } else {
                            $hits++
                        }
                    }
                }

                $coverage = $hits / ($Samples * $Samples)
                if ($coverage -ge 1.0) { continue }

                $old = $Bitmap.GetPixel($x, $y)
                $alpha = [int][Math]::Round(255 * $coverage)
                # Premultiply the colour as well, so a partially covered edge
                # pixel does not keep full-brightness purple behind a low alpha
                # and fringe against a light background.
                $Bitmap.SetPixel($x, $y, [System.Drawing.Color]::FromArgb(
                        $alpha,
                        [int]($old.R * $coverage),
                        [int]($old.G * $coverage),
                        [int]($old.B * $coverage)))
            }
        }
    }
}
