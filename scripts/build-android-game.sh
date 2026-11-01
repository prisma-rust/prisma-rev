#!/usr/bin/env bash
# 打 game-only Android APK（无 launcher / 无 Tauri / 无 WebView）。
#
#   bash scripts/build-android-game.sh                      # projects/prisma，debug
#   bash scripts/build-android-game.sh fishing              # projects/fishing，debug
#   bash scripts/build-android-game.sh fishing release       # release（未签名，需自行 apksigner）
#   bash scripts/build-android-game.sh debug --skip-cargo    # 只重打包（.so 已就绪）
#   bash scripts/build-android-game.sh --project fishing --skip-cargo
#
# 产物：<project>/android/app/build/outputs/apk/<变体>/*.apk
#
# 工程名是 `projects/` 下的目录名（也可以给相对/绝对路径）。库名从
# `<project>/assets/settings.ini [build] lib_name` 或 `Cargo.toml` 读出来，脚本不再
# 按目录名猜 `.so` 名字；工程缺 `android/` 时先跑
# `bash scripts/new-android-game.sh <project>` 生成宿主（#598）。
#
# 环境变量（至少要有 SDK 和 NDK）：
#   ANDROID_HOME       Android SDK 根目录（ANDROID_SDK_ROOT 亦可）
#   ANDROID_NDK_HOME   NDK 根目录（不设则取 $ANDROID_HOME/ndk/ 里最新的）
#
# aarch64 主机（Ubuntu glibc / Termux）额外需要：
#   - 宿主 clang + lld + LLVM 工具（apt install clang lld llvm-21 / pkg install clang lld llvm）
#   - 先跑一次 bash scripts/make-ndk-host-native.sh "$ANDROID_NDK_HOME"（把官方 x86_64
#     NDK 换成本机可执行的 clang shim）
#   - build-tools 里的 aapt2/aidl/zipalign/split-select 换成 aarch64 版（Google 只发
#     x86_64），本脚本会自动用 -Pandroid.aapt2FromMavenOverride 指向 SDK 里能跑的那个
# 详见 docs/android-apk.md；aarch64 主机另见 docs/android-arm64.md。
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=lib/project-info.sh
. "$ROOT/scripts/lib/project-info.sh"

MODE=""
PROJECT=""
SKIP_CARGO=0
while [ $# -gt 0 ]; do
    case "$1" in
        debug|release)
            [ -z "$MODE" ] || { echo "模式只能给一次（$MODE / $1）" >&2; exit 1; }
            MODE="$1"; shift ;;
        --project)
            [ $# -ge 2 ] || { echo "--project 需要工程名" >&2; exit 1; }
            PROJECT="$2"; shift 2 ;;
        --skip-cargo) SKIP_CARGO=1; shift ;;
        -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
        -*) echo "未知参数：$1" >&2; exit 1 ;;
        *)
            [ -z "$PROJECT" ] || { echo "只需要一个工程名" >&2; exit 1; }
            PROJECT="$1"; shift ;;
    esac
done
MODE="${MODE:-debug}"

PROJECT_DIR=$(prism_project_dir "$ROOT" "${PROJECT:-prisma}")
PROJECT_NAME=$(prism_project_name "$PROJECT_DIR")
LIB_NAME=$(prism_lib_name "$PROJECT_DIR")
ANDROID_DIR="$PROJECT_DIR/android"
GAME_MANIFEST="$PROJECT_DIR/Cargo.toml"
ASSETS_DIR="$PROJECT_DIR/assets"
JNI_DIR="$ANDROID_DIR/app/src/main/jniLibs/arm64-v8a"
ASSET_DIR="$ANDROID_DIR/app/src/main/assets/game"
TARGET=aarch64-linux-android

[ -d "$ANDROID_DIR" ] || {
    echo "工程 $PROJECT_NAME 还没有 Android 宿主：$ANDROID_DIR" >&2
    echo "先生成：bash scripts/new-android-game.sh $PROJECT_NAME" >&2
    exit 1
}

# 宿主 manifest 里固定了 `android.app.lib_name`；它与库名不一致时游戏会在
# `System.loadLibrary` 处崩溃，所以构建期就拦住（#598）。
MANIFEST="$ANDROID_DIR/app/src/main/AndroidManifest.xml"
DECLARED_LIB=$(sed -nE 's/.*android:name="android.app.lib_name".*//; s/.*android:value="([^"]+)".*/\1/p' "$MANIFEST" | head -n1)
if [ -z "$DECLARED_LIB" ]; then
    DECLARED_LIB=$(tr -d '\n' < "$MANIFEST" | grep -o 'android.app.lib_name"[[:space:]]*android:value="[^"]*"' | sed -E 's/.*android:value="([^"]+)"/\1/' || true)
fi
if [ -n "$DECLARED_LIB" ] && [ "$DECLARED_LIB" != "$LIB_NAME" ]; then
    echo "库名不一致：$MANIFEST 声明 lib_name=$DECLARED_LIB，而工程库名是 $LIB_NAME" >&2
    echo "改 manifest 的 android.app.lib_name，或在 assets/settings.ini [build] 里写 lib_name = $DECLARED_LIB" >&2
    exit 1
fi

case "$MODE" in
    debug)   GRADLE_TASK=assembleDebug;   CARGO_PROFILE_DIR=debug ;;
    release) GRADLE_TASK=assembleRelease; CARGO_PROFILE_DIR=release ;;
    *) echo "用法: $0 [project] [debug|release] [--skip-cargo]" >&2; exit 1 ;;
esac

# ---- SDK / NDK -------------------------------------------------------------
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
[ -n "$SDK" ] && [ -d "$SDK" ] || {
    echo "找不到 Android SDK：设置 ANDROID_HOME" >&2
    exit 1
}
NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [ -z "$NDK" ]; then
    NDK=$(ls -d "$SDK"/ndk/*/ 2>/dev/null | sort -V | tail -n1)
    NDK="${NDK%/}"
fi
[ -n "$NDK" ] && [ -d "$NDK/toolchains/llvm" ] || {
    echo "找不到 NDK：设置 ANDROID_NDK_HOME（sdkmanager \"ndk;<版本>\"）" >&2
    exit 1
}
export ANDROID_NDK_HOME="$NDK"
echo "NDK: $NDK"

HOST_PRE=$(ls -d "$NDK"/toolchains/llvm/prebuilt/*/ | head -n1)
HOST_PRE="${HOST_PRE%/}"

# ---- API level = minSdk（不低于 26：libaaudio.so 从 26 才有）----------------
MIN_SDK=$(sed -nE 's/.*minSdk *= *([0-9]+).*/\1/p' "$ANDROID_DIR/app/build.gradle.kts" | head -n1)
MIN_SDK="${MIN_SDK:-31}"
API=$(( MIN_SDK < 26 ? 26 : MIN_SDK ))
LINKER="$HOST_PRE/bin/aarch64-linux-android$API-clang"
[ -x "$LINKER" ] || {
    echo "找不到 $LINKER —— aarch64 主机请先跑：bash scripts/make-ndk-host-native.sh \"$NDK\"" >&2
    exit 1
}
echo "project=$PROJECT_NAME lib=$LIB_NAME target=$TARGET api=$API (minSdk=$MIN_SDK)"

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$LINKER"
export CC_aarch64_linux_android="$LINKER"
export CXX_aarch64_linux_android="$HOST_PRE/bin/aarch64-linux-android$API-clang++"
export AR_aarch64_linux_android="${AR_aarch64_linux_android:-ar}"
export RANLIB_aarch64_linux_android="${RANLIB_aarch64_linux_android:-ranlib}"

printf 'sdk.dir=%s\n' "$SDK" > "$ANDROID_DIR/local.properties"

# ---- shader 产物（.spv 不入库）--------------------------------------------
if ! ls "$ROOT"/assets/shaders/*.spv >/dev/null 2>&1 && ! ls "$ROOT"/assets/shaders/slang/*.spv >/dev/null 2>&1; then
    echo "缺少 .spv：先在有 slangc 的机器上 bash assets/shaders/compile.sh，" >&2
    echo "或从 CI 的 spirv artifact 下载（见 docs/build-guide-termux.md §3）。" >&2
    exit 1
fi

# ---- 1. 游戏 .so ----------------------------------------------------------
if [ "$SKIP_CARGO" -eq 0 ]; then
    echo "==> cargo build ($MODE) $TARGET"
    cargo build --manifest-path "$GAME_MANIFEST" --lib --target "$TARGET" \
        $( [ "$MODE" = release ] && echo --release )
fi
SO="$PROJECT_DIR/target/$TARGET/$CARGO_PROFILE_DIR/lib$LIB_NAME.so"
[ -f "$SO" ] || { echo "找不到 $SO" >&2; exit 1; }
mkdir -p "$JNI_DIR"
cp -f "$SO" "$JNI_DIR/lib$LIB_NAME.so"
echo "==> lib$LIB_NAME.so $(du -h "$JNI_DIR/lib$LIB_NAME.so" | cut -f1)"

# ---- 2. 场景包 + 资产 -----------------------------------------------------
# 每个工程有自己的 assets/ 与 scenes.json；`build-scenes` 读工程的清单、写工程的
# pak（#598），不再固定跑仓库根的 `assets/`。
echo "==> build-scenes（$ASSETS_DIR/scenes.json）"
[ -f "$ASSETS_DIR/scenes.json" ] || { echo "找不到 $ASSETS_DIR/scenes.json" >&2; exit 1; }
(
    cd "$ROOT"
    cargo run -q -p prism-build-pipeline -- build-scenes \
        --manifest "$ASSETS_DIR/scenes.json" \
        --staging "$PROJECT_DIR/target/scene_pack" \
        --output "$ASSETS_DIR/scenes.pak"
)
mkdir -p "$ASSET_DIR"
# 运行时挂载 `assets/` 下的全部 `.pak`（#530），所以按扩展名复制。
shopt -s nullglob
PAKS=("$ASSETS_DIR"/*.pak)
METAS=("$ASSETS_DIR"/*.pak.meta.json)
HDRS=("$ASSETS_DIR"/*.hdr "$ASSETS_DIR"/*.exr)
shopt -u nullglob
[ ${#PAKS[@]} -gt 0 ] || { echo "$ASSETS_DIR 下没有 .pak，APK 里将没有任何资源" >&2; exit 1; }
cp -f "${PAKS[@]}" "${METAS[@]}" "$ASSET_DIR/"
# 新格式 `settings.ini`；旧项目还只有 `settings.toml` 时照样带上（引擎会回退读）。
for name in settings.ini settings.toml; do
    [ -f "$ASSETS_DIR/$name" ] && cp -f "$ASSETS_DIR/$name" "$ASSET_DIR/"
done
if [ ${#HDRS[@]} -gt 0 ]; then
    cp -f "${HDRS[@]}" "$ASSET_DIR/"
fi

# ---- 3. APK ---------------------------------------------------------------
# aarch64 主机上 AGP 默认从 Maven 拉 aapt2（x86_64，跑不起来）；指向 SDK 里能跑的那个。
GRADLE_ARGS=(-p "$ANDROID_DIR" "$GRADLE_TASK" --console=plain)
for cand in $(ls -d "$SDK"/build-tools/*/aapt2 2>/dev/null | sort -V -r); do
    if "$cand" version >/dev/null 2>&1; then
        echo "aapt2: $cand"
        GRADLE_ARGS+=("-Pandroid.aapt2FromMavenOverride=$cand")
        break
    fi
done
if ! ls "$SDK"/build-tools/*/aapt2 >/dev/null 2>&1 || ! "$(ls -d "$SDK"/build-tools/*/aapt2 | sort -V -r | head -n1)" version >/dev/null 2>&1; then
    echo "提示：SDK 里没有能在本机执行的 aapt2（aarch64 主机见 docs/android-arm64.md）。" >&2
fi

echo "==> gradle $GRADLE_TASK"
# 一定要 clean：换过 .so 后增量打包会在 zip 里留下巨大的空洞（曾经 21MB 产物变 217MB）。
"$ANDROID_DIR/gradlew" clean "${GRADLE_ARGS[@]}" --no-daemon \
    -Dorg.gradle.jvmargs="-Xmx2g -XX:MaxMetaspaceSize=768m -Dfile.encoding=UTF-8"

APK=$(ls -t "$ANDROID_DIR"/app/build/outputs/apk/*/*.apk 2>/dev/null | head -n1)
[ -n "$APK" ] || { echo "没有找到 APK 产物" >&2; exit 1; }
echo
echo "APK: $APK ($(du -h "$APK" | cut -f1))"
if [ "$MODE" = release ]; then
    echo "release 未签名，安装前先签名："
    echo "  \$SDK/build-tools/<ver>/apksigner sign --ks <keystore> --out app-signed.apk \"$APK\""
fi
