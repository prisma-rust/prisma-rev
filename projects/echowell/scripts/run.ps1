param(
    [switch]$SkipAssetGeneration,
    [switch]$SkipSceneBuild,
    [switch]$Release
)

$ErrorActionPreference = "Stop"
$projectDir = (Resolve-Path (Join-Path $PSScriptRoot "..\")).Path
$repoRoot = (Resolve-Path (Join-Path $projectDir "..\..\")).Path

if (-not $SkipAssetGeneration) {
    $assetsDir = Join-Path $projectDir "assets\sprites"
    if (-not (Test-Path (Join-Path $assetsDir "hero.png"))) {
        Push-Location $projectDir
        try {
            python scripts/generate_assets.py
            if ($LASTEXITCODE -ne 0) { throw "Sprite generation failed" }
        } finally {
            Pop-Location
        }
    }
}

if (-not $SkipSceneBuild) {
    Push-Location $repoRoot
    try {
        cargo run -q -p prism-build-pipeline -- build-scenes `
            --manifest projects/echowell/assets/scenes.json `
            --staging projects/echowell/target/scene_pack `
            --output projects/echowell/assets/scenes.pak
        if ($LASTEXITCODE -ne 0) { throw "Scene package build failed" }
    } finally {
        Pop-Location
    }
}

Push-Location $projectDir
try {
    $cargoArgs = @("run", "--bin", "echowell")
    if ($Release) { $cargoArgs += "--release" }
    & cargo @cargoArgs
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
