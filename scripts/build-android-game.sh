#!/usr/bin/env bash
# 打 game-only Android APK（无 launcher / 无 Tauri / 无 WebView）。
#
#   bash scripts/build-android-game.sh            # debug
#   bash scripts/build-android-game.sh release    # release（未签名，需自行 apksigner）
#   bash scripts/build-android-game.sh debug --skip-cargo   # 只重打包（.so 已就绪）
#
# 产物：projects/prisma/android/app/build/outputs/apk/<变体>/*.apk
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

MODE="${1:-debug}"
shift || true
SKIP_CARGO=0
for arg in "$@"; do
    case "$arg" in
        --skip-cargo) SKIP_CARGO=1 ;;
        *) echo "未知参数：$arg" >&2; exit 1 ;;
    esac
done

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ANDROID_DIR="$ROOT/projects/prisma/android"
GAME_MANIFEST="$ROOT/projects/prisma/Cargo.toml"
JNI_DIR="$ANDROID_DIR/app/src/main/jniLibs/arm64-v8a"
ASSET_DIR="$ANDROID_DIR/app/src/main/assets/game"
TARGET=aarch64-linux-android

case "$MODE" in
    debug)   GRADLE_TASK=assembleDebug;   CARGO_PROFILE_DIR=debug ;;
    release) GRADLE_TASK=assembleRelease; CARGO_PROFILE_DIR=release ;;
    *) echo "用法: $0 [debug|release] [--skip-cargo]" >&2; exit 1 ;;
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
echo "target=$TARGET api=$API (minSdk=$MIN_SDK)"

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
SO="$ROOT/projects/prisma/target/$TARGET/$CARGO_PROFILE_DIR/libprisma.so"
[ -f "$SO" ] || { echo "找不到 $SO" >&2; exit 1; }
mkdir -p "$JNI_DIR"
cp -f "$SO" "$JNI_DIR/libprisma.so"
echo "==> libprisma.so $(du -h "$JNI_DIR/libprisma.so" | cut -f1)"

# ---- 2. 场景包 + 资产 -----------------------------------------------------
echo "==> build-scenes"
cargo run -q -p prism-build-pipeline -- build-scenes
mkdir -p "$ASSET_DIR"
# 运行时挂载 `assets/` 下的全部 `.pak`（#530），所以按扩展名复制。
shopt -s nullglob
PAKS=("$ROOT"/assets/*.pak)
METAS=("$ROOT"/assets/*.pak.meta.json)
shopt -u nullglob
[ ${#PAKS[@]} -gt 0 ] || { echo "assets/ 下没有 .pak，APK 里将没有任何资源" >&2; exit 1; }
cp -f "${PAKS[@]}" "${METAS[@]}" "$ASSET_DIR/"
# 新格式 `settings.ini`；旧项目还只有 `settings.toml` 时照样带上（引擎会回退读）。
for name in settings.ini settings.toml; do
    [ -f "$ROOT/assets/$name" ] && cp -f "$ROOT/assets/$name" "$ASSET_DIR/"
done
if [ -f "$ROOT/assets/valley_of_desolation_1k.hdr" ]; then
    cp -f "$ROOT/assets/valley_of_desolation_1k.hdr" "$ASSET_DIR/"
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
