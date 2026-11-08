#!/usr/bin/env python3
"""Lego 特性矩阵：保证「关掉某块能力」的组合真的能编译。

    python scripts/check_features.py          # 跑默认矩阵
    python scripts/check_features.py --list   # 只打印组合

为什么需要它：feature 一旦没人编译就会烂掉（`#[cfg]` 漏一处，几个月后才发现）。
项目模板宣称「2D 项目不编 3D / 路径追踪」，CI 就必须真的按那些组合编一遍。

新增能力（feature）时在 `MATRIX` 里加一条**最小可用组合**，不要只测默认组合。
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.console import force_utf8_streams  # noqa: E402

force_utf8_streams()

# (标签, cargo 参数)。每条都必须是「项目模板会真的这样构建」的组合。
MATRIX: list[tuple[str, list[str]]] = [
    ("prism-render 全关", ["check", "-p", "prism-render", "--no-default-features"]),
    # UI 叠加但**不要**文字：只画四边形，不链接任何文本光栅后端。
    (
        "prism-render ui（无文本后端）",
        ["check", "-p", "prism-render", "--no-default-features", "--features", "ui"],
    ),
    # 文本后端必须各自单独成立（互斥，不能一起开）。
    (
        "prism-render ui + fontdue",
        ["check", "-p", "prism-render", "--no-default-features", "--features", "ui,ui-fontdue"],
    ),
    # Linux 上 winit 必须至少有一个窗口后端，所以「全关」不是合法组合；
    # 最小可用组合是 x11-only（或 wayland-only）。
    ("prism-app 最小 (x11)", ["check", "-p", "prism-app", "--no-default-features", "--features", "x11"]),
    ("prism-app 最小 (wayland)", ["check", "-p", "prism-app", "--no-default-features", "--features", "wayland"]),
    (
        "prism-app x11 + pathtrace",
        ["check", "-p", "prism-app", "--no-default-features", "--features", "x11,pathtrace"],
    ),
    ("prism-app 2D 档（无 3D 组件组）",
     ["check", "-p", "prism-app", "--no-default-features", "--features", "x11,group-2d,group-ui"],
    ),
    (
        "prism-app 3D 极简（无 skybox/shadows/gtao）",
        ["check", "-p", "prism-app", "--no-default-features", "--features", "x11,group-3d,ibl"],
    ),
    ("prism-app 默认（全量）", ["check", "-p", "prism-app"]),
]


def main() -> int:
    parser = argparse.ArgumentParser(description="PrismaRev feature matrix check")
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args()

    if args.list:
        for label, cmd in MATRIX:
            print(f"{label}: cargo {' '.join(cmd)}")
        return 0

    failed: list[str] = []
    for label, cmd in MATRIX:
        sys.stderr.write(f"→ {label}: cargo {' '.join(cmd)}\n")
        proc = subprocess.run(["cargo", *cmd], cwd=ROOT, capture_output=True, text=True)
        if proc.returncode != 0:
            failed.append(label)
            sys.stderr.write(proc.stderr[-4000:] + "\n")
        else:
            sys.stderr.write("  ok\n")

    if failed:
        print(f"| Lego 特性矩阵 | ❌ | 失败：{', '.join(failed)} |")
        return 1
    print(f"| Lego 特性矩阵 | ✅ | {len(MATRIX)} 个组合 |")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
