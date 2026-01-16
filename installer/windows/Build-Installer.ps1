<#
.SYNOPSIS
    Builds the Strom MSI installer with bundled dependencies.

.DESCRIPTION
    This script orchestrates the complete MSI build process:
    1. Prepares the GStreamer runtime bundle
    2. Prepares the Graphviz bundle
    3. Compiles the WiX installer

.PARAMETER Version
    Product version (e.g., "0.3.10")

.PARAMETER StromExe
    Path to the strom.exe binary

.PARAMETER StromMcpServerExe
    Path to the strom-mcp-server.exe binary

.PARAMETER FullGStreamer
    Include all GStreamer plugins (larger bundle)

.PARAMETER SkipDependencies
    Skip downloading dependencies (use existing bundles)
#>

param(
    [Parameter(Mandatory=$true)]
    [string]$Version,

    [Parameter(Mandatory=$true)]
    [string]$StromExe,

    [Parameter(Mandatory=$true)]
    [string]$StromMcpServerExe,

    [switch]$FullGStreamer,
    [switch]$SkipDependencies
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$BuildDir = "$ScriptDir\build"
$OutputDir = "$ScriptDir\output"

Write-Host "============================================" -ForegroundColor Cyan
Write-Host "  Strom MSI Installer Build" -ForegroundColor Cyan
Write-Host "============================================" -ForegroundColor Cyan
Write-Host ""
Write-Host "Version:        $Version"
Write-Host "Strom:          $StromExe"
Write-Host "MCP Server:     $StromMcpServerExe"
Write-Host "GStreamer mode: $(if ($FullGStreamer) { 'Full' } else { 'Minimal' })"
Write-Host ""

# Validate input files
if (-not (Test-Path $StromExe)) {
    Write-Error "Strom executable not found: $StromExe"
    exit 1
}
if (-not (Test-Path $StromMcpServerExe)) {
    Write-Error "Strom MCP Server executable not found: $StromMcpServerExe"
    exit 1
}

# Create build directories
New-Item -ItemType Directory -Path $BuildDir -Force | Out-Null
New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null

Push-Location $BuildDir

try {
    # Step 1: Prepare GStreamer bundle
    if (-not $SkipDependencies) {
        Write-Host ""
        Write-Host "Step 1/3: Preparing GStreamer bundle..." -ForegroundColor Yellow
        $gstArgs = @{
            OutputDir = "gstreamer-bundle"
        }
        if ($FullGStreamer) {
            $gstArgs.Full = $true
        }
        & "$ScriptDir\Prepare-GStreamer.ps1" @gstArgs
    } else {
        Write-Host "Step 1/3: Skipping GStreamer bundle (using existing)..." -ForegroundColor Yellow
    }

    # Step 2: Prepare Graphviz bundle
    if (-not $SkipDependencies) {
        Write-Host ""
        Write-Host "Step 2/3: Preparing Graphviz bundle..." -ForegroundColor Yellow
        & "$ScriptDir\Prepare-Graphviz.ps1" -OutputDir "graphviz-bundle"
    } else {
        Write-Host "Step 2/3: Skipping Graphviz bundle (using existing)..." -ForegroundColor Yellow
    }

    # Step 3: Build WiX installer
    Write-Host ""
    Write-Host "Step 3/3: Building WiX installer..." -ForegroundColor Yellow

    # Check for WiX
    $wixPath = Get-Command "wix" -ErrorAction SilentlyContinue
    if (-not $wixPath) {
        Write-Error "WiX Toolset not found. Install with: dotnet tool install --global wix"
        exit 1
    }

    # Copy WiX source files to build dir
    Copy-Item "$ScriptDir\Product.wxs" $BuildDir -Force
    Copy-Item "$ScriptDir\License.rtf" $BuildDir -Force

    # Create placeholder images if they don't exist
    if (-not (Test-Path "$ScriptDir\banner.bmp")) {
        Write-Host "  Creating placeholder banner image..."
        # Create a simple 493x58 banner (WiX standard size)
        Add-Type -AssemblyName System.Drawing
        $banner = New-Object System.Drawing.Bitmap(493, 58)
        $g = [System.Drawing.Graphics]::FromImage($banner)
        $g.Clear([System.Drawing.Color]::FromArgb(45, 45, 48))
        $font = New-Object System.Drawing.Font("Arial", 20, [System.Drawing.FontStyle]::Bold)
        $brush = [System.Drawing.Brushes]::White
        $g.DrawString("Strom", $font, $brush, 320, 12)
        $font.Dispose()
        $g.Dispose()
        $banner.Save("$BuildDir\banner.bmp", [System.Drawing.Imaging.ImageFormat]::Bmp)
        $banner.Dispose()
    } else {
        Copy-Item "$ScriptDir\banner.bmp" $BuildDir -Force
    }

    if (-not (Test-Path "$ScriptDir\dialog.bmp")) {
        Write-Host "  Creating placeholder dialog image..."
        # Create a simple 493x312 dialog background (WiX standard size)
        Add-Type -AssemblyName System.Drawing
        $dialog = New-Object System.Drawing.Bitmap(493, 312)
        $g = [System.Drawing.Graphics]::FromImage($dialog)
        $g.Clear([System.Drawing.Color]::FromArgb(45, 45, 48))
        $font = New-Object System.Drawing.Font("Arial", 36, [System.Drawing.FontStyle]::Bold)
        $brush = [System.Drawing.Brushes]::White
        $g.DrawString("Strom", $font, $brush, 30, 100)
        $font2 = New-Object System.Drawing.Font("Arial", 12)
        $g.DrawString("GStreamer Flow Engine", $font2, $brush, 30, 160)
        $font.Dispose()
        $font2.Dispose()
        $g.Dispose()
        $dialog.Save("$BuildDir\dialog.bmp", [System.Drawing.Imaging.ImageFormat]::Bmp)
        $dialog.Dispose()
    } else {
        Copy-Item "$ScriptDir\dialog.bmp" $BuildDir -Force
    }

    # Convert paths to absolute
    $absStromExe = (Resolve-Path $StromExe).Path
    $absStromMcpServerExe = (Resolve-Path $StromMcpServerExe).Path
    $absGstDir = (Resolve-Path "gstreamer-bundle").Path
    $absGvDir = (Resolve-Path "graphviz-bundle").Path

    # Build MSI using WiX
    Write-Host "  Compiling WiX installer..."

    $msiName = "strom-$Version-windows-x86_64.msi"

    wix build `
        -d ProductVersion="$Version" `
        -d StromExe="$absStromExe" `
        -d StromMcpServerExe="$absStromMcpServerExe" `
        -d GStreamerDir="$absGstDir" `
        -d GraphvizDir="$absGvDir" `
        -ext WixToolset.UI.wixext `
        -o "$OutputDir\$msiName" `
        Product.wxs

    if ($LASTEXITCODE -ne 0) {
        Write-Error "WiX build failed with exit code $LASTEXITCODE"
        exit 1
    }

    # Report results
    $msiSize = (Get-Item "$OutputDir\$msiName").Length / 1MB
    Write-Host ""
    Write-Host "============================================" -ForegroundColor Green
    Write-Host "  Build Complete!" -ForegroundColor Green
    Write-Host "============================================" -ForegroundColor Green
    Write-Host ""
    Write-Host "MSI Installer: $OutputDir\$msiName"
    Write-Host "Size: $([math]::Round($msiSize, 2)) MB"
    Write-Host ""

} finally {
    Pop-Location
}
