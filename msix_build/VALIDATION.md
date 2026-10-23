# Windows Default Terminal (DefTerm) & MSIX Packaging Validation Guide

This document details how to build, package, install, and validate Alacritty as the Windows 10/11 Default Terminal Emulator.

---

## 1. Architecture Overview & Discovery

Windows 10 (21H2+) and Windows 11 implement **Default Terminal Delegation (DefTerm)** via the Windows Terminal `DelegationConfig` architecture:

### Why Single-Extension Registration Was Not Enough
In `microsoft/terminal`'s source code (`src/propslib/DelegationConfig.cpp` and `src/cascadia/TerminalSettingsModel/DefaultTerminal.cpp`), Windows Terminal and Windows Settings populate the "Default terminal application" dropdown by querying **two** catalogs:
1. `com.microsoft.windows.console.host` (The ConPTY broker, `OpenConsole`)
2. `com.microsoft.windows.terminal.host` (The terminal emulator UX, `Alacritty`)

The algorithm strictly pairs them:
```cpp
for (const auto& term : terminals) {
    for (const auto& con : consoles) {
        if (term.info.IsFromSamePackage(con.info)) {
            // Match found! Adds to dropdown
            packages.push_back(...);
            break;
        }
    }
}
```
If an MSIX package registers only `com.microsoft.windows.terminal.host`, `IsFromSamePackage` is never true, and the terminal is silently discarded from the dropdown.

### Complete Solution
1. **Paired AppExtensions**:
   - `com.microsoft.windows.console.host` -> ID `OpenConsole`, CLSID `{2EACA947-7F5F-4CFA-BA87-8F7FBEEFBE69}`
   - `com.microsoft.windows.terminal.host` -> ID `AlacrittyHost`, CLSID `{E8A0E476-80E2-4C1B-871B-6500F951B9D1}`
2. **Packaged Out-Of-Process COM Servers**:
   - `OpenConsole.exe`: The broker executable included directly in the package layout.
   - `alacritty.exe -Embedding`: Alacritty's COM server handling `ITerminalHandoff3`.
   - `OpenConsoleProxy.dll`: Proxy/stub for `system_handle` marshaling (`IConsoleHandoff` and `ITerminalHandoff`).
3. **Registry Virtualization Exclusion**:
   - Excludes `HKEY_CURRENT_USER\Console\%%Startup` from registry write virtualization so delegation changes propagate to the system conhost.
4. **Persistent PTY Reference**:
   - The `reference` handle passed in `EstablishPtyHandoff` is retained inside `Pty` (`HandoffProcess`) for the lifetime of the window so conhost does not prematurely terminate the session.

---

## 2. Automated Packaging & Installation

Run the PowerShell automation script from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File .\msix_build\package_msix.ps1
```

### Automation Actions:
1. Compiles the binary via `cargo build --release --bin alacritty`.
2. Assembles the staging layout (`msix_build\layout\`) containing:
   - `alacritty.exe`
   - `OpenConsole.exe` (sourced from Windows Terminal)
   - `OpenConsoleProxy.dll`
   - `AppxManifest.xml`
   - `Assets\` and `Public\`
3. Generates/updates a local self-signed certificate (`CN=AlacrittyDevCert`) and trusts it in `Cert:\CurrentUser\TrustedPeople`.
4. Packages into `msix_build\Alacritty.msix` using `MakeAppx.exe`.
5. Signs `Alacritty.msix` using `signtool.exe`.
6. Deploys/updates the package using `Add-AppxPackage`.

---

## 3. Verification & Validation Checklist

### Phase 1: Package Registration
Verify that Windows AppX registered the package:

```powershell
Get-AppxPackage -Name "Alacritty.Terminal"
```
**Expected**:
- `Name`: `Alacritty.Terminal`
- `Status`: `Ok`
- `InstallLocation`: `C:\Program Files\WindowsApps\Alacritty.Terminal_...`

### Phase 2: AppExtension Pairing Verification
Verify that both extensions are registered and paired from the same package:

```powershell
Add-Type -AssemblyName System.Runtime.WindowsRuntime
[Windows.ApplicationModel.AppExtensions.AppExtensionCatalog, Windows.ApplicationModel, ContentType = WindowsRuntime] | Out-Null
$asTask = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Length -eq 1 -and $_.IsGenericMethod })[0]
$taskMethod = $asTask.MakeGenericMethod([System.Collections.Generic.IReadOnlyList[Windows.ApplicationModel.AppExtensions.AppExtension]])

$terms = $taskMethod.Invoke($null, @([Windows.ApplicationModel.AppExtensions.AppExtensionCatalog]::Open("com.microsoft.windows.terminal.host").FindAllAsync())).Result
$cons = $taskMethod.Invoke($null, @([Windows.ApplicationModel.AppExtensions.AppExtensionCatalog]::Open("com.microsoft.windows.console.host").FindAllAsync())).Result

foreach ($t in $terms) {
    foreach ($c in $cons) {
        if ($t.Package.Id.FamilyName -eq $c.Package.Id.FamilyName) {
            Write-Host "Matched: $($t.Package.DisplayName) by $($t.Package.PublisherDisplayName)"
        }
    }
}
```
**Output**:
```
Matched: Windows Terminal by Microsoft Corporation
Matched: Alacritty by Alacritty Contributors
```

### Phase 3: Windows Settings & Windows Terminal Dropdown
1. Open Windows Terminal:
   - Go to **Settings -> Startup -> Default terminal application**.
2. Alternatively open Windows Settings:
   - **Windows 11**: `Settings -> System -> Advanced -> Terminal` (or `Privacy & security -> For developers -> Terminal` / `Параметры -> Терминал по умолчанию`).
3. Click the dropdown:
   - [x] **Alacritty** is present in the list alongside Windows Terminal and Windows Console Host.
   - [x] Selecting Alacritty sets `DelegationTerminal` to `{E8A0E476-80E2-4C1B-871B-6500F951B9D1}`.

### Phase 4: Console Application Handoff Smoke Test
1. Set Alacritty as default terminal in registry or via Settings:
   ```powershell
   reg add "HKCU\Console\%%Startup" /v "DelegationConsole" /t REG_SZ /d "{2EACA947-7F5F-4CFA-BA87-8F7FBEEFBE69}" /f
   reg add "HKCU\Console\%%Startup" /v "DelegationTerminal" /t REG_SZ /d "{E8A0E476-80E2-4C1B-871B-6500F951B9D1}" /f
   ```
2. Launch any console application (e.g. `cmd.exe`, `powershell.exe`, or double-click a `.bat` file):
   ```powershell
   cmd.exe /c start cmd.exe
   ```
3. Observe:
   - Windows automatically opens an Alacritty window attached to the handed-off pseudoconsole.
   - The interactive prompt renders correctly with GPU acceleration.
   - Closing `cmd.exe` (or typing `exit`) cleanly closes the Alacritty window.
