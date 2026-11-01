#!/usr/bin/env bash
# 给用户工程生成 game-only Android 宿主（#598）。
#
#   bash scripts/new-android-game.sh fishing
#   bash scripts/new-android-game.sh fishing --package com.example.fishing --label "Cozy Fishing"
#   bash scripts/new-android-game.sh fishing --force   # 覆盖已有 projects/fishing/android
#
# 从 `templates/android-game/` 复制 Gradle 工程，替换三处占位符：
#   __GAME_NAME__  工程的库名（`[lib] name` → `[package] name`，`-` 换成 `_`）
#   __PACKAGE__    Android 包名（默认 `com.prisma.<工程名>`）
#   __LABEL__      桌面图标下的应用名（默认工程名）
#
# 生成之后用 `bash scripts/build-android-game.sh <project>` 打 APK。模板里已经带
# `minSdk 31`、`abiFilters arm64-v8a`、`packaging.keepDebugSymbols` 与签名注释，**不要**
# 再用 `tauri android init` 之类的工具覆盖（见 docs/android-apk.md）。
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=lib/project-info.sh
. "$ROOT/scripts/lib/project-info.sh"

PROJECT=""
PACKAGE=""
LABEL=""
FORCE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --package)
            [ $# -ge 2 ] || { echo "--package 需要包名" >&2; exit 1; }
            PACKAGE="$2"; shift 2 ;;
        --label)
            [ $# -ge 2 ] || { echo "--label 需要应用名" >&2; exit 1; }
            LABEL="$2"; shift 2 ;;
        --force) FORCE=1; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        -*) echo "未知参数：$1" >&2; exit 1 ;;
        *) [ -z "$PROJECT" ] || { echo "只需要一个工程名" >&2; exit 1; }; PROJECT="$1"; shift ;;
    esac
done

[ -n "$PROJECT" ] || { echo "用法: $0 <project> [--package <id>] [--label <name>] [--force]" >&2; exit 1; }

PROJECT_DIR=$(prism_project_dir "$ROOT" "$PROJECT")
PROJECT_NAME=$(prism_project_name "$PROJECT_DIR")
LIB_NAME=$(prism_lib_name "$PROJECT_DIR")
if [ -z "$PACKAGE" ]; then
    SANITIZED=$(printf '%s' "$PROJECT_NAME" | tr -c 'a-zA-Z0-9' '_' | sed 's/_*$//')
    [ -n "$SANITIZED" ] || {
        echo "无法从目录名 '$PROJECT_NAME' 推断包名；请用 --package <id>" >&2
        exit 1
    }
    PACKAGE="com.prisma.$SANITIZED"
fi
case "$PACKAGE" in
    *[!a-zA-Z0-9._]*|.*|*.|*..*)
        echo "包名 '$PACKAGE' 不合法（只允许字母 / 数字 / 点，且不能以点开头或结尾）" >&2
        exit 1 ;;
esac
LABEL="${LABEL:-$PROJECT_NAME}"
PACKAGE_PATH=$(printf '%s' "$PACKAGE" | tr '.' '/')

TEMPLATE="$ROOT/templates/android-game"
[ -d "$TEMPLATE" ] || { echo "缺少模板目录：$TEMPLATE" >&2; exit 1; }

ANDROID_DIR="$PROJECT_DIR/android"
if [ -e "$ANDROID_DIR" ] && [ "$FORCE" -eq 0 ]; then
    echo "$ANDROID_DIR 已存在；确实要覆盖请加 --force" >&2
    exit 1
fi

echo "==> $PROJECT_NAME → $ANDROID_DIR"
echo "    lib_name = $LIB_NAME"
echo "    package  = $PACKAGE"
echo "    label    = $LABEL"

# 占位符替换：值里的 `&` / `|` 对 sed 有特殊含义，先转义。
escape_sed() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }
LIB_SED=$(escape_sed "$LIB_NAME")
PACKAGE_SED=$(escape_sed "$PACKAGE")
LABEL_SED=$(escape_sed "$LABEL")

rm -rf "$ANDROID_DIR"
mkdir -p "$ANDROID_DIR"
# `-a` 保留可执行位（gradlew）；文本与二进制（gradle-wrapper.jar）都原样复制。
cp -a "$TEMPLATE/." "$ANDROID_DIR/"

# Java 源码必须落在包名对应的目录里，否则 AndroidManifest 的 `.MainActivity` 找不到类。
mkdir -p "$ANDROID_DIR/app/src/main/java/$PACKAGE_PATH"
mv "$ANDROID_DIR/app/src/main/java/MainActivity.java" \
   "$ANDROID_DIR/app/src/main/java/$PACKAGE_PATH/MainActivity.java"

# 只改文本文件；gradle-wrapper.jar 是二进制，不能过 sed。
while IFS= read -r -d '' file; do
    case "$file" in
        *.jar) continue ;;
    esac
    sed -i \
        -e "s|__GAME_NAME__|$LIB_SED|g" \
        -e "s|__PACKAGE__|$PACKAGE_SED|g" \
        -e "s|__LABEL__|$LABEL_SED|g" \
        "$file"
done < <(find "$ANDROID_DIR" -type f -print0)

chmod +x "$ANDROID_DIR/gradlew"
[ -f "$ANDROID_DIR/gradle/wrapper/gradle-wrapper.jar" ] || {
    echo "模板缺 gradle-wrapper.jar：Android 打包会失败" >&2
    exit 1
}

echo
echo "完成。下一步："
echo "  bash scripts/build-android-game.sh $PROJECT_NAME            # debug APK"
echo "  bash scripts/build-android-game.sh $PROJECT_NAME release    # release APK（未签名）"
