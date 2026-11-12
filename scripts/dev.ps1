# PrismaRev 开发启动器（RustRover / 终端通用）
#
#   .\scripts\dev.ps1                # 编译着色器(有 slangc 时) -> 重打场景包 -> 运行游戏
#   .\scripts\dev.ps1 -Target editor # 启动 Tauri 编辑器 (pnpm -C editor tauri dev)
#
# 常用开关：
#   -SkipShaders   不重编 Slang（只改了 .rs 时省几秒）
#   -SkipScenes    不重打 assets/scenes.pak（没改场景/预制体时可用）
#   -Release       游戏用 --release 构建
#
# 说明：根 workspace 没有可运行的 bin，游戏二进制在 projects/prisma（独立 workspace），
# 且它按 **当前工作目录** 找 `assets/scenes.pak` / `assets/settings.toml`，
# 所以本脚本固定在仓库根目录启动。
param(
    [ValidateSet("game", "editor")]
    [string]$Target = "game",
    [switch]$SkipShaders,
    [switch]$SkipScenes,
    [switch]$Release
)

$ErrorActionPreference = "Stop"

# 脚本在 scripts/ 下，切到仓库根目录（游戏按 cwd 解析 assets/）。
Set-Location (Split-Path -Parent $PSScriptRoot)

function Resolve-VulkanSdk {
    $candidates = @(
        $env:VULKAN_SDK,
        "D:\VulkanSDK\1.4.350.0",
        "C:\VulkanSDK\1.4.350.0"
    ) | Where-Object { $_ -and (Test-Path (Join-Path $_ "Bin\glslc.exe")) }
    return $candidates | Select-Object -First 1
}

$sdk = Resolve-VulkanSdk
if ($sdk) {
    $env:VK_SDK = $sdk
    $env:VULKAN_SDK = $sdk
    $env:VK_LAYER_PATH = Join-Path $sdk "Bin"
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$(Join-Path $sdk 'Bin');$env:PATH"
} else {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
}

if (-not $env:RUST_LOG) { $env:RUST_LOG = "warn,tracy_client=off" }

if ($Target -eq "editor") {
    Write-Host "启动编辑器 (Tauri)..." -ForegroundColor Cyan
    pnpm -C editor tauri dev
    exit $LASTEXITCODE
}

if (-not $SkipShaders) {
    if (Get-Command slangc -ErrorAction SilentlyContinue) {
        Write-Host "编译 Slang 着色器..." -ForegroundColor Cyan
        & bash assets/shaders/compile.sh
        if ($LASTEXITCODE -ne 0) { Write-Error "着色器编译失败"; exit 1 }
    } else {
        Write-Warning "未找到 slangc，跳过着色器编译（沿用上次产物/CI 的 .spv）。"
    }
}

if (-not $SkipScenes) {
    Write-Host "重打场景包 (assets/scenes.pak)..." -ForegroundColor Cyan
    cargo run -q -p prism-build-pipeline -- build-scenes
    if ($LASTEXITCODE -ne 0) { Write-Error "场景包构建失败"; exit 1 }
}

$cargoArgs = @("run", "--manifest-path", "projects/prisma/Cargo.toml", "--bin", "prisma")
if ($Release) { $cargoArgs += "--release" }

Write-Host "运行游戏: cargo $($cargoArgs -join ' ')" -ForegroundColor Cyan
cargo @cargoArgs
exit $LASTEXITCODE
