<#
.SYNOPSIS
    Prepares a minimal GStreamer runtime bundle for the Strom MSI installer.

.DESCRIPTION
    Downloads the GStreamer runtime MSI and extracts only the necessary
    components for Strom to function. This creates a smaller bundle by
    excluding development files, documentation, and unused plugins.

.PARAMETER Version
    GStreamer version to download (default: 1.24.13)

.PARAMETER OutputDir
    Directory to extract files to (default: gstreamer-bundle)

.PARAMETER Full
    If specified, includes all plugins (larger bundle, ~300MB)
    Otherwise, includes only essential plugins (~150MB)
#>

param(
    [string]$Version = "1.24.13",
    [string]$OutputDir = "gstreamer-bundle",
    [switch]$Full
)

$ErrorActionPreference = "Stop"

# GStreamer download URL from Eyevinn mirror (freedesktop.org blocks CI)
$BaseUrl = "https://github.com/Eyevinn/strom/releases/download/gstreamer-deps"
$MsiFile = "gstreamer-1.0-msvc-x86_64-$Version.msi"
$MsiUrl = "$BaseUrl/$MsiFile"

# Temporary extraction directory
$TempDir = "gstreamer-temp"
$GstRoot = "$TempDir\gstreamer\1.0\msvc_x86_64"

Write-Host "=== Preparing GStreamer Runtime Bundle ===" -ForegroundColor Cyan
Write-Host "Version: $Version"
Write-Host "Output: $OutputDir"
Write-Host "Mode: $(if ($Full) { 'Full (all plugins)' } else { 'Minimal (essential plugins)' })"
Write-Host ""

# Clean up previous runs
if (Test-Path $TempDir) {
    Write-Host "Cleaning up previous extraction..."
    Remove-Item -Recurse -Force $TempDir
}
if (Test-Path $OutputDir) {
    Remove-Item -Recurse -Force $OutputDir
}

# Download GStreamer MSI
if (-not (Test-Path $MsiFile)) {
    Write-Host "Downloading GStreamer runtime from $MsiUrl..."
    Invoke-WebRequest -Uri $MsiUrl -OutFile $MsiFile -UseBasicParsing
    Write-Host "Download complete: $((Get-Item $MsiFile).Length / 1MB) MB"
} else {
    Write-Host "Using existing download: $MsiFile"
}

# Extract MSI using msiexec to a temp directory
Write-Host "Extracting GStreamer MSI..."
$installArgs = "/a `"$((Get-Location).Path)\$MsiFile`" /qn TARGETDIR=`"$((Get-Location).Path)\$TempDir`""
Start-Process msiexec.exe -ArgumentList $installArgs -Wait -NoNewWindow

if (-not (Test-Path $GstRoot)) {
    Write-Error "Extraction failed - GStreamer root not found at $GstRoot"
    exit 1
}

# Create output directory structure
New-Item -ItemType Directory -Path "$OutputDir\bin" -Force | Out-Null
New-Item -ItemType Directory -Path "$OutputDir\lib\gstreamer-1.0" -Force | Out-Null
New-Item -ItemType Directory -Path "$OutputDir\share" -Force | Out-Null

# Essential DLLs to include (core runtime)
$EssentialDlls = @(
    # GStreamer core
    "gstreamer-1.0-0.dll",
    "gstbase-1.0-0.dll",
    "gstcontroller-1.0-0.dll",
    "gstnet-1.0-0.dll",
    "gstcheck-1.0-0.dll",

    # GStreamer libraries
    "gstapp-1.0-0.dll",
    "gstaudio-1.0-0.dll",
    "gstvideo-1.0-0.dll",
    "gstpbutils-1.0-0.dll",
    "gstrtp-1.0-0.dll",
    "gstrtsp-1.0-0.dll",
    "gstsdp-1.0-0.dll",
    "gsttag-1.0-0.dll",
    "gstgl-1.0-0.dll",
    "gstwebrtc-1.0-0.dll",
    "gstcodecs-1.0-0.dll",
    "gstcodecparsers-1.0-0.dll",
    "gstsctp-1.0-0.dll",
    "gstplay-1.0-0.dll",
    "gstplayer-1.0-0.dll",
    "gstriff-1.0-0.dll",
    "gstfft-1.0-0.dll",
    "gstallocators-1.0-0.dll",
    "gstinsertbin-1.0-0.dll",
    "gstmpegts-1.0-0.dll",
    "gstadaptivedemux-1.0-0.dll",
    "gstbadaudio-1.0-0.dll",
    "gstisoff-1.0-0.dll",
    "gsturidownloader-1.0-0.dll",

    # GLib and dependencies
    "glib-2.0-0.dll",
    "gobject-2.0-0.dll",
    "gmodule-2.0-0.dll",
    "gio-2.0-0.dll",
    "gthread-2.0-0.dll",
    "intl-8.dll",
    "pcre2-8-0.dll",
    "ffi-8.dll",
    "z1.dll",
    "zlib1.dll",

    # SSL/TLS
    "libssl-3-x64.dll",
    "libcrypto-3-x64.dll",

    # Audio/Video codecs
    "avcodec-61.dll",
    "avformat-61.dll",
    "avutil-59.dll",
    "swresample-5.dll",
    "swscale-8.dll",
    "avfilter-10.dll",

    # Image formats
    "jpeg62.dll",
    "libpng16.dll",

    # Other dependencies
    "orc-0.4-0.dll",
    "libnice-0.dll",
    "soup-3.0-0.dll",
    "json-glib-1.0-0.dll",
    "libxml2.dll",
    "iconv-2.dll",
    "lzma.dll",
    "bz2.dll",
    "opus.dll",
    "libsrtp2.dll",
    "graphene-1.0-0.dll",
    "cairo.dll",
    "cairo-gobject.dll",
    "pango-1.0-0.dll",
    "pangocairo-1.0-0.dll",
    "pangoft2-1.0-0.dll",
    "pangowin32-1.0-0.dll",
    "harfbuzz.dll",
    "freetype.dll",
    "fontconfig-1.dll",
    "expat.dll",
    "pixman-1-0.dll"
)

# Essential plugins for Strom
$EssentialPlugins = @(
    # Core plugins
    "gstcoreelements.dll",
    "gsttypefindfunctions.dll",
    "gstplayback.dll",
    "gstautodetect.dll",
    "gstapp.dll",

    # Network/Streaming
    "gstrtp.dll",
    "gstrtpmanager.dll",
    "gstrtsp.dll",
    "gstudp.dll",
    "gsttcp.dll",
    "gstsoup.dll",
    "gstwebrtc.dll",
    "gstdtls.dll",
    "gstnice.dll",
    "gstsctp.dll",
    "gstsrtp.dll",
    "gstrist.dll",
    "gsthls.dll",
    "gstdash.dll",
    "gstinter.dll",

    # Video
    "gstvideoconvertscale.dll",
    "gstvideofilter.dll",
    "gstvideotestsrc.dll",
    "gstvideoparsersbad.dll",
    "gstrawparse.dll",
    "gstvideorate.dll",
    "gstd3d11.dll",
    "gstd3d12.dll",
    "gstnvcodec.dll",
    "gstqsv.dll",
    "gstamfcodec.dll",
    "gstopenh264.dll",
    "gstx264.dll",
    "gstx265.dll",
    "gstvpx.dll",
    "gstav1.dll",
    "gstaom.dll",
    "gstsvtav1.dll",
    "gstlibav.dll",
    "gstdeinterlace.dll",
    "gstimagefreeze.dll",
    "gstdebug.dll",

    # Audio
    "gstaudioconvert.dll",
    "gstaudioresample.dll",
    "gstaudiotestsrc.dll",
    "gstaudiomixer.dll",
    "gstvolume.dll",
    "gstopus.dll",
    "gstwasapi.dll",
    "gstwasapi2.dll",
    "gstdirectsound.dll",
    "gstlame.dll",
    "gstaudiofx.dll",

    # Muxers/Demuxers
    "gstmatroska.dll",
    "gstmpegtsdemux.dll",
    "gstmpegtsmux.dll",
    "gstisomp4.dll",
    "gstflv.dll",
    "gstogg.dll",
    "gstavi.dll",
    "gstmultifile.dll",

    # Image
    "gstpng.dll",
    "gstjpeg.dll",

    # OpenGL
    "gstopengl.dll",

    # Encoding
    "gstvideobox.dll",
    "gstcompositor.dll",
    "gstencoding.dll",

    # Utils
    "gstgio.dll",
    "gstaudioparsers.dll",
    "gstadaptivedemux2.dll"
)

# Copy executables
Write-Host "Copying GStreamer tools..."
$tools = @("gst-launch-1.0.exe", "gst-inspect-1.0.exe", "gst-discoverer-1.0.exe", "gst-typefind-1.0.exe")
foreach ($tool in $tools) {
    $src = "$GstRoot\bin\$tool"
    if (Test-Path $src) {
        Copy-Item $src "$OutputDir\bin\" -Force
    }
}

# Copy DLLs
Write-Host "Copying runtime DLLs..."
$copied = 0
$srcBin = "$GstRoot\bin"
$dllFiles = Get-ChildItem "$srcBin\*.dll" -File

foreach ($dll in $dllFiles) {
    $include = $Full -or ($EssentialDlls -contains $dll.Name)

    if ($include) {
        Copy-Item $dll.FullName "$OutputDir\bin\" -Force
        $copied++
    }
}
Write-Host "  Copied $copied DLLs"

# Copy plugins
Write-Host "Copying GStreamer plugins..."
$copiedPlugins = 0
$srcPlugins = "$GstRoot\lib\gstreamer-1.0"
$pluginFiles = Get-ChildItem "$srcPlugins\*.dll" -File -ErrorAction SilentlyContinue

if ($pluginFiles) {
    foreach ($plugin in $pluginFiles) {
        $include = $Full -or ($EssentialPlugins -contains $plugin.Name)

        if ($include) {
            Copy-Item $plugin.FullName "$OutputDir\lib\gstreamer-1.0\" -Force
            $copiedPlugins++
        }
    }
}
Write-Host "  Copied $copiedPlugins plugins"

# Copy share files (GStreamer registry, etc.)
Write-Host "Copying share files..."
if (Test-Path "$GstRoot\share\gstreamer-1.0") {
    Copy-Item "$GstRoot\share\gstreamer-1.0" "$OutputDir\share\" -Recurse -Force
}

# Create registry initialization batch file
$registryBat = @"
@echo off
REM Initialize GStreamer registry for Strom
set GST_PLUGIN_PATH=%~dp0..\lib\gstreamer-1.0
set PATH=%~dp0;%PATH%
gst-inspect-1.0.exe --version
echo GStreamer registry initialized.
"@
$registryBat | Out-File -FilePath "$OutputDir\bin\init-gstreamer.bat" -Encoding ASCII

# Clean up temp directory
Write-Host "Cleaning up..."
Remove-Item -Recurse -Force $TempDir

# Report size
$totalSize = (Get-ChildItem -Recurse $OutputDir | Measure-Object -Property Length -Sum).Sum / 1MB
Write-Host ""
Write-Host "=== Bundle Complete ===" -ForegroundColor Green
Write-Host "Total size: $([math]::Round($totalSize, 2)) MB"
Write-Host "Location: $OutputDir"
Write-Host ""
Write-Host "Contents:"
Write-Host "  bin/     - GStreamer tools and DLLs"
Write-Host "  lib/     - GStreamer plugins"
Write-Host "  share/   - GStreamer data files"
