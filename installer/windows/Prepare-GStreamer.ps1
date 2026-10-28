<#
.SYNOPSIS
    Prepares a minimal GStreamer runtime bundle for the Strom MSI installer.

.DESCRIPTION
    Downloads the GStreamer installer and extracts only the necessary
    components for Strom to function. This creates a smaller bundle by
    excluding development files, documentation, and unused plugins.

.PARAMETER Version
    GStreamer version to download (default: 1.28.8)

.PARAMETER OutputDir
    Directory to extract files to (default: gstreamer-bundle)

.PARAMETER Full
    If specified, copies all of bin, lib\gstreamer-1.0, share and libexec
    (larger bundle, ~320MB). This is what the released MSI ships.
    Both modes copy lib\gio\modules (the GIO TLS backend).
    Otherwise, includes every DLL in bin but only the essential plugins listed
    below, and fails if one of them is missing from the installer.
#>

param(
    [string]$Version = "1.28.8",
    [string]$OutputDir = "gstreamer-bundle",
    [switch]$Full
)

$ErrorActionPreference = "Stop"

# GStreamer download URL from Eyevinn mirror (freedesktop.org blocks CI)
$BaseUrl = "https://github.com/Eyevinn/strom/releases/download/gstreamer-deps"
# Since 1.28 upstream ships one Inno Setup installer instead of MSIs.
$InstallerFile = "gstreamer-1.0-msvc-x86_64-$Version.exe"
$InstallerUrl = "$BaseUrl/$InstallerFile"

# Temporary extraction directory
$TempDir = "gstreamer-temp"
$GstRoot = "$((Get-Location).Path)\$TempDir"

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

# Download the GStreamer installer
if (-not (Test-Path $InstallerFile)) {
    Write-Host "Downloading GStreamer from $InstallerUrl..."
    $ProgressPreference = "SilentlyContinue"
    Invoke-WebRequest -Uri $InstallerUrl -OutFile $InstallerFile -UseBasicParsing
    Write-Host "Download complete: $((Get-Item $InstallerFile).Length / 1MB) MB"
} else {
    Write-Host "Using existing download: $InstallerFile"
}

# Unpack the runtime into a temp directory. /portable=1 writes no
# uninstaller, environment variable or registry key and skips the Visual C++
# Redistributable; /TYPE=runtime takes every runtime component and no
# development files.
Write-Host "Unpacking GStreamer..."
$LogFile = "$((Get-Location).Path)\gstreamer-install.log"
$p = Start-Process ".\$InstallerFile" -Wait -NoNewWindow -PassThru `
    -ArgumentList "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /portable=1 /TYPE=runtime /DIR=`"$GstRoot`" /LOG=`"$LogFile`""
if ($p.ExitCode -ne 0) {
    if (Test-Path $LogFile) { Get-Content $LogFile -Tail 50 }
    throw "GStreamer installer failed with exit code $($p.ExitCode)"
}

foreach ($d in @("bin", "lib\gstreamer-1.0", "libexec\gstreamer-1.0")) {
    if (-not (Test-Path "$GstRoot\$d")) {
        throw "GStreamer unpack is missing $GstRoot\$d"
    }
}
Write-Host "Unpacked $((Get-ChildItem "$GstRoot\bin\*.dll").Count) DLLs, $((Get-ChildItem "$GstRoot\lib\gstreamer-1.0\*.dll").Count) plugins"

# Create output directory structure
New-Item -ItemType Directory -Path "$OutputDir\bin" -Force | Out-Null
New-Item -ItemType Directory -Path "$OutputDir\lib\gstreamer-1.0" -Force | Out-Null
New-Item -ItemType Directory -Path "$OutputDir\share" -Force | Out-Null

# Every DLL in bin is copied, in both modes. The plugins below pull in
# libraries of their own (gstx264.dll needs the x264 DLL, and so on), and a
# hand-kept list of those missed some and broke the plugins that needed them.

# Essential plugins for Strom. Names as GStreamer 1.28.8 ships them; a missing
# one fails the script (see below), so a rename upstream shows up here instead
# of as a missing element on a user's machine.
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
    "gstdav1d.dll",
    "gstrav1e.dll",
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

if (-not $Full) {
    $missing = @($EssentialPlugins | Where-Object { -not (Test-Path "$GstRoot\lib\gstreamer-1.0\$_") })
    if ($missing.Count -gt 0) {
        throw "GStreamer $Version does not ship these essential files (renamed or dropped upstream?): $($missing -join ', ')"
    }
}

# Copy executables
Write-Host "Copying GStreamer tools..."
$tools = @("gst-launch-1.0.exe", "gst-inspect-1.0.exe", "gst-discoverer-1.0.exe", "gst-typefind-1.0.exe")
if ($Full) {
    # Everything in bin, as the released MSI has always shipped it.
    $tools = Get-ChildItem "$GstRoot\bin" -File | Where-Object { $_.Extension -ne ".dll" } | Select-Object -ExpandProperty Name
}
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
    Copy-Item $dll.FullName "$OutputDir\bin\" -Force
    $copied++
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

# Copy GIO modules, in both modes. GIO gets TLS from gioopenssl.dll, loaded
# from lib\gio\modules next to the GLib in bin. Without it every https://
# request fails with "TLS support is not available": souphttpsrc, and with it
# HLS and DASH in the Media Player, cannot open any https URL (issue #1079).
Write-Host "Copying GIO modules..."
$gioModules = "$GstRoot\lib\gio\modules"
if (-not (Test-Path "$gioModules\gioopenssl.dll")) {
    throw "GStreamer $Version has no $gioModules\gioopenssl.dll: GIO would have no TLS backend, and no https:// source would open"
}
New-Item -ItemType Directory -Path "$OutputDir\lib\gio\modules" -Force | Out-Null
Copy-Item "$gioModules\*.dll" "$OutputDir\lib\gio\modules\" -Force
Write-Host "  Copied $((Get-ChildItem "$OutputDir\lib\gio\modules\*.dll").Count) GIO modules"

# Copy share files (GStreamer registry, etc.)
Write-Host "Copying share files..."
if ($Full) {
    Copy-Item "$GstRoot\share\*" "$OutputDir\share\" -Recurse -Force
} elseif (Test-Path "$GstRoot\share\gstreamer-1.0") {
    Copy-Item "$GstRoot\share\gstreamer-1.0" "$OutputDir\share\" -Recurse -Force
}

# Copy libexec files (contains gst-ptp-helper.exe for PTP clock support)
Write-Host "Copying libexec files..."
if (Test-Path "$GstRoot\libexec") {
    New-Item -ItemType Directory -Path "$OutputDir\libexec" -Force | Out-Null
    Copy-Item "$GstRoot\libexec\*" "$OutputDir\libexec\" -Recurse -Force
    Write-Host "  Copied libexec files (including gst-ptp-helper.exe)"
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
Write-Host "  lib/     - GStreamer plugins, GIO modules (TLS)"
Write-Host "  libexec/ - GStreamer helper executables (gst-ptp-helper.exe)"
Write-Host "  share/   - GStreamer data files"
