# Converts LICENSE into the RTF that the MSI wizard displays.
#
# WixUI will only render a licence page from RTF, and our LICENSE is plain
# text. Generated at package time rather than committed, so the two cannot
# drift - a stale licence in an installer is worse than none.
#
# Its own script because RTF escaping is backslash-heavy and nesting it inside
# another script's quoting was a reliable source of mangled output.

param(
    [string]$Source,
    [string]$Destination
)

$ErrorActionPreference = 'Stop'

$bs = '\'   # backslash, the RTF escape character. A STRING, not a
            # [char]: .Replace would otherwise bind the (char,char) overload
            # and reject a two-character replacement.
$text = Get-Content $Source -Raw

# Order matters: escape backslashes first, or the escapes we add below get
# escaped again.
$text = $text.Replace($bs, "$bs$bs")
$text = $text.Replace('{', "$bs{")
$text = $text.Replace('}', "$bs}")
# RTF has no concept of a newline; paragraphs are explicit.
$text = $text.Replace("`r`n", "${bs}par`r`n")
$text = $text.Replace("`n", "${bs}par`n")

$header = "{${bs}rtf1${bs}ansi${bs}deff0" +
          "{${bs}fonttbl{${bs}f0${bs}fnil${bs}fcharset0 Segoe UI;}}" +
          "${bs}fs18 "

New-Item -ItemType Directory -Force -Path (Split-Path $Destination) | Out-Null
Set-Content -Path $Destination -Value ($header + $text + '}') -Encoding ASCII
Write-Output "wrote $Destination ($((Get-Item $Destination).Length) bytes)"
