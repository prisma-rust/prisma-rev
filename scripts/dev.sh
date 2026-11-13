#!/usr/bin/env bash
# PrismaRev dev launcher (Linux / macOS) — the bash counterpart of scripts/dev.ps1.
#
#   scripts/dev.sh                  # projects/prisma
#   scripts/dev.sh sponza           # projects/sponza
#   scripts/dev.sh sponza --release -- --some-game-arg
#
# The runtime loads scenes only from `assets/*.pak` (there is no disk fallback for
# the scene itself), so `cargo run` on its own opens a window that draws nothing but
# the clear colour — mid grey, with no dialog. This script builds the scene package
# first, then runs the project.
#
# Options:
#   --release       run the game with --release
#   --skip-scenes   keep the existing assets/scenes.pak
#   --shaders       recompile Slang first (needs `slangc`; the .spv are committed,
#                   so this is only needed after editing a .slang)
#   --              everything after it is passed to the game binary
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

PROJECT="prisma"
RELEASE=0
BUILD_SCENES=1
BUILD_SHADERS=0
PASSTHROUGH=()

while [ $# -gt 0 ]; do
    case "$1" in
        --release) RELEASE=1 ;;
        --skip-scenes) BUILD_SCENES=0 ;;
        --shaders) BUILD_SHADERS=1 ;;
        --) shift; PASSTHROUGH=("$@"); break ;;
        -h | --help) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) PROJECT="$1" ;;
    esac
    shift
done

PROJECT_DIR="$ROOT/projects/$PROJECT"
if [ ! -f "$PROJECT_DIR/Cargo.toml" ]; then
    echo "no such project: $PROJECT (looked for $PROJECT_DIR/Cargo.toml)" >&2
    echo "available: $(ls "$ROOT/projects" | tr '\n' ' ')" >&2
    exit 2
fi

# `build-scenes` resolves `assets/` against the current directory, so it has to run
# where the manifest lives: a project with its own `assets/scenes.json` packages
# into its own directory, one without (projects/prisma) reuses the repo-root assets.
if [ -f "$PROJECT_DIR/assets/scenes.json" ]; then
    ASSET_ROOT="$PROJECT_DIR"
else
    ASSET_ROOT="$ROOT"
fi

if [ "$BUILD_SHADERS" = 1 ]; then
    echo "==> recompiling Slang shaders"
    bash "$ROOT/assets/shaders/compile.sh"
fi

if [ "$BUILD_SCENES" = 1 ]; then
    echo "==> building scene package in $ASSET_ROOT"
    (
        cd "$ASSET_ROOT"
        cargo run -q --manifest-path "$ROOT/Cargo.toml" -p prism-build-pipeline -- build-scenes
    )
fi

CMD=(cargo run)
if [ "$RELEASE" = 1 ]; then
    CMD+=(--release)
fi
if [ ${#PASSTHROUGH[@]} -gt 0 ]; then
    CMD+=(--)
    CMD+=("${PASSTHROUGH[@]}")
fi

echo "==> running $PROJECT ($PROJECT_DIR)"
cd "$PROJECT_DIR"
"${CMD[@]}"
