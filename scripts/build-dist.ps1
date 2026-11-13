# 发布档构建（Windows）：nightly `-Z build-std` + immediate-abort。
# 与 `scripts/build-dist.sh` 等价，参数/理由见那边的注释。
#
#   powershell -File scripts/build-dist.ps1
#   powershell -File scripts/build-dist.ps1 projects/template
#   powershell -File scripts/build-dist.ps1 projects/template x86_64-pc-windows-gnu
#   powershell -File scripts/build-dist.ps1 -Size projects/template    # 极限体积（tiny + 禁内联）
#   powershell -File scripts/build-dist.ps1 -Pack projects/template    # 再套 UPX 压缩壳
#
# -Size 对 `.sh --size`：切 `tiny` profile（opt-level="z"）并禁内联，比 dist 再小 ~30%，
# 代价是 CPU 热路径慢 10–30%。-Size 与 -Profile 同时给时以 -Profile 为准。
# -Pack 只给发布档用，压完必须做代码签名（未签名的自解压壳会被 Windows Defender 等误报）。
# 找不到 upx 时只提示、不失败。
param(
    [string]$Project = "projects/template",
    [string]$Target = "",
    [string]$Profile = "dist",
    [string]$Nightly = "nightly",
    [switch]$Pack,
    [switch]$Size
)

$ErrorActionPreference = "Stop"
# scripts/ 的上一级就是仓库根；参数一律用位置传，PowerShell 5.1 的 Join-Path 不吃 -LiteralPath。
$Root = Split-Path -Parent $PSScriptRoot
$Manifest = Join-Path $Root (Join-Path $Project "Cargo.toml")
if (-not (Test-Path $Manifest)) { throw "找不到 $Manifest" }

if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    throw "需要 rustup（用它装 nightly + rust-src）"
}

$installed = rustup toolchain list
if ($installed -notmatch "^$Nightly") {
    Write-Host "build-dist: 安装 $Nightly（首次约 100 MB）"
    rustup toolchain install $Nightly --profile minimal
}
$components = rustup component list --toolchain $Nightly
if ($components -notmatch "rust-src \(installed\)") {
    Write-Host "build-dist: 安装 rust-src"
    rustup component add rust-src --toolchain $Nightly
}

if ([string]::IsNullOrEmpty($Target)) {
    $Target = ((rustc -vV | Select-String '^host: ').ToString() -replace '^host: ', '').Trim()
}

$name = (Select-String -Path $Manifest -Pattern '^name = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value

# -Size 对 `.sh --size`：默认档位切 tiny，再叠「禁内联」这刀（纯体积手段）。
# 显式给了 -Profile 就尊重它，只加禁内联。
$ExtraRustFlags = ""
if ($Size) {
    if ($Profile -eq "dist") { $Profile = "tiny" }
    $ExtraRustFlags = " -C llvm-args=-inline-threshold=0 -C llvm-args=-inlinehint-threshold=0"
}

Write-Host "build-dist: $Project ($name) → $Target / profile=$Profile"

# 参数只走命令行，不写进 Cargo.toml：stable 构建不受影响。
$env:RUSTFLAGS = "-Zunstable-options -Cpanic=immediate-abort$ExtraRustFlags"
cargo "+$Nightly" build `
    --manifest-path $Manifest `
    --profile $Profile `
    --package $name `
    --target $Target `
    -Z build-std=std,panic_abort
if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（exit $LASTEXITCODE）" }

$bin = Join-Path $Root (Join-Path $Project "target\${Target}\${Profile}\${name}.exe")
if (Test-Path $bin) {
    # 注意别用 `$size`：PowerShell 变量大小写不敏感，会和 `-Size` 开关撞名。
    $bytes = (Get-Item $bin).Length
    Write-Host ("build-dist: {0}  {1} bytes ({2:N2} MB)" -f $bin, $bytes, ($bytes / 1MB))
} else {
    Write-Host "build-dist: 构建成功但没找到 $bin（检查包名与 bin 名是否一致）"
}

# 可选压缩壳（-Pack）。压缩不是构建的正确性前提，任何一步失败都只提示。
if ($Pack) {
    if (-not (Test-Path $bin)) {
        Write-Host "build-dist: -Pack 跳过（没有找到产物）"
    } elseif ($Target -match 'android') {
        Write-Host "build-dist: -Pack 跳过：Android 产物不打压缩壳"
    } elseif (-not (Get-Command upx -ErrorAction SilentlyContinue)) {
        Write-Host "build-dist: 未找到 upx，跳过压缩（-Pack 是可选项，构建照常成功）"
        Write-Host "  安装：winget install upx，或从 https://upx.github.io 下载后加进 PATH"
        Write-Host "  未压缩产物已保留：$bin"
    } else {
        $before = (Get-Item $bin).Length
        Write-Host "build-dist: upx --best --lzma"
        & upx --best --lzma $bin
        if ($LASTEXITCODE -eq 0) {
            $after = (Get-Item $bin).Length
            $ratio = if ($before -gt 0) { (1 - $after / $before) * 100 } else { 0 }
            Write-Host ("build-dist: packed  {0} -> {1} bytes ({2:N2} MB, 压缩率 {3:N1}%)" -f $before, $after, ($after / 1MB), $ratio)
        } else {
            Write-Host "build-dist: upx 压缩失败，保留未压缩产物（构建本身仍然成功）"
        }
    }
    Write-Host "build-dist: 注意：压缩后请对产物做代码签名，否则 Windows Defender 等会误报（未签名的自解压壳是典型启发式特征）"
}
