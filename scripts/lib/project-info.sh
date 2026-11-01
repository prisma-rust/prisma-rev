#!/usr/bin/env bash
# 工程信息解析（#598）：被 `new-android-game.sh` / `build-android-game.sh` 共用。
#
# 只读 Cargo.toml 与 assets/settings.ini，不依赖目录名猜库名：
#   1. `assets/settings.ini` 的 `[build] lib_name`（显式覆盖，优先）
#   2. `Cargo.toml` 的 `[lib] name`
#   3. `Cargo.toml` 的 `[package] name`（`-` 换成 `_`，cargo 的默认库名规则）
#
# 用法（在脚本里 source）：
#   . "$ROOT/scripts/lib/project-info.sh"
#   PROJECT_DIR=$(prism_project_dir "$ROOT" fishing)
#   LIB_NAME=$(prism_lib_name "$PROJECT_DIR")

# 读取 INI 里 `[section] key` 的值（行内不解析注释；双引号去掉）。
# 找不到 / 空值 → 输出空行并返回 1。
prism_ini_get() {
    local file="$1" section="$2" key="$3"
    [ -f "$file" ] || return 1
    awk -v want_section="$section" -v want_key="$key" '
        /^[[:space:]]*[#;]/ { next }
        /^[[:space:]]*\[/ {
            line = $0
            gsub(/^[[:space:]]*\[|\][[:space:]]*$/, "", line)
            in_section = (line == want_section)
            next
        }
        in_section && /^[[:space:]]*[A-Za-z0-9_.-]+[[:space:]]*=/ {
            split($0, parts, "=")
            name = parts[1]
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", name)
            if (name != want_key) next
            value = substr($0, index($0, "=") + 1)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
            if (value ~ /^".*"$/) value = substr(value, 2, length(value) - 2)
            print value
            found = 1
            exit
        }
        END { if (!found) exit 1 }
    ' "$file"
}

# Cargo.toml 某个 `[section]` 下的 `name = "..."`。
prism_cargo_name() {
    local manifest="$1" section="$2"
    [ -f "$manifest" ] || return 1
    awk -v want="$section" '
        /^[[:space:]]*\[/ {
            line = $0
            gsub(/^[[:space:]]*\[|\][[:space:]]*$/, "", line)
            in_section = (line == want)
            next
        }
        in_section && /^[[:space:]]*name[[:space:]]*=/ {
            value = $0
            sub(/^[^=]*=[[:space:]]*"/, "", value)
            sub(/".*$/, "", value)
            print value
            found = 1
            exit
        }
        END { if (!found) exit 1 }
    ' "$manifest"
}

# `projects/<name>` 或绝对/相对路径 → 绝对路径；必须有 Cargo.toml。
prism_project_dir() {
    local root="$1" name="$2"
    local dir
    case "$name" in
        /*) dir="$name" ;;
        */*) dir="$root/$name" ;;
        *) dir="$root/projects/$name" ;;
    esac
    [ -f "$dir/Cargo.toml" ] || {
        echo "找不到工程：$dir/Cargo.toml（用法：<project> 是 projects/ 下的目录名）" >&2
        return 1
    }
    (cd "$dir" && pwd)
}

# 工程的库名（.so 名字去掉 lib 前缀与 .so 后缀，也是 `android.app.lib_name`）。
prism_lib_name() {
    local dir="$1"
    local name
    if name=$(prism_ini_get "$dir/assets/settings.ini" build lib_name) && [ -n "$name" ]; then
        printf '%s\n' "$name"
        return 0
    fi
    if name=$(prism_cargo_name "$dir/Cargo.toml" lib) && [ -n "$name" ]; then
        printf '%s\n' "$name" | tr '-' '_'
        return 0
    fi
    if name=$(prism_cargo_name "$dir/Cargo.toml" package) && [ -n "$name" ]; then
        printf '%s\n' "$name" | tr '-' '_'
        return 0
    fi
    echo "读不出库名：$dir/Cargo.toml 缺 [package] name" >&2
    return 1
}

# 工程名（目录名，用于默认包名 / 标签）。
prism_project_name() {
    basename "$1"
}
