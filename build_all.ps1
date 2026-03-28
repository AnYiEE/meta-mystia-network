#!/usr/bin/env pwsh

<#
.SYNOPSIS
    Build meta-mystia-network for all target platforms and feature configurations.
.DESCRIPTION
    Builds both default and logging variants for:
      - x86_64-pc-windows-msvc / x86_64-pc-windows-gnu
      - aarch64-pc-windows-msvc / aarch64-pc-windows-gnullvm
      - universal2-apple-darwin   (via cargo-zigbuild)
      - x86_64-unknown-linux-gnu  (via cargo-zigbuild)
      - aarch64-unknown-linux-gnu (via cargo-zigbuild)
    On Windows hosts, MSVC toolchain is used for Windows targets.
    On non-Windows hosts (macOS/Linux), gnu/gnullvm toolchain is used for Windows targets.
    All artifacts are collected into the ./target/output directory with unified naming.
#>

$ErrorActionPreference = "Stop"

$LibName = "meta_mystia_network"
$OutputDir = Join-Path $PSScriptRoot "target" "output"

# Detect host OS and choose Windows target ABI and build tool
if ($IsWindows) {
  $WinX64 = "x86_64-pc-windows-msvc"
  $WinArm = "aarch64-pc-windows-msvc"
  $WinDebugExt = "pdb"
  $WinZigbuild = $false
}
else {
  # aarch64-pc-windows-gnu is not available on non-Windows hosts,
  # so x86_64 uses gnu and aarch64 uses gnullvm; both via cargo-zigbuild
  $WinX64 = "x86_64-pc-windows-gnu"
  $WinArm = "aarch64-pc-windows-gnullvm"
  $WinDebugExt = $null
  $WinZigbuild = $true
}

# Target definitions
$Targets = @(
  @{ Triple = $WinX64; Ext = "dll"; DebugExt = $WinDebugExt; Prefix = ""; Zigbuild = $WinZigbuild },
  @{ Triple = $WinArm; Ext = "dll"; DebugExt = $WinDebugExt; Prefix = ""; Zigbuild = $WinZigbuild },
  @{ Triple = "universal2-apple-darwin"; Ext = "dylib"; DebugExt = $null; Prefix = "lib"; Zigbuild = $true },
  @{ Triple = "x86_64-unknown-linux-gnu"; Ext = "so"; DebugExt = "dwp"; Prefix = "lib"; Zigbuild = $true },
  @{ Triple = "aarch64-unknown-linux-gnu"; Ext = "so"; DebugExt = "dwp"; Prefix = "lib"; Zigbuild = $true }
)

# Feature variants
$Variants = @(
  @{ Suffix = ""; Features = @() },
  @{ Suffix = "-logging"; Features = @("--features", "logging") }
)

# Clean and create output directory
if (Test-Path $OutputDir) {
  Remove-Item -Recurse -Force $OutputDir
}
New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null

foreach ($target in $Targets) {
  foreach ($variant in $Variants) {
    $triple = $target.Triple
    $label = "$triple$($variant.Suffix)"

    Write-Host "`n========================================" -ForegroundColor Cyan
    Write-Host " Building: $label" -ForegroundColor Cyan
    Write-Host "========================================" -ForegroundColor Cyan

    # Build arguments
    $buildArgs = @("--release", "--target", $triple) + $variant.Features

    if ($target.Zigbuild) {
      Write-Host "cargo zigbuild $($buildArgs -join ' ')" -ForegroundColor Yellow
      & cargo zigbuild @buildArgs
    }
    else {
      Write-Host "cargo build $($buildArgs -join ' ')" -ForegroundColor Yellow
      & cargo build @buildArgs
    }

    if ($LASTEXITCODE -ne 0) {
      Write-Error "Build failed: $label"
      exit 1
    }

    # Source paths
    $releaseDir = Join-Path $PSScriptRoot "target" $triple "release"
    $srcLib = Join-Path $releaseDir "$($target.Prefix)$LibName.$($target.Ext)"

    # Output naming: meta_mystia_network[-logging]-<target>.<ext>
    $outLib = Join-Path $OutputDir "$LibName$($variant.Suffix)-$triple.$($target.Ext)"

    Write-Host "  -> $outLib"
    Copy-Item -Path $srcLib -Destination $outLib

    # Copy debug info (.pdb / .dwp) if it exists
    if ($target.DebugExt) {
      $srcDebug = Join-Path $releaseDir "$($target.Prefix)$LibName.$($target.DebugExt)"
      if (Test-Path $srcDebug) {
        $outDebug = Join-Path $OutputDir "$LibName$($variant.Suffix)-$triple.$($target.DebugExt)"
        Write-Host "  -> $outDebug"
        Copy-Item -Path $srcDebug -Destination $outDebug
      }
    }

    # Copy .dSYM directory (macOS) if it exists
    $dsymDir = Join-Path $releaseDir "$($target.Prefix)$LibName.$($target.Ext).dSYM"
    if (Test-Path $dsymDir) {
      $outDsym = Join-Path $OutputDir "$LibName$($variant.Suffix)-$triple.$($target.Ext).dSYM"
      Write-Host "  -> $outDsym"
      Copy-Item -Path $dsymDir -Destination $outDsym -Recurse
    }
  }
}

Write-Host "`n========================================" -ForegroundColor Green
Write-Host " All builds completed successfully!" -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Green
Write-Host "`nOutput files:"
Get-ChildItem $OutputDir -Recurse -File | ForEach-Object {
  $relative = $_.FullName.Substring($OutputDir.Length + 1)
  $sizeKB = [math]::Round($_.Length / 1KB, 1)
  Write-Host "  $relative  ($sizeKB KB)"
}
