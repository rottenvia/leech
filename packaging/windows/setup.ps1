<#
.SYNOPSIS
Installs or uninstalls Leech for the current Windows user without elevation.
Preserves write permissions for seamless in-app autoupdates.
#>
param(
    [switch]$Uninstall,
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'
$appName = 'Leech'
$publisher = 'Leech contributors'
$website = 'https://github.com/rottenvia/leech'
$installDir = Join-Path $env:LOCALAPPDATA "Programs\$appName"
$uninstallKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$appName"
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) "$appName.lnk"

# Check if Leech is currently running
$running = Get-Process leech -ErrorAction SilentlyContinue
if ($running) {
    if ($Quiet) {
        $running | Stop-Process -Force
    } else {
        throw "Leech is currently running. Please close Leech before running setup."
    }
}

if ($Uninstall) {
    if (Test-Path -LiteralPath $shortcut) {
        Remove-Item -LiteralPath $shortcut -Force
    }
    if (Test-Path -LiteralPath $uninstallKey) {
        Remove-Item -LiteralPath $uninstallKey -Recurse -Force
    }
    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    if ((Get-ItemProperty -LiteralPath $runKey -Name $appName -ErrorAction SilentlyContinue)) {
        Remove-ItemProperty -LiteralPath $runKey -Name $appName -Force
    }
    if (Test-Path -LiteralPath $installDir) {
        Remove-Item -LiteralPath $installDir -Recurse -Force
    }
    if (!$Quiet) {
        Write-Host "Leech was successfully uninstalled."
    }
    return
}

# Install
$distDir = $PSScriptRoot
$executable = Join-Path $distDir 'leech.exe'
if (!(Test-Path -LiteralPath $executable)) {
    $candidate = Join-Path (Join-Path $distDir '..\..\dist') 'leech.exe'
    if (Test-Path -LiteralPath $candidate) {
        $distDir = (Resolve-Path (Join-Path $distDir '..\..\dist')).Path
        $executable = $candidate
    } else {
        throw "leech.exe not found in $distDir. Run this script from the release package directory or build the project first."
    }
}

[IO.Directory]::CreateDirectory($installDir) | Out-Null

# Copy payload
Copy-Item -Path "$distDir\*" -Destination $installDir -Recurse -Force
if ($PSCommandPath -and (Test-Path -LiteralPath $PSCommandPath)) {
    Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $installDir 'setup.ps1') -Force
}

# Create Start Menu shortcut with AUMID
$notificationScript = Join-Path $installDir 'install-notifications.ps1'
if (Test-Path -LiteralPath $notificationScript) {
    & $notificationScript -Force
}

# Determine version
$version = '0.1.0'
try {
    $versionInfo = (Get-Item -LiteralPath (Join-Path $installDir 'leech.exe')).VersionInfo.ProductVersion
    if ($versionInfo) { $version = $versionInfo }
} catch {}

# Register in Windows Add/Remove Programs
if (!(Test-Path -LiteralPath $uninstallKey)) {
    New-Item -Path $uninstallKey -Force | Out-Null
}

$uninstallCmd = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$installDir\setup.ps1`" -Uninstall"
Set-ItemProperty -LiteralPath $uninstallKey -Name 'DisplayName' -Value $appName
Set-ItemProperty -LiteralPath $uninstallKey -Name 'DisplayVersion' -Value $version
Set-ItemProperty -LiteralPath $uninstallKey -Name 'Publisher' -Value $publisher
Set-ItemProperty -LiteralPath $uninstallKey -Name 'DisplayIcon' -Value "$installDir\leech.exe,0"
Set-ItemProperty -LiteralPath $uninstallKey -Name 'InstallLocation' -Value $installDir
Set-ItemProperty -LiteralPath $uninstallKey -Name 'UninstallString' -Value $uninstallCmd
Set-ItemProperty -LiteralPath $uninstallKey -Name 'QuietUninstallString' -Value "$uninstallCmd -Quiet"
Set-ItemProperty -LiteralPath $uninstallKey -Name 'URLInfoAbout' -Value $website
Set-ItemProperty -LiteralPath $uninstallKey -Name 'NoModify' -Value 1 -Type DWord
Set-ItemProperty -LiteralPath $uninstallKey -Name 'NoRepair' -Value 1 -Type DWord

if (!$Quiet) {
    Write-Host "Leech $version installed successfully to $installDir"
}
