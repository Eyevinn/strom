<#
.SYNOPSIS
    Prepares a minimal Graphviz bundle for the Strom MSI installer.

.DESCRIPTION
    Downloads Graphviz and extracts only the necessary components
    for DOT graph visualization in Strom.

.PARAMETER Version
    Graphviz version to download (default: 12.2.1)

.PARAMETER OutputDir
    Directory to extract files to (default: graphviz-bundle)
#>

param(
    [string]$Version = "12.2.1",
    [string]$OutputDir = "graphviz-bundle"
)

$ErrorActionPreference = "Stop"

# Graphviz download URL
$ZipFile = "windows_10_cmake_Release_graphviz-install-$Version-win64.zip"
$ZipUrl = "https://gitlab.com/api/v4/projects/4207231/packages/generic/graphviz-releases/$Version/$ZipFile"

# Temp extraction directory
$TempDir = "graphviz-temp"

Write-Host "=== Preparing Graphviz Bundle ===" -ForegroundColor Cyan
Write-Host "Version: $Version"
Write-Host "Output: $OutputDir"
Write-Host ""

# Clean up previous runs
if (Test-Path $TempDir) {
    Write-Host "Cleaning up previous extraction..."
    Remove-Item -Recurse -Force $TempDir
}
if (Test-Path $OutputDir) {
    Remove-Item -Recurse -Force $OutputDir
}

# Download Graphviz
if (-not (Test-Path $ZipFile)) {
    Write-Host "Downloading Graphviz from $ZipUrl..."
    Invoke-WebRequest -Uri $ZipUrl -OutFile $ZipFile -UseBasicParsing
    Write-Host "Download complete: $((Get-Item $ZipFile).Length / 1MB) MB"
} else {
    Write-Host "Using existing download: $ZipFile"
}

# Extract ZIP
Write-Host "Extracting Graphviz..."
Expand-Archive -Path $ZipFile -DestinationPath $TempDir -Force

# Find the extracted directory (it's nested)
$extractedDir = Get-ChildItem $TempDir -Directory | Select-Object -First 1

if (-not $extractedDir) {
    Write-Error "Extraction failed - no directory found in $TempDir"
    exit 1
}

$gvRoot = $extractedDir.FullName

# Create output directory structure
New-Item -ItemType Directory -Path "$OutputDir\bin" -Force | Out-Null

# Essential executables for DOT graph generation
$EssentialExes = @(
    "dot.exe",
    "neato.exe",
    "circo.exe",
    "fdp.exe",
    "sfdp.exe",
    "twopi.exe"
)

# Essential DLLs
$EssentialDlls = @(
    "cdt.dll",
    "cgraph.dll",
    "gvc.dll",
    "gvplugin_core.dll",
    "gvplugin_dot_layout.dll",
    "gvplugin_gd.dll",
    "gvplugin_neato_layout.dll",
    "gvplugin_pango.dll",
    "pathplan.dll",
    "xdot.dll",
    # Dependencies
    "expat.dll",
    "libpng16.dll",
    "zlib1.dll"
)

# Copy executables
Write-Host "Copying Graphviz tools..."
$srcBin = "$gvRoot\bin"
foreach ($exe in $EssentialExes) {
    $src = "$srcBin\$exe"
    if (Test-Path $src) {
        Copy-Item $src "$OutputDir\bin\" -Force
        Write-Host "  $exe"
    }
}

# Copy DLLs
Write-Host "Copying runtime DLLs..."
$dllFiles = Get-ChildItem "$srcBin\*.dll" -File -ErrorAction SilentlyContinue

if ($dllFiles) {
    foreach ($dll in $dllFiles) {
        Copy-Item $dll.FullName "$OutputDir\bin\" -Force
    }
    Write-Host "  Copied $($dllFiles.Count) DLLs"
}

# Copy config file if exists
$configFile = "$gvRoot\bin\config6"
if (Test-Path $configFile) {
    Copy-Item $configFile "$OutputDir\bin\" -Force
    Write-Host "  Copied config6"
}

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
Write-Host "  bin/     - Graphviz tools (dot, neato, etc.) and DLLs"
