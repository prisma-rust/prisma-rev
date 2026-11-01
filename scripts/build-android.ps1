#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Build PrismaRev for Android arm64-v8a.

.DESCRIPTION
    `-Project prisma`（默认）：launcher + 游戏的完整 APK——编译 `projects/prisma`
    的 cdylib（`libprisma.so`）via cargo-ndk，再用 Tauri CLI 组装
    `launcher/src-tauri/gen/android`（启动器 + GameActivity）。

    其他工程：委托给 `scripts/build-android-game.sh <project> [debug|release]`
    打 **game-only** APK（不含 launcher / WebView），宿主由
    `scripts/new-android-game.sh <project>` 生成（#598）。launcher APK 的 manifest
    固定 `android.app.lib_name = prisma`，所以不能直接换个工程名就打进去。

.PARAMETER Project
    `projects/` 下的工程目录名，默认 `prisma`。

.PARAMETER Mode
    只对委托路径生效：`debug`（默认）或 `release`。
#>
param(
    [string]$Project = "prisma",
    [ValidateSet("debug", "release")]
    [string]$Mode = "debug"
)
$ErrorActionPreference = "Stop"

$ProjectRoot = Split-Path -Parent $PSScriptRoot

if ($Project -ne "prisma") {
    $delegate = Join-Path $PSScriptRoot "build-android-game.sh"
    if (-not (Test-Path $delegate)) {
        Write-Error "找不到 $delegate（game-only APK 由它负责）"
        exit 1
    }
    Write-Host "Project '$Project' → game-only APK via scripts/build-android-game.sh $Project $Mode"
    bash $delegate $Project $Mode
    exit $LASTEXITCODE
}

$AndroidDir  = Join-Path $ProjectRoot "launcher\src-tauri\gen\android"
$JniLibsDir  = Join-Path $AndroidDir "app\src\main\jniLibs"
$GameManifest = Join-Path $ProjectRoot "projects\$Project\Cargo.toml"

# ---- Prerequisites ---------------------------------------------------------

# 1. Rust Android target
$target = "aarch64-linux-android"
$installed = rustup target list --installed 2>$null
if ($installed -notcontains $target) {
    Write-Host "Adding Rust target $target..."
    rustup target add $target
}

# 2. cargo-ndk
cargo ndk --version 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Host "Installing cargo-ndk..."
    cargo install cargo-ndk
}

# 3. NDK: honor ANDROID_NDK_HOME, else fall back to the newest NDK under
#    ANDROID_HOME\ndk. The env var is frequently stale on dev machines, so
#    validate it (must contain toolchains\llvm) before trusting it.
function Get-NdkRoot {
    $candidates = @()
    if ($env:ANDROID_NDK_HOME) { $candidates += $env:ANDROID_NDK_HOME }
    if ($env:ANDROID_HOME) {
        $ndkBase = Join-Path $env:ANDROID_HOME "ndk"
        if (Test-Path $ndkBase) {
            $candidates += Get-ChildItem $ndkBase -Directory | Sort-Object Name -Descending | ForEach-Object { $_.FullName }
        }
    }
    foreach ($c in $candidates) {
        if (Test-Path (Join-Path $c "toolchains\llvm")) {
            return $c
        }
    }
    return $null
}

$ndkRoot = Get-NdkRoot
if (-not $ndkRoot) {
    Write-Warning "No usable NDK found (checked ANDROID_NDK_HOME and ANDROID_HOME\ndk\*)."
    Write-Warning "Install the NDK via Android Studio SDK Manager and set ANDROID_NDK_HOME."
    exit 1
}
# cargo-ndk reads ANDROID_NDK_HOME itself, so override it with the validated
# path — otherwise a stale value wins and cargo-ndk fails on version detection.
$env:ANDROID_NDK_HOME = $ndkRoot
Write-Host "Using NDK: $ndkRoot"

# 4. cargo-ndk platform (API level) = the manifest's minSdk. The .so must not
#    link symbols newer than minSdk, so do NOT just take the newest sysroot.
#    Floor is 26 (libaaudio appears there; cargo-ndk's own default of 21 fails
#    to link with "-laaudio not found"). If the NDK is too old for minSdk, fall
#    back to its newest available level.
$minSdk = 31
$gradleFile = Join-Path $AndroidDir "app\build.gradle.kts"
if (Test-Path $gradleFile) {
    $m = Select-String -Path $gradleFile -Pattern 'minSdk\s*=\s*(\d+)' | Select-Object -First 1
    if ($m) { $minSdk = [int]$m.Matches[0].Groups[1].Value }
}
$apiLevel = [Math]::Max($minSdk, 26)
$sysrootLib = Join-Path $ndkRoot "toolchains\llvm\prebuilt\windows-x86_64\sysroot\usr\lib\aarch64-linux-android"
if (Test-Path $sysrootLib) {
    $available = Get-ChildItem $sysrootLib -Directory |
        Where-Object { $_.Name -match '^\d+$' } |
        ForEach-Object { [int]$_.Name } |
        Sort-Object -Descending
    if ($available -and ($available -notcontains $apiLevel)) {
        $apiLevel = $available | Select-Object -First 1
        Write-Warning "NDK sysroot has no API $minSdk; using $apiLevel instead."
    }
}
Write-Host "cargo-ndk platform (API level): $apiLevel  (manifest minSdk = $minSdk)"

# ---- Shaders ---------------------------------------------------------------

# .spv files are include_bytes!-ed at compile time and gitignored; make sure
# they exist before building (Android hosts have no slangc — compile on a
# desktop/CI host first, or fetch the CI `spirv` artifact).
if (Get-Command slangc -ErrorAction SilentlyContinue) {
    Write-Host "Compiling shaders..."
    bash (Join-Path $ProjectRoot "assets\shaders\compile.sh")
} else {
    $spv = Get-ChildItem (Join-Path $ProjectRoot "assets\shaders\*.spv") -ErrorAction SilentlyContinue
    if (-not $spv) {
        Write-Warning "No .spv files found and slangc is unavailable."
        Write-Warning "Compile shaders on a desktop host first, or download the CI spirv artifact."
        exit 1
    }
    Write-Host "Using prebuilt .spv ($($spv.Count) files)."
}

# ---- Scene package + APK assets -------------------------------------------

# The game runtime reads `assets/scenes.pak` from the filesystem. On Android
# `run_on_android` copies the APK `game/` asset directory to `<filesDir>/assets`,
# so the game payload must be shipped inside the APK. Keep it under a
# subdirectory: the Tauri launcher also owns the asset root for its frontend.
$AndroidAssets = Join-Path $AndroidDir "app\src\main\assets"
$AndroidGameAssets = Join-Path $AndroidAssets "game"
New-Item -ItemType Directory -Force -Path $AndroidGameAssets | Out-Null

Write-Host "Building scene package (assets/scenes.pak)..."
Push-Location $ProjectRoot
try {
    cargo run -q -p prism-build-pipeline -- build-scenes
    if ($LASTEXITCODE -ne 0) {
        Write-Error "scene package build failed"
        exit 1
    }
} finally {
    Pop-Location
}

# 运行时挂载 `assets/` 下的**全部** `.pak`（#530），所以按扩展名复制，
# 不再写死 `scenes.pak`——以后按内容分包不用改脚本。
$Paks = Get-ChildItem (Join-Path $ProjectRoot "assets") -Filter *.pak -File
$Manifests = Get-ChildItem (Join-Path $ProjectRoot "assets") -Filter *.pak.meta.json -File
if ($Paks.Count -eq 0) {
    Write-Error "no .pak produced under assets/ — the APK would contain no assets"
    exit 1
}
$Paks | Copy-Item -Destination $AndroidGameAssets -Force
$Manifests | Copy-Item -Destination $AndroidGameAssets -Force
Write-Host "Copied $($Paks.Count) package(s) into the APK assets."

# 新格式 `settings.ini`；旧项目还只有 `settings.toml` 时照样带上（引擎会回退读）。
foreach ($Name in @("settings.ini", "settings.toml")) {
    $Settings = Join-Path $ProjectRoot "assets\$Name"
    if (Test-Path $Settings) {
        Copy-Item $Settings (Join-Path $AndroidGameAssets $Name) -Force
    }
}

$Environment = Join-Path $ProjectRoot "assets\valley_of_desolation_1k.hdr"
if (Test-Path $Environment) {
    Copy-Item $Environment $AndroidGameAssets -Force
}

# ---- Build Rust .so --------------------------------------------------------

Write-Host "Building game ($Project crate) for $target..."

cargo ndk `
    -P $apiLevel `
    -t arm64-v8a `
    -o $JniLibsDir `
    build --profile dist --manifest-path $GameManifest -p $Project

if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo ndk failed"
    exit 1
}

Write-Host "Rust .so built successfully."

# ---- Assemble APK via the Tauri CLI ----------------------------------------

# Do NOT call `gradlew assembleDebug` directly: the Gradle `rust` plugin's
# rustBuild* tasks shell out to `pnpm tauri android android-studio-script` to
# build the launcher's own .so, and that only resolves when the Tauri CLI (or
# Android Studio) is the process launching Gradle. Driving Gradle from here
# fails with "A problem occurred starting process 'command 'pnpm.bat''".
# The Tauri CLI runs the same Gradle build and picks up the .so we just wrote
# into jniLibs/.
$LauncherDir = Join-Path $ProjectRoot "launcher"
Write-Host "Assembling APK via Tauri CLI (pnpm tauri android build)..."
Push-Location $LauncherDir
try {
    pnpm tauri android build --debug --target aarch64
    if ($LASTEXITCODE -ne 0) {
        Write-Error "tauri android build failed"
        exit 1
    }
} finally {
    Pop-Location
}

$apk = Join-Path $AndroidDir "app\build\outputs\apk\universal\debug\app-universal-debug.apk"
if (Test-Path $apk) {
    Write-Host "APK: $apk"
} else {
    Write-Warning "Expected APK not found at $apk (check the Tauri CLI output above)."
}

Write-Host "Build complete."
