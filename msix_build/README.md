# Alacritty Windows Default Terminal (DefTerm) & MSIX Packaging

This directory contains the packaging manifest, assets, and automation scripts to package Alacritty as an MSIX application and register it as the **Windows 10/11 Default Terminal Emulator** (DefTerm).

---

## Features

- **Windows Default Terminal Integration**: Registers Alacritty in Windows Settings (`Settings -> System -> Advanced -> Terminal` / `Параметры -> Терминал по умолчанию`) and Windows Terminal Startup Settings.
- **Console Application Handoff**: Intercepts console application launches (`cmd.exe`, `powershell.exe`, `.bat` files, WSL console shortcuts) via the out-of-process COM server (`ITerminalHandoff3`) and ConPTY broker (`OpenConsole.exe`).
- **Zero-Dependency Automated Deployment**: A single PowerShell script (`package_msix.ps1`) compiles, stages, self-signs with a local certificate, and installs the package.

---

## Quick Start

### 1. Prerequisites
- **OS**: Windows 11 or Windows 10 (Build 19041+ / 2004+).
- **Developer Mode**: Enabled in `Settings -> System -> For developers -> Developer Mode`.
- **Rust**: Stable toolchain (`x86_64-pc-windows-msvc`).
- **Windows 10/11 SDK**: `MakeAppx.exe` and `signtool.exe` (installed with Visual Studio or the standalone Windows SDK).
- **Windows Terminal**: Installed on the machine (provides `OpenConsole.exe` and `OpenConsoleProxy.dll`).

### 2. Build & Deploy
Run the automation script from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File .\msix_build\package_msix.ps1
```

The script will:
1. Compile Alacritty in Release mode (`cargo build --release --bin alacritty`).
2. Collect dependencies (`OpenConsole.exe`, `OpenConsoleProxy.dll`, logos, manifest).
3. Generate a local self-signed code-signing certificate (`CN=AlacrittyDevCert`) and trust it in `Cert:\CurrentUser\TrustedPeople`.
4. Package the application into `Alacritty.msix`.
5. Sign the package with the certificate.
6. Install/update the package via `Add-AppxPackage`.

### 3. Uninstall
To uninstall the packaged application:

```powershell
powershell -ExecutionPolicy Bypass -File .\msix_build\package_msix.ps1 -Uninstall
```

---

## Directory Structure

```
msix_build/
├── AppxManifest.xml      # MSIX package manifest with DefTerm AppExtensions & ComServer
├── Assets/               # High-resolution Alacritty icons for Windows Start/Taskbar/Settings
│   ├── Square44x44Logo.png
│   ├── Square150x150Logo.png
│   └── StoreLogo.png
├── package_msix.ps1      # Automated build, package, sign, and install PowerShell script
├── README.md             # This guide
└── VALIDATION.md         # Detailed technical verification and architecture notes
```
