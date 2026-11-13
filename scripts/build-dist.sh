#!/usr/bin/env bash
# 发布档构建：nightly `-Z build-std` + immediate-abort，比 stable `dist` 再小 9~10%。
#
#   bash scripts/build-dist.sh                        # 默认 projects/template，宿主 target
#   bash scripts/build-dist.sh projects/fishing       # 指定工程
#   bash scripts/build-dist.sh projects/template aarch64-linux-android
#   bash scripts/build-dist.sh --size [project] [target]   # 极限体积（tiny profile + 禁内联）
#   bash scripts/build-dist.sh --size --pack [project]     # 再套 UPX 自解压壳（见下）
#   PROFILE=dist bash scripts/build-dist.sh           # 换 profile
#
# --pack：构建完成后用 `upx --best --lzma` 压一层自解压壳。**只给发布档用**，而且压完
# 必须对产物做代码签名——未签名的压缩壳是最经典的杀软误报模式（Windows Defender 等）。
# 找不到 upx 时只提示、不失败：压缩是可选优化，不该让构建变红。
#
# 为什么要它：官方 libstd 是预编译的，链接时只能整块接受；`-Z build-std` 让 std 和你的
# 代码一起走 fat LTO，`-Cpanic=immediate-abort` 再删掉 panic 格式化（core::fmt）那一大片。
# 实测（aarch64 Linux，dist）：3D 全量 4.47 MB → 4.01 MB（−459 KB），
# 2D 极简 + 仅 X11 2.89 MB → 2.63 MB（−262 KB）。
#
# 代价：只在**发布构建**需要 nightly，日常 `cargo run` 仍是 stable——因为参数全在
# 命令行，没有写进任何 Cargo.toml（写了会让 stable cargo 直接拒绝解析清单）。
#
# 注意：immediate-abort 下 panic 只剩 abort，没有消息/回溯；只用于发布包，别拿它调试。
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --size：极限体积档（tiny profile + 禁止内联）。位置参数保持向后兼容。
# --pack：可选压缩壳（UPX）。同上，位置参数不受影响。
SIZE_MODE=0
PACK_MODE=0
ARGS=()
for arg in "$@"; do
    case "$arg" in
        --size|--tiny) SIZE_MODE=1 ;;
        --pack|--upx) PACK_MODE=1 ;;
        *) ARGS+=("$arg") ;;
    esac
done
PROJECT="${ARGS[0]:-projects/template}"
TARGET="${ARGS[1]:-}"
if [ "${SIZE_MODE}" = "1" ]; then
    PROFILE="${PROFILE:-tiny}"
else
    PROFILE="${PROFILE:-dist}"
fi
NIGHTLY="${NIGHTLY:-nightly}"

# ---------------------------------------------------------------- 环境检查
if ! command -v rustup >/dev/null 2>&1; then
    echo "build-dist: 需要 rustup（用它装 nightly + rust-src）" >&2
    exit 1
fi

if ! rustup toolchain list | grep -q "^${NIGHTLY}"; then
    echo "build-dist: 安装 ${NIGHTLY}（首次约 100 MB）"
    rustup toolchain install "${NIGHTLY}" --profile minimal
fi

if ! rustup component list --toolchain "${NIGHTLY}" | grep -q '^rust-src (installed)'; then
    echo "build-dist: 安装 rust-src"
    rustup component add rust-src --toolchain "${NIGHTLY}"
fi

# build-std 要求显式 --target，即使目标是宿主。
if [ -z "${TARGET}" ]; then
    TARGET="$("${NIGHTLY_TRIPLE:-rustc}" -vV 2>/dev/null | sed -n 's/^host: //p' || true)"
    if [ -z "${TARGET}" ]; then
        TARGET="$(rustc -vV | sed -n 's/^host: //p')"
    fi
fi

MANIFEST="${ROOT}/${PROJECT}/Cargo.toml"
if [ ! -f "${MANIFEST}" ]; then
    echo "build-dist: 找不到 ${MANIFEST}" >&2
    exit 1
fi

# 工程自己的 workspace 根（profile 从那里读）。
PKG="$(sed -n 's/^name = "\(.*\)"/\1/p' "${MANIFEST}" | head -1)"

echo "build-dist: ${PROJECT} (${PKG}) → ${TARGET} / profile=${PROFILE}"

# ---------------------------------------------------------------- 构建
# RUSTFLAGS 同时作用于 std 与工程，保证 panic 策略一致；
# 不写进 Cargo.toml，stable 构建因此完全不受影响。
# --size 再叠一刀：禁内联（纯体积手段）。热路径会慢 10–30%，只建议竞赛/极限场景用；
# 想两者兼得就在工程 Cargo.toml 里给关键 crate 单独写 [profile.tiny.package.<crate>] opt-level = 3。
EXTRA_RUSTFLAGS=""
if [ "${SIZE_MODE}" = "1" ]; then
    EXTRA_RUSTFLAGS=" -C llvm-args=-inline-threshold=0 -C llvm-args=-inlinehint-threshold=0"
fi

RUSTFLAGS="-Zunstable-options -Cpanic=immediate-abort${EXTRA_RUSTFLAGS}" \
    cargo "+${NIGHTLY}" build \
    --manifest-path "${MANIFEST}" \
    --profile "${PROFILE}" \
    --package "${PKG}" \
    --target "${TARGET}" \
    -Z build-std=std,panic_abort

# ---------------------------------------------------------------- 报告
BIN="${ROOT}/${PROJECT}/target/${TARGET}/${PROFILE}/${PKG}"
# Windows 目标的可执行文件带 `.exe` 后缀。
if [ ! -f "${BIN}" ] && [ -f "${BIN}.exe" ]; then
    BIN="${BIN}.exe"
fi
if [ -f "${BIN}" ]; then
    SIZE="$(stat -c%s "${BIN}")"
    printf 'build-dist: %s  %s bytes (%.2f MB)\n' "${BIN}" "${SIZE}" "$(echo "${SIZE}" | awk '{print $1/1048576}')"
else
    echo "build-dist: 构建成功但没找到 ${BIN}（检查包名与 bin 名是否一致）" >&2
fi

# ---------------------------------------------------------------- 可选压缩壳（--pack）
# 顺序：不在 Android 上做 → 没有 upx 就提示并存盘 → 有 upx 才压。
# 任何一步都不允许让构建失败：压缩是发布包的可选优化，不是构建的正确性前提。
if [ "${PACK_MODE}" = "1" ]; then
    if [ ! -f "${BIN}" ]; then
        echo "build-dist: --pack 跳过（没有找到产物）" >&2
    elif [ "${TARGET#*android}" != "${TARGET}" ]; then
        # Android 的 .so 由系统加载器 / Play 扫描主导，压缩壳只会换来误报与
        # 更慢的首次 page-in，收益为负。
        echo "build-dist: --pack 跳过：Android 产物不打压缩壳"
    elif ! command -v upx >/dev/null 2>&1; then
        echo "build-dist: 未找到 upx，跳过压缩（--pack 是可选项，构建照常成功）"
        echo "  安装：Termux  pkg install upx"
        echo "        Debian/Ubuntu  sudo apt install upx-ucl"
        echo "        Windows  winget install upx（或 https://upx.github.io）"
        echo "  未压缩产物已保留：${BIN}"
    else
        BEFORE="$(stat -c%s "${BIN}")"
        echo "build-dist: $(upx --version 2>/dev/null | head -1 || echo upx) → upx --best --lzma"
        if upx --best --lzma "${BIN}"; then
            AFTER="$(stat -c%s "${BIN}")"
            printf 'build-dist: packed  %s → %s bytes (%.2f MB, 压缩率 %.1f%%)\n' \
                "${BEFORE}" "${AFTER}" \
                "$(awk -v a="${AFTER}" 'BEGIN { print a/1048576 }')" \
                "$(awk -v a="${BEFORE}" -v b="${AFTER}" 'BEGIN { if (a > 0) print (1-b/a)*100; else print 0 }')"
        else
            echo "build-dist: upx 压缩失败，保留未压缩产物（构建本身仍然成功）" >&2
        fi
    fi
    echo "build-dist: 注意：压缩后请对产物做代码签名，否则 Windows Defender 等会误报（未签名的自解压壳是典型启发式特征）"
fi
