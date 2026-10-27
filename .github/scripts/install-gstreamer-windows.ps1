<#
.SYNOPSIS
    Installs GStreamer (runtime and development files) and pkg-config on a
    GitHub Windows runner, for the CI and release builds.

.DESCRIPTION
    Since 1.28 upstream ships one Inno Setup installer per arch, with the
    development files as components of it, instead of a runtime and a devel
    MSI. The installer comes from our gstreamer-deps release mirror, because
    freedesktop.org blocks CI downloads.

    Writes GSTREAMER_1_0_ROOT_MSVC_X86_64 and PKG_CONFIG_PATH to GITHUB_ENV
    and the bin directory to GITHUB_PATH.

.PARAMETER Version
    GStreamer version to install.

.PARAMETER Root
    The msvc_x86_64 directory to install into.
#>

param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$Root = "D:\gstreamer\1.0\msvc_x86_64"
)

$ErrorActionPreference = "Stop"

$BaseUrl = "https://github.com/Eyevinn/strom/releases/download/gstreamer-deps"
$Installer = "gstreamer-1.0-msvc-x86_64-$Version.exe"
$LogFile = "$((Get-Location).Path)\gstreamer-install.log"

Write-Host "Downloading GStreamer $Version..."
$ProgressPreference = "SilentlyContinue"
Invoke-WebRequest -Uri "$BaseUrl/$Installer" -OutFile $Installer

# /TYPE=devel is "Runtime and development headers": every runtime component
# (x264enc's codecs-gpl-restricted group included) is a member of that type,
# plus the headers and .pc files. It replaces the MSI's ADDLOCAL=ALL, without
# which the MSI left out plugin groups and both recorder tests skipped green on
# the Windows runner alone. /DIR is the msvc_x86_64 root itself, not the parent
# INSTALLDIR the MSI took.
#
# Start-Process discards the exit code without -PassThru, so a failed install
# would read as green here and surface 20 minutes later as a missing element,
# which looks like a broken test rather than a broken install.
Write-Host "Installing GStreamer $Version into $Root..."
$p = Start-Process ".\$Installer" -Wait -NoNewWindow -PassThru `
    -ArgumentList "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /TYPE=devel /DIR=`"$Root`" /LOG=`"$LogFile`""
if ($p.ExitCode -ne 0) {
    if (Test-Path $LogFile) { Get-Content $LogFile -Tail 50 }
    throw "GStreamer installer failed with exit code $($p.ExitCode)"
}

# A component selection that silently shrinks fails here, not as a skipped
# test: x264enc stands for the optional plugin groups, the .pc file and the
# header for the development files.
foreach ($f in @("lib\gstreamer-1.0\gstx264.dll", "lib\pkgconfig\gstreamer-1.0.pc", "include\gstreamer-1.0\gst\gst.h")) {
    if (-not (Test-Path "$Root\$f")) {
        throw "GStreamer install is missing $f"
    }
}

# --allow-empty-checksums: pkgconfiglite downloads over plain HTTP.
choco install pkgconfiglite --yes --no-progress --allow-empty-checksums
if ($LASTEXITCODE -ne 0) {
    throw "choco install pkgconfiglite failed with exit code $LASTEXITCODE"
}

"GSTREAMER_1_0_ROOT_MSVC_X86_64=$Root\" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
"PKG_CONFIG_PATH=$Root\lib\pkgconfig" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
"$Root\bin" | Out-File -FilePath $env:GITHUB_PATH -Append -Encoding utf8
