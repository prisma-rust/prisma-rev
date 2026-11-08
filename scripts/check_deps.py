#!/usr/bin/env python3
"""玩家依赖树守护：`cargo tree -p prism-app` 的预算与黑名单。

    python scripts/check_deps.py            # 检查（CI / pr_ready 用）
    python scripts/check_deps.py --list     # 打印整棵玩家依赖树

为什么要它：`prism-app` 是**玩家二进制**唯一的顶层依赖，它多带一个 crate，
每个用户项目、每次 Android 打包都多带一个。历史上它曾经把 rapier3d/parry3d、
tokio、exr、jpeg 解码器全拉进来（255 个 crate），这些都是构建期或可选能力。

规则（改动本文件时同步更新 `docs/DESIGN.md` §11）：
  1. 玩家树里不得出现 `BANNED` 中的构建期/可选能力 crate；
  2. 玩家树 crate 总数不得超过 `MAX_PLAYER_CRATES`；
  3. 新增依赖必须在这里显式放行——超预算就红，不允许"顺手调高"。

stdout 是给 PR 贴的 Markdown 行，进度/诊断走 stderr。
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.console import force_utf8_streams  # noqa: E402

PLAYER_CRATE = "prism-app"

# 允许的玩家树上限。留一点余量，但**不要**为了过 CI 无脑调高：
# 要么把新依赖做成 feature，要么在 PR 里说明为什么它必须随玩家发布。
MAX_PLAYER_CRATES = 155

# 构建期 / 编辑器 / 可选能力：出现即失败。
BANNED: dict[str, str] = {
    "gltf": "资产导入只在 prism-cooker（构建期）",
    "gltf-json": "资产导入只在 prism-cooker（构建期）",
    "clap": "CLI 参数解析只在构建工具",
    "rayon": "构建期并行，运行时不该有",
    "exr": "EXR 解码只在 cooker 导入",
    "jpeg-decoder": "JPEG 解码只在 cooker 导入",
    "tiff": "TIFF 解码只在 cooker 导入",
    "tokio": "异步 IO 是 prism-asset 的 `async-io` feature，默认关闭",
    "rapier3d": "物理是 prism-app 的 `physics` feature，默认关闭",
    "parry3d": "物理是 prism-app 的 `physics` feature，默认关闭",
    "cpal": "音频后端是 prism-app 的 `audio` feature，默认关闭",
    "firewheel": "音频后端是 prism-app 的 `audio` feature，默认关闭",
    "symphonia": "音频解码是 prism-app 的 `audio` feature，默认关闭",
    "symphonium": "音频解码是 prism-app 的 `audio` feature，默认关闭",
}


force_utf8_streams()


def player_tree() -> list[str]:
    """解析 `cargo tree -p prism-app`，返回唯一的 crate 名列表。"""
    proc = subprocess.run(
        ["cargo", "tree", "-p", PLAYER_CRATE, "-e", "normal", "--prefix", "none"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit("check_deps: `cargo tree` 失败")

    names: set[str] = set()
    for line in proc.stdout.splitlines():
        line = line.strip()
        if not line or line.startswith("["):
            continue
        # `name v1.2.3 (/path)` / `name v1.2.3 (*)` → name
        name = line.split(" ")[0]
        if name:
            names.add(name)
    return sorted(names)


def main() -> int:
    parser = argparse.ArgumentParser(description="PrismaRev player dependency guard")
    parser.add_argument("--list", action="store_true", help="打印玩家依赖树")
    args = parser.parse_args()

    tree = player_tree()
    if args.list:
        print("\n".join(tree))
        return 0

    banned_hits = [(name, BANNED[name]) for name in tree if name in BANNED]
    total = len(tree)

    problems: list[str] = []
    if banned_hits:
        for name, reason in banned_hits:
            problems.append(f"`{name}` 不该出现在玩家树：{reason}")
    if total > MAX_PLAYER_CRATES:
        problems.append(f"玩家依赖 {total} 个 crate，超过预算 {MAX_PLAYER_CRATES}")

    if problems:
        for problem in problems:
            sys.stderr.write(f"check_deps: {problem}\n")
        sys.stderr.write(
            "check_deps: 修复方式见 scripts/check_deps.py 顶部注释"
            "（做 feature / 移到构建期 / 显式放行）\n"
        )
        print("| 依赖预算 | ❌ | " + "；".join(problems) + " |")
        return 1

    print(f"| 依赖预算 | ✅ | player {total}/{MAX_PLAYER_CRATES} crates，无构建期依赖 |")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
