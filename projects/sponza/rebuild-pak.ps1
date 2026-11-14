# Sponza scenes.pak 一键重构脚本 (PowerShell)
#
# 用法:
#   .\rebuild-pak.ps1
#   或在仓库根目录执行:
#   .\projects\sponza\rebuild-pak.ps1

$ErrorActionPreference = "Stop"

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    if (Test-Path "$env:USERPROFILE\.cargo\bin\cargo.exe") {
        $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    }
}

$sponzaDir = $PSScriptRoot
$rootDir = (Resolve-Path (Join-Path $sponzaDir "..\..")).Path

Set-Location $sponzaDir

Write-Host "[rebuild-pak] Rebuilding Sponza scene package (assets/scenes.pak)..." -ForegroundColor Cyan
& cargo run -p prism-build-pipeline --manifest-path (Join-Path $rootDir "Cargo.toml") -- build-scenes @args
if ($LASTEXITCODE -ne 0) {
    Write-Error "[rebuild-pak] Failed to rebuild scenes.pak."
    exit $LASTEXITCODE
}

Write-Host "[rebuild-pak] Successfully updated assets/scenes.pak!" -ForegroundColor Green
