#!/usr/bin/env pwsh

<#
.SYNOPSIS
    Build meta-mystia-network for all target platforms and feature configurations.
.DESCRIPTION
    Builds both default and logging variants for:
      - x86_64-pc-windows-msvc    (native or via cargo-xwin)
      - aarch64-pc-windows-msvc   (native or via cargo-xwin)
      - universal2-apple-darwin   (via cargo-zigbuild)
      - x86_64-unknown-linux-gnu  (via cargo-zigbuild)
      - aarch64-unknown-linux-gnu (via cargo-zigbuild)
    On Windows hosts, native MSVC toolchain is used for Windows targets.
    On non-Windows hosts (macOS/Linux), cargo-xwin is used for Windows MSVC targets.

    Note: On non-Windows hosts, thunk-rs (VC-LTL5 + YY-Thunks) and winres are
    skipped due to lld-link symbol conflicts and lack of rc.exe respectively.
    Cross-compiled Windows DLLs require Windows 10+, while native Windows builds
    support Windows 7+ via YY-Thunks API polyfills.

    All artifacts are collected into the ./target/output directory with unified naming.
#>

$ErrorActionPreference = "Stop"

$LibName = "meta_mystia_network"
$OutputDir = Join-Path $PSScriptRoot "target" "output"

# Detect host OS and choose Windows build tool
# Both paths target MSVC; on non-Windows hosts cargo-xwin provides the SDK/CRT.
if ($IsWindows) {
  $WinBuildTool = "cargo"       # native MSVC toolchain
}
else {
  $WinBuildTool = "xwin"        # cargo-xwin cross-compilation
}
$WinX64 = "x86_64-pc-windows-msvc"
$WinArm = "aarch64-pc-windows-msvc"

# Target definitions
# BuildTool: "cargo" = native cargo build, "zigbuild" = cargo-zigbuild, "xwin" = cargo xwin build
$Targets = @(
  @{ Triple = $WinX64; Ext = "dll"; DebugExt = "pdb"; Prefix = ""; BuildTool = $WinBuildTool },
  @{ Triple = $WinArm; Ext = "dll"; DebugExt = "pdb"; Prefix = ""; BuildTool = $WinBuildTool },
  @{ Triple = "universal2-apple-darwin"; Ext = "dylib"; DebugExt = $null; Prefix = "lib"; BuildTool = "zigbuild" },
  @{ Triple = "x86_64-unknown-linux-gnu"; Ext = "so"; DebugExt = "dwp"; Prefix = "lib"; BuildTool = "zigbuild" },
  @{ Triple = "aarch64-unknown-linux-gnu"; Ext = "so"; DebugExt = "dwp"; Prefix = "lib"; BuildTool = "zigbuild" }
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

    switch ($target.BuildTool) {
      "zigbuild" {
        Write-Host "cargo zigbuild $($buildArgs -join ' ')" -ForegroundColor Yellow
        & cargo zigbuild @buildArgs
      }
      "xwin" {
        Write-Host "cargo xwin build $($buildArgs -join ' ')" -ForegroundColor Yellow
        & cargo xwin build @buildArgs
      }
      default {
        Write-Host "cargo build $($buildArgs -join ' ')" -ForegroundColor Yellow
        & cargo build @buildArgs
      }
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
