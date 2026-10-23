#Requires -Version 5.1
<#
.SYNOPSIS
    Build, package, sign, and install Alacritty as the Windows Default Terminal.

.DESCRIPTION
    Automates the DefTerm MSIX workflow for Alacritty:
    1. Compiles the binary via 'cargo build --release --bin alacritty'.
    2. Assembles the staging layout (alacritty.exe, OpenConsole.exe, OpenConsoleProxy.dll, AppxManifest.xml, Assets/).
    3. Generates a local self-signed certificate (CN=AlacrittyDevCert) and exports dev_cert.pfx.
    4. Packages the application using MakeAppx.exe pack /d ./layout /p ./Alacritty.msix /nv.
    5. Signs the package with signtool.exe sign /fd SHA256 /a /f dev_cert.pfx /p "password" ./Alacritty.msix.
    6. Deploys the package locally via Add-AppxPackage -Path ./Alacritty.msix.
#>

[CmdletBinding()]
param(
    [ValidateSet('Release', 'Debug')]
    [string]$Configuration = 'Release',

    [string]$Publisher = 'CN=AlacrittyDevCert',

    [string]$CertPassword = 'password',

    [string]$ExecutablePath,

    [string]$MakeAppxPath,

    [string]$SignToolPath,

    [switch]$SkipBuild,

    [switch]$SkipSign,

    [switch]$SkipInstall,

    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

$ScriptDir = $PSScriptRoot
$RepoRoot = Split-Path -Parent $ScriptDir
Set-Location $RepoRoot

$PackageName = 'Alacritty.Terminal'

# Handle uninstallation
if ($Uninstall) {
    Write-Host "==> Uninstalling $PackageName..." -ForegroundColor Cyan
    $existing = Get-AppxPackage -Name $PackageName -ErrorAction SilentlyContinue
    if ($existing) {
        Remove-AppxPackage -Package $existing.PackageFullName
        Write-Host "Successfully uninstalled $($existing.PackageFullName)" -ForegroundColor Green
    } else {
        Write-Host "Package $PackageName is not currently installed." -ForegroundColor Yellow
    }
    return
}

# Ensure Cargo is discoverable
if (-not (Get-Command 'cargo' -ErrorAction SilentlyContinue)) {
    $fallbackPaths = @(
        "$env:USERPROFILE\.cargo\bin",
        "C:\Users\hrdcoreee\AppData\Local\Temp\opencode\rust-check\cargo\bin"
    )
    foreach ($p in $fallbackPaths) {
        if (Test-Path (Join-Path $p 'cargo.exe')) {
            $env:PATH = "$p;$env:PATH"
            $env:CARGO_HOME = Split-Path -Parent $p
            $env:RUSTUP_HOME = Join-Path (Split-Path -Parent (Split-Path -Parent $p)) 'rustup'
            break
        }
    }
}

# 1. Locate Windows SDK Tools (MakeAppx.exe & SignTool.exe)
function Find-SdkTool([string]$ToolName, [string]$ExplicitPath) {
    if ($ExplicitPath -and (Test-Path $ExplicitPath)) {
        return (Resolve-Path $ExplicitPath).Path
    }
    $cmd = Get-Command $ToolName -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }
    $searchRoots = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "${env:ProgramFiles}\Windows Kits\10\bin"
    )
    foreach ($root in $searchRoots) {
        if (Test-Path $root) {
            $found = Get-ChildItem -Path $root -Filter $ToolName -Recurse -ErrorAction SilentlyContinue |
                Where-Object { $_.FullName -match 'x64' } |
                Sort-Object FullName -Descending |
                Select-Object -First 1
            if ($found) {
                return $found.FullName
            }
        }
    }
    throw "Could not find $ToolName. Please install the Windows 10/11 SDK or supply the path via parameter."
}

$MakeAppxExe = Find-SdkTool 'makeappx.exe' $MakeAppxPath
$SignToolExe = Find-SdkTool 'signtool.exe' $SignToolPath
Write-Host "==> MakeAppx: $MakeAppxExe" -ForegroundColor DarkGray
Write-Host "==> SignTool: $SignToolExe" -ForegroundColor DarkGray

# 2. Step 1: Compile Alacritty via Cargo
$TargetExe = $null
if ($ExecutablePath) {
    $TargetExe = (Resolve-Path $ExecutablePath).Path
} else {
    $profileDir = if ($Configuration -eq 'Release') { 'release' } else { 'debug' }
    $TargetExe = Join-Path $RepoRoot "target\$profileDir\alacritty.exe"
}

if (-not $SkipBuild) {
    Write-Host "==> Compiling alacritty ($Configuration)..." -ForegroundColor Cyan
    $cargoArgs = @('build', '--bin', 'alacritty')
    if ($Configuration -eq 'Release') {
        $cargoArgs += '--release'
    }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo build failed with exit code $LASTEXITCODE"
    }
}

if (-not (Test-Path $TargetExe)) {
    throw "Target executable not found at: $TargetExe"
}

# 3. Step 2: Assemble Staging Layout
$LayoutDir = Join-Path $ScriptDir 'layout'
Write-Host "==> Assembling staging layout in $LayoutDir..." -ForegroundColor Cyan

if (Test-Path $LayoutDir) {
    Remove-Item -Path $LayoutDir -Recurse -Force
}
New-Item -ItemType Directory -Path $LayoutDir -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $LayoutDir 'Public') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $LayoutDir 'Assets') -Force | Out-Null

Copy-Item -Path $TargetExe -Destination (Join-Path $LayoutDir 'alacritty.exe') -Force
Copy-Item -Path (Join-Path $ScriptDir 'Assets\*') -Destination (Join-Path $LayoutDir 'Assets') -Recurse -Force

# Locate OpenConsole.exe and OpenConsoleProxy.dll (from Windows Terminal or fallback)
$openConsoleExe = $null
$proxyStub = $null
$wtPackages = Get-AppxPackage -Name '*WindowsTerminal*' -ErrorAction SilentlyContinue
foreach ($wt in $wtPackages) {
    $candExe = Join-Path $wt.InstallLocation 'OpenConsole.exe'
    if (Test-Path $candExe) {
        $openConsoleExe = $candExe
    }
    $candProxy = Join-Path $wt.InstallLocation 'OpenConsoleProxy.dll'
    if (Test-Path $candProxy) {
        $proxyStub = $candProxy
    }
    if ($openConsoleExe -and $proxyStub) {
        break
    }
}

if (-not $openConsoleExe) {
    $cand = "C:\Program Files\WindowsApps\Microsoft.WindowsTerminal*\OpenConsole.exe"
    $found = Get-ChildItem -Path $cand -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($found) { $openConsoleExe = $found.FullName }
}

if ($openConsoleExe -and (Test-Path $openConsoleExe)) {
    Write-Host "==> Including OpenConsole.exe from $openConsoleExe" -ForegroundColor DarkGray
    Copy-Item -Path $openConsoleExe -Destination (Join-Path $LayoutDir 'OpenConsole.exe') -Force
} else {
    throw "OpenConsole.exe was not found. OpenConsole is required as the console host broker for DefTerm integration."
}

if (-not $proxyStub) {
    $cand = "C:\Program Files\WindowsApps\Microsoft.WindowsTerminal*\OpenConsoleProxy.dll"
    $found = Get-ChildItem -Path $cand -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($found) { $proxyStub = $found.FullName }
}

if ($proxyStub -and (Test-Path $proxyStub)) {
    Write-Host "==> Including OpenConsoleProxy.dll from $proxyStub" -ForegroundColor DarkGray
    Copy-Item -Path $proxyStub -Destination (Join-Path $LayoutDir 'OpenConsoleProxy.dll') -Force
} else {
    Write-Warning "OpenConsoleProxy.dll was not found on system. COM out-of-process marshaling may depend on existing registrations."
}

# Copy manifest
$ManifestSource = Join-Path $ScriptDir 'AppxManifest.xml'
Copy-Item -Path $ManifestSource -Destination (Join-Path $LayoutDir 'AppxManifest.xml') -Force

# 4. Step 3: Self-Signed Certificate Setup
$PfxPath = Join-Path $ScriptDir 'dev_cert.pfx'
$CerPath = Join-Path $ScriptDir 'dev_cert.cer'

if (-not $SkipSign) {
    Write-Host "==> Setting up Code Signing Certificate ($Publisher)..." -ForegroundColor Cyan
    
    # Check if certificate exists in CurrentUser\My
    $cert = Get-ChildItem -Path Cert:\CurrentUser\My -CodeSigningCert -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -eq $Publisher } |
        Select-Object -First 1

    if (-not $cert) {
        Write-Host "==> Creating new self-signed certificate: $Publisher" -ForegroundColor Yellow
        $cert = New-SelfSignedCertificate -Type CodeSigningCert `
            -Subject $Publisher `
            -CertStoreLocation Cert:\CurrentUser\My `
            -NotAfter (Get-Date).AddYears(5)
    }

    # Export PFX
    $secPass = ConvertTo-SecureString -String $CertPassword -AsPlainText -Force
    Export-PfxCertificate -Cert $cert -FilePath $PfxPath -Password $secPass -Force | Out-Null
    Export-Certificate -Cert $cert -FilePath $CerPath -Force | Out-Null

    # Trust in CurrentUser\Root so Add-AppxPackage succeeds without error
    $trusted = Get-ChildItem -Path Cert:\CurrentUser\TrustedPeople -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $cert.Thumbprint }
    if (-not $trusted) {
        Write-Host "==> Adding certificate to Cert:\CurrentUser\TrustedPeople for package trust..." -ForegroundColor Yellow
        Import-Certificate -FilePath $CerPath -CertStoreLocation Cert:\CurrentUser\TrustedPeople | Out-Null
    }
}

# 5. Step 4: Package MSIX
$MsixPath = Join-Path $ScriptDir 'Alacritty.msix'
if (Test-Path $MsixPath) {
    Remove-Item -Path $MsixPath -Force
}

Write-Host "==> Packaging MSIX: $MsixPath..." -ForegroundColor Cyan
& $MakeAppxExe pack /d $LayoutDir /p $MsixPath /nv /o
if ($LASTEXITCODE -ne 0) {
    throw "MakeAppx failed with exit code $LASTEXITCODE"
}

# 6. Step 5: Sign Package
if (-not $SkipSign) {
    Write-Host "==> Signing package with $PfxPath..." -ForegroundColor Cyan
    & $SignToolExe sign /fd SHA256 /a /f $PfxPath /p $CertPassword $MsixPath
    if ($LASTEXITCODE -ne 0) {
        throw "SignTool failed with exit code $LASTEXITCODE"
    }
}

# 7. Step 6: Deploy Locally
if (-not $SkipInstall) {
    Write-Host "==> Deploying package to current user..." -ForegroundColor Cyan
    
    # Remove existing package if installed with same name to ensure clean update
    $existing = Get-AppxPackage -Name $PackageName -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "==> Removing existing installation: $($existing.PackageFullName)" -ForegroundColor DarkGray
        Remove-AppxPackage -Package $existing.PackageFullName
    }
    
    Add-AppxPackage -Path $MsixPath -ForceApplicationShutdown
    Write-Host "==> Package successfully deployed!" -ForegroundColor Green
    
    $installed = Get-AppxPackage -Name $PackageName
    Write-Host "    Name:         $($installed.Name)"
    Write-Host "    Package:      $($installed.PackageFullName)"
    Write-Host "    InstallPath:  $($installed.InstallLocation)"
    Write-Host "    Status:       $($installed.Status)"
}

Write-Host ""
Write-Host "==========================================================" -ForegroundColor Green
Write-Host " MSIX DefTerm package complete: $MsixPath" -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green
