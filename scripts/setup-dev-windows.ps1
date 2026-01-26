#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Sets up a Windows development environment for Strom.

.DESCRIPTION
    Installs Rust, build tools, WASM toolchain, and GStreamer for Strom development.
    Run this script as Administrator in PowerShell.

.NOTES
    EXECUTION POLICY:
    If you get "running scripts is disabled on this system" error, run:
        Set-ExecutionPolicy -ExecutionPolicy Bypass -Scope Process
    Or run the script directly with:
        powershell -ExecutionPolicy Bypass -File .\setup-dev-windows.ps1

    WINDOWS SANDBOX:
    Windows Sandbox doesn't have winget pre-installed. Run these commands first:

        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri "https://aka.ms/getwinget" -OutFile "$env:TEMP\winget.msixbundle"
        Invoke-WebRequest -Uri "https://aka.ms/windowsappsdk/1.8/latest/windowsappruntimeinstall-x64.exe" -OutFile "$env:TEMP\appruntime.exe"
        & "$env:TEMP\appruntime.exe" --quiet
        Invoke-WebRequest -Uri "https://github.com/microsoft/winget-cli/releases/download/v1.9.25180/DesktopAppInstaller_Dependencies.zip" -OutFile "$env:TEMP\deps.zip"
        Expand-Archive -Path "$env:TEMP\deps.zip" -DestinationPath "$env:TEMP\deps"
        Add-AppxPackage -Path "$env:TEMP\deps\x64\Microsoft.VCLibs.140.00.UWPDesktop_14.0.33728.0_x64.appx"
        Add-AppxPackage -Path "$env:TEMP\winget.msixbundle"
#>

param(
    [switch]$SkipGStreamer,
    [string]$GStreamerVersion = "1.26.10"
)

$ErrorActionPreference = "Stop"

function Write-Step {
    param([string]$Message)
    Write-Host "`n===> $Message" -ForegroundColor Cyan
}

function Test-CommandExists {
    param([string]$Command)
    $null -ne (Get-Command $Command -ErrorAction SilentlyContinue)
}

function Add-ToPath {
    param([string]$Path)
    if ($env:PATH -notlike "*$Path*") {
        $env:PATH = "$env:PATH;$Path"
    }
}

# Refresh PATH from system to detect previously installed tools
$env:PATH = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH", "User")

# ============================================================================
# Rust
# ============================================================================
Write-Step "Installing Rust via rustup"

if (Test-CommandExists "rustup") {
    Write-Host "Rustup already installed, updating..." -ForegroundColor Yellow
    rustup update
} else {
    winget install --id Rustlang.Rustup -e --source winget --accept-source-agreements --accept-package-agreements
    # Refresh PATH for this session
    $env:PATH = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH", "User")
}

# ============================================================================
# Visual Studio Build Tools
# ============================================================================
Write-Step "Installing Visual Studio 2022 Build Tools"

$vsInstalled = Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64" -ErrorAction SilentlyContinue
if (-not $vsInstalled) {
    winget install --id Microsoft.VisualStudio.2022.BuildTools -e --source winget --accept-source-agreements --accept-package-agreements `
        --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
    Write-Host "Build Tools installed. You may need to restart your terminal." -ForegroundColor Yellow
} else {
    Write-Host "Visual Studio Build Tools already installed" -ForegroundColor Yellow
}

# ============================================================================
# CMake
# ============================================================================
Write-Step "Installing CMake"

if (Test-CommandExists "cmake") {
    Write-Host "CMake already installed" -ForegroundColor Yellow
} else {
    winget install --id Kitware.CMake -e --source winget --accept-source-agreements --accept-package-agreements
    Add-ToPath "C:\Program Files\CMake\bin"
}

# ============================================================================
# NASM
# ============================================================================
Write-Step "Installing NASM"

if (Test-CommandExists "nasm") {
    Write-Host "NASM already installed" -ForegroundColor Yellow
} else {
    winget install --id NASM.NASM -e --source winget --accept-source-agreements --accept-package-agreements
    Add-ToPath "C:\Program Files\NASM"
}

# ============================================================================
# Graphviz (for pipeline visualization)
# ============================================================================
Write-Step "Installing Graphviz"

if (Test-CommandExists "dot") {
    Write-Host "Graphviz already installed" -ForegroundColor Yellow
} else {
    winget install --id Graphviz.Graphviz -e --source winget --accept-source-agreements --accept-package-agreements
    Add-ToPath "C:\Program Files\Graphviz\bin"
}

# ============================================================================
# pkg-config (for native dependency discovery)
# ============================================================================
Write-Step "Installing pkg-config"

if (Test-CommandExists "pkg-config") {
    Write-Host "pkg-config already installed" -ForegroundColor Yellow
} else {
    winget install --id bloodrock.pkg-config-lite -e --source winget --accept-source-agreements --accept-package-agreements
    Add-ToPath "C:\Program Files\pkg-config-lite\bin"
}

# ============================================================================
# WASM Toolchain
# ============================================================================
Write-Step "Setting up WASM toolchain"

# Ensure rustup is available - refresh PATH and add cargo bin directory
$env:PATH = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH", "User")
Add-ToPath "$env:USERPROFILE\.cargo\bin"

if (-not (Test-CommandExists "rustup")) {
    Write-Host "rustup not found in PATH. Please restart your terminal and run this script again." -ForegroundColor Red
    Write-Host "If the problem persists, install rustup manually from https://rustup.rs" -ForegroundColor Yellow
    exit 1
}

rustup target add wasm32-unknown-unknown

if (Test-CommandExists "trunk") {
    Write-Host "Trunk already installed" -ForegroundColor Yellow
} else {
    cargo install trunk
}

# ============================================================================
# GStreamer
# ============================================================================
if (-not $SkipGStreamer) {
    Write-Step "Installing GStreamer $GStreamerVersion"

    $gstreamerRoot = "C:\gstreamer\1.0\msvc_x86_64"

    if (Test-Path $gstreamerRoot) {
        Write-Host "GStreamer already installed at $gstreamerRoot" -ForegroundColor Yellow
    } else {
        $tempDir = "$env:TEMP\gstreamer-install"
        New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

        # Download from Strom GitHub releases mirror (freedesktop.org blocks automated downloads)
        $baseUrl = "https://github.com/Eyevinn/strom/releases/download/gstreamer-deps"
        $runtimeMsi = "gstreamer-1.0-msvc-x86_64-$GStreamerVersion.msi"
        $develMsi = "gstreamer-1.0-devel-msvc-x86_64-$GStreamerVersion.msi"

        # Disable progress bar for much faster downloads
        $ProgressPreference = 'SilentlyContinue'

        Write-Host "Downloading GStreamer runtime from GitHub mirror..."
        Invoke-WebRequest -Uri "$baseUrl/$runtimeMsi" -OutFile "$tempDir\$runtimeMsi"

        Write-Host "Downloading GStreamer development SDK from GitHub mirror..."
        Invoke-WebRequest -Uri "$baseUrl/$develMsi" -OutFile "$tempDir\$develMsi"

        $ProgressPreference = 'Continue'

        Write-Host "Installing GStreamer runtime..."
        Start-Process msiexec.exe -ArgumentList "/i `"$tempDir\$runtimeMsi`" /quiet /norestart INSTALLDIR=C:\gstreamer" -Wait

        Write-Host "Installing GStreamer development SDK..."
        Start-Process msiexec.exe -ArgumentList "/i `"$tempDir\$develMsi`" /quiet /norestart INSTALLDIR=C:\gstreamer" -Wait

        Remove-Item -Recurse -Force $tempDir
    }

    # Set environment variables permanently
    Write-Step "Configuring GStreamer environment variables"

    # Convert to MSYS2-style path: C:\path -> /c/path (pkg-config treats ':' as path separator)
    $pkgConfigPath = "$gstreamerRoot\lib\pkgconfig" -replace '\\', '/'
    $pkgConfigPath = $pkgConfigPath -replace '^([A-Za-z]):', { '/' + $_.Groups[1].Value.ToLower() }

    [System.Environment]::SetEnvironmentVariable("GSTREAMER_1_0_ROOT_MSVC_X86_64", $gstreamerRoot, "Machine")
    [System.Environment]::SetEnvironmentVariable("PKG_CONFIG_PATH", $pkgConfigPath, "Machine")

    # Add to system PATH
    $machinePath = [System.Environment]::GetEnvironmentVariable("PATH", "Machine")
    if ($machinePath -notlike "*$gstreamerRoot\bin*") {
        [System.Environment]::SetEnvironmentVariable("PATH", "$machinePath;$gstreamerRoot\bin", "Machine")
    }

    # Set for current session
    $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = $gstreamerRoot
    $env:PKG_CONFIG_PATH = $pkgConfigPath
    Add-ToPath "$gstreamerRoot\bin"
}

# ============================================================================
# Verify Installation
# ============================================================================
Write-Step "Verifying installation"

$checks = @(
    @{ Name = "Rust"; Command = "rustc --version" },
    @{ Name = "Cargo"; Command = "cargo --version" },
    @{ Name = "WASM target"; Command = "rustup target list --installed | Select-String wasm32" },
    @{ Name = "Trunk"; Command = "trunk --version" },
    @{ Name = "CMake"; Command = "cmake --version | Select-Object -First 1" },
    @{ Name = "NASM"; Command = "nasm --version" },
    @{ Name = "Graphviz"; Command = "dot -V 2>&1" },
    @{ Name = "pkg-config"; Command = "pkg-config --version" }
)

if (-not $SkipGStreamer) {
    $checks += @{ Name = "GStreamer"; Command = "gst-inspect-1.0 --version" }
}

$allPassed = $true
foreach ($check in $checks) {
    try {
        $result = Invoke-Expression $check.Command 2>$null
        if ($result) {
            Write-Host "[OK] $($check.Name): $result" -ForegroundColor Green
        } else {
            Write-Host "[WARN] $($check.Name): installed but version check failed" -ForegroundColor Yellow
        }
    } catch {
        Write-Host "[FAIL] $($check.Name): not found" -ForegroundColor Red
        $allPassed = $false
    }
}

# ============================================================================
# Done
# ============================================================================
Write-Host "`n"
if ($allPassed) {
    Write-Host "Development environment setup complete!" -ForegroundColor Green
} else {
    Write-Host "Setup completed with warnings. You may need to restart your terminal or install missing components manually." -ForegroundColor Yellow
}

Write-Host @"

Next steps:
  1. Restart your terminal to reload environment variables
  2. Run: cargo run

"@ -ForegroundColor Cyan
