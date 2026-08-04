#Requires -Version 5.1
<#
.SYNOPSIS
    mcpls installer for Windows.

.DESCRIPTION
    Downloads the latest (or a pinned) mcpls release archive for the current
    architecture, verifies its SHA256 checksum, and installs the binary to
    a directory on PATH.

.PARAMETER Version
    Release tag to install, e.g. "v0.3.8". Defaults to the latest release.

.PARAMETER InstallDir
    Directory to install mcpls.exe into. Defaults to "$HOME\.local\bin".

.EXAMPLE
    irm https://raw.githubusercontent.com/bug-ops/mcpls/main/scripts/install.ps1 | iex
#>
[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$InstallDir = "$HOME\.local\bin"
)

$ErrorActionPreference = "Stop"
$Repo = "bug-ops/mcpls"
$BinName = "mcpls.exe"

function Get-Arch {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) {
        $arch = $env:PROCESSOR_ARCHITEW6432
    }
    switch -Regex ($arch) {
        "ARM64" { return "aarch64" }
        "AMD64" { return "x86_64" }
        default { throw "Unsupported architecture: $arch" }
    }
}

function Get-TargetTriple([string]$Arch) {
    return "$Arch-pc-windows-msvc"
}

function Main {
    $arch = Get-Arch
    $target = Get-TargetTriple $arch
    $archive = "mcpls-$target.zip"

    if ($Version -eq "latest") {
        $baseUrl = "https://github.com/$Repo/releases/latest/download"
        $versionLabel = "latest"
    }
    else {
        $baseUrl = "https://github.com/$Repo/releases/download/$Version"
        $versionLabel = $Version
    }

    Write-Host "Installing mcpls ($versionLabel) for $target..."

    $tmpDir = Join-Path $env:TEMP ([System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Path $tmpDir | Out-Null
    try {
        $archivePath = Join-Path $tmpDir $archive
        $checksumPath = "$archivePath.sha256"

        Write-Host "Downloading $archive..."
        Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile $archivePath -UseBasicParsing
        Invoke-WebRequest -Uri "$baseUrl/$archive.sha256" -OutFile $checksumPath -UseBasicParsing

        Write-Host "Verifying checksum..."
        $expected = (Get-Content $checksumPath -Raw).Trim().Split()[0].ToLowerInvariant()
        $actual = (Get-FileHash -Path $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($expected -ne $actual) {
            throw "Checksum mismatch for $archive`: expected $expected, got $actual"
        }

        Write-Host "Extracting..."
        Expand-Archive -Path $archivePath -DestinationPath $tmpDir -Force

        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        Copy-Item -Path (Join-Path $tmpDir $BinName) -Destination (Join-Path $InstallDir $BinName) -Force

        Write-Host "Installed mcpls to $(Join-Path $InstallDir $BinName)"

        $pathEntries = $env:Path -split ";"
        if ($pathEntries -notcontains $InstallDir.TrimEnd("\")) {
            Write-Host ""
            Write-Host "Warning: $InstallDir is not on your PATH."
            Write-Host "Add it for future sessions with:"
            Write-Host "  [Environment]::SetEnvironmentVariable('Path', `$env:Path + ';$InstallDir', 'User')"
            $env:Path = "$env:Path;$InstallDir"
        }

        Write-Host ""
        & (Join-Path $InstallDir $BinName) --version
    }
    finally {
        Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Main
