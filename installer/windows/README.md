# Strom Windows MSI Installer

This directory contains the configuration and scripts for building a Windows MSI installer that bundles Strom with all required dependencies.

## What's Included in the Installer

The MSI installer bundles:

| Component | Description | Size (approx) |
|-----------|-------------|---------------|
| **strom.exe** | Main Strom application | ~15 MB |
| **strom-mcp-server.exe** | MCP server for LLM integration | ~5 MB |
| **GStreamer Runtime** | Multimedia framework (all plugins) | ~300 MB |
| **Graphviz** | Graph visualization for debug graphs | ~30 MB |

Total installer size: **~350 MB**

## Installation Features

- **One-click install** - No manual dependency setup
- **Start Menu shortcuts** - Launch Strom or Strom (Web UI mode)
- **Environment variables** - Automatically configured PATH and GST_PLUGIN_PATH
- **Feature selection** - Option to skip Graphviz if not needed
- **Clean uninstall** - Removes all components and environment variables

## Building Locally

### Prerequisites

1. **Windows 10/11** (64-bit)
2. **.NET SDK 6.0+** (for WiX)
3. **WiX Toolset v5**:
   ```powershell
   dotnet tool install --global wix
   wix extension add WixToolset.UI.wixext -g
   ```

### Build Steps

1. **Download or build Strom binaries**:
   ```powershell
   # Or use pre-built binaries from a release
   cargo build --release --package strom
   cargo build --release --package strom-mcp-server
   ```

2. **Run the build script**:
   ```powershell
   cd installer/windows
   .\Build-Installer.ps1 -Version "0.3.10" `
       -StromExe "..\..\target\release\strom.exe" `
       -StromMcpServerExe "..\..\target\release\strom-mcp-server.exe"
   ```

3. **Find the MSI** in `installer/windows/output/`

### Build Options

```powershell
# Full GStreamer (all plugins, larger bundle)
.\Build-Installer.ps1 -Version "0.3.10" -StromExe "..." -StromMcpServerExe "..." -FullGStreamer

# Skip downloading dependencies (use existing bundles)
.\Build-Installer.ps1 -Version "0.3.10" -StromExe "..." -StromMcpServerExe "..." -SkipDependencies
```

## Automated Builds

The MSI is automatically built by GitHub Actions when a new release is created:

1. The `release.yml` workflow builds the Windows binaries
2. The `release-msi.yml` workflow:
   - Downloads the release binaries
   - Bundles GStreamer and Graphviz
   - Builds and uploads the MSI to the GitHub release

## File Structure

```
installer/windows/
├── Product.wxs              # WiX installer definition
├── License.rtf              # License shown during installation
├── Build-Installer.ps1      # Main build orchestration script
├── Prepare-GStreamer.ps1    # GStreamer bundle preparation
├── Prepare-Graphviz.ps1     # Graphviz bundle preparation
└── README.md                # This file
```

## Installation Paths

When installed, Strom uses the following directory structure:

```
C:\Program Files\Strom\
├── bin\                     # Strom executables
│   ├── strom.exe
│   └── strom-mcp-server.exe
├── gstreamer\
│   ├── bin\                 # GStreamer DLLs and tools
│   └── lib\gstreamer-1.0\   # GStreamer plugins
└── graphviz\
    └── bin\                 # Graphviz executables
```

## Environment Variables

The installer sets the following system environment variables:

- **PATH**: Adds `bin\`, `gstreamer\bin\`, and `graphviz\bin\`
- **GST_PLUGIN_PATH**: Points to `gstreamer\lib\gstreamer-1.0\`

## Troubleshooting

### MSI build fails with "file not found"
Ensure all source paths are absolute paths and the files exist.

### GStreamer plugins not loading
Run `gst-inspect-1.0 --version` to verify GStreamer is working. Check that `GST_PLUGIN_PATH` is set correctly.

### Graphviz dot command not found
Verify Graphviz bin directory is in PATH. The `dot.exe` command should be accessible from any terminal.
